use super::feed::{MarketTelemetrySnapshot, WsSnapshotDiagnostics};
use super::{ORDER_HISTORY_LIMIT, TRADE_LOOKBACK_LIMIT};
use crate::cli::OutputFormat;
use anyhow::Result;
use standx_maker::{
    MakerAccountProjection, MakerConfig, MakerLedger, MakerStats, MarketDataMode,
    OrderLatencyTracker, RequestTimeoutPhase, RestingQuote, SizeSkewController, SpreadController,
    VolBreaker,
};
use standx_sdk::account_stream::AccountStreamHealth;
use standx_sdk::client::StandXClient;
use standx_sdk::models::{Balance, FundingHistoryEntry, Order, Position, Trade};
use standx_sdk::order_response::{OrderCommandSender, OrderResponseHealth};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const ACCOUNT_AUDIT_INTERVAL: Duration = Duration::from_secs(30);
const REST_POSITION_RECHECK_DELAY: Duration = Duration::from_secs(3);
/// Funding is hourly on this venue, so 200 rows is >8 days of cover for a
/// window that starts at session start. A batch that comes back at exactly this
/// size may be truncated and is reported rather than silently accepted.
pub(super) const FUNDING_HISTORY_LIMIT: u32 = 200;
const BALANCE_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const BALANCE_MAX_STALE: Duration = Duration::from_secs(60);
const BALANCE_REFRESH_RETRY: Duration = Duration::from_secs(5);
/// Stage 5-b: how old the authoritative balance may be while an account hard
/// floor is armed — one whole refresh interval (30s) plus one retry window
/// (5s), i.e. a single missed refresh that then succeeded. Beyond that the
/// snapshot is no longer evidence of solvency and the cycle fails closed.
/// Deliberately tighter than [`BALANCE_MAX_STALE`], which only bounds how long
/// a *cached* balance may keep feeding telemetry and alerts.
pub(super) const BALANCE_FLOOR_MAX_AGE: Duration = Duration::from_secs(35);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OrderRequestKind {
    Place,
    Cancel,
    InventoryExit,
}

