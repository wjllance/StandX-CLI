//! WebSocket client for real-time data

use crate::auth::Credentials;
use crate::endpoints::StandXEndpoints;
use crate::error::{Error, Result};
use crate::models::*;
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

struct AbortTaskOnDrop(JoinHandle<()>);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// WebSocket client state
#[derive(Debug, Clone, PartialEq)]
pub enum WsState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

/// WebSocket message wrapper
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum WsMessage {
    Connected,
    Disconnected,
    Price(WsMarketUpdate<PriceData>),
    Depth(WsMarketUpdate<OrderBook>),
    Trade(Trade),
    Position(Position),
    Balance(Balance),
    Order(Order),
    Kline(KlineData),
    AccountUpdate(String),
    Error(String),
    Heartbeat,
}

/// Public-market payload together with the envelope metadata needed to decide
/// whether two independently-published channels can form one safe snapshot.
#[derive(Debug, Clone)]
pub struct WsMarketUpdate<T> {
    pub data: T,
    /// Exchange sequence when the venue included one in the envelope or data.
    pub seq: Option<u64>,
    /// Venue timestamp copied without reinterpretation from the envelope/data.
    pub server_time: Option<String>,
    /// Raw venue timestamp from the message envelope, when present.
    pub envelope_time: Option<String>,
    /// Raw venue timestamp from the channel payload, when present.
    pub payload_time: Option<String>,
    /// Local monotonic receipt time, assigned before forwarding the payload.
    pub received_at: Instant,
}

fn scalar_to_string(value: Option<&serde_json::Value>) -> Option<String> {
    value.and_then(|value| match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    })
}

fn parse_market_update<T>(
    envelope: &serde_json::Value,
    received_at: Instant,
) -> Option<WsMarketUpdate<T>>
where
    T: DeserializeOwned,
{
    let payload = envelope.get("data")?;
    let data = serde_json::from_value(payload.clone()).ok()?;
    let seq = envelope
        .get("seq")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| payload.get("seq").and_then(serde_json::Value::as_u64));
    let envelope_time =
        scalar_to_string(envelope.get("timestamp").or_else(|| envelope.get("time")));
    let payload_time = scalar_to_string(payload.get("timestamp").or_else(|| payload.get("time")));
    let server_time = envelope_time.clone().or_else(|| payload_time.clone());
    Some(WsMarketUpdate {
        data,
        seq,
        server_time,
        envelope_time,
        payload_time,
        received_at,
    })
}

/// StandX WebSocket client
pub struct StandXWebSocket {
    url: String,
    token: Option<String>,
    state: Arc<RwLock<WsState>>,
    subscriptions: Arc<RwLock<Vec<String>>>,
    #[allow(dead_code)]
    message_tx: mpsc::Sender<WsMessage>,
    #[allow(dead_code)]
    message_rx: Arc<RwLock<mpsc::Receiver<WsMessage>>>,
    reconnect_attempts: Arc<RwLock<u32>>,
    #[allow(dead_code)]
    channel: String,
    #[allow(dead_code)]
    symbol: Option<String>,
    verbose: bool,
}

impl StandXWebSocket {
    /// Create a new WebSocket client (requires auth for user channels)
    pub fn new() -> Result<Self> {
        Self::new_with_verbose(false)
    }

    /// Create a new WebSocket client with verbose mode
    pub fn new_with_verbose(verbose: bool) -> Result<Self> {
        Self::from_endpoints_with_verbose(&StandXEndpoints::default(), verbose)
    }

    /// Create an authenticated client for a validated endpoint set.
    pub fn from_endpoints(endpoints: &StandXEndpoints) -> Result<Self> {
        Self::from_endpoints_with_verbose(endpoints, false)
    }

