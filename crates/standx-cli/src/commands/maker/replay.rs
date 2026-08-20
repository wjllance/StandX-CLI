//! CLI adapter for normalized deterministic maker replay traces.

use crate::cli::OutputFormat;
use anyhow::{Context, Result};
use serde::Deserialize;
#[cfg(test)]
use standx_maker::DesiredQuote;
use standx_maker::{
    run_replay, Action, AdaptiveSpreadConfig, ExecutionCosts, FillRole, MakerConfig,
    MarketSnapshot, PerformanceFill, ReplayCycle, ReplayEvent, ReplayResult, ReplaySettings,
    RestingQuote, SizeSkewConfig, SpreadTier,
};
use standx_sdk::models::OrderSide;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

const TRACE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TraceRecord {
    Header {
        schema_version: u32,
        symbol: String,
        git_sha: String,
        config_hash: String,
        seed: u64,
        config: TraceMakerConfig,
        settings: Box<TraceReplaySettings>,
    },
    Cycle {
        event_time_ms: i64,
        cycle: u64,
        mark: f64,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
        position: f64,
        #[serde(default)]
        resting: Vec<TraceRestingQuote>,
        #[serde(default)]
        pending_slots: Vec<TraceQuoteSlot>,
        eligible_bid_qty: f64,
        eligible_ask_qty: f64,
    },
    Fill {
        trade_id: u64,
        order_id: u64,
        role: TraceFillRole,
        side: OrderSide,
        price: f64,
        qty: f64,
        mark_at_fill: f64,
        event_time_ms: i64,
        costs: Option<TraceExecutionCosts>,
    },
    Funding {
        event_time_ms: i64,
        cashflow_quote: f64,
    },
    Finish {
        event_time_ms: i64,
    },
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TraceFillRole {
    PassiveMaker,
    InventoryExit,
}

impl From<TraceFillRole> for FillRole {
    fn from(value: TraceFillRole) -> Self {
        match value {
            TraceFillRole::PassiveMaker => Self::PassiveMaker,
            TraceFillRole::InventoryExit => Self::InventoryExit,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceExecutionCosts {
    fee_quote: f64,
    rebate_quote: f64,
}

impl From<TraceExecutionCosts> for ExecutionCosts {
    fn from(value: TraceExecutionCosts) -> Self {
        Self {
            fee_quote: value.fee_quote,
            rebate_quote: value.rebate_quote,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceMakerConfig {
    spread_bps: f64,
    band_bps: f64,
    level_step_bps: f64,
    refresh_bps: f64,
    levels: u32,
    size: f64,
    max_position: f64,
    skew_bps: f64,
    price_decimals: u32,
    qty_decimals: u32,
    min_order_qty: f64,
}

impl From<TraceMakerConfig> for MakerConfig {
    fn from(value: TraceMakerConfig) -> Self {
        Self {
            spread_bps: value.spread_bps,
            band_bps: value.band_bps,
            level_step_bps: value.level_step_bps,
            refresh_bps: value.refresh_bps,
            levels: value.levels,
            size: value.size,
            max_position: value.max_position,
            skew_bps: value.skew_bps,
            price_decimals: value.price_decimals,
            qty_decimals: value.qty_decimals,
            min_order_qty: value.min_order_qty,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceReplaySettings {
    starting_position: f64,
    starting_mark: f64,
    max_divergence_bps: f64,
    require_full_touch: bool,
    vol_window: Option<usize>,
    #[serde(default)]
    vol_window_secs: Option<u64>,
    vol_pause_bps: f64,
    #[serde(default)]
    adaptive_spread: Option<TraceAdaptiveSpreadConfig>,
    #[serde(default)]
    size_skew: Option<TraceSizeSkewConfig>,
    active_exit_enabled: bool,
    inventory_exit_pct: f64,
    inventory_exit_qty: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceAdaptiveSpreadConfig {
    enabled: bool,
    min_spread_bps: f64,
    max_spread_bps: f64,
    tiers: Vec<TraceSpreadTier>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceSpreadTier {
    #[serde(default)]
    enter_vol_bps: Option<f64>,
    #[serde(default)]
    exit_vol_bps: Option<f64>,
    spread_bps: f64,
    refresh_bps: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceSizeSkewConfig {
    enabled: bool,
    activate_pct: f64,
    release_pct: f64,
    add_side_factor: f64,
}

impl TryFrom<TraceReplaySettings> for ReplaySettings {
    type Error = anyhow::Error;

    fn try_from(value: TraceReplaySettings) -> Result<Self> {
        if value.vol_window.is_some() && value.vol_window_secs.is_some() {
            anyhow::bail!("replay settings vol_window and vol_window_secs conflict");
        }
        let adaptive_spread = value
            .adaptive_spread
            .map(|config| AdaptiveSpreadConfig {
                enabled: config.enabled,
                min_spread_bps: config.min_spread_bps,
                max_spread_bps: config.max_spread_bps,
                tiers: config
                    .tiers
                    .into_iter()
                    .map(|tier| SpreadTier {
                        enter_vol_bps: tier.enter_vol_bps,
                        exit_vol_bps: tier.exit_vol_bps,
                        spread_bps: tier.spread_bps,
                        refresh_bps: tier.refresh_bps,
                    })
                    .collect(),
            })
            .unwrap_or_default();
        if adaptive_spread.enabled && value.vol_window_secs.is_none() {
            anyhow::bail!("adaptive replay requires vol_window_secs");
        }
        let size_skew = value
            .size_skew
            .map(|config| SizeSkewConfig {
                enabled: config.enabled,
                activate_pct: config.activate_pct,
                release_pct: config.release_pct,
                add_side_factor: config.add_side_factor,
            })
            .unwrap_or_default();
        Ok(Self {
            starting_position: value.starting_position,
            starting_mark: value.starting_mark,
            max_divergence_bps: value.max_divergence_bps,
            require_full_touch: value.require_full_touch,
            vol_window: value.vol_window.unwrap_or(1),
            vol_window_secs: value.vol_window_secs,
            vol_pause_bps: value.vol_pause_bps,
            adaptive_spread,
            size_skew,
            active_exit_enabled: value.active_exit_enabled,
            inventory_exit_pct: value.inventory_exit_pct,
            inventory_exit_qty: value.inventory_exit_qty,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceRestingQuote {
    order_id: Option<String>,
    side: OrderSide,
    level: u32,
    price: f64,
    qty: f64,
    ref_center: f64,
    placed_at_cycle: u64,
}

impl From<TraceRestingQuote> for RestingQuote {
    fn from(value: TraceRestingQuote) -> Self {
        Self {
            order_id: value.order_id,
            side: value.side,
            level: value.level,
            price: value.price,
            qty: value.qty,
            ref_center: value.ref_center,
            placed_at_cycle: value.placed_at_cycle,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceQuoteSlot {
    side: OrderSide,
    level: u32,
}

struct ParsedTrace {
    symbol: String,
    git_sha: String,
    config_hash: String,
    seed: u64,
    config: MakerConfig,
    settings: ReplaySettings,
    events: Vec<ReplayEvent>,
    end_time_ms: i64,
}

pub(super) fn run(path: &Path, output_format: OutputFormat) -> Result<()> {
    let reader: Box<dyn Read> = if path == Path::new("-") {
        Box::new(std::io::stdin())
    } else {
        Box::new(
            std::fs::File::open(path)
                .with_context(|| format!("failed to open replay trace {}", path.display()))?,
        )
    };
    let trace = parse(BufReader::new(reader))?;
    let result = run_replay(
        &trace.config,
        trace.settings.clone(),
        &trace.events,
        trace.end_time_ms,
    )?;
    emit(&trace, &result, output_format);
    Ok(())
}

fn parse(reader: impl BufRead) -> Result<ParsedTrace> {
    let mut header = None;
    let mut events = Vec::new();
    let mut finish = None;
    for (index, line) in reader.lines().enumerate() {
        let line_number = index + 1;
        let line =
            line.with_context(|| format!("failed to read replay trace line {line_number}"))?;
        if line.trim().is_empty() {
            continue;
        }
        if finish.is_some() {
            anyhow::bail!("replay trace has a record after finish at line {line_number}");
        }
        let record: TraceRecord = serde_json::from_str(&line)
            .with_context(|| format!("invalid replay trace record at line {line_number}"))?;
        match record {
            TraceRecord::Header {
                schema_version,
                symbol,
                git_sha,
                config_hash,
                seed,
                config,
                settings,
            } => {
                if header.is_some() || !events.is_empty() {
                    anyhow::bail!("replay header must be the first and only header");
                }
                if schema_version != TRACE_SCHEMA_VERSION {
                    anyhow::bail!(
                        "unsupported replay schema version {schema_version}; expected {TRACE_SCHEMA_VERSION}"
                    );
                }
                if symbol.trim().is_empty()
                    || git_sha.trim().is_empty()
                    || config_hash.trim().is_empty()
                {
                    anyhow::bail!("replay header identity fields must be non-empty");
                }
                header = Some((
                    symbol,
                    git_sha,
                    config_hash,
                    seed,
                    config.into(),
                    (*settings).try_into()?,
                ));
            }
            TraceRecord::Cycle {
                event_time_ms,
                cycle,
                mark,
                best_bid,
                best_ask,
                position,
                resting,
                pending_slots,
                eligible_bid_qty,
                eligible_ask_qty,
            } => {
                require_header(&header, line_number)?;
                events.push(ReplayEvent::Cycle(ReplayCycle {
                    event_time_ms,
                    cycle,
                    market: MarketSnapshot {
                        mark,
                        best_bid,
                        best_ask,
                    },
                    position,
                    resting: resting.into_iter().map(Into::into).collect(),
                    pending_slots: pending_slots
                        .into_iter()
                        .map(|slot| (slot.side, slot.level))
                        .collect(),
                    eligible_bid_qty,
                    eligible_ask_qty,
                }));
            }
            TraceRecord::Fill {
                trade_id,
                order_id,
                role,
                side,
                price,
                qty,
                mark_at_fill,
                event_time_ms,
                costs,
            } => {
                require_header(&header, line_number)?;
                events.push(ReplayEvent::Fill(PerformanceFill {
                    trade_id,
                    order_id,
                    role: role.into(),
                    side,
                    price,
                    qty,
                    mark_at_fill,
                    event_time_ms,
                    costs: costs.map(Into::into),
                }));
            }
            TraceRecord::Funding {
                event_time_ms,
                cashflow_quote,
            } => {
                require_header(&header, line_number)?;
                events.push(ReplayEvent::Funding {
                    event_time_ms,
                    cashflow_quote,
                });
            }
            TraceRecord::Finish { event_time_ms } => {
                require_header(&header, line_number)?;
                finish = Some(event_time_ms);
            }
        }
    }
    let (symbol, git_sha, config_hash, seed, config, settings) =
        header.context("replay trace is missing header")?;
    let end_time_ms = finish.context("replay trace is missing finish")?;
    Ok(ParsedTrace {
        symbol,
        git_sha,
        config_hash,
        seed,
        config,
        settings,
        events,
        end_time_ms,
    })
}

fn require_header<T>(header: &Option<T>, line_number: usize) -> Result<()> {
    if header.is_none() {
        anyhow::bail!("replay record at line {line_number} appears before header");
    }
    Ok(())
}

fn emit(trace: &ParsedTrace, result: &ReplayResult, output_format: OutputFormat) {
    if output_format == OutputFormat::Json {
        for cycle in &result.cycles {
            let actions = cycle
                .plan
                .as_ref()
                .map(|plan| plan.actions.iter().map(action_json).collect::<Vec<_>>());
            println!(
                "{}",
                serde_json::json!({
                    "action": "replay_cycle",
                    "symbol": trace.symbol,
                    "git_sha": trace.git_sha,
                    "config_hash": trace.config_hash,
                    "seed": trace.seed,
                    "event_time_ms": cycle.event_time_ms,
                    "cycle": cycle.cycle,
                    "halted": cycle.preflight.halted,
                    "skip": cycle.preflight.skip.map(|skip| format!("{skip:?}")),
                    "rolling_vol_bps": cycle.spread_decision.rolling_vol_bps,
                    "adaptive_spread_enabled": cycle.spread_decision.enabled,
                    "adaptive_spread_tier": cycle.spread_decision.tier,
                    "effective_spread_bps": cycle.spread_decision.effective_spread_bps,
                    "effective_refresh_bps": cycle.spread_decision.effective_refresh_bps,
                    "size_skew_enabled": cycle.size_skew_decision.enabled,
                    "size_skew_active": cycle.size_skew_decision.active,
                    "size_skew_add_side": cycle.size_skew_decision.add_side,
                    "size_skew_inventory_ratio": cycle.size_skew_decision.inventory_ratio,
                    "size_skew_add_qty": cycle.size_skew_decision.add_qty,
                    "actions": actions,
                })
            );
        }
        println!("{}", summary_json(trace, result));
    } else {
        println!(
            "Replay {} cycles={} passive_fills={} exit_fills={} net_pnl={:.6} uptime={:.2}%",
            trace.symbol,
            result.cycles.len(),
            result.performance.passive_fills,
            result.performance.exit_fills,
            result.performance.net_pnl_quote,
            result.performance.quote_time.two_sided_uptime_pct,
        );
    }
}

fn action_json(action: &Action) -> serde_json::Value {
    match action {
        Action::Place(quote) => serde_json::json!({
            "kind": "place", "side": quote.side, "level": quote.level,
            "price": quote.price, "qty": quote.qty,
        }),
        Action::Cancel {
            order_id,
            side,
            level,
            price,
            reason,
        } => serde_json::json!({
            "kind": "cancel", "order_id": order_id, "side": side,
            "level": level, "price": price, "reason": reason.as_str(),
        }),
        Action::Hold {
            side,
            level,
            price,
            age_cycles,
            drift_bps,
        } => serde_json::json!({
            "kind": "hold", "side": side, "level": level, "price": price,
            "age_cycles": age_cycles, "drift_bps": drift_bps,
        }),
    }
}

fn summary_json(trace: &ParsedTrace, result: &ReplayResult) -> serde_json::Value {
    let performance = &result.performance;
    let markouts = performance
        .markouts
        .iter()
        .map(|markout| {
            serde_json::json!({
                "window_ms": markout.window_ms,
                "samples": markout.samples,
                "pending": markout.pending,
                "unavailable": markout.unavailable,
                "qty": markout.qty,
                "quote_pnl": markout.quote_pnl,
                "avg_bps": markout.avg_bps,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "action": "replay_summary",
        "schema_version": TRACE_SCHEMA_VERSION,
        "symbol": trace.symbol,
        "git_sha": trace.git_sha,
        "config_hash": trace.config_hash,
        "seed": trace.seed,
        "cycles": result.cycles.len(),
        "passive_fills": performance.passive_fills,
        "passive_qty": performance.passive_qty,
        "passive_cashflow_quote": performance.passive_cashflow_quote,
        "passive_capture_bps": performance.passive_capture_bps,
        "exit_fills": performance.exit_fills,
        "exit_qty": performance.exit_qty,
        "exit_cashflow_quote": performance.exit_cashflow_quote,
        "gross_spread_quote": performance.gross_spread_quote,
        "fee_quote": performance.fee_quote,
        "rebate_quote": performance.rebate_quote,
        "execution_costs_unavailable": performance.execution_costs_unavailable,
        "funding_unattributed": performance.funding_unattributed,
        "funding_coverage_gap": performance.funding_coverage_gap,
        "funding_quote": performance.funding_quote,
        "funding_available": performance.funding_available,
        "net_pnl_complete": performance.net_pnl_complete,
        "exit_cost_quote": performance.exit_cost_quote,
        "inventory_mtm_change_quote": performance.inventory_mtm_change_quote,
        "net_pnl_quote": performance.net_pnl_quote,
        "position": performance.position,
        "markouts": markouts,
        "time_weighted_uptime_pct": performance.quote_time.two_sided_uptime_pct,
        "observed_ms": performance.quote_time.observed_ms,
        "two_sided_ms": performance.quote_time.two_sided_ms,
        "eligible_bid_qty_ms": performance.quote_time.eligible_bid_qty_ms,
        "eligible_ask_qty_ms": performance.quote_time.eligible_ask_qty_ms,
        "eligible_total_qty_ms": performance.quote_time.eligible_total_qty_ms,
        "inventory_observed_ms": performance.inventory_time.observed_ms,
        "inventory_nonzero_ms": performance.inventory_time.nonzero_ms,
        "inventory_abs_qty_ms": performance.inventory_time.abs_qty_ms,
        "inventory_avg_abs_qty": performance.inventory_time.avg_abs_qty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACE: &str = r#"{"type":"header","schema_version":1,"symbol":"BTC-USD","git_sha":"abc","config_hash":"def","seed":7,"config":{"spread_bps":5.0,"band_bps":20.0,"level_step_bps":2.0,"refresh_bps":3.0,"levels":1,"size":1.0,"max_position":10.0,"skew_bps":0.0,"price_decimals":2,"qty_decimals":2,"min_order_qty":0.01},"settings":{"starting_position":0.0,"starting_mark":100.0,"max_divergence_bps":25.0,"require_full_touch":true,"vol_window":12,"vol_pause_bps":0.0,"active_exit_enabled":false,"inventory_exit_pct":0.0,"inventory_exit_qty":0.0}}
{"type":"cycle","event_time_ms":0,"cycle":0,"mark":100.0,"best_bid":99.99,"best_ask":100.01,"position":0.0,"eligible_bid_qty":1.0,"eligible_ask_qty":1.0}
{"type":"fill","trade_id":1,"order_id":10,"role":"passive_maker","side":"buy","price":99.95,"qty":1.0,"mark_at_fill":100.0,"event_time_ms":0,"costs":{"fee_quote":0.0,"rebate_quote":0.0}}
{"type":"cycle","event_time_ms":1000,"cycle":1,"mark":100.1,"best_bid":100.09,"best_ask":100.11,"position":1.0,"eligible_bid_qty":1.0,"eligible_ask_qty":1.0}
{"type":"finish","event_time_ms":1000}
"#;

    #[test]
    fn parses_and_replays_normalized_trace_without_external_state() {
        let trace = parse(BufReader::new(TRACE.as_bytes())).unwrap();
        let first = run_replay(
            &trace.config,
            trace.settings.clone(),
            &trace.events,
            trace.end_time_ms,
        )
        .unwrap();
        let second = run_replay(
            &trace.config,
            trace.settings.clone(),
            &trace.events,
            trace.end_time_ms,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.cycles.len(), 2);
        assert_eq!(first.performance.passive_fills, 1);
        assert_eq!(
            first.cycles[0].plan.as_ref().unwrap().actions,
            vec![
                Action::Place(DesiredQuote {
                    side: OrderSide::Buy,
                    level: 0,
                    price: 99.95,
                    qty: 1.0,
                }),
                Action::Place(DesiredQuote {
                    side: OrderSide::Sell,
                    level: 0,
                    price: 100.05,
                    qty: 1.0,
                }),
            ]
        );
        assert_eq!(
            first.cycles[1].plan.as_ref().unwrap().actions,
            vec![
                Action::Place(DesiredQuote {
                    side: OrderSide::Buy,
                    level: 0,
                    price: 100.04,
                    qty: 1.0,
                }),
                Action::Place(DesiredQuote {
                    side: OrderSide::Sell,
                    level: 0,
                    price: 100.16,
                    qty: 1.0,
                }),
            ]
        );
        assert!(first.cycles.iter().all(|cycle| {
            cycle
                .plan
                .as_ref()
                .is_some_and(|plan| plan.quote_geometry.len() == 2)
        }));
        // The expected bytes below are pinned literals that predate the
        // observation-only `quote_geometry` field. Asserting against them is
        // what proves geometry did not perturb the action sequence; comparing
        // a geometry-cleared clone against itself would prove nothing.
        let action_bytes: Vec<Vec<u8>> = first
            .cycles
            .iter()
            .flat_map(|cycle| cycle.plan.as_ref().unwrap().actions.iter())
            .map(|action| serde_json::to_vec(&action_json(action)).unwrap())
            .collect();
        assert_eq!(
            action_bytes,
            vec![
                br#"{"kind":"place","level":0,"price":99.95,"qty":1.0,"side":"buy"}"#.to_vec(),
                br#"{"kind":"place","level":0,"price":100.05,"qty":1.0,"side":"sell"}"#.to_vec(),
                br#"{"kind":"place","level":0,"price":100.04,"qty":1.0,"side":"buy"}"#.to_vec(),
                br#"{"kind":"place","level":0,"price":100.16,"qty":1.0,"side":"sell"}"#.to_vec(),
            ]
        );
    }

    /// docs/33: the `InventoryTrim` exit's *trigger* (`plan_cycle`'s
    /// `requested_inventory_exit`/`inventory_exit`) lives entirely in
    /// `standx-maker` and this change does not touch it — only how
    /// `standx-cli` executes the resulting `InventoryExit` changes (Market
    /// vs. the new Alo/Ioc path), and that execution machinery is never
    /// reached by replay at all (replay only drives `plan_cycle`, never live
    /// order submission). This test pins the trigger's exact output as a
    /// regression lock: if a future change to `plan_cycle`/`reconcile` ever
    /// perturbed the trigger, this fails independently of `alo_enabled`,
    /// which replay has no knob for in the first place.
    const EXIT_TRIGGER_TRACE: &str = r#"{"type":"header","schema_version":1,"symbol":"BTC-USD","git_sha":"abc","config_hash":"def","seed":7,"config":{"spread_bps":5.0,"band_bps":20.0,"level_step_bps":2.0,"refresh_bps":3.0,"levels":1,"size":1.0,"max_position":10.0,"skew_bps":0.0,"price_decimals":2,"qty_decimals":2,"min_order_qty":0.01},"settings":{"starting_position":6.0,"starting_mark":100.0,"max_divergence_bps":25.0,"require_full_touch":true,"vol_window":12,"vol_pause_bps":0.0,"active_exit_enabled":true,"inventory_exit_pct":50.0,"inventory_exit_qty":2.0}}
{"type":"cycle","event_time_ms":0,"cycle":0,"mark":100.0,"best_bid":99.99,"best_ask":100.01,"position":6.0,"eligible_bid_qty":0.0,"eligible_ask_qty":0.0}
{"type":"finish","event_time_ms":0}
"#;

    #[test]
    fn inventory_trim_trigger_is_pinned_and_untouched_by_the_alo_ioc_execution_path() {
        let trace = parse(BufReader::new(EXIT_TRIGGER_TRACE.as_bytes())).unwrap();
        let result = run_replay(
            &trace.config,
            trace.settings.clone(),
            &trace.events,
            trace.end_time_ms,
        )
        .unwrap();
        assert_eq!(result.cycles.len(), 1);
        let plan = result.cycles[0].plan.as_ref().unwrap();
        // Quotes are fully suppressed while an exit is outstanding (known
        // cost 4, unrelated to and predating stage 8).
        assert!(plan.actions.is_empty());
        assert_eq!(
            format!("{:?}", plan.requested_inventory_exit),
            "Some(InventoryExit { side: Sell, qty: 2.0, kind: InventoryTrim })"
        );
        assert_eq!(
            format!("{:?}", plan.inventory_exit),
            "Some(InventoryExit { side: Sell, qty: 2.0, kind: InventoryTrim })"
        );
        assert_eq!(plan.exit_suppression, None);
    }

    #[test]
    fn schema_v1_without_size_skew_still_parses_as_disabled() {
        let trace = parse(BufReader::new(TRACE.as_bytes())).unwrap();
        assert_eq!(trace.settings.size_skew, SizeSkewConfig::default());
    }

    #[test]
    fn rejects_unknown_fields_and_records_after_finish() {
        let unknown = TRACE.replace("\"seed\":7", "\"seed\":7,\"surprise\":true");
        assert!(parse(BufReader::new(unknown.as_bytes())).is_err());
        let trailing =
            format!("{TRACE}{{\"type\":\"funding\",\"event_time_ms\":2,\"cashflow_quote\":0.1}}\n");
        assert!(parse(BufReader::new(trailing.as_bytes())).is_err());
    }

    #[test]
    fn schema_v1_accepts_optional_duration_window_and_adaptive_config() {
        let adaptive = TRACE.replace(
            "\"vol_window\":12,",
            "\"vol_window_secs\":60,\"adaptive_spread\":{\"enabled\":true,\"min_spread_bps\":5.0,\"max_spread_bps\":10.0,\"tiers\":[{\"spread_bps\":5.0,\"refresh_bps\":3.0},{\"enter_vol_bps\":10.0,\"exit_vol_bps\":7.0,\"spread_bps\":10.0,\"refresh_bps\":4.0}]},",
        );
        let trace = parse(BufReader::new(adaptive.as_bytes())).unwrap();
        assert_eq!(trace.settings.vol_window_secs, Some(60));
        assert!(trace.settings.adaptive_spread.enabled);
        assert_eq!(trace.settings.adaptive_spread.tiers.len(), 2);
    }

    #[test]
    fn schema_v1_accepts_size_skew_config() {
        let configured = TRACE.replace(
            "\"vol_pause_bps\":0.0,",
            "\"vol_pause_bps\":0.0,\"size_skew\":{\"enabled\":true,\"activate_pct\":30.0,\"release_pct\":20.0,\"add_side_factor\":0.5},",
        );
        let trace = parse(BufReader::new(configured.as_bytes())).unwrap();
        assert!(trace.settings.size_skew.enabled);
        assert_eq!(trace.settings.size_skew.activate_pct, 30.0);
        assert_eq!(trace.settings.size_skew.release_pct, 20.0);
        assert_eq!(trace.settings.size_skew.add_side_factor, 0.5);
    }

    #[test]
    fn replay_rejects_sample_and_duration_windows_together() {
        let conflict = TRACE.replace(
            "\"vol_window\":12,",
            "\"vol_window\":12,\"vol_window_secs\":60,",
        );
        assert!(parse(BufReader::new(conflict.as_bytes())).is_err());
    }
}
