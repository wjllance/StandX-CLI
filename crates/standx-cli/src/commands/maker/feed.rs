use super::model::{optional_decimal, Decimal};
use anyhow::Result;
use standx_sdk::client::StandXClient;
use standx_sdk::models::{OrderBook, Trade};
use standx_sdk::websocket::{StandXWebSocket, WsMarketUpdate, WsMessage};
use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;
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

const BOOK_LEVEL_CAPACITY: usize = 5;
const TRADE_TAPE_CAPACITY: usize = 256;
const TRADE_TAPE_WINDOW: Duration = Duration::from_secs(5);
const PUBLIC_TRADE_RAW_SAMPLE_LIMIT: usize = 50;

/// Observation-only book depth and public trades. This deliberately has a
/// different lock from [`FeedState`]: public trades may arrive much more often
/// than maker cycles read the mark/touch cache, and telemetry must not contend
/// with that decision-critical read path.
pub(super) struct MarketTelemetry {
    origin: Instant,
    book: Option<BookObservation>,
    tape: VecDeque<TradeObservation>,
}

#[derive(Clone, Debug, PartialEq)]
struct BookObservation {
    bid_levels: Vec<(f64, f64)>,
    ask_levels: Vec<(f64, f64)>,
    local_recv_ms: u64,
    received_at: Instant,
}

