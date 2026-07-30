//! `lag-recorder` — a read-only diagnostic that records StandX and Hyperliquid
//! mark prices side by side so an offline analyzer can measure how far StandX's
//! aggregated mark price lags the leading venue.
//!
//! This command performs **no** authentication and places **no** orders. It only
//! subscribes to public market-data feeds and appends one NDJSON line per price
//! update to a file. It deliberately shares nothing with the maker trading path.
//!
//! Timestamp discipline: every record is stamped at the moment its producer task
//! reads the message off the socket, using one process-wide monotonic
//! `Instant` origin (comparable across tasks) plus a UTC wall-clock for
//! cross-run correlation. Stamping at receipt — not at the consumer — keeps the
//! two venues on one common clock without channel-queuing skew. The measured
//! lag still carries a fixed differential-network-latency offset between the two
//! venues, so the recorder must run from the same host/region as the maker for
//! the number to be representative (see docs/plan for the honest caveats).

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use standx_sdk::websocket::{StandXWebSocket, WsMessage};
use standx_sdk::StandXEndpoints;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const HYPERLIQUID_WS_URL: &str = "wss://api.hyperliquid.xyz/ws";
/// How long a producer waits before rebuilding a dropped connection.
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Application-level keepalive cadence for the Hyperliquid socket.
const HL_PING_INTERVAL: Duration = Duration::from_secs(30);

/// One recorded price observation from a single venue.
///
/// Every field except the identity/timestamp trio is optional because each feed
/// populates a different subset (StandX `price` carries mark/index/last, StandX
/// `depth_book` carries best bid/ask, Hyperliquid carries mark/mid/index). The
/// schema is kept stable — absent values serialize as `null` — so the offline
/// analyzer can rely on a fixed set of keys.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct LagRecord {
    /// `"standx"` or `"hyperliquid"`.
    source: &'static str,
    /// Monotonic milliseconds since this process's recorder origin. The only
    /// clock used for cross-venue lag.
    local_recv_ms: i64,
    /// Wall-clock receipt time, for correlation across runs/systems only.
    local_recv_utc: String,
    mark: Option<f64>,
    mid: Option<f64>,
    index: Option<f64>,
    last: Option<f64>,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    /// Venue-provided timestamp, copied verbatim (never used for lag).
    server_time: Option<String>,
    /// Venue sequence number when present.
    seq: Option<u64>,
}

/// Derive the Hyperliquid coin symbol from a StandX symbol by stripping a
/// trailing `-USD`/`-USDT` quote suffix (`HYPE-USD` -> `HYPE`).
fn derive_hl_coin(symbol: &str) -> String {
    let upper = symbol.to_uppercase();
    for suffix in ["-USDT", "-USD"] {
        if let Some(base) = upper.strip_suffix(suffix) {
            return base.to_string();
        }
    }
    upper
}

