use super::super::{feed::MarketTelemetrySnapshot, pipeline::LiveAccountPollState};
use super::*;
use crate::cli::OutputFormat;
use standx_sdk::client::StandXClient;

fn config() -> maker::MakerConfig {
    maker::MakerConfig {
        spread_bps: 10.0,
        band_bps: 20.0,
        level_step_bps: 2.0,
        refresh_bps: 3.0,
        levels: 2,
        size: 0.01,
        max_position: 0.05,
        skew_bps: 0.0,
        price_decimals: 2,
        qty_decimals: 4,
        min_order_qty: 0.001,
    }
}

#[derive(Default)]
struct Scenario<'a> {
    divergent_market: bool,
    inventory_exit_pct: f64,
    audit_client: Option<&'a StandXClient>,
}

async fn run(
    stats: &mut MakerStats,
    position: &mut f64,
    resting: &mut Vec<RestingQuote>,
    stop_loss: f64,
    projection: Option<&mut MakerAccountProjection>,
    scenario: Scenario<'_>,
) -> Result<CycleResult> {
    let cfg = config();
    // No transport is supplied. Any live write attempt makes the test fail.
    let client = StandXClient::with_base_url("http://127.0.0.1:1".into()).unwrap();
    let client = scenario.audit_client.unwrap_or(&client);
    let divergent_market = scenario.divergent_market;
    let live = projection.is_some();
    let mut ledger = MakerLedger::new(*position);
    let mut exit_pending = false;
    let mut exit_order = None;
    let mut breaker = maker::VolBreaker::new(10, 0.0);
    let mut spread = maker::SpreadController::new(Default::default(), &cfg).unwrap();
    let mut size = maker::SizeSkewController::new(Default::default(), &cfg).unwrap();
    let mut guard = maker::GuardController::new(Default::default()).unwrap();
    let mut shift = 0.0;
    let mut excess = super::super::pipeline::ExternalExcessTelemetry::default();
    let account_health = AccountStreamHealth::new(1);
    let order_health = OrderResponseHealth::default();
    let mut poll = LiveAccountPollState::new(
        serde_json::from_value(serde_json::json!({
            "balance":"100", "cross_available":"100", "cross_balance":"100", "cross_margin":"0",
            "cross_upnl":"0", "equity":"100", "isolated_balance":"0", "isolated_upnl":"0",
            "locked":"0", "pnl_24h":"0", "pnl_freeze":"0", "upnl":"0"
        }))
        .unwrap(),
        Instant::now()
            - std::time::Duration::from_secs(if scenario.audit_client.is_some() {
                31
            } else {
                0
            }),
    );
    maker_cycle(
        CycleRequest {
            client,
            symbol: "BTC-USD",
            cfg: &cfg,
            live,
            cycle: 1,
            mark: 100.0,
            best_bid: Some(if divergent_market { 105.0 } else { 99.99 }),
            best_ask: Some(if divergent_market { 105.01 } else { 100.01 }),
            market_data_mode: maker::MarketDataMode::Active,
            market_source: "test",
            recovery: false,
            market_fallback_reason: None,
            ws_snapshot: None,
            market_telemetry: &MarketTelemetrySnapshot::default(),
            max_divergence_bps: 100.0,
            inventory_exit_pct: scenario.inventory_exit_pct,
            inventory_exit_qty: 0.01,
            inventory_exit_cfg: Default::default(),
            stop_equity_below: 0.0,
            stop_loss,
            stop_margin_below: 0.0,
            wind_down: false,
            qty_tolerance: 0.00005,
            session_started_at: 0,
            run_order_prefix: "sxmk-test-",
            starting_position: *position,
            output_format: OutputFormat::Quiet,
            order_commands: None,
            order_response_health: Some(&order_health),
            account_stream_health: Some(&account_health),
            performance_time_ms: 0,
        },
        CycleState {
            resting,
            account_projection: projection,
            inventory_exit_pending: &mut exit_pending,
            inventory_exit_order: &mut exit_order,
            ledger: &mut ledger,
            sim_position: position,
            stats,
            breaker: &mut breaker,
            spread_controller: &mut spread,
            size_skew_controller: &mut size,
            nonlinear_skew: Default::default(),
            external_skew: Default::default(),
            microprice: Default::default(),
            external_skew_previous_shift_bps: &mut shift,
            external_excess_telemetry: &mut excess,
            guard_controller: &mut guard,
            external_divergence: None,
            external_basis_bps: None,
            order_request_deadlines: None,
            live_account_poll: Some(&mut poll),
            order_latency: None,
            latency_started: None,
        },
    )
    .await
}

#[tokio::test]
async fn known_stop_loss_precedes_even_market_skip_and_account_reads() {
    let mut stats = MakerStats::with_inventory_baseline(0.02, 110.0);
    let result = run(
        &mut stats,
        &mut 0.02,
        &mut vec![],
        0.1,
        None,
        Scenario {
            divergent_market: true,
            ..Default::default()
        },
    )
    .await;
    let Err(error) = result else {
        panic!("known loss must stop instead of skipping");
    };
    assert!(error.is::<maker::SessionStopLoss>());
}

#[tokio::test]
async fn paper_fill_stop_records_fill_without_more_quotes() {
    let mut stats = MakerStats::default();
    let mut position = 0.0;
    let mut resting = vec![RestingQuote {
        order_id: None,
        side: OrderSide::Buy,
        level: 0,
        price: 101.0,
        qty: 0.02,
        ref_center: 100.0,
        placed_at_cycle: 0,
    }];
    let result = run(
        &mut stats,
        &mut position,
        &mut resting,
        0.01,
        None,
        Scenario::default(),
    )
    .await
    .unwrap();
    assert_eq!(result.fills, 1);
    assert_eq!(stats.fills(), 1);
    assert_eq!(
        position, 0.02,
        "triggering fill must remain in the paper position"
    );
    assert_eq!((result.places, result.cancels, result.holds), (0, 0, 0));
    assert!(result.stop_loss.is_some());
    assert!(resting.is_empty());
}

