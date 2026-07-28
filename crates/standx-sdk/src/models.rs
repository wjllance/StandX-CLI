use serde::{Deserialize, Deserializer, Serialize};

/// Helper to deserialize string or number to string
fn string_or_number_to_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNumber {
        String(String),
        Number(serde_json::Number),
    }

    match StringOrNumber::deserialize(deserializer)? {
        StringOrNumber::String(s) => Ok(s),
        StringOrNumber::Number(n) => Ok(n.to_string()),
    }
}

/// Helper to deserialize OrderSide from string
fn deserialize_order_side<'de, D>(deserializer: D) -> Result<OrderSide, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    match s.to_lowercase().as_str() {
        "buy" => Ok(OrderSide::Buy),
        "sell" => Ok(OrderSide::Sell),
        _ => Err(serde::de::Error::custom(format!(
            "unknown order side: {}",
            s
        ))),
    }
}

/// Helper to deserialize optional OrderSide from string
pub(crate) fn deserialize_order_side_optional<'de, D>(
    deserializer: D,
) -> Result<Option<OrderSide>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    match s.to_lowercase().as_str() {
        "buy" | "long" => Ok(Some(OrderSide::Buy)),
        "sell" | "short" => Ok(Some(OrderSide::Sell)),
        _ => Ok(None),
    }
}

/// Helper to deserialize OrderType from string
fn deserialize_order_type<'de, D>(deserializer: D) -> Result<OrderType, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    match s.to_lowercase().as_str() {
        "limit" => Ok(OrderType::Limit),
        "market" => Ok(OrderType::Market),
        _ => Err(serde::de::Error::custom(format!(
            "unknown order type: {}",
            s
        ))),
    }
}

/// Trading symbol information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SymbolInfo {
    pub symbol: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub base_decimals: u32,
    pub price_tick_decimals: u32,
    pub qty_tick_decimals: u32,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub min_order_qty: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub def_leverage: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub max_leverage: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub maker_fee: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub taker_fee: String,
    pub status: String,
}

/// Market data for a symbol
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MarketData {
    pub symbol: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub mark_price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub index_price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub last_price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub volume_24h: String,
    #[serde(
        rename = "high_price_24h",
        deserialize_with = "string_or_number_to_string"
    )]
    pub high_24h: String,
    #[serde(
        rename = "low_price_24h",
        deserialize_with = "string_or_number_to_string"
    )]
    pub low_24h: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub funding_rate: String,
    /// Optional 24h change percentage from API, when available
    #[serde(
        default,
        alias = "price_change_percent_24h",
        alias = "price_change_24h",
        alias = "change_24h",
        deserialize_with = "string_or_number_to_string"
    )]
    pub change_24h_percent: String,
    pub next_funding_time: String,
}

/// Price data for a symbol
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PriceData {
    pub symbol: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub mark_price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub index_price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub last_price: String,
    #[serde(alias = "time")]
    pub timestamp: String,
}

/// Kline/candlestick data
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KlineData {
    pub symbol: Option<String>,
    pub interval: Option<String>,
    pub time: i64,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub open: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub high: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub low: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub close: String,
    pub volume: f64,
    pub volume_quote: f64,
}

/// Order book level
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrderBookLevel {
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub qty: String,
}

/// Order book depth
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OrderBook {
    #[serde(default)]
    pub symbol: String,
    pub bids: Vec<[String; 2]>,
    pub asks: Vec<[String; 2]>,
    #[serde(default)]
    pub timestamp: String,
}

impl OrderBook {
    /// Get best bid price
    pub fn best_bid(&self) -> Option<&str> {
        self.bids
            .iter()
            .filter_map(|level| {
                let price = level[0].parse::<f64>().ok()?;
                (price.is_finite() && price > 0.0).then_some((price, level[0].as_str()))
            })
            .max_by(|(left, _), (right, _)| left.total_cmp(right))
            .map(|(_, price)| price)
    }

    /// Get best ask price
    pub fn best_ask(&self) -> Option<&str> {
        self.asks
            .iter()
            .filter_map(|level| {
                let price = level[0].parse::<f64>().ok()?;
                (price.is_finite() && price > 0.0).then_some((price, level[0].as_str()))
            })
            .min_by(|(left, _), (right, _)| left.total_cmp(right))
            .map(|(_, price)| price)
    }

