//! Deterministic projection of the live maker account stream.
//!
//! The projection deliberately does not account fills. [`crate::MakerLedger`]
//! remains the only fill/PnL/position ingestion path; this module consumes the
//! resulting, already-deduplicated fill outcomes only to keep the projected
//! open quantity in sync.

use crate::{is_current_run_client_order_id, open_qty_adopts, RestingQuote};
use standx_sdk::models::OrderSide;
use std::collections::{HashMap, VecDeque};
use std::fmt;

pub const MAX_PENDING_ORDER_REQUESTS: usize = 256;

/// Recently cancelled current-run venue order IDs kept to recognize replayed
/// account-stream updates after the cancel request has been accepted. This is
/// deliberately bounded: older observations still fail closed rather than
/// turning a long-lived maker session into an unbounded trust cache.
const MAX_RETIRED_ORDER_IDS: usize = 512;

/// Recently completed command request IDs and their typed request metadata.
/// This keeps duplicate/late acknowledgements idempotent and lets a delayed
/// account-order update recover the exact accepted place after cleanup closes
/// its quote slot. The bound keeps long-running sessions from accumulating
/// unbounded correlation state; older replays continue to fail closed.
const MAX_COMPLETED_ORDER_REQUESTS: usize = 512;

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionPendingPlace {
    pub request_id: String,
    pub client_order_id: String,
    pub side: OrderSide,
    pub price: f64,
    pub qty: f64,
    pub level: u32,
    pub ref_center: f64,
    pub cycle: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionPendingCancel {
    pub request_id: String,
    pub order_id: u64,
    pub side: OrderSide,
    pub level: u32,
    pub price: f64,
    pub cycle: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionPendingRequest {
    Place(ProjectionPendingPlace),
    Cancel(ProjectionPendingCancel),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionRequestResolution {
    PlaceAccepted,
    PlaceRejected,
    CancelResolved,
}

/// Whether the order-response (placement) channel survived a verified maker
/// cleanup or was torn down and replaced. This is the *decision* a recovery
/// flow makes; [`MakerAccountProjection::finish_verified_cleanup`] maps it to
/// the mechanical projection reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderResponseContinuity {
    /// The order-response stream is still the same channel, so acknowledgements
    /// it has not yet delivered may still arrive and must stay correlated.
    Preserved,
    /// The order-response stream was replaced, so no acknowledgement for a
    /// request issued on the old channel can ever arrive; end those
    /// obligations as part of the cleanup.
    Replaced,
}

impl ProjectionRequestResolution {
    pub fn accepts_response(self, accepted: bool) -> bool {
        match self {
            Self::PlaceAccepted | Self::CancelResolved => accepted,
            Self::PlaceRejected => !accepted,
        }
    }
}

impl ProjectionPendingRequest {
    pub fn request_id(&self) -> &str {
        match self {
            Self::Place(pending) => &pending.request_id,
            Self::Cancel(pending) => &pending.request_id,
        }
    }

    pub fn operation(&self) -> RequestOperation {
        match self {
            Self::Place(_) => RequestOperation::Place,
            Self::Cancel(_) => RequestOperation::Cancel,
        }
    }
}

/// Which order-affecting command a request carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOperation {
    Place,
    Cancel,
}

impl RequestOperation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Place => "place",
            Self::Cancel => "cancel",
        }
    }
}

/// How far a request has progressed through its two independent lifecycles.
///
/// A request is registered before its network write, so "registered but never
/// acknowledged" is a state the maker can observe and report — which is what
/// distinguishes a lost write from a venue that never answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestLifecycle {
    /// Registered and awaiting its command acknowledgement.
    AwaitingAck,
    /// Acknowledged, but its venue exposure (the quote slot) is still open —
    /// an accepted place whose account-order observation has not arrived.
    AwaitingVenue,
    /// Both halves resolved; only a bounded tombstone remains.
    Retired,
    /// The registry has no record of the request at all.
    Unknown,
}

impl RequestLifecycle {
    pub fn label(self) -> &'static str {
        match self {
            Self::AwaitingAck => "awaiting_ack",
            Self::AwaitingVenue => "awaiting_venue",
            Self::Retired => "retired",
            Self::Unknown => "unknown",
        }
    }
}

/// What an observed order-response acknowledgement turned out to be.
///
/// This replaces a bare `matched: bool`. That boolean collapsed four materially
/// different situations into one "unexpected request_id" diagnostic, so an
/// operator reading a fail-closed stop could not tell a protocol violation from
/// a late-but-consistent duplicate, or a contradicted local view from an ID this
/// run never minted. Every variant below names one of those situations, and
/// [`ResponseCorrelation::fails_closed`] is the single place the safety policy
/// for each is written down.
///
/// Note on run scope: the SDK mints `request_id` as a bare UUIDv4 with no run or
/// generation marker, so an acknowledgement from a *different run* that somehow
/// reached this stream is indistinguishable from [`Self::Orphan`]. Separating
/// those two would require run-scoped request identities minted in `standx-sdk`,
/// the way client order IDs already carry a run prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseCorrelation {
    /// Matched a request still awaiting its acknowledgement.
    Matched {
        operation: RequestOperation,
        lifecycle: RequestLifecycle,
    },
    /// The request is already resolved and this acknowledgement agrees with the
    /// recorded outcome: a duplicate or delayed delivery, idempotent to drop.
    ///
    /// `resolved_in` is the generation that resolved it, which may predate the
    /// current one — a replayed ack crossing an account-stream reconnect is the
    /// expected case, not a fault. It is carried for diagnostics only: a
    /// duplicate of something already applied stays safe to drop whatever epoch
    /// resolved it, because dropping it changes nothing.
    LateKnown {
        operation: RequestOperation,
        resolution: ProjectionRequestResolution,
        resolved_in: u64,
        lifecycle: RequestLifecycle,
    },
    /// The request is resolved but this acknowledgement contradicts the recorded
    /// outcome — the local view and the venue's cannot both be right.
    Contradictory {
        operation: RequestOperation,
        resolution: ProjectionRequestResolution,
        resolved_in: u64,
        lifecycle: RequestLifecycle,
    },
    /// A well-formed `request_id` this run's registry has never held — neither
    /// pending nor a tombstone. Either the venue invented it or our registration
    /// was lost before the write.
    Orphan,
    /// The request is registered and its acknowledgement already resolved, but
    /// the tombstone recording *how* it resolved has been evicted from the
    /// bounded history. The acknowledgement cannot be checked for consistency,
    /// and "cannot check" is not "consistent".
    Unverifiable,
    /// No `request_id` at all. The protocol requires one on every command
    /// acknowledgement, so this is a protocol violation, not an unknown ID.
    Unidentified,
}

impl ResponseCorrelation {
    /// Stable snake_case name for structured diagnostics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Matched { .. } => "matched",
            Self::LateKnown { .. } => "late_known",
            Self::Contradictory { .. } => "contradictory",
            Self::Orphan => "orphan_current_run",
            Self::Unverifiable => "unverifiable",
            Self::Unidentified => "unidentified",
        }
    }

    pub fn operation(&self) -> Option<RequestOperation> {
        match self {
            Self::Matched { operation, .. }
            | Self::LateKnown { operation, .. }
            | Self::Contradictory { operation, .. } => Some(*operation),
            Self::Orphan | Self::Unverifiable | Self::Unidentified => None,
        }
    }

    pub fn lifecycle(&self) -> RequestLifecycle {
        match self {
            Self::Matched { lifecycle, .. }
            | Self::LateKnown { lifecycle, .. }
            | Self::Contradictory { lifecycle, .. } => *lifecycle,
            Self::Unverifiable => RequestLifecycle::AwaitingVenue,
            Self::Orphan | Self::Unidentified => RequestLifecycle::Unknown,
        }
    }

    /// Whether observing this correlation must stop live order work.
    ///
    /// Deliberately *not* a blanket "unknown means ignore": only the two
    /// consistent outcomes continue. `LateKnown` is a duplicate of something
    /// already applied, so dropping it changes nothing; everything else is
    /// either a contradiction, a wrong-epoch decision, an ID the maker cannot
    /// account for, or a malformed frame — none of which the maker may quietly
    /// keep quoting through.
    pub fn fails_closed(&self) -> bool {
        !matches!(self, Self::Matched { .. } | Self::LateKnown { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionRegistryError {
    Capacity { limit: usize },
    DuplicateRequestId { request_id: String },
}

impl fmt::Display for ProjectionRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capacity { limit } => write!(
                formatter,
                "order-response request registry reached its limit of {limit}"
            ),
            Self::DuplicateRequestId { request_id } => {
                write!(
                    formatter,
                    "duplicate order-response request ID {request_id}"
                )
            }
        }
    }
}