#[tokio::test]
async fn live_cycle_reserves_pending_quantities_before_transport_writes() {
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    for (side, tag, price) in [(OrderSide::Buy, "b", 99.9), (OrderSide::Sell, "s", 100.1)] {
        projection.apply(
            1,
            AccountProjectionEvent::PlaceSubmitted(ProjectionPendingPlace {
                request_id: tag.into(),
                client_order_id: format!("sxmk-test-{tag}"),
                side,
                price,
                qty: 0.05,
                level: 0,
                ref_center: 100.0,
                cycle: 0,
            }),
        );
    }
    let result = run(
        &mut MakerStats::default(),
        &mut 0.0,
        &mut vec![],
        0.0,
        Some(&mut projection),
        Scenario::default(),
    )
    .await
    .unwrap();
    assert_eq!((result.places, result.cancels), (0, 0));
    assert!(result.stop_loss.is_none());
}

#[tokio::test]
async fn disabled_stop_keeps_ordinary_paper_quotes() {
    let mut resting = vec![];
    let result = run(
        &mut MakerStats::default(),
        &mut 0.0,
        &mut resting,
        0.0,
        None,
        Scenario::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        (result.places, result.cancels, result.holds, result.fills),
        (4, 0, 0, 0)
    );
    assert!(result.stop_loss.is_none());
    assert_eq!(resting.len(), 4);
}

/// Unlike paper mode, this drives the live inventory-exit gate after a REST
/// backfill creates a loss. No command sender exists, so an attempted exit
/// fails the test before any request can be written.
#[tokio::test]
async fn rest_fill_stop_prevents_live_inventory_exit() {
    use mockito::{Matcher, Server};
    let mut server = Server::new_async().await;
    let positions = serde_json::json!([{
        "id": 1, "symbol": "BTC-USD", "side": "buy", "qty": "0.02",
        "entry_price": "101", "entry_value": "2.02", "holding_margin": "1",
        "initial_margin": "1", "leverage": "1", "mark_price": "100", "margin_asset": "DUSD",
        "margin_mode": "cross", "position_value": "2", "realized_pnl": "0", "required_margin": "1",
        "status": "open", "upnl": "-0.02", "time": "2026-07-10T00:00:00Z",
        "created_at": "2026-07-10T00:00:00Z", "updated_at": "2026-07-10T00:00:00Z", "user": "test"
    }]);
    let trade = serde_json::json!({"code":0,"message":"ok","result":[{
        "id":42,"order_id":7,"symbol":"BTC-USD","side":"buy","price":"101","qty":"0.02",
        "time":"2026-07-10T00:00:00Z","is_buyer_taker":false
    }]});
    let history = serde_json::json!({"code":0,"message":"ok","result":[{
        "id":"7","cl_ord_id":"sxmk-test-q00000001b0","symbol":"BTC-USD","side":"buy",
        "order_type":"limit","qty":"0.02","fill_qty":"0.02","price":"101","status":"filled",
        "created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"
    }]});
    let mut mocks = Vec::new();
    for (path, body) in [
        (
            "/api/query_open_orders",
            serde_json::json!({"code":0,"message":"ok","result":[]}),
        ),
        ("/api/query_positions", positions),
        ("/api/query_orders", history),
        ("/api/query_trades", trade),
        ("/api/query_funding_history", serde_json::json!([])),
    ] {
        mocks.push(
            server
                .mock("GET", path)
                .match_query(Matcher::Any)
                .with_status(200)
                .with_header("content-type", "application/json")
                .with_body(body.to_string())
                .expect(1)
                .create_async()
                .await,
        );
    }
    // A refresh failure retains the deliberately fresh-enough cached balance.
    let balance = server
        .mock("GET", "/api/query_balance")
        .with_status(503)
        .expect(1)
        .create_async()
        .await;
    // Restore the process-wide JWT before releasing the shared test lock.
    // Requests read this env var on demand, so the guard spans the local I/O.
    struct RestoreJwt {
        original: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl Drop for RestoreJwt {
        fn drop(&mut self) {
            match &self.original {
                Some(v) => std::env::set_var("STANDX_JWT", v),
                None => std::env::remove_var("STANDX_JWT"),
            }
        }
    }
    let lock = crate::TEST_ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let jwt = RestoreJwt {
        original: std::env::var("STANDX_JWT").ok(),
        _lock: lock,
    };
    std::env::set_var("STANDX_JWT", "maker-safety-test-jwt");
    let client = StandXClient::with_base_url(server.url()).unwrap();
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    // The position event can precede its trade. REST backfill must reconcile
    // that position and check the resulting loss before executing the exit.
    projection.apply(
        1,
        AccountProjectionEvent::PositionObserved { position: 0.02 },
    );
    let mut stats = MakerStats::default();
    let result = run(
        &mut stats,
        &mut 0.0,
        &mut vec![],
        0.01,
        Some(&mut projection),
        Scenario {
            inventory_exit_pct: 10.0,
            audit_client: Some(&client),
            ..Default::default()
        },
    )
    .await;
    drop(jwt);
    let result = result.unwrap();
    assert!(result.stop_loss.is_some());
    assert_eq!(result.fills, 1);
    assert_eq!(stats.fills(), 1);
    assert_eq!(stats.position(), 0.02);
    assert_eq!((result.places, result.cancels), (0, 0));
    for mock in mocks {
        mock.assert_async().await;
    }
    balance.assert_async().await;
}