    /// Get spread between best bid and ask
    /// Returns format: "$X.XX (Y.YY%)"
    pub fn spread(&self) -> Option<String> {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => {
                if let (Ok(b), Ok(a)) = (bid.parse::<f64>(), ask.parse::<f64>()) {
                    let spread_value = a - b;
                    let spread_percent = if a > 0.0 {
                        (spread_value / a) * 100.0
                    } else {
                        0.0
                    };
                    Some(format!("${:.2} ({:.2}%)", spread_value, spread_percent))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// Recent trade
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Trade {
    #[serde(default)]
    pub id: u64,
    #[serde(
        default,
        alias = "created_at",
        deserialize_with = "string_or_number_to_string"
    )]
    pub time: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub qty: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub side: Option<String>,
    #[serde(default, rename = "is_taker")]
    pub is_buyer_taker: bool,
    #[serde(default)]
    pub fee_asset: Option<String>,
    #[serde(default)]
    pub fee_qty: Option<String>,
    #[serde(default)]
    pub pnl: Option<String>,
    #[serde(default, rename = "order_id")]
    pub order_id: Option<u64>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

/// Kline/candlestick data
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Kline {
    pub time: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub open: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub high: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub low: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub close: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub volume: String,
}

/// Kline API response wrapper
#[derive(Debug, Clone, Deserialize)]
pub struct KlineResponse {
    pub s: String,
    #[serde(default)]
    pub t: Vec<i64>,
    #[serde(default)]
    pub o: Vec<f64>,
    #[serde(default)]
    pub h: Vec<f64>,
    #[serde(default)]
    pub l: Vec<f64>,
    #[serde(default)]
    pub c: Vec<f64>,
    #[serde(default)]
    pub v: Vec<f64>,
}

impl KlineResponse {
    pub fn to_klines(self) -> Vec<Kline> {
        let mut klines = Vec::new();
        for i in 0..self.t.len() {
            klines.push(Kline {
                time: self.t[i].to_string(),
                open: self.o[i].to_string(),
                high: self.h[i].to_string(),
                low: self.l[i].to_string(),
                close: self.c[i].to_string(),
                volume: self.v[i].to_string(),
            });
        }
        klines
    }
}

/// Funding rate information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FundingRate {
    pub id: i64,
    pub symbol: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub funding_rate: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub mark_price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub index_price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub premium: String,
    pub time: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One row of the authenticated funding payment/receipt history
/// (`GET /api/query_funding_history`).
///
/// `qty` is the signed cashflow in `asset`: **negative is paid, positive is
/// received** (verified against live data on 2026-07-28), which matches the
/// sign convention of the maker performance ledger's funding input.
///
/// Two deliberate deviations from the published schema, both observed on live
/// responses:
/// - the documented `transact_time` field is **not returned**, so `created_at`
///   is the event time (kept optional here in case it appears later);
/// - the endpoint returns a **bare JSON array**, not the `{code, message,
///   result}` envelope that `query_trades` uses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FundingHistoryEntry {
    /// Monotonic row id; usable as the `last_id` pagination cursor.
    pub id: i64,
    pub user: String,
    /// Settlement asset of the cashflow (e.g. `DUSD` for a `-USD` symbol).
    pub asset: String,
    pub symbol: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub qty: String,
    /// Row kind; funding cashflows are `funding_fee`. Callers must filter
    /// rather than assume this endpoint only ever returns one kind.
    pub txn_type: String,
    pub created_at: String,
    pub updated_at: String,
    /// Documented but absent from live responses; prefer `created_at`.
    #[serde(default)]
    pub transact_time: Option<String>,
}

/// Block trade taker info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakerInfo {
    pub address: String,
    #[serde(default)]
    pub side: Option<String>,
}

/// Block trade information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockTrade {
    pub id: i64,
    pub symbol: String,
    pub side: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub qty: String,
    #[serde(
        rename = "blockStatus",
        deserialize_with = "string_or_number_to_string"
    )]
    pub block_status: String,
    #[serde(
        rename = "matchPolicy",
        deserialize_with = "string_or_number_to_string"
    )]
    pub match_policy: String,
    #[serde(
        rename = "makerAddress",
        deserialize_with = "string_or_number_to_string"
    )]
    pub maker_address: String,
    #[serde(rename = "takerInfoList", default)]
    pub taker_info_list: Vec<TakerInfo>,
    #[serde(rename = "expireTime")]
    pub expire_time: i64,
}

