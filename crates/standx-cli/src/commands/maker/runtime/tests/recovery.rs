use super::*;

struct JwtGuard {
    original: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl JwtGuard {
    fn set() -> Self {
        // Share the crate-wide env lock so this STANDX_JWT mutation cannot
        // race env reads in other modules' tests. See crate::TEST_ENV_LOCK.
        let lock = crate::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let original = std::env::var("STANDX_JWT").ok();
        std::env::set_var("STANDX_JWT", "runtime-test-jwt");
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

fn quiet_notifier() -> MakerNotifier {
    MakerNotifier::new(
        OutputFormat::Quiet,
        None,
        crate::cli::AlertWebhookFormat::Raw,
    )
}

fn resting_quote() -> RestingQuote {
    RestingQuote {
        order_id: None,
        side: OrderSide::Buy,
        level: 0,
        price: 100.0,
        qty: 0.001,
        ref_center: 100.0,
        placed_at_cycle: 1,
    }
}

fn warning_notice(kind: &'static str) -> RiskNotice<'static> {
    RiskNotice::warning(kind, "disconnected_frozen", "test freeze", "BTC-USD", 7).expected(0.0)
}

fn order_response_freeze_spec() -> FreezeSpec<'static> {
    FreezeSpec {
        target: RecoveryTarget::OrderResponse,
        trigger: MakerEvent::OrderResponseDisconnected("stream closed".to_string()),
        cleanup_effect_stop: EffectFailureStop::OrderResponse,
        recovery_effect_stop: EffectFailureStop::OrderResponse,
        cleanup_failure_prefix: "order-response ".to_string(),
        cleanup_failed_exit: MakerExit::OrderResponse,
        notice: FreezeNotice::Risk(warning_notice("order_response")),
        frozen_note: None,
        abort_account_stream_handle: false,
        continuity: OrderResponseContinuity::Replaced,
        cancel_venue_orders: true,
        price_decimals: 2,
    }
}

/// Invariant: the freeze preamble empties the maker book on the venue
/// (cancelling only maker-owned orders), clears local book state, and
/// hands back a recovery token from which quoting can resume.
#[tokio::test]
async fn freeze_preamble_empties_the_maker_book_and_hands_back_recovery() {
    use mockito::{Matcher, Server};
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    let open_before = server
        .mock("GET", "/api/query_open_orders")
        .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"code":0,"message":"ok","result":[
                {"id":"42","cl_ord_id":"sxmk-freeze-buy","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"},
                {"id":"99","cl_ord_id":"manual-order","symbol":"BTC-USD","side":"sell","order_type":"limit","qty":"0.001","fill_qty":"0","price":"65000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"}
            ]}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let cancel = server
        .mock("POST", "/api/cancel_orders")
        .match_body(Matcher::Json(serde_json::json!({ "order_id_list": [42] })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"code":0,"message":"accepted"}"#)
        .expect(1)
        .create_async()
        .await;
    let open_after = server
        .mock("GET", "/api/query_open_orders")
        .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"code":0,"message":"ok","result":[
                {"id":"99","cl_ord_id":"manual-order","symbol":"BTC-USD","side":"sell","order_type":"limit","qty":"0.001","fill_qty":"0","price":"65000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"}
            ]}"#,
        )
        .expect(1)
        .create_async()
        .await;

    let client = StandXClient::with_base_url(server.url()).unwrap();
    let notifier = quiet_notifier();
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::RunCycle(_))
    ));
    let mut resting = vec![resting_quote()];
    let mut inventory_exit_pending = true;
    let mut next_cycle_is_recovery = false;

    let recovery_token = freeze_and_cleanup_for_recovery(
        &mut RecoveryIo {
            runtime_state: &mut runtime_state,
            notifier: &notifier,
            client: &client,
            session: None,
            resting: &mut resting,
            inventory_exit_pending: &mut inventory_exit_pending,
            next_cycle_is_recovery: &mut next_cycle_is_recovery,
            symbol: "BTC-USD",
            cycle: 7,
            output_format: OutputFormat::Quiet,
        },
        order_response_freeze_spec(),
    )
    .await
    .expect("freeze preamble must hand back a recovery token");

    assert!(resting.is_empty(), "local book must be cleared");
    assert!(!inventory_exit_pending);
    assert!(
        runtime_state.pending_effect().is_none(),
        "no stale effects may remain after the preamble"
    );
    open_before.assert_async().await;
    cancel.assert_async().await;
    open_after.assert_async().await;

    // Recovery success must resume quoting with a fresh cycle.
    runtime_state.handle(MakerEvent::RecoverySucceeded(recovery_token));
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::RunCycle(_))
    ));
}

