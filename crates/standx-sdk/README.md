# standx-sdk

> Low-level Rust SDK for the StandX perpetual DEX: typed REST APIs, public and
> authenticated WebSocket streams, data models, and Ed25519 request signing.

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](../../LICENSE)

`standx-sdk` is the exchange integration crate behind the
[`standx` CLI](../../README.md). Use the CLI for shell-based automation. Use
this crate when a Rust application needs direct access to typed responses,
stream lifecycles, request signing, or custom transport supervision.

| Need | Use |
|------|-----|
| Shell commands, JSON output, and live-safety gates | [`standx` CLI](../../README.md) |
| StandX REST, WebSocket, auth, and wire models in Rust | `standx-sdk` |
| Deterministic market-making strategy and risk logic | [`standx-maker`](../standx-maker/README.md) |

## Status

- **Version:** `0.1.0`, pre-1.0; public APIs may change between releases.
- **MSRV:** Rust 1.75.
- **Distribution:** Git or workspace path dependency; not published to
  crates.io yet.
- **Output:** no stdout output or TUI dependency. Optional WebSocket diagnostics
  go to stderr only when verbose mode is enabled.

Public REST and market streams need no credentials. Authenticated constructors
and private REST calls load credentials from `STANDX_JWT` /
`STANDX_PRIVATE_KEY`, falling back to the credential store written by the CLI.

## Capabilities

| Module | Purpose |
|--------|---------|
| `client` | Public and authenticated REST requests |
| `websocket` | Public `price`, `depth_book`, `public_trade`, and `kline` streams |
| `account_stream` | Authenticated `order`, `position`, `trade`, and `balance` events with health tracking |
| `order_response` | Authenticated WebSocket order commands and correlated responses |
| `auth` | Credential loading, JWT expiry inspection, and Ed25519 signing |
| `models` / `error` | Typed API models and structured, classifiable errors |
| `endpoints` | Validated REST and WebSocket endpoint derivation |

## Installation

Add the SDK from Git:

```toml
[dependencies]
standx-sdk = { git = "https://github.com/wjllance/standx-cli" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

For reproducible applications, pin the Git dependency with Cargo's `rev`
option. In a local checkout of this workspace, use a path dependency:

```toml
[dependencies]
standx-sdk = { path = "crates/standx-sdk" }
```

## Quick start: public REST

Public market data works without credentials:

```rust
use standx_sdk::client::StandXClient;

#[tokio::main]
async fn main() -> standx_sdk::Result<()> {
    let client = StandXClient::new()?;

    let ticker = client.get_symbol_market("BTC-USD").await?;
    println!("BTC mark: {}", ticker.mark_price);

    let book = client.get_depth("BTC-USD", Some(10)).await?;
    println!("best bid/ask: {:?} / {:?}", book.best_bid(), book.best_ask());

    Ok(())
}
```

No-argument constructors use the production StandX endpoints.

## Authentication and trading

`Credentials` loads environment variables first, then the on-disk credential
store:

- `STANDX_JWT` authenticates account reads and user streams.
- `STANDX_PRIVATE_KEY` is the Base58 Ed25519 key used to sign trading requests.

It also exposes JWT lifetime helpers: `is_expired`, `remaining_seconds`, and
`expires_at_string`. `StandXSigner` provides `from_base58`, `sign_request`,
`sign_request_now`, and `pubkey_hex` for callers that need direct signing.

> **Live-order warning:** the REST and WebSocket order APIs submit real commands
> when configured with production endpoints and valid credentials. This SDK
> does not provide the CLI's confirmation and live-authorization gates.

## API guide

### REST client

`StandXClient` covers public and authenticated REST in one type. Auth headers
are built per request and include an Ed25519 signature when a private key is
available.

| Area | Methods |
|------|---------|
| **Market** | `get_symbol_info`, `get_symbol_market`, `get_symbol_price`, `get_depth`, `get_recent_trades`, `get_kline`, `get_funding_rate`, `get_block_trades`, `health_check` |
| **Account** | `get_balance`, `get_positions`, `get_open_orders`, `get_order`, `get_order_history`, `get_user_trades`, `get_funding_history` |
| **Orders** | `create_order`, `cancel_order`, `cancel_orders`, `cancel_all_orders` |
| **Risk config** | `get_position_config`, `change_leverage`, `change_margin_mode`, `transfer_margin` |

Authenticated order creation uses the same typed client:

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
        // Client-generated correlation ID for recognising the order after a
        // reconnect.
        cl_ord_id: Some("mybot-0001".into()),
        ..Default::default()
    })
    .await?;

println!("submission request id: {}", order.id);
```

The `Order.id` returned by `create_order` is the gateway `request_id`, not the
integer venue order ID accepted by `cancel_order`. Treat the REST response as a
submission acknowledgement. Before considering the order effective or trying
to cancel it, confirm it through the account order stream or locate its
authoritative venue ID in `get_open_orders` using the unique `cl_ord_id`.

### Public market WebSocket

Register subscriptions before connecting so the initial connection and every
reconnect carry the same subscription set:

