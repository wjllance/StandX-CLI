//! Bounded live-gate verification for the authenticated order command socket.
//!
//! This intentionally lives in the CLI: it executes network I/O and does not
//! alter the maker planner or its normal quote decisions.

use super::model::position_for_symbol;
use super::notify::MakerNotifier;
use super::output::ts_now_millis;
use super::{FailSafeShutdown, LIVE_MAKER_ENV};
use crate::cli::{AlertWebhookFormat, OutputFormat};
use anyhow::Result;
use standx_maker::{format_decimals, round_to_decimals};
use standx_sdk::auth::Credentials;
use standx_sdk::client::order::CreateOrderParams;
use standx_sdk::client::StandXClient;
use standx_sdk::models::{Order, OrderSide, OrderType, TimeInForce};
use standx_sdk::order_response::{OrderCommandSender, OrderResponse, OrderResponseStream};
use std::time::Duration;
use tokio::sync::mpsc;

const CANARY_PREFIX: &str = "sxmk-canary-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CanaryStage {
    PreflightVerified,
    CreateSubmitted,
    CreateAccepted,
    CreateRejected,
    OrderVisible,
    CancelSubmitted,
    CancelAccepted,
    CancelRejected,
    AbsenceVerified,
    PositionVerified,
    PositionMismatch,
    CleanupStarted,
    CleanupVerified,
    CleanupFailed,
}

impl CanaryStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::PreflightVerified => "preflight_verified",
            Self::CreateSubmitted => "create_submitted",
            Self::CreateAccepted => "create_accepted",
            Self::CreateRejected => "create_rejected",
            Self::OrderVisible => "order_visible",
            Self::CancelSubmitted => "cancel_submitted",
            Self::CancelAccepted => "cancel_accepted",
            Self::CancelRejected => "cancel_rejected",
            Self::AbsenceVerified => "absence_verified",
            Self::PositionVerified => "position_verified",
            Self::PositionMismatch => "position_mismatch",
            Self::CleanupStarted => "cleanup_started",
            Self::CleanupVerified => "cleanup_verified",
            Self::CleanupFailed => "cleanup_failed",
        }
    }
}

struct CanaryEvidence<'a> {
    symbol: &'a str,
    client_order_id: &'a str,
    quantity: String,
    price: String,
}

impl CanaryEvidence<'_> {
    fn value_at(
        &self,
        timestamp: &str,
        stage: CanaryStage,
        request_id: Option<&str>,
        order_id: Option<&str>,
        response: Option<&OrderResponse>,
        position: Option<f64>,
    ) -> serde_json::Value {
        serde_json::json!({
            "ts": timestamp,
            "action": "ws_command_canary",
            "event": stage.as_str(),
            "symbol": self.symbol,
            "client_order_id": self.client_order_id,
            "request_id": request_id,
            "order_id": order_id,
            "response_code": response.map(|response| response.code),
            "response_message": response.map(|response| response.message.as_str()),
            "quantity": self.quantity,
            "price": self.price,
            "position": position,
        })
    }

    fn emit(
        &self,
        stage: CanaryStage,
        request_id: Option<&str>,
        order_id: Option<&str>,
        response: Option<&OrderResponse>,
    ) {
        let timestamp = ts_now_millis();
        println!(
            "{}",
            self.value_at(&timestamp, stage, request_id, order_id, response, None)
        );
    }

    fn emit_position(&self, stage: CanaryStage, order_id: Option<&str>, position: f64) {
        let timestamp = ts_now_millis();
        println!(
            "{}",
            self.value_at(&timestamp, stage, None, order_id, None, Some(position))
        );
    }
}

fn canary_price(mark: f64, offset_bps: f64, decimals: u32) -> Result<f64> {
    if !mark.is_finite() || mark <= 0.0 {
        return Err(anyhow::anyhow!("venue returned an invalid mark price"));
    }
    if !(1.0..=1_000.0).contains(&offset_bps) {
        return Err(anyhow::anyhow!(
            "--price-offset-bps must be between 1 and 1000 for a bounded post-only canary"
        ));
    }
    let price = round_to_decimals(mark * (1.0 - offset_bps / 10_000.0), decimals);
    if price <= 0.0 {
        return Err(anyhow::anyhow!(
            "canary price rounded to a non-positive value"
        ));
    }
    Ok(price)
}