/// Invariant: when the venue book cannot be emptied, the preamble stops
/// the runtime with the flow's exit and its exact historical wording.
#[tokio::test]
async fn freeze_preamble_cleanup_failure_stops_with_the_flow_exit() {
    use mockito::{Matcher, Server};
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    let open_orders = server
        .mock("GET", "/api/query_open_orders")
        .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
        .with_status(500)
        .with_body("venue unavailable")
        .expect_at_least(1)
        .create_async()
        .await;

    let client = StandXClient::with_base_url(server.url()).unwrap();
    let notifier = quiet_notifier();
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let _ = runtime_state.next_effect();
    let mut resting = vec![resting_quote()];
    let mut inventory_exit_pending = false;
    let mut next_cycle_is_recovery = false;

    let exit = freeze_and_cleanup_for_recovery(
        &mut RecoveryIo {
            runtime_state: &mut runtime_state,
            notifier: &notifier,
            client: &client,
            session: None,
            resting: &mut resting,
            inventory_exit_pending: &mut inventory_exit_pending,
            next_cycle_is_recovery: &mut next_cycle_is_recovery,
            symbol: "BTC-USD",
            cycle: 7,
            output_format: OutputFormat::Quiet,
        },
        order_response_freeze_spec(),
    )
    .await
    .expect_err("cleanup failure must stop the runtime");

    match exit {
        MakerExit::OrderResponse(reason) => {
            assert!(
                reason.contains("order-response freeze cleanup failed:"),
                "cleanup-failure wording drifted: {reason}"
            );
        }
        other => panic!(
            "order-response cleanup failure must exit as OrderResponse, got {:?}",
            other.lifecycle_reason()
        ),
    }
    // The runtime is stopping: no further work may be scheduled.
    runtime_state.handle(MakerEvent::Timer);
    assert!(runtime_state.pending_effect().is_none());
    open_orders.assert_async().await;
}

/// Invariant: if the runtime cannot enter the freeze (it is already
/// stopping), the preamble fails closed instead of proceeding to cleanup.
#[tokio::test]
async fn freeze_preamble_fails_closed_when_runtime_cannot_freeze() {
    let client = StandXClient::new().unwrap();
    let notifier = quiet_notifier();
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let _ = runtime_state.next_effect();
    runtime_state.handle(MakerEvent::StopRequested(RuntimeStopReason::CtrlC));
    while runtime_state.next_effect().is_some() {}
    let mut resting = vec![resting_quote()];
    let mut inventory_exit_pending = false;
    let mut next_cycle_is_recovery = false;

    let exit = freeze_and_cleanup_for_recovery(
        &mut RecoveryIo {
            runtime_state: &mut runtime_state,
            notifier: &notifier,
            client: &client,
            session: None,
            resting: &mut resting,
            inventory_exit_pending: &mut inventory_exit_pending,
            next_cycle_is_recovery: &mut next_cycle_is_recovery,
            symbol: "BTC-USD",
            cycle: 7,
            output_format: OutputFormat::Quiet,
        },
        order_response_freeze_spec(),
    )
    .await
    .expect_err("a stopping runtime must not begin cleanup");
    assert!(matches!(exit, MakerExit::PositionReconciliation(_)));
    assert!(
        !resting.is_empty(),
        "no cleanup may run when the freeze was rejected"
    );
}

/// Regression: a leftover order-response frame that fails closed during replay
/// must be reported to the caller, not silently absorbed. The runtime is
/// already `Frozen` by the time the freeze preamble replays leftovers (mirrored
/// here via the same `OrderResponseDisconnected` trigger the order-response
/// flow uses), so the `OrderResponseUnmatched` event `replay_leftover_responses`
/// raises internally is a no-op — `freeze_and_cleanup_for_recovery` only
/// notices the failure because it checks this function's return value, not
/// because a new effect got queued.
#[test]
fn replay_leftover_responses_reports_a_fail_closed_correlation() {
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    let mut order_latency = maker::OrderLatencyTracker::default();
    let latency_started = std::time::Instant::now();
    let order_response_health = OrderResponseHealth::default();
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let _ = runtime_state.next_effect();
    runtime_state.handle(MakerEvent::OrderResponseDisconnected(
        "stream closed".to_string(),
    ));
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::AbortInFlight(_))
    ));
    assert!(matches!(
        runtime_state.next_effect(),
        Some(MakerEffect::Cleanup { .. })
    ));
    assert!(
        runtime_state.pending_effect().is_none(),
        "no further effect is queued once Frozen"
    );

    // A leftover frame for a request this run never registered classifies as
    // Orphan and must fail closed.
    let leftover = vec![order_response(Some("foreign-req"), 0)];

    let failure = replay_leftover_responses(
        leftover,
        LeftoverReplayContext {
            projection: &mut projection,
            order_latency: &mut order_latency,
            latency_started,
            order_response_health: &order_response_health,
            output_format: OutputFormat::Quiet,
            symbol: "BTC-USD",
            cycle: 7,
            price_decimals: 2,
        },
        &mut runtime_state,
    );

    assert!(failure.is_some(), "an Orphan correlation must fail closed");
    assert!(
        !order_response_health.is_healthy(),
        "the order-response stream must be marked unhealthy"
    );
    // The runtime was already Frozen going in, so the OrderResponseUnmatched
    // event raised inside `order_response_failure` queues nothing new — the
    // `Some` return is the only signal the caller has to act on.
    assert!(
        runtime_state.pending_effect().is_none(),
        "the runtime queues no new effect for a fail-closed leftover while already frozen"
    );
}

