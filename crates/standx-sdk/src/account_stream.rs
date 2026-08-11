//! Authenticated user order, position, and trade notifications.

use crate::auth::Credentials;
use crate::endpoints::StandXEndpoints;
use crate::error::{Error, Result};
use crate::models::{deserialize_order_side_optional, OrderSide, OrderStatus};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const ACCOUNT_STREAM_ROTATE_AFTER: Duration = Duration::from_secs(23 * 60 * 60 + 50 * 60);
/// How often we send a client-side ping to keep the connection observably
/// alive and to elicit a pong (which resets the idle deadline).
const ACCOUNT_STREAM_PING_INTERVAL: Duration = Duration::from_secs(30);
/// If no inbound frame (data, ping, or pong) arrives within this window the
/// connection is treated as stale — this catches half-open TCP sessions where
/// the socket never errors but the peer has silently gone away.
const ACCOUNT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

fn string_or_number<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(value) => Ok(value),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "expected string or number, got {other}"
        ))),
    }
}

fn u64_string_or_number<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = string_or_number(deserializer)?;
    value.parse::<u64>().map_err(serde::de::Error::custom)
}

fn nonzero_u64_string_or_number<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = u64_string_or_number(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom("expected a non-zero stable ID"));
    }
    Ok(value)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountChannel {
    Order,
    Position,
    Trade,
    Balance,
}

impl AccountChannel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Order => "order",
            Self::Position => "position",
            Self::Trade => "trade",
            Self::Balance => "balance",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Order => 0,
            Self::Position => 1,
            Self::Trade => 2,
            Self::Balance => 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrderUpdate {
    #[serde(default)]
    pub seq: u64,
    #[serde(rename = "id", deserialize_with = "u64_string_or_number")]
    pub order_id: u64,
    #[serde(default)]
    pub cl_ord_id: Option<String>,
    pub symbol: String,
    pub side: OrderSide,
    #[serde(deserialize_with = "string_or_number")]
    pub qty: String,
    #[serde(default, deserialize_with = "string_or_number")]
    pub fill_qty: String,
    #[serde(default, deserialize_with = "string_or_number")]
    pub fill_avg_price: String,
    #[serde(default, deserialize_with = "string_or_number")]
    pub price: String,
    pub status: OrderStatus,
    #[serde(default)]
    pub reduce_only: bool,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PositionUpdate {
    #[serde(default)]
    pub seq: u64,
    #[serde(default, deserialize_with = "u64_string_or_number")]
    pub id: u64,
    pub symbol: String,
    #[serde(default, deserialize_with = "deserialize_order_side_optional")]
    pub side: Option<OrderSide>,
    #[serde(deserialize_with = "string_or_number")]
    pub qty: String,
    #[serde(default, deserialize_with = "string_or_number")]
    pub entry_price: String,
    #[serde(default, deserialize_with = "string_or_number")]
    pub realized_pnl: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub updated_at: String,
}

/// A single user trade from the authenticated account stream.
///
/// Unlike an order update's cumulative fill quantity, this is an immutable
/// venue execution. Both IDs are required so consumers can safely deduplicate
/// it against REST reconciliation data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TradeUpdate {
    #[serde(default)]
    pub seq: u64,
    #[serde(
        rename = "id",
        alias = "trade_id",
        deserialize_with = "nonzero_u64_string_or_number"
    )]
    pub trade_id: u64,
    #[serde(deserialize_with = "nonzero_u64_string_or_number")]
    pub order_id: u64,
    pub symbol: String,
    pub side: OrderSide,
    #[serde(deserialize_with = "string_or_number")]
    pub price: String,
    #[serde(deserialize_with = "string_or_number")]
    pub qty: String,
    #[serde(
        rename = "time",
        alias = "created_at",
        alias = "updated_at",
        deserialize_with = "string_or_number"
    )]
    pub trade_ts: String,
}

