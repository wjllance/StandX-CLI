//! Account-floor and accounting-invariant exits must freeze the runtime and
//! clean the venue book before any webhook is awaited. A webhook retries for
//! up to ~18s (3 x 5s timeout + backoff), and the previous cycle's quotes are
//! still resting for that whole time. Mirrors the stop-loss ordering pinned by
//! `stop_loss_cleans_venue_orders_before_delivering_any_webhook`.

use super::super::cycle_flow::CycleAttempt;
use super::recovery::JwtGuard;
use super::runtime_flow::{ingest_harness, IngestHarness};
use super::*;
use mockito::{Matcher, Server, ServerGuard};
use std::sync::{Arc, Mutex};

type EventLog = Arc<Mutex<Vec<String>>>;
type Payloads = Arc<Mutex<Vec<serde_json::Value>>>;

fn logged(log: &EventLog) -> Vec<String> {
    log.lock().unwrap().clone()
}

/// A venue with one resting maker order. Every cancel, the empty-book
/// verification, and every webhook POST is appended to the returned log in
/// arrival order, so ordering is observed at the venue boundary rather than
/// inferred from code structure.
async fn venue_with_one_resting_order(server: &mut ServerGuard) -> (EventLog, Payloads) {
    let log: EventLog = Arc::new(Mutex::new(Vec::new()));
    let payloads: Payloads = Arc::new(Mutex::new(Vec::new()));
    server
        .mock("GET", "/api/query_open_orders")
        .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"{"code":0,"message":"ok","result":[
                {"id":"42","cl_ord_id":"sxmk-test-q00000001b0","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"open","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:00Z"}
            ]}"#,
        )
        .expect(1)
        .create_async()
        .await;
    let cancel_log = Arc::clone(&log);
    server
        .mock("POST", "/api/cancel_orders")
        .match_body(Matcher::Json(serde_json::json!({ "order_id_list": [42] })))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |_| {
            cancel_log.lock().unwrap().push("cancel".to_string());
            br#"{"code":0,"message":"accepted"}"#.to_vec()
        })
        .expect(1)
        .create_async()
        .await;
    let verified_log = Arc::clone(&log);
    server
        .mock("GET", "/api/query_open_orders")
        .match_query(Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body_from_request(move |_| {
            verified_log
                .lock()
                .unwrap()
                .push("book_verified_empty".to_string());
            br#"{"code":0,"message":"ok","result":[]}"#.to_vec()
        })
        .create_async()
        .await;
    server
        .mock("GET", "/api/query_order")
        .match_query(Matcher::UrlEncoded("order_id".into(), "42".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"id":"42","cl_ord_id":"sxmk-test-q00000001b0","symbol":"BTC-USD","side":"buy","order_type":"limit","qty":"0.001","fill_qty":"0","price":"63000","status":"canceled","created_at":"2026-07-10T00:00:00Z","updated_at":"2026-07-10T00:00:01Z"}"#)
        .create_async()
        .await;
    server
        .mock("GET", "/api/query_positions")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create_async()
        .await;
    let webhook_log = Arc::clone(&log);
    let webhook_payloads = Arc::clone(&payloads);
    server
        .mock("POST", "/webhook")
        .with_status(200)
        .with_body_from_request(move |request| {
            let body = String::from_utf8_lossy(request.body().unwrap()).to_string();
            let kind = ["account_floor", "accounting_invariant"]
                .into_iter()
                .find(|kind| body.contains(kind))
                .unwrap_or("other");
            webhook_log.lock().unwrap().push(format!("webhook:{kind}"));
            if let Ok(value) = serde_json::from_str(&body) {
                webhook_payloads.lock().unwrap().push(value);
            }
            b"ok".to_vec()
        })
        .expect_at_least(1)
        .create_async()
        .await;
    (log, payloads)
}

fn runtime_against(server: &ServerGuard, stop_loss: f64, start: f64) -> MakerRuntime {
    let IngestHarness {
        mut runtime,
        _account_tx,
        _order_tx,
    } = ingest_harness(stop_loss, start);
    // The harness drops its senders with this scope; keep them open for the
    // life of the runtime so a closed channel cannot become a different exit.
    std::mem::forget(_account_tx);
    std::mem::forget(_order_tx);
    runtime.deps.client = standx_sdk::client::StandXClient::with_base_url(server.url()).unwrap();
    runtime.deps.notifier = MakerNotifier::new(
        OutputFormat::Quiet,
        Some(format!("{}/webhook", server.url())),
        crate::cli::AlertWebhookFormat::Raw,
    );
    // Not due: keep the JWT expiry monitor from sending its own notices.
    runtime.lifecycle.last_token_expiry_check = Some(std::time::Instant::now());
    runtime
}

/// The delivered notice for `kind`, as the webhook received it.
fn notice_payload(payloads: &Payloads, kind: &str) -> serde_json::Value {
    payloads
        .lock()
        .unwrap()
        .iter()
        .find(|payload| payload["kind"] == kind)
        .cloned()
        .unwrap_or_else(|| panic!("no {kind} notice delivered"))
}

fn expect_exit(directive: LoopDirective, what: &str) -> MakerExit {
    match directive {
        LoopDirective::Exit(exit) => exit,
        LoopDirective::Proceed => panic!("{what}: expected exit, got proceed"),
        LoopDirective::Restart => panic!("{what}: expected exit, got restart"),
    }
}

/// The ordering contract, observed at the venue: at the moment the flow hands
/// the exit to shutdown no webhook has been delivered (the flow itself must not
/// await one), and during shutdown the book is cancelled and verified empty
/// before the first webhook, with exactly one notice for the triggering cause.
fn assert_cleanup_precedes_notification(log: &EventLog, at_exit: &[String], kind: &str) {
    assert!(
        at_exit.iter().all(|entry| !entry.starts_with("webhook:")),
        "the exit path awaited a webhook before shutdown cleanup: {at_exit:?}"
    );
    let all = logged(log);
    let first_webhook = all
        .iter()
        .position(|entry| entry.starts_with("webhook:"))
        .expect("the stop notice must still be delivered");
    let cancel = all.iter().position(|entry| entry == "cancel");
    let verified = all.iter().position(|entry| entry == "book_verified_empty");
    assert!(
        cancel.is_some_and(|cancel| cancel < first_webhook),
        "orders must be cancelled before any webhook: {all:?}"
    );
    assert!(
        verified.is_some_and(|verified| verified < first_webhook),
        "the empty book must be verified before any webhook: {all:?}"
    );
    let expected = format!("webhook:{kind}");
    assert_eq!(
        all.iter().filter(|entry| **entry == expected).count(),
        1,
        "exactly one {kind} notice must be delivered: {all:?}"
    );
}

#[tokio::test]
async fn account_floor_exit_cleans_the_book_before_delivering_the_webhook() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    let (log, payloads) = venue_with_one_resting_order(&mut server).await;
    let mut runtime = runtime_against(&server, 0.0, 0.0);
    let work_token = take_cycle_work(&mut runtime.recovery.runtime_state)
        .expect("cycle work lookup succeeds")
        .expect("startup schedules cycle work");
    let attempt = CycleAttempt {
        work_token,
        exit_pending_before: false,
        breaker_halted_before: false,
        result: Err(anyhow::Error::new(AccountFloorError::breach(
            "equity", 40.0, 50.0,
        ))),
    };

    let exit = expect_exit(runtime.finish_cycle(attempt).await, "account floor");
    assert!(matches!(exit, MakerExit::AccountFloor(_)), "{exit:?}");
    let at_exit = logged(&log);
    runtime.recovery.runtime_state.handle(MakerEvent::Timer);
    assert!(
        runtime.recovery.runtime_state.pending_effect().is_none(),
        "the runtime must already be stopping when the exit is handed over"
    );

    assert!(runtime.shutdown(exit).await.is_err());
    assert_cleanup_precedes_notification(&log, &at_exit, "account_floor");
    let notice = notice_payload(&payloads, "account_floor");
    assert_eq!(notice["severity"], "critical");
    assert_eq!(notice["event"], "triggered");
    assert_eq!(
        notice["message"],
        "account equity 40.00 < floor 50.00; shutting down"
    );
    assert_eq!(notice["position_after"], 0.0);
    assert_eq!(notice["expected_position"], 0.0);
}

