# standx-maker

> Deterministic market-making strategy and risk engine for StandX.

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](../../LICENSE)

This crate is the decision layer behind `standx maker run`. It plans quotes,
tracks inventory and session PnL, and decides when to halt, exit, freeze, or
recover — but it never talks to the exchange itself.

## The contract

- **No I/O.** No network, no clock, no filesystem, no terminal. Every function
  takes plain values and returns decisions or typed effects.
- **Deterministic.** Same typed inputs → same outputs, so the whole strategy is
  replayable and unit-testable offline.
- **Depends only on `standx-sdk`**, and only for model types (`OrderSide` at the
  crate root). The CLI executes the effects this crate returns; the effects never
  execute themselves.

That boundary is enforced deliberately — see [AGENTS.md](../../AGENTS.md) for the
rules on what belongs here versus in `standx-cli` or `standx-sdk`.

## Modules

- **Strategy** — `inventory`, `volatility`, `risk`
- **Accounting** — `ledger`, `performance`, `account_projection`
- **Lifecycle** — `runtime`, `recovery`, `market_data`, `ownership`,
  `external_guard`, `latency`, `replay`

Crate-level docs in [`src/lib.rs`](src/lib.rs) explain the anti-flicker loop and
the numeric representation choice.

## Where to look next

- **Running the bot** (every flag, telemetry, live safety rails) →
  [docs/13-maker.md](../../docs/13-maker.md)
- **Live-mode unlock criteria** → [docs/14-maker-live-gate.md](../../docs/14-maker-live-gate.md)
- **Contribution boundary** → [AGENTS.md](../../AGENTS.md)
- **The transport layer this sits on** → [`standx-sdk`](../standx-sdk/README.md)

```bash
cargo test -p standx-maker
```

## License

MIT OR Apache-2.0
