use super::model::{optional_decimal, Decimal};
use anyhow::Result;
use standx_sdk::client::StandXClient;
use standx_sdk::websocket::{StandXWebSocket, WsMarketUpdate, WsMessage};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, RwLock};

/// Latest market data from the WebSocket feed. Values are pre-parsed on
/// receipt so cycle reads are lock-and-go.
#[derive(Default)]
pub(super) struct FeedState {
    mark: Option<f64>,
    mark_meta: Option<FeedMeta>,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    book_meta: Option<FeedMeta>,
    reconnect_issue: Option<WsSnapshotIssue>,
}

#[derive(Clone)]
struct FeedMeta {
    exchange_seq: Option<u64>,
    server_time: Option<String>,
    envelope_time: Option<String>,
    payload_time: Option<String>,
    received_at: Instant,
}

/// Observation-only metadata for explaining why the latest independently
/// published mark and book updates did or did not form a coherent snapshot.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct WsSnapshotDiagnostics {
    pub(super) mark_seq: Option<u64>,
    pub(super) book_seq: Option<u64>,
    pub(super) mark_server_time: Option<String>,
    pub(super) book_server_time: Option<String>,
    pub(super) mark_envelope_time: Option<String>,
    pub(super) book_envelope_time: Option<String>,
    pub(super) mark_payload_time: Option<String>,
    pub(super) book_payload_time: Option<String>,
    pub(super) mark_age_ms: Option<u64>,
    pub(super) book_age_ms: Option<u64>,
    pub(super) local_skew_ms: Option<u64>,
    pub(super) server_skew_ms: Option<u64>,
}

/// One acquired market input plus observation-only WS cache diagnostics.
pub(super) struct AcquiredMarketSnapshot {
    pub(super) mark: f64,
    pub(super) best_bid: Option<f64>,
    pub(super) best_ask: Option<f64>,
    pub(super) source: &'static str,
    pub(super) fallback_reason: Option<&'static str>,
    pub(super) ws_snapshot: Option<WsSnapshotDiagnostics>,
}

/// WS cache entries older than this fall back to REST for the cycle. REST
/// polling refreshed data once per interval, so 5s keeps freshness at least
/// as good as the old behavior while tolerating slow feed ticks.
const WS_STALE_AFTER: Duration = Duration::from_secs(5);
/// `price` and `depth_book` arrive on separate public channels at different
/// cadences. Cross-channel skew therefore shares the same budget as the
/// independent freshness check: both inputs may be used while each remains
/// fresh, with mark/mid divergence still enforced by maker preflight. Venue
/// time is preferred; local receive-time skew is used only when either venue
/// timestamp is unavailable.
const WS_SNAPSHOT_MAX_SKEW: Duration = WS_STALE_AFTER;
/// A socket can stay TCP-healthy while one subscribed channel stops yielding
/// usable updates. Rebuild the whole public connection when either channel
/// has been idle this long.
const MARKET_FEED_IDLE_AFTER: Duration = Duration::from_secs(15);
const MARKET_FEED_REBUILD_DELAY: Duration = Duration::from_secs(10);
const MARKET_FEED_IDLE_REBUILD_DELAY: Duration = Duration::from_secs(1);

/// Why the latest public WebSocket cache cannot safely be used for a maker
/// cycle. These stable labels are emitted with `cycle_summary` so a REST
/// fallback can be diagnosed from the uploaded JSON logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WsSnapshotIssue {
    WarmingUp,
    MarkStale,
    BookStale,
    MarkAndBookStale,
    PriceIdle,
    BookIdle,
    PriceAndBookIdle,
    StreamEnded,
    LocalSkew,
    ServerTimeSkew,
    InvalidSnapshot,
}