async fn await_response(
    responses: &mut mpsc::Receiver<OrderResponse>,
    request_id: &str,
    timeout: Duration,
) -> Result<OrderResponse> {
    let response = tokio::time::timeout(timeout, responses.recv())
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for order-response acknowledgement"))?
        .ok_or_else(|| {
            anyhow::anyhow!("order-response stream closed before the expected acknowledgement")
        })?;
    if response.request_id.as_deref() == Some(request_id) {
        Ok(response)
    } else {
        Err(anyhow::anyhow!(
            "received an uncorrelated order-response acknowledgement during canary"
        ))
    }
}

async fn wait_for_order(
    client: &StandXClient,
    symbol: &str,
    client_order_id: &str,
    timeout: Duration,
) -> Result<Order> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(order) = client
            .get_open_orders(Some(symbol))
            .await?
            .into_iter()
            .find(|order| order.cl_ord_id.as_deref() == Some(client_order_id))
        {
            return Ok(order);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "canary order was not visible through the HTTP open-order snapshot"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn wait_until_absent(
    client: &StandXClient,
    symbol: &str,
    client_order_id: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let present = client
            .get_open_orders(Some(symbol))
            .await?
            .into_iter()
            .any(|order| order.cl_ord_id.as_deref() == Some(client_order_id));
        if !present {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow::anyhow!(
                "canary order remained visible after its cancellation acknowledgement"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn cleanup_current_order(
    client: &StandXClient,
    symbol: &str,
    client_order_id: &str,
    timeout: Duration,
) -> Result<Option<String>> {
    let orders = client.get_open_orders(Some(symbol)).await?;
    if let Some(order) = orders
        .into_iter()
        .find(|order| order.cl_ord_id.as_deref() == Some(client_order_id))
    {
        let order_id = order.id.clone();
        client.cancel_order(symbol, &order.id).await?;
        wait_until_absent(client, symbol, client_order_id, timeout).await?;
        return Ok(Some(order_id));
    }
    Ok(None)
}

pub(super) struct WsCommandCanaryRequest {
    pub(super) symbol: String,
    pub(super) size: Option<f64>,
    pub(super) price_offset_bps: f64,
    pub(super) timeout_secs: u64,
    pub(super) alert_webhook: String,
    pub(super) alert_webhook_format: AlertWebhookFormat,
    pub(super) output_format: OutputFormat,
}

pub(super) async fn run_ws_command_canary(
    request: WsCommandCanaryRequest,
    endpoints: &standx_sdk::StandXEndpoints,
) -> Result<()> {
    let WsCommandCanaryRequest {
        symbol,
        size,
        price_offset_bps,
        timeout_secs,
        alert_webhook,
        alert_webhook_format,
        output_format,
    } = request;
    // --timeout-secs (1..=30) and the required --alert-webhook are now enforced
    // by clap, so --help documents them and they cannot reach here invalid.
    if std::env::var(LIVE_MAKER_ENV).ok().as_deref() != Some("1") {
        return Err(anyhow::anyhow!(
            "{}=1 is required for the WS command canary",
            LIVE_MAKER_ENV
        ));
    }
    let _live_process_lock = super::process_lock::LiveProcessLock::acquire()?;
    let credentials = Credentials::load()?;
    if credentials.is_expired() || credentials.private_key.is_empty() {
        return Err(anyhow::anyhow!(
            "WS command canary requires current signing credentials"
        ));
    }

    let session_id = uuid::Uuid::new_v4().to_string();
    let client = StandXClient::from_endpoints(endpoints)?.with_session_id(session_id.clone());
    let notifier = MakerNotifier::new(output_format, Some(alert_webhook), alert_webhook_format);
    let timeout = Duration::from_secs(timeout_secs);
    let infos = client.get_symbol_info().await?;
    let info = infos
        .iter()
        .find(|info| info.symbol.eq_ignore_ascii_case(&symbol))
        .ok_or_else(|| anyhow::anyhow!("unknown symbol '{symbol}'"))?;
    if info.status != "trading" {
        return Err(anyhow::anyhow!("symbol {} is not trading", info.symbol));
    }
    let symbol = info.symbol.clone();
    if !client.get_open_orders(Some(&symbol)).await?.is_empty() {
        return Err(anyhow::anyhow!(
            "WS command canary requires an empty {} order book to avoid touching unrelated orders",
            symbol
        ));
    }
    if position_for_symbol(&client.get_positions(Some(&symbol)).await?, &symbol)?.abs() > 0.0 {
        return Err(anyhow::anyhow!(
            "WS command canary requires a flat {} position",
            symbol
        ));
    }
    let min_size: f64 = info.min_order_qty.parse().map_err(|_| {
        anyhow::anyhow!("venue returned an invalid minimum quantity for {}", symbol)
    })?;
    let quantity = round_to_decimals(size.unwrap_or(min_size), info.qty_tick_decimals);
    if quantity < min_size || quantity <= 0.0 {
        return Err(anyhow::anyhow!(
            "--size is below the venue minimum for {}",
            symbol
        ));
    }
    let mark: f64 = client
        .get_symbol_market(&symbol)
        .await?
        .mark_price
        .parse()
        .map_err(|_| anyhow::anyhow!("venue returned an invalid mark price for {}", symbol))?;
    let price = canary_price(mark, price_offset_bps, info.price_tick_decimals)?;
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let client_order_id = format!("{}{}", CANARY_PREFIX, &run_id[..12]);
    let evidence = CanaryEvidence {
        symbol: &symbol,
        client_order_id: &client_order_id,
        quantity: format_decimals(quantity, info.qty_tick_decimals),
        price: format_decimals(price, info.price_tick_decimals),
    };
    evidence.emit_position(CanaryStage::PreflightVerified, None, 0.0);
    let stream = OrderResponseStream::from_endpoints(session_id, endpoints)?;
    let (commands, mut responses, _health, handle) = stream.connect().await?;
    notifier
        .lifecycle(
            "started",
            "WS command canary started: submitting one bounded post-only order",
            &symbol,
            false,
        )
        .await;

    let result = run_commands(
        &client,
        &commands,
        &mut responses,
        CanaryOrderPlan {
            symbol: &symbol,
            client_order_id: &client_order_id,
            quantity,
            price,
            qty_decimals: info.qty_tick_decimals,
            price_decimals: info.price_tick_decimals,
            timeout,
        },
        &evidence,
    )
    .await;
    handle.abort();
    match result {
        Ok(()) => {
            notifier
                .lifecycle(
                    "completed",
                    "WS command canary completed: acknowledged create/cancel, HTTP absence, and flat position verified",
                    &symbol,
                    true,
                )
                .await;
            Ok(())
        }
        Err(error) => {
            evidence.emit(CanaryStage::CleanupStarted, None, None, None);
            let message = match cleanup_current_order(&client, &symbol, &client_order_id, timeout)
                .await
            {
                Ok(order_id) => {
                    evidence.emit(
                        CanaryStage::CleanupVerified,
                        None,
                        order_id.as_deref(),
                        None,
                    );
                    format!("WS command canary failed safe: {error}")
                }
                Err(cleanup_error) => {
                    evidence.emit(CanaryStage::CleanupFailed, None, None, None);
                    format!(
                        "WS command canary failed safe: {error}; HTTP cleanup could not verify absence: {cleanup_error}"
                    )
                }
            };
            notifier.lifecycle("failed", &message, &symbol, true).await;
            Err(anyhow::Error::new(FailSafeShutdown { message }))
        }
    }
}

/// The single bounded order the canary places, cancels, and verifies gone.
///
/// Matches the request-struct shape the sibling maker flows use
/// (`CycleRequest` / `ReconcileRequest`): the plan travels as one value, the
/// I/O handles stay explicit parameters.
struct CanaryOrderPlan<'a> {
    symbol: &'a str,
    client_order_id: &'a str,
    quantity: f64,
    price: f64,
    qty_decimals: u32,
    price_decimals: u32,
    /// Bounds each response wait and each REST visibility check.
    timeout: Duration,
}

async fn run_commands(
    client: &StandXClient,
    commands: &OrderCommandSender,
    responses: &mut mpsc::Receiver<OrderResponse>,
    plan: CanaryOrderPlan<'_>,
    evidence: &CanaryEvidence<'_>,
) -> Result<()> {
    let CanaryOrderPlan {
        symbol,
        client_order_id,
        quantity,
        price,
        qty_decimals,
        price_decimals,
        timeout,
    } = plan;
    let create_request_id = commands
        .create_order(&CreateOrderParams {
            symbol: symbol.to_string(),
            cl_ord_id: Some(client_order_id.to_string()),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            quantity: format_decimals(quantity, qty_decimals),
            price: Some(format_decimals(price, price_decimals)),
            time_in_force: Some(TimeInForce::Alo),
            reduce_only: false,
            stop_price: None,
            sl_price: None,
            tp_price: None,
        })
        .await?;
    evidence.emit(
        CanaryStage::CreateSubmitted,
        Some(&create_request_id),
        None,
        None,
    );
    let create_response = await_response(responses, &create_request_id, timeout).await?;
    if !create_response.accepted() {
        evidence.emit(
            CanaryStage::CreateRejected,
            Some(&create_request_id),
            None,
            Some(&create_response),
        );
        return Err(anyhow::anyhow!(
            "venue rejected canary order:new: {}",
            create_response.message
        ));
    }
    evidence.emit(
        CanaryStage::CreateAccepted,
        Some(&create_request_id),
        None,
        Some(&create_response),
    );
    let order = wait_for_order(client, symbol, client_order_id, timeout).await?;
    evidence.emit(CanaryStage::OrderVisible, None, Some(&order.id), None);
    let cancel_request_id = commands.cancel_order(&order.id).await?;
    evidence.emit(
        CanaryStage::CancelSubmitted,
        Some(&cancel_request_id),
        Some(&order.id),
        None,
    );
    let cancel_response = await_response(responses, &cancel_request_id, timeout).await?;
    if !cancel_response.accepted() {
        evidence.emit(
            CanaryStage::CancelRejected,
            Some(&cancel_request_id),
            Some(&order.id),
            Some(&cancel_response),
        );
        return Err(anyhow::anyhow!(
            "venue rejected canary order:cancel: {}",
            cancel_response.message
        ));
    }
    evidence.emit(
        CanaryStage::CancelAccepted,
        Some(&cancel_request_id),
        Some(&order.id),
        Some(&cancel_response),
    );
    wait_until_absent(client, symbol, client_order_id, timeout).await?;
    evidence.emit(CanaryStage::AbsenceVerified, None, Some(&order.id), None);
    let position = position_for_symbol(&client.get_positions(Some(symbol)).await?, symbol)?;
    if position != 0.0 {
        evidence.emit_position(CanaryStage::PositionMismatch, Some(&order.id), position);
        return Err(anyhow::anyhow!(
            "canary post-check found non-zero {} position {position:+.8}",
            symbol
        ));
    }
    evidence.emit_position(CanaryStage::PositionVerified, Some(&order.id), position);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn price_is_bounded_below_mark_and_rounded_to_tick() {
        assert_eq!(canary_price(100.0, 100.0, 2).unwrap(), 99.0);
        assert!(canary_price(100.0, 0.0, 2).is_err());
        assert!(canary_price(0.0, 100.0, 2).is_err());
    }

    #[test]
    fn evidence_value_has_stable_correlation_fields() {
        let evidence = CanaryEvidence {
            symbol: "BTC-USD",
            client_order_id: "sxmk-canary-test",
            quantity: "0.001".to_string(),
            price: "99.50".to_string(),
        };
        let response = OrderResponse {
            code: 0,
            message: "accepted".to_string(),
            request_id: Some("request-1".to_string()),
        };

        assert_eq!(
            evidence.value_at(
                "2026-07-14T00:00:00.000Z",
                CanaryStage::CancelAccepted,
                Some("request-1"),
                Some("42"),
                Some(&response),
                None,
            ),
            json!({
                "ts": "2026-07-14T00:00:00.000Z",
                "action": "ws_command_canary",
                "event": "cancel_accepted",
                "symbol": "BTC-USD",
                "client_order_id": "sxmk-canary-test",
                "request_id": "request-1",
                "order_id": "42",
                "response_code": 0,
                "response_message": "accepted",
                "quantity": "0.001",
                "price": "99.50",
                "position": null,
            })
        );
    }

    #[tokio::test]
    async fn await_response_accepts_only_the_expected_request_id() {
        let (tx, mut responses) = mpsc::channel(1);
        tx.send(OrderResponse {
            code: 0,
            message: "accepted".to_string(),
            request_id: Some("request-1".to_string()),
        })
        .await
        .unwrap();

        let response = await_response(&mut responses, "request-1", Duration::from_millis(100))
            .await
            .unwrap();
        assert!(response.accepted());

        let (tx, mut responses) = mpsc::channel(1);
        tx.send(OrderResponse {
            code: 0,
            message: "accepted".to_string(),
            request_id: Some("other-request".to_string()),
        })
        .await
        .unwrap();

        let error = await_response(&mut responses, "request-1", Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("uncorrelated"));
    }

    #[tokio::test]
    async fn await_response_fails_when_stream_closes_or_times_out() {
        let (tx, mut responses) = mpsc::channel(1);
        drop(tx);
        let error = await_response(&mut responses, "request-1", Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("stream closed"));

        let (_tx, mut responses) = mpsc::channel(1);
        let error = await_response(&mut responses, "request-1", Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
    }
}
