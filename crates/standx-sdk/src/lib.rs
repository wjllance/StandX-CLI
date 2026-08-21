//! StandX SDK
//!
//! Rust SDK for the StandX perpetual DEX: REST client, WebSocket streams,
//! data models, and Ed25519 request signing.
//!
//! This crate is designed for AI Agents and automated systems:
//! - Structured output by default
//! - Non-interactive design for automation
//! - Sub-100ms latency for real-time decisions
//!
//! ## Quick Start
//!
//! `no_run`: this example is compiled and type-checked on every `cargo test`,
//! but never executed. Running it would issue a real HTTP request to the
//! production endpoint, which made the workspace test gate depend on network
//! reachability — a transient DNS or connectivity blip failed `cargo test`
//! with no test actually broken, which is worse than useless in CI and has
//! already caused one misread gate result.
//!
//! ```rust,no_run
//! use standx_sdk::client::StandXClient;
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let client = StandXClient::new()?;
//!     let ticker = client.get_symbol_market("BTC-USD").await?;
//!     println!("BTC Price: {}", ticker.mark_price);
//!     Ok(())
//! }
//! ```
//!
//! ## Features
//!
//! - `tabled`: implements [`tabled::Tabled`] for core models so they can be
//!   rendered as tables (used by the `standx` CLI; off by default so SDK
//!   consumers carry no presentation dependencies).
//!
//! ## Design Principles
//!
//! 1. **Agent-First**: Every API is designed for machine consumption
//! 2. **Structured Everything**: All outputs are JSON-serializable
//! 3. **Zero-Config**: Works out of the box with environment variables
//! 4. **Composable**: Tools can be combined into complex workflows

#![allow(clippy::result_large_err)]

pub mod account_stream;
pub mod auth;
pub mod client;
pub mod endpoints;
pub mod error;
pub mod models;
pub mod order_response;
pub mod websocket;

pub use endpoints::StandXEndpoints;
pub use error::{Error, Result};
