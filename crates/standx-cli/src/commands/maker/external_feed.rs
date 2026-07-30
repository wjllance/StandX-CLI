//! Hyperliquid midPx feed for the external-price guard (stage 3 v1).
//!
//! A resident background task keeps the latest leader mid price in a shared
//! cache; the maker cycle reads it and normalizes freshness into the typed
//! [`standx_maker::ExternalDivergence`] input. The client mirrors the proven
//! `lag-recorder` Hyperliquid session (same endpoint, channel, and keepalive)
//! but is deliberately a separate, minimal copy: the diagnostic tool and the
//! trading path must not share evolution.
//!
//! Failure semantics are OPEN by design: any disconnect, parse gap, or silence
//! simply leaves the cache stale, the guard controller treats stale samples as
//! absent, and quoting continues unaffected. This task can never stop the
//! maker.

use super::model::Decimal;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, RwLock};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

const HYPERLIQUID_WS_URL: &str = "wss://api.hyperliquid.xyz/ws";
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Latest leader observation. `received_at` is the local monotonic receipt
/// time; the cycle derives `age_ms` from it when building the typed input.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct ExternalFeedState {
    pub mid: Option<f64>,
    pub received_at: Option<Instant>,
}

/// Derive the Hyperliquid coin from a StandX symbol (`HYPE-USD` -> `HYPE`).
pub(super) fn leader_coin(symbol: &str) -> String {
    let upper = symbol.to_uppercase();
    for suffix in ["-USDT", "-USD"] {
        if let Some(base) = upper.strip_suffix(suffix) {
            return base.to_string();
        }
    }
    upper
}

/// Spawn the resident leader-feed task. The watch channel ticks on every
/// accepted update so the cycle loop can wake early on fresh divergence.
pub(super) fn spawn_external_feed(
    coin: String,
) -> (
    Arc<RwLock<ExternalFeedState>>,
    watch::Receiver<u64>,
    JoinHandle<()>,
) {
    let state = Arc::new(RwLock::new(ExternalFeedState::default()));
    let (tx, rx) = watch::channel(0u64);
    let task_state = Arc::clone(&state);
    let handle = tokio::spawn(async move {
        let mut seq: u64 = 0;
        loop {
            if let Err(error) = feed_session(&coin, &task_state, &tx, &mut seq).await {
                eprintln!(
                    "⚠️ external guard feed error (fail-open, quoting unaffected): {error:#}"
                );
            }
            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(RECONNECT_DELAY).await;
        }
    });
    (state, rx, handle)
}

