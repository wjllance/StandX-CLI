use crate::cli::*;
use crate::output;
use anyhow::Result;
use serde::Serialize;
use standx_sdk::client::order::CreateOrderParams;
use standx_sdk::client::StandXClient;
use standx_sdk::models::{OrderSide, OrderType, TimeInForce};
use standx_sdk::order_response::{OrderResponse, OrderResponseStream};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Serialize, PartialEq, Eq)]
struct WsOrderResult {
    transport: &'static str,
    operation: &'static str,
    symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    order_id: Option<String>,
    request_id: String,
    response_code: i64,
    response_message: String,
    accepted: bool,
}

impl WsOrderResult {
    fn new(
        operation: &'static str,
        symbol: String,
        order_id: Option<String>,
        request_id: String,
        response: OrderResponse,
    ) -> Self {
        let accepted = response.accepted();
        Self {
            transport: "ws",
            operation,
            symbol,
            order_id,
            request_id,
            response_code: response.code,
            response_message: response.message,
            accepted,
        }
    }
}

/// Handle order commands.
///
/// `verbose` is passed only to the order-response stream, which writes raw
/// post-authentication inbound frames to stderr.
pub async fn handle_order(
    command: OrderCommands,
    output_format: OutputFormat,
    verbose: bool,
) -> Result<()> {
    match command {
        OrderCommands::Create {
            symbol,
            side,
            order_type,
            qty,
            price,
            tif,
            reduce_only,
            sl_price,
            tp_price,
            transport,
            timeout_secs,
        } => {
            let side = match side.to_lowercase().as_str() {
                "buy" => OrderSide::Buy,
                "sell" => OrderSide::Sell,
                _ => return Err(anyhow::anyhow!("Invalid side: {}", side)),
            };

            let order_type = match order_type.to_lowercase().as_str() {
                "limit" => OrderType::Limit,
                "market" => OrderType::Market,
                _ => return Err(anyhow::anyhow!("Invalid order type: {}", order_type)),
            };

            let time_in_force = tif.map(|t| match t.to_uppercase().as_str() {
                "GTC" => TimeInForce::Gtc,
                "IOC" => TimeInForce::Ioc,
                "FOK" => TimeInForce::Fok,
                "ALO" => TimeInForce::Alo,
                _ => TimeInForce::Gtc,
            });

            let params = CreateOrderParams {
                symbol: symbol.clone(),
                cl_ord_id: None,
                side,
                order_type,
                quantity: qty,
                price,
                time_in_force,
                reduce_only,
                stop_price: None,
                sl_price,
                tp_price,
            };

            match transport {
                OrderTransport::Http => create_order_http(params).await,
                OrderTransport::Ws => {
                    let (request_id, response) =
                        create_order_ws(&params, Duration::from_secs(timeout_secs), verbose)
                            .await?;
                    finish_ws_result(
                        WsOrderResult::new("create", symbol, None, request_id, response),
                        output_format,
                    )
                }
            }
        }
        OrderCommands::Cancel {
            symbol,
            order_id,
            transport,
            timeout_secs,
        } => match transport {
            OrderTransport::Http => cancel_order_http(&symbol, &order_id).await,
            OrderTransport::Ws => {
                let (request_id, response) =
                    cancel_order_ws(&order_id, Duration::from_secs(timeout_secs), verbose).await?;
                finish_ws_result(
                    WsOrderResult::new("cancel", symbol, Some(order_id), request_id, response),
                    output_format,
                )
            }
        },
        OrderCommands::CancelAll { symbol } => {
            let client = StandXClient::new()?;
            client.cancel_all_orders(&symbol).await?;
            println!("✅ All orders for {} cancelled successfully", symbol);
            Ok(())
        }
    }
}

async fn create_order_http(params: CreateOrderParams) -> Result<()> {
    let client = StandXClient::new()?;
    let order = client.create_order(params).await?;
    println!("✅ Order created successfully!");
    println!("   Order ID: {}", order.id);
    println!("   Symbol: {}", order.symbol);
    println!("   Side: {:?}", order.side);
    println!("   Type: {:?}", order.order_type);
    println!("   Quantity: {}", order.qty);
    if !order.price.is_empty() && order.price != "0" {
        println!("   Price: {}", order.price);
    }
    Ok(())
}

async fn cancel_order_http(symbol: &str, order_id: &str) -> Result<()> {
    let client = StandXClient::new()?;
    client.cancel_order(symbol, order_id).await?;
    println!("✅ Order {} cancelled successfully", order_id);
    Ok(())
}