/// Raw wallet balance notification from the authenticated account stream.
///
/// This deliberately does not reuse [`crate::models::Balance`]: the stream
/// reports wallet fields (`free`, `total`, `locked`, `occupied`), while the
/// REST model is a derived unified margin snapshot (`equity`, `upnl`,
/// `cross_available`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BalanceUpdate {
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub account_type: String,
    #[serde(default)]
    pub token: String,
    #[serde(deserialize_with = "string_or_number")]
    pub free: String,
    #[serde(deserialize_with = "string_or_number")]
    pub total: String,
    #[serde(deserialize_with = "string_or_number")]
    pub locked: String,
    #[serde(deserialize_with = "string_or_number")]
    pub occupied: String,
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountEvent {
    Connected { epoch: u64 },
    Order(OrderUpdate),
    Position(PositionUpdate),
    Trade(TradeUpdate),
    Balance(BalanceUpdate),
    Disconnected { reason: String },
    Error { reason: String },
}

#[derive(Debug, Clone)]
pub struct AccountStreamHealth {
    healthy: Arc<AtomicBool>,
    failure_reason: Arc<Mutex<Option<String>>>,
    epoch: Arc<AtomicU64>,
    last_seq: Arc<[AtomicU64; 4]>,
}

impl AccountStreamHealth {
    /// Constructs a standalone healthy handle for the given epoch. Production
    /// handles are created by [`AccountStream::connect`]; this constructor
    /// exists so downstream tests can build a session fixture and drive
    /// [`Self::mark_unhealthy`], mirroring `OrderResponseHealth`. The epoch is
    /// explicit (no `Default`) so fixtures cannot silently disagree with the
    /// projection generation they are paired with.
    pub fn new(epoch: u64) -> Self {
        Self {
            healthy: Arc::new(AtomicBool::new(true)),
            failure_reason: Arc::new(Mutex::new(None)),
            epoch: Arc::new(AtomicU64::new(epoch)),
            last_seq: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn failure_reason(&self) -> Option<String> {
        self.failure_reason
            .lock()
            .ok()
            .and_then(|reason| reason.clone())
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn last_seq(&self, channel: AccountChannel) -> u64 {
        self.last_seq[channel.index()].load(Ordering::Acquire)
    }

    pub fn mark_unhealthy(&self, reason: impl Into<String>) {
        if let Ok(mut failure_reason) = self.failure_reason.lock() {
            *failure_reason = Some(reason.into());
        }
        self.healthy.store(false, Ordering::Release);
    }
}

pub struct AccountStream {
    url: String,
    token: String,
    epoch: u64,
    ping_interval: Duration,
    idle_timeout: Duration,
}

fn validate_account_auth(envelope: &serde_json::Value) -> Result<()> {
    if envelope.get("channel").and_then(|value| value.as_str()) != Some("auth") {
        return Err(Error::WebSocket {
            message: "unexpected account event before authentication".to_string(),
        });
    }

    // Authentication must have positive, explicitly typed venue evidence.
    // Missing or non-numeric codes are malformed responses, not code 0 success.
    let code = envelope
        .pointer("/data/code")
        .and_then(|value| value.as_i64())
        .ok_or_else(|| Error::WebSocket {
            message: "account-stream authentication response has no numeric data.code".to_string(),
        })?;
    if code != 200 && code != 0 {
        return Err(Error::AuthRequired {
            message: envelope
                .pointer("/data/msg")
                .and_then(|value| value.as_str())
                .unwrap_or("account-stream authentication rejected")
                .to_string(),
            resolution: "Run 'standx auth login' and retry".to_string(),
        });
    }
    Ok(())
}

impl AccountStream {
    pub fn new(epoch: u64) -> Result<Self> {
        Self::from_endpoints(epoch, &StandXEndpoints::default())
    }

    pub fn from_endpoints(epoch: u64, endpoints: &StandXEndpoints) -> Result<Self> {
        let credentials = Credentials::load()?;
        if credentials.is_expired() {
            return Err(Error::AuthRequired {
                message: "Token expired".to_string(),
                resolution: "Run 'standx auth login' or set STANDX_JWT".to_string(),
            });
        }
        Ok(Self {
            url: endpoints.stream_url().to_string(),
            token: credentials.token,
            epoch,
            ping_interval: ACCOUNT_STREAM_PING_INTERVAL,
            idle_timeout: ACCOUNT_STREAM_IDLE_TIMEOUT,
        })
    }

    #[cfg(test)]
    fn with_url_and_token(url: impl Into<String>, token: impl Into<String>, epoch: u64) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            epoch,
            ping_interval: ACCOUNT_STREAM_PING_INTERVAL,
            idle_timeout: ACCOUNT_STREAM_IDLE_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_heartbeat(mut self, ping_interval: Duration, idle_timeout: Duration) -> Self {
        self.ping_interval = ping_interval;
        self.idle_timeout = idle_timeout;
        self
    }

    pub async fn connect(
        &self,
        channels: &[AccountChannel],
    ) -> Result<(
        mpsc::Receiver<AccountEvent>,
        AccountStreamHealth,
        tokio::task::JoinHandle<()>,
    )> {
        if channels.is_empty() {
            return Err(Error::Validation {
                field: "channels".to_string(),
                message: "account stream requires at least one channel".to_string(),
            });
        }
        let (stream, _) = connect_async(&self.url).await?;
        let (mut write, mut read) = stream.split();
        let streams = channels
            .iter()
            .map(|channel| serde_json::json!({ "channel": channel.as_str() }))
            .collect::<Vec<_>>();
        write
            .send(Message::Text(
                serde_json::json!({
                    "auth": { "token": self.token, "streams": streams }
                })
                .to_string()
                .into(),
            ))
            .await?;

        loop {
            let message = tokio::time::timeout(Duration::from_secs(10), read.next())
                .await
                .map_err(|_| Error::WebSocket {
                    message: "timed out waiting for account-stream authentication".to_string(),
                })?
                .ok_or_else(|| Error::WebSocket {
                    message: "account stream closed before authentication".to_string(),
                })??;
            match message {
                Message::Text(text) => {
                    let envelope: serde_json::Value = serde_json::from_str(&text)?;
                    validate_account_auth(&envelope)?;
                    break;
                }
                Message::Ping(payload) => write.send(Message::Pong(payload)).await?,
                Message::Close(_) => {
                    return Err(Error::WebSocket {
                        message: "account stream closed before authentication".to_string(),
                    });
                }
                _ => {}
            }
        }

        let (tx, rx) = mpsc::channel(512);
        let health = AccountStreamHealth::new(self.epoch);
        let task_health = health.clone();
        let epoch = self.epoch;
        let ping_interval = self.ping_interval;
        let idle_timeout = self.idle_timeout;
        let _ = tx.send(AccountEvent::Connected { epoch }).await;
        let handle = tokio::spawn(async move {
            let rotation = tokio::time::sleep(ACCOUNT_STREAM_ROTATE_AFTER);
            tokio::pin!(rotation);
            // First ping fires after one interval (not immediately), so the
            // handshake and any buffered startup frames are handled first.
            let mut ping = tokio::time::interval_at(
                tokio::time::Instant::now() + ping_interval,
                ping_interval,
            );
            ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Read-side idle deadline; reset on every inbound frame.
            let idle = tokio::time::sleep(idle_timeout);
            tokio::pin!(idle);
            loop {
                let message = tokio::select! {
                    _ = &mut rotation => {
                        let reason = "account stream proactive 23h50m rotation".to_string();
                        task_health.mark_unhealthy(reason.clone());
                        let _ = tx.send(AccountEvent::Disconnected { reason }).await;
                        return;
                    }
                    _ = ping.tick() => {
                        if let Err(error) = write.send(Message::Ping(Vec::new().into())).await {
                            let reason = format!("account-stream ping failed: {error}");
                            task_health.mark_unhealthy(reason.clone());
                            let _ = tx.send(AccountEvent::Error { reason }).await;
                            return;
                        }
                        continue;
                    }
                    _ = &mut idle => {
                        let reason = format!(
                            "account stream idle for {}s (no ping/pong/data; connection likely half-open)",
                            idle_timeout.as_secs()
                        );
                        task_health.mark_unhealthy(reason.clone());
                        let _ = tx.send(AccountEvent::Disconnected { reason }).await;
                        return;
                    }
                    message = read.next() => message,
                };
                // Any inbound frame proves the peer is alive; extend the deadline.
                idle.as_mut()
                    .reset(tokio::time::Instant::now() + idle_timeout);
                let Some(message) = message else {
                    let reason = "account stream ended without a close frame".to_string();
                    task_health.mark_unhealthy(reason.clone());
                    let _ = tx.send(AccountEvent::Disconnected { reason }).await;
                    return;
                };
                match message {
                    Ok(Message::Text(text)) => {
                        let result = parse_account_event(&text, &task_health);
                        match result {
                            Ok(Some(event)) => {
                                if tx.send(event).await.is_err() {
                                    return;
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                let reason = format!("invalid account-stream payload: {error}");
                                task_health.mark_unhealthy(reason.clone());
                                let _ = tx.send(AccountEvent::Error { reason }).await;
                                return;
                            }
                        }
                    }
                    Ok(Message::Ping(payload)) => {
                        if let Err(error) = write.send(Message::Pong(payload)).await {
                            let reason = format!("account-stream pong failed: {error}");
                            task_health.mark_unhealthy(reason.clone());
                            let _ = tx.send(AccountEvent::Error { reason }).await;
                            return;
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        let reason = frame.map_or_else(
                            || "account stream closed without a close frame".to_string(),
                            |frame| {
                                format!(
                                    "account stream closed: code={} reason={:?}",
                                    u16::from(frame.code),
                                    frame.reason
                                )
                            },
                        );
                        task_health.mark_unhealthy(reason.clone());
                        let _ = tx.send(AccountEvent::Disconnected { reason }).await;
                        return;
                    }
                    Err(error) => {
                        let reason = format!("account stream WebSocket error: {error}");
                        task_health.mark_unhealthy(reason.clone());
                        let _ = tx.send(AccountEvent::Error { reason }).await;
                        return;
                    }
                    _ => {}
                }
            }
        });
        Ok((rx, health, handle))
    }
}

fn parse_account_event(text: &str, health: &AccountStreamHealth) -> Result<Option<AccountEvent>> {
    let envelope: serde_json::Value = serde_json::from_str(text)?;
    let Some(channel) = envelope.get("channel").and_then(|value| value.as_str()) else {
        return Ok(None);
    };
    let account_channel = match channel {
        "order" => AccountChannel::Order,
        "position" => AccountChannel::Position,
        "trade" => AccountChannel::Trade,
        "balance" => AccountChannel::Balance,
        "auth" => return Ok(None),
        _ => return Ok(None),
    };
    let seq = envelope
        .get("seq")
        .and_then(|value| value.as_u64())
        .ok_or_else(|| Error::WebSocket {
            message: format!("{channel} event has no numeric seq"),
        })?;
    // StandX does not document whether `seq` is global or channel-local, nor
    // whether it is contiguous. Channel-local monotonic validation is safe for
    // either scope: reject duplicates/regressions within a channel, but allow
    // interleaved channels and gaps without falsely declaring the stream dead.
    let channel_seq = &health.last_seq[account_channel.index()];
    let previous = channel_seq.load(Ordering::Acquire);
    if previous != 0 && seq <= previous {
        return Err(Error::WebSocket {
            message: format!("account stream {channel} seq regressed from {previous} to {seq}"),
        });
    }
    channel_seq.store(seq, Ordering::Release);
    let data = envelope
        .get("data")
        .cloned()
        .ok_or_else(|| Error::WebSocket {
            message: format!("{channel} event has no data"),
        })?;
    match account_channel {
        AccountChannel::Order => {
            let mut order = serde_json::from_value::<OrderUpdate>(data)?;
            order.seq = seq;
            Ok(Some(AccountEvent::Order(order)))
        }
        AccountChannel::Position => {
            let mut position = serde_json::from_value::<PositionUpdate>(data)?;
            position.seq = seq;
            Ok(Some(AccountEvent::Position(position)))
        }
        AccountChannel::Trade => {
            let mut trade = serde_json::from_value::<TradeUpdate>(data)?;
            trade.seq = seq;
            Ok(Some(AccountEvent::Trade(trade)))
        }
        AccountChannel::Balance => {
            let mut balance = serde_json::from_value::<BalanceUpdate>(data)?;
            balance.seq = seq;
            Ok(Some(AccountEvent::Balance(balance)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::accept_async;

    #[test]
    fn parses_account_channel_fixtures_into_typed_events() {
        let health = AccountStreamHealth::new(1);
        let order = parse_account_event(
            r#"{"seq":35,"channel":"order","data":{"id":2547027,"cl_ord_id":"sxmk-run-q1b0","symbol":"XAG-USD","side":"buy","qty":"0.200","fill_qty":"0.200","fill_avg_price":"58.87","price":"58.87","status":"filled","reduce_only":false,"updated_at":"2026-07-12T22:10:04Z"}}"#,
            &health,
        )
        .unwrap()
        .unwrap();
        assert!(
            matches!(order, AccountEvent::Order(update) if update.order_id == 2_547_027 && update.seq == 35)
        );

        let position = parse_account_event(
            r#"{"seq":36,"channel":"position","data":{"id":80853,"symbol":"XAG-USD","qty":"0.201","entry_price":"58.88","realized_pnl":"7.21","status":"open","updated_at":"2026-07-12T22:10:04Z"}}"#,
            &health,
        )
        .unwrap()
        .unwrap();
        assert!(
            matches!(position, AccountEvent::Position(update) if update.qty == "0.201" && update.side.is_none() && update.seq == 36)
        );

        let short_position = parse_account_event(
            r#"{"seq":37,"channel":"position","data":{"id":80853,"symbol":"XAG-USD","qty":"-0.116","entry_price":"58.24","realized_pnl":"7.12","status":"open","updated_at":"2026-07-13T05:56:25Z"}}"#,
            &health,
        )
        .unwrap()
        .unwrap();
        assert!(
            matches!(short_position, AccountEvent::Position(update) if update.side.is_none() && update.qty == "-0.116" && update.seq == 37)
        );

        // The authenticated trade payload follows the user-trade shape and
        // must carry both stable identifiers for ledger deduplication.
        let trade = parse_account_event(
            r#"{"seq":38,"channel":"trade","data":{"id":409870,"order_id":1820682,"side":"sell","symbol":"BTC-USD","price":"121900","qty":"0.01","created_at":"2025-08-11T03:36:19.352620Z"}}"#,
            &health,
        )
        .unwrap()
        .unwrap();
        assert!(
            matches!(trade, AccountEvent::Trade(update) if update.trade_id == 409870 && update.order_id == 1820682 && update.side == OrderSide::Sell && update.seq == 38)
        );

        // This is the documented balance-stream shape. It is raw wallet
        // state, not the REST unified margin Balance model.
        let balance = parse_account_event(
            r#"{"seq":39,"channel":"balance","data":{"account_type":"perps","free":"906946.976225666","locked":"0.000000000","occupied":"0","token":"DUSD","total":"923207.752500717","updated_at":"2025-08-09T09:36:54.504639Z"}}"#,
            &health,
        )
        .unwrap()
        .unwrap();
        assert!(
            matches!(balance, AccountEvent::Balance(update) if update.total == "923207.752500717" && update.free == "906946.976225666" && update.seq == 39)
        );
    }

    #[test]
    fn rejects_trade_without_stable_trade_or_order_id() {
        let health = AccountStreamHealth::new(1);
        assert!(parse_account_event(
            r#"{"seq":1,"channel":"trade","data":{"id":0,"order_id":7,"side":"buy","symbol":"BTC-USD","price":"100","qty":"0.1","time":"2026-07-14T00:00:00Z"}}"#,
            &health,
        )
        .is_err());

        let health = AccountStreamHealth::new(1);
        assert!(parse_account_event(
            r#"{"seq":1,"channel":"trade","data":{"id":9,"order_id":0,"side":"buy","symbol":"BTC-USD","price":"100","qty":"0.1","time":"2026-07-14T00:00:00Z"}}"#,
            &health,
        )
        .is_err());
    }

    #[test]
    fn seq_regression_is_rejected() {
        let health = AccountStreamHealth::new(1);
        let latest = r#"{"seq":9,"channel":"trade","data":{"id":9,"order_id":7,"side":"buy","symbol":"BTC-USD","price":"100","qty":"0.1","time":"2026-07-14T00:00:00Z"}}"#;
        let stale = r#"{"seq":8,"channel":"trade","data":{"id":8,"order_id":7,"side":"buy","symbol":"BTC-USD","price":"100","qty":"0.1","time":"2026-07-14T00:00:00Z"}}"#;
        parse_account_event(latest, &health).unwrap();
        assert!(parse_account_event(stale, &health).is_err());
        assert_eq!(health.last_seq(AccountChannel::Trade), 9);
    }

    #[test]
    fn seq_is_monotonic_per_channel_and_allows_gaps() {
        let health = AccountStreamHealth::new(1);
        parse_account_event(
            r#"{"seq":100,"channel":"trade","data":{"id":100,"order_id":7,"side":"buy","symbol":"BTC-USD","price":"100","qty":"0.1","time":"2026-07-14T00:00:00Z"}}"#,
            &health,
        )
        .unwrap();
        let position = r#"{"seq":3,"channel":"position","data":{"symbol":"BTC-USD","qty":"0","entry_price":"0","realized_pnl":"0","status":"closed","updated_at":"now"}}"#;
        parse_account_event(position, &health).unwrap();
        parse_account_event(
            r#"{"seq":900,"channel":"trade","data":{"id":900,"order_id":7,"side":"buy","symbol":"BTC-USD","price":"100","qty":"0.1","time":"2026-07-14T00:00:00Z"}}"#,
            &health,
        )
        .unwrap();

        assert_eq!(health.last_seq(AccountChannel::Trade), 900);
        assert_eq!(health.last_seq(AccountChannel::Position), 3);
        assert_eq!(health.last_seq(AccountChannel::Order), 0);

        assert!(parse_account_event(position, &health).is_err());
    }

    #[test]
    fn auth_and_unknown_channels_do_not_change_seq_state() {
        let health = AccountStreamHealth::new(1);
        assert!(parse_account_event(
            r#"{"seq":99,"channel":"auth","data":{"code":200}}"#,
            &health,
        )
        .unwrap()
        .is_none());
        assert!(
            parse_account_event(r#"{"seq":100,"channel":"unknown","data":{}}"#, &health,)
                .unwrap()
                .is_none()
        );
        assert_eq!(health.last_seq(AccountChannel::Order), 0);
        assert_eq!(health.last_seq(AccountChannel::Position), 0);
        assert_eq!(health.last_seq(AccountChannel::Trade), 0);
        assert_eq!(health.last_seq(AccountChannel::Balance), 0);
    }

    #[test]
    fn account_auth_requires_explicit_numeric_success_code() {
        for malformed in [
            r#"{"channel":"auth","data":{"msg":"success"}}"#,
            r#"{"channel":"auth","data":{"code":null,"msg":"success"}}"#,
            r#"{"channel":"auth","data":{"code":"200","msg":"success"}}"#,
        ] {
            let envelope = serde_json::from_str(malformed).unwrap();
            assert!(matches!(
                validate_account_auth(&envelope),
                Err(Error::WebSocket { message })
                    if message.contains("no numeric data.code")
            ));
        }

        for success in [
            r#"{"channel":"auth","data":{"code":0,"msg":"success"}}"#,
            r#"{"channel":"auth","data":{"code":200,"msg":"success"}}"#,
        ] {
            let envelope = serde_json::from_str(success).unwrap();
            validate_account_auth(&envelope).unwrap();
        }

        let rejected = serde_json::json!({
            "channel": "auth",
            "data": { "code": 401, "msg": "token rejected" },
        });
        assert!(matches!(
            validate_account_auth(&rejected),
            Err(Error::AuthRequired { message, .. }) if message == "token rejected"
        ));
    }

    #[tokio::test]
    async fn connect_rejects_auth_without_numeric_code() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let _auth = websocket.next().await.unwrap().unwrap();
            websocket
                .send(Message::Text(
                    r#"{"seq":1,"channel":"auth","data":{"msg":"success"}}"#.into(),
                ))
                .await
                .unwrap();
        });

        let stream = AccountStream::with_url_and_token(url, "jwt", 3);
        match stream.connect(&[AccountChannel::Order]).await {
            Err(Error::WebSocket { message }) => {
                assert!(message.contains("no numeric data.code"));
            }
            Err(error) => panic!("unexpected malformed-auth error: {error:?}"),
            Ok(_) => panic!("malformed auth response must not create a healthy account stream"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn connect_waits_for_auth_and_delivers_order() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let auth = websocket
                .next()
                .await
                .unwrap()
                .unwrap()
                .into_text()
                .unwrap();
            let auth: serde_json::Value = serde_json::from_str(&auth).unwrap();
            assert_eq!(auth["auth"]["token"], "jwt");
            websocket
                .send(Message::Text(
                    r#"{"seq":1,"channel":"auth","data":{"code":200,"msg":"success"}}"#.into(),
                ))
                .await
                .unwrap();
            websocket
                .send(Message::Text(
                    r#"{"seq":2,"channel":"order","data":{"id":7,"cl_ord_id":"sxmk-run-q1b0","symbol":"XAG-USD","side":"buy","qty":"0.200","fill_qty":"0.200","fill_avg_price":"58.87","price":"58.87","status":"filled","updated_at":"now"}}"#.into(),
                ))
                .await
                .unwrap();
        });

        let stream = AccountStream::with_url_and_token(url, "jwt", 4);
        let (mut events, health, handle) = stream
            .connect(&[AccountChannel::Order, AccountChannel::Position])
            .await
            .unwrap();
        assert_eq!(
            events.recv().await,
            Some(AccountEvent::Connected { epoch: 4 })
        );
        assert!(
            matches!(events.recv().await, Some(AccountEvent::Order(order)) if order.order_id == 7)
        );
        server.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while health.is_healthy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn idle_connection_is_marked_unhealthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        // Server authenticates then stays connected but silent forever,
        // simulating a half-open connection that never errors or closes.
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(socket).await.unwrap();
            let _auth = websocket.next().await.unwrap().unwrap();
            websocket
                .send(Message::Text(
                    r#"{"seq":1,"channel":"auth","data":{"code":200,"msg":"success"}}"#.into(),
                ))
                .await
                .unwrap();
            // Absorb the client's pings without replying, then hold the socket
            // open so the client can only detect death via the idle timeout.
            while let Some(Ok(_)) = websocket.next().await {}
        });

        // Idle timeout well below the ping interval so the idle deadline, not a
        // ping write failure, is what trips health.
        let stream = AccountStream::with_url_and_token(url, "jwt", 7)
            .with_heartbeat(Duration::from_secs(10), Duration::from_millis(200));
        let (mut events, health, handle) = stream.connect(&[AccountChannel::Order]).await.unwrap();
        assert_eq!(
            events.recv().await,
            Some(AccountEvent::Connected { epoch: 7 })
        );

        let disconnect = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("idle timeout should surface a disconnect");
        match disconnect {
            Some(AccountEvent::Disconnected { reason }) => {
                assert!(reason.contains("idle"), "unexpected reason: {reason}");
            }
            other => panic!("expected idle Disconnected, got {other:?}"),
        }
        assert!(!health.is_healthy());
        handle.await.unwrap();
        server.abort();
    }
}