impl OrderRequestKind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Place => "place",
            Self::Cancel => "cancel",
            Self::InventoryExit => "inventory_exit",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TrackedOrderRequest {
    kind: OrderRequestKind,
    submitted_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TimedOutOrderRequest {
    pub(super) request_id: String,
    pub(super) kind: OrderRequestKind,
    pub(super) phase: RequestTimeoutPhase,
    pub(super) age: Duration,
}

/// CLI-owned monotonic deadlines for requests registered in the pure account
/// projection. Timing stays outside `standx-maker`; correlation stays inside.
#[derive(Debug, Default)]
pub(super) struct OrderRequestDeadlines {
    requests: HashMap<String, TrackedOrderRequest>,
}

impl OrderRequestDeadlines {
    pub(super) fn record(
        &mut self,
        request_id: String,
        kind: OrderRequestKind,
        submitted_at: Instant,
    ) {
        self.requests
            .insert(request_id, TrackedOrderRequest { kind, submitted_at });
    }

    pub(super) fn retain_pending(&mut self, projection: &MakerAccountProjection) {
        self.requests
            .retain(|request_id, _| projection.has_pending_request_lifecycle(request_id));
    }

    pub(super) fn next_deadline(&self, timeout: Duration) -> Option<Instant> {
        self.requests
            .values()
            .map(|request| request.submitted_at + timeout)
            .min()
    }

    pub(super) fn timed_out(
        &self,
        projection: &MakerAccountProjection,
        now: Instant,
        timeout: Duration,
    ) -> Option<TimedOutOrderRequest> {
        self.requests
            .iter()
            .filter_map(|(request_id, request)| {
                let age = now.saturating_duration_since(request.submitted_at);
                (age >= timeout).then_some((request_id, request, age))
            })
            .min_by(|(left_id, left, _), (right_id, right, _)| {
                left.submitted_at
                    .cmp(&right.submitted_at)
                    .then_with(|| left_id.cmp(right_id))
            })
            .map(|(request_id, request, age)| TimedOutOrderRequest {
                request_id: request_id.clone(),
                kind: request.kind,
                phase: if projection.pending_request(request_id).is_some() {
                    RequestTimeoutPhase::Acknowledgement
                } else {
                    RequestTimeoutPhase::AccountOrder
                },
                age,
            })
    }
}

pub(super) struct CycleRequest<'a> {
    pub(super) client: &'a StandXClient,
    pub(super) symbol: &'a str,
    pub(super) cfg: &'a MakerConfig,
    pub(super) live: bool,
    pub(super) cycle: u64,
    pub(super) mark: f64,
    pub(super) best_bid: Option<f64>,
    pub(super) best_ask: Option<f64>,
    pub(super) market_data_mode: MarketDataMode,
    pub(super) market_source: &'static str,
    /// Observation-only classification. The first successfully committed
    /// cycle after bounded recovery is grouped separately in latency output.
    pub(super) recovery: bool,
    pub(super) market_fallback_reason: Option<&'static str>,
    /// Observation-only metadata from the public WS cache. This never feeds
    /// strategy, safety, or source-selection decisions.
    pub(super) ws_snapshot: Option<&'a WsSnapshotDiagnostics>,
    /// Observation-only bounded book/tape snapshot. It is consumed solely by
    /// cycle-summary rendering and never by maker planning or safety logic.
    pub(super) market_telemetry: &'a MarketTelemetrySnapshot,
    pub(super) max_divergence_bps: f64,
    pub(super) inventory_exit_pct: f64,
    pub(super) inventory_exit_qty: f64,
    /// Stage 5-b account hard floors (quote units, 0 = disarmed). Evaluated
    /// inside the cycle, before any order work, so a breached balance cannot
    /// add exposure in the very cycle that observed it.
    pub(super) stop_equity_below: f64,
    pub(super) stop_margin_below: f64,
    /// Latched supervisor wind-down request (SIGUSR1 from the A/B
    /// orchestrator): stop quoting and flatten via reduce-only exits.
    pub(super) wind_down: bool,
    /// Venue-quantity tolerance; positions at or below it count as flat.
    pub(super) qty_tolerance: f64,
    pub(super) session_started_at: i64,
    pub(super) run_order_prefix: &'a str,
    pub(super) starting_position: f64,
    pub(super) output_format: OutputFormat,
    pub(super) order_commands: Option<&'a OrderCommandSender>,
    pub(super) order_response_health: Option<&'a OrderResponseHealth>,
    pub(super) account_stream_health: Option<&'a AccountStreamHealth>,
    /// UTC-like epoch derived from a fixed wall-clock anchor plus monotonic
    /// elapsed time; used only for observation and replay metrics.
    pub(super) performance_time_ms: i64,
}

pub(super) struct CycleState<'a> {
    pub(super) resting: &'a mut Vec<RestingQuote>,
    pub(super) account_projection: Option<&'a mut MakerAccountProjection>,
    pub(super) inventory_exit_pending: &'a mut bool,
    pub(super) ledger: &'a mut MakerLedger,
    pub(super) sim_position: &'a mut f64,
    pub(super) stats: &'a mut MakerStats,
    pub(super) breaker: &'a mut VolBreaker,
    pub(super) spread_controller: &'a mut SpreadController,
    pub(super) size_skew_controller: &'a mut SizeSkewController,
    /// Stage 3 v1 nonlinear price-skew strength (disabled ≡ legacy linear).
    pub(super) nonlinear_skew: standx_maker::NonlinearSkewConfig,
    pub(super) external_skew: standx_maker::ExternalSkewConfig,
    /// In-venue touch-mid center offset (default disabled).
    pub(super) microprice: standx_maker::MicroPriceConfig,
    pub(super) external_skew_previous_shift_bps: &'a mut f64,
    pub(super) external_excess_telemetry: &'a mut ExternalExcessTelemetry,
    pub(super) guard_controller: &'a mut standx_maker::GuardController,
    /// Caller-normalized external leader observation for this cycle; `None`
    /// when the guard is disabled or the feed has no usable sample.
    pub(super) external_divergence: Option<TimedExternalDivergence>,
    /// Current divergence-basis EMA (telemetry only).
    pub(super) external_basis_bps: Option<f64>,
    pub(super) order_request_deadlines: Option<&'a mut OrderRequestDeadlines>,
    /// Live-only REST polling state. It is deliberately a CLI concern: it
    /// controls I/O cadence and cached account presentation, not strategy.
    pub(super) live_account_poll: Option<&'a mut LiveAccountPollState>,
    /// Observation-only command lifecycle tracker. It never feeds strategy or
    /// safety decisions.
    pub(super) order_latency: Option<&'a mut OrderLatencyTracker>,
    pub(super) latency_started: Option<Instant>,
}

