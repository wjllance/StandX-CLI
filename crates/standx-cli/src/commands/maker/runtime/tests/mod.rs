use super::super::pipeline::OrderRequestKind;
use super::*;
use standx_maker::{
    OrderObservation, ProjectionPendingCancel, ProjectionPendingPlace, RequestTimeoutPhase,
};
use standx_sdk::account_stream::{OrderUpdate, PositionUpdate};
use standx_sdk::order_response::OrderResponseHealth;

fn pending_place(request_id: &str) -> ProjectionPendingPlace {
    ProjectionPendingPlace {
        request_id: request_id.to_string(),
        client_order_id: format!("cl-{request_id}"),
        side: OrderSide::Buy,
        price: 100.0,
        qty: 1.0,
        level: 0,
        ref_center: 100.0,
        cycle: 1,
    }
}

fn account_balance() -> standx_sdk::models::Balance {
    standx_sdk::models::Balance {
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

fn projection_with_pending(request_ids: &[&str]) -> MakerAccountProjection {
    let mut projection = MakerAccountProjection::new(1, "sxmk-test-", 0.0, 0.005, 0.00005);
    for request_id in request_ids {
        projection.apply(
            1,
            AccountProjectionEvent::PlaceSubmitted(pending_place(request_id)),
        );
    }
    projection
}

/// Apply one acknowledgement and return its correlation verdict. Keeps the
/// permutation tests reading as a sequence of observations rather than a wall of
/// repeated `apply_order_response` argument lists.
fn correlate(
    projection: &mut MakerAccountProjection,
    request_id: Option<&str>,
    code: i64,
) -> standx_maker::ResponseCorrelation {
    apply_order_response(
        order_response(request_id, code),
        projection,
        OutputFormat::Quiet,
        "BTC-USD",
        1,
        2,
    )
    .expect("acknowledgement must not be a cancel rejection")
}

fn order_response(request_id: Option<&str>, code: i64) -> OrderResponse {
    OrderResponse {
        code,
        message: String::new(),
        request_id: request_id.map(str::to_string),
    }
}

fn position_update(symbol: &str, side: Option<OrderSide>, qty: &str) -> PositionUpdate {
    PositionUpdate {
        seq: 0,
        id: 0,
        symbol: symbol.to_string(),
        side,
        qty: qty.to_string(),
        entry_price: String::new(),
        realized_pnl: String::new(),
        status: String::new(),
        updated_at: String::new(),
    }
}

mod account_events;
mod order_events;
mod recovery;
mod runtime_flow;
