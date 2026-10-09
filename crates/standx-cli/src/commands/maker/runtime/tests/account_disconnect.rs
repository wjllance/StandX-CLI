//! Flow-level coverage for an authenticated account-stream disconnect that
//! lands while a quote cycle owns in-flight work (the routine 23h50m rotation).
//! A disconnect is a transport fault: it must reach account-stream recovery,
//! never position reconciliation, and never a hard `recovery_failed` stop.

use super::recovery::JwtGuard;
use super::runtime_flow::{
    ingest_harness, owned_order, owned_trade, position_event, IngestHarness,
};
use super::*;
use mockito::{Matcher, Server, ServerGuard};
use standx_sdk::account_stream::AccountEvent;

fn describe(directive: &LoopDirective) -> String {
    match directive {
        LoopDirective::Proceed => "proceed".to_string(),
        LoopDirective::Restart => "restart".to_string(),
        LoopDirective::Exit(exit) => format!("exit: {}", exit.lifecycle_reason()),
    }
}

async fn mount_flat_venue(server: &mut ServerGuard) {
    mount_venue(server, "[]").await;
}

fn long_position(qty: &str) -> String {
    serde_json::json!([{
        "id": 1, "symbol": "BTC-USD", "side": "buy", "qty": qty,
        "entry_price": "100.0", "entry_value": "50", "holding_margin": "1",
        "initial_margin": "1", "leverage": "1", "mark_price": "100.0",
        "margin_asset": "DUSD", "margin_mode": "cross", "position_value": "50",
        "realized_pnl": "0", "required_margin": "1", "status": "open", "upnl": "0",
        "time": "2026-07-28T00:00:00Z", "created_at": "2026-07-28T00:00:00Z",
        "updated_at": "2026-07-28T00:00:00Z", "user": "test"
    }])
    .to_string()
}

async fn mount_venue(server: &mut ServerGuard, positions: &str) {
    let empty = r#"{"code":0,"message":"ok","result":[]}"#;
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
            .with_body(empty)
            .create_async()
            .await;
    }
    server
        .mock("GET", "/api/query_positions")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(positions.to_string())
        .create_async()
        .await;
    server
        .mock("GET", "/api/query_funding_history")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create_async()
        .await;
}

fn disconnected(reason: &str) -> AccountEvent {
    AccountEvent::Disconnected {
        reason: reason.to_string(),
    }
}

/// One cycle as `drive` runs it: the work phase, then the cycle completion
/// when the work phase produced an attempt.
async fn run_cycle(runtime: &mut MakerRuntime) -> LoopDirective {
    match runtime.execute_cycle().await {
        Ok(attempt) => runtime.finish_cycle(attempt).await,
        Err(directive) => directive,
    }
}

/// The reconciliation window can legitimately finish "recovered" (Ready with
/// `RunCycle` queued) while the stream is dead. That is only safe because the
/// next pre-cycle phase refreezes for the dead stream before any cycle work
/// is taken. Pin both halves: the window path really ran (strict oracle: the
/// select's closed-channel branch would have queued cleanup instead, so a
/// timing slip fails loudly rather than passing through the wrong path), and
/// pre_cycle exits through account-stream recovery, not into quoting.
async fn assert_window_recovery_then_refrozen_by_pre_cycle(runtime: &mut MakerRuntime) {
    assert!(
        matches!(
            runtime.recovery.runtime_state.pending_effect(),
            Some(MakerEffect::RunCycle(_))
        ),
        "the window path must have declared recovery with RunCycle queued, got {:?}",
        runtime.recovery.runtime_state.pending_effect()
    );
    assert_pre_cycle_enters_account_stream_recovery(runtime).await;
}

async fn assert_pre_cycle_enters_account_stream_recovery(runtime: &mut MakerRuntime) {
    let next = describe(&runtime.pre_cycle_phase().await);
    assert!(
        next.contains("reconnect disabled"),
        "pre-cycle must refreeze for the dead account stream, got {next}"
    );
}

fn account_stream_healthy(runtime: &MakerRuntime) -> bool {
    runtime
        .live_session
        .as_ref()
        .expect("live session")
        .account_stream_health
        .is_healthy()
}

