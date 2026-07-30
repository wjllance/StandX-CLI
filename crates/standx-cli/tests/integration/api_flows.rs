//! API Flow Integration Tests
//! Tests API workflows using mock servers

use assert_cmd::cargo::cargo_bin_cmd;
use mockito::Server;
use predicates::str::contains;
use standx_cli::client::StandXClient;

#[tokio::test]
async fn test_market_data_flow() {
    let mut server = Server::new_async().await;

    // Mock symbols endpoint
    let _m1 = server.mock("GET", "/api/query_symbol_info")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"[{"symbol":"BTC-USD","base_asset":"BTC","quote_asset":"DUSD","base_decimals":9,"price_tick_decimals":2,"qty_tick_decimals":4,"min_order_qty":"0.0001","def_leverage":"10","max_leverage":"40","maker_fee":"0.0001","taker_fee":"0.0004","status":"trading"}]"#)
        .create();

    // Mock market data endpoint
    let _m2 = server.mock("GET", "/api/query_symbol_market")
        .match_query(mockito::Matcher::UrlEncoded("symbol".into(), "BTC-USD".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"symbol":"BTC-USD","mark_price":"68000.00","index_price":"68001.50","last_price":"68000.00","volume_24h":"1234567.89","high_price_24h":"69000.00","low_price_24h":"67000.00","funding_rate":"0.0001","next_funding_time":"2026-02-24T16:00:00Z"}"#)
        .create();

    let client = StandXClient::with_base_url(server.url()).unwrap();

    // Step 1: Get symbols
    let symbols = client.get_symbol_info().await.unwrap();
    assert!(!symbols.is_empty());
    assert_eq!(symbols[0].symbol, "BTC-USD");

    // Step 2: Get market data for first symbol
    let market = client.get_symbol_market("BTC-USD").await.unwrap();
    assert_eq!(market.symbol, "BTC-USD");
}

#[tokio::test]
async fn test_api_error_handling_flow() {
    let mut server = Server::new_async().await;

    // Mock 401 unauthorized
    let _m = server
        .mock("GET", "/api/query_positions")
        .with_status(401)
        .with_header("content-type", "application/json")
        .with_body(r#"{"error":"Unauthorized","message":"Invalid or missing authentication"}"#)
        .create();

    let client = StandXClient::with_base_url(server.url()).unwrap();

    // Should handle 401 gracefully
    let result = client.get_positions(None).await;
    assert!(result.is_err());
}

#[test]
fn cli_endpoint_override_routes_market_request_to_loopback() {
    let mut server = Server::new();
    let symbols = server
        .mock("GET", "/api/query_symbol_info")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            r#"[{"symbol":"MOCK-USD","base_asset":"MOCK","quote_asset":"DUSD","base_decimals":9,"price_tick_decimals":2,"qty_tick_decimals":4,"min_order_qty":"0.0001","def_leverage":"10","max_leverage":"40","maker_fee":"0.0001","taker_fee":"0.0004","status":"trading"}]"#,
        )
        .expect(1)
        .create();

    let mut command = cargo_bin_cmd!("standx");
    command.args([
        "--endpoint",
        &server.url(),
        "--output",
        "json",
        "market",
        "symbols",
    ]);
    command
        .assert()
        .success()
        .stdout(contains("\"symbol\": \"MOCK-USD\""));
    symbols.assert();
}