/// An external observation plus the instant at which its feed age was
/// normalized. Account/audit I/O may delay planning, so the elapsed time must
/// be folded back into `age_ms` immediately before the guard consumes it.
#[derive(Clone, Copy, Debug)]
pub(super) struct TimedExternalDivergence {
    pub(super) value: standx_maker::ExternalDivergence,
    pub(super) normalized_at: Instant,
}

impl TimedExternalDivergence {
    pub(super) fn at(self, now: Instant) -> standx_maker::ExternalDivergence {
        let elapsed_ms = u64::try_from(
            now.saturating_duration_since(self.normalized_at)
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        standx_maker::ExternalDivergence {
            divergence_bps: self.value.divergence_bps,
            age_ms: self.value.age_ms.saturating_add(elapsed_ms),
        }
    }
}

/// Last guard-normalized excess sample available to fill telemetry. The expiry
/// preserves the guard's `max_age_ms` fail-open semantics between cycles without
/// adding a feed read or a second signal calculation.
#[derive(Debug, Default)]
pub(super) struct ExternalExcessTelemetry {
    value_bps: Option<f64>,
    valid_until: Option<Instant>,
}

impl ExternalExcessTelemetry {
    pub(super) fn observe(
        &mut self,
        value_bps: Option<f64>,
        input: Option<standx_maker::ExternalDivergence>,
        max_age_ms: u64,
        now: Instant,
    ) {
        let Some((value_bps, input)) = value_bps.filter(|value| value.is_finite()).zip(
            input.filter(|sample| sample.age_ms <= max_age_ms && sample.divergence_bps.is_finite()),
        ) else {
            self.value_bps = None;
            self.valid_until = None;
            return;
        };
        let remaining_ms = max_age_ms - input.age_ms;
        self.value_bps = Some(value_bps);
        self.valid_until = Some(now + Duration::from_millis(remaining_ms));
    }