impl WsSnapshotIssue {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::WarmingUp => "ws_warming_up",
            Self::MarkStale => "ws_mark_stale",
            Self::BookStale => "ws_book_stale",
            Self::MarkAndBookStale => "ws_mark_and_book_stale",
            Self::PriceIdle => "ws_price_idle",
            Self::BookIdle => "ws_book_idle",
            Self::PriceAndBookIdle => "ws_price_and_book_idle",
            Self::StreamEnded => "ws_stream_ended",
            Self::LocalSkew => "ws_local_time_skew",
            Self::ServerTimeSkew => "ws_server_time_skew",
            Self::InvalidSnapshot => "ws_invalid_snapshot",
        }
    }

    pub(super) const fn is_idle(self) -> bool {
        matches!(
            self,
            Self::PriceIdle | Self::BookIdle | Self::PriceAndBookIdle
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FeedSnapshotVersion {
    mark_received_at: Instant,
    book_received_at: Instant,
}

impl FeedSnapshotVersion {
    pub(super) fn both_advanced_from(self, previous: Option<Self>) -> bool {
        previous.map_or(true, |previous| {
            self.mark_received_at > previous.mark_received_at
                && self.book_received_at > previous.book_received_at
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct ChannelFreshness {
    price: Instant,
    book: Instant,
}

impl ChannelFreshness {
    fn new(now: Instant) -> Self {
        Self {
            price: now,
            book: now,
        }
    }

    fn next_deadline(self) -> Instant {
        self.price.min(self.book) + MARKET_FEED_IDLE_AFTER
    }

    fn idle_issue(self, now: Instant) -> Option<WsSnapshotIssue> {
        let price_idle = now.saturating_duration_since(self.price) >= MARKET_FEED_IDLE_AFTER;
        let book_idle = now.saturating_duration_since(self.book) >= MARKET_FEED_IDLE_AFTER;
        match (price_idle, book_idle) {
            (true, true) => Some(WsSnapshotIssue::PriceAndBookIdle),
            (true, false) => Some(WsSnapshotIssue::PriceIdle),
            (false, true) => Some(WsSnapshotIssue::BookIdle),
            (false, false) => None,
        }
    }
}

fn parse_server_time_millis(value: &str) -> Option<i64> {
    let value = value.trim();
    if let Ok(raw) = value.parse::<i64>() {
        return Some(if raw.unsigned_abs() < 100_000_000_000 {
            raw.saturating_mul(1_000)
        } else {
            raw
        });
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn update_is_newer<T>(previous: Option<&FeedMeta>, update: &WsMarketUpdate<T>) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    if matches!(
        (previous.exchange_seq, update.seq),
        (Some(previous), Some(next)) if next <= previous
    ) {
        return false;
    }
    !matches!(
        (
        previous
            .server_time
            .as_deref()
            .and_then(parse_server_time_millis),
        update
            .server_time
            .as_deref()
            .and_then(parse_server_time_millis),
    ),
        (Some(previous), Some(next)) if next <= previous
    )
}

fn parse_optional_positive_price(value: Option<&str>) -> Option<Option<f64>> {
    match value {
        None => Some(None),
        Some(value) => optional_decimal(value, Decimal::Positive).map(Some),
    }
}

fn update_meta<T>(update: &WsMarketUpdate<T>) -> FeedMeta {
    FeedMeta {
        exchange_seq: update.seq,
        server_time: update.server_time.clone(),
        envelope_time: update.envelope_time.clone(),
        payload_time: update.payload_time.clone(),
        received_at: update.received_at,
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn ws_snapshot_diagnostics(state: &FeedState, now: Instant) -> WsSnapshotDiagnostics {
    let mark_meta = state.mark_meta.as_ref();
    let book_meta = state.book_meta.as_ref();
    let mark_server_time = mark_meta
        .and_then(|meta| meta.server_time.as_deref())
        .and_then(parse_server_time_millis);
    let book_server_time = book_meta
        .and_then(|meta| meta.server_time.as_deref())
        .and_then(parse_server_time_millis);

    WsSnapshotDiagnostics {
        mark_seq: mark_meta.and_then(|meta| meta.exchange_seq),
        book_seq: book_meta.and_then(|meta| meta.exchange_seq),
        mark_server_time: mark_meta.and_then(|meta| meta.server_time.clone()),
        book_server_time: book_meta.and_then(|meta| meta.server_time.clone()),
        mark_envelope_time: mark_meta.and_then(|meta| meta.envelope_time.clone()),
        book_envelope_time: book_meta.and_then(|meta| meta.envelope_time.clone()),
        mark_payload_time: mark_meta.and_then(|meta| meta.payload_time.clone()),
        book_payload_time: book_meta.and_then(|meta| meta.payload_time.clone()),
        mark_age_ms: mark_meta
            .map(|meta| duration_millis(now.saturating_duration_since(meta.received_at))),
        book_age_ms: book_meta
            .map(|meta| duration_millis(now.saturating_duration_since(meta.received_at))),
        local_skew_ms: mark_meta.zip(book_meta).map(|(mark, book)| {
            duration_millis(
                mark.received_at
                    .saturating_duration_since(book.received_at)
                    .max(book.received_at.saturating_duration_since(mark.received_at)),
            )
        }),
        server_skew_ms: mark_server_time
            .zip(book_server_time)
            .map(|(mark, book)| mark.abs_diff(book)),
    }
}

fn coherent_ws_snapshot(
    state: &FeedState,
    now: Instant,
) -> std::result::Result<(f64, Option<f64>, Option<f64>), WsSnapshotIssue> {
    let (Some(mark_meta), Some(book_meta)) = (state.mark_meta.as_ref(), state.book_meta.as_ref())
    else {
        return Err(WsSnapshotIssue::WarmingUp);
    };
    let mark_stale = now.saturating_duration_since(mark_meta.received_at) >= WS_STALE_AFTER;
    let book_stale = now.saturating_duration_since(book_meta.received_at) >= WS_STALE_AFTER;
    if mark_stale || book_stale {
        return Err(match (mark_stale, book_stale) {
            (true, true) => WsSnapshotIssue::MarkAndBookStale,
            (true, false) => WsSnapshotIssue::MarkStale,
            (false, true) => WsSnapshotIssue::BookStale,
            (false, false) => unreachable!("at least one cache entry is stale"),
        });
    }
    let mark_server_time = mark_meta
        .server_time
        .as_deref()
        .and_then(parse_server_time_millis);
    let book_server_time = book_meta
        .server_time
        .as_deref()
        .and_then(parse_server_time_millis);
    if let (Some(mark_time), Some(book_time)) = (mark_server_time, book_server_time) {
        if mark_time.abs_diff(book_time) > WS_SNAPSHOT_MAX_SKEW.as_millis() as u64 {
            return Err(WsSnapshotIssue::ServerTimeSkew);
        }
    } else {
        let local_skew = mark_meta
            .received_at
            .saturating_duration_since(book_meta.received_at)
            .max(
                book_meta
                    .received_at
                    .saturating_duration_since(mark_meta.received_at),
            );
        if local_skew > WS_SNAPSHOT_MAX_SKEW {
            return Err(WsSnapshotIssue::LocalSkew);
        }
    }
    let mark = state.mark.ok_or(WsSnapshotIssue::WarmingUp)?;
    validated_snapshot(mark, state.best_bid, state.best_ask, "ws")
        .map(|(mark, best_bid, best_ask, _)| (mark, best_bid, best_ask))
        .map_err(|_| WsSnapshotIssue::InvalidSnapshot)
}

pub(super) fn ws_snapshot_issue(state: &FeedState, now: Instant) -> Option<WsSnapshotIssue> {
    coherent_ws_snapshot(state, now)
        .err()
        .map(|issue| state.reconnect_issue.unwrap_or(issue))
}

pub(super) fn fresh_ws_sample(
    state: &FeedState,
) -> Option<(f64, Option<f64>, Option<f64>, FeedSnapshotVersion)> {
    let (mark, best_bid, best_ask) = coherent_ws_snapshot(state, Instant::now()).ok()?;
    let version = FeedSnapshotVersion {
        mark_received_at: state.mark_meta.as_ref()?.received_at,
        book_received_at: state.book_meta.as_ref()?.received_at,
    };
    Some((mark, best_bid, best_ask, version))
}

async fn reset_feed_state(state: &RwLock<FeedState>, issue: WsSnapshotIssue) {
    *state.write().await = FeedState {
        reconnect_issue: Some(issue),
        ..FeedState::default()
    };
}

/// Spawn the resident market-feed task: one public WS connection carrying
/// `price` + `depth_book`, written into a shared cache. The outer loop wraps
/// the SDK's internal 5-attempt reconnect — when the stream ends (attempts
/// exhausted or clean close), it rebuilds the connection from scratch, since
/// subscriptions only take effect when registered before `connect_managed()`.
pub(super) fn spawn_market_feed(
    symbol: String,
    verbose: bool,
    endpoints: standx_sdk::StandXEndpoints,
) -> (
    Arc<RwLock<FeedState>>,
    watch::Receiver<u64>,
    tokio::task::JoinHandle<()>,
) {
    let state = Arc::new(RwLock::new(FeedState::default()));
    let (tx, rx) = watch::channel(0u64);
    let state_task = state.clone();

    let handle = tokio::spawn(async move {
        let mut seq = 0u64;
        loop {
            let ws = match StandXWebSocket::without_auth_from_endpoints_with_verbose(
                &endpoints, verbose,
            ) {
                Ok(ws) => ws,
                Err(e) => {
                    eprintln!("⚠️  market feed setup failed: {e}; retrying in 10s");
                    tokio::time::sleep(MARKET_FEED_REBUILD_DELAY).await;
                    continue;
                }
            };
            let _ = ws.subscribe("price", Some(&symbol)).await;
            let _ = ws.subscribe("depth_book", Some(&symbol)).await;
            let (mut events, connection_handle) = match ws.connect_managed().await {
                Ok(connection) => connection,
                Err(e) => {
                    eprintln!("⚠️  market feed connect failed: {e}; retrying in 10s");
                    tokio::time::sleep(MARKET_FEED_REBUILD_DELAY).await;
                    continue;
                }
            };
            let mut freshness = ChannelFreshness::new(Instant::now());
            let rebuild_delay = loop {
                let idle_deadline = tokio::time::Instant::from_std(freshness.next_deadline());
                tokio::select! {
                    message = events.recv() => {
                        let Some(msg) = message else {
                            connection_handle.abort();
                            reset_feed_state(&state_task, WsSnapshotIssue::StreamEnded).await;
                            seq = seq.saturating_add(1);
                            let _ = tx.send(seq);
                            eprintln!("⚠️  market feed stream ended; rebuilding connection in 10s");
                            break MARKET_FEED_REBUILD_DELAY;
                        };
                        match &msg {
                            WsMessage::Connected => {
                                *state_task.write().await = FeedState::default();
                                freshness = ChannelFreshness::new(Instant::now());
                                seq = seq.saturating_add(1);
                                let _ = tx.send(seq);
                                continue;
                            }
                            WsMessage::Disconnected => {
                                reset_feed_state(&state_task, WsSnapshotIssue::StreamEnded).await;
                                seq = seq.saturating_add(1);
                                let _ = tx.send(seq);
                                continue;
                            }
                            _ => {}
                        }
                        let accepted = match msg {
                            WsMessage::Price(update)
                                if update.data.symbol.eq_ignore_ascii_case(&symbol) =>
                            {
                                let received_at = update.received_at;
                                if let Some(mark) =
                                    optional_decimal(&update.data.mark_price, Decimal::Positive)
                                {
                                    {
                                        let mut s = state_task.write().await;
                                        if update_is_newer(s.mark_meta.as_ref(), &update) {
                                            s.mark = Some(mark);
                                            s.mark_meta = Some(update_meta(&update));
                                            if s.book_meta.is_some() {
                                                s.reconnect_issue = None;
                                            }
                                            Some((true, received_at))
                                        } else {
                                            None
                                        }
                                    }
                                } else {
                                    None
                                }
                            }
                            WsMessage::Depth(update)
                                if update.data.symbol.eq_ignore_ascii_case(&symbol) =>
                            {
                                let received_at = update.received_at;
                                let parsed = (
                                    parse_optional_positive_price(update.data.best_bid()),
                                    parse_optional_positive_price(update.data.best_ask()),
                                );
                                if let (Some(best_bid), Some(best_ask)) = parsed {
                                    let mut s = state_task.write().await;
                                    if update_is_newer(s.book_meta.as_ref(), &update) {
                                        s.best_bid = best_bid;
                                        s.best_ask = best_ask;
                                        s.book_meta = Some(update_meta(&update));
                                        if s.mark_meta.is_some() {
                                            s.reconnect_issue = None;
                                        }
                                        Some((false, received_at))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        if let Some((price, received_at)) = accepted {
                            if price {
                                freshness.price = received_at;
                            } else {
                                freshness.book = received_at;
                            }
                            seq = seq.saturating_add(1);
                            let _ = tx.send(seq);
                        }
                    }
                    _ = tokio::time::sleep_until(idle_deadline) => {
                        let now = Instant::now();
                        let Some(issue) = freshness.idle_issue(now) else {
                            continue;
                        };
                        connection_handle.abort();
                        reset_feed_state(&state_task, issue).await;
                        seq = seq.saturating_add(1);
                        let _ = tx.send(seq);
                        eprintln!(
                            "⚠️  market feed effective-update watchdog fired (reason={}); rebuilding connection in 1s",
                            issue.as_str()
                        );
                        break MARKET_FEED_IDLE_REBUILD_DELAY;
                    }
                }
            };
            tokio::time::sleep(rebuild_delay).await;
        }
    });

    (state, rx, handle)
}

/// One market snapshot: WS cache when fresh, REST fallback otherwise
/// (startup warm-up, feed outage, or --no-ws).
pub(super) async fn market_snapshot(
    client: &StandXClient,
    symbol: &str,
    feed: Option<&Arc<RwLock<FeedState>>>,
) -> Result<AcquiredMarketSnapshot> {
    let mut ws_issue = None;
    let mut ws_snapshot = None;
    if let Some(feed) = feed {
        let s = feed.read().await;
        let now = Instant::now();
        ws_snapshot = Some(ws_snapshot_diagnostics(&s, now));
        match coherent_ws_snapshot(&s, now) {
            Ok((mark, best_bid, best_ask)) => {
                return Ok(AcquiredMarketSnapshot {
                    mark,
                    best_bid,
                    best_ask,
                    source: "ws",
                    fallback_reason: None,
                    ws_snapshot,
                });
            }
            Err(issue) => {
                ws_issue = Some(s.reconnect_issue.unwrap_or(issue).as_str());
            }
        }
    }

    let (price, depth) = tokio::join!(
        client.get_symbol_price(symbol),
        client.get_depth(symbol, Some(5))
    );
    let price = price?;
    let depth = depth?;
    let mark: f64 = price
        .mark_price
        .parse()
        .map_err(|_| anyhow::anyhow!("unparseable mark price: {}", price.mark_price))?;
    let best_bid: Option<f64> = depth.best_bid().and_then(|s| s.parse().ok());
    let best_ask: Option<f64> = depth.best_ask().and_then(|s| s.parse().ok());
    validated_snapshot(mark, best_bid, best_ask, "rest").map(
        |(mark, best_bid, best_ask, source)| AcquiredMarketSnapshot {
            mark,
            best_bid,
            best_ask,
            source,
            fallback_reason: ws_issue,
            ws_snapshot,
        },
    )
}

fn validated_snapshot(
    mark: f64,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    source: &'static str,
) -> Result<(f64, Option<f64>, Option<f64>, &'static str)> {
    if !mark.is_finite() || mark <= 0.0 {
        return Err(anyhow::anyhow!("invalid mark price from {source}: {mark}"));
    }
    if best_bid.is_some_and(|price| !price.is_finite() || price <= 0.0) {
        return Err(anyhow::anyhow!("invalid best bid from {source}"));
    }
    if best_ask.is_some_and(|price| !price.is_finite() || price <= 0.0) {
        return Err(anyhow::anyhow!("invalid best ask from {source}"));
    }
    Ok((mark, best_bid, best_ask, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_validation_accepts_valid_and_one_sided_books() {
        assert!(validated_snapshot(100.0, Some(99.9), Some(100.1), "test").is_ok());
        assert!(validated_snapshot(100.0, Some(99.9), None, "test").is_ok());
    }

    #[test]
    fn snapshot_validation_rejects_non_finite_values_but_preserves_crossed_book_for_preflight() {
        assert!(validated_snapshot(f64::NAN, Some(99.9), Some(100.1), "test").is_err());
        assert!(validated_snapshot(100.0, Some(f64::INFINITY), Some(100.1), "test").is_err());
        assert!(validated_snapshot(100.0, Some(100.1), Some(100.1), "test").is_ok());
    }

    fn meta(seq: u64, server_time: &str, received_at: Instant) -> FeedMeta {
        FeedMeta {
            exchange_seq: Some(seq),
            server_time: Some(server_time.to_string()),
            envelope_time: Some(server_time.to_string()),
            payload_time: Some(server_time.to_string()),
            received_at,
        }
    }

    #[test]
    fn regressed_sequence_or_server_time_does_not_replace_feed_state() {
        let now = Instant::now();
        let previous = meta(10, "2026-07-14T00:00:10Z", now);
        let regressed_seq = WsMarketUpdate {
            data: (),
            seq: Some(9),
            server_time: Some("2026-07-14T00:00:11Z".to_string()),
            envelope_time: Some("2026-07-14T00:00:11Z".to_string()),
            payload_time: None,
            received_at: now,
        };
        assert!(!update_is_newer(Some(&previous), &regressed_seq));
        let regressed_time = WsMarketUpdate {
            data: (),
            seq: Some(11),
            server_time: Some("2026-07-14T00:00:09Z".to_string()),
            envelope_time: Some("2026-07-14T00:00:09Z".to_string()),
            payload_time: None,
            received_at: now,
        };
        assert!(!update_is_newer(Some(&previous), &regressed_time));
        let duplicate_time = WsMarketUpdate {
            data: (),
            seq: None,
            server_time: Some("2026-07-14T00:00:10Z".to_string()),
            envelope_time: Some("2026-07-14T00:00:10Z".to_string()),
            payload_time: None,
            received_at: now,
        };
        assert!(!update_is_newer(Some(&previous), &duplicate_time));
    }

    #[test]
    fn effective_book_update_rejects_invalid_prices_but_accepts_empty_sides() {
        assert_eq!(parse_optional_positive_price(None), Some(None));
        assert_eq!(
            parse_optional_positive_price(Some("100.25")),
            Some(Some(100.25))
        );
        for value in ["0", "-1", "NaN", "not-a-price"] {
            assert_eq!(parse_optional_positive_price(Some(value)), None);
        }
    }

    #[test]
    fn coherent_snapshot_prefers_server_time_over_local_receive_skew() {
        let now = Instant::now();
        let state = FeedState {
            mark: Some(100.0),
            mark_meta: Some(meta(1, "2026-07-14T00:00:00Z", now)),
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            book_meta: Some(meta(
                1,
                "2026-07-14T00:00:00Z",
                now + Duration::from_secs(3),
            )),
            reconnect_issue: None,
        };
        assert!(coherent_ws_snapshot(&state, now + Duration::from_secs(3)).is_ok());
    }

    #[test]
    fn coherent_snapshot_accepts_channel_cadence_skew_within_freshness_budget() {
        let now = Instant::now();
        let state = FeedState {
            mark: Some(100.0),
            mark_meta: Some(meta(1, "2026-07-14T00:00:00Z", now)),
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            book_meta: Some(meta(2, "2026-07-14T00:00:03Z", now)),
            reconnect_issue: None,
        };
        assert!(coherent_ws_snapshot(&state, now).is_ok());
    }

    #[test]
    fn coherent_snapshot_rejects_server_skew_beyond_freshness_budget() {
        let now = Instant::now();
        let state = FeedState {
            mark: Some(100.0),
            mark_meta: Some(meta(1, "2026-07-14T00:00:00Z", now)),
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            book_meta: Some(meta(2, "2026-07-14T00:00:06Z", now)),
            reconnect_issue: None,
        };
        assert_eq!(
            coherent_ws_snapshot(&state, now),
            Err(WsSnapshotIssue::ServerTimeSkew)
        );
    }

    #[test]
    fn coherent_snapshot_accepts_local_cadence_skew_within_freshness_budget() {
        let now = Instant::now();
        let state = FeedState {
            mark: Some(100.0),
            mark_meta: Some(meta(1, "2026-07-14T00:00:00Z", now)),
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            book_meta: Some(FeedMeta {
                exchange_seq: Some(2),
                server_time: None,
                envelope_time: None,
                payload_time: None,
                received_at: now + Duration::from_secs(3),
            }),
            reconnect_issue: None,
        };
        assert!(coherent_ws_snapshot(&state, now + Duration::from_secs(3)).is_ok());
    }

    #[test]
    fn coherent_snapshot_rejects_local_skew_beyond_freshness_budget() {
        let now = Instant::now();
        let state = FeedState {
            mark: Some(100.0),
            mark_meta: Some(meta(1, "2026-07-14T00:00:00Z", now)),
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            book_meta: Some(FeedMeta {
                exchange_seq: Some(2),
                server_time: None,
                envelope_time: None,
                payload_time: None,
                received_at: now + Duration::from_secs(6),
            }),
            reconnect_issue: None,
        };
        assert_eq!(
            coherent_ws_snapshot(&state, now),
            Err(WsSnapshotIssue::LocalSkew)
        );
    }

    #[test]
    fn snapshot_diagnostics_preserve_raw_times_and_both_skew_domains() {
        let now = Instant::now();
        let mut mark = meta(10, "2026-07-14T00:00:01Z", now - Duration::from_millis(250));
        mark.envelope_time = Some("1752451201000".to_string());
        mark.payload_time = Some("2026-07-14T00:00:01Z".to_string());
        let mut book = meta(20, "2026-07-14T00:00:03Z", now - Duration::from_millis(50));
        book.envelope_time = Some("1752451203000".to_string());
        book.payload_time = Some("2026-07-14T00:00:02Z".to_string());
        let state = FeedState {
            mark: Some(100.0),
            mark_meta: Some(mark),
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            book_meta: Some(book),
            reconnect_issue: None,
        };

        let diagnostics = ws_snapshot_diagnostics(&state, now);

        assert_eq!(diagnostics.mark_seq, Some(10));
        assert_eq!(diagnostics.book_seq, Some(20));
        assert_eq!(diagnostics.mark_age_ms, Some(250));
        assert_eq!(diagnostics.book_age_ms, Some(50));
        assert_eq!(diagnostics.local_skew_ms, Some(200));
        assert_eq!(diagnostics.server_skew_ms, Some(2_000));
        assert_eq!(
            diagnostics.mark_envelope_time.as_deref(),
            Some("1752451201000")
        );
        assert_eq!(
            diagnostics.book_payload_time.as_deref(),
            Some("2026-07-14T00:00:02Z")
        );
    }

    #[test]
    fn coherent_snapshot_rejects_stale_channel_before_skew_checks() {
        let now = Instant::now();
        let state = FeedState {
            mark: Some(100.0),
            mark_meta: Some(meta(1, "2026-07-14T00:00:00Z", now)),
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            book_meta: Some(meta(2, "2026-07-14T00:00:00Z", now - WS_STALE_AFTER)),
            reconnect_issue: None,
        };
        assert_eq!(
            coherent_ws_snapshot(&state, now),
            Err(WsSnapshotIssue::BookStale)
        );
    }

    #[test]
    fn coherent_snapshot_reports_warmup_and_mark_staleness() {
        let now = Instant::now();
        let mut state = FeedState::default();
        assert_eq!(
            coherent_ws_snapshot(&state, now),
            Err(WsSnapshotIssue::WarmingUp)
        );

        state.mark = Some(100.0);
        state.mark_meta = Some(meta(1, "2026-07-14T00:00:00Z", now - WS_STALE_AFTER));
        state.best_bid = Some(99.9);
        state.best_ask = Some(100.1);
        state.book_meta = Some(meta(1, "2026-07-14T00:00:00Z", now));
        assert_eq!(
            coherent_ws_snapshot(&state, now),
            Err(WsSnapshotIssue::MarkStale)
        );
    }

    #[test]
    fn idle_watchdog_tracks_price_and_book_independently() {
        let now = Instant::now();
        let mut freshness = ChannelFreshness::new(now);
        assert_eq!(
            freshness.idle_issue(now + MARKET_FEED_IDLE_AFTER - Duration::from_millis(1)),
            None
        );

        freshness.price = now + Duration::from_secs(10);
        assert_eq!(
            freshness.idle_issue(now + MARKET_FEED_IDLE_AFTER),
            Some(WsSnapshotIssue::BookIdle)
        );

        freshness.book = now + MARKET_FEED_IDLE_AFTER;
        assert_eq!(
            freshness.idle_issue(now + Duration::from_secs(25)),
            Some(WsSnapshotIssue::PriceIdle)
        );

        let both_idle = ChannelFreshness::new(now);
        assert_eq!(
            both_idle.idle_issue(now + MARKET_FEED_IDLE_AFTER),
            Some(WsSnapshotIssue::PriceAndBookIdle)
        );
    }

    #[test]
    fn snapshot_version_requires_both_channels_to_advance() {
        let now = Instant::now();
        let previous = FeedSnapshotVersion {
            mark_received_at: now,
            book_received_at: now,
        };
        for offset in 1..=3 {
            assert!(!FeedSnapshotVersion {
                mark_received_at: now,
                book_received_at: now + Duration::from_millis(offset * 100),
            }
            .both_advanced_from(Some(previous)));
        }
        assert!(FeedSnapshotVersion {
            mark_received_at: now + Duration::from_secs(1),
            book_received_at: now + Duration::from_secs(1),
        }
        .both_advanced_from(Some(previous)));
    }

    #[test]
    fn reconnect_issue_explains_empty_cache_after_idle_reset() {
        let state = FeedState {
            reconnect_issue: Some(WsSnapshotIssue::PriceIdle),
            ..FeedState::default()
        };
        assert_eq!(
            ws_snapshot_issue(&state, Instant::now()),
            Some(WsSnapshotIssue::PriceIdle)
        );
    }
}