    /// Create an authenticated client for a validated endpoint set.
    pub fn from_endpoints_with_verbose(endpoints: &StandXEndpoints, verbose: bool) -> Result<Self> {
        let creds = Credentials::load()?;

        if creds.is_expired() {
            return Err(Error::AuthRequired {
                message: "Token expired".to_string(),
                resolution: "Run 'standx auth login' or set STANDX_JWT environment variable"
                    .to_string(),
            });
        }

        let (message_tx, message_rx) = mpsc::channel(100);

        Ok(Self {
            url: endpoints.stream_url().to_string(),
            token: Some(creds.token),
            state: Arc::new(RwLock::new(WsState::Disconnected)),
            subscriptions: Arc::new(RwLock::new(Vec::new())),
            message_tx,
            message_rx: Arc::new(RwLock::new(message_rx)),
            reconnect_attempts: Arc::new(RwLock::new(0)),
            channel: String::new(),
            symbol: None,
            verbose,
        })
    }

    /// Create without authentication (for public channels only)
    pub fn without_auth() -> Result<Self> {
        Self::without_auth_with_verbose(false)
    }

    /// Create without authentication with verbose mode
    pub fn without_auth_with_verbose(verbose: bool) -> Result<Self> {
        Self::without_auth_from_endpoints_with_verbose(&StandXEndpoints::default(), verbose)
    }

    /// Create an unauthenticated client for a validated endpoint set.
    pub fn without_auth_from_endpoints(endpoints: &StandXEndpoints) -> Result<Self> {
        Self::without_auth_from_endpoints_with_verbose(endpoints, false)
    }

    /// Create an unauthenticated client for a validated endpoint set.
    pub fn without_auth_from_endpoints_with_verbose(
        endpoints: &StandXEndpoints,
        verbose: bool,
    ) -> Result<Self> {
        let (message_tx, message_rx) = mpsc::channel(100);

        Ok(Self {
            url: endpoints.stream_url().to_string(),
            token: None,
            state: Arc::new(RwLock::new(WsState::Disconnected)),
            subscriptions: Arc::new(RwLock::new(Vec::new())),
            message_tx,
            message_rx: Arc::new(RwLock::new(message_rx)),
            reconnect_attempts: Arc::new(RwLock::new(0)),
            channel: String::new(),
            symbol: None,
            verbose,
        })
    }

    /// Create with custom WebSocket URL
    pub fn with_url(url: String) -> Result<Self> {
        let creds = Credentials::load()?;

        if creds.is_expired() {
            return Err(Error::AuthRequired {
                message: "Token expired".to_string(),
                resolution: "Run 'standx auth login' or set STANDX_JWT environment variable"
                    .to_string(),
            });
        }

        let (message_tx, message_rx) = mpsc::channel(100);

        Ok(Self {
            url,
            token: Some(creds.token),
            state: Arc::new(RwLock::new(WsState::Disconnected)),
            subscriptions: Arc::new(RwLock::new(Vec::new())),
            message_tx,
            message_rx: Arc::new(RwLock::new(message_rx)),
            reconnect_attempts: Arc::new(RwLock::new(0)),
            channel: String::new(),
            symbol: None,
            verbose: false,
        })
    }

    /// Connect and start the WebSocket client
    pub async fn connect(&self) -> Result<mpsc::Receiver<WsMessage>> {
        let (rx, _handle) = self.connect_managed().await?;
        Ok(rx)
    }

    /// Connect and return ownership of the background task so a caller with
    /// its own liveness policy can actively tear down a silent connection.
    pub async fn connect_managed(&self) -> Result<(mpsc::Receiver<WsMessage>, JoinHandle<()>)> {
        let (tx, rx) = mpsc::channel(100);

        let url = self.url.clone();
        let token = self.token.clone();
        let state = self.state.clone();
        let subscriptions = self.subscriptions.clone();
        let reconnect_attempts = self.reconnect_attempts.clone();
        let verbose = self.verbose;

        let handle = tokio::spawn(async move {
            loop {
                *state.write().await = WsState::Connecting;

                match connect_and_run(&url, token.as_deref(), &subscriptions, &tx, verbose).await {
                    Ok(_) => {
                        *reconnect_attempts.write().await = 0;
                    }
                    Err(e) => {
                        let attempts = *reconnect_attempts.read().await;
                        if attempts >= 5 {
                            let _ = tx
                                .send(WsMessage::Error(format!(
                                    "Max reconnection attempts reached: {}",
                                    e
                                )))
                                .await;
                            break;
                        }

                        *reconnect_attempts.write().await = attempts + 1;
                        *state.write().await = WsState::Reconnecting;

                        tokio::time::sleep(RECONNECT_DELAY).await;
                    }
                }
            }
        });

        Ok((rx, handle))
    }