    pub(super) fn current(&self, now: Instant) -> Option<f64> {
        match (self.value_bps, self.valid_until) {
            (Some(value), Some(valid_until)) if now <= valid_until && value.is_finite() => {
                Some(value)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct CycleResult {
    pub(super) places: u64,
    pub(super) cancels: u64,
    pub(super) holds: u64,
    pub(super) fills: u64,
    /// Latest account snapshot (live mode only; `None` in paper mode).
    pub(super) balance: Option<Balance>,
}

pub(super) struct AccountAudit {
    pub(super) open_orders: Vec<Order>,
    pub(super) positions: Vec<Position>,
    pub(super) filled_orders: Vec<Order>,
    pub(super) trades: Vec<Trade>,
    /// Funding cashflows since session start. Hourly on this venue, so the 30s
    /// audit cadence over-samples it comfortably.
    ///
    /// Deliberately a `Result` the audit does **not** propagate: funding is
    /// attribution-only, while this same audit backs position reconciliation
    /// and recovery. A transient failure or a schema change on
    /// `/api/query_funding_history` must not be able to fail a safety
    /// reconciliation — it may only mark the attribution incomplete.
    pub(super) funding: std::result::Result<Vec<FundingHistoryEntry>, String>,
}

/// Cached, REST-derived account presentation plus the low-frequency full
/// account audit cadence. Healthy maker cycles use the authenticated account
/// stream projection and perform no account REST reads.
pub(super) struct LiveAccountPollState {
    balance: Balance,
    /// Funding row ids already accounted for. `last_id` on the venue endpoint
    /// pages *backward* (older rows), so there is no "since" cursor to lean on —
    /// dedup is by id, the same way trade dedup works.
    applied_funding_ids: std::collections::HashSet<i64>,
    balance_updated_at: Instant,
    next_balance_refresh_at: Instant,
    next_account_audit_at: Instant,
    rest_position_recheck_at: Option<Instant>,
}

impl LiveAccountPollState {
    pub(super) fn new(balance: Balance, now: Instant) -> Self {
        Self {
            balance,
            balance_updated_at: now,
            applied_funding_ids: std::collections::HashSet::new(),
            next_balance_refresh_at: now + BALANCE_REFRESH_INTERVAL,
            next_account_audit_at: now + ACCOUNT_AUDIT_INTERVAL,
            rest_position_recheck_at: None,
        }
    }

    pub(super) fn balance(&self) -> &Balance {
        &self.balance
    }

    pub(super) fn applied_funding_ids(&mut self) -> &mut std::collections::HashSet<i64> {
        &mut self.applied_funding_ids
    }

    pub(super) fn balance_refresh_due(&self, now: Instant) -> bool {
        now >= self.next_balance_refresh_at
    }

    /// Make the next maker cycle refresh the authoritative unified balance.
    ///
    /// The account stream's `balance` payload is a wallet-level view and does
    /// not expose the derived `equity` / `cross_available` values used by the
    /// configured account-risk floors. A stream update therefore acts as an
    /// immediate refresh trigger rather than being reinterpreted as those REST
    /// fields.
    pub(super) fn request_balance_refresh(&mut self, now: Instant) {
        self.next_balance_refresh_at = self.next_balance_refresh_at.min(now);
    }

    pub(super) fn account_audit_due(&self, now: Instant) -> bool {
        now >= self.next_account_audit_at
    }

    pub(super) fn rest_position_recheck_pending(&self) -> bool {
        self.rest_position_recheck_at.is_some()
    }

    /// Defer the first REST-only position disagreement for one bounded
    /// confirmation window. Returns true only when a successful audit still
    /// disagrees at or after the scheduled recheck deadline.
    pub(super) fn record_rest_position_mismatch(&mut self, now: Instant) -> bool {
        match self.rest_position_recheck_at {
            Some(deadline) => now >= deadline,
            None => {
                let deadline = now + REST_POSITION_RECHECK_DELAY;
                self.rest_position_recheck_at = Some(deadline);
                self.next_account_audit_at = deadline;
                false
            }
        }
    }

    pub(super) fn balance_is_within_stale_limit(&self, now: Instant) -> bool {
        now.duration_since(self.balance_updated_at) <= BALANCE_MAX_STALE
    }

    /// How old the cached authoritative balance is.
    pub(super) fn balance_age(&self, now: Instant) -> Duration {
        now.duration_since(self.balance_updated_at)
    }

    pub(super) fn record_balance_refresh(&mut self, balance: Balance, now: Instant) {
        self.balance = balance;
        self.balance_updated_at = now;
        self.next_balance_refresh_at = now + BALANCE_REFRESH_INTERVAL;
    }

    pub(super) fn record_balance_refresh_failure(&mut self, now: Instant) {
        self.next_balance_refresh_at = now + BALANCE_REFRESH_RETRY;
    }

    pub(super) fn record_account_audit(&mut self, now: Instant) {
        self.rest_position_recheck_at = None;
        self.next_account_audit_at = now + ACCOUNT_AUDIT_INTERVAL;
    }
}

pub(super) async fn fetch_account_audit(
    client: &StandXClient,
    symbol: &str,
    session_started_at: i64,
    now: i64,
) -> Result<AccountAudit> {
    let (open_orders, positions, filled_orders, trades, funding) = tokio::join!(
        client.get_open_orders(Some(symbol)),
        client.get_positions(Some(symbol)),
        client.get_order_history(Some(symbol), Some(ORDER_HISTORY_LIMIT)),
        client.get_user_trades(symbol, session_started_at, now, Some(TRADE_LOOKBACK_LIMIT)),
        // No cursor: `last_id` pages backward on this venue, so incrementality
        // comes from id dedup on the apply side, not from the request.
        client.get_funding_history(
            Some(symbol),
            Some(session_started_at),
            None,
            Some(FUNDING_HISTORY_LIMIT)
        ),
    );
    Ok(AccountAudit {
        open_orders: open_orders?,
        positions: positions?,
        filled_orders: filled_orders?,
        trades: trades?,
        // Severity inversion guard: `?` here would let a telemetry endpoint
        // abort order/position reconciliation.
        funding: funding.map_err(|error| error.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use standx_maker::{
        AccountProjectionEvent, OrderObservation, OrderResponseContinuity, ProjectionPendingPlace,
    };
    use standx_sdk::models::OrderSide;

    const TEST_RUN_PREFIX: &str = "sxmk-deadline-";

    fn pending_place(request_id: &str) -> ProjectionPendingPlace {
        ProjectionPendingPlace {
            request_id: request_id.to_owned(),
            client_order_id: format!("{TEST_RUN_PREFIX}q00000001b0"),
            side: OrderSide::Buy,
            price: 100.0,
            qty: 0.2,
            level: 0,
            ref_center: 100.0,
            cycle: 1,
        }
    }

    fn observed_order() -> OrderObservation {
        OrderObservation {
            order_id: 7,
            client_order_id: Some(format!("{TEST_RUN_PREFIX}q00000001b0")),
            side: OrderSide::Buy,
            price: 100.0,
            open_qty: 0.2,
            terminal: false,
        }
    }

    fn projection_with_pending_place(request_id: &str) -> MakerAccountProjection {
        let mut projection = MakerAccountProjection::new(1, TEST_RUN_PREFIX, 0.0, 0.005, 0.00005);
        projection.apply(
            1,
            AccountProjectionEvent::PlaceSubmitted(pending_place(request_id)),
        );
        projection
    }

    struct JwtGuard {
        original: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl JwtGuard {
        fn set() -> Self {
            // Share the crate-wide env lock so this STANDX_JWT mutation cannot
            // run concurrently with env reads in other modules' tests (e.g. the
            // maker cleanup test's Credentials::load). A per-module lock would
            // not exclude those cross-module races. See crate::TEST_ENV_LOCK.
            let lock = crate::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let original = std::env::var("STANDX_JWT").ok();
            std::env::set_var("STANDX_JWT", "pipeline-test-jwt");
            Self {
                original,
                _lock: lock,
            }
        }
    }

    impl Drop for JwtGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => std::env::set_var("STANDX_JWT", value),
                None => std::env::remove_var("STANDX_JWT"),
            }
        }
    }

    fn balance() -> Balance {
        Balance {
            balance: "100".to_string(),
            cross_available: "90".to_string(),
            cross_balance: "100".to_string(),
            cross_margin: "0".to_string(),
            cross_upnl: "0".to_string(),
            equity: "100".to_string(),
            isolated_balance: "0".to_string(),
            isolated_upnl: "0".to_string(),
            locked: "0".to_string(),
            pnl_24h: "0".to_string(),
            pnl_freeze: "0".to_string(),
            upnl: "0".to_string(),
        }
    }

    #[test]
    fn request_deadline_distinguishes_ack_and_account_order_phases() {
        let submitted_at = Instant::now();
        let timeout = Duration::from_secs(10);
        let mut projection = projection_with_pending_place("p1");
        let mut deadlines = OrderRequestDeadlines::default();
        deadlines.record("p1".to_owned(), OrderRequestKind::Place, submitted_at);

        assert_eq!(
            deadlines.next_deadline(timeout),
            Some(submitted_at + timeout)
        );
        assert!(deadlines
            .timed_out(
                &projection,
                submitted_at + timeout - Duration::from_millis(1),
                timeout,
            )
            .is_none());
        assert_eq!(
            deadlines
                .timed_out(&projection, submitted_at + timeout, timeout)
                .unwrap()
                .phase,
            RequestTimeoutPhase::Acknowledgement
        );

        projection.apply(
            1,
            AccountProjectionEvent::PlaceAccepted {
                request_id: "p1".to_owned(),
            },
        );
        assert_eq!(
            deadlines
                .timed_out(&projection, submitted_at + timeout, timeout)
                .unwrap()
                .phase,
            RequestTimeoutPhase::AccountOrder
        );

        projection.apply(1, AccountProjectionEvent::OrderObserved(observed_order()));
        deadlines.retain_pending(&projection);
        assert_eq!(deadlines.next_deadline(timeout), None);
        assert!(deadlines
            .timed_out(&projection, submitted_at + timeout, timeout)
            .is_none());
    }

    #[test]
    fn account_order_before_ack_keeps_ack_deadline_open() {
        let submitted_at = Instant::now();
        let timeout = Duration::from_secs(10);
        let mut projection = projection_with_pending_place("p1");
        let mut deadlines = OrderRequestDeadlines::default();
        deadlines.record("p1".to_owned(), OrderRequestKind::Place, submitted_at);

        projection.apply(1, AccountProjectionEvent::OrderObserved(observed_order()));
        deadlines.retain_pending(&projection);

        let timed_out = deadlines
            .timed_out(&projection, submitted_at + timeout, timeout)
            .unwrap();
        assert_eq!(timed_out.kind, OrderRequestKind::Place);
        assert_eq!(timed_out.phase, RequestTimeoutPhase::Acknowledgement);
    }

    #[test]
    fn preserved_cleanup_keeps_an_unacked_deadline_until_it_times_out() {
        // A freeze whose order-response channel survives (account-stream or
        // reconciliation) must not release an in-flight placement's deadline:
        // the ack can still arrive, so the request must keep timing out if it
        // never does.
        let submitted_at = Instant::now();
        let timeout = Duration::from_secs(10);
        let mut projection = projection_with_pending_place("p1");
        let mut deadlines = OrderRequestDeadlines::default();
        deadlines.record("p1".to_owned(), OrderRequestKind::Place, submitted_at);

        projection.finish_verified_cleanup(OrderResponseContinuity::Preserved);
        deadlines.retain_pending(&projection);

        assert_eq!(
            deadlines.next_deadline(timeout),
            Some(submitted_at + timeout),
            "a preserved unacked placement must keep its deadline"
        );
        let timed_out = deadlines
            .timed_out(&projection, submitted_at + timeout, timeout)
            .expect("a preserved unacked placement must still fail closed on timeout");
        assert_eq!(timed_out.kind, OrderRequestKind::Place);
        assert_eq!(timed_out.phase, RequestTimeoutPhase::Acknowledgement);
    }

    #[test]
    fn replaced_cleanup_retires_the_old_deadline_so_it_cannot_time_out() {
        // A freeze that replaces the order-response channel ends the old ack
        // obligation: no response can arrive on the torn-down stream, so the
        // stale deadline must be dropped and can never fire a spurious timeout.
        let submitted_at = Instant::now();
        let timeout = Duration::from_secs(10);
        let mut projection = projection_with_pending_place("p1");
        let mut deadlines = OrderRequestDeadlines::default();
        deadlines.record("p1".to_owned(), OrderRequestKind::Place, submitted_at);

        projection.finish_verified_cleanup(OrderResponseContinuity::Replaced);
        deadlines.retain_pending(&projection);

        assert_eq!(
            deadlines.next_deadline(timeout),
            None,
            "a replaced channel's stale deadline must be dropped"
        );
        assert!(
            deadlines
                .timed_out(&projection, submitted_at + timeout, timeout)
                .is_none(),
            "a retired request must never fire a spurious timeout"
        );
    }

    #[test]
    fn live_account_poll_uses_success_based_audit_and_balance_schedules() {
        let now = Instant::now();
        let mut state = LiveAccountPollState::new(balance(), now);
        let due = now + Duration::from_secs(30);

        assert!(!state.account_audit_due(due - Duration::from_millis(1)));
        assert!(!state.balance_refresh_due(due - Duration::from_millis(1)));
        assert!(state.account_audit_due(due));
        assert!(state.balance_refresh_due(due));

        state.record_account_audit(due);
        state.record_balance_refresh(balance(), due);
        assert!(!state.account_audit_due(due + Duration::from_secs(29)));
        assert!(!state.balance_refresh_due(due + Duration::from_secs(29)));
    }

    #[test]
    fn rest_position_mismatch_gets_one_three_second_recheck() {
        let now = Instant::now();
        let mut state = LiveAccountPollState::new(balance(), now);
        let first_audit = now + ACCOUNT_AUDIT_INTERVAL;

        assert!(!state.record_rest_position_mismatch(first_audit));
        assert!(state.rest_position_recheck_pending());
        assert!(!state.account_audit_due(first_audit + Duration::from_millis(2_999)));

        let recheck = first_audit + REST_POSITION_RECHECK_DELAY;
        assert!(state.account_audit_due(recheck));
        assert!(state.record_rest_position_mismatch(recheck));
        assert!(state.account_audit_due(recheck + Duration::from_secs(1)));

        state.record_account_audit(recheck);
        assert!(!state.rest_position_recheck_pending());
        assert!(!state.account_audit_due(recheck + Duration::from_secs(29)));
        assert!(state.account_audit_due(recheck + ACCOUNT_AUDIT_INTERVAL));
    }

    /// Adversarial-review fix: funding is attribution-only, but this audit also
    /// backs position reconciliation and recovery. A funding endpoint failure
    /// must therefore never fail the audit.
    #[tokio::test]
    async fn funding_failure_does_not_fail_the_account_audit() {
        let _jwt = JwtGuard::set();
        let mut server = Server::new_async().await;
        let wrapped = r#"{"code":0,"message":"ok","result":[]}"#;
        for path in [
            "/api/query_open_orders",
            "/api/query_orders",
            "/api/query_trades",
        ] {
            server
                .mock("GET", path)
                .match_query(Matcher::Any)
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(wrapped)
                .create_async()
                .await;
        }
        server
            .mock("GET", "/api/query_positions")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create_async()
            .await;
        // The one endpoint that is allowed to fail without consequence.
        server
            .mock("GET", "/api/query_funding_history")
            .match_query(Matcher::Any)
            .with_status(500)
            .with_body("upstream exploded")
            .create_async()
            .await;
        let client = StandXClient::with_base_url(server.url()).unwrap();

        let audit = fetch_account_audit(&client, "HYPE-USD", 1_784_304_000, 1_784_304_060)
            .await
            .expect("a funding failure must not fail the audit");
        assert!(audit.open_orders.is_empty());
        assert!(audit.positions.is_empty());
        assert!(audit.trades.is_empty());
        let error = audit.funding.expect_err("funding must surface as an error");
        assert!(error.contains("500"), "{error}");
    }

    /// Adversarial-review fix: the hard-floor freshness budget is measured from
    /// the instant the balance was recorded, so the caller must date a refresh
    /// by when its response arrived — not by when the request was issued.
    #[test]
    fn floor_freshness_budget_is_measured_from_the_recorded_instant() {
        let now = Instant::now();
        let mut state = LiveAccountPollState::new(balance(), now);
        assert_eq!(state.balance_age(now), Duration::ZERO);
        assert_eq!(
            state.balance_age(now + Duration::from_secs(10)),
            Duration::from_secs(10)
        );

        // Recording later (i.e. when the response actually landed) resets the
        // budget from that later instant, never from the earlier one.
        let landed = now + Duration::from_secs(12);
        state.record_balance_refresh(balance(), landed);
        assert_eq!(state.balance_age(landed), Duration::ZERO);
        assert_eq!(
            state.balance_age(landed + BALANCE_FLOOR_MAX_AGE),
            BALANCE_FLOOR_MAX_AGE
        );
        // One second past the budget is stale — this is the comparison the
        // armed floor makes, so an under-reported age is what would bypass it.
        assert!(
            state.balance_age(landed + BALANCE_FLOOR_MAX_AGE + Duration::from_secs(1))
                > BALANCE_FLOOR_MAX_AGE
        );
    }

    #[test]
    fn balance_failure_retries_quickly_and_expires_after_stale_limit() {
        let now = Instant::now();
        let mut state = LiveAccountPollState::new(balance(), now);
        let refresh_due = now + Duration::from_secs(30);

        state.record_balance_refresh_failure(refresh_due);
        assert!(!state.balance_refresh_due(refresh_due + Duration::from_secs(4)));
        assert!(state.balance_refresh_due(refresh_due + Duration::from_secs(5)));
        assert!(state.balance_is_within_stale_limit(now + Duration::from_secs(60)));
        assert!(!state.balance_is_within_stale_limit(now + Duration::from_secs(61)));
    }

    #[test]
    fn account_stream_balance_update_makes_authoritative_refresh_due_immediately() {
        let now = Instant::now();
        let mut state = LiveAccountPollState::new(balance(), now);

        assert!(!state.balance_refresh_due(now));
        state.request_balance_refresh(now);
        assert!(state.balance_refresh_due(now));

        state.record_balance_refresh(balance(), now);
        assert!(!state.balance_refresh_due(now + Duration::from_secs(29)));
        assert!(state.balance_refresh_due(now + Duration::from_secs(30)));
    }

    #[test]
    fn failed_account_audit_stays_due_until_a_successful_commit() {
        let now = Instant::now();
        let state = LiveAccountPollState::new(balance(), now);
        let due = now + Duration::from_secs(30);

        assert!(state.account_audit_due(due));
        // An audit failure deliberately does not call
        // `record_account_audit`, so the next cycle must retry it.
        assert!(state.account_audit_due(due + Duration::from_secs(1)));
    }

    #[tokio::test]
    async fn normal_cycles_make_no_account_rest_reads_until_audit_is_due() {
        let _jwt = JwtGuard::set();
        let mut server = Server::new_async().await;
        let open_orders = server
            .mock("GET", "/api/query_open_orders")
            .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"code":0,"message":"ok","result":[]}"#)
            .expect(1)
            .create_async()
            .await;
        let positions = server
            .mock("GET", "/api/query_positions")
            .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let filled_orders = server
            .mock("GET", "/api/query_orders")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"code":0,"message":"ok","result":[]}"#)
            .expect(1)
            .create_async()
            .await;
        let trades = server
            .mock("GET", "/api/query_trades")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"code":0,"message":"ok","result":[]}"#)
            .expect(1)
            .create_async()
            .await;
        // Funding history is a bare array, not the {code,message,result} envelope.
        let funding = server
            .mock("GET", "/api/query_funding_history")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let balance_mock = server
            .mock("GET", "/api/query_balance")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(serde_json::to_string(&balance()).unwrap())
            .expect(1)
            .create_async()
            .await;
        let client = StandXClient::with_base_url(server.url()).unwrap();
        let now = Instant::now();
        let mut poll = LiveAccountPollState::new(balance(), now);

        // A healthy cycle before the deadline does not invoke any of the
        // mocks above; it reads the local projection instead.
        assert!(!poll.account_audit_due(now));
        assert!(!poll.balance_refresh_due(now));

        let due = now + Duration::from_secs(30);
        assert!(poll.account_audit_due(due));
        assert!(poll.balance_refresh_due(due));
        let (audit, refreshed_balance) = tokio::join!(
            fetch_account_audit(&client, "BTC-USD", 1_784_304_000, 1_784_304_060),
            client.get_balance(),
        );
        let audit = audit.unwrap();
        assert!(audit.open_orders.is_empty());
        assert!(audit.positions.is_empty());
        assert!(audit.trades.is_empty());
        assert!(audit.funding.expect("funding fetch succeeded").is_empty());
        poll.record_account_audit(due);
        poll.record_balance_refresh(refreshed_balance.unwrap(), due);

        funding.assert_async().await;
        open_orders.assert_async().await;
        positions.assert_async().await;
        filled_orders.assert_async().await;
        trades.assert_async().await;
        balance_mock.assert_async().await;
    }
}