/// The rotation as the SDK emits it: an explicit `Disconnected` event, then the
/// sender is dropped and the channel closes.
#[tokio::test]
async fn rotation_disconnect_during_cycle_routes_to_account_stream_recovery() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    mount_flat_venue(&mut server).await;
    let IngestHarness {
        mut runtime,
        _account_tx,
        _order_tx,
    } = ingest_harness(0.0, 0.0);
    runtime.deps.client = standx_sdk::client::StandXClient::with_base_url(server.url()).unwrap();
    _account_tx
        .send(disconnected("scheduled rotation"))
        .await
        .unwrap();
    drop(_account_tx);

    let directive = run_cycle(&mut runtime).await;

    assert!(
        matches!(directive, LoopDirective::Restart),
        "a disconnect during a cycle must hand over to recovery, got {}",
        describe(&directive)
    );
    assert!(
        !account_stream_healthy(&runtime),
        "the account stream must be marked unhealthy so recovery reconnects it"
    );
    // The reducer's Debug output carries the freeze reason. Without this the
    // cycle-invalidation route and the direct route end in the same queued
    // effects (the target upgrade hides the difference), so the freeze reason
    // is the only trace that the disconnect was classified before freezing.
    let frozen = format!("{:?}", runtime.recovery.runtime_state);
    assert!(
        !frozen.contains("account state changed during maker cycle"),
        "a disconnect must not freeze as a cycle invalidation: {frozen}"
    );
    take_cleanup_effect(
        &mut runtime.recovery.runtime_state,
        RecoveryTarget::AccountStream,
    )
    .expect("the freeze must queue account-stream cleanup, not reconciliation cleanup");
}

/// The rotation freeze must lead pre-cycle into account-stream recovery (no
/// target-mismatch stop between the two).
#[tokio::test]
async fn rotation_then_pre_cycle_reaches_account_stream_recovery() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    mount_flat_venue(&mut server).await;
    let IngestHarness {
        mut runtime,
        _account_tx,
        _order_tx,
    } = ingest_harness(0.0, 0.0);
    runtime.deps.client = standx_sdk::client::StandXClient::with_base_url(server.url()).unwrap();
    _account_tx
        .send(disconnected("scheduled rotation"))
        .await
        .unwrap();
    drop(_account_tx);

    let directive = run_cycle(&mut runtime).await;

    assert!(matches!(directive, LoopDirective::Restart));
    assert_pre_cycle_enters_account_stream_recovery(&mut runtime).await;
}

/// Facts the stream delivered before it dropped must still reach the ledger
/// exactly once; only the events after the failure are not re-read from REST.
#[tokio::test]
async fn events_queued_behind_a_disconnect_are_still_applied() {
    let IngestHarness {
        mut runtime,
        _account_tx,
        _order_tx,
    } = ingest_harness(0.0, 0.0);
    _account_tx
        .send(disconnected("scheduled rotation"))
        .await
        .unwrap();
    _account_tx
        .send(owned_order(7, "q00000001b0"))
        .await
        .unwrap();
    _account_tx
        .send(owned_trade(11, 7, "110", "0.02"))
        .await
        .unwrap();

    let directive = run_cycle(&mut runtime).await;

    assert!(matches!(directive, LoopDirective::Restart));
    assert!((runtime.loop_state.ledger.expected_position - 0.02).abs() < 1e-9);
    assert_eq!(runtime.loop_state.counters.total_fills, 1);
}

/// The cycle consumed these reconciliation obligations before the disconnect
/// aborted it. They must survive into the next cycle rather than vanish.
#[tokio::test]
async fn disconnect_does_not_swallow_pending_reconciliation_obligations() {
    let IngestHarness {
        mut runtime,
        _account_tx,
        _order_tx,
    } = ingest_harness(0.0, 0.0);
    runtime.recovery.account_position_mismatch = Some(0.5);
    runtime.recovery.account_order_reconciliation_required = true;
    _account_tx
        .send(disconnected("scheduled rotation"))
        .await
        .unwrap();

    let directive = run_cycle(&mut runtime).await;

    assert!(matches!(directive, LoopDirective::Restart));
    assert_eq!(runtime.recovery.account_position_mismatch, Some(0.5));
    assert!(runtime.recovery.account_order_reconciliation_required);
}

/// A position event invalidates the cycle first, so the runtime freezes for
/// reconciliation; the disconnect that follows in the same drain must upgrade
/// the still-queued cleanup instead of being swallowed by the frozen state.
#[tokio::test]
async fn disconnect_after_invalidating_event_upgrades_the_frozen_recovery_target() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    mount_flat_venue(&mut server).await;
    let IngestHarness {
        mut runtime,
        _account_tx,
        _order_tx,
    } = ingest_harness(0.0, 0.2);
    runtime.deps.client = standx_sdk::client::StandXClient::with_base_url(server.url()).unwrap();
    _account_tx.send(position_event("0.2")).await.unwrap();
    _account_tx
        .send(disconnected("scheduled rotation"))
        .await
        .unwrap();
    drop(_account_tx);

    let directive = run_cycle(&mut runtime).await;

    assert!(
        matches!(directive, LoopDirective::Restart),
        "a disconnect behind an invalidating event must still reach recovery, got {}",
        describe(&directive)
    );
    assert!(!account_stream_healthy(&runtime));
    take_cleanup_effect(
        &mut runtime.recovery.runtime_state,
        RecoveryTarget::AccountStream,
    )
    .expect("the queued cleanup must have been upgraded to the account-stream target");
}