/// Order side
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OrderSide {
    Buy,
    Sell,
}

/// Order type
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    Limit,
    Market,
}

/// Time in force
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TimeInForce {
    Gtc, // Good Till Cancel
    Ioc, // Immediate or Cancel
    Fok, // Fill or Kill
    Alo, // Add Liquidity Only (post-only: rejected instead of taking)
}

/// Order status
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OrderStatus {
    #[serde(rename = "new")]
    New,
    #[serde(rename = "partially_filled")]
    PartiallyFilled,
    #[serde(rename = "filled")]
    Filled,
    #[serde(rename = "canceled")]
    Canceled,
    #[serde(rename = "rejected")]
    Rejected,
    #[serde(rename = "expired")]
    Expired,
    #[serde(rename = "open")]
    Open,
    #[serde(rename = "untriggered")]
    Untriggered,
}

/// Position side
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PositionSide {
    Long,
    Short,
}

/// Order request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub symbol: String,
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: OrderType,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub quantity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_in_force: Option<TimeInForce>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<String>,
}

/// Order response
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Order {
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub id: String,
    #[serde(default)]
    pub cl_ord_id: Option<String>,
    pub symbol: String,
    #[serde(deserialize_with = "deserialize_order_side")]
    pub side: OrderSide,
    #[serde(rename = "order_type", deserialize_with = "deserialize_order_type")]
    pub order_type: OrderType,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub qty: String,
    #[serde(deserialize_with = "string_or_number_to_string", default)]
    pub fill_qty: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub price: String,
    pub status: OrderStatus,
    pub created_at: String,
    pub updated_at: String,
}

#[cfg(feature = "tabled")]
impl tabled::Tabled for Order {
    const LENGTH: usize = 100;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.id.clone().into(),
            self.symbol.clone().into(),
            format!("{:?}", self.side).into(),
            format!("{:?}", self.order_type).into(),
            self.qty.clone().into(),
            self.fill_qty.clone().into(),
            self.price.clone().into(),
            format!("{:?}", self.status).into(),
            self.created_at
                .split('T')
                .next()
                .unwrap_or(&self.created_at)
                .into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Order ID".into(),
            "Symbol".into(),
            "Side".into(),
            "Type".into(),
            "Quantity".into(),
            "Filled".into(),
            "Price".into(),
            "Status".into(),
            "Created".into(),
        ]
    }
}

/// Position information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Position {
    pub id: i64,
    pub symbol: String,
    /// Position side (Buy/Long or Sell/Short)
    #[serde(default, deserialize_with = "deserialize_order_side_optional")]
    pub side: Option<OrderSide>,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub qty: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub entry_price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub entry_value: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub holding_margin: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub initial_margin: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub leverage: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub mark_price: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub margin_asset: String,
    pub margin_mode: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub position_value: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub realized_pnl: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub required_margin: String,
    pub status: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub upnl: String,
    /// Return on Equity (ROE) percentage - calculated as upnl/initial_margin * 100
    #[serde(default, skip_serializing)]
    pub roe: Option<String>,
    pub time: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liq_price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mmr: Option<String>,
    pub user: String,
}

#[cfg(feature = "tabled")]
impl tabled::Tabled for Position {
    const LENGTH: usize = 100;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.symbol.clone().into(),
            self.qty.clone().into(),
            self.entry_price.clone().into(),
            self.mark_price.clone().into(),
            self.liq_price.clone().unwrap_or_default().into(),
            self.leverage.clone().into(),
            self.margin_mode.clone().into(),
            self.upnl.clone().into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Symbol".into(),
            "Qty".into(),
            "Entry Price".into(),
            "Mark Price".into(),
            "Liq Price".into(),
            "Leverage".into(),
            "Margin Mode".into(),
            "Unrealized PnL".into(),
        ]
    }
}