#[derive(Clone, Debug, PartialEq)]
struct TradeObservation {
    local_recv_ms: u64,
    id: u64,
    price: f64,
    qty: f64,
    side: Option<String>,
    is_taker: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct BookTelemetrySnapshot {
    pub(super) bid_levels: Option<Vec<(f64, f64)>>,
    pub(super) ask_levels: Option<Vec<(f64, f64)>>,
    pub(super) age_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct TapeTelemetrySnapshot {
    pub(super) count_5s: usize,
    pub(super) buy_qty_5s: f64,
    pub(super) sell_qty_5s: f64,
    pub(super) unknown_qty_5s: f64,
    pub(super) last_trade_age_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct MarketTelemetrySnapshot {
    pub(super) book: BookTelemetrySnapshot,
    pub(super) tape: TapeTelemetrySnapshot,
}

impl Default for MarketTelemetry {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl MarketTelemetry {
    fn new(origin: Instant) -> Self {
        Self {
            origin,
            book: None,
            tape: VecDeque::with_capacity(TRADE_TAPE_CAPACITY),
        }
    }

    fn local_recv_ms(&self, received_at: Instant) -> u64 {
        duration_millis(received_at.saturating_duration_since(self.origin))
    }

    fn observe_book(&mut self, book: &OrderBook, received_at: Instant) {
        self.book = Some(BookObservation {
            bid_levels: parse_depth_levels(&book.bids, true),
            ask_levels: parse_depth_levels(&book.asks, false),
            local_recv_ms: self.local_recv_ms(received_at),
            received_at,
        });
    }

    fn clear(&mut self) {
        self.book = None;
        self.tape.clear();
    }

    fn clear_book(&mut self) {
        self.book = None;
    }

    fn observe_trade(&mut self, trade: &Trade, received_at: Instant) {
        let (Some(price), Some(qty)) = (
            optional_decimal(&trade.price, Decimal::Positive),
            optional_decimal(&trade.qty, Decimal::Positive),
        ) else {
            return;
        };
        let local_recv_ms = self.tape.back().map_or_else(
            || self.local_recv_ms(received_at),
            |last| last.local_recv_ms.max(self.local_recv_ms(received_at)),
        );
        if self.tape.len() == TRADE_TAPE_CAPACITY {
            self.tape.pop_front();
        }
        self.tape.push_back(TradeObservation {
            local_recv_ms,
            id: trade.id,
            price,
            qty,
            side: trade.side.clone(),
            is_taker: trade.is_buyer_taker,
        });
    }

    pub(super) fn snapshot(
        &self,
        now: Instant,
        expected_book_received_at: Option<Instant>,
    ) -> MarketTelemetrySnapshot {
        let now_ms = self.local_recv_ms(now);
        let book = self
            .book
            .as_ref()
            .filter(|book| expected_book_received_at == Some(book.received_at))
            .map_or_else(BookTelemetrySnapshot::default, |book| {
                BookTelemetrySnapshot {
                    bid_levels: (!book.bid_levels.is_empty()).then(|| book.bid_levels.clone()),
                    ask_levels: (!book.ask_levels.is_empty()).then(|| book.ask_levels.clone()),
                    age_ms: Some(now_ms.saturating_sub(book.local_recv_ms)),
                }
            });
        let mut tape = TapeTelemetrySnapshot {
            last_trade_age_ms: self
                .tape
                .back()
                .map(|trade| now_ms.saturating_sub(trade.local_recv_ms)),
            ..TapeTelemetrySnapshot::default()
        };
        let window_ms = duration_millis(TRADE_TAPE_WINDOW);
        for trade in self
            .tape
            .iter()
            .filter(|trade| now_ms.saturating_sub(trade.local_recv_ms) <= window_ms)
        {
            tape.count_5s += 1;
            match trade.side.as_deref().map(str::trim) {
                Some(side) if side.eq_ignore_ascii_case("buy") => {
                    tape.buy_qty_5s += trade.qty;
                }
                Some(side) if side.eq_ignore_ascii_case("sell") => {
                    tape.sell_qty_5s += trade.qty;
                }
                _ => {
                    // `is_buyer_taker` has a serde default. It is retained in
                    // the tape but never used to invent a missing side.
                    tape.unknown_qty_5s += trade.qty;
                }
            }
        }
        MarketTelemetrySnapshot { book, tape }
    }
}

fn parse_depth_levels(levels: &[[String; 2]], bids: bool) -> Vec<(f64, f64)> {
    let mut parsed: Vec<_> = levels
        .iter()
        .filter_map(|level| {
            let price = optional_decimal(&level[0], Decimal::Positive)?;
            let qty = optional_decimal(&level[1], Decimal::Positive)?;
            Some((price, qty))
        })
        .collect();
    parsed.sort_by(|left, right| {
        if bids {
            right.0.total_cmp(&left.0)
        } else {
            left.0.total_cmp(&right.0)
        }
    });
    parsed.truncate(BOOK_LEVEL_CAPACITY);
    parsed
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
    /// Exact WS book-cache version used for this acquisition. Observation
    /// telemetry is rendered only when its independently locked depth copy
    /// carries this same receive instant.
    pub(super) book_received_at: Option<Instant>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FreshnessChannel {
    Price,
    Book,
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
/// `price` + `depth_book` + observation-only `public_trade`, written into a
/// decision cache and a separately locked bounded telemetry store. The outer
/// loop wraps the SDK's internal 5-attempt reconnect — when the stream ends
/// (attempts exhausted or clean close), it rebuilds the connection from
/// scratch, since subscriptions only take effect when registered before
/// `connect_managed()`.
pub(super) struct SpawnedMarketFeed {
    pub(super) state: Arc<RwLock<FeedState>>,
    pub(super) telemetry: Arc<RwLock<MarketTelemetry>>,
    pub(super) updates: watch::Receiver<u64>,
    pub(super) handle: tokio::task::JoinHandle<()>,
}

pub(super) fn spawn_market_feed(
    symbol: String,
    verbose: bool,
    endpoints: standx_sdk::StandXEndpoints,
) -> SpawnedMarketFeed {
    let state = Arc::new(RwLock::new(FeedState::default()));
    let telemetry = Arc::new(RwLock::new(MarketTelemetry::default()));
    let (tx, rx) = watch::channel(0u64);
    let state_task = state.clone();
    let telemetry_task = telemetry.clone();
    let public_trade_raw_sample_budget = Arc::new(AtomicUsize::new(PUBLIC_TRADE_RAW_SAMPLE_LIMIT));

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
            let ws = ws.with_public_trade_raw_sample_budget(public_trade_raw_sample_budget.clone());
            let _ = ws.subscribe("price", Some(&symbol)).await;
            let _ = ws.subscribe("depth_book", Some(&symbol)).await;
            let _ = ws.subscribe("public_trade", Some(&symbol)).await;
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
                            telemetry_task.write().await.clear();
                            seq = seq.saturating_add(1);
                            let _ = tx.send(seq);
                            eprintln!("⚠️  market feed stream ended; rebuilding connection in 10s");
                            break MARKET_FEED_REBUILD_DELAY;
                        };
                        match &msg {
                            WsMessage::Connected => {
                                *state_task.write().await = FeedState::default();
                                telemetry_task.write().await.clear();
                                freshness = ChannelFreshness::new(Instant::now());
                                seq = seq.saturating_add(1);
                                let _ = tx.send(seq);
                                continue;
                            }
                            WsMessage::Disconnected => {
                                reset_feed_state(&state_task, WsSnapshotIssue::StreamEnded).await;
                                telemetry_task.write().await.clear();
                                seq = seq.saturating_add(1);
                                let _ = tx.send(seq);
                                continue;
                            }
                            _ => {}
                        }
                        match &msg {
                            WsMessage::Trade(trade)
                                if trade.symbol.as_deref().map_or(
                                    true,
                                    |trade_symbol| trade_symbol.eq_ignore_ascii_case(&symbol),
                                ) =>
                            {
                                telemetry_task
                                    .write()
                                    .await
                                    .observe_trade(trade, Instant::now());
                            }
                            _ => {}
                        }
                        let accepted = match &msg {
                            WsMessage::Price(update)
                                if update.data.symbol.eq_ignore_ascii_case(&symbol) =>
                            {
                                let received_at = update.received_at;
                                if let Some(mark) =
                                    optional_decimal(&update.data.mark_price, Decimal::Positive)
                                {
                                    {
                                        let mut s = state_task.write().await;
                                        if update_is_newer(s.mark_meta.as_ref(), update) {
                                            s.mark = Some(mark);
                                            s.mark_meta = Some(update_meta(update));
                                            if s.book_meta.is_some() {
                                                s.reconnect_issue = None;
                                            }
                                            Some((FreshnessChannel::Price, received_at))
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
                                    if update_is_newer(s.book_meta.as_ref(), update) {
                                        s.best_bid = best_bid;
                                        s.best_ask = best_ask;
                                        s.book_meta = Some(update_meta(update));
                                        if s.mark_meta.is_some() {
                                            s.reconnect_issue = None;
                                        }
                                        drop(s);
                                        telemetry_task
                                            .write()
                                            .await
                                            .observe_book(&update.data, received_at);
                                        Some((FreshnessChannel::Book, received_at))
                                    } else {
                                        None
                                    }
                                } else {
                                    telemetry_task.write().await.clear_book();
                                    None
                                }
                            }
                            _ => None,
                        };
                        if let Some((channel, received_at)) = accepted {
                            match channel {
                                FreshnessChannel::Price => freshness.price = received_at,
                                FreshnessChannel::Book => freshness.book = received_at,
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
                        telemetry_task.write().await.clear();
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

    SpawnedMarketFeed {
        state,
        telemetry,
        updates: rx,
        handle,
    }
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
                    book_received_at: s.book_meta.as_ref().map(|meta| meta.received_at),
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
            book_received_at: None,
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
    fn depth_telemetry_is_sorted_bounded_and_does_not_replace_touch_derivation() {
        let book = OrderBook {
            symbol: "BTC-USD".to_string(),
            bids: vec![
                ["101".to_string(), "NaN".to_string()],
                ["99".to_string(), "1".to_string()],
                ["100".to_string(), "2".to_string()],
                ["98".to_string(), "3".to_string()],
                ["97".to_string(), "4".to_string()],
                ["96".to_string(), "5".to_string()],
                ["95".to_string(), "6".to_string()],
            ],
            asks: vec![
                ["102".to_string(), "bad".to_string()],
                ["104".to_string(), "1".to_string()],
                ["103".to_string(), "2".to_string()],
                ["105".to_string(), "3".to_string()],
                ["106".to_string(), "4".to_string()],
                ["107".to_string(), "5".to_string()],
                ["108".to_string(), "6".to_string()],
            ],
            timestamp: String::new(),
        };

        // Existing touch derivation considers only price validity. Telemetry
        // additionally requires valid quantity and must never replace it.
        assert_eq!(book.best_bid(), Some("101"));
        assert_eq!(book.best_ask(), Some("102"));
        assert_eq!(
            parse_depth_levels(&book.bids, true),
            vec![
                (100.0, 2.0),
                (99.0, 1.0),
                (98.0, 3.0),
                (97.0, 4.0),
                (96.0, 5.0)
            ]
        );
        assert_eq!(
            parse_depth_levels(&book.asks, false),
            vec![
                (103.0, 2.0),
                (104.0, 1.0),
                (105.0, 3.0),
                (106.0, 4.0),
                (107.0, 5.0)
            ]
        );

        let malformed = vec![
            ["NaN".to_string(), "1".to_string()],
            ["100".to_string(), "0".to_string()],
            ["bad".to_string(), "bad".to_string()],
        ];
        assert!(parse_depth_levels(&malformed, true).is_empty());
    }

    #[test]
    fn depth_telemetry_is_null_unless_it_matches_the_acquired_book_version() {
        let origin = Instant::now();
        let acquired_at = origin + Duration::from_millis(10);
        let later_update_at = origin + Duration::from_millis(20);
        let book = OrderBook {
            symbol: "BTC-USD".to_string(),
            bids: vec![["99.9".to_string(), "2.0".to_string()]],
            asks: vec![["100.1".to_string(), "3.0".to_string()]],
            timestamp: String::new(),
        };
        let mut telemetry = MarketTelemetry::new(origin);
        telemetry.observe_book(&book, later_update_at);

        let mismatched = telemetry.snapshot(later_update_at, Some(acquired_at));
        assert_eq!(mismatched.book, BookTelemetrySnapshot::default());

        let matched = telemetry.snapshot(later_update_at, Some(later_update_at));
        assert_eq!(matched.book.bid_levels, Some(vec![(99.9, 2.0)]));
        assert_eq!(matched.book.ask_levels, Some(vec![(100.1, 3.0)]));
        assert_eq!(matched.book.age_ms, Some(0));
    }

    #[test]
    fn public_trade_absence_or_activity_does_not_affect_feed_freshness_or_version() {
        let now = Instant::now();
        let state = FeedState {
            mark: Some(100.0),
            mark_meta: Some(meta(1, "2026-07-14T00:00:00Z", now)),
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            book_meta: Some(meta(2, "2026-07-14T00:00:00Z", now)),
            reconnect_issue: None,
        };
        let freshness = ChannelFreshness::new(now);
        let version = FeedSnapshotVersion {
            mark_received_at: state.mark_meta.as_ref().unwrap().received_at,
            book_received_at: state.book_meta.as_ref().unwrap().received_at,
        };
        let mut telemetry = MarketTelemetry::new(now);

        assert_eq!(telemetry.snapshot(now, None).tape.count_5s, 0);
        assert_eq!(
            freshness.idle_issue(now + MARKET_FEED_IDLE_AFTER - Duration::from_millis(1)),
            None
        );
        let mut price_and_book_only = ChannelFreshness::new(now);
        for seconds in [10, 20, 30] {
            let received_at = now + Duration::from_secs(seconds);
            price_and_book_only.price = received_at;
            price_and_book_only.book = received_at;
            assert_eq!(
                price_and_book_only
                    .idle_issue(received_at + MARKET_FEED_IDLE_AFTER - Duration::from_millis(1)),
                None
            );
        }
        assert!(coherent_ws_snapshot(&state, now + Duration::from_secs(3)).is_ok());

        let trade: Trade = serde_json::from_value(serde_json::json!({
            "id": 7,
            "price": "100.0",
            "qty": "1.5",
            "side": "buy",
            "is_taker": true
        }))
        .unwrap();
        telemetry.observe_trade(&trade, now + Duration::from_millis(10));

        assert_eq!(
            telemetry
                .snapshot(now + Duration::from_millis(10), None)
                .tape
                .count_5s,
            1
        );
        assert_eq!(
            version,
            FeedSnapshotVersion {
                mark_received_at: state.mark_meta.as_ref().unwrap().received_at,
                book_received_at: state.book_meta.as_ref().unwrap().received_at,
            }
        );
        assert_eq!(
            freshness.idle_issue(now + MARKET_FEED_IDLE_AFTER),
            Some(WsSnapshotIssue::PriceAndBookIdle)
        );
    }

    #[test]
    fn trade_without_side_stays_unknown_instead_of_using_defaulted_taker_flag() {
        let now = Instant::now();
        let trade: Trade = serde_json::from_value(serde_json::json!({
            "id": 8,
            "price": "100.0",
            "qty": "2.5"
        }))
        .unwrap();
        assert_eq!(trade.side, None);
        assert!(!trade.is_buyer_taker);

        let mut telemetry = MarketTelemetry::new(now);
        telemetry.observe_trade(&trade, now);
        let snapshot = telemetry.snapshot(now, None);

        assert_eq!(telemetry.tape.back().unwrap().side, None);
        assert_eq!(snapshot.tape.buy_qty_5s, 0.0);
        assert_eq!(snapshot.tape.sell_qty_5s, 0.0);
        assert_eq!(snapshot.tape.unknown_qty_5s, 2.5);
    }

    #[test]
    fn trade_tape_evicts_oldest_entries_at_fixed_capacity() {
        let now = Instant::now();
        let mut telemetry = MarketTelemetry::new(now);
        for id in 0..300u64 {
            let trade = Trade {
                id,
                time: String::new(),
                price: "100.0".to_string(),
                qty: "1.0".to_string(),
                side: Some("sell".to_string()),
                is_buyer_taker: id % 2 == 0,
                fee_asset: None,
                fee_qty: None,
                pnl: None,
                order_id: None,
                symbol: Some("BTC-USD".to_string()),
                value: None,
            };
            telemetry.observe_trade(&trade, now + Duration::from_millis(id));
        }

        assert_eq!(telemetry.tape.len(), TRADE_TAPE_CAPACITY);
        let first = telemetry.tape.front().unwrap();
        let last = telemetry.tape.back().unwrap();
        assert_eq!((first.id, first.price, first.is_taker), (44, 100.0, true));
        assert_eq!((last.id, last.price, last.is_taker), (299, 100.0, false));
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