    /// Subscribe to a channel
    pub async fn subscribe(&self, channel: &str, symbol: Option<&str>) -> Result<()> {
        let mut subs = self.subscriptions.write().await;
        let topic = if let Some(sym) = symbol {
            format!("{}:{}", channel, sym)
        } else {
            channel.to_string()
        };
        subs.push(topic);
        Ok(())
    }

    /// Subscribe to a channel with interval (for kline)
    pub async fn subscribe_with_interval(
        &self,
        channel: &str,
        symbol: Option<&str>,
        interval: Option<&str>,
    ) -> Result<()> {
        let mut subs = self.subscriptions.write().await;
        let topic = if let (Some(sym), Some(int)) = (symbol, interval) {
            format!("{}:{}:{}", channel, sym, int)
        } else if let Some(sym) = symbol {
            format!("{}:{}", channel, sym)
        } else {
            channel.to_string()
        };
        subs.push(topic);
        Ok(())
    }

    /// Get current state
    pub async fn state(&self) -> WsState {
        self.state.read().await.clone()
    }
}

/// Connect to WebSocket and run message loop
/// Verbose flag controls debug output - only shows debug logs when enabled
async fn connect_and_run(
    url: &str,
    token: Option<&str>,
    subscriptions: &Arc<RwLock<Vec<String>>>,
    message_tx: &mpsc::Sender<WsMessage>,
    verbose: bool,
) -> Result<()> {
    let ws_url = url.to_string();
    if verbose {
        eprintln!("[WebSocket Debug] Connecting to: {}", ws_url);
    }

    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .map_err(|e| Error::Unknown(format!("WebSocket connect failed: {}", e)))?;
    if verbose {
        eprintln!("[WebSocket Debug] Connected successfully");
    }

    let (mut write, mut read) = ws_stream.split();

    // Get subscriptions early for auth message
    let subs = subscriptions.read().await;

    // Send authentication only if token is provided
    if let Some(t) = token {
        // Build streams array from subscriptions
        let streams: Vec<serde_json::Value> = subs
            .iter()
            .map(|topic| {
                let parts: Vec<&str> = topic.split(':').collect();
                let channel = parts[0];
                let symbol = if parts.len() > 1 { parts[1] } else { "" };
                if symbol.is_empty() {
                    serde_json::json!({ "channel": channel })
                } else {
                    serde_json::json!({ "channel": channel, "symbol": symbol })
                }
            })
            .collect();

        let auth_msg = serde_json::json!({
            "auth": {
                "token": t,
                "streams": streams
            }
        });
        if verbose {
            eprintln!(
                "[WebSocket Debug] Sending authentication for {} stream(s)",
                subs.len()
            );
        }
        write
            .send(Message::Text(auth_msg.to_string().into()))
            .await
            .map_err(|e| Error::Unknown(format!("Failed to send auth: {}", e)))?;
        if verbose {
            eprintln!("[WebSocket Debug] Auth sent");
        }
    } else if verbose {
        eprintln!("[WebSocket Debug] Skipping auth (public channel)");
    }

    // Send subscription messages for all registered subscriptions
    if verbose {
        eprintln!("[WebSocket Debug] Subscribing to {} topics", subs.len());
    }

    // Wait a bit for server to be ready
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    for topic in subs.iter() {
        // Parse topic to get channel, symbol, and optional interval
        // Format: "price:BTC-USD" or "kline:BTC-USD:3S"
        let parts: Vec<&str> = topic.split(':').collect();
        let channel = parts.first().copied().unwrap_or(topic.as_str());
        let symbol = parts.get(1).copied().unwrap_or("");
        let interval = parts.get(2).copied();

        // Build subscription message
        let mut sub_obj = serde_json::json!({
            "channel": channel,
            "symbol": symbol
        });

        // Add interval for kline channel
        if channel == "kline" {
            if let Some(int) = interval {
                sub_obj["interval"] = serde_json::json!(int);
            }
        }

        let sub_msg = serde_json::json!({
            "subscribe": sub_obj
        });
        if verbose {
            eprintln!("[WebSocket Debug] Sending subscribe: {}", sub_msg);
        }
        if let Err(e) = write.send(Message::Text(sub_msg.to_string().into())).await {
            let _ = message_tx
                .send(WsMessage::Error(format!(
                    "Failed to subscribe to {}: {}",
                    topic, e
                )))
                .await;
        }
    }

    let _ = message_tx.send(WsMessage::Connected).await;
    if verbose {
        eprintln!("[WebSocket Debug] Entering message loop");
    }

    // Spawn heartbeat task
    let heartbeat_tx = message_tx.clone();
    let heartbeat_write = Arc::new(RwLock::new(write));
    let heartbeat_write_clone = heartbeat_write.clone();

    let heartbeat_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            interval.tick().await;
            let mut writer = heartbeat_write_clone.write().await;
            if let Err(e) = writer.send(Message::Ping(vec![].into())).await {
                let _ = heartbeat_tx
                    .send(WsMessage::Error(format!("Heartbeat failed: {}", e)))
                    .await;
                break;
            }
        }
    });
    // Cancelling the owning connection task (for example, from a market-feed
    // idle watchdog) must also stop the heartbeat writer that owns the other
    // half of the socket.
    let _heartbeat_guard = AbortTaskOnDrop(heartbeat_handle);

    // Main message loop
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(text)) => {
                // Debug: print received message
                if verbose {
                    eprintln!("[WebSocket Debug] Received: {}", text);
                }

                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                    // Check for error response
                    if let Some(code) = data.get("code").and_then(|c| c.as_i64()) {
                        if code != 0 {
                            let message = data
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("Unknown error");
                            if verbose {
                                eprintln!("[WebSocket Debug] Server error: {}", message);
                            }
                            continue;
                        }
                    }

                    // Parse message based on channel field
                    if let Some(channel) = data.get("channel").and_then(|c| c.as_str()) {
                        if verbose {
                            eprintln!("[WebSocket Debug] Message channel: {}", channel);
                        }
                        if data.get("data").is_some() {
                            match channel {
                                "price" => {
                                    if let Some(price) =
                                        parse_market_update::<PriceData>(&data, Instant::now())
                                    {
                                        let _ = message_tx.send(WsMessage::Price(price)).await;
                                    }
                                }
                                "depth_book" => {
                                    if let Some(depth) =
                                        parse_market_update::<OrderBook>(&data, Instant::now())
                                    {
                                        let _ = message_tx.send(WsMessage::Depth(depth)).await;
                                    }
                                }
                                "public_trade" => {
                                    if let Ok(trade) =
                                        serde_json::from_value::<Trade>(data["data"].clone())
                                    {
                                        let _ = message_tx.send(WsMessage::Trade(trade)).await;
                                    }
                                }
                                "kline" => {
                                    // Kline data is an array, take first element
                                    if let Some(kline_array) = data["data"].as_array() {
                                        if let Some(kline_item) = kline_array.first() {
                                            if let Ok(mut kline) = serde_json::from_value::<KlineData>(
                                                kline_item.clone(),
                                            ) {
                                                // Get symbol and interval from parent message
                                                if kline.symbol.is_none() {
                                                    kline.symbol = data
                                                        .get("symbol")
                                                        .and_then(|s| s.as_str())
                                                        .map(String::from);
                                                }
                                                if kline.interval.is_none() {
                                                    kline.interval = data
                                                        .get("interval")
                                                        .and_then(|i| i.as_str())
                                                        .map(String::from);
                                                }
                                                let _ =
                                                    message_tx.send(WsMessage::Kline(kline)).await;
                                            }
                                        }
                                    }
                                }
                                "order" | "position" | "balance" | "trade" => {
                                    if verbose {
                                        eprintln!(
                                            "[WebSocket Debug] User channel received: {}",
                                            channel
                                        );
                                    }
                                    // TODO: Parse user-specific messages
                                }
                                _ => {
                                    if verbose {
                                        eprintln!("[WebSocket Debug] Unknown channel: {}", channel);
                                    }
                                }
                            }
                        }
                    } else if verbose {
                        eprintln!("[WebSocket Debug] No channel field in message");
                    }
                } else if verbose {
                    eprintln!("[WebSocket Debug] Failed to parse JSON: {}", text);
                }
            }
            Ok(Message::Ping(data)) => {
                let mut writer = heartbeat_write.write().await;
                if let Err(e) = writer.send(Message::Pong(data)).await {
                    return Err(Error::Unknown(format!("Failed to send pong: {}", e)));
                }
            }
            Ok(Message::Pong(_)) => {
                let _ = message_tx.send(WsMessage::Heartbeat).await;
            }
            Ok(Message::Close(frame)) => {
                if verbose {
                    eprintln!("[WebSocket Debug] Connection closed: {:?}", frame);
                }
                let _ = message_tx.send(WsMessage::Disconnected).await;
                break;
            }
            Ok(Message::Frame(_)) => {
                // Frame messages are handled internally by tungstenite
            }
            Ok(Message::Binary(data)) => {
                if verbose {
                    eprintln!(
                        "[WebSocket Debug] Received binary data: {} bytes",
                        data.len()
                    );
                }
            }
            Err(e) => {
                if verbose {
                    eprintln!("[WebSocket Debug] WebSocket error: {}", e);
                }
                return Err(Error::Unknown(format!("WebSocket error: {}", e)));
            }
        }
    }

    if verbose {
        eprintln!("[WebSocket Debug] Message loop ended");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ws_state() {
        assert_ne!(WsState::Connected, WsState::Disconnected);
    }

    #[test]
    fn market_update_preserves_exchange_and_local_metadata() {
        let envelope = serde_json::json!({
            "seq": 42,
            "channel": "price",
            "timestamp": "2026-07-14T00:00:00Z",
            "data": {
                "symbol": "BTC-USD",
                "mark_price": "100",
                "index_price": "100",
                "last_price": "100",
                "timestamp": "2026-07-14T00:00:00Z"
            }
        });
        let received_at = Instant::now();
        let update = parse_market_update::<PriceData>(&envelope, received_at).unwrap();
        assert_eq!(update.data.symbol, "BTC-USD");
        assert_eq!(update.seq, Some(42));
        assert_eq!(update.server_time.as_deref(), Some("2026-07-14T00:00:00Z"));
        assert_eq!(
            update.envelope_time.as_deref(),
            Some("2026-07-14T00:00:00Z")
        );
        assert_eq!(update.payload_time.as_deref(), Some("2026-07-14T00:00:00Z"));
        assert_eq!(update.received_at, received_at);
    }

    #[test]
    fn market_update_keeps_distinct_envelope_and_payload_times() {
        let envelope = serde_json::json!({
            "channel": "depth_book",
            "timestamp": 1_752_499_200_000i64,
            "data": {
                "symbol": "BTC-USD",
                "bids": [["99", "1"]],
                "asks": [["101", "1"]],
                "timestamp": "2026-07-15T00:00:01Z"
            }
        });
        let update = parse_market_update::<OrderBook>(&envelope, Instant::now()).unwrap();

        assert_eq!(update.server_time.as_deref(), Some("1752499200000"));
        assert_eq!(update.envelope_time.as_deref(), Some("1752499200000"));
        assert_eq!(update.payload_time.as_deref(), Some("2026-07-15T00:00:01Z"));
    }
}