impl std::error::Error for ProjectionRegistryError {}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedOrder {
    pub order_id: u64,
    pub client_order_id: String,
    pub side: OrderSide,
    pub price: f64,
    pub open_qty: f64,
    pub level: u32,
    pub ref_center: f64,
    pub placed_at_cycle: u64,
    total_qty: f64,
    stream_filled_qty: f64,
    ledger_filled_qty: f64,
}

impl ProjectedOrder {
    fn resting_quote(&self) -> RestingQuote {
        RestingQuote {
            order_id: Some(self.order_id.to_string()),
            side: self.side,
            level: self.level,
            price: self.price,
            qty: self.open_qty,
            ref_center: self.ref_center,
            placed_at_cycle: self.placed_at_cycle,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderObservation {
    pub order_id: u64,
    pub client_order_id: Option<String>,
    pub side: OrderSide,
    pub price: f64,
    pub open_qty: f64,
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AccountProjectionEvent {
    AdvanceCycle { cycle: u64 },
    PlaceSubmitted(ProjectionPendingPlace),
    PlaceAccepted { request_id: String },
    PlaceRejected { request_id: String },
    CancelSubmitted(ProjectionPendingCancel),
    CancelResolved { request_id: String },
    OrderObserved(OrderObservation),
    TradeApplied { order_id: u64, qty: f64 },
    PositionObserved { position: f64 },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionOutcome {
    pub applied: bool,
    pub order_changed: bool,
    pub position_changed: bool,
    pub unknown_current_run_order: bool,
    /// Request whose venue-visible order state became effective. This may be
    /// observed before its independent command acknowledgement.
    pub effective_request_id: Option<String>,
    pub request_registry_error: Option<ProjectionRegistryError>,
}

/// One in-flight place/cancel request, tracked in a single registry.
///
/// A request has two independent lifecycles that used to live in two parallel
/// collections:
///
/// - `ack_pending`: still awaiting the command-stream ack (`PlaceAccepted` /
///   `PlaceRejected` / `CancelResolved`). Counts toward the registry capacity
///   and request-id dedup.
/// - `slot_open`: still an unmatched pending place/cancel — visible in the
///   `pending_places()` / `pending_cancels()` views and eligible for order
///   adoption. Cleared only once the order is observed, rejected, resolved,
///   or explicit cleanup invalidates the venue exposure.
///
/// The two clear independently: a place can be adopted from the account stream
/// (slot closes) before its command-stream ack arrives, or observed terminal
/// while a late ack is still outstanding. An entry is dropped only once both
/// are false — see [`MakerAccountProjection::drop_settled`].
#[derive(Debug, Clone, PartialEq)]
struct PendingEntry {
    request: ProjectionPendingRequest,
    ack_pending: bool,
    slot_open: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct CompletedRequest {
    request: ProjectionPendingRequest,
    resolution: ProjectionRequestResolution,
    /// Projection generation the request was resolved in. A tombstone that
    /// outlived its generation still identifies the request, but an
    /// acknowledgement arriving against it belongs to a superseded epoch and
    /// must be reported as such rather than as an unknown ID.
    generation: u64,
}

impl CompletedRequest {
    fn request_id(&self) -> &str {
        self.request.request_id()
    }

    fn accepted_place(&self) -> Option<&ProjectionPendingPlace> {
        match (&self.request, self.resolution) {
            (
                ProjectionPendingRequest::Place(place),
                ProjectionRequestResolution::PlaceAccepted,
            ) => Some(place),
            _ => None,
        }
    }

    fn resolved_cancel(&self) -> Option<&ProjectionPendingCancel> {
        match (&self.request, self.resolution) {
            (
                ProjectionPendingRequest::Cancel(cancel),
                ProjectionRequestResolution::CancelResolved,
            ) => Some(cancel),
            _ => None,
        }
    }
}

impl PendingEntry {
    fn request_id(&self) -> &str {
        self.request.request_id()
    }

    fn place(&self) -> Option<&ProjectionPendingPlace> {
        match &self.request {
            ProjectionPendingRequest::Place(place) => Some(place),
            ProjectionPendingRequest::Cancel(_) => None,
        }
    }

    fn cancel(&self) -> Option<&ProjectionPendingCancel> {
        match &self.request {
            ProjectionPendingRequest::Cancel(cancel) => Some(cancel),
            ProjectionPendingRequest::Place(_) => None,
        }
    }

    fn is_settled(&self) -> bool {
        !self.ack_pending && !self.slot_open
    }
}

/// Level assigned to a current-run order adopted with neither a matching
/// pending place nor a prior projection (e.g. one observed after a reconnect).
/// It is deliberately outside the maker's real level range so `reconcile`
/// treats it as `Stale` and cancels it, rather than mistaking it for a live
/// quote slot the strategy would try to hold.
const UNKNOWN_ADOPTED_LEVEL: u32 = u32::MAX;

/// The slot metadata adopted for an observed order: where it sits in the quote
/// ladder and how much of it has already filled.
struct AdoptedSlot {
    level: u32,
    ref_center: f64,
    placed_at_cycle: u64,
    total_qty: f64,
    ledger_filled_qty: f64,
}

impl AdoptedSlot {
    fn from_place(place: &ProjectionPendingPlace) -> Self {
        Self {
            level: place.level,
            ref_center: place.ref_center,
            placed_at_cycle: place.cycle,
            total_qty: place.qty,
            ledger_filled_qty: 0.0,
        }
    }

    fn from_existing(order: &ProjectedOrder) -> Self {
        Self {
            level: order.level,
            ref_center: order.ref_center,
            placed_at_cycle: order.placed_at_cycle,
            total_qty: order.total_qty,
            ledger_filled_qty: order.ledger_filled_qty,
        }
    }

    fn unknown(observation: &OrderObservation) -> Self {
        Self {
            level: UNKNOWN_ADOPTED_LEVEL,
            ref_center: observation.price,
            placed_at_cycle: 0,
            total_qty: observation.open_qty,
            ledger_filled_qty: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MakerAccountProjection {
    generation: u64,
    run_order_prefix: String,
    orders: HashMap<u64, ProjectedOrder>,
    pending: Vec<PendingEntry>,
    completed: VecDeque<CompletedRequest>,
    retired_order_ids: VecDeque<u64>,
    observed_position: f64,
    /// Half a price tick. Adopting a venue-echoed order by price must tolerate
    /// the representation difference between the submitted and echoed values
    /// (up to several ULPs at a ~100 price); an exact/EPSILON compare would
    /// miss the pending place it belongs to.
    price_tolerance: f64,
    /// Half a qty tick. Open quantity at or below this is treated as fully
    /// filled (sub-tick dust), not a still-resting order.
    qty_tolerance: f64,
}

impl MakerAccountProjection {
    pub fn new(
        generation: u64,
        run_order_prefix: impl Into<String>,
        position: f64,
        price_tolerance: f64,
        qty_tolerance: f64,
    ) -> Self {
        Self {
            generation,
            run_order_prefix: run_order_prefix.into(),
            orders: HashMap::new(),
            pending: Vec::new(),
            completed: VecDeque::new(),
            retired_order_ids: VecDeque::new(),
            observed_position: position,
            price_tolerance,
            qty_tolerance,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn reset(&mut self, generation: u64, position: f64) {
        self.generation = generation;
        self.orders.clear();
        self.pending.clear();
        self.completed.clear();
        self.retired_order_ids.clear();
        self.observed_position = position;
    }

    /// Begin a new account-stream epoch after maker cleanup without dropping
    /// acknowledgements that are still in flight on the independent
    /// order-response stream. The cleanup has removed executable venue orders,
    /// so quote slots are closed; only correlation metadata and bounded retired
    /// order IDs survive the stream epoch change.
    pub fn reset_after_cleanup_preserving_pending_acks(&mut self, generation: u64, position: f64) {
        self.generation = generation;
        self.clear_orders_preserving_pending_acks();
        self.observed_position = position;
    }

    /// Close every executable quote slot after a maker cleanup has verified
    /// the venue book is empty, resolving in-flight order-response
    /// acknowledgements according to `continuity`. Both variants close the
    /// venue slots — that invariant lives here, in one place — and differ only
    /// in whether pending request correlation survives the cleanup. Recovery
    /// flows call this instead of the mechanical `clear_orders_*` primitives so
    /// the decision they make (did the placement channel survive?) is explicit.
    pub fn finish_verified_cleanup(&mut self, continuity: OrderResponseContinuity) {
        match continuity {
            OrderResponseContinuity::Preserved => self.clear_orders_preserving_pending_acks(),
            OrderResponseContinuity::Replaced => self.clear_orders_and_pending(),
        }
    }

    pub fn clear_orders_and_pending(&mut self) {
        self.orders.clear();
        self.pending.clear();
    }

    /// Clear executable quote state while retaining acknowledgements that the
    /// order-response stream has not delivered yet. A fill or account update
    /// can arrive before its correlated order response; a reconciliation
    /// freeze must therefore close the quote slots without turning that later,
    /// valid response into an unknown request ID.
    pub fn clear_orders_preserving_pending_acks(&mut self) {
        let order_ids = self.orders.keys().copied().collect::<Vec<_>>();
        for order_id in order_ids {
            self.remember_retired_order(order_id);
        }
        self.orders.clear();
        for entry in &mut self.pending {
            entry.slot_open = false;
        }
        self.drop_settled();
    }

    pub fn observed_position(&self) -> f64 {
        self.observed_position
    }

    pub fn resting_quotes(&self) -> Vec<RestingQuote> {
        let mut orders = self.orders.values().collect::<Vec<_>>();
        orders.sort_by_key(|order| order.order_id);
        orders
            .into_iter()
            .map(ProjectedOrder::resting_quote)
            .collect()
    }

    /// Open pending places, derived from the registry. Cheap to rebuild — the
    /// set is bounded by the maker's level count and is only ever iterated.
    pub fn pending_places(&self) -> Vec<ProjectionPendingPlace> {
        self.pending
            .iter()
            .filter(|entry| entry.slot_open)
            .filter_map(|entry| entry.place().cloned())
            .collect()
    }

    /// Open pending cancels, derived from the registry.
    pub fn pending_cancels(&self) -> Vec<ProjectionPendingCancel> {
        self.pending
            .iter()
            .filter(|entry| entry.slot_open)
            .filter_map(|entry| entry.cancel().cloned())
            .collect()
    }

    pub fn pending_request(&self, request_id: &str) -> Option<&ProjectionPendingRequest> {
        self.pending
            .iter()
            .find(|entry| entry.ack_pending && entry.request_id() == request_id)
            .map(|entry| &entry.request)
    }

    pub fn pending_request_count(&self) -> usize {
        self.pending
            .iter()
            .filter(|entry| entry.ack_pending)
            .count()
    }

    /// Whether either half of a submitted request lifecycle is still open.
    ///
    /// A request remains live while its acknowledgement or venue-exposure
    /// slot is still open. For an accepted place that means waiting for the
    /// corresponding account-order observation; terminal responses and
    /// explicit cleanup may close a lifecycle earlier. The CLI uses this
    /// clock-free query without duplicating projection correlation rules.
    pub fn has_pending_request_lifecycle(&self, request_id: &str) -> bool {
        self.pending
            .iter()
            .any(|entry| entry.request_id() == request_id)
    }

    /// Classify one observed order-response acknowledgement against the request
    /// registry. Pure lookup: the caller decides what to do with the verdict.
    ///
    /// Checked pending-first, then tombstones, because a request that is both
    /// still awaiting its ack and already tombstoned cannot exist — an entry is
    /// tombstoned only once its ack lifecycle closed.
    pub fn classify_response(
        &self,
        request_id: Option<&str>,
        accepted: bool,
    ) -> ResponseCorrelation {
        let Some(request_id) = request_id else {
            return ResponseCorrelation::Unidentified;
        };
        if let Some(entry) = self
            .pending
            .iter()
            .find(|entry| entry.ack_pending && entry.request_id() == request_id)
        {
            return ResponseCorrelation::Matched {
                operation: entry.request.operation(),
                lifecycle: RequestLifecycle::AwaitingAck,
            };
        }
        // The ack lifecycle has closed, so the tombstone — not the still-open
        // quote slot — is the authority on what the outcome was. An entry whose
        // slot is open is reported as such, but a second acknowledgement is
        // still judged against the recorded resolution: a rejection arriving for
        // an accepted place contradicts it whether or not the venue has
        // confirmed the order yet.
        let slot_still_open = self
            .pending
            .iter()
            .any(|entry| entry.request_id() == request_id);
        let Some(completed) = self
            .completed
            .iter()
            .rev()
            .find(|entry| entry.request_id() == request_id)
        else {
            return if slot_still_open {
                // Registered, ack resolved, but the tombstone that recorded the
                // outcome has been evicted. Consistency cannot be checked, and
                // "cannot check" is not "consistent".
                ResponseCorrelation::Unverifiable
            } else {
                ResponseCorrelation::Orphan
            };
        };
        let operation = completed.request.operation();
        let resolution = completed.resolution;
        let resolved_in = completed.generation;
        let lifecycle = if slot_still_open {
            RequestLifecycle::AwaitingVenue
        } else {
            RequestLifecycle::Retired
        };
        if resolution.accepts_response(accepted) {
            ResponseCorrelation::LateKnown {
                operation,
                resolution,
                resolved_in,
                lifecycle,
            }
        } else {
            ResponseCorrelation::Contradictory {
                operation,
                resolution,
                resolved_in,
                lifecycle,
            }
        }
    }

    pub fn completed_request_resolution(
        &self,
        request_id: &str,
    ) -> Option<ProjectionRequestResolution> {
        self.completed
            .iter()
            .rev()
            .find(|entry| entry.request_id() == request_id)
            .map(|entry| entry.resolution)
    }

    pub fn apply(&mut self, generation: u64, event: AccountProjectionEvent) -> ProjectionOutcome {
        if generation != self.generation {
            return ProjectionOutcome::default();
        }
        match event {
            AccountProjectionEvent::AdvanceCycle { .. } => {
                // Strategy cycles are not a transport deadline: account/order
                // events can advance several cycles inside one wall-clock
                // second. Keep every pending venue exposure reserved until an
                // explicit response, account-order observation, or cleanup
                // closes it. Silently expiring here can permit a duplicate
                // place while the original request is still live.
                ProjectionOutcome {
                    applied: true,
                    ..ProjectionOutcome::default()
                }
            }
            AccountProjectionEvent::PlaceSubmitted(pending) => {
                if let Err(error) = self.register_request(ProjectionPendingRequest::Place(pending))
                {
                    return ProjectionOutcome {
                        request_registry_error: Some(error),
                        ..ProjectionOutcome::default()
                    };
                }
                ProjectionOutcome {
                    applied: true,
                    ..ProjectionOutcome::default()
                }
            }
            AccountProjectionEvent::PlaceAccepted { request_id } => {
                // The venue accepted the place: it is no longer ack-pending.
                // The slot stays open until the order is observed.
                let request = self
                    .pending
                    .iter_mut()
                    .find(|entry| {
                        entry.ack_pending
                            && entry.request_id() == request_id
                            && entry.place().is_some()
                    })
                    .map(|entry| {
                        entry.ack_pending = false;
                        entry.request.clone()
                    });
                let applied = request.is_some();
                if let Some(request) = request {
                    self.remember_completed_request(
                        request,
                        ProjectionRequestResolution::PlaceAccepted,
                    );
                }
                self.drop_settled();
                ProjectionOutcome {
                    applied,
                    ..ProjectionOutcome::default()
                }
            }
            AccountProjectionEvent::PlaceRejected { request_id } => {
                // A reject is terminal: it clears both the ack and the slot.
                let request = self
                    .pending
                    .iter_mut()
                    .find(|entry| entry.request_id() == request_id && entry.place().is_some())
                    .map(|entry| {
                        entry.ack_pending = false;
                        entry.slot_open = false;
                        entry.request.clone()
                    });
                let applied = request.is_some();
                if let Some(request) = request {
                    self.remember_completed_request(
                        request,
                        ProjectionRequestResolution::PlaceRejected,
                    );
                }
                self.drop_settled();
                ProjectionOutcome {
                    applied,
                    ..ProjectionOutcome::default()
                }
            }
            AccountProjectionEvent::CancelSubmitted(pending) => {
                let order_id = pending.order_id;
                if let Err(error) = self.register_request(ProjectionPendingRequest::Cancel(pending))
                {
                    return ProjectionOutcome {
                        request_registry_error: Some(error),
                        ..ProjectionOutcome::default()
                    };
                }
                self.orders.remove(&order_id);
                self.remember_retired_order(order_id);
                ProjectionOutcome {
                    applied: true,
                    order_changed: true,
                    ..ProjectionOutcome::default()
                }
            }
            AccountProjectionEvent::CancelResolved { request_id } => {
                let index = self
                    .pending
                    .iter()
                    .position(|entry| entry.request_id() == request_id && entry.cancel().is_some());
                let Some(index) = index else {
                    return ProjectionOutcome::default();
                };
                // Only a still-open cancel is holding an order out of the map;
                // cleanup or a terminal account observation may have closed
                // the slot before the response arrives.
                let entry = self.pending.remove(index);
                let order_changed = if entry.slot_open {
                    let order_id = entry.cancel().expect("cancel entry").order_id;
                    self.orders.remove(&order_id).is_some()
                } else {
                    false
                };
                self.remember_completed_request(
                    entry.request,
                    ProjectionRequestResolution::CancelResolved,
                );
                ProjectionOutcome {
                    applied: true,
                    order_changed,
                    ..ProjectionOutcome::default()
                }
            }
            AccountProjectionEvent::OrderObserved(observation) => self.observe_order(observation),
            AccountProjectionEvent::TradeApplied { order_id, qty } => {
                let qty_tolerance = self.qty_tolerance;
                let Some(order) = self.orders.get_mut(&order_id) else {
                    return ProjectionOutcome {
                        applied: true,
                        ..ProjectionOutcome::default()
                    };
                };
                order.ledger_filled_qty += qty;
                order.open_qty = (order.total_qty
                    - order.stream_filled_qty.max(order.ledger_filled_qty))
                .max(0.0);
                if order.open_qty <= qty_tolerance {
                    self.orders.remove(&order_id);
                }
                ProjectionOutcome {
                    applied: true,
                    order_changed: true,
                    ..ProjectionOutcome::default()
                }
            }
            AccountProjectionEvent::PositionObserved { position } => {
                let changed = self.observed_position != position;
                self.observed_position = position;
                ProjectionOutcome {
                    applied: true,
                    position_changed: changed,
                    ..ProjectionOutcome::default()
                }
            }
        }
    }

    fn register_request(
        &mut self,
        request: ProjectionPendingRequest,
    ) -> Result<(), ProjectionRegistryError> {
        if self
            .pending
            .iter()
            .any(|entry| entry.request_id() == request.request_id())
            || self
                .completed_request_resolution(request.request_id())
                .is_some()
        {
            return Err(ProjectionRegistryError::DuplicateRequestId {
                request_id: request.request_id().to_string(),
            });
        }
        if self.pending_request_count() >= MAX_PENDING_ORDER_REQUESTS {
            return Err(ProjectionRegistryError::Capacity {
                limit: MAX_PENDING_ORDER_REQUESTS,
            });
        }
        self.pending.push(PendingEntry {
            request,
            ack_pending: true,
            slot_open: true,
        });
        Ok(())
    }

    /// Drop registry entries whose ack and slot lifecycles have both completed.
    fn drop_settled(&mut self) {
        self.pending.retain(|entry| !entry.is_settled());
    }

    fn remember_completed_request(
        &mut self,
        request: ProjectionPendingRequest,
        resolution: ProjectionRequestResolution,
    ) {
        if self
            .completed
            .iter()
            .any(|entry| entry.request_id() == request.request_id())
        {
            return;
        }
        self.completed.push_back(CompletedRequest {
            request,
            resolution,
            generation: self.generation,
        });
        if self.completed.len() > MAX_COMPLETED_ORDER_REQUESTS {
            self.completed.pop_front();
        }
    }

    fn remember_retired_order(&mut self, order_id: u64) {
        if self.retired_order_ids.contains(&order_id) {
            return;
        }
        self.retired_order_ids.push_back(order_id);
        if self.retired_order_ids.len() > MAX_RETIRED_ORDER_IDS {
            self.retired_order_ids.pop_front();
        }
    }

    fn observe_order(&mut self, observation: OrderObservation) -> ProjectionOutcome {
        if !is_current_run_client_order_id(
            observation.client_order_id.as_deref(),
            &self.run_order_prefix,
        ) {
            return ProjectionOutcome::default();
        }
        if observation.terminal || observation.open_qty <= self.qty_tolerance {
            self.handle_terminal_observation(&observation)
        } else {
            self.adopt_open_observation(observation)
        }
    }

    /// A terminal (or zero-qty) observation removes the projected order and
    /// closes matching place/cancel slots — registry entries linger when an
    /// acknowledgement is still pending so the late response can settle.
    fn handle_terminal_observation(&mut self, observation: &OrderObservation) -> ProjectionOutcome {
        let known = self.order_observation_is_known(observation);
        let changed = self.orders.remove(&observation.order_id).is_some();
        let effective_request_id = self
            .pending
            .iter()
            .find(|entry| {
                entry.slot_open
                    && entry
                        .cancel()
                        .is_some_and(|cancel| cancel.order_id == observation.order_id)
            })
            .map(|entry| entry.request_id().to_string())
            .or_else(|| {
                self.completed.iter().rev().find_map(|entry| {
                    entry
                        .resolved_cancel()
                        .filter(|cancel| cancel.order_id == observation.order_id)
                        .map(|_| entry.request_id().to_string())
                })
            })
            .or_else(|| {
                let client_order_id = observation.client_order_id.as_deref()?;
                self.pending
                    .iter()
                    .find(|entry| {
                        entry.slot_open
                            && entry
                                .place()
                                .is_some_and(|place| place.client_order_id == client_order_id)
                    })
                    .map(|entry| entry.request_id().to_string())
            });
        for entry in &mut self.pending {
            let matches = match &entry.request {
                ProjectionPendingRequest::Place(place) => observation
                    .client_order_id
                    .as_deref()
                    .is_some_and(|client_order_id| place.client_order_id == client_order_id),
                ProjectionPendingRequest::Cancel(cancel) => cancel.order_id == observation.order_id,
            };
            if matches {
                entry.slot_open = false;
            }
        }
        self.drop_settled();
        if known {
            self.remember_retired_order(observation.order_id);
        }
        ProjectionOutcome {
            applied: true,
            order_changed: changed,
            effective_request_id,
            ..ProjectionOutcome::default()
        }
    }

    /// A live (non-terminal) observation adopts the order's slot: match it to a
    /// pending place if possible, otherwise fall back to any existing
    /// projection, then to an unknown-order slot, and reconcile open qty.
    fn adopt_open_observation(&mut self, observation: OrderObservation) -> ProjectionOutcome {
        let retired = self.retired_order_ids.contains(&observation.order_id);
        // A replay of an order already cancelled/cleared must remain stale so
        // reconcile cancels it again; completed place metadata must not turn
        // it back into a quote the strategy is willing to hold.
        let pending_match = if retired {
            None
        } else {
            self.match_pending_slot(&observation)
        };
        let completed_match = if retired || pending_match.is_some() {
            None
        } else {
            self.completed_place_slot(&observation)
        };
        let effective_request_id = pending_match
            .as_ref()
            .or(completed_match.as_ref())
            .map(|(_, request_id)| request_id.clone());
        let slot = pending_match
            .map(|(slot, _)| slot)
            .or_else(|| completed_match.map(|(slot, _)| slot));
        let known = slot.is_some() || self.orders.contains_key(&observation.order_id) || retired;
        let slot = slot.unwrap_or_else(|| {
            self.orders
                .get(&observation.order_id)
                .map(AdoptedSlot::from_existing)
                .unwrap_or_else(|| AdoptedSlot::unknown(&observation))
        });
        let stream_filled_qty = (slot.total_qty - observation.open_qty).max(0.0);
        let open_qty = (slot.total_qty - stream_filled_qty.max(slot.ledger_filled_qty)).max(0.0);
        self.orders.insert(
            observation.order_id,
            ProjectedOrder {
                order_id: observation.order_id,
                client_order_id: observation.client_order_id.unwrap_or_default(),
                side: observation.side,
                price: observation.price,
                open_qty,
                level: slot.level,
                ref_center: slot.ref_center,
                placed_at_cycle: slot.placed_at_cycle,
                total_qty: slot.total_qty,
                stream_filled_qty,
                ledger_filled_qty: slot.ledger_filled_qty,
            },
        );
        ProjectionOutcome {
            applied: true,
            order_changed: true,
            unknown_current_run_order: !known,
            effective_request_id,
            ..ProjectionOutcome::default()
        }
    }

    /// Find the open pending place this observation fills — by client-order-id,
    /// else by a side/price/qty heuristic — close its slot, and return the slot
    /// info to adopt. Returns `None` when no pending place matches.
    fn match_pending_slot(
        &mut self,
        observation: &OrderObservation,
    ) -> Option<(AdoptedSlot, String)> {
        let price_tolerance = self.price_tolerance;
        let index = self
            .pending
            .iter()
            .position(|entry| {
                entry.slot_open
                    && entry.place().is_some_and(|place| {
                        Some(place.client_order_id.as_str())
                            == observation.client_order_id.as_deref()
                    })
            })
            .or_else(|| {
                self.pending.iter().position(|entry| {
                    entry.slot_open
                        && entry.place().is_some_and(|place| {
                            place.side == observation.side
                                && (place.price - observation.price).abs() <= price_tolerance
                                && open_qty_adopts(observation.open_qty, place.qty)
                        })
                })
            })?;
        let place = self.pending[index]
            .place()
            .expect("matched entry is a place")
            .clone();
        let request_id = self.pending[index].request_id().to_string();
        self.pending[index].slot_open = false;
        self.drop_settled();
        Some((AdoptedSlot::from_place(&place), request_id))
    }

    fn completed_place_slot(
        &self,
        observation: &OrderObservation,
    ) -> Option<(AdoptedSlot, String)> {
        let client_order_id = observation.client_order_id.as_deref()?;
        self.completed.iter().rev().find_map(|entry| {
            entry
                .accepted_place()
                .filter(|place| place.client_order_id == client_order_id)
                .map(|place| {
                    (
                        AdoptedSlot::from_place(place),
                        entry.request_id().to_string(),
                    )
                })
        })
    }

    fn order_observation_is_known(&self, observation: &OrderObservation) -> bool {
        self.orders.contains_key(&observation.order_id)
            || self.retired_order_ids.contains(&observation.order_id)
            || observation
                .client_order_id
                .as_deref()
                .is_some_and(|client_order_id| {
                    self.pending.iter().any(|entry| {
                        entry
                            .place()
                            .is_some_and(|place| place.client_order_id == client_order_id)
                    }) || self.completed.iter().any(|entry| {
                        entry
                            .accepted_place()
                            .is_some_and(|place| place.client_order_id == client_order_id)
                    })
                })
    }

    /// Return current-run REST open orders that the live projection cannot
    /// explain as either already projected or still awaiting their account
    /// order observation.
    ///
    /// REST order-set absence and quantity differences are deliberately not
    /// audited here. The authenticated account stream owns steady-state order
    /// truth; retaining a projection-only order is conservative because its
    /// quote slot stays occupied. A REST-only order is safe to tolerate only
    /// while it matches an active place slot by client-order ID or an active
    /// cancel slot by venue order ID.
    pub fn unexpected_rest_open_order_ids(
        &self,
        generation: u64,
        observed_orders: &[OrderObservation],
    ) -> Vec<u64> {
        if generation != self.generation {
            return Vec::new();
        }

        let mut unexpected = observed_orders
            .iter()
            .filter(|order| {
                !order.terminal
                    && is_current_run_client_order_id(
                        order.client_order_id.as_deref(),
                        &self.run_order_prefix,
                    )
            })
            .filter(|order| {
                if self.orders.contains_key(&order.order_id) {
                    return false;
                }
                let Some(client_order_id) = order.client_order_id.as_deref() else {
                    return true;
                };
                !self.pending.iter().any(|entry| {
                    entry.slot_open
                        && (entry
                            .place()
                            .is_some_and(|place| place.client_order_id == client_order_id)
                            || entry
                                .cancel()
                                .is_some_and(|cancel| cancel.order_id == order.order_id))
                })
            })
            .map(|order| order.order_id)
            .collect::<Vec<_>>();
        unexpected.sort_unstable();
        unexpected.dedup();
        unexpected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "sxmk-run-";

    fn pending(request_id: &str) -> ProjectionPendingPlace {
        ProjectionPendingPlace {
            request_id: request_id.to_owned(),
            client_order_id: format!("{PREFIX}q00000001b0"),
            side: OrderSide::Buy,
            price: 100.0,
            qty: 0.2,
            level: 0,
            ref_center: 100.0,
            cycle: 1,
        }
    }

    /// Every step a request's identity can be observed through, so a
    /// permutation test reads as an ordered list of observations.
    #[derive(Debug, Clone, Copy)]
    enum Step {
        /// Register the place before any network write.
        Submit,
        /// Command-stream acknowledgement (accepted / rejected).
        Ack { accepted: bool },
        /// Account-stream order update closing the quote slot.
        Venue { terminal: bool },
        /// Recovery bumped the generation, preserving pending acks.
        ReconnectPreservingAcks,
    }

    fn drive(steps: &[Step]) -> MakerAccountProjection {
        let mut projection = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        let mut generation = 1;
        for step in steps {
            match *step {
                Step::Submit => {
                    projection.apply(
                        generation,
                        AccountProjectionEvent::PlaceSubmitted(pending("req-1")),
                    );
                }
                Step::Ack { accepted } => {
                    let event = if accepted {
                        AccountProjectionEvent::PlaceAccepted {
                            request_id: "req-1".to_owned(),
                        }
                    } else {
                        AccountProjectionEvent::PlaceRejected {
                            request_id: "req-1".to_owned(),
                        }
                    };
                    projection.apply(generation, event);
                }
                Step::Venue { terminal } => {
                    projection.apply(
                        generation,
                        AccountProjectionEvent::OrderObserved(order(
                            if terminal { 0.0 } else { 0.2 },
                            terminal,
                        )),
                    );
                }
                Step::ReconnectPreservingAcks => {
                    generation += 1;
                    projection.reset_after_cleanup_preserving_pending_acks(generation, 0.0);
                }
            }
        }
        projection
    }

    /// The classification table, pinned per ordering. These are the permutations
    /// #277 requires: WS-then-account, account-then-WS, duplicates, delayed
    /// deliveries, partial-fill-then-cancel, reconnect replay, and wrong IDs.
    #[test]
    fn correlation_verdict_is_pinned_for_every_observation_ordering() {
        let accepted = Step::Ack { accepted: true };
        let rejected = Step::Ack { accepted: false };
        let resting = Step::Venue { terminal: false };
        let gone = Step::Venue { terminal: true };

        // (steps already observed, the ack now arriving, expected verdict label)
        let cases: Vec<(&str, Vec<Step>, bool, &str)> = vec![
            (
                "registered, awaiting its first ack",
                vec![Step::Submit],
                true,
                "matched",
            ),
            (
                "WS ack then account order: duplicate ack after the slot closed",
                vec![Step::Submit, accepted, resting],
                true,
                "late_known",
            ),
            (
                "account order then WS ack: adoption closed the slot first",
                vec![Step::Submit, resting],
                true,
                "matched",
            ),
            (
                "duplicate ack while the venue has not confirmed yet",
                vec![Step::Submit, accepted],
                true,
                "late_known",
            ),
            (
                "rejection contradicting an accepted place, slot still open",
                vec![Step::Submit, accepted],
                false,
                "contradictory",
            ),
            (
                "acceptance contradicting a rejected place",
                vec![Step::Submit, rejected],
                true,
                "contradictory",
            ),
            (
                "partial fill then terminal: ack replayed after the order is gone",
                vec![Step::Submit, accepted, resting, gone],
                true,
                "late_known",
            ),
            (
                "reconnect replay: ack resolved in the previous generation",
                vec![Step::Submit, accepted, Step::ReconnectPreservingAcks],
                true,
                "late_known",
            ),
            (
                "ack arriving before anything was registered",
                vec![],
                true,
                "orphan_current_run",
            ),
        ];

        for (name, steps, accepted_response, expected) in cases {
            let projection = drive(&steps);
            let verdict = projection.classify_response(Some("req-1"), accepted_response);
            assert_eq!(verdict.label(), expected, "case: {name}");
            // The safety policy follows from the verdict, nothing else.
            assert_eq!(
                verdict.fails_closed(),
                !matches!(expected, "matched" | "late_known"),
                "case: {name}"
            );
        }
    }

    /// A reconnect that preserves pending acks must keep the *pending* ack
    /// matched, not demote it to a tombstone verdict — that is the whole point of
    /// preserving it across the generation bump.
    #[test]
    fn reconnect_preserving_acks_keeps_an_unacknowledged_request_matched() {
        let projection = drive(&[Step::Submit, Step::ReconnectPreservingAcks]);
        let verdict = projection.classify_response(Some("req-1"), true);
        assert_eq!(
            verdict,
            ResponseCorrelation::Matched {
                operation: RequestOperation::Place,
                lifecycle: RequestLifecycle::AwaitingAck,
            }
        );
    }

    /// A missing `request_id` is a protocol violation with its own verdict. It
    /// must never share the "unknown ID" path, and it must never be ignored.
    #[test]
    fn a_frame_without_a_request_id_is_reported_as_a_protocol_violation() {
        let projection = drive(&[Step::Submit]);
        let verdict = projection.classify_response(None, true);
        assert_eq!(verdict, ResponseCorrelation::Unidentified);
        assert!(verdict.fails_closed());
        assert_eq!(verdict.lifecycle(), RequestLifecycle::Unknown);
        assert_eq!(verdict.operation(), None);
    }

    /// Tombstone eviction must not silently become "consistent". Once the
    /// recorded resolution is gone the maker cannot check the acknowledgement,
    /// so it fails closed instead of assuming agreement.
    #[test]
    fn evicting_the_resolution_tombstone_fails_closed_rather_than_assuming_agreement() {
        let mut projection = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        projection.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("req-1")));
        projection.apply(
            1,
            AccountProjectionEvent::PlaceAccepted {
                request_id: "req-1".to_owned(),
            },
        );
        assert_eq!(
            projection.classify_response(Some("req-1"), true).label(),
            "late_known"
        );

        // Push the tombstone out of the bounded history. The quote slot stays
        // open the whole time, so the request is still registered.
        for index in 0..=MAX_COMPLETED_ORDER_REQUESTS {
            let request_id = format!("filler-{index}");
            let mut filler = pending(&request_id);
            filler.client_order_id = format!("{PREFIX}q0000000{index}b0");
            projection.apply(1, AccountProjectionEvent::PlaceSubmitted(filler));
            projection.apply(1, AccountProjectionEvent::PlaceRejected { request_id });
        }

        let verdict = projection.classify_response(Some("req-1"), true);
        assert_eq!(verdict, ResponseCorrelation::Unverifiable);
        assert!(verdict.fails_closed());
    }

    fn order(open_qty: f64, terminal: bool) -> OrderObservation {
        OrderObservation {
            order_id: 7,
            client_order_id: Some(format!("{PREFIX}q00000001b0")),
            side: OrderSide::Buy,
            price: 100.0,
            open_qty,
            terminal,
        }
    }

    #[test]
    fn account_order_reports_place_effective_before_ack() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        assert_eq!(outcome.effective_request_id.as_deref(), Some("p1"));
        assert_eq!(
            state.pending_request("p1"),
            Some(&ProjectionPendingRequest::Place(pending("p1")))
        );
    }

    #[test]
    fn terminal_account_order_reports_cancel_effective() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        state.apply(
            1,
            AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
                request_id: "c1".to_string(),
                order_id: 7,
                side: OrderSide::Buy,
                level: 0,
                price: 100.0,
                cycle: 2,
            }),
        );
        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
        assert_eq!(outcome.effective_request_id.as_deref(), Some("c1"));
    }

    #[test]
    fn terminal_account_order_reports_cancel_effective_after_ack() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        state.apply(
            1,
            AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
                request_id: "c1".to_string(),
                order_id: 7,
                side: OrderSide::Buy,
                level: 0,
                price: 100.0,
                cycle: 2,
            }),
        );
        state.apply(
            1,
            AccountProjectionEvent::CancelResolved {
                request_id: "c1".to_string(),
            },
        );

        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
        assert_eq!(outcome.effective_request_id.as_deref(), Some("c1"));
    }

    #[test]
    fn order_then_trade_and_duplicate_trade_outcome_are_idempotent() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        state.apply(
            1,
            AccountProjectionEvent::TradeApplied {
                order_id: 7,
                qty: 0.1,
            },
        );
        assert_eq!(state.resting_quotes()[0].qty, 0.1);
        // The ledger suppresses duplicate trades, so no second outcome is
        // delivered. Replayed order state converges to the same open qty.
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.1, false)));
        assert_eq!(state.resting_quotes()[0].qty, 0.1);
    }

    #[test]
    fn trade_before_order_does_not_create_phantom_order() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(
            1,
            AccountProjectionEvent::TradeApplied {
                order_id: 7,
                qty: 0.1,
            },
        );
        assert!(state.resting_quotes().is_empty());
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.1, false)));
        assert_eq!(state.resting_quotes()[0].qty, 0.1);
    }

    #[test]
    fn partial_fill_then_cancel_is_terminal_in_either_order() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        state.apply(
            1,
            AccountProjectionEvent::TradeApplied {
                order_id: 7,
                qty: 0.1,
            },
        );
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
        assert!(state.resting_quotes().is_empty());
    }

    #[test]
    fn wrong_run_and_stale_generation_are_ignored() {
        let mut state = MakerAccountProjection::new(2, PREFIX, 0.0, 0.005, 0.00005);
        let mut wrong = order(0.2, false);
        wrong.client_order_id = Some("sxmk-other-q00000001b0".to_string());
        assert!(
            !state
                .apply(2, AccountProjectionEvent::OrderObserved(wrong))
                .applied
        );
        assert!(
            !state
                .apply(1, AccountProjectionEvent::PlaceSubmitted(pending("old")))
                .applied
        );
        assert!(state.pending_places().is_empty());
    }

    #[test]
    fn cancel_ack_after_close_is_idempotent() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        state.apply(
            1,
            AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
                request_id: "c1".to_string(),
                order_id: 7,
                side: OrderSide::Buy,
                level: 0,
                price: 100.0,
                cycle: 2,
            }),
        );
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
        assert!(state.pending_cancels().is_empty());
        assert!(matches!(
            state.pending_request("c1"),
            Some(ProjectionPendingRequest::Cancel(_))
        ));
        assert!(
            state
                .apply(
                    1,
                    AccountProjectionEvent::CancelResolved {
                        request_id: "c1".to_string()
                    }
                )
                .applied
        );
        assert!(state.resting_quotes().is_empty());
    }

    #[test]
    fn late_open_after_cancel_ack_is_recognized_as_a_retired_current_run_order() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(
            1,
            AccountProjectionEvent::PlaceAccepted {
                request_id: "p2".to_string(),
            },
        );
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        state.apply(
            1,
            AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
                request_id: "c1".to_string(),
                order_id: 7,
                side: OrderSide::Buy,
                level: 0,
                price: 100.0,
                cycle: 2,
            }),
        );
        state.apply(
            1,
            AccountProjectionEvent::CancelResolved {
                request_id: "c1".to_string(),
            },
        );

        // The order channel can replay an open state after the cancel command
        // was accepted. It is still ours, so project it as stale for another
        // cancellation instead of treating it as an external/unknown order.
        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        assert!(outcome.applied && outcome.order_changed);
        assert!(!outcome.unknown_current_run_order);
        assert_eq!(state.resting_quotes()[0].level, UNKNOWN_ADOPTED_LEVEL);
    }

    #[test]
    fn cleanup_marks_cleared_orders_as_retired_for_late_open_replays() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(
            1,
            AccountProjectionEvent::PlaceAccepted {
                request_id: "p1".to_string(),
            },
        );
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));

        state.clear_orders_preserving_pending_acks();
        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        assert!(outcome.applied && outcome.order_changed);
        assert!(!outcome.unknown_current_run_order);
        assert_eq!(state.resting_quotes()[0].level, UNKNOWN_ADOPTED_LEVEL);
    }

    #[test]
    fn late_unknown_open_after_verified_cleanup_forces_reconciliation() {
        // After a verified cleanup (either continuity), a fresh current-run
        // open order with no pending place must never silently become a
        // holdable quote: it is flagged unknown and adopted at the sentinel
        // level so reconciliation cancels it rather than resuming on it.
        for continuity in [
            OrderResponseContinuity::Preserved,
            OrderResponseContinuity::Replaced,
        ] {
            let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
            // Establish and adopt an initial quote, then verify-cleanup it.
            state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
            state.apply(
                1,
                AccountProjectionEvent::PlaceAccepted {
                    request_id: "p1".to_string(),
                },
            );
            state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
            state.finish_verified_cleanup(continuity);
            assert!(
                state.resting_quotes().is_empty(),
                "{continuity:?}: cleanup must leave no executable quote"
            );

            // A brand-new current-run order (unseen id, no pending place) lands
            // late on the venue.
            let mut late = order(0.2, false);
            late.order_id = 4242;
            late.client_order_id = Some(format!("{PREFIX}q00000099x9"));
            let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(late));

            assert!(
                outcome.unknown_current_run_order,
                "{continuity:?}: a late unknown current-run order must require reconciliation"
            );
            let resting = state.resting_quotes();
            assert_eq!(resting.len(), 1);
            assert_eq!(
                resting[0].level, UNKNOWN_ADOPTED_LEVEL,
                "{continuity:?}: it must be adopted at the sentinel level, not a holdable quote"
            );
        }
    }

    #[test]
    fn account_reconnect_reset_preserves_unacked_order_response_registry() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(
            1,
            AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
                request_id: "c1".to_string(),
                order_id: 7,
                side: OrderSide::Buy,
                level: 0,
                price: 100.0,
                cycle: 1,
            }),
        );

        state.reset_after_cleanup_preserving_pending_acks(2, 0.0);
        assert_eq!(state.generation(), 2);
        assert!(state.pending_places().is_empty());
        assert!(state.pending_cancels().is_empty());
        assert!(matches!(
            state.pending_request("p1"),
            Some(ProjectionPendingRequest::Place(_))
        ));
        assert!(matches!(
            state.pending_request("c1"),
            Some(ProjectionPendingRequest::Cancel(_))
        ));

        state.apply(
            2,
            AccountProjectionEvent::PlaceAccepted {
                request_id: "p1".to_string(),
            },
        );
        state.apply(
            2,
            AccountProjectionEvent::CancelResolved {
                request_id: "c1".to_string(),
            },
        );
        assert_eq!(state.pending_request_count(), 0);
        state.reset_after_cleanup_preserving_pending_acks(3, 0.0);
        assert_eq!(
            state.completed_request_resolution("p1"),
            Some(ProjectionRequestResolution::PlaceAccepted)
        );
        assert_eq!(
            state.completed_request_resolution("c1"),
            Some(ProjectionRequestResolution::CancelResolved)
        );
    }

    #[test]
    fn late_place_ack_matches_after_account_order_is_already_terminal() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
        assert!(state.pending_places().is_empty());
        assert!(matches!(
            state.pending_request("p1"),
            Some(ProjectionPendingRequest::Place(_))
        ));

        let outcome = state.apply(
            1,
            AccountProjectionEvent::PlaceAccepted {
                request_id: "p1".to_string(),
            },
        );
        assert!(outcome.applied);
        assert_eq!(state.pending_request_count(), 0);
    }

    #[test]
    fn freeze_closes_quote_slots_but_preserves_unacked_response_registry() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(
            1,
            AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
                request_id: "c1".to_string(),
                order_id: 7,
                side: OrderSide::Buy,
                level: 0,
                price: 100.0,
                cycle: 1,
            }),
        );

        state.clear_orders_preserving_pending_acks();
        assert!(state.pending_places().is_empty());
        assert!(state.pending_cancels().is_empty());
        assert!(matches!(
            state.pending_request("p1"),
            Some(ProjectionPendingRequest::Place(_))
        ));
        assert!(matches!(
            state.pending_request("c1"),
            Some(ProjectionPendingRequest::Cancel(_))
        ));

        assert!(
            state
                .apply(
                    1,
                    AccountProjectionEvent::PlaceAccepted {
                        request_id: "p1".to_string(),
                    },
                )
                .applied
        );
        assert!(
            state
                .apply(
                    1,
                    AccountProjectionEvent::CancelResolved {
                        request_id: "c1".to_string(),
                    },
                )
                .applied
        );
        assert_eq!(state.pending_request_count(), 0);
    }

    #[test]
    fn request_registry_is_strictly_bounded_and_rejects_duplicates() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        for index in 0..MAX_PENDING_ORDER_REQUESTS {
            let outcome = state.apply(
                1,
                AccountProjectionEvent::PlaceSubmitted(pending(&format!("p{index}"))),
            );
            assert!(outcome.request_registry_error.is_none());
        }
        assert_eq!(state.pending_request_count(), MAX_PENDING_ORDER_REQUESTS);

        let overflow = state.apply(
            1,
            AccountProjectionEvent::PlaceSubmitted(pending("overflow")),
        );
        assert!(matches!(
            overflow.request_registry_error,
            Some(ProjectionRegistryError::Capacity {
                limit: MAX_PENDING_ORDER_REQUESTS
            })
        ));

        let mut duplicate = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        duplicate.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("same")));
        let outcome = duplicate.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("same")));
        assert!(matches!(
            outcome.request_registry_error,
            Some(ProjectionRegistryError::DuplicateRequestId { .. })
        ));
    }

    #[test]
    fn position_projects_independently_of_ordering() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        let outcome = state.apply(
            1,
            AccountProjectionEvent::PositionObserved { position: 0.2 },
        );
        assert!(outcome.position_changed);
        assert_eq!(state.observed_position(), 0.2);
    }

    #[test]
    fn order_before_position_and_position_before_order_converge() {
        let mut order_first = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        order_first.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        order_first.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        order_first.apply(
            1,
            AccountProjectionEvent::PositionObserved { position: 0.2 },
        );

        let mut position_first = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        position_first.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        position_first.apply(
            1,
            AccountProjectionEvent::PositionObserved { position: 0.2 },
        );
        position_first.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));

        assert_eq!(
            order_first.observed_position(),
            position_first.observed_position()
        );
        assert_eq!(
            order_first.resting_quotes(),
            position_first.resting_quotes()
        );
    }

    #[test]
    fn rest_audit_tolerates_projection_absence_and_known_quantity_drift() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));

        assert!(state.unexpected_rest_open_order_ids(1, &[]).is_empty());
        assert!(state
            .unexpected_rest_open_order_ids(1, &[order(0.1, false)])
            .is_empty());
        assert_eq!(state.resting_quotes()[0].qty, 0.2);
    }

    #[test]
    fn rest_audit_tolerates_projected_order_plus_place_awaiting_account_stream() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));

        let mut second = pending("p2");
        second.client_order_id = format!("{PREFIX}q00000002a0");
        second.side = OrderSide::Sell;
        second.price = 101.0;
        second.level = 1;
        second.cycle = 2;
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(second.clone()));

        let mut projected = order(0.1, false);
        let rest_only = OrderObservation {
            order_id: 8,
            client_order_id: Some(second.client_order_id),
            side: second.side,
            price: second.price,
            open_qty: second.qty,
            terminal: false,
        };

        assert!(state
            .unexpected_rest_open_order_ids(1, &[projected.clone(), rest_only.clone()])
            .is_empty());

        state.apply(
            1,
            AccountProjectionEvent::PlaceAccepted {
                request_id: "p1".to_string(),
            },
        );
        projected.open_qty = 0.05;
        assert!(state
            .unexpected_rest_open_order_ids(1, &[projected, rest_only])
            .is_empty());
    }

    #[test]
    fn rest_audit_rejects_unexpected_or_retired_current_run_open_order() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        let unexpected = order(0.2, false);
        assert_eq!(
            state.unexpected_rest_open_order_ids(1, std::slice::from_ref(&unexpected)),
            vec![7]
        );

        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(1, AccountProjectionEvent::OrderObserved(unexpected.clone()));
        state.apply(1, AccountProjectionEvent::OrderObserved(order(0.0, true)));
        assert_eq!(
            state.unexpected_rest_open_order_ids(1, std::slice::from_ref(&unexpected)),
            vec![7]
        );

        let mut wrong_run = unexpected;
        wrong_run.client_order_id = Some("sxmk-other-q00000001b0".to_string());
        assert!(state
            .unexpected_rest_open_order_ids(1, &[wrong_run])
            .is_empty());
        assert!(state
            .unexpected_rest_open_order_ids(2, &[order(0.2, false)])
            .is_empty());
    }

    #[test]
    fn rest_audit_tolerates_order_until_pending_cancel_resolves() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        let open = order(0.2, false);
        state.apply(1, AccountProjectionEvent::OrderObserved(open.clone()));
        state.apply(
            1,
            AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
                request_id: "c1".to_string(),
                order_id: open.order_id,
                side: open.side,
                level: 0,
                price: open.price,
                cycle: 2,
            }),
        );

        assert!(state
            .unexpected_rest_open_order_ids(1, std::slice::from_ref(&open))
            .is_empty());

        state.apply(
            1,
            AccountProjectionEvent::CancelResolved {
                request_id: "c1".to_string(),
            },
        );
        assert_eq!(state.unexpected_rest_open_order_ids(1, &[open]), vec![7]);
    }

    #[test]
    fn rapid_cycle_advances_keep_unconfirmed_slots_reserved() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(
            1,
            AccountProjectionEvent::CancelSubmitted(ProjectionPendingCancel {
                request_id: "c1".to_string(),
                order_id: 9,
                side: OrderSide::Sell,
                level: 0,
                price: 101.0,
                cycle: 1,
            }),
        );
        assert_eq!(state.pending_places().len(), 1);
        assert_eq!(state.pending_cancels().len(), 1);

        // Account events can wake several cycles in one wall-clock second.
        // None may release the quote slot while the original venue request is
        // still awaiting a correlated outcome.
        for cycle in 2..=100 {
            state.apply(1, AccountProjectionEvent::AdvanceCycle { cycle });
        }
        assert_eq!(state.pending_places().len(), 1);
        assert_eq!(state.pending_cancels().len(), 1);
        assert_eq!(state.pending_request_count(), 2);
        assert!(matches!(
            state.pending_request("p1"),
            Some(ProjectionPendingRequest::Place(_))
        ));
        assert!(matches!(
            state.pending_request("c1"),
            Some(ProjectionPendingRequest::Cancel(_))
        ));

        // A rejection is the explicit terminal outcome that releases it.
        state.apply(
            1,
            AccountProjectionEvent::PlaceRejected {
                request_id: "p1".to_string(),
            },
        );
        assert!(state.pending_places().is_empty());
        assert_eq!(state.pending_request_count(), 1);
        state.apply(
            1,
            AccountProjectionEvent::CancelResolved {
                request_id: "c1".to_string(),
            },
        );
        assert!(state.pending_cancels().is_empty());
        assert_eq!(state.pending_request_count(), 0);
    }

    #[test]
    fn accepted_place_stays_reserved_until_account_order_is_visible() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(
            1,
            AccountProjectionEvent::PlaceAccepted {
                request_id: "p1".to_string(),
            },
        );
        for cycle in 2..=100 {
            state.apply(1, AccountProjectionEvent::AdvanceCycle { cycle });
        }
        assert_eq!(state.pending_places().len(), 1);
        assert_eq!(state.pending_request_count(), 0);
        assert_eq!(
            state.completed_request_resolution("p1"),
            Some(ProjectionRequestResolution::PlaceAccepted)
        );

        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        assert!(outcome.applied && outcome.order_changed);
        assert!(
            !outcome.unknown_current_run_order,
            "a delayed account update must retain the accepted place identity"
        );
        assert!(state.pending_places().is_empty());
        assert_eq!(outcome.effective_request_id.as_deref(), Some("p1"));
        assert_eq!(state.resting_quotes()[0].level, 0);
    }

    #[test]
    fn rejected_place_tombstone_does_not_authorize_an_open_order() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));
        state.apply(
            1,
            AccountProjectionEvent::PlaceRejected {
                request_id: "p1".to_string(),
            },
        );

        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        assert!(outcome.unknown_current_run_order);
    }

    #[test]
    fn completed_request_tombstones_are_bounded_and_reset_with_the_run() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        for index in 0..=MAX_COMPLETED_ORDER_REQUESTS {
            let request_id = format!("p{index}");
            state.apply(
                1,
                AccountProjectionEvent::PlaceSubmitted(pending(&request_id)),
            );
            state.apply(1, AccountProjectionEvent::PlaceRejected { request_id });
        }
        assert_eq!(state.completed.len(), MAX_COMPLETED_ORDER_REQUESTS);
        assert_eq!(state.completed_request_resolution("p0"), None);
        assert_eq!(
            state.completed_request_resolution(&format!("p{MAX_COMPLETED_ORDER_REQUESTS}")),
            Some(ProjectionRequestResolution::PlaceRejected)
        );

        state.reset(2, 0.0);
        assert!(state.completed.is_empty());
    }

    #[test]
    fn open_observation_adopts_pending_by_price_qty_heuristic() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));

        // A different (but still current-run) client-order-id that matches the
        // pending place on side/price/qty is adopted via the heuristic branch.
        let mut observation = order(0.2, false);
        observation.order_id = 42;
        observation.client_order_id = Some(format!("{PREFIX}q00000009z9"));
        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(observation));

        assert!(outcome.applied && outcome.order_changed);
        assert!(
            !outcome.unknown_current_run_order,
            "a heuristic pending match is not an unknown order"
        );
        let resting = state.resting_quotes();
        assert_eq!(resting.len(), 1);
        assert_eq!(resting[0].level, 0, "adopts the pending place's level");
        assert!(state.pending_places().is_empty(), "the slot is consumed");
    }

    #[test]
    fn unknown_current_run_order_adopts_with_sentinel_level() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);

        // A current-run order with no pending place and no prior projection is
        // adopted at the out-of-range sentinel level so reconcile cancels it.
        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(order(0.2, false)));
        assert!(outcome.applied);
        assert!(outcome.unknown_current_run_order);
        let resting = state.resting_quotes();
        assert_eq!(resting.len(), 1);
        assert_eq!(resting[0].level, UNKNOWN_ADOPTED_LEVEL);
    }

    #[test]
    fn heuristic_adopts_pending_despite_one_ulp_price_echo_difference() {
        let mut state = MakerAccountProjection::new(1, PREFIX, 0.0, 0.005, 0.00005);
        // pending("p1") rests a buy at price 100.0, qty 0.2, level 0.
        state.apply(1, AccountProjectionEvent::PlaceSubmitted(pending("p1")));

        // The venue echoes the "same" price one ULP away (~1.4e-14 at 100) —
        // far above f64::EPSILON but far below half a price tick. The old
        // `<= f64::EPSILON` compare would miss the pending place and adopt the
        // order at the unknown sentinel level; the tick tolerance matches it.
        let echoed_price = f64::from_bits(100.0_f64.to_bits() + 1);
        assert_ne!(echoed_price, 100.0);
        assert!((echoed_price - 100.0).abs() > f64::EPSILON);

        let mut observation = order(0.2, false);
        observation.order_id = 55;
        // A current-run id that does NOT match the pending's client-order-id,
        // forcing the side/price/qty heuristic branch.
        observation.client_order_id = Some(format!("{PREFIX}q00000042c0"));
        observation.price = echoed_price;

        let outcome = state.apply(1, AccountProjectionEvent::OrderObserved(observation));
        assert!(outcome.applied && outcome.order_changed);
        assert!(
            !outcome.unknown_current_run_order,
            "a one-ULP price echo still matches its pending place"
        );
        assert_eq!(
            state.resting_quotes()[0].level,
            0,
            "adopts the pending place's real level, not the unknown sentinel"
        );
        assert!(state.pending_places().is_empty());
    }
}
