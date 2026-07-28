//! Deterministic ownership and slot rules for a single maker run.
//!
//! These helpers deliberately operate on normalized values rather than venue
//! payloads. The CLI/SDK adapters are responsible for extracting a client
//! order ID, position quantity, or pending request from transport models.

use standx_sdk::models::OrderSide;

/// Prefix reserved for maker client-order IDs across all runs.
pub const MAKER_CL_ORD_ID_PREFIX: &str = "sxmk-";

/// A stable quote slot in the maker ladder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuoteSlot {
    pub side: OrderSide,
    pub level: u32,
}

/// Build the bounded client-order ID for a normal quote.
pub fn quote_client_order_id(
    run_order_prefix: &str,
    cycle: u64,
    side: OrderSide,
    level: u32,
) -> String {
    let side_code = match side {
        OrderSide::Buy => 'b',
        OrderSide::Sell => 's',
    };
    format!(
        "{run_order_prefix}q{:08x}{side_code}{level:x}",
        cycle as u32
    )
}

/// Build the bounded client-order ID for a reduce-only inventory exit.
pub fn exit_client_order_id(run_order_prefix: &str, cycle: u64) -> String {
    format!("{run_order_prefix}x{:08x}", cycle as u32)
}

/// Whether a client-order ID belongs to any maker run.
pub fn is_maker_client_order_id(client_order_id: Option<&str>) -> bool {
    client_order_id.is_some_and(|id| id.starts_with(MAKER_CL_ORD_ID_PREFIX))
}

/// Whether a client-order ID belongs to the active maker run.
pub fn is_current_run_client_order_id(
    client_order_id: Option<&str>,
    run_order_prefix: &str,
) -> bool {
    client_order_id.is_some_and(|id| id.starts_with(run_order_prefix))
}

/// Whether an adopted startup position is within the configured limit, with a
/// half quantity-tick tolerance for venue rounding.
pub fn position_within_limit(position: f64, max_position: f64, qty_decimals: u32) -> bool {
    let qty_tolerance = 10_f64.powi(-(qty_decimals as i32)) / 2.0;
    position.abs() <= max_position + qty_tolerance
}

/// Whether a venue's open quantity can safely adopt a just-submitted quote.
pub fn open_qty_adopts(open_qty: f64, placed_qty: f64) -> bool {
    open_qty > 0.0 && open_qty <= placed_qty * (1.0 + 1e-6)
}

/// Whether a submitted-but-not-yet-visible request covers a quote slot.
pub fn pending_covers_slot(
    pending: impl IntoIterator<Item = QuoteSlot>,
    side: OrderSide,
    level: u32,
) -> bool {
    pending
        .into_iter()
        .any(|slot| slot == QuoteSlot { side, level })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_bounded_order_ids_and_recognizes_their_ownership() {
        let prefix = "sxmk-abcdef123456-";
        let quote = quote_client_order_id(prefix, u64::MAX, OrderSide::Sell, u32::MAX);
        let exit = exit_client_order_id(prefix, u64::MAX);

        assert_eq!(quote, "sxmk-abcdef123456-qffffffffsffffffff");
        assert_eq!(exit, "sxmk-abcdef123456-xffffffff");
        assert!(is_maker_client_order_id(Some(&quote)));
        assert!(is_current_run_client_order_id(Some(&quote), prefix));
        assert!(!is_current_run_client_order_id(Some(&quote), "sxmk-other-"));
    }

    #[test]
    fn applies_position_and_pending_slot_tolerances() {
        // Half a qty tick of float slop is inside the limit; real excess is not.
        assert!(position_within_limit(0.800_05, 0.8, 3));
        assert!(!position_within_limit(0.800_6, 0.8, 3));
        // The limit is on the magnitude, so a short at the limit is inside it.
        assert!(position_within_limit(-0.8, 0.8, 3));
    }

    #[test]
    fn open_quantity_adopts_any_unfilled_remainder_but_not_a_stranger() {
        // Full remainder, a half-filled remainder, and a tiny one all adopt:
        // the maker placed this order and part of it is still working.
        assert!(open_qty_adopts(0.01, 0.01));
        assert!(open_qty_adopts(0.005, 0.01));
        assert!(open_qty_adopts(0.0001, 0.01));
        // Float slop just over the placed qty is still ours.
        assert!(open_qty_adopts(0.01 + 1e-9, 0.01));
        // Nothing open left to match.
        assert!(!open_qty_adopts(0.0, 0.01));
        // More than was ever placed belongs to someone else.
        assert!(!open_qty_adopts(0.02, 0.01));
    }

    #[test]
    fn pending_covers_only_its_own_side_and_level() {
        let pending = || {
            [QuoteSlot {
                side: OrderSide::Buy,
                level: 0,
            }]
        };
        assert!(pending_covers_slot(pending(), OrderSide::Buy, 0));
        assert!(!pending_covers_slot(pending(), OrderSide::Buy, 1));
        assert!(!pending_covers_slot(pending(), OrderSide::Sell, 0));
    }
}
