//! Pure maker runtime state transitions; the CLI executes queued effects.

use std::collections::VecDeque;

pub const MAX_CONSECUTIVE_CYCLE_ERRORS: u32 = 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RuntimePhase {
    Starting,
    Ready,
    Frozen { reason: String },
    Stopping { reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkKind {
    Cycle,
    Cleanup,
    Recovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkToken {
    pub generation: u64,
    pub kind: WorkKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryTarget {
    AccountStream,
    OrderResponse,
    PositionReconciliation,
    MarketData,
}

impl RecoveryTarget {
    pub fn label(self) -> &'static str {
        match self {
            Self::AccountStream => "account_stream",
            Self::OrderResponse => "order_response",
            Self::PositionReconciliation => "position_reconciliation",
            Self::MarketData => "market_data",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestTimeoutPhase {
    Acknowledgement,
    AccountOrder,
}

impl RequestTimeoutPhase {
    /// Pure safety policy: recover the stream responsible for the missing
    /// half of the request lifecycle.
    pub fn recovery_target(self) -> RecoveryTarget {
        match self {
            Self::Acknowledgement => RecoveryTarget::OrderResponse,
            Self::AccountOrder => RecoveryTarget::AccountStream,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Acknowledgement => "acknowledgement",
            Self::AccountOrder => "account_order",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeStopReason {
    CtrlC,
    OrderResponse(String),
    PositionReconciliation(String),
    MarketData(String),
    ConsecutiveCycleErrors(String),
    StopLoss(String),
    /// An account-level hard floor (equity / available margin) was breached.
    /// Distinct from `StopLoss`: solvency, not strategy performance.
    AccountFloor(String),
    AccountingInvariant(String),
    CleanupFailure {
        target: RecoveryTarget,
        reason: String,
    },
}

impl RuntimeStopReason {
    fn detail(&self) -> String {
        match self {
            Self::CtrlC => "Ctrl+C".to_string(),
            Self::OrderResponse(reason)
            | Self::PositionReconciliation(reason)
            | Self::MarketData(reason)
            | Self::ConsecutiveCycleErrors(reason)
            | Self::StopLoss(reason)
            | Self::AccountFloor(reason)
            | Self::AccountingInvariant(reason) => reason.clone(),
            Self::CleanupFailure { reason, .. } => reason.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MakerEvent {
    StartupReady,
    Timer,
    MarketChanged,
    CycleCompleted(WorkToken),
    CycleInvalidated {
        reason: String,
    },
    CycleFailed {
        token: WorkToken,
        reason: String,
    },
    AccountStreamDisconnected(String),
    OrderResponseDisconnected(String),
    PositionMismatch,
    MarketDataDegraded(String),
    CleanupCompleted(WorkToken),
    CleanupFailed {
        token: WorkToken,
        reason: String,
    },
    RecoverySucceeded(WorkToken),
    RecoveryFailed {
        token: WorkToken,
        reason: String,
    },
    OrderResponseUnmatched {
        request_id: String,
    },
    OrderCancelRejected {
        request_id: String,
        code: i64,
        message: String,
    },
    StopRequested(RuntimeStopReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MakerEffect {
    RunCycle(WorkToken),
    AbortInFlight(WorkToken),
    CommitCycle(WorkToken),
    Cleanup {
        token: WorkToken,
        target: RecoveryTarget,
    },
    Recover {
        token: WorkToken,
        target: RecoveryTarget,
    },
    Stop(RuntimeStopReason),
}

#[derive(Debug)]
pub struct MakerState {
    phase: RuntimePhase,
    generation: u64,
    in_flight: Option<WorkToken>,
    recovery_target: Option<RecoveryTarget>,
    replan_requested: bool,
    consecutive_cycle_errors: u32,
    effects: VecDeque<MakerEffect>,
}

impl MakerState {
    pub fn starting() -> Self {
        Self {
            phase: RuntimePhase::Starting,
            generation: 0,
            in_flight: None,
            recovery_target: None,
            replan_requested: false,
            consecutive_cycle_errors: 0,
            effects: VecDeque::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn phase(&self) -> &RuntimePhase {
        &self.phase
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    #[cfg(test)]
    pub(crate) fn is_frozen(&self) -> bool {
        matches!(self.phase, RuntimePhase::Frozen { .. })
    }

    /// Applies an event and retains every resulting effect until the CLI
    /// explicitly drains it with [`Self::next_effect`].
    pub fn handle(&mut self, event: MakerEvent) {
        let effects = self.transition(event);
        self.effects.extend(effects);
    }

    pub fn next_effect(&mut self) -> Option<MakerEffect> {
        self.effects.pop_front()
    }

    pub fn pending_effect(&self) -> Option<&MakerEffect> {
        self.effects.front()
    }

    pub fn consecutive_cycle_errors(&self) -> u32 {
        self.consecutive_cycle_errors
    }

    fn transition(&mut self, event: MakerEvent) -> Vec<MakerEffect> {
        match event {
            MakerEvent::StartupReady => {
                if !matches!(self.phase, RuntimePhase::Starting) {
                    return Vec::new();
                }
                self.phase = RuntimePhase::Ready;
                self.request_cycle()
            }
            MakerEvent::Timer | MakerEvent::MarketChanged => {
                if !matches!(self.phase, RuntimePhase::Ready) {
                    return Vec::new();
                }
                if self.in_flight.is_some() {
                    self.replan_requested = true;
                    Vec::new()
                } else {
                    self.request_cycle()
                }
            }
            MakerEvent::CycleCompleted(token) => {
                if !self.matches_in_flight(token, WorkKind::Cycle) {
                    return Vec::new();
                }
                self.in_flight = None;
                self.consecutive_cycle_errors = 0;
                let mut effects = vec![MakerEffect::CommitCycle(token)];
                if self.replan_requested && matches!(self.phase, RuntimePhase::Ready) {
                    self.replan_requested = false;
                    effects.extend(self.request_cycle());
                }
                effects
            }
            MakerEvent::CycleInvalidated { reason } => {
                self.freeze(reason, RecoveryTarget::PositionReconciliation)
            }
            MakerEvent::CycleFailed { token, reason } => {
                if !self.matches_in_flight(token, WorkKind::Cycle) {
                    return Vec::new();
                }
                self.in_flight = None;
                self.consecutive_cycle_errors = self.consecutive_cycle_errors.saturating_add(1);
                if self.consecutive_cycle_errors >= MAX_CONSECUTIVE_CYCLE_ERRORS {
                    self.stop(RuntimeStopReason::ConsecutiveCycleErrors(reason))
                } else {
                    Vec::new()
                }
            }
            MakerEvent::AccountStreamDisconnected(reason) => {
                self.freeze(reason, RecoveryTarget::AccountStream)
            }
            MakerEvent::OrderResponseDisconnected(reason) => {
                self.freeze(reason, RecoveryTarget::OrderResponse)
            }
            MakerEvent::PositionMismatch => self.freeze(
                "position mismatch".to_string(),
                RecoveryTarget::PositionReconciliation,
            ),
            MakerEvent::MarketDataDegraded(reason) => {
                self.freeze(reason, RecoveryTarget::MarketData)
            }
            MakerEvent::CleanupCompleted(token) => {
                if !self.matches_in_flight(token, WorkKind::Cleanup) {
                    return Vec::new();
                }
                let Some(target) = self.recovery_target else {
                    return self.stop(RuntimeStopReason::CleanupFailure {
                        target: RecoveryTarget::PositionReconciliation,
                        reason: "cleanup completed without a recovery target".to_string(),
                    });
                };
                let token = WorkToken {
                    generation: self.generation,
                    kind: WorkKind::Recovery,
                };
                self.in_flight = Some(token);
                vec![MakerEffect::Recover { token, target }]
            }
            MakerEvent::CleanupFailed { token, reason } => {
                if !self.matches_in_flight(token, WorkKind::Cleanup) {
                    return Vec::new();
                }
                self.in_flight = None;
                self.stop(RuntimeStopReason::CleanupFailure {
                    target: self
                        .recovery_target
                        .unwrap_or(RecoveryTarget::PositionReconciliation),
                    reason,
                })
            }
            MakerEvent::RecoverySucceeded(token) => {
                if !self.matches_in_flight(token, WorkKind::Recovery) {
                    return Vec::new();
                }
                self.in_flight = None;
                self.recovery_target = None;
                self.consecutive_cycle_errors = 0;
                self.phase = RuntimePhase::Ready;
                self.request_cycle()
            }
            MakerEvent::RecoveryFailed { token, reason } => {
                if !self.matches_in_flight(token, WorkKind::Recovery) {
                    return Vec::new();
                }
                self.in_flight = None;
                let stop = match self.recovery_target {
                    Some(RecoveryTarget::OrderResponse) => RuntimeStopReason::OrderResponse(reason),
                    Some(RecoveryTarget::MarketData) => RuntimeStopReason::MarketData(reason),
                    _ => RuntimeStopReason::PositionReconciliation(reason),
                };
                self.stop(stop)
            }
            MakerEvent::OrderResponseUnmatched { request_id } => self.freeze(
                format!("unexpected order-response request ID {request_id}"),
                RecoveryTarget::OrderResponse,
            ),
            MakerEvent::OrderCancelRejected {
                request_id,
                code,
                message,
            } => self.freeze(
                order_cancel_rejection_reason(&request_id, code, &message),
                RecoveryTarget::OrderResponse,
            ),
            MakerEvent::StopRequested(reason) => self.stop(reason),
        }
    }

    fn matches_in_flight(&self, token: WorkToken, kind: WorkKind) -> bool {
        // `in_flight` is only ever set with the current generation and is taken
        // on every generation bump, so `self.in_flight == Some(token)` already
        // implies the generation matches; only the kind still needs checking.
        token.kind == kind && self.in_flight == Some(token)
    }

    fn request_cycle(&mut self) -> Vec<MakerEffect> {
        let token = WorkToken {
            generation: self.generation,
            kind: WorkKind::Cycle,
        };
        self.in_flight = Some(token);
        vec![MakerEffect::RunCycle(token)]
    }

    fn freeze(&mut self, reason: String, target: RecoveryTarget) -> Vec<MakerEffect> {
        if matches!(
            self.phase,
            RuntimePhase::Frozen { .. } | RuntimePhase::Stopping { .. }
        ) {
            return Vec::new();
        }
        self.effects.clear();
        self.generation = self.generation.saturating_add(1);
        self.replan_requested = false;
        let mut effects = self.abort_effect();
        self.phase = RuntimePhase::Frozen { reason };
        self.recovery_target = Some(target);
        let token = WorkToken {
            generation: self.generation,
            kind: WorkKind::Cleanup,
        };
        self.in_flight = Some(token);
        effects.push(MakerEffect::Cleanup { token, target });
        effects
    }

    fn stop(&mut self, reason: RuntimeStopReason) -> Vec<MakerEffect> {
        if matches!(self.phase, RuntimePhase::Stopping { .. }) {
            return Vec::new();
        }
        self.effects.clear();
        self.generation = self.generation.saturating_add(1);
        self.replan_requested = false;
        self.recovery_target = None;
        let mut effects = self.abort_effect();
        self.phase = RuntimePhase::Stopping {
            reason: reason.detail(),
        };
        effects.push(MakerEffect::Stop(reason));
        effects
    }

    fn abort_effect(&mut self) -> Vec<MakerEffect> {
        self.in_flight
            .take()
            .map(MakerEffect::AbortInFlight)
            .into_iter()
            .collect()
    }
}

pub fn order_cancel_rejection_reason(request_id: &str, code: i64, message: &str) -> String {
    format!(
        "order-response cancel rejected for request {request_id}: code={code} message={message:?}; refusing further live orders"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next_cycle(state: &mut MakerState) -> WorkToken {
        match state.next_effect().expect("cycle effect") {
            MakerEffect::RunCycle(token) => token,
            effect => panic!("expected cycle, got {effect:?}"),
        }
    }

    #[test]
    fn effects_are_retained_until_explicitly_drained() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let token = next_cycle(&mut state);
        assert_eq!(token.kind, WorkKind::Cycle);
        assert!(state.next_effect().is_none());
    }

    #[test]
    fn critical_events_abort_stale_work_and_clean_up() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let token = next_cycle(&mut state);
        state.handle(MakerEvent::PositionMismatch);
        assert_eq!(state.generation(), 1);
        assert!(matches!(state.phase(), RuntimePhase::Frozen { .. }));
        assert_eq!(state.next_effect(), Some(MakerEffect::AbortInFlight(token)));
        let cleanup = match state.next_effect().expect("cleanup") {
            MakerEffect::Cleanup { token, target } => {
                assert_eq!(target, RecoveryTarget::PositionReconciliation);
                token
            }
            effect => panic!("expected cleanup, got {effect:?}"),
        };
        state.handle(MakerEvent::CycleCompleted(token));
        assert!(state.next_effect().is_none());
        state.handle(MakerEvent::CleanupCompleted(cleanup));
        assert!(matches!(
            state.next_effect(),
            Some(MakerEffect::Recover {
                target: RecoveryTarget::PositionReconciliation,
                ..
            })
        ));
    }

    #[test]
    fn frozen_ignores_timer_and_market_changed() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let token = next_cycle(&mut state);
        // Freeze on a position mismatch and drain the abort + cleanup it queues.
        state.handle(MakerEvent::PositionMismatch);
        assert!(state.is_frozen());
        while state.next_effect().is_some() {}

        // While frozen, market ticks must not schedule any new cycle work or
        // arm a replan — the recovery flow owns the state until it completes.
        state.handle(MakerEvent::Timer);
        state.handle(MakerEvent::MarketChanged);
        assert_eq!(state.next_effect(), None);
        assert!(state.is_frozen());
        let _ = token;
    }

    #[test]
    fn coalesced_timer_commits_then_replans() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let token = next_cycle(&mut state);
        state.handle(MakerEvent::Timer);
        state.handle(MakerEvent::MarketChanged);
        assert!(state.next_effect().is_none());
        state.handle(MakerEvent::CycleCompleted(token));
        assert_eq!(state.next_effect(), Some(MakerEffect::CommitCycle(token)));
        assert!(matches!(
            state.next_effect(),
            Some(MakerEffect::RunCycle(_))
        ));
    }

    #[test]
    fn each_recovery_target_runs_only_after_cleanup() {
        for (event, expected_target) in [
            (
                MakerEvent::AccountStreamDisconnected("closed".to_string()),
                RecoveryTarget::AccountStream,
            ),
            (
                MakerEvent::OrderResponseDisconnected("closed".to_string()),
                RecoveryTarget::OrderResponse,
            ),
            (
                MakerEvent::PositionMismatch,
                RecoveryTarget::PositionReconciliation,
            ),
            (
                MakerEvent::MarketDataDegraded("feed idle".to_string()),
                RecoveryTarget::MarketData,
            ),
        ] {
            let mut state = MakerState::starting();
            state.handle(MakerEvent::StartupReady);
            let _ = next_cycle(&mut state);
            state.handle(event);
            let _ = state.next_effect();
            let cleanup = match state.next_effect().expect("cleanup") {
                MakerEffect::Cleanup { token, target } => {
                    assert_eq!(target, expected_target);
                    token
                }
                effect => panic!("expected cleanup, got {effect:?}"),
            };
            state.handle(MakerEvent::CleanupCompleted(cleanup));
            let recovery = match state.next_effect().expect("recovery") {
                MakerEffect::Recover { token, target } => {
                    assert_eq!(target, expected_target);
                    token
                }
                effect => panic!("expected recovery, got {effect:?}"),
            };
            state.handle(MakerEvent::RecoverySucceeded(recovery));
            assert!(matches!(
                state.next_effect(),
                Some(MakerEffect::RunCycle(_))
            ));
            assert!(matches!(state.phase(), RuntimePhase::Ready));
        }
    }

    #[test]
    fn request_timeout_phase_selects_the_responsible_stream() {
        assert_eq!(
            RequestTimeoutPhase::Acknowledgement.recovery_target(),
            RecoveryTarget::OrderResponse
        );
        assert_eq!(
            RequestTimeoutPhase::AccountOrder.recovery_target(),
            RecoveryTarget::AccountStream
        );
    }

    #[test]
    fn cleanup_or_recovery_failure_stops() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let _ = next_cycle(&mut state);
        state.handle(MakerEvent::PositionMismatch);
        let _ = state.next_effect();
        let cleanup = match state.next_effect().unwrap() {
            MakerEffect::Cleanup { token, .. } => token,
            _ => unreachable!(),
        };
        state.handle(MakerEvent::CleanupFailed {
            token: cleanup,
            reason: "residual orders".to_string(),
        });
        assert!(matches!(state.next_effect(), Some(MakerEffect::Stop(_))));

        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let _ = next_cycle(&mut state);
        state.handle(MakerEvent::AccountStreamDisconnected("closed".to_string()));
        let _ = state.next_effect();
        let cleanup = match state.next_effect().unwrap() {
            MakerEffect::Cleanup { token, .. } => token,
            _ => unreachable!(),
        };
        state.handle(MakerEvent::CleanupCompleted(cleanup));
        let recovery = match state.next_effect().unwrap() {
            MakerEffect::Recover { token, .. } => token,
            _ => unreachable!(),
        };
        state.handle(MakerEvent::RecoveryFailed {
            token: recovery,
            reason: "timeout".to_string(),
        });
        assert!(matches!(state.next_effect(), Some(MakerEffect::Stop(_))));
    }

    #[test]
    fn third_cycle_failure_stops_and_stale_failures_are_ignored() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        for attempt in 0..3 {
            let token = next_cycle(&mut state);
            state.handle(MakerEvent::CycleFailed {
                token,
                reason: format!("failure {attempt}"),
            });
            state.handle(MakerEvent::CycleFailed {
                token,
                reason: "stale duplicate".to_string(),
            });
            if attempt < 2 {
                assert!(state.pending_effect().is_none());
                state.handle(MakerEvent::Timer);
            }
        }
        assert!(matches!(state.next_effect(), Some(MakerEffect::Stop(_))));
    }

    #[test]
    fn ctrl_c_aborts_and_stops() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let token = next_cycle(&mut state);
        state.handle(MakerEvent::StopRequested(RuntimeStopReason::CtrlC));
        assert_eq!(state.next_effect(), Some(MakerEffect::AbortInFlight(token)));
        assert_eq!(
            state.next_effect(),
            Some(MakerEffect::Stop(RuntimeStopReason::CtrlC))
        );
    }

    #[test]
    fn unmatched_response_fails_closed_immediately() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let _ = next_cycle(&mut state);
        state.handle(MakerEvent::OrderResponseUnmatched {
            request_id: "unknown".to_string(),
        });
        assert!(state.is_frozen());
        assert!(matches!(
            state.next_effect(),
            Some(MakerEffect::AbortInFlight(_))
        ));
        assert!(matches!(
            state.next_effect(),
            Some(MakerEffect::Cleanup {
                target: RecoveryTarget::OrderResponse,
                ..
            })
        ));
    }

    #[test]
    fn rejected_cancel_fails_closed_through_order_response_recovery() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let _ = next_cycle(&mut state);
        state.handle(MakerEvent::OrderCancelRejected {
            request_id: "cancel-1".to_string(),
            code: 400,
            message: "rejected".to_string(),
        });
        assert!(matches!(
            state.phase(),
            RuntimePhase::Frozen { reason } if reason.contains("cancel-1") && reason.contains("code=400")
        ));
        assert!(matches!(
            state.next_effect(),
            Some(MakerEffect::AbortInFlight(_))
        ));
        assert!(matches!(
            state.next_effect(),
            Some(MakerEffect::Cleanup {
                target: RecoveryTarget::OrderResponse,
                ..
            })
        ));
    }

    /// Every event that must freeze the runtime, paired with the recovery
    /// target its cleanup and recovery effects must carry. Conformance tests
    /// below iterate this table so a new fault variant that misses one of the
    /// shared safety steps fails here rather than drifting silently.
    fn freeze_cases() -> Vec<(MakerEvent, RecoveryTarget)> {
        vec![
            (
                MakerEvent::AccountStreamDisconnected("stream closed".to_string()),
                RecoveryTarget::AccountStream,
            ),
            (
                MakerEvent::OrderResponseDisconnected("stream closed".to_string()),
                RecoveryTarget::OrderResponse,
            ),
            (
                MakerEvent::PositionMismatch,
                RecoveryTarget::PositionReconciliation,
            ),
            (
                MakerEvent::OrderResponseUnmatched {
                    request_id: "req-1".to_string(),
                },
                RecoveryTarget::OrderResponse,
            ),
            (
                MakerEvent::OrderCancelRejected {
                    request_id: "cancel-1".to_string(),
                    code: 400,
                    message: "rejected".to_string(),
                },
                RecoveryTarget::OrderResponse,
            ),
            (
                MakerEvent::CycleInvalidated {
                    reason: "account state changed".to_string(),
                },
                RecoveryTarget::PositionReconciliation,
            ),
        ]
    }

    fn stop_reasons() -> Vec<RuntimeStopReason> {
        vec![
            RuntimeStopReason::CtrlC,
            RuntimeStopReason::OrderResponse("boom".to_string()),
            RuntimeStopReason::PositionReconciliation("boom".to_string()),
            RuntimeStopReason::MarketData("boom".to_string()),
            RuntimeStopReason::ConsecutiveCycleErrors("boom".to_string()),
            RuntimeStopReason::StopLoss("boom".to_string()),
            RuntimeStopReason::AccountingInvariant("boom".to_string()),
            RuntimeStopReason::CleanupFailure {
                target: RecoveryTarget::OrderResponse,
                reason: "boom".to_string(),
            },
        ]
    }

    /// Freezes a fresh runtime with `event` and returns the cleanup token,
    /// asserting the queued effects are exactly AbortInFlight → Cleanup.
    fn freeze_and_take_cleanup(
        event: MakerEvent,
        target: RecoveryTarget,
    ) -> (MakerState, WorkToken) {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let cycle = next_cycle(&mut state);
        state.handle(event);
        assert_eq!(state.next_effect(), Some(MakerEffect::AbortInFlight(cycle)));
        let cleanup = match state.next_effect().expect("cleanup effect") {
            MakerEffect::Cleanup {
                token,
                target: actual,
            } => {
                assert_eq!(actual, target);
                token
            }
            effect => panic!("expected cleanup, got {effect:?}"),
        };
        assert_eq!(state.next_effect(), None);
        (state, cleanup)
    }

    #[test]
    fn every_fault_clears_queued_work_and_freezes_with_abort_then_cleanup() {
        for (event, target) in freeze_cases() {
            let mut state = MakerState::starting();
            // Leave StartupReady's RunCycle effect *undrained* so the test
            // proves a fault clears stale queued work instead of leaving a
            // placement effect behind the abort/cleanup pair.
            state.handle(MakerEvent::StartupReady);
            let queued = match state.pending_effect() {
                Some(MakerEffect::RunCycle(token)) => *token,
                effect => panic!("expected queued cycle, got {effect:?}"),
            };
            state.handle(event.clone());
            assert!(state.is_frozen(), "{event:?} must freeze the runtime");
            assert_eq!(
                state.next_effect(),
                Some(MakerEffect::AbortInFlight(queued)),
                "{event:?} must abort in-flight work first"
            );
            assert!(
                matches!(
                    state.next_effect(),
                    Some(MakerEffect::Cleanup { target: actual, .. }) if actual == target
                ),
                "{event:?} must queue cleanup for {target:?}"
            );
            assert_eq!(
                state.next_effect(),
                None,
                "{event:?} must not leave stale effects behind cleanup"
            );
        }
    }

    #[test]
    fn accounting_invariant_stop_clears_queued_cycle() {
        let mut state = MakerState::starting();
        // Leave StartupReady's RunCycle effect *undrained* so the test proves
        // an accounting-invariant stop clears stale queued work instead of
        // leaving a placement effect behind the abort/stop pair.
        state.handle(MakerEvent::StartupReady);
        let queued = match state.pending_effect() {
            Some(MakerEffect::RunCycle(token)) => *token,
            effect => panic!("expected queued cycle, got {effect:?}"),
        };
        let reason = RuntimeStopReason::AccountingInvariant("boom".to_string());
        state.handle(MakerEvent::StopRequested(reason.clone()));
        assert_eq!(
            state.next_effect(),
            Some(MakerEffect::AbortInFlight(queued))
        );
        assert_eq!(state.next_effect(), Some(MakerEffect::Stop(reason)));
        assert_eq!(
            state.next_effect(),
            None,
            "queued RunCycle must not survive the accounting-invariant stop"
        );
    }

    #[test]
    fn frozen_runtime_ignores_market_events_and_later_faults() {
        for (event, target) in freeze_cases() {
            let (mut state, cleanup) = freeze_and_take_cleanup(event.clone(), target);
            state.handle(MakerEvent::Timer);
            state.handle(MakerEvent::MarketChanged);
            for (other, _) in freeze_cases() {
                state.handle(other);
            }
            assert_eq!(
                state.next_effect(),
                None,
                "no effect may be queued while frozen for {event:?}"
            );
            // The original fault still owns the recovery flow.
            state.handle(MakerEvent::CleanupCompleted(cleanup));
            assert!(
                matches!(
                    state.next_effect(),
                    Some(MakerEffect::Recover { target: actual, .. }) if actual == target
                ),
                "recovery after {event:?} must keep target {target:?}"
            );
        }
    }

    #[test]
    fn stale_cleanup_and_recovery_tokens_are_rejected() {
        for (event, target) in freeze_cases() {
            let (mut state, cleanup) = freeze_and_take_cleanup(event.clone(), target);
            let stale = WorkToken {
                generation: cleanup.generation + 7,
                kind: cleanup.kind,
            };
            state.handle(MakerEvent::CleanupCompleted(stale));
            state.handle(MakerEvent::CleanupFailed {
                token: stale,
                reason: "stale".to_string(),
            });
            assert_eq!(state.next_effect(), None);
            assert!(state.is_frozen());

            state.handle(MakerEvent::CleanupCompleted(cleanup));
            let recovery = match state.next_effect().expect("recovery effect") {
                MakerEffect::Recover { token, .. } => token,
                effect => panic!("expected recovery, got {effect:?}"),
            };
            let stale = WorkToken {
                generation: recovery.generation + 7,
                kind: recovery.kind,
            };
            state.handle(MakerEvent::RecoverySucceeded(stale));
            state.handle(MakerEvent::RecoveryFailed {
                token: stale,
                reason: "stale".to_string(),
            });
            assert_eq!(state.next_effect(), None);
            assert!(state.is_frozen());
        }
    }

    #[test]
    fn recovery_failure_stop_reason_maps_by_target() {
        for (event, target) in freeze_cases() {
            let (mut state, cleanup) = freeze_and_take_cleanup(event.clone(), target);
            state.handle(MakerEvent::CleanupCompleted(cleanup));
            let recovery = match state.next_effect().expect("recovery effect") {
                MakerEffect::Recover { token, .. } => token,
                effect => panic!("expected recovery, got {effect:?}"),
            };
            state.handle(MakerEvent::RecoveryFailed {
                token: recovery,
                reason: "reconnect exhausted".to_string(),
            });
            let stop = match state.next_effect().expect("stop effect") {
                MakerEffect::Stop(reason) => reason,
                effect => panic!("expected stop, got {effect:?}"),
            };
            match target {
                RecoveryTarget::OrderResponse => assert_eq!(
                    stop,
                    RuntimeStopReason::OrderResponse("reconnect exhausted".to_string()),
                    "order-response recovery failure must stop as OrderResponse for {event:?}"
                ),
                RecoveryTarget::AccountStream | RecoveryTarget::PositionReconciliation => {
                    assert_eq!(
                        stop,
                        RuntimeStopReason::PositionReconciliation(
                            "reconnect exhausted".to_string()
                        ),
                        "recovery failure must stop as PositionReconciliation for {event:?}"
                    )
                }
                RecoveryTarget::MarketData => assert_eq!(
                    stop,
                    RuntimeStopReason::MarketData("reconnect exhausted".to_string()),
                    "market-data recovery failure must stop as MarketData for {event:?}"
                ),
            }
        }
    }

    #[test]
    fn cleanup_failure_stop_carries_the_recovery_target() {
        for (event, target) in freeze_cases() {
            let (mut state, cleanup) = freeze_and_take_cleanup(event.clone(), target);
            state.handle(MakerEvent::CleanupFailed {
                token: cleanup,
                reason: "residual orders".to_string(),
            });
            assert_eq!(
                state.next_effect(),
                Some(MakerEffect::Stop(RuntimeStopReason::CleanupFailure {
                    target,
                    reason: "residual orders".to_string(),
                })),
                "cleanup failure must stop with CleanupFailure {target:?} for {event:?}"
            );
        }
    }

    #[test]
    fn stop_requested_passes_the_reason_through_and_seals_the_runtime() {
        for reason in stop_reasons() {
            let mut state = MakerState::starting();
            state.handle(MakerEvent::StartupReady);
            let cycle = next_cycle(&mut state);
            state.handle(MakerEvent::StopRequested(reason.clone()));
            assert_eq!(state.next_effect(), Some(MakerEffect::AbortInFlight(cycle)));
            assert_eq!(state.next_effect(), Some(MakerEffect::Stop(reason.clone())));
            // Once stopping, nothing may queue further work or a second stop.
            state.handle(MakerEvent::Timer);
            state.handle(MakerEvent::StopRequested(RuntimeStopReason::CtrlC));
            for (fault, _) in freeze_cases() {
                state.handle(fault);
            }
            assert_eq!(
                state.next_effect(),
                None,
                "stopping runtime must ignore all later events after {reason:?}"
            );
        }
    }

    #[test]
    fn recovery_success_resets_the_cycle_error_streak() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        for attempt in 0..2 {
            let token = next_cycle(&mut state);
            state.handle(MakerEvent::CycleFailed {
                token,
                reason: format!("failure {attempt}"),
            });
            assert!(state.pending_effect().is_none());
            state.handle(MakerEvent::Timer);
        }
        let _ = next_cycle(&mut state);
        state.handle(MakerEvent::PositionMismatch);
        let _ = state.next_effect();
        let cleanup = match state.next_effect().expect("cleanup effect") {
            MakerEffect::Cleanup { token, .. } => token,
            effect => panic!("expected cleanup, got {effect:?}"),
        };
        state.handle(MakerEvent::CleanupCompleted(cleanup));
        let recovery = match state.next_effect().expect("recovery effect") {
            MakerEffect::Recover { token, .. } => token,
            effect => panic!("expected recovery, got {effect:?}"),
        };
        state.handle(MakerEvent::RecoverySucceeded(recovery));
        // A third consecutive failure would have stopped the runtime had the
        // streak survived recovery; a fresh failure must not.
        let token = next_cycle(&mut state);
        state.handle(MakerEvent::CycleFailed {
            token,
            reason: "post-recovery failure".to_string(),
        });
        assert!(
            state.pending_effect().is_none(),
            "recovery must reset the consecutive cycle error streak"
        );
    }

    #[test]
    fn invalidated_cycle_aborts_cleans_up_and_rejects_stale_completion() {
        let mut state = MakerState::starting();
        state.handle(MakerEvent::StartupReady);
        let token = next_cycle(&mut state);
        state.handle(MakerEvent::CycleInvalidated {
            reason: "account state changed during cycle".to_string(),
        });
        assert_eq!(state.generation(), token.generation + 1);
        assert_eq!(state.next_effect(), Some(MakerEffect::AbortInFlight(token)));
        assert!(matches!(
            state.next_effect(),
            Some(MakerEffect::Cleanup {
                target: RecoveryTarget::PositionReconciliation,
                ..
            })
        ));
        assert_eq!(state.consecutive_cycle_errors, 0);

        state.handle(MakerEvent::CycleCompleted(token));
        assert!(state.next_effect().is_none());
        assert!(matches!(state.phase(), RuntimePhase::Frozen { .. }));
    }
}