/// Position configuration (leverage settings)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PositionConfig {
    pub symbol: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub leverage: String,
    #[serde(deserialize_with = "string_or_number_to_string", default)]
    pub max_leverage: String,
    #[serde(deserialize_with = "string_or_number_to_string", default)]
    pub def_leverage: String,
    #[serde(default)]
    pub margin_mode: String,
}

#[cfg(feature = "tabled")]
impl tabled::Tabled for PositionConfig {
    const LENGTH: usize = 4;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.symbol.clone().into(),
            self.leverage.clone().into(),
            self.max_leverage.clone().into(),
            self.def_leverage.clone().into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Symbol".into(),
            "Current Leverage".into(),
            "Max Leverage".into(),
            "Default Leverage".into(),
        ]
    }
}

/// Account balance
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Balance {
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub balance: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub cross_available: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub cross_balance: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub cross_margin: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub cross_upnl: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub equity: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub isolated_balance: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub isolated_upnl: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub locked: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub pnl_24h: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub pnl_freeze: String,
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub upnl: String,
}

#[cfg(feature = "tabled")]
impl tabled::Tabled for Balance {
    const LENGTH: usize = 100;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.balance.clone().into(),
            self.cross_available.clone().into(),
            self.equity.clone().into(),
            self.locked.clone().into(),
            self.upnl.clone().into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Balance".into(),
            "Available".into(),
            "Equity".into(),
            "Locked".into(),
            "Unrealized PnL".into(),
        ]
    }
}

/// API response wrapper
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiResponse<T> {
    pub code: i32,
    pub message: String,
    pub data: T,
}

/// Health check response
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthStatus {
    pub status: String,
    #[serde(default)]
    pub version: String,
}

/// Dashboard snapshot - unified view of account, positions, orders, and market data
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DashboardSnapshot {
    /// Snapshot timestamp (ISO 8601)
    pub timestamp: String,
    /// Account balance information
    pub account: Option<Balance>,
    /// Open positions
    pub positions: Vec<Position>,
    /// Sum of realized PnL across all queried positions
    #[serde(default, deserialize_with = "string_or_number_to_string")]
    pub total_realized_pnl: String,
    /// Open orders
    pub orders: Vec<Order>,
    /// Market data for tracked symbols
    pub market: Vec<MarketData>,
    /// Recent trades
    pub trades: Vec<Trade>,
    /// Order book for first symbol
    pub order_book: Option<OrderBook>,
}

/// Portfolio snapshot - portfolio summary and performance
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PortfolioSnapshot {
    /// Snapshot timestamp (ISO 8601)
    pub timestamp: String,
    /// Total portfolio value in USD
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub total_value_usd: String,
    /// PnL in the last 24 hours
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub total_pnl_24h: String,
    /// Total realized PnL (unrealized)
    #[serde(deserialize_with = "string_or_number_to_string")]
    pub total_pnl_realized: String,
    /// Open positions
    pub positions: Vec<Position>,
}

/// Format symbol info for display
#[cfg(feature = "tabled")]
impl tabled::Tabled for SymbolInfo {
    const LENGTH: usize = 100;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.symbol.clone().into(),
            self.base_asset.clone().into(),
            self.quote_asset.clone().into(),
            self.status.clone().into(),
            format!("{}x", self.max_leverage).into(),
            self.maker_fee.clone().into(),
            self.taker_fee.clone().into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Symbol".into(),
            "Base".into(),
            "Quote".into(),
            "Status".into(),
            "Max Lev".into(),
            "Maker Fee".into(),
            "Taker Fee".into(),
        ]
    }
}

/// Format market data for display
#[cfg(feature = "tabled")]
impl tabled::Tabled for MarketData {
    const LENGTH: usize = 100;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.symbol.clone().into(),
            self.mark_price.clone().into(),
            self.index_price.clone().into(),
            self.last_price.clone().into(),
            self.volume_24h.clone().into(),
            self.high_24h.clone().into(),
            self.low_24h.clone().into(),
            self.funding_rate.clone().into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Symbol".into(),
            "Mark Price".into(),
            "Index Price".into(),
            "Last Price".into(),
            "Volume 24h".into(),
            "High 24h".into(),
            "Low 24h".into(),
            "Funding Rate".into(),
        ]
    }
}