/// An unevaluable floor keeps its distinct event label through the deferral.
#[tokio::test]
async fn unevaluable_account_floor_keeps_its_event_label_after_cleanup() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    let (log, payloads) = venue_with_one_resting_order(&mut server).await;
    let mut runtime = runtime_against(&server, 0.0, 0.0);
    let work_token = take_cycle_work(&mut runtime.recovery.runtime_state)
        .expect("cycle work lookup succeeds")
        .expect("startup schedules cycle work");
    let attempt = CycleAttempt {
        work_token,
        exit_pending_before: false,
        breaker_halted_before: false,
        result: Err(anyhow::Error::new(AccountFloorError::balance_stale(
            120, 60,
        ))),
    };

    let exit = expect_exit(runtime.finish_cycle(attempt).await, "stale floor");
    let at_exit = logged(&log);
    assert!(runtime.shutdown(exit).await.is_err());

    assert_cleanup_precedes_notification(&log, &at_exit, "account_floor");
    assert_eq!(
        notice_payload(&payloads, "account_floor")["event"],
        "unevaluable"
    );
}

#[tokio::test]
async fn in_cycle_accounting_invariant_exit_cleans_the_book_before_the_webhook() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    let (log, payloads) = venue_with_one_resting_order(&mut server).await;
    let mut runtime = runtime_against(&server, 0.0, 0.0);
    // Ledger and session stats disagree beyond tolerance.
    runtime.loop_state.ledger.expected_position = 0.5;
    runtime.test_buffered_cycle = Some(TestBufferedCycle {
        events: Vec::new(),
        mark: 100.0,
        fills: 0,
    });

    let directive = match runtime.execute_cycle().await {
        Err(directive) => directive,
        Ok(_) => panic!("an accounting mismatch must exit before the cycle completes"),
    };
    let exit = expect_exit(directive, "in-cycle accounting invariant");
    assert!(
        matches!(exit, MakerExit::AccountingInvariant(_)),
        "{exit:?}"
    );
    let at_exit = logged(&log);
    runtime.recovery.runtime_state.handle(MakerEvent::Timer);
    assert!(runtime.recovery.runtime_state.pending_effect().is_none());

    assert!(runtime.shutdown(exit).await.is_err());
    assert_cleanup_precedes_notification(&log, &at_exit, "accounting_invariant");
    let notice = notice_payload(&payloads, "accounting_invariant");
    assert_eq!(notice["severity"], "critical");
    assert_eq!(notice["event"], "mismatch");
    assert_eq!(notice["expected_position"], 0.5);
    assert_eq!(notice["observed_position"], 0.0);
    assert!(notice["message"]
        .as_str()
        .is_some_and(|message| message.contains("differs from ledger expected")));
}

