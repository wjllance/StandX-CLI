//! Hermetic CLI process contracts.
//!
//! Every command that performs I/O is routed to a loopback test server through
//! `--endpoint`. These tests must never fall back to the public StandX API.

use assert_cmd::cargo::cargo_bin_cmd;
use mockito::{Matcher, Server};
use std::process::Output;
use tempfile::TempDir;

const SYMBOLS: &str = r#"[{
    "symbol":"MOCK-USD",
    "base_asset":"MOCK",
    "quote_asset":"DUSD",
    "base_decimals":9,
    "price_tick_decimals":2,
    "qty_tick_decimals":4,
    "min_order_qty":"0.0001",
    "def_leverage":"10",
    "max_leverage":"40",
    "maker_fee":"0.0001",
    "taker_fee":"0.0004",
    "status":"trading"
}]"#;

const TICKER: &str = r#"{
    "symbol":"BTC-USD",
    "mark_price":"68000.00",
    "index_price":"68001.50",
    "last_price":"67999.50",
    "volume_24h":"1234567.89",
    "high_price_24h":"69000.00",
    "low_price_24h":"67000.00",
    "funding_rate":"0.0001",
    "next_funding_time":"2026-02-24T16:00:00Z"
}"#;

const DEPTH: &str = r#"{
    "symbol":"BTC-USD",
    "bids":[["68000.00","1.0"]],
    "asks":[["68100.00","0.5"]],
    "timestamp":"2026-02-24T15:00:00Z"
}"#;

const FUNDING: &str = r#"[{
    "id":12345,
    "symbol":"BTC-USD",
    "funding_rate":"0.00001250",
    "mark_price":"68000.00",
    "index_price":"68001.50",
    "premium":"0.00000100",
    "time":"2026-02-24T16:00:00Z",
    "created_at":"2026-02-24T16:00:00Z",
    "updated_at":"2026-02-24T16:00:00Z"
}]"#;

fn run(endpoint: Option<&str>, args: &[&str]) -> Output {
    let home = TempDir::new().expect("temporary CLI home");
    let mut command = cargo_bin_cmd!("standx");
    command.env("HOME", home.path());
    if let Some(endpoint) = endpoint {
        command.args(["--endpoint", endpoint]);
    }
    command.args(args);
    command.output().expect("standx command executes")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

#[test]
fn version_and_help_contracts_do_not_require_network() {
    let version = run(None, &["--version"]);
    assert!(version.status.success(), "{}", stderr(&version));
    let version_stdout = stdout(&version);
    assert!(version_stdout.contains("standx"));
    assert!(version_stdout.contains(env!("CARGO_PKG_VERSION")));

    let help = run(None, &["--help"]);
    assert!(help.status.success(), "{}", stderr(&help));
    let help_stdout = stdout(&help);
    assert!(help_stdout.contains("OpenClaw"));
    assert!(help_stdout.contains("Usage:"));

    let market_help = run(None, &["market", "--help"]);
    assert!(market_help.status.success(), "{}", stderr(&market_help));
    let market_help_stdout = stdout(&market_help);
    assert!(market_help_stdout.contains("symbols"));
    assert!(market_help_stdout.contains("ticker"));
}

#[test]
fn symbols_support_all_output_formats_against_the_endpoint_override() {
    let mut server = Server::new();
    let symbols = server
        .mock("GET", "/api/query_symbol_info")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SYMBOLS)
        .expect(4)
        .create();
    let endpoint = server.url();

    let json = run(Some(&endpoint), &["--output", "json", "market", "symbols"]);
    assert!(json.status.success(), "{}", stderr(&json));
    let json_value: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("symbols JSON output");
    assert_eq!(json_value[0]["symbol"], "MOCK-USD");

    let table = run(Some(&endpoint), &["--output", "table", "market", "symbols"]);
    assert!(table.status.success(), "{}", stderr(&table));
    let table_stdout = stdout(&table);
    assert!(table_stdout.contains("MOCK-USD"));
    assert!(table_stdout.contains("Symbol"));

    let csv = run(Some(&endpoint), &["--output", "csv", "market", "symbols"]);
    assert!(csv.status.success(), "{}", stderr(&csv));
    let csv_stdout = stdout(&csv);
    assert!(csv_stdout.starts_with("symbol,base_asset,quote_asset"));
    assert!(csv_stdout.contains("MOCK-USD,MOCK,DUSD"));

    let quiet = run(Some(&endpoint), &["--output", "quiet", "market", "symbols"]);
    assert!(quiet.status.success(), "{}", stderr(&quiet));
    assert!(quiet.stdout.is_empty());

    symbols.assert();
}

#[test]
fn ticker_contract_is_exact_and_uses_the_endpoint_override() {
    let mut server = Server::new();
    let ticker = server
        .mock("GET", "/api/query_symbol_market")
        .match_query(Matcher::UrlEncoded(
            "symbol".to_string(),
            "BTC-USD".to_string(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(TICKER)
        .expect(1)
        .create();

    let output = run(
        Some(&server.url()),
        &["--output", "json", "market", "ticker", "BTC-USD"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("ticker JSON output");
    assert_eq!(value["symbol"], "BTC-USD");
    assert_eq!(value["last_price"], "67999.50");
    ticker.assert();
}

#[test]
fn depth_contract_is_exact_and_uses_the_endpoint_override() {
    let mut server = Server::new();
    let depth = server
        .mock("GET", "/api/query_depth_book")
        .match_query(Matcher::UrlEncoded(
            "symbol".to_string(),
            "BTC-USD".to_string(),
        ))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(DEPTH)
        .expect(1)
        .create();

    let output = run(
        Some(&server.url()),
        &["--output", "json", "market", "depth", "BTC-USD"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("depth JSON output");
    assert_eq!(value["symbol"], "BTC-USD");
    assert_eq!(value["bids"][0][0], "68000.00");
    assert_eq!(value["asks"][0][0], "68100.00");
    depth.assert();
}

#[test]
fn funding_contract_is_exact_and_uses_the_endpoint_override() {
    let mut server = Server::new();
    let funding = server
        .mock("GET", "/api/query_funding_rates")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(FUNDING)
        .expect(1)
        .create();

    let output = run(
        Some(&server.url()),
        &[
            "--output", "json", "market", "funding", "BTC-USD", "--days", "1",
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("funding JSON output");
    assert_eq!(value[0]["symbol"], "BTC-USD");
    assert_eq!(value[0]["funding_rate"], "0.00001250");
    funding.assert();
}

#[test]
fn api_failure_is_nonzero_and_machine_readable() {
    let mut server = Server::new();
    let symbols = server
        .mock("GET", "/api/query_symbol_info")
        .with_status(503)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":"temporarily unavailable"}"#)
        .expect(1)
        .create();

    let output = run(
        Some(&server.url()),
        &["--output", "json", "market", "symbols"],
    );
    assert!(!output.status.success(), "command unexpectedly succeeded");
    let value: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("structured error JSON");
    // The CLI currently boxes command-handler errors through anyhow, so the
    // outer process contract is UNKNOWN_ERROR even when the cause is typed.
    // Keep the current observable contract explicit; changing the error type
    // belongs in a separate output-contract change.
    assert_eq!(value["error"]["error_type"], "UNKNOWN_ERROR");
    assert_eq!(
        value["error"]["message"],
        "API error: 503 - {\"error\":\"temporarily unavailable\"}"
    );
    symbols.assert();
}
