//! WebSocket client for real-time data

use crate::auth::Credentials;
use crate::endpoints::StandXEndpoints;
use crate::error::{Error, Result};
use crate::models::*;
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    Trade(WsPublicTrade),
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
    /// Copied only when the payload has `size_ahead` as a finite number.
    /// Absence stays `None`. This is never derived from bid or ask quantities.
    pub size_ahead: Option<f64>,
    /// Copied only when the payload has `our_rank` as a non-negative integer.
    /// Absence stays `None`. This is never derived from level order.
    pub our_rank: Option<u64>,
}

/// One public trade plus the envelope clock. `id` is separate from
/// [`crate::models::Trade::id`]: the typed trade defaults a missing id to 0,
/// and that default must not be logged as a venue id.
#[derive(Debug, Clone)]
pub struct WsPublicTrade {
    pub update: WsMarketUpdate<crate::models::Trade>,
    pub id: Option<u64>,
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
        size_ahead: payload.get("size_ahead").and_then(finite_f64),
        our_rank: payload.get("our_rank").and_then(integer_u64),
    })
}

fn parse_public_trade(envelope: &serde_json::Value, received_at: Instant) -> Option<WsPublicTrade> {
    let update = parse_market_update(envelope, received_at)?;
    let id = envelope.get("data")?.get("id").and_then(integer_u64);
    Some(WsPublicTrade { update, id })
}

fn finite_f64(value: &serde_json::Value) -> Option<f64> {
    let parsed = match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    parsed.is_finite().then_some(parsed)
}

fn integer_u64(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .or_else(|| integer_from_f64(number.as_f64()?)),
        serde_json::Value::String(text) => {
            let text = text.trim();
            text.parse::<u64>()
                .ok()
                .or_else(|| integer_from_f64(text.parse().ok()?))
        }
        _ => None,
    }
}

fn integer_from_f64(value: f64) -> Option<u64> {
    // Only integers that f64 can represent exactly. Larger ranks arrive as
    // JSON integers and take the `as_u64` path above.
    if value.is_finite() && value >= 0.0 && value < (1u64 << 53) as f64 && value.fract() == 0.0 {
        Some(value as u64)
    } else {
        None
    }
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
    public_trade_raw_sample_budget: Option<Arc<AtomicUsize>>,
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
            public_trade_raw_sample_budget: None,
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
            public_trade_raw_sample_budget: None,
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
            public_trade_raw_sample_budget: None,
        })
    }

    /// Opt into a process-shared, bounded stderr sample of exact
    /// `public_trade` frames. Parsing and typed trade delivery remain
    /// unchanged; exhausting the budget simply disables further samples.
    pub fn with_public_trade_raw_sample_budget(mut self, budget: Arc<AtomicUsize>) -> Self {
        self.public_trade_raw_sample_budget = Some(budget);
        self
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
        let public_trade_raw_sample_budget = self.public_trade_raw_sample_budget.clone();

        let handle = tokio::spawn(async move {
            loop {
                *state.write().await = WsState::Connecting;

                match connect_and_run(
                    &url,
                    token.as_deref(),
                    &subscriptions,
                    &tx,
                    verbose,
                    public_trade_raw_sample_budget.as_deref(),
                )
                .await
                {
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
    public_trade_raw_sample_budget: Option<&AtomicUsize>,
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
                                    if take_public_trade_raw_sample(public_trade_raw_sample_budget)
                                    {
                                        eprintln!("public_trade raw sample: {text}");
                                    }
                                    if let Some(trade) = parse_public_trade(&data, Instant::now()) {
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

fn take_public_trade_raw_sample(budget: Option<&AtomicUsize>) -> bool {
    budget.is_some_and(|budget| {
        // `fetch_update` is the MSRV-1.75 name. Current stable deprecates it
        // in favor of `try_update`, which is newer than rust-version.
        #[allow(deprecated)]
        let updated = budget.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
            remaining.checked_sub(1)
        });
        updated.is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ws_state() {
        assert_ne!(WsState::Connected, WsState::Disconnected);
    }

    #[test]
    fn public_trade_raw_sample_budget_is_exact_and_bounded() {
        let budget = AtomicUsize::new(50);

        for _ in 0..50 {
            assert!(take_public_trade_raw_sample(Some(&budget)));
        }
        assert!(!take_public_trade_raw_sample(Some(&budget)));
        assert!(!take_public_trade_raw_sample(None));
        assert_eq!(budget.load(Ordering::Relaxed), 0);
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
        assert_eq!(update.size_ahead, None);
        assert_eq!(update.our_rank, None);
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
        assert_eq!(update.size_ahead, None);
        assert_eq!(update.our_rank, None);
    }

    #[test]
    fn depth_queue_fields_are_copied_only_when_present_and_not_derived() {
        let received_at = Instant::now();
        let plain = serde_json::json!({
            "channel": "depth_book",
            "data": {
                "symbol": "BTC-USD",
                "bids": [["99", "5"]],
                "asks": [["101", "7"]]
            }
        });
        let plain = parse_market_update::<OrderBook>(&plain, received_at).unwrap();
        assert_eq!(plain.data.best_bid(), Some("99"));
        assert_eq!(plain.size_ahead, None);
        assert_eq!(plain.our_rank, None);

        let present = serde_json::json!({
            "channel": "depth_book",
            "data": {
                "symbol": "BTC-USD",
                "bids": [["99", "5"]],
                "asks": [["101", "7"]],
                "size_ahead": 1.5,
                "our_rank": 3
            }
        });
        let present = parse_market_update::<OrderBook>(&present, received_at).unwrap();
        assert_eq!(present.size_ahead, Some(1.5));
        assert_eq!(present.our_rank, Some(3));

        let invalid = serde_json::json!({
            "channel": "depth_book",
            "data": {
                "symbol": "BTC-USD",
                "bids": [["99", "5"]],
                "asks": [["101", "7"]],
                "size_ahead": "ahead",
                "our_rank": 1.5
            }
        });
        let invalid = parse_market_update::<OrderBook>(&invalid, received_at).unwrap();
        assert_eq!(invalid.size_ahead, None);
        assert_eq!(invalid.our_rank, None);
    }

    #[test]
    fn public_trade_keeps_server_time_and_does_not_invent_a_missing_id() {
        let received_at = Instant::now();
        let envelope = serde_json::json!({
            "channel": "public_trade",
            "timestamp": 1_752_499_200_500i64,
            "data": {
                "price": "100",
                "qty": "1.5",
                "is_taker": true
            }
        });
        let trade = parse_public_trade(&envelope, received_at).unwrap();
        assert_eq!(trade.id, None);
        assert_eq!(trade.update.data.id, 0);
        assert_eq!(trade.update.data.side, None);
        assert!(trade.update.data.is_buyer_taker);
        assert_eq!(trade.update.server_time.as_deref(), Some("1752499200500"));

        let with_id = serde_json::json!({
            "channel": "public_trade",
            "data": {
                "id": 7,
                "price": "100",
                "qty": "1",
                "side": "buy",
                "time": "2026-07-14T00:00:00.500Z"
            }
        });
        let trade = parse_public_trade(&with_id, received_at).unwrap();
        assert_eq!(trade.id, Some(7));
        assert_eq!(
            trade.update.server_time.as_deref(),
            Some("2026-07-14T00:00:00.500Z")
        );
        assert_eq!(trade.update.data.side.as_deref(), Some("buy"));
    }
}