/// Parse a JSON scalar that may be a string or a number into `f64`.
fn json_f64(value: Option<&serde_json::Value>) -> Option<f64> {
    match value? {
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Build a Hyperliquid record from a decoded `activeAssetCtx` message, or return
/// `None` for any other message (subscription acks, pongs, other channels).
fn hyperliquid_record(
    value: &serde_json::Value,
    local_recv_ms: i64,
    local_recv_utc: String,
) -> Option<LagRecord> {
    if value.get("channel")?.as_str()? != "activeAssetCtx" {
        return None;
    }
    let ctx = value.get("data")?.get("ctx")?;
    Some(LagRecord {
        source: "hyperliquid",
        local_recv_ms,
        local_recv_utc,
        mark: json_f64(ctx.get("markPx")),
        mid: json_f64(ctx.get("midPx")),
        index: json_f64(ctx.get("oraclePx")),
        last: None,
        best_bid: None,
        best_ask: None,
        server_time: None,
        seq: None,
    })
}

/// Milliseconds since the recorder origin, as a signed integer.
fn elapsed_ms(origin: Instant) -> i64 {
    origin.elapsed().as_millis() as i64
}

pub async fn handle_lag_recorder(
    symbol: String,
    hl_coin: Option<String>,
    out: String,
    flush_secs: u64,
    status_secs: u64,
    verbose: bool,
    endpoints: &StandXEndpoints,
) -> Result<()> {
    let hl_coin = hl_coin.unwrap_or_else(|| derive_hl_coin(&symbol));
    let origin = Instant::now();

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out)
        .with_context(|| format!("failed to open output file '{out}'"))?;
    let mut writer = BufWriter::new(file);

    eprintln!(
        "lag-recorder: StandX symbol={symbol} vs Hyperliquid coin={hl_coin} -> {out}\n\
         read-only; no auth, no orders. Press Ctrl+C to stop.",
    );

    let (tx, mut rx) = mpsc::channel::<LagRecord>(4096);
    let standx = tokio::spawn(run_standx(
        symbol.clone(),
        origin,
        tx.clone(),
        verbose,
        endpoints.clone(),
    ));
    let hyperliquid = tokio::spawn(run_hyperliquid(hl_coin.clone(), origin, tx));

    let flush_secs = flush_secs.max(1);
    let status_secs = status_secs.max(1);
    let mut flush_tick = tokio::time::interval(Duration::from_secs(flush_secs));
    let mut status_tick = tokio::time::interval(Duration::from_secs(status_secs));
    flush_tick.reset();
    status_tick.reset();

    let mut standx_count: u64 = 0;
    let mut hl_count: u64 = 0;
    let mut standx_last_mark: Option<f64> = None;
    let mut hl_last_mark: Option<f64> = None;

    loop {
        tokio::select! {
            biased;
            _ = shutdown_signal() => {
                eprintln!("lag-recorder: shutdown signal received, flushing…");
                break;
            }
            maybe = rx.recv() => {
                match maybe {
                    Some(record) => {
                        match record.source {
                            "standx" => {
                                standx_count += 1;
                                if record.mark.is_some() {
                                    standx_last_mark = record.mark;
                                }
                            }
                            _ => {
                                hl_count += 1;
                                if record.mark.is_some() {
                                    hl_last_mark = record.mark;
                                }
                            }
                        }
                        write_record(&mut writer, &record)?;
                    }
                    None => {
                        // Both producers dropped their senders — should not
                        // happen while they loop, but exit cleanly if it does.
                        eprintln!("lag-recorder: all producers ended, stopping.");
                        break;
                    }
                }
            }
            _ = flush_tick.tick() => {
                writer.flush().ok();
            }
            _ = status_tick.tick() => {
                emit_status(standx_count, hl_count, standx_last_mark, hl_last_mark);
            }
        }
    }

    standx.abort();
    hyperliquid.abort();

    // Drain anything already queued so the tail of the file is complete.
    while let Ok(record) = rx.try_recv() {
        write_record(&mut writer, &record)?;
    }
    writer.flush().context("failed to flush output file")?;

    eprintln!(
        "lag-recorder: stopped. standx_records={standx_count} hyperliquid_records={hl_count}",
    );
    Ok(())
}

fn write_record(writer: &mut BufWriter<std::fs::File>, record: &LagRecord) -> Result<()> {
    let line = serde_json::to_string(record).context("failed to serialize record")?;
    writeln!(writer, "{line}").context("failed to write record")?;
    Ok(())
}

fn emit_status(standx: u64, hl: u64, standx_mark: Option<f64>, hl_mark: Option<f64>) {
    let diff = match (standx_mark, hl_mark) {
        (Some(s), Some(h)) if h != 0.0 => format!("{:+.2}bps", (s / h - 1.0) * 1e4),
        _ => "n/a".to_string(),
    };
    eprintln!(
        "lag-recorder: standx={standx} (mark {}) | hyperliquid={hl} (mark {}) | standx-hl {diff}",
        fmt_mark(standx_mark),
        fmt_mark(hl_mark),
    );
}

fn fmt_mark(mark: Option<f64>) -> String {
    mark.map(|m| format!("{m}")).unwrap_or_else(|| "-".into())
}

/// Complete on SIGINT or (unix) SIGTERM so supervisors and Ctrl+C both stop the
/// recorder gracefully.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(stream) => stream,
            Err(_) => return std::future::pending().await,
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(_) => return std::future::pending().await,
        };
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// StandX producer: subscribe to the public `price` and `depth_book` channels
/// (no auth) and forward each update as a `LagRecord`. Rebuilds the connection
/// when the managed stream ends.
async fn run_standx(
    symbol: String,
    origin: Instant,
    tx: mpsc::Sender<LagRecord>,
    verbose: bool,
    endpoints: StandXEndpoints,
) {
    loop {
        if let Err(error) = standx_session(&symbol, origin, &tx, verbose, &endpoints).await {
            eprintln!("lag-recorder: StandX feed error: {error:#}");
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn standx_session(
    symbol: &str,
    origin: Instant,
    tx: &mpsc::Sender<LagRecord>,
    verbose: bool,
    endpoints: &StandXEndpoints,
) -> Result<()> {
    let ws = StandXWebSocket::without_auth_from_endpoints_with_verbose(endpoints, verbose)
        .context("failed to build StandX websocket client")?;
    let _ = ws.subscribe("price", Some(symbol)).await;
    let _ = ws.subscribe("depth_book", Some(symbol)).await;
    let (mut rx, _handle) = ws
        .connect_managed()
        .await
        .context("failed to connect StandX websocket")?;

    while let Some(msg) = rx.recv().await {
        let now_ms = elapsed_ms(origin);
        let utc = chrono::Utc::now().to_rfc3339();
        let record = match msg {
            WsMessage::Price(update) => LagRecord {
                source: "standx",
                local_recv_ms: now_ms,
                local_recv_utc: utc,
                mark: update.data.mark_price.parse().ok(),
                mid: None,
                index: update.data.index_price.parse().ok(),
                last: update.data.last_price.parse().ok(),
                best_bid: None,
                best_ask: None,
                server_time: update.server_time.clone(),
                seq: update.seq,
            },
            WsMessage::Depth(update) => LagRecord {
                source: "standx",
                local_recv_ms: now_ms,
                local_recv_utc: utc,
                mark: None,
                mid: None,
                index: None,
                last: None,
                best_bid: update.data.best_bid().and_then(|p| p.parse().ok()),
                best_ask: update.data.best_ask().and_then(|p| p.parse().ok()),
                server_time: update.server_time.clone(),
                seq: update.seq,
            },
            _ => continue,
        };
        if tx.send(record).await.is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// Hyperliquid producer: subscribe to `activeAssetCtx` for the coin and forward
/// mark/mid/oracle prices. Rebuilds the connection on any error/close.
async fn run_hyperliquid(coin: String, origin: Instant, tx: mpsc::Sender<LagRecord>) {
    loop {
        if let Err(error) = hyperliquid_session(&coin, origin, &tx).await {
            eprintln!("lag-recorder: Hyperliquid feed error: {error:#}");
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn hyperliquid_session(
    coin: &str,
    origin: Instant,
    tx: &mpsc::Sender<LagRecord>,
) -> Result<()> {
    use futures::{SinkExt, StreamExt};

    let (stream, _response) = tokio_tungstenite::connect_async(HYPERLIQUID_WS_URL)
        .await
        .context("failed to connect Hyperliquid websocket")?;
    let (mut write, mut read) = stream.split();

    let subscribe = serde_json::json!({
        "method": "subscribe",
        "subscription": { "type": "activeAssetCtx", "coin": coin },
    })
    .to_string();
    write
        .send(Message::Text(subscribe.into()))
        .await
        .context("failed to send Hyperliquid subscription")?;

    let mut ping = tokio::time::interval(HL_PING_INTERVAL);
    ping.reset();

    loop {
        tokio::select! {
            _ = ping.tick() => {
                let frame = serde_json::json!({ "method": "ping" }).to_string();
                write
                    .send(Message::Text(frame.into()))
                    .await
                    .context("failed to send Hyperliquid ping")?;
            }
            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(msg)) => msg,
                    Some(Err(error)) => return Err(error).context("Hyperliquid stream error"),
                    None => return Ok(()),
                };
                match msg {
                    Message::Text(text) => {
                        let now_ms = elapsed_ms(origin);
                        let utc = chrono::Utc::now().to_rfc3339();
                        let value: serde_json::Value = match serde_json::from_str(&text) {
                            Ok(value) => value,
                            Err(_) => continue,
                        };
                        if let Some(record) = hyperliquid_record(&value, now_ms, utc) {
                            if tx.send(record).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        write
                            .send(Message::Pong(payload))
                            .await
                            .context("failed to answer Hyperliquid ping")?;
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_hl_coin_strips_quote_suffix() {
        assert_eq!(derive_hl_coin("HYPE-USD"), "HYPE");
        assert_eq!(derive_hl_coin("BTC-USDT"), "BTC");
        assert_eq!(derive_hl_coin("hype-usd"), "HYPE");
        assert_eq!(derive_hl_coin("HYPE"), "HYPE");
    }

    #[test]
    fn hyperliquid_record_parses_active_asset_ctx() {
        let value = serde_json::json!({
            "channel": "activeAssetCtx",
            "data": {
                "coin": "HYPE",
                "ctx": {
                    "markPx": "59.123",
                    "midPx": "59.130",
                    "oraclePx": "59.100",
                    "funding": "0.0000125"
                }
            }
        });
        let record = hyperliquid_record(&value, 42, "2026-07-19T00:00:00Z".to_string())
            .expect("should decode activeAssetCtx");
        assert_eq!(record.source, "hyperliquid");
        assert_eq!(record.local_recv_ms, 42);
        assert_eq!(record.mark, Some(59.123));
        assert_eq!(record.mid, Some(59.130));
        assert_eq!(record.index, Some(59.100));
        assert_eq!(record.last, None);
    }

    #[test]
    fn hyperliquid_record_ignores_other_channels() {
        let value = serde_json::json!({
            "channel": "subscriptionResponse",
            "data": { "method": "subscribe" }
        });
        assert!(hyperliquid_record(&value, 0, "t".to_string()).is_none());
    }

    #[test]
    fn lag_record_serializes_stable_schema() {
        let record = LagRecord {
            source: "standx",
            local_recv_ms: 100,
            local_recv_utc: "2026-07-19T00:00:00Z".to_string(),
            mark: Some(59.5),
            mid: None,
            index: Some(59.4),
            last: Some(59.6),
            best_bid: None,
            best_ask: None,
            server_time: Some("2026-07-19T00:00:00.5Z".to_string()),
            seq: Some(7),
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        assert_eq!(value["source"], "standx");
        assert_eq!(value["mark"], 59.5);
        assert!(value["mid"].is_null());
        assert_eq!(value["seq"], 7);
        // All schema keys must always be present.
        for key in [
            "source",
            "local_recv_ms",
            "local_recv_utc",
            "mark",
            "mid",
            "index",
            "last",
            "best_bid",
            "best_ask",
            "server_time",
            "seq",
        ] {
            assert!(value.get(key).is_some(), "missing key {key}");
        }
    }
}
