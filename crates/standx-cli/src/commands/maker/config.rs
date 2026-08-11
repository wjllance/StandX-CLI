//! Non-sensitive maker strategy configuration.

use crate::cli::{AlertWebhookFormat, MakerRunFlags};
use crate::config::Config;
use anyhow::Result;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdaptiveSpreadTierFileConfig {
    pub enter_vol_bps: Option<f64>,
    pub exit_vol_bps: Option<f64>,
    pub spread_bps: f64,
    pub refresh_bps: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdaptiveSpreadFileConfig {
    pub enabled: Option<bool>,
    pub min_spread_bps: f64,
    pub max_spread_bps: f64,
    pub tiers: Vec<AdaptiveSpreadTierFileConfig>,
}

impl AdaptiveSpreadFileConfig {
    pub(super) fn into_domain(
        self,
        enabled_override: Option<bool>,
    ) -> standx_maker::AdaptiveSpreadConfig {
        standx_maker::AdaptiveSpreadConfig {
            enabled: enabled_override.or(self.enabled).unwrap_or(false),
            min_spread_bps: self.min_spread_bps,
            max_spread_bps: self.max_spread_bps,
            tiers: self
                .tiers
                .into_iter()
                .map(|tier| standx_maker::SpreadTier {
                    enter_vol_bps: tier.enter_vol_bps,
                    exit_vol_bps: tier.exit_vol_bps,
                    spread_bps: tier.spread_bps,
                    refresh_bps: tier.refresh_bps,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SizeSkewFileConfig {
    pub enabled: Option<bool>,
    pub activate_pct: f64,
    pub release_pct: f64,
    pub add_side_factor: f64,
}

impl SizeSkewFileConfig {
    pub(super) fn into_domain(
        self,
        enabled_override: Option<bool>,
    ) -> standx_maker::SizeSkewConfig {
        standx_maker::SizeSkewConfig {
            enabled: enabled_override.or(self.enabled).unwrap_or(false),
            activate_pct: self.activate_pct,
            release_pct: self.release_pct,
            add_side_factor: self.add_side_factor,
        }
    }
}

/// Stage 3 v1 nonlinear price skew (`[nonlinear_skew]`). Field defaults match
/// [`standx_maker::NonlinearSkewConfig`] so partial files stay valid.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct NonlinearSkewFileConfig {
    pub enabled: Option<bool>,
    pub boost: Option<f64>,
    pub cap_bps: Option<f64>,
}

impl NonlinearSkewFileConfig {
    pub(super) fn into_domain(self) -> standx_maker::NonlinearSkewConfig {
        let defaults = standx_maker::NonlinearSkewConfig::default();
        standx_maker::NonlinearSkewConfig {
            enabled: self.enabled.unwrap_or(false),
            boost: self.boost.unwrap_or(defaults.boost),
            cap_bps: self.cap_bps.unwrap_or(defaults.cap_bps),
        }
    }
}

/// Continuous external-price center offset (`[external_skew]`). This mechanism
/// is TOML-only so frozen A/B arm files remain the single source of truth.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExternalSkewFileConfig {
    pub enabled: Option<bool>,
    pub lambda: Option<f64>,
    pub cap_bps: Option<f64>,
    pub dead_zone_bps: Option<f64>,
}

impl ExternalSkewFileConfig {
    pub(super) fn into_domain(self) -> standx_maker::ExternalSkewConfig {
        let defaults = standx_maker::ExternalSkewConfig::default();
        standx_maker::ExternalSkewConfig {
            enabled: self.enabled.unwrap_or(defaults.enabled),
            lambda: self.lambda.unwrap_or(defaults.lambda),
            cap_bps: self.cap_bps.unwrap_or(defaults.cap_bps),
            dead_zone_bps: self.dead_zone_bps.unwrap_or(defaults.dead_zone_bps),
        }
    }
}

/// Continuous in-venue touch-mid center offset (`[microprice]`). TOML-only so
/// frozen A/B arm files remain the single source of truth.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MicroPriceFileConfig {
    pub enabled: Option<bool>,
    pub lambda: Option<f64>,
    pub cap_bps: Option<f64>,
    pub dead_zone_bps: Option<f64>,
}

impl MicroPriceFileConfig {
    pub(super) fn into_domain(self) -> standx_maker::MicroPriceConfig {
        let defaults = standx_maker::MicroPriceConfig::default();
        standx_maker::MicroPriceConfig {
            enabled: self.enabled.unwrap_or(defaults.enabled),
            lambda: self.lambda.unwrap_or(defaults.lambda),
            cap_bps: self.cap_bps.unwrap_or(defaults.cap_bps),
            dead_zone_bps: self.dead_zone_bps.unwrap_or(defaults.dead_zone_bps),
        }
    }
}

/// External-price defensive guard (`[external_guard]`). Field defaults match
/// [`standx_maker::GuardConfig`] so partial files stay valid.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExternalGuardFileConfig {
    pub enabled: Option<bool>,
    pub enter_bps: Option<f64>,
    pub exit_bps: Option<f64>,
    pub max_age_ms: Option<u64>,
    /// CLI-side basis EMA half-life (seconds): the guard triggers on the
    /// excess divergence over this slow baseline, so the persistent
    /// leader-vs-mark basis never latches the guard.
    pub basis_half_life_secs: Option<u64>,
}

/// Default half-life for the divergence-basis EMA (seconds).
pub(super) const DEFAULT_GUARD_BASIS_HALF_LIFE_SECS: u64 = 300;

impl ExternalGuardFileConfig {
    pub(super) fn into_domain(self) -> standx_maker::GuardConfig {
        let defaults = standx_maker::GuardConfig::default();
        standx_maker::GuardConfig {
            enabled: self.enabled.unwrap_or(false),
            enter_bps: self.enter_bps.unwrap_or(defaults.enter_bps),
            exit_bps: self.exit_bps.unwrap_or(defaults.exit_bps),
            max_age_ms: self.max_age_ms.unwrap_or(defaults.max_age_ms),
        }
    }
}

/// Values are optional so an explicit CLI flag can override one field without
/// requiring every strategy default to be repeated in TOML.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MakerFileConfig {
    pub spread_bps: Option<f64>,
    pub band_bps: Option<f64>,
    pub size: Option<f64>,
    pub levels: Option<u32>,
    pub level_step_bps: Option<f64>,
    pub refresh_bps: Option<f64>,
    pub interval: Option<u64>,
    pub max_position: Option<f64>,
    pub skew_bps: Option<f64>,
    pub inventory_exit_pct: Option<f64>,
    pub inventory_exit_qty: Option<f64>,
    pub max_divergence_bps: Option<f64>,
    pub vol_pause_bps: Option<f64>,
    pub vol_window: Option<u32>,
    pub vol_window_secs: Option<u64>,
    pub adaptive_spread: Option<AdaptiveSpreadFileConfig>,
    pub size_skew: Option<SizeSkewFileConfig>,
    pub nonlinear_skew: Option<NonlinearSkewFileConfig>,
    pub external_skew: Option<ExternalSkewFileConfig>,
    pub microprice: Option<MicroPriceFileConfig>,
    pub external_guard: Option<ExternalGuardFileConfig>,
    pub stop_loss: Option<f64>,
    pub alert_loss: Option<f64>,
    pub alert_inventory_pct: Option<f64>,
    pub alert_position_change_pct: Option<f64>,
    pub alert_uptime: Option<f64>,
    pub alert_equity_below: Option<f64>,
    pub alert_margin_below: Option<f64>,
    /// Account-level hard floors (stage 5-b). Distinct from the `alert_*`
    /// thresholds above: breaching these stops the session through
    /// `RuntimeStopReason::AccountFloor`. Default (absent / 0) = off.
    pub stop_equity_below: Option<f64>,
    pub stop_margin_below: Option<f64>,
    pub no_ws: Option<bool>,
    pub order_response_reconnect_attempts: Option<u32>,
    pub order_response_reconnect_backoff: Option<u64>,
    pub account_stream_reconnect_attempts: Option<u32>,
    pub account_stream_reconnect_backoff: Option<u64>,
    /// Deprecated compatibility fields. Existing production files continue to
    /// parse, but transport recovery no longer uses an incident-count circuit.
    pub recovery_incidents_per_window: Option<u32>,
    pub recovery_window_secs: Option<u64>,
}

pub(super) fn load(path: Option<&Path>) -> Result<MakerFileConfig> {
    let path = path
        .map(PathBuf::from)
        .unwrap_or_else(|| Config::default_config_dir().join("maker.toml"));
    if !path.exists() {
        if path.as_path() == Config::default_config_dir().join("maker.toml") {
            return Ok(MakerFileConfig::default());
        }
        return Err(anyhow::anyhow!(
            "maker config file not found: {}",
            path.display()
        ));
    }
    let content = std::fs::read_to_string(&path)?;
    toml::from_str(&content)
        .map_err(|error| anyhow::anyhow!("invalid maker config {}: {}", path.display(), error))
}

/// Resolve `maker run`'s effective configuration: CLI flag, then TOML file,
/// then built-in default.
///
/// This is the one place a strategy knob's default is written down. Keeping it
/// next to [`MakerFileConfig`] means a new knob touches the clap struct (its
/// help text), the TOML mirror here, and one line below — never a fourth
/// destructuring list that silently drops it.
pub(super) fn merge(
    flags: MakerRunFlags,
    file: MakerFileConfig,
    verbose: bool,
) -> Result<MakerRunArgs> {
    let MakerRunFlags {
        spread_bps,
        band_bps,
        size,
        levels,
        level_step_bps,
        refresh_bps,
        interval,
        max_position,
        skew_bps,
        inventory_exit_pct,
        inventory_exit_qty,
        max_divergence_bps,
        vol_pause_bps,
        vol_window,
        adaptive_spread,
        size_skew,
        stop_loss,
        alert_loss,
        alert_inventory_pct,
        alert_position_change_pct,
        alert_uptime,
        alert_equity_below,
        alert_margin_below,
        stop_equity_below,
        stop_margin_below,
        alert_webhook,
        alert_webhook_format,
        no_ws,
        live,
        order_response_reconnect_attempts,
        order_response_reconnect_backoff,
        account_stream_reconnect_attempts,
        account_stream_reconnect_backoff,
        recovery_incidents_per_window,
        recovery_window_secs,
        controlled_disconnect_after,
    } = flags;

    let selected_vol_window = vol_window.or(file.vol_window);
    let selected_vol_window_secs = file.vol_window_secs;
    if selected_vol_window.is_some() && selected_vol_window_secs.is_some() {
        return Err(anyhow::anyhow!(
            "--vol-window conflicts with vol_window_secs in TOML; choose samples or seconds"
        ));
    }
    let adaptive_spread = match file.adaptive_spread {
        Some(config) => config.into_domain(adaptive_spread),
        None if adaptive_spread.unwrap_or(false) => {
            return Err(anyhow::anyhow!(
                "--adaptive-spread requires an [adaptive_spread] TOML section"
            ));
        }
        None => standx_maker::AdaptiveSpreadConfig::default(),
    };
    if adaptive_spread.enabled && selected_vol_window_secs.is_none() {
        return Err(anyhow::anyhow!(
            "adaptive spread requires vol_window_secs in TOML"
        ));
    }
    let size_skew = match file.size_skew {
        Some(config) => config.into_domain(size_skew),
        None if size_skew.is_some() => {
            return Err(anyhow::anyhow!(
                "--size-skew requires a [size_skew] TOML section"
            ));
        }
        None => standx_maker::SizeSkewConfig::default(),
    };
    // Stage 3 v1 combined candidate: TOML-only switches (no CLI overrides) so
    // frozen A/B configs stay the single source of truth.
    let nonlinear_skew = file
        .nonlinear_skew
        .map(|config| config.into_domain())
        .unwrap_or_default();
    let external_skew = file
        .external_skew
        .map(|config| config.into_domain())
        .unwrap_or_default();
    let microprice = file
        .microprice
        .map(|config| config.into_domain())
        .unwrap_or_default();
    let external_guard_basis_half_life_secs = file
        .external_guard
        .as_ref()
        .and_then(|config| config.basis_half_life_secs)
        .unwrap_or(DEFAULT_GUARD_BASIS_HALF_LIFE_SECS);
    let external_guard = file
        .external_guard
        .map(|config| config.into_domain())
        .unwrap_or_default();
    // Keep accepting the removed rolling-circuit knobs for one compatibility
    // window so existing production commands/configs do not fail to parse. They
    // deliberately do not enter MakerRunArgs.
    let _legacy_recovery_circuit = (
        recovery_incidents_per_window.or(file.recovery_incidents_per_window),
        recovery_window_secs.or(file.recovery_window_secs),
    );

    Ok(MakerRunArgs {
        spread_bps: choose(spread_bps, file.spread_bps, 5.0),
        band_bps: choose(band_bps, file.band_bps, 20.0),
        size: choose(size, file.size, 0.01),
        levels: choose(levels, file.levels, 1),
        level_step_bps: choose(level_step_bps, file.level_step_bps, 2.0),
        refresh_bps: choose(refresh_bps, file.refresh_bps, 3.0),
        interval: choose(interval, file.interval, 5),
        max_position: choose(max_position, file.max_position, 0.05),
        skew_bps: choose(skew_bps, file.skew_bps, 0.0),
        inventory_exit_pct: choose(inventory_exit_pct, file.inventory_exit_pct, 0.0),
        inventory_exit_qty: choose(inventory_exit_qty, file.inventory_exit_qty, 0.0),
        max_divergence_bps: choose(max_divergence_bps, file.max_divergence_bps, 25.0),
        vol_pause_bps: choose(vol_pause_bps, file.vol_pause_bps, 0.0),
        vol_window: selected_vol_window.unwrap_or(12),
        vol_window_secs: selected_vol_window_secs,
        adaptive_spread,
        size_skew,
        nonlinear_skew,
        external_skew,
        microprice,
        external_guard,
        external_guard_basis_half_life_secs,
        stop_loss: choose(stop_loss, file.stop_loss, 0.0),
        alert_loss: choose(alert_loss, file.alert_loss, 0.0),
        alert_inventory_pct: choose(alert_inventory_pct, file.alert_inventory_pct, 0.0),
        alert_position_change_pct: choose(
            alert_position_change_pct,
            file.alert_position_change_pct,
            0.0,
        ),
        alert_uptime: choose(alert_uptime, file.alert_uptime, 0.0),
        alert_equity_below: choose(alert_equity_below, file.alert_equity_below, 0.0),
        alert_margin_below: choose(alert_margin_below, file.alert_margin_below, 0.0),
        stop_equity_below: choose(stop_equity_below, file.stop_equity_below, 0.0),
        stop_margin_below: choose(stop_margin_below, file.stop_margin_below, 0.0),
        alert_webhook,
        alert_webhook_format,
        no_ws: choose(no_ws, file.no_ws, false),
        live,
        order_response_reconnect_attempts: choose(
            order_response_reconnect_attempts,
            file.order_response_reconnect_attempts,
            3,
        ),
        order_response_reconnect_backoff: choose(
            order_response_reconnect_backoff,
            file.order_response_reconnect_backoff,
            2,
        ),
        account_stream_reconnect_attempts: choose(
            account_stream_reconnect_attempts,
            file.account_stream_reconnect_attempts,
            3,
        ),
        account_stream_reconnect_backoff: choose(
            account_stream_reconnect_backoff,
            file.account_stream_reconnect_backoff,
            2,
        ),
        controlled_disconnect_after,
        verbose,
    })
}

/// CLI flag wins over the TOML file, which wins over the built-in default.
fn choose<T: Copy>(cli: Option<T>, file: Option<T>, default: T) -> T {
    cli.or(file).unwrap_or(default)
}

pub(super) struct MakerRunArgs {
    pub(super) spread_bps: f64,
    pub(super) band_bps: f64,
    pub(super) size: f64,
    pub(super) levels: u32,
    pub(super) level_step_bps: f64,
    pub(super) refresh_bps: f64,
    pub(super) interval: u64,
    pub(super) max_position: f64,
    pub(super) skew_bps: f64,
    pub(super) inventory_exit_pct: f64,
    pub(super) inventory_exit_qty: f64,
    pub(super) max_divergence_bps: f64,
    pub(super) vol_pause_bps: f64,
    pub(super) vol_window: u32,
    pub(super) vol_window_secs: Option<u64>,
    pub(super) adaptive_spread: standx_maker::AdaptiveSpreadConfig,
    pub(super) size_skew: standx_maker::SizeSkewConfig,
    pub(super) nonlinear_skew: standx_maker::NonlinearSkewConfig,
    pub(super) external_skew: standx_maker::ExternalSkewConfig,
    pub(super) microprice: standx_maker::MicroPriceConfig,
    pub(super) external_guard: standx_maker::GuardConfig,
    pub(super) external_guard_basis_half_life_secs: u64,
    pub(super) stop_loss: f64,
    pub(super) alert_loss: f64,
    pub(super) alert_inventory_pct: f64,
    pub(super) alert_position_change_pct: f64,
    pub(super) alert_uptime: f64,
    pub(super) alert_equity_below: f64,
    pub(super) alert_margin_below: f64,
    /// Account-level hard floors (stage 5-b): breaching either stops the
    /// session through `MakerExit::AccountFloor`. 0 = off, the default.
    pub(super) stop_equity_below: f64,
    pub(super) stop_margin_below: f64,
    pub(super) alert_webhook: Option<String>,
    pub(super) alert_webhook_format: AlertWebhookFormat,
    pub(super) no_ws: bool,
    pub(super) live: bool,
    pub(super) order_response_reconnect_attempts: u32,
    pub(super) order_response_reconnect_backoff: u64,
    pub(super) account_stream_reconnect_attempts: u32,
    pub(super) account_stream_reconnect_backoff: u64,
    pub(super) controlled_disconnect_after: Option<u64>,
    pub(super) verbose: bool,
}

/// Validate the CLI-owned composition constraints for `[external_skew]`.
/// Maker core remains a pure signal/center calculation and never learns about
/// TOML sections or ladder geometry.
pub(super) fn validate_external_skew(
    external: standx_maker::ExternalSkewConfig,
    base: &standx_maker::MakerConfig,
    adaptive_spread: &standx_maker::AdaptiveSpreadConfig,
    nonlinear: standx_maker::NonlinearSkewConfig,
    guard: standx_maker::GuardConfig,
) -> Result<()> {
    if !external.lambda.is_finite()
        || !external.cap_bps.is_finite()
        || !external.dead_zone_bps.is_finite()
    {
        return Err(anyhow::anyhow!("external skew values must be finite"));
    }
    if external.lambda < 0.0 {
        return Err(anyhow::anyhow!("external skew lambda must be >= 0"));
    }
    if external.cap_bps <= 0.0 {
        return Err(anyhow::anyhow!("external skew cap_bps must be > 0"));
    }
    if external.dead_zone_bps < 0.0 {
        return Err(anyhow::anyhow!("external skew dead_zone_bps must be >= 0"));
    }
    if !external.enabled {
        return Ok(());
    }
    if !guard.enabled {
        return Err(anyhow::anyhow!(
            "enabled external skew requires an enabled [external_guard]"
        ));
    }
    if external.cap_bps >= guard.enter_bps {
        return Err(anyhow::anyhow!(
            "external skew cap_bps must be < external_guard enter_bps"
        ));
    }

    let ladder_bps = f64::from(base.levels.saturating_sub(1)) * base.level_step_bps;
    // `skew_center_with` falls back to the legacy linear curve when nonlinear
    // skew is off, and that curve saturates at `base.skew_bps`.
    let inventory_cap_bps = if nonlinear.enabled {
        nonlinear.cap_bps
    } else {
        base.skew_bps
    };
    let spread_cap_bps = if adaptive_spread.enabled {
        adaptive_spread
            .tiers
            .iter()
            .map(|tier| tier.spread_bps)
            .fold(base.spread_bps, f64::max)
    } else {
        base.spread_bps
    };
    let budget_bps = spread_cap_bps + ladder_bps + inventory_cap_bps + external.cap_bps;
    if budget_bps > base.band_bps {
        return Err(anyhow::anyhow!(
            "external skew violates band red line: spread_bps + ladder + inventory cap + cap_bps = {budget_bps} must be <= band_bps {}",
            base.band_bps
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn cli_value_overrides_maker_file_then_default() {
        assert_eq!(choose(Some(3_u32), Some(2), 1), 3);
        assert_eq!(choose(None, Some(2_u32), 1), 2);
        assert_eq!(choose(None::<u32>, None, 1), 1);
    }

    use super::*;

    #[test]
    fn parses_partial_non_sensitive_strategy_file() {
        let config: MakerFileConfig = toml::from_str(
            "spread_bps = 8\nmax_position = 0.02\nalert_position_change_pct = 20\nno_ws = true\norder_response_reconnect_attempts = 3\norder_response_reconnect_backoff = 2\naccount_stream_reconnect_attempts = 3\naccount_stream_reconnect_backoff = 2\nrecovery_incidents_per_window = 3\nrecovery_window_secs = 3600\n",
        )
        .unwrap();
        assert_eq!(config.spread_bps, Some(8.0));
        assert_eq!(config.max_position, Some(0.02));
        assert_eq!(config.alert_position_change_pct, Some(20.0));
        assert_eq!(config.no_ws, Some(true));
        assert_eq!(config.order_response_reconnect_attempts, Some(3));
        assert_eq!(config.order_response_reconnect_backoff, Some(2));
        assert_eq!(config.account_stream_reconnect_attempts, Some(3));
        assert_eq!(config.account_stream_reconnect_backoff, Some(2));
        assert_eq!(config.recovery_incidents_per_window, Some(3));
        assert_eq!(config.recovery_window_secs, Some(3600));
        assert_eq!(config.size, None);
    }

    #[test]
    fn parses_stop_loss_and_account_floor_fields() {
        let config: MakerFileConfig =
            toml::from_str("stop_loss = 25\nalert_equity_below = 100\nalert_margin_below = 40\n")
                .unwrap();
        assert_eq!(config.stop_loss, Some(25.0));
        assert_eq!(config.alert_equity_below, Some(100.0));
        assert_eq!(config.alert_margin_below, Some(40.0));
        // Stage 5-b hard floors are absent unless configured: alert thresholds
        // must never arm the solvency brake by proxy.
        assert_eq!(config.stop_equity_below, None);
        assert_eq!(config.stop_margin_below, None);
    }

    #[test]
    fn parses_account_hard_floor_fields_separately_from_alerts() {
        let config: MakerFileConfig = toml::from_str(
            "stop_equity_below = 80
stop_margin_below = 20
",
        )
        .unwrap();
        assert_eq!(config.stop_equity_below, Some(80.0));
        assert_eq!(config.stop_margin_below, Some(20.0));
        assert_eq!(config.alert_equity_below, None);
        assert_eq!(config.alert_margin_below, None);
    }

    #[test]
    fn parses_structured_adaptive_spread_tiers() {
        let config: MakerFileConfig = toml::from_str(
            r#"
vol_window_secs = 60
[adaptive_spread]
enabled = true
min_spread_bps = 8
max_spread_bps = 18

[[adaptive_spread.tiers]]
spread_bps = 8
refresh_bps = 4

[[adaptive_spread.tiers]]
enter_vol_bps = 10
exit_vol_bps = 7
spread_bps = 12
refresh_bps = 5
"#,
        )
        .unwrap();
        let adaptive = config.adaptive_spread.unwrap().into_domain(Some(false));
        assert!(!adaptive.enabled);
        assert_eq!(adaptive.tiers.len(), 2);
        assert_eq!(adaptive.tiers[1].enter_vol_bps, Some(10.0));
    }

    #[test]
    fn parses_size_skew_and_cli_override_wins() {
        let config: MakerFileConfig = toml::from_str(
            r#"
[size_skew]
enabled = true
activate_pct = 30
release_pct = 20
add_side_factor = 0.5
"#,
        )
        .unwrap();
        let file_config = config.size_skew.unwrap();
        let configured = file_config.clone().into_domain(None);
        let overridden = file_config.into_domain(Some(false));

        assert!(configured.enabled);
        assert!(!overridden.enabled);
        assert_eq!(overridden.activate_pct, 30.0);
        assert_eq!(overridden.release_pct, 20.0);
        assert_eq!(overridden.add_side_factor, 0.5);
    }

    #[test]
    fn example_keeps_active_inventory_exit_disabled() {
        let config: MakerFileConfig = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker.toml"
        )))
        .unwrap();

        assert_eq!(config.inventory_exit_pct, Some(0.0));
        assert_eq!(config.inventory_exit_qty, Some(0.0));
        assert_eq!(config.order_response_reconnect_attempts, Some(3));
        assert_eq!(config.order_response_reconnect_backoff, Some(2));
        assert_eq!(config.account_stream_reconnect_attempts, Some(3));
        assert_eq!(config.account_stream_reconnect_backoff, Some(2));
        assert_eq!(config.recovery_incidents_per_window, None);
        assert_eq!(config.recovery_window_secs, None);
    }

    #[test]
    fn rejects_unknown_keys_so_a_typo_does_not_silently_disable_a_guard() {
        // `alert_los` is a typo for `alert_loss`; without deny_unknown_fields it
        // parses fine and the loss guard stays off without warning.
        let error = toml::from_str::<MakerFileConfig>("alert_los = 3.0\n").unwrap_err();
        assert!(
            error.to_string().contains("alert_los"),
            "error should name the offending key: {error}"
        );
    }

    #[test]
    fn xag_example_enables_twenty_percent_position_jump_alert() {
        let config: MakerFileConfig = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-xag-100u.toml"
        )))
        .unwrap();

        assert_eq!(config.max_position, Some(0.8));
        assert_eq!(config.alert_position_change_pct, Some(20.0));
    }

    #[test]
    fn conservative_bnb_example_preserves_xag_notional_scale() {
        let config: MakerFileConfig = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-bnb-100u-conservative.toml"
        )))
        .unwrap();

        assert_eq!(config.size, Some(0.02));
        assert_eq!(config.max_position, Some(0.08));
        assert_eq!(config.inventory_exit_pct, Some(50.0));
        assert_eq!(config.inventory_exit_qty, Some(0.02));
    }

    #[test]
    fn conservative_tsla_example_preserves_xag_notional_scale() {
        let config: MakerFileConfig = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-tsla-100u-conservative.toml"
        )))
        .unwrap();

        assert_eq!(config.size, Some(0.03));
        assert_eq!(config.max_position, Some(0.12));
        assert_eq!(config.inventory_exit_pct, Some(50.0));
        assert_eq!(config.inventory_exit_qty, Some(0.03));
    }

    #[test]
    fn stage2_live_arms_only_differ_by_adaptive_enable_switch() {
        let baseline = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-stage2-xag-baseline.toml"
        ));
        let candidate = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-stage2-xag-candidate.toml"
        ));
        assert_eq!(
            baseline.replace("enabled = false", "enabled = true"),
            candidate
        );

        let baseline: MakerFileConfig = toml::from_str(baseline).unwrap();
        let candidate: MakerFileConfig = toml::from_str(candidate).unwrap();
        assert_eq!(baseline.vol_window_secs, Some(60));
        assert_eq!(baseline.size, Some(0.01));
        assert_eq!(baseline.max_position, Some(0.2));
        assert!(!baseline.adaptive_spread.unwrap().enabled.unwrap());
        assert!(candidate.adaptive_spread.unwrap().enabled.unwrap());
    }

    #[test]
    fn stage3_live_arms_only_differ_by_size_skew_enable_switch() {
        let baseline = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-stage3-hype-baseline.toml"
        ));
        let candidate = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-stage3-hype-candidate.toml"
        ));
        assert_eq!(baseline.lines().count(), candidate.lines().count());
        let differing_lines: Vec<_> = baseline
            .lines()
            .zip(candidate.lines())
            .filter(|(baseline_line, candidate_line)| baseline_line != candidate_line)
            .collect();
        assert_eq!(differing_lines, vec![("enabled = false", "enabled = true")]);

        let baseline: MakerFileConfig = toml::from_str(baseline).unwrap();
        let candidate: MakerFileConfig = toml::from_str(candidate).unwrap();
        assert!(!baseline.adaptive_spread.unwrap().enabled.unwrap());
        assert!(!candidate.adaptive_spread.unwrap().enabled.unwrap());

        let baseline = baseline.size_skew.unwrap().into_domain(None);
        let candidate = candidate.size_skew.unwrap().into_domain(None);
        assert!(!baseline.enabled);
        assert!(candidate.enabled);
        assert_eq!(baseline.activate_pct, 30.0);
        assert_eq!(baseline.release_pct, 20.0);
        assert_eq!(baseline.add_side_factor, 0.5);
    }

    #[test]
    fn parses_nonlinear_skew_and_external_guard_sections() {
        let config: MakerFileConfig = toml::from_str(
            "[nonlinear_skew]\nenabled = true\nboost = 3.0\ncap_bps = 12.0\n\n[external_guard]\nenabled = true\nenter_bps = 6.0\nexit_bps = 3.0\nmax_age_ms = 5000\n",
        )
        .unwrap();
        let nonlinear = config.nonlinear_skew.unwrap().into_domain();
        assert!(nonlinear.enabled);
        assert_eq!(nonlinear.boost, 3.0);
        assert_eq!(nonlinear.cap_bps, 12.0);
        let guard = config.external_guard.unwrap().into_domain();
        assert!(guard.enabled);
        assert_eq!(guard.enter_bps, 6.0);
        assert_eq!(guard.exit_bps, 3.0);
        assert_eq!(guard.max_age_ms, 5000);

        // Partial sections fall back to domain defaults, disabled by default.
        let partial: MakerFileConfig =
            toml::from_str("[nonlinear_skew]\nboost = 2.0\n\n[external_guard]\nenter_bps = 8.0\n")
                .unwrap();
        let nonlinear = partial.nonlinear_skew.unwrap().into_domain();
        assert!(!nonlinear.enabled);
        assert_eq!(nonlinear.boost, 2.0);
        assert_eq!(nonlinear.cap_bps, 12.0);
        let guard = partial.external_guard.unwrap().into_domain();
        assert!(!guard.enabled);
        assert_eq!(guard.enter_bps, 8.0);
        assert_eq!(guard.exit_bps, 3.0);
    }

    #[test]
    fn parses_external_skew_full_partial_and_rejects_unknown_fields() {
        let config: MakerFileConfig = toml::from_str(
            "[external_skew]\nenabled = true\nlambda = 0.5\ncap_bps = 8.0\ndead_zone_bps = 1.0\n",
        )
        .unwrap();
        let skew = config.external_skew.unwrap().into_domain();
        assert!(skew.enabled);
        assert_eq!(skew.lambda, 0.5);
        assert_eq!(skew.cap_bps, 8.0);
        assert_eq!(skew.dead_zone_bps, 1.0);

        let partial: MakerFileConfig = toml::from_str("[external_skew]\nlambda = 0.25\n").unwrap();
        let skew = partial.external_skew.unwrap().into_domain();
        assert!(!skew.enabled);
        assert_eq!(skew.lambda, 0.25);
        assert_eq!(skew.cap_bps, 8.0);
        assert_eq!(skew.dead_zone_bps, 1.0);

        assert!(toml::from_str::<MakerFileConfig>(
            "[external_skew]\nenabled = true\nunknown = 1\n"
        )
        .is_err());
    }

    fn external_skew_validation_base() -> standx_maker::MakerConfig {
        standx_maker::MakerConfig {
            spread_bps: 8.0,
            band_bps: 30.0,
            level_step_bps: 2.0,
            refresh_bps: 4.0,
            levels: 1,
            size: 0.1,
            max_position: 1.0,
            skew_bps: 8.0,
            price_decimals: 3,
            qty_decimals: 2,
            min_order_qty: 0.1,
        }
    }

    #[test]
    fn external_skew_validation_enforces_guard_and_band_red_lines() {
        let external = standx_maker::ExternalSkewConfig {
            enabled: true,
            ..Default::default()
        };
        let nonlinear = standx_maker::NonlinearSkewConfig {
            enabled: true,
            boost: 3.0,
            cap_bps: 12.0,
        };
        let guard = standx_maker::GuardConfig {
            enabled: true,
            enter_bps: 10.0,
            exit_bps: 5.0,
            max_age_ms: 5000,
        };
        assert!(validate_external_skew(
            external,
            &external_skew_validation_base(),
            &Default::default(),
            nonlinear,
            guard,
        )
        .is_ok());

        let mut too_many_levels = external_skew_validation_base();
        too_many_levels.levels = 3;
        let error = validate_external_skew(
            external,
            &too_many_levels,
            &Default::default(),
            nonlinear,
            guard,
        )
        .expect_err("outer ladder levels must consume band budget");
        assert!(error.to_string().contains("band red line"), "{error}");

        let wide_guard = standx_maker::GuardConfig {
            enter_bps: 20.0,
            ..guard
        };
        let exact_edge = standx_maker::ExternalSkewConfig {
            cap_bps: 10.0,
            ..external
        };
        assert!(validate_external_skew(
            exact_edge,
            &external_skew_validation_base(),
            &Default::default(),
            nonlinear,
            wide_guard,
        )
        .is_ok());
        let over_edge = standx_maker::ExternalSkewConfig {
            cap_bps: 10.5,
            ..external
        };
        assert!(validate_external_skew(
            over_edge,
            &external_skew_validation_base(),
            &Default::default(),
            nonlinear,
            wide_guard,
        )
        .is_err());

        let guard_disabled = standx_maker::GuardConfig {
            enabled: false,
            ..guard
        };
        assert!(validate_external_skew(
            external,
            &external_skew_validation_base(),
            &Default::default(),
            nonlinear,
            guard_disabled,
        )
        .is_err());

        let adaptive_spread = standx_maker::AdaptiveSpreadConfig {
            enabled: true,
            min_spread_bps: 8.0,
            max_spread_bps: 18.0,
            tiers: vec![standx_maker::SpreadTier {
                enter_vol_bps: None,
                exit_vol_bps: None,
                spread_bps: 18.0,
                refresh_bps: 4.0,
            }],
        };
        assert!(validate_external_skew(
            external,
            &external_skew_validation_base(),
            &adaptive_spread,
            nonlinear,
            guard,
        )
        .is_err());
    }

    #[test]
    fn external_skew_validation_rejects_invalid_scalars_even_when_disabled() {
        let base = external_skew_validation_base();
        for external in [
            standx_maker::ExternalSkewConfig {
                lambda: f64::NAN,
                ..Default::default()
            },
            standx_maker::ExternalSkewConfig {
                lambda: -0.5,
                ..Default::default()
            },
            standx_maker::ExternalSkewConfig {
                cap_bps: 0.0,
                ..Default::default()
            },
            standx_maker::ExternalSkewConfig {
                dead_zone_bps: -1.0,
                ..Default::default()
            },
        ] {
            assert!(validate_external_skew(
                external,
                &base,
                &Default::default(),
                Default::default(),
                Default::default(),
            )
            .is_err());
        }
    }

    #[test]
    fn external_skew_band_budget_uses_legacy_cap_when_nonlinear_is_off() {
        let external = standx_maker::ExternalSkewConfig {
            enabled: true,
            ..Default::default()
        };
        let nonlinear = standx_maker::NonlinearSkewConfig::default();
        let guard = standx_maker::GuardConfig {
            enabled: true,
            enter_bps: 10.0,
            exit_bps: 5.0,
            max_age_ms: 5000,
        };
        let mut base = external_skew_validation_base();
        base.skew_bps = 16.0;
        assert!(
            validate_external_skew(external, &base, &Default::default(), nonlinear, guard,)
                .is_err()
        );
    }

    #[test]
    fn external_skew_candidate_is_frozen_baseline_plus_one_section() {
        let baseline = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-guard-hype-candidate.toml"
        ));
        let candidate = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-external-skew-hype-candidate.toml"
        ));
        let suffix = concat!(
            "\n# Candidate arm (docs/29): continuous external-price center offset.\n",
            "# Band red line: spread(8) + nonlinear.cap(12) + external.cap(8) = 28 <= band(30).\n",
            "[external_skew]\n",
            "enabled = true\n",
            "lambda = 0.5\n",
            "cap_bps = 8.0\n",
            "dead_zone_bps = 1.0\n",
        );
        assert_eq!(candidate.strip_suffix(suffix), Some(baseline));
    }

    /// The frozen production baseline (docs/25: skew + guard both on) must keep
    /// parsing untouched, and stage 5-b must not have armed anything in it: the
    /// account hard floors stay absent, so the new solvency brake cannot fire
    /// on the production config until an operator adds them deliberately.
    #[test]
    fn frozen_production_baseline_parses_with_hard_floors_unarmed() {
        let config: MakerFileConfig = toml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-guard-hype-candidate.toml"
        )))
        .unwrap();

        // The two accepted stage 3 mechanisms are the baseline.
        assert!(config.nonlinear_skew.as_ref().unwrap().enabled.unwrap());
        assert!(config.external_guard.as_ref().unwrap().enabled.unwrap());
        // Alerts stay armed…
        assert_eq!(config.alert_equity_below, Some(94.0));
        assert_eq!(config.alert_margin_below, Some(30.0));
        assert_eq!(config.stop_loss, Some(5.0));
        // …and the stage 5-b hard floors stay off.
        assert_eq!(config.stop_equity_below, None);
        assert_eq!(config.stop_margin_below, None);
    }

    #[test]
    fn stage3v1_live_arms_only_differ_by_combined_enable_switches() {
        let baseline = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-stage3v1-hype-baseline.toml"
        ));
        let candidate = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/maker-stage3v1-hype-candidate.toml"
        ));
        assert_eq!(baseline.lines().count(), candidate.lines().count());
        let differing_lines: Vec<_> = baseline
            .lines()
            .zip(candidate.lines())
            .filter(|(baseline_line, candidate_line)| baseline_line != candidate_line)
            .collect();
        assert_eq!(
            differing_lines,
            vec![
                ("enabled = false", "enabled = true"),
                ("enabled = false", "enabled = true"),
            ]
        );

        let baseline: MakerFileConfig = toml::from_str(baseline).unwrap();
        let candidate: MakerFileConfig = toml::from_str(candidate).unwrap();
        // Every other controller stays off in both arms.
        assert!(!baseline.adaptive_spread.as_ref().unwrap().enabled.unwrap());
        assert!(!candidate.adaptive_spread.as_ref().unwrap().enabled.unwrap());
        assert!(!baseline
            .size_skew
            .as_ref()
            .unwrap()
            .enabled
            .unwrap_or(false));
        assert!(!candidate
            .size_skew
            .as_ref()
            .unwrap()
            .enabled
            .unwrap_or(false));

        let baseline_nl = baseline.nonlinear_skew.unwrap().into_domain();
        let candidate_nl = candidate.nonlinear_skew.unwrap().into_domain();
        assert!(!baseline_nl.enabled);
        assert!(candidate_nl.enabled);
        assert_eq!(candidate_nl.boost, 3.0);
        assert_eq!(candidate_nl.cap_bps, 12.0);

        let baseline_guard = baseline.external_guard.unwrap().into_domain();
        let candidate_guard = candidate.external_guard.unwrap().into_domain();
        assert!(!baseline_guard.enabled);
        assert!(candidate_guard.enabled);
        // Round-2 base (release owner 2026-07-23): thresholds raised to shed
        // the noisy 6-10bps band; see docs/22.
        assert_eq!(candidate_guard.enter_bps, 10.0);
        assert_eq!(candidate_guard.exit_bps, 5.0);
        assert_eq!(candidate_guard.max_age_ms, 5000);

        // Band red line holds for the frozen candidate: spread + cap <= band.
        let spread = candidate.spread_bps.unwrap();
        let band = candidate.band_bps.unwrap();
        assert!(spread + candidate_nl.cap_bps <= band);
    }
}