/// Invariant: leftovers that all correlate cleanly produce no failure, and the
/// runtime is left exactly as it was.
#[test]
fn replay_leftover_responses_returns_none_when_every_frame_matches() {
    let mut projection = projection_with_pending(&["req-1"]);
    let mut order_latency = maker::OrderLatencyTracker::default();
    let latency_started = std::time::Instant::now();
    let order_response_health = OrderResponseHealth::default();
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let _ = runtime_state.next_effect();

    let leftover = vec![order_response(Some("req-1"), 0)];

    let failure = replay_leftover_responses(
        leftover,
        LeftoverReplayContext {
            projection: &mut projection,
            order_latency: &mut order_latency,
            latency_started,
            order_response_health: &order_response_health,
            output_format: OutputFormat::Quiet,
            symbol: "BTC-USD",
            cycle: 7,
            price_decimals: 2,
        },
        &mut runtime_state,
    );

    assert!(failure.is_none());
    assert!(order_response_health.is_healthy());
}

/// Invariant: the resume tail restores quoting state (flags, error
/// streak, paper book) and schedules the next cycle via the runtime.
#[tokio::test]
async fn resume_tail_restores_quoting_state_and_schedules_a_cycle() {
    let client = StandXClient::new().unwrap();
    let notifier = quiet_notifier();
    let mut runtime_state = MakerState::starting();
    runtime_state.handle(MakerEvent::StartupReady);
    let _ = runtime_state.next_effect();
    runtime_state.handle(MakerEvent::PositionMismatch);
    let _ = runtime_state.next_effect(); // AbortInFlight
    let cleanup = match runtime_state.next_effect() {
        Some(MakerEffect::Cleanup { token, .. }) => token,
        other => panic!("expected cleanup effect, got {other:?}"),
    };
    runtime_state.handle(MakerEvent::CleanupCompleted(cleanup));
    let recovery_token = match runtime_state.next_effect() {
        Some(MakerEffect::Recover { token, .. }) => token,
        other => panic!("expected recovery effect, got {other:?}"),
    };
    let mut resting = vec![resting_quote()];
    let mut inventory_exit_pending = false;
    let mut next_cycle_is_recovery = false;

    resume_quoting_after_recovery(
        &mut RecoveryIo {
            runtime_state: &mut runtime_state,
            notifier: &notifier,
            client: &client,
            session: None,
            resting: &mut resting,
            inventory_exit_pending: &mut inventory_exit_pending,
            next_cycle_is_recovery: &mut next_cycle_is_recovery,
            symbol: "BTC-USD",
            cycle: 7,
            output_format: OutputFormat::Quiet,
        },
        ResumeSpec {
            recovery_token,
            observed: 0.0,
            continuity: OrderResponseContinuity::Preserved,
            clear_resting: true,
            recovered_note: None,
            notice: RiskNotice::resolved(
                "position_reconciliation",
                "recovered",
                "test resume",
                "BTC-USD",
                7,
            )
            .expected(0.0)
            .observed(0.0),
        },
    )
    .await;

    assert!(resting.is_empty());
    assert!(next_cycle_is_recovery);
    assert!(
        matches!(runtime_state.next_effect(), Some(MakerEffect::RunCycle(_))),
        "resume must schedule the next quoting cycle"
    );
}

