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
    for path in ["/api/query_positions", "/api/query_funding_history"] {
        server
            .mock("GET", path)
            .match_query(Matcher::Any)
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body("[]")
            .create_async()
            .await;
    }
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
}