async fn create_order_ws(
    params: &CreateOrderParams,
    timeout: Duration,
    verbose: bool,
) -> Result<(String, OrderResponse)> {
    run_ws_command(WsCommand::Create(params), timeout, verbose).await
}

async fn cancel_order_ws(
    order_id: &str,
    timeout: Duration,
    verbose: bool,
) -> Result<(String, OrderResponse)> {
    run_ws_command(WsCommand::Cancel(order_id), timeout, verbose).await
}

enum WsCommand<'a> {
    Create(&'a CreateOrderParams),
    Cancel(&'a str),
}

async fn run_ws_command(
    command: WsCommand<'_>,
    timeout: Duration,
    verbose: bool,
) -> Result<(String, OrderResponse)> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let stream = OrderResponseStream::new(session_id)?.with_verbose(verbose);
    let (commands, mut responses, _health, handle) = stream.connect().await?;

    // Always stop the short-lived connection task, including send/response
    // failures. A written command is never retried or downgraded to REST:
    // after that point the venue state may have changed even if no response
    // reaches this process.
    let result = async {
        let prepared = match command {
            WsCommand::Create(params) => commands.prepare_create_order(params)?,
            WsCommand::Cancel(order_id) => commands.prepare_cancel_order(order_id)?,
        };
        let request_id = prepared.request_id().to_string();
        commands.send_prepared(prepared).await.map_err(|error| {
            anyhow::anyhow!(
                "failed to send WebSocket order command for request_id={request_id}: {error}; \
                 submission state is unknown, verify account orders before retrying"
            )
        })?;
        let response = await_ws_response(&mut responses, &request_id, timeout).await?;
        Ok((request_id, response))
    }
    .await;
    handle.abort();
    result
}

async fn await_ws_response(
    responses: &mut mpsc::Receiver<OrderResponse>,
    request_id: &str,
    timeout: Duration,
) -> Result<OrderResponse> {
    let response = tokio::time::timeout(timeout, responses.recv())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out waiting for WebSocket order response for request_id={request_id}; \
                 submission state is unknown, verify account orders before retrying"
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "WebSocket order-response stream closed for request_id={request_id}; \
                 submission state is unknown, verify account orders before retrying"
            )
        })?;

    if response.request_id.as_deref() != Some(request_id) {
        return Err(anyhow::anyhow!(
            "received uncorrelated WebSocket order response: expected request_id={request_id}, \
             got request_id={}; submission state is unknown, verify account orders before retrying",
            response.request_id.as_deref().unwrap_or("<missing>")
        ));
    }
    Ok(response)
}

fn finish_ws_result(result: WsOrderResult, output_format: OutputFormat) -> Result<()> {
    emit_ws_result(&result, output_format)?;
    if result.accepted {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "WebSocket order:{} rejected for request_id={}: code={} message={}",
            result.operation,
            result.request_id,
            result.response_code,
            result.response_message
        ))
    }
}

fn emit_ws_result(result: &WsOrderResult, output_format: OutputFormat) -> Result<()> {
    let Some(rendered) = render_ws_result(result, output_format)? else {
        return Ok(());
    };
    if rendered.ends_with('\n') {
        print!("{rendered}");
    } else {
        println!("{rendered}");
    }
    Ok(())
}