/// Reconciliation window: the stream channel closes while REST backfill is
/// still explaining a position gap. That is a disconnect, not an unvalidated
/// event: the window keeps converging over REST and recovery resumes into the
/// account-stream phase.
#[tokio::test]
async fn closed_channel_in_reconciliation_window_is_not_a_hard_stop() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    mount_flat_venue(&mut server).await;
    let IngestHarness {
        mut runtime,
        _account_tx,
        _order_tx,
    } = ingest_harness(0.0, 0.0);
    runtime.deps.client = standx_sdk::client::StandXClient::with_base_url(server.url()).unwrap();
    runtime.recovery.account_position_mismatch = Some(0.5);
    // The stream must stay open through the cycle's select and close while the
    // 500ms-first reconciliation window is waiting.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        drop(_account_tx);
    });

    let directive = run_cycle(&mut runtime).await;

    assert!(
        matches!(directive, LoopDirective::Restart),
        "a closed channel inside the window must not fail recovery, got {}",
        describe(&directive)
    );
    assert!(
        !account_stream_healthy(&runtime),
        "the closed channel must leave the stream unhealthy for the next phase"
    );
    assert_window_recovery_then_refrozen_by_pre_cycle(&mut runtime).await;
}

/// Same window, but the rotation arrives as the explicit event rather than a
/// bare close.
#[tokio::test]
async fn disconnect_event_in_reconciliation_window_is_not_a_hard_stop() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    mount_flat_venue(&mut server).await;
    let IngestHarness {
        mut runtime,
        _account_tx,
        _order_tx,
    } = ingest_harness(0.0, 0.0);
    runtime.deps.client = standx_sdk::client::StandXClient::with_base_url(server.url()).unwrap();
    runtime.recovery.account_position_mismatch = Some(0.5);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        _account_tx
            .send(disconnected("scheduled rotation"))
            .await
            .unwrap();
    });

    let directive = run_cycle(&mut runtime).await;

    assert!(
        matches!(directive, LoopDirective::Restart),
        "a disconnect event inside the window must not fail recovery, got {}",
        describe(&directive)
    );
    assert!(!account_stream_healthy(&runtime));
    assert_window_recovery_then_refrozen_by_pre_cycle(&mut runtime).await;
}

/// Position-then-disconnect upgrades the queued reconciliation cleanup; the
/// upgraded freeze must also reach account-stream recovery from pre-cycle.
#[tokio::test]
async fn upgraded_freeze_then_pre_cycle_reaches_account_stream_recovery() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    mount_venue(&mut server, &long_position("0.2")).await;
    let IngestHarness {
        mut runtime,
        _account_tx,
        _order_tx,
    } = ingest_harness(0.0, 0.2);
    runtime.deps.client = standx_sdk::client::StandXClient::with_base_url(server.url()).unwrap();
    _account_tx.send(position_event("0.2")).await.unwrap();
    _account_tx
        .send(disconnected("scheduled rotation"))
        .await
        .unwrap();
    drop(_account_tx);

    let directive = run_cycle(&mut runtime).await;

    assert!(matches!(directive, LoopDirective::Restart));
    assert_pre_cycle_enters_account_stream_recovery(&mut runtime).await;
}

/// A lost stream must not launder an unexplained venue position: the window
/// still fails closed when REST cannot reconcile the gap.
#[tokio::test]
async fn stream_lost_in_window_with_unexplained_position_fails_closed() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    mount_venue(&mut server, &long_position("0.5")).await;
    let IngestHarness {
        mut runtime,
        _account_tx,
        _order_tx,
    } = ingest_harness(0.0, 0.0);
    runtime.deps.client = standx_sdk::client::StandXClient::with_base_url(server.url()).unwrap();
    runtime.recovery.account_position_mismatch = Some(0.5);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        drop(_account_tx);
    });

    let directive = run_cycle(&mut runtime).await;

    let text = describe(&directive);
    assert!(
        matches!(directive, LoopDirective::Exit(_)),
        "an unexplained position must stop, got {text}"
    );
    assert!(text.contains("after 3s freeze"), "{text}");
}