/// Format trade for display
#[cfg(feature = "tabled")]
impl tabled::Tabled for Trade {
    const LENGTH: usize = 100;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.time.split('.').next().unwrap_or(&self.time).into(),
            self.price.clone().into(),
            self.qty.clone().into(),
            if self.is_buyer_taker {
                "Buy".into()
            } else {
                "Sell".into()
            },
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Time".into(),
            "Price".into(),
            "Quantity".into(),
            "Side".into(),
        ]
    }
}

/// Format funding rate for display
#[cfg(feature = "tabled")]
impl tabled::Tabled for FundingRate {
    const LENGTH: usize = 6;

    fn fields(&self) -> Vec<std::borrow::Cow<'_, str>> {
        vec![
            self.time.split('T').next().unwrap_or(&self.time).into(),
            self.time
                .split('T')
                .nth(1)
                .unwrap_or("")
                .split('.')
                .next()
                .unwrap_or("")
                .into(),
            self.funding_rate.clone().into(),
            self.mark_price.clone().into(),
            self.index_price.clone().into(),
            self.premium.clone().into(),
        ]
    }

    fn headers() -> Vec<std::borrow::Cow<'static, str>> {
        vec![
            "Date".into(),
            "Time".into(),
            "Funding Rate".into(),
            "Mark Price".into(),
            "Index Price".into(),
            "Premium".into(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_symbol_info_deserialization() {
        let json = r#"{
            "symbol": "BTC-USD",
            "base_asset": "BTC",
            "quote_asset": "DUSD",
            "base_decimals": 9,
            "price_tick_decimals": 2,
            "qty_tick_decimals": 4,
            "min_order_qty": "0.0001",
            "def_leverage": "10",
            "max_leverage": "40",
            "maker_fee": "0.0001",
            "taker_fee": "0.0004",
            "status": "trading"
        }"#;

        let info: SymbolInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.symbol, "BTC-USD");
        assert_eq!(info.max_leverage, "40");
    }

    #[test]
    fn test_symbol_info_with_numbers() {
        // API sometimes returns numbers instead of strings
        let json = r#"{
            "symbol": "BTC-USD",
            "base_asset": "BTC",
            "quote_asset": "DUSD",
            "base_decimals": 9,
            "price_tick_decimals": 2,
            "qty_tick_decimals": 4,
            "min_order_qty": 0.0001,
            "def_leverage": 10,
            "max_leverage": 40,
            "maker_fee": 0.0001,
            "taker_fee": 0.0004,
            "status": "trading"
        }"#;

        let info: SymbolInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.min_order_qty, "0.0001");
        assert_eq!(info.max_leverage, "40");
    }

    #[test]
    fn test_market_data_with_floats() {
        let json = r#"{
            "symbol": "BTC-USD",
            "mark_price": 8739.106200000342,
            "index_price": 8738.5,
            "last_price": 8740.0,
            "volume_24h": "1000000",
            "high_price_24h": 9000.0,
            "low_price_24h": 8500.0,
            "funding_rate": "0.0001",
            "next_funding_time": "2026-01-01T00:00:00Z"
        }"#;

        let data: MarketData = serde_json::from_str(json).unwrap();
        assert_eq!(data.mark_price, "8739.106200000342");
        assert_eq!(data.index_price, "8738.5");
    }

    #[test]
    fn test_order_book_spread() {
        let book = OrderBook {
            symbol: "BTC-USD".to_string(),
            bids: vec![
                ["68000".to_string(), "1.0".to_string()],
                ["67900".to_string(), "2.0".to_string()],
            ],
            asks: vec![
                ["68100".to_string(), "0.5".to_string()],
                ["68200".to_string(), "1.0".to_string()],
            ],
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };

        assert_eq!(book.best_bid(), Some("68000"));
        assert_eq!(book.best_ask(), Some("68100"));
        assert_eq!(book.spread(), Some("$100.00 (0.15%)".to_string()));
    }

    #[test]
    fn test_order_book_sorting() {
        // Server might return unsorted data
        let book = OrderBook {
            symbol: "BTC-USD".to_string(),
            bids: vec![
                ["67900".to_string(), "2.0".to_string()],
                ["not-a-price".to_string(), "9.0".to_string()],
                ["68000".to_string(), "1.0".to_string()],
            ],
            asks: vec![
                ["68200".to_string(), "1.0".to_string()],
                ["NaN".to_string(), "9.0".to_string()],
                ["68100".to_string(), "0.5".to_string()],
            ],
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        };

        assert_eq!(book.best_bid(), Some("68000"));
        assert_eq!(book.best_ask(), Some("68100"));
    }

    #[test]
    fn test_position_full_deserialization() {
        let json = r#"[{
            "id": 80374,
            "symbol": "BTC-USD",
            "qty": "0.5",
            "entry_price": "62000",
            "entry_value": "31000",
            "holding_margin": "1550",
            "initial_margin": "1550",
            "leverage": "20",
            "mark_price": "67972.53",
            "margin_asset": "DUSD",
            "margin_mode": "isolated",
            "position_value": "33986.27",
            "realized_pnl": "0.062040",
            "required_margin": "1699.31",
            "status": "open",
            "upnl": "2986.27",
            "time": "2026-02-26T07:45:48.770053Z",
            "created_at": "2026-02-25T14:07:08.498140Z",
            "updated_at": "2026-02-25T17:31:29.932389Z",
            "liq_price": "60000",
            "mmr": "0.05",
            "user": "bsc_0x7ccEA090C8BCE0038c9407c9341baF3f6c714Fe2"
        }]"#;

        let positions: Vec<Position> = serde_json::from_str(json).unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].symbol, "BTC-USD");
        assert_eq!(positions[0].qty, "0.5");
    }

    #[test]
    fn test_position_optional_null_fields() {
        let json = r#"[{
            "id": 1,
            "symbol": "ETH-USD",
            "qty": "0",
            "entry_price": "0",
            "entry_value": "0",
            "holding_margin": "0",
            "initial_margin": "0",
            "leverage": "20",
            "mark_price": "3456.78",
            "margin_asset": "DUSD",
            "margin_mode": "isolated",
            "position_value": "0",
            "realized_pnl": "0",
            "required_margin": "0",
            "status": "open",
            "upnl": "0",
            "time": "2026-02-26T07:45:48Z",
            "created_at": "2026-02-25T14:07:08Z",
            "updated_at": "2026-02-25T17:31:29Z",
            "liq_price": null,
            "mmr": null,
            "user": "test_user"
        }]"#;

        let positions: Vec<Position> = serde_json::from_str(json).unwrap();
        assert!(positions[0].liq_price.is_none());
        assert!(positions[0].mmr.is_none());
    }

    #[test]
    fn test_position_zero_qty() {
        // 测试持仓为 0 的情况（无实际持仓）
        let json = r#"[{
            "id": 100,
            "symbol": "BTC-USD",
            "qty": "0",
            "entry_price": "0",
            "entry_value": "0",
            "holding_margin": "0",
            "initial_margin": "0",
            "leverage": "20",
            "mark_price": "65000",
            "margin_asset": "DUSD",
            "margin_mode": "isolated",
            "position_value": "0",
            "realized_pnl": "0",
            "required_margin": "0",
            "status": "open",
            "upnl": "0",
            "time": "2026-02-26T07:45:48Z",
            "created_at": "2026-02-25T14:07:08Z",
            "updated_at": "2026-02-25T17:31:29Z",
            "user": "test_user"
        }]"#;

        let positions: Vec<Position> = serde_json::from_str(json).unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].qty, "0");
        assert_eq!(positions[0].entry_price, "0");
        assert_eq!(positions[0].position_value, "0");
        assert_eq!(positions[0].upnl, "0");
        // 零持仓时应该没有强平价格
        assert!(positions[0].liq_price.is_none());
    }

    #[test]
    fn test_order_side_deserialization() {
        // Test deserialize_order_side with valid values
        let json = r#"{
            "id": "1",
            "symbol": "BTC-USD",
            "side": "buy",
            "order_type": "limit",
            "qty": "0.1",
            "fill_qty": "0",
            "price": "60000",
            "status": "open",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;
        let order: Order = serde_json::from_str(json).unwrap();
        assert_eq!(order.side, OrderSide::Buy);

        let json = r#"{
            "id": "2",
            "symbol": "BTC-USD",
            "side": "sell",
            "order_type": "limit",
            "qty": "0.1",
            "fill_qty": "0",
            "price": "60000",
            "status": "open",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;
        let order: Order = serde_json::from_str(json).unwrap();
        assert_eq!(order.side, OrderSide::Sell);

        // Test case insensitivity
        let json = r#"{
            "id": "3",
            "symbol": "BTC-USD",
            "side": "BUY",
            "order_type": "limit",
            "qty": "0.1",
            "fill_qty": "0",
            "price": "60000",
            "status": "open",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;
        let order: Order = serde_json::from_str(json).unwrap();
        assert_eq!(order.side, OrderSide::Buy);

        let json = r#"{
            "id": "4",
            "symbol": "BTC-USD",
            "side": "Sell",
            "order_type": "limit",
            "qty": "0.1",
            "fill_qty": "0",
            "price": "60000",
            "status": "open",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;
        let order: Order = serde_json::from_str(json).unwrap();
        assert_eq!(order.side, OrderSide::Sell);
    }

    #[test]
    fn test_order_side_deserialization_invalid() {
        // Test invalid order side
        let json = r#"{"side": "invalid"}"#;
        let result: Result<Order, _> = serde_json::from_str(json);
        assert!(result.is_err());

        // Test empty string
        let json = r#"{"side": ""}"#;
        let result: Result<Order, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_order_type_deserialization() {
        // Test deserialize_order_type with valid values
        let json = r#"{
            "id": "1",
            "symbol": "BTC-USD",
            "side": "buy",
            "order_type": "limit",
            "qty": "0.1",
            "fill_qty": "0",
            "price": "60000",
            "status": "open",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;
        let order: Order = serde_json::from_str(json).unwrap();
        assert_eq!(order.order_type, OrderType::Limit);

        let json = r#"{
            "id": "2",
            "symbol": "BTC-USD",
            "side": "buy",
            "order_type": "market",
            "qty": "0.1",
            "fill_qty": "0",
            "price": "60000",
            "status": "open",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;
        let order: Order = serde_json::from_str(json).unwrap();
        assert_eq!(order.order_type, OrderType::Market);

        // Test case insensitivity
        let json = r#"{
            "id": "3",
            "symbol": "BTC-USD",
            "side": "buy",
            "order_type": "LIMIT",
            "qty": "0.1",
            "fill_qty": "0",
            "price": "60000",
            "status": "open",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;
        let order: Order = serde_json::from_str(json).unwrap();
        assert_eq!(order.order_type, OrderType::Limit);

        let json = r#"{
            "id": "4",
            "symbol": "BTC-USD",
            "side": "buy",
            "order_type": "Market",
            "qty": "0.1",
            "fill_qty": "0",
            "price": "60000",
            "status": "open",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;
        let order: Order = serde_json::from_str(json).unwrap();
        assert_eq!(order.order_type, OrderType::Market);
    }

    #[test]
    fn test_order_type_deserialization_invalid() {
        // Test invalid order type
        let json = r#"{"order_type": "stop_loss"}"#;
        let result: Result<Order, _> = serde_json::from_str(json);
        assert!(result.is_err());

        // Test empty string
        let json = r#"{"order_type": ""}"#;
        let result: Result<Order, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_order_status_untriggered() {
        // Test new Untriggered status
        let _json = r#"{"status": "untriggered"}"#;
        // We need to test this via Order since OrderStatus is not directly deserializable
        let order_json = r#"{
            "id": "123",
            "symbol": "BTC-USD",
            "side": "buy",
            "order_type": "limit",
            "qty": "0.1",
            "fill_qty": "0",
            "price": "60000",
            "status": "untriggered",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        }"#;
        let order: Order = serde_json::from_str(order_json).unwrap();
        assert_eq!(order.status, OrderStatus::Untriggered);
    }

    #[test]
    fn test_funding_rate_full_deserialization() {
        let json = r#"{
            "id": 12345,
            "symbol": "BTC-USD",
            "funding_rate": "0.00001250",
            "mark_price": "63127.37",
            "index_price": "63126.67",
            "premium": "0.00000100",
            "time": "2024-01-01T08:00:00Z",
            "created_at": "2024-01-01T07:59:59Z",
            "updated_at": "2024-01-01T08:00:00Z"
        }"#;

        let rate: FundingRate = serde_json::from_str(json).unwrap();
        assert_eq!(rate.id, 12345);
        assert_eq!(rate.symbol, "BTC-USD");
        assert_eq!(rate.funding_rate, "0.00001250");
    }
}