fn render_ws_result(result: &WsOrderResult, output_format: OutputFormat) -> Result<Option<String>> {
    let rendered = match output_format {
        OutputFormat::Table => {
            let status = if result.accepted {
                "✅ WebSocket order command accepted"
            } else {
                "❌ WebSocket order command rejected"
            };
            let mut rendered = format!(
                "{status}\n   Transport: {}\n   Operation: {}\n   Symbol: {}\n",
                result.transport, result.operation, result.symbol
            );
            if let Some(order_id) = result.order_id.as_deref() {
                rendered.push_str(&format!("   Order ID: {order_id}\n"));
            }
            rendered.push_str(&format!(
                "   Request ID: {}\n   Response code: {}\n   Response message: {}\n   Accepted: {}",
                result.request_id, result.response_code, result.response_message, result.accepted
            ));
            Some(rendered)
        }
        OutputFormat::Json => Some(output::format_json(result)?),
        OutputFormat::Csv => Some(output::format_csv(std::slice::from_ref(result))?),
        OutputFormat::Quiet => None,
    };
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response(request_id: Option<&str>, code: i64, message: &str) -> OrderResponse {
        OrderResponse {
            code,
            message: message.to_string(),
            request_id: request_id.map(str::to_string),
        }
    }

    #[tokio::test]
    async fn awaits_only_the_correlated_response() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(response(Some("req-1"), 0, "accepted"))
            .await
            .unwrap();

        let received = await_ws_response(&mut rx, "req-1", Duration::from_millis(50))
            .await
            .unwrap();
        assert!(received.accepted());
    }

    #[tokio::test]
    async fn reports_unknown_state_on_timeout_or_closed_stream() {
        let (_tx, mut rx) = mpsc::channel(1);
        let timeout = await_ws_response(&mut rx, "req-timeout", Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(timeout.to_string().contains("request_id=req-timeout"));
        assert!(timeout.to_string().contains("submission state is unknown"));

        let (tx, mut rx) = mpsc::channel(1);
        drop(tx);
        let closed = await_ws_response(&mut rx, "req-closed", Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(closed.to_string().contains("request_id=req-closed"));
        assert!(closed.to_string().contains("submission state is unknown"));
    }

    #[tokio::test]
    async fn rejects_uncorrelated_response_with_both_request_ids() {
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(response(Some("other"), 0, "accepted"))
            .await
            .unwrap();

        let error = await_ws_response(&mut rx, "expected", Duration::from_millis(50))
            .await
            .unwrap_err();
        let error = error.to_string();
        assert!(error.contains("expected request_id=expected"));
        assert!(error.contains("got request_id=other"));
        assert!(error.contains("submission state is unknown"));
    }

    #[test]
    fn ws_result_has_stable_machine_fields_for_acceptance_and_rejection() {
        let accepted = WsOrderResult::new(
            "create",
            "BTC-USD".to_string(),
            None,
            "req-1".to_string(),
            response(Some("req-1"), 0, "accepted"),
        );
        assert_eq!(
            serde_json::to_value(&accepted).unwrap(),
            json!({
                "transport": "ws",
                "operation": "create",
                "symbol": "BTC-USD",
                "request_id": "req-1",
                "response_code": 0,
                "response_message": "accepted",
                "accepted": true,
            })
        );

        let rejected = WsOrderResult::new(
            "cancel",
            "BTC-USD".to_string(),
            Some("42".to_string()),
            "req-2".to_string(),
            response(Some("req-2"), 400, "order already closed"),
        );
        assert!(!rejected.accepted);
        assert_eq!(rejected.order_id.as_deref(), Some("42"));
    }

    #[test]
    fn ws_result_renders_table_json_csv_and_quiet_without_cross_format_noise() {
        let result = WsOrderResult::new(
            "cancel",
            "BTC-USD".to_string(),
            Some("42".to_string()),
            "req-1".to_string(),
            response(Some("req-1"), 0, "accepted"),
        );

        let table = render_ws_result(&result, OutputFormat::Table)
            .unwrap()
            .unwrap();
        assert!(table.contains("WebSocket order command accepted"));
        assert!(table.contains("Transport: ws"));
        assert!(table.contains("Request ID: req-1"));
        assert!(table.contains("Accepted: true"));

        let json = render_ws_result(&result, OutputFormat::Json)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["request_id"],
            "req-1"
        );
        assert!(!json.contains("WebSocket Order Debug"));

        let csv = render_ws_result(&result, OutputFormat::Csv)
            .unwrap()
            .unwrap();
        assert!(csv.starts_with("transport,operation,symbol,order_id,request_id"));
        assert!(csv.contains("ws,cancel,BTC-USD,42,req-1,0,accepted,true"));

        assert!(render_ws_result(&result, OutputFormat::Quiet)
            .unwrap()
            .is_none());
    }

    #[test]
    fn ws_acceptance_succeeds_and_rejection_returns_an_error() {
        let accepted = WsOrderResult::new(
            "create",
            "BTC-USD".to_string(),
            None,
            "req-ok".to_string(),
            response(Some("req-ok"), 0, "accepted"),
        );
        assert!(finish_ws_result(accepted, OutputFormat::Quiet).is_ok());

        let rejected = WsOrderResult::new(
            "cancel",
            "BTC-USD".to_string(),
            Some("42".to_string()),
            "req-rejected".to_string(),
            response(Some("req-rejected"), 400, "order already closed"),
        );
        let error = finish_ws_result(rejected, OutputFormat::Quiet).unwrap_err();
        assert!(error.to_string().contains("request_id=req-rejected"));
        assert!(error.to_string().contains("code=400"));
    }
}
