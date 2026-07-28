//! Command implementations
//!
//! One submodule per command domain; this file wires them together and
//! re-exports the public handler surface consumed by `main.rs`.

mod account;
mod auth;
mod block;
mod config;
mod dashboard;
mod lag_recorder;
mod leverage;
mod maker;
mod margin;
mod market;
mod order;
mod portfolio;
mod stream;
mod trade;
mod update;
mod util;

pub use account::handle_account;
pub use auth::handle_auth;
pub use block::handle_block;
pub use config::handle_config;
pub use dashboard::handle_dashboard;
pub use lag_recorder::handle_lag_recorder;
pub use leverage::handle_leverage;
pub use maker::{handle_maker, panic_webhook_body, FailSafeShutdown, FAIL_SAFE_EXIT_CODE};
pub use margin::handle_margin;
pub use market::handle_market;
pub use order::handle_order;
pub use portfolio::{handle_portfolio, PortfolioCommand};
pub use stream::handle_stream;
pub use trade::handle_trade;
pub use update::handle_update;
pub use util::parse_time_string;
