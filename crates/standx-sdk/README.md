# standx-sdk

> Rust SDK for the StandX perpetual DEX — REST client, WebSocket streams, data
> models, and Ed25519 request signing.

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](../../LICENSE)

This is the library that powers the [`standx` CLI](../../README.md). If your
agent can shell out, use the CLI. If you are writing a Rust bot and want typed
models, streams, and signing without a subprocess, use this crate directly.

**Presentation-free by design**: no table/TUI/formatting dependencies, nothing
written to stdout, and no reading of global config. Table rendering for the core
models is behind the optional `tabled` feature, which only the CLI enables.
WebSocket debug tracing is opt-in (`StandXWebSocket::new_with_verbose(true)` /
`without_auth_with_verbose`) and goes to stderr.

**Stability**: pre-1.0 (`0.1.0`) — the API can change between releases. MSRV 1.75.

---

## Install

Not published to crates.io yet — depend on it by git:

```toml
[dependencies]
standx-sdk = { git = "https://github.com/wjllance/standx-cli" }
tokio = { version = "1", features = ["full"] }
```

Or, in a local checkout of this workspace:

```toml
standx-sdk = { path = "crates/standx-sdk" }
```

---

## Quick Start

Public market data needs no credentials:

```rust
use standx_sdk::client::StandXClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = StandXClient::new()?;

    let ticker = client.get_symbol_market("BTC-USD").await?;
    println!("BTC mark: {}", ticker.mark_price);

    let book = client.get_depth("BTC-USD", Some(10)).await?;
    println!("best bid/ask: {:?} / {:?}", book.best_bid(), book.best_ask());

    Ok(())
}
```

---

## What's in the box

### `client` — REST

`StandXClient` covers public and authenticated REST in one type. Auth headers
are built per request and signed when a private key is present.

| Area | Methods |
|------|---------|
| **Market** | `get_symbol_info`, `get_symbol_market`, `get_symbol_price`, `get_depth`, `get_recent_trades`, `get_kline`, `get_funding_rate`, `get_block_trades`, `health_check` |
| **Account** | `get_balance`, `get_positions`, `get_open_orders`, `get_order`, `get_order_history`, `get_user_trades`, `get_funding_history` |
| **Orders** | `create_order`, `cancel_order`, `cancel_orders`, `cancel_all_orders` |
| **Risk config** | `get_position_config`, `change_leverage`, `change_margin_mode`, `transfer_margin` |

```rust
use standx_sdk::client::order::CreateOrderParams;
use standx_sdk::client::StandXClient;
use standx_sdk::models::{OrderSide, OrderType, TimeInForce};

let client = StandXClient::new()?;

let order = client
    .create_order(CreateOrderParams {
        symbol: "BTC-USD".into(),
        side: OrderSide::Buy,
        order_type: OrderType::Limit,
        quantity: "0.001".into(),
        price: Some("64000".into()),
        // Add-liquidity-only: rejected rather than taking. This is the
        // maker-safe TIF; `post_only` is spelled `Alo` on this venue.
        time_in_force: Some(TimeInForce::Alo),
        // Client-generated correlation ID — how a bot recognises its own
        // orders after a reconnect.
        cl_ord_id: Some("mybot-0001".into()),
        ..Default::default()
    })
    .await?;

println!("order id: {}", order.id);
```

### `auth` — credentials and signing

- `Credentials` — load from env (`STANDX_JWT` / `STANDX_PRIVATE_KEY`) or the
  on-disk credential store, with JWT expiry inspection (`is_expired`,
  `remaining_seconds`, `expires_at_string`).
- `StandXSigner` — Ed25519 signing from a Base58 private key
  (`from_base58`, `sign_request`, `sign_request_now`, `pubkey_hex`).

The JWT alone is enough to read account state; trading calls additionally
require the private key.

### `websocket` — public market streams

```rust
use standx_sdk::websocket::{StandXWebSocket, WsMessage};

let ws = StandXWebSocket::without_auth()?;
let mut rx = ws.connect().await?;
ws.subscribe("price", Some("BTC-USD")).await?;

while let Some(msg) = rx.recv().await {
    match msg {
        WsMessage::Price(update) => println!("{} seq={:?}", update.data.mark_price, update.seq),
        WsMessage::Disconnected => break,
        _ => {}
    }
}
```

Channels: `price`, `depth_book`, `public_trade`, `kline`. Messages arrive as a
typed `WsMessage`; public-market payloads are wrapped in `WsMarketUpdate<T>`,
which carries the venue's sequence and timestamp alongside the data — so a
consumer can decide whether two independently-published channels form one
coherent snapshot instead of assuming it. `connect_managed` returns the reader
task handle for callers that own the lifecycle.

### `account_stream` — authenticated user streams

`AccountStream` delivers `AccountEvent` values over the `order`, `position`,
`trade`, and `balance` channels, with `AccountStreamHealth` exposing per-channel
sequence numbers, an epoch, and an explicit failure reason. Gap detection is the
point: a bot that reconciles against a stream needs to know when the stream
stopped being trustworthy.

### `order_response` — WebSocket order commands

`OrderResponseStream` places and cancels orders over the authenticated
WS command channel instead of REST, for latency-sensitive callers. Commands can
be `prepare`d to obtain the `request_id` before sending, so an in-flight order
is still identifiable if the response never arrives. `OrderResponseHealth`
reports session liveness.

### `models` and `error`

Every wire type is `serde`-serializable: `MarketData`, `PriceData`, `OrderBook`,
`Trade`, `Kline`, `FundingRate`, `Order`, `Position`, `Balance`, plus the
`OrderSide` / `OrderType` / `TimeInForce` / `OrderStatus` enums.

`Error` is a `thiserror` enum with retryability baked in — transport failures
and `RateLimitExceeded { retry_after }` are classified as retryable, so callers
can branch on the variant rather than on a message string.

---

## Feature flags

| Flag | Default | Effect |
|------|---------|--------|
| `tabled` | off | Implements `tabled::Tabled` for core models. Enabled by the CLI; SDK consumers otherwise carry no presentation dependencies. |

---

## Testing

Unit tests cover models, signing, credential handling, error classification and
the REST paths. No network required — REST is exercised against `mockito`:

```bash
cargo test -p standx-sdk
```

---

## Related

- [`standx` CLI](../../README.md) — the agent-facing binary built on this crate
- [`standx-maker`](../standx-maker/README.md) — deterministic market-making
  strategy and risk engine, also built on this crate
- [API docs](../../API_DOCUMENTATION.md) — the underlying StandX HTTP/WS API

---

## License

MIT OR Apache-2.0