```rust
use standx_sdk::websocket::{StandXWebSocket, WsMessage};

let ws = StandXWebSocket::without_auth()?;
ws.subscribe("price", Some("BTC-USD")).await?;
let mut rx = ws.connect().await?;

while let Some(message) = rx.recv().await {
    match message {
        WsMessage::Price(update) => {
            println!("{} seq={:?}", update.data.mark_price, update.seq);
        }
        WsMessage::Disconnected => break,
        _ => {}
    }
}
```

Public payloads arrive as typed `WsMessage` variants. `price` and `depth_book`
use `WsMarketUpdate<T>`, which keeps the venue sequence, venue timestamp, and
local monotonic receipt time. Consumers can therefore judge whether
independently published channels form a coherent snapshot.

Use `subscribe_with_interval` for `kline`. Use `connect_managed` when the caller
needs ownership of the reader task for shutdown or supervision. Verbose tracing
is opt-in through `new_with_verbose` or `without_auth_with_verbose` and writes
to stderr.

### Authenticated account stream

`AccountStream` delivers typed `AccountEvent` values for the `order`,
`position`, `trade`, and `balance` channels. `AccountStreamHealth` exposes the
connection epoch, per-channel sequence numbers, and an explicit failure reason
so consumers can fail closed and reconcile after gaps.

### WebSocket order commands

`OrderResponseStream` places and cancels orders over the authenticated command
channel. `connect` returns an `OrderCommandSender`, correlated response
receiver, `OrderResponseHealth`, and supervisor task.

Commands can be prepared before I/O so the request ID is registered before a
write begins. Keep socket delivery, gateway acknowledgement, and venue/account
effectiveness as separate lifecycle stages:

1. Call `prepare_create_order` or `prepare_cancel_order`.
2. Store `PreparedOrderCommand::request_id()` in the caller's ledger before I/O.
3. Call `send_prepared`; success means only that the frame reached the local
   WebSocket writer, so keep the request pending.
4. Correlate the `OrderResponse`, then require the matching account order event
   or an authoritative REST observation before treating placement or
   cancellation as effective. A code-zero gateway `accepted` response alone is
   not venue confirmation.
5. After cancellation, confirm a terminal account event or REST absence and
   reconcile the position. On timeout, disconnect, contradiction, or unknown
   effectiveness, freeze new placements, run bounded maker-order cleanup, and
   resume only after the order book and position reconcile.

`OrderResponseStream::new(session_id)?.with_verbose(true)` logs only raw
post-authentication inbound responses to stderr. Signed outbound commands and
authentication payloads are never logged.

### Custom endpoints

`StandXEndpoints` validates one root URL and derives matching REST, market and
account WebSocket, and order-response WebSocket addresses:

```rust
use standx_sdk::client::StandXClient;
use standx_sdk::websocket::StandXWebSocket;
use standx_sdk::StandXEndpoints;

let endpoints = StandXEndpoints::new("https://perps.example.com")?;
let client = StandXClient::from_endpoints(&endpoints)?;
let public_stream = StandXWebSocket::without_auth_from_endpoints(&endpoints)?;
```

Custom plaintext HTTP is accepted only for localhost or loopback test servers.
Invalid configuration returns an error rather than falling back to production.
For public market streams, use `StandXWebSocket::without_auth_from_endpoints`
as shown above. Public REST calls through `StandXClient` do not need credentials,
but account and order methods load and send the JWT to the configured REST host;
order requests may also include signatures. The authenticated
`StandXWebSocket::from_endpoints`, `AccountStream::from_endpoints`, and
`OrderResponseStream::from_endpoints` constructors load and send the JWT to the
configured stream host. Use custom endpoints for authenticated traffic only
when those hosts are trusted with the account credentials.

### Models and errors

REST data models and `Error` are `serde`-serializable. Core types include
`MarketData`, `PriceData`, `OrderBook`, `Trade`, `Kline`, `FundingRate`,
`Order`, `Position`, and `Balance`, plus the `OrderSide`, `OrderType`,
`TimeInForce`, and `OrderStatus` enums.

`Error::is_retryable()` classifies retryable HTTP/API failures, rate limits,
and WebSocket failures without requiring callers to parse display strings.
`Error::to_json()` provides a structured error payload for automation.

## Feature flags

| Flag | Default | Effect |
|------|---------|--------|
| `tabled` | off | Implements `tabled::Tabled` for core models. The CLI enables it; SDK-only consumers avoid the presentation dependency. |

## Testing

Tests cover models, signing, credential handling, error classification, REST
paths, and WebSocket lifecycle behavior. REST uses `mockito`; WebSocket tests
use local loopback servers. They do not require StandX credentials or an
external StandX connection.

From the workspace root:

```bash
cargo test -p standx-sdk --offline
```

## Related documentation

- [`standx` CLI](../../README.md) — command-line workflows and live-safety gates
- [`standx-maker`](../standx-maker/README.md) — deterministic market-making
  strategy and risk engine
- [StandX API reference](../../API_DOCUMENTATION.md) — underlying HTTP and
  WebSocket protocol

## License

MIT OR Apache-2.0