/// Invariant: the continuity knob keeps its per-flow semantics —
/// preserving pending request lifecycles for late acks when the channel
/// survives, or dropping them when the placement channel is replaced.
#[test]
fn finish_verified_cleanup_preserves_or_drops_pending_requests() {
    let mut projection = projection_with_pending(&["request-1"]);
    projection.finish_verified_cleanup(OrderResponseContinuity::Preserved);
    assert!(
        projection.has_pending_request_lifecycle("request-1"),
        "Preserved continuity must keep pending request lifecycles"
    );

    let mut projection = projection_with_pending(&["request-1"]);
    projection.finish_verified_cleanup(OrderResponseContinuity::Replaced);
    assert!(
        !projection.has_pending_request_lifecycle("request-1"),
        "Replaced continuity must clear pending request lifecycles"
    );
}

/// Adversarial-review fix: one post-cleanup snapshot must not be enough to
/// report the account flat. A fill that lands while cleanup is cancelling can
/// still be absent from the REST position view, and a false `flat` is the one
/// outcome nobody gets told about.
mod post_cleanup_position {
    use super::super::lifecycle::confirm_venue_position;
    use super::JwtGuard;
    use mockito::{Matcher, Server};
    use standx_sdk::client::StandXClient;

    const TOL: f64 = 0.0005;
    /// Tests drive the two-snapshot logic without waiting the real 1.5s.
    const NO_DELAY: std::time::Duration = std::time::Duration::ZERO;

    fn position_body(qty: &str, side: &str) -> String {
        serde_json::json!([{
            "id": 1,
            "symbol": "HYPE-USD",
            "side": side,
            "qty": qty,
            "entry_price": "55.0",
            "entry_value": "5.5",
            "holding_margin": "1",
            "initial_margin": "1",
            "leverage": "1",
            "mark_price": "55.0",
            "margin_asset": "DUSD",
            "margin_mode": "cross",
            "position_value": "5.5",
            "realized_pnl": "0",
            "required_margin": "1",
            "status": "open",
            "upnl": "0",
            "time": "2026-07-28T00:00:00Z",
            "created_at": "2026-07-28T00:00:00Z",
            "updated_at": "2026-07-28T00:00:00Z",
            "user": "test"
        }])
        .to_string()
    }

    /// A non-flat first read needs no confirmation: it is already actionable.
    #[tokio::test]
    async fn non_flat_snapshot_is_returned_immediately() {
        let _jwt = JwtGuard::set();
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/api/query_positions")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(position_body("0.1", "sell"))
            .expect(1)
            .create_async()
            .await;
        let client = StandXClient::with_base_url(server.url()).unwrap();

        let observed = confirm_venue_position(&client, "HYPE-USD", TOL, NO_DELAY).await;
        assert_eq!(observed, Some(-0.1));
        mock.assert_async().await;
    }

    /// The case the single-snapshot version got wrong: the first read is flat
    /// only because the cancel-race fill has not propagated yet.
    #[tokio::test]
    async fn late_fill_after_a_flat_first_read_is_still_handed_off() {
        let _jwt = JwtGuard::set();
        let mut server = Server::new_async().await;
        let flat = server
            .mock("GET", "/api/query_positions")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .expect(2)
            .create_async()
            .await;
        let client = StandXClient::with_base_url(server.url()).unwrap();
        let observed_flat = confirm_venue_position(&client, "HYPE-USD", TOL, NO_DELAY).await;
        assert_eq!(observed_flat, Some(0.0), "two agreeing reads confirm flat");
        flat.assert_async().await;

        // Same shutdown, but the venue reveals the fill on the second read.
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/query_positions")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .expect(1)
            .create_async()
            .await;
        let late = server
            .mock("GET", "/api/query_positions")
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(position_body("0.1", "buy"))
            .expect(1)
            .create_async()
            .await;
        let client = StandXClient::with_base_url(server.url()).unwrap();
        let observed = confirm_venue_position(&client, "HYPE-USD", TOL, NO_DELAY).await;
        assert_eq!(
            observed,
            Some(0.1),
            "a fill revealed by the settlement snapshot must not read as flat"
        );
        late.assert_async().await;
    }

    /// An unreadable venue is `None`, which the caller renders as `unknown` —
    /// never as flat.
    #[tokio::test]
    async fn failed_snapshot_is_unknown_not_flat() {
        let _jwt = JwtGuard::set();
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/query_positions")
            .match_query(Matcher::Any)
            .with_status(500)
            .with_body("nope")
            .create_async()
            .await;
        let client = StandXClient::with_base_url(server.url()).unwrap();

        assert_eq!(
            confirm_venue_position(&client, "HYPE-USD", TOL, NO_DELAY).await,
            None
        );
    }
}