async fn feed_session(
    coin: &str,
    state: &Arc<RwLock<ExternalFeedState>>,
    tx: &watch::Sender<u64>,
    seq: &mut u64,
) -> anyhow::Result<()> {
    let (stream, _response) = tokio_tungstenite::connect_async(HYPERLIQUID_WS_URL).await?;
    let (mut write, mut read) = stream.split();

    let subscribe = serde_json::json!({
        "method": "subscribe",
        "subscription": { "type": "activeAssetCtx", "coin": coin },
    })
    .to_string();
    write.send(Message::Text(subscribe.into())).await?;

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.reset();

    loop {
        tokio::select! {
            _ = ping.tick() => {
                let frame = serde_json::json!({ "method": "ping" }).to_string();
                write.send(Message::Text(frame.into())).await?;
            }
            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(msg)) => msg,
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                };
                match msg {
                    Message::Text(text) => {
                        if let Some(mid) = parse_mid(&text) {
                            {
                                let mut guard = state.write().await;
                                guard.mid = Some(mid);
                                guard.received_at = Some(Instant::now());
                            }
                            *seq += 1;
                            if tx.send(*seq).is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        write.send(Message::Pong(payload)).await?;
                    }
                    Message::Close(_) => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}

/// Extract midPx from an `activeAssetCtx` frame; anything else is `None`.
fn parse_mid(text: &str) -> Option<f64> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if value.get("channel")?.as_str()? != "activeAssetCtx" {
        return None;
    }
    match value.get("data")?.get("ctx")?.get("midPx")? {
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
    // Same "usable price" rule the maker's own feed applies.
    .filter(|mid| Decimal::Positive.admits(*mid))
}

/// Raw leader-vs-mark divergence in bps plus the sample age. `None` on any
/// missing or non-positive value (fail-open).
pub(super) fn raw_divergence(
    state: ExternalFeedState,
    mark: f64,
    now: Instant,
) -> Option<(f64, u64)> {
    let mid = state.mid?;
    let received_at = state.received_at?;
    if !mark.is_finite() || mark <= 0.0 {
        return None;
    }
    Some((
        (mid / mark - 1.0) * 1e4,
        now.saturating_duration_since(received_at).as_millis() as u64,
    ))
}

/// Slow EMA baseline over the raw divergence.
///
/// HL midPx and StandX mark carry a persistent static basis (~-14bps observed
/// on HYPE in the 2026-07-22 paper smoke — venue premium/funding structure,
/// not a snipe signal). Feeding raw levels into the guard would latch it
/// permanently on one side — the exact uptime failure that rejected stage-3
/// v0. The guard therefore triggers on the EXCESS divergence over this
/// baseline: jumps (seconds) pass through, the basis (minutes-stable) is
/// absorbed. The first sample initializes the baseline, so startup basis
/// never fires the guard.
#[derive(Debug)]
pub(super) struct DivergenceBaseline {
    ema_bps: Option<f64>,
    last_update: Option<Instant>,
    half_life: Duration,
}

impl DivergenceBaseline {
    pub(super) fn new(half_life_secs: u64) -> Self {
        Self {
            ema_bps: None,
            last_update: None,
            half_life: Duration::from_secs(half_life_secs.max(1)),
        }
    }

    /// Excess of `raw_bps` over the baseline as of BEFORE this observation
    /// (a fresh jump is never partially absorbed by its own sample), then
    /// fold the observation into the EMA.
    pub(super) fn observe(&mut self, raw_bps: f64, now: Instant) -> f64 {
        let excess = self.peek(raw_bps);
        match (self.ema_bps, self.last_update) {
            (Some(ema), Some(last)) => {
                let dt = now.saturating_duration_since(last).as_secs_f64();
                let alpha = 1.0 - 0.5f64.powf(dt / self.half_life.as_secs_f64());
                self.ema_bps = Some(ema + alpha * (raw_bps - ema));
            }
            _ => self.ema_bps = Some(raw_bps),
        }
        self.last_update = Some(now);
        excess
    }

    /// Excess without updating the baseline (used by the early-wake check;
    /// the cycle is the single writer).
    pub(super) fn peek(&self, raw_bps: f64) -> f64 {
        match self.ema_bps {
            Some(ema) => raw_bps - ema,
            None => 0.0,
        }
    }

    /// Current baseline (telemetry).
    pub(super) fn basis_bps(&self) -> Option<f64> {
        self.ema_bps
    }
}

/// Normalize the cached leader state into the core's typed input: raw
/// divergence minus the slow baseline. Updates the baseline as a side effect
/// (call once per cycle). `None` fails open.
pub(super) fn divergence_input(
    state: ExternalFeedState,
    mark: f64,
    now: Instant,
    baseline: &mut DivergenceBaseline,
) -> Option<standx_maker::ExternalDivergence> {
    let (raw_bps, age_ms) = raw_divergence(state, mark, now)?;
    let excess = baseline.observe(raw_bps, now);
    Some(standx_maker::ExternalDivergence {
        divergence_bps: excess,
        age_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leader_coin_strips_quote_suffix() {
        assert_eq!(leader_coin("HYPE-USD"), "HYPE");
        assert_eq!(leader_coin("btc-usdt"), "BTC");
        assert_eq!(leader_coin("HYPE"), "HYPE");
    }

    #[test]
    fn parse_mid_accepts_active_asset_ctx_only() {
        let frame = r#"{"channel":"activeAssetCtx","data":{"coin":"HYPE","ctx":{"markPx":"59.1","midPx":"59.2","oraclePx":"59.0"}}}"#;
        assert_eq!(parse_mid(frame), Some(59.2));
        assert_eq!(
            parse_mid(r#"{"channel":"subscriptionResponse","data":{}}"#),
            None
        );
        assert_eq!(parse_mid("not json"), None);
        let bad = r#"{"channel":"activeAssetCtx","data":{"ctx":{"midPx":"-1"}}}"#;
        assert_eq!(parse_mid(bad), None);
    }

    #[test]
    fn divergence_input_normalizes_and_fails_open() {
        let now = Instant::now();
        let state = ExternalFeedState {
            mid: Some(60.06),
            received_at: Some(now),
        };
        let mut baseline = DivergenceBaseline::new(300);
        // First sample initializes the baseline: startup basis yields ZERO
        // excess, never a guard trigger.
        let input = divergence_input(state, 60.0, now, &mut baseline).unwrap();
        assert_eq!(input.divergence_bps, 0.0);
        assert_eq!(input.age_ms, 0);
        assert!((baseline.basis_bps().unwrap() - 10.0).abs() < 1e-9);

        let mut baseline = DivergenceBaseline::new(300);
        assert!(divergence_input(ExternalFeedState::default(), 60.0, now, &mut baseline).is_none());
        assert!(divergence_input(state, 0.0, now, &mut baseline).is_none());
        assert!(divergence_input(state, f64::NAN, now, &mut baseline).is_none());
    }

    #[test]
    fn baseline_absorbs_static_basis_but_passes_jumps_through() {
        let start = Instant::now();
        let mut baseline = DivergenceBaseline::new(300);

        // Persistent -14bps basis (the HYPE paper-smoke observation): after
        // initialization every repeat reads ~zero excess.
        assert_eq!(baseline.observe(-14.0, start), 0.0);
        for i in 1..=10 {
            let now = start + Duration::from_secs(3 * i);
            let excess = baseline.observe(-14.0, now);
            assert!(excess.abs() < 1e-9, "static basis must not fire: {excess}");
        }

        // A fresh 8bps jump on top of the basis passes through (measured
        // against the pre-jump baseline, not absorbed by its own sample).
        let jump_at = start + Duration::from_secs(60);
        let excess = baseline.observe(-14.0 + 8.0, jump_at);
        assert!(
            (excess - 8.0).abs() < 0.1,
            "jump must pass through: {excess}"
        );

        // peek never mutates.
        let before = baseline.basis_bps().unwrap();
        let _ = baseline.peek(50.0);
        assert_eq!(baseline.basis_bps().unwrap(), before);

        // A sustained shift is slowly absorbed into the basis (half-life
        // behavior): after one half-life the excess halves.
        let mut shifted = DivergenceBaseline::new(300);
        shifted.observe(0.0, start);
        let one_half_life = start + Duration::from_secs(300);
        shifted.observe(10.0, one_half_life);
        let after = shifted.basis_bps().unwrap();
        assert!((after - 5.0).abs() < 0.1, "half-life absorption: {after}");
    }
}