#[tokio::test]
async fn pre_cycle_accounting_invariant_exit_cleans_the_book_before_the_webhook() {
    let _jwt = JwtGuard::set();
    let mut server = Server::new_async().await;
    let (log, payloads) = venue_with_one_resting_order(&mut server).await;
    let mut runtime = runtime_against(&server, 0.0, 0.0);
    runtime.loop_state.ledger.expected_position = 0.5;

    let exit = expect_exit(
        runtime.pre_cycle_phase().await,
        "pre-cycle accounting invariant",
    );
    assert!(
        matches!(exit, MakerExit::AccountingInvariant(_)),
        "{exit:?}"
    );
    let at_exit = logged(&log);
    runtime.recovery.runtime_state.handle(MakerEvent::Timer);
    assert!(runtime.recovery.runtime_state.pending_effect().is_none());

    assert!(runtime.shutdown(exit).await.is_err());
    assert_cleanup_precedes_notification(&log, &at_exit, "accounting_invariant");
    let notice = notice_payload(&payloads, "accounting_invariant");
    assert_eq!(notice["severity"], "critical");
    assert_eq!(notice["event"], "mismatch");
    assert_eq!(notice["expected_position"], 0.5);
    assert_eq!(notice["observed_position"], 0.0);
    assert!(notice["message"]
        .as_str()
        .is_some_and(|message| message.contains("differs from ledger expected")));
}
