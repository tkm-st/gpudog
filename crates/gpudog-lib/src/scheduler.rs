//! GPU reservation lifecycle: who holds which device, and who is waiting.
//!
//! Pure logic. No sockets, no NVML, no clock of its own: the caller passes
//! `now` in, so tests drive time directly instead of sleeping. This is where
//! the hard bugs live, so it is deliberately the easiest part to test.
//!
//! Ordering rules, chosen deliberately:
//!
//! * **The daemon orders the queue, not the client.** Each walk of the
//!   queue visits waiting requests by their user's *load*, lowest first, then
//!   by arrival. A user's load is the GPUs it holds, plus the free managed
//!   GPUs observed in use by its processes outside GPUDog, plus what its
//!   earlier waiting requests want. So a user with nothing is served before
//!   one who already has a lot, and one user's requests stay in order.
//! * **A waiting request blocks only the GPUs it could use.** A request that
//!   cannot be granted now reserves its usable slots for the rest of the walk:
//!   nobody behind it takes them, so a large request is not starved by small
//!   ones, but requests that need none of them are still served.
//! * **Waiting on yourself blocks only yourself.** A request that cannot be
//!   met without a slot its own user is using (held, or busy with that
//!   user's processes) reserves just those slots. Otherwise a user could hold
//!   GPU 0 and ask for GPUs 0 to 6 to keep 1 to 6 idle forever.
//! * **Waiting on an outside process blocks only its GPU.** A request that
//!   cannot be met without slots its own user uses or slots busy outside
//!   GPUDog reserves just those, its user's and the busy ones: nothing says
//!   such a process will ever end.
//! * **One user, one waiting request at a time.** Once a user's request is
//!   blocked, or held back by its own `max_held_per_uid`, that user's later
//!   requests are passed over in the same walk, so one user cannot reserve
//!   more slots with more requests.
//! * **Impossible requests are denied, never queued.** Asking for more devices
//!   than the machine has, or for an index that does not exist, fails
//!   immediately rather than waiting forever.
//!
//! Devices are addressed by [`Slot`], a position in the daemon's startup
//! device list, which never changes. A configuration reload changes which
//! slots are managed ([`Scheduler::set_managed`]), and with them the daemon
//! GPU numbers clients see, which this module also keeps
//! ([`Scheduler::display_of`]).

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::gpu_inventory::GpuInventory;
use crate::protocol::{DenyReason, GpuSelection};

/// A position in the daemon's startup device list: the inventory's index.
///
/// Fixed for the daemon's lifetime, unlike the daemon GPU number shown to
/// clients, which a reload that changes `devices` can renumber. The newtype
/// keeps the two from being mixed up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Slot(pub u32);

impl Slot {
    fn at(self) -> usize {
        self.0 as usize
    }
}

/// Whether the daemon allocates a slot's device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// Listed in `devices` (or every device, when it is unset): allocatable.
    Managed,
    /// Removed from `devices` by a reload while held. Never allocated again,
    /// but still shown, even once released, until the next reload, so the
    /// daemon GPU numbers of running jobs change only at a reload.
    Retiring,
    /// Neither managed nor shown.
    Unmanaged,
}

/// Identifies a reservation. The service uses the connection id, because a
/// reservation is scoped to exactly one connection.
pub type RequestId = u64;

/// A request to hold GPUs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationRequest {
    pub id: RequestId,
    /// Owner, from `SO_PEERCRED`. Used for logging and per-user accounting.
    pub uid: u32,
    pub selection: GpuSelection,
    pub num_gpus: u32,
    /// Specific devices, when `selection` is `ExplicitList`.
    pub gpus: Vec<Slot>,
    /// For `Count` only: allocate from these devices while they are
    /// managed. `None` means any managed device.
    pub candidates: Option<Vec<Slot>>,
    /// `None` means wait indefinitely.
    pub deadline: Option<Instant>,
}

impl ReservationRequest {
    /// Devices wanted, in either selection mode.
    fn wanted(&self) -> u32 {
        match self.selection {
            // Bounded by `MAX_GPU_COUNT` at the protocol boundary.
            // `ExplicitUuids` is resolved to slots by the service first.
            GpuSelection::ExplicitList | GpuSelection::ExplicitUuids => self.gpus.len() as u32,
            GpuSelection::Count => self.num_gpus,
        }
    }
}

/// What happened to a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReservationOutcome {
    /// Devices are held now.
    Granted { gpus: Vec<Slot> },
    /// Waiting at this 0-based queue position.
    Queued { position: u32 },
    /// Terminal failure.
    Denied { reason: DenyReason, detail: String },
}

impl ReservationOutcome {
    fn denied(reason: DenyReason, detail: impl Into<String>) -> Self {
        Self::Denied {
            reason,
            detail: detail.into(),
        }
    }
}

/// A queued request whose state changed during [`Scheduler::tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationTransition {
    pub id: RequestId,
    pub uid: u32,
    pub outcome: ReservationOutcome,
}

/// Snapshot of one reservation, held or queued, for the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationSnapshot {
    pub id: RequestId,
    pub uid: u32,
    /// Devices held. Empty for a queued request.
    pub gpus: Vec<Slot>,
    /// Devices wanted, in either selection mode.
    pub num_gpus: u32,
    /// When the reservation was granted, or when the request was queued.
    pub since: Instant,
    /// Queue position. Always 0 for a holder.
    pub position: u32,
}

/// Snapshot of one held device, for logging and the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceHolding {
    pub slot: Slot,
    pub holder: RequestId,
    pub uid: u32,
}

/// Who was observed using one busy slot, for attributing it to a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyOwners {
    pub slot: Slot,
    /// Uids of the processes observed on the slot whose uid is known. The
    /// slot counts as each of these users' (see [`Scheduler::uses`]);
    /// processes of unknown uid add nobody.
    pub uids: Vec<u32>,
}

pub const DEFAULT_MAX_HELD_PER_UID: u32 = 8;
pub const DEFAULT_MAX_QUEUED_PER_UID: u32 = 8;

/// Per-uid caps. The fd *is* the lock, so without them one user can take
/// every device or fill the queue and deny everyone else service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerLimits {
    /// Devices one uid may hold at once, across all its connections.
    pub max_held_per_uid: u32,
    /// Requests one uid may have waiting at once.
    pub max_queued_per_uid: u32,
}

impl Default for SchedulerLimits {
    fn default() -> Self {
        Self {
            max_held_per_uid: DEFAULT_MAX_HELD_PER_UID,
            max_queued_per_uid: DEFAULT_MAX_QUEUED_PER_UID,
        }
    }
}

#[derive(Debug, Clone)]
struct Holder {
    gpus: Vec<Slot>,
    uid: u32,
    granted_at: Instant,
}

#[derive(Debug, Clone)]
struct QueuedRequest {
    request: ReservationRequest,
    /// When the request entered the queue, for the dashboard's wait time.
    /// Not the tie-breaker: queue order is, because two requests can share a
    /// clock reading.
    queued_at: Instant,
    /// Arrival order, the tie-breaker between equal loads.
    seq: u64,
}

fn plural_gpus(count: u32) -> String {
    format!("{count} {}", if count == 1 { "GPU" } else { "GPUs" })
}

fn plural_requests(count: u32) -> String {
    format!(
        "{count} {}",
        if count == 1 { "request" } else { "requests" }
    )
}

/// Decrements a per-uid counter, removing the entry at zero.
///
/// Reaching zero from zero can only mean a bookkeeping bug, so it asserts in
/// debug builds; release builds clamp and keep serving.
fn unwind(counters: &mut HashMap<u32, u32>, uid: u32, by: u32) {
    let Some(count) = counters.get_mut(&uid) else {
        debug_assert!(false, "per-uid counter for {uid} missing");
        return;
    };
    debug_assert!(*count >= by, "per-uid counter for {uid} underflow");
    *count = count.saturating_sub(by);
    if *count == 0 {
        counters.remove(&uid);
    }
}

/// Exclusive whole-GPU allocator with a load-ordered queue.
#[derive(Debug, Clone)]
pub struct Scheduler {
    /// Slot → holder, or `None` when free. Every per-slot vector has one
    /// entry per startup device, fixed at construction.
    owner: Vec<Option<RequestId>>,
    observed_busy: Vec<bool>,
    /// Slot → who was seen using it, meaningful only while it is observed
    /// busy and not held. Unattributed until [`Scheduler::set_busy_owners`].
    busy_owners: Vec<Vec<u32>>,
    states: Vec<SlotState>,
    /// Slot → UUID, for denials that name a device a reload removed.
    uuids: Vec<String>,
    /// Daemon GPU number → slot: the slots not unmanaged, in slot order.
    /// Recomputed only by [`Scheduler::set_managed`].
    shown: Vec<Slot>,
    allocation_paused: bool,
    held: HashMap<RequestId, Holder>,
    /// Arrival order. The service order is [`Scheduler::walk_order`].
    queue: Vec<QueuedRequest>,
    next_seq: u64,
    /// Devices currently held per uid, and requests currently queued per uid.
    held_per_uid: HashMap<u32, u32>,
    queued_per_uid: HashMap<u32, u32>,
    limits: SchedulerLimits,
    /// Denials decided outside [`Scheduler::tick`] (by [`Scheduler::set_limits`])
    /// and reported by the next `tick`, so every queued session hears its
    /// outcome through the same path.
    pending: Vec<ReservationTransition>,
}

impl Scheduler {
    /// One slot per device of `inventory`, all managed.
    pub fn new(inventory: &dyn GpuInventory, limits: SchedulerLimits) -> Self {
        let count = inventory.devices().len();
        Self {
            owner: vec![None; count],
            observed_busy: vec![false; count],
            busy_owners: vec![Vec::new(); count],
            states: vec![SlotState::Managed; count],
            uuids: inventory.devices().iter().map(|d| d.uuid.clone()).collect(),
            shown: (0u32..).take(count).map(Slot).collect(),
            allocation_paused: false,
            held: HashMap::new(),
            queue: Vec::new(),
            next_seq: 0,
            held_per_uid: HashMap::new(),
            queued_per_uid: HashMap::new(),
            limits,
            pending: Vec::new(),
        }
    }

    pub fn limits(&self) -> SchedulerLimits {
        self.limits
    }

    /// Replaces the per-uid caps, for a configuration reload.
    ///
    /// Forward only: nothing held is taken back, and requests already
    /// waiting stay queued even if their uid is now over `max_queued_per_uid`.
    /// The one exception is a waiting request that asks for more GPUs than
    /// the new `max_held_per_uid` allows by itself. It could never be granted,
    /// and it would hold up every later request of its user, so it is denied
    /// as [`Scheduler::submit`] would have denied it. The denial is reported
    /// by the next [`Scheduler::tick`], so callers must tick right after
    /// this, or that waiter hears nothing until something else does.
    pub fn set_limits(&mut self, limits: SchedulerLimits) {
        self.limits = limits;
        self.deny_hopeless_waiters();
    }

    /// Changes which slots are managed, for a configuration reload:
    /// `managed[slot]` says whether that slot should be.
    ///
    /// Forward only, like [`Scheduler::set_limits`]. A held slot that is no
    /// longer managed becomes [`SlotState::Retiring`]: its holder keeps it,
    /// and it is never allocated again. Daemon GPU numbers are recomputed
    /// here and nowhere else. Waiting requests that can no longer be
    /// satisfied are denied as `submit` would have denied them, and the
    /// denials are reported by the next [`Scheduler::tick`].
    ///
    /// Returns the slots that became managed. Their observed usage is
    /// unknown, so a caller that observes usage must mark them busy.
    pub fn set_managed(&mut self, managed: &[bool]) -> Vec<Slot> {
        debug_assert_eq!(managed.len(), self.states.len(), "one flag per slot");
        let mut added = Vec::new();
        for (index, state) in (0u32..).zip(self.states.iter_mut()) {
            let next = if managed.get(index as usize).copied().unwrap_or(false) {
                SlotState::Managed
            } else if self.owner[index as usize].is_some() {
                SlotState::Retiring
            } else {
                SlotState::Unmanaged
            };
            if next == SlotState::Managed && *state != SlotState::Managed {
                added.push(Slot(index));
            }
            if next != *state {
                // Whoever was seen on it before is stale until the next
                // observation of the current devices.
                self.busy_owners[index as usize].clear();
            }
            *state = next;
        }
        self.shown = (0u32..)
            .zip(&self.states)
            .filter(|(_, state)| **state != SlotState::Unmanaged)
            .map(|(index, _)| Slot(index))
            .collect();
        self.deny_hopeless_waiters();
        added
    }

    /// Moves to `pending` every waiting request that could never be granted
    /// under the current limits and managed slots. One waiting for devices
    /// that are gone would keep its usable slots from every request behind it;
    /// one over the held cap would wait forever.
    fn deny_hopeless_waiters(&mut self) {
        for entry in std::mem::take(&mut self.queue) {
            let request = &entry.request;
            // Same order as `submit`, so a waiter gets the detail text a new
            // request would. The reason can differ: for a removed UUID the
            // service refuses a new request as `INVALID_REQUEST`, while a
            // waiter here gets `UNSATISFIABLE`.
            let outcome = self.reject_impossible(request).or_else(|| {
                (request.wanted() > self.limits.max_held_per_uid)
                    .then(|| self.over_cap_on_its_own(request))
            });
            match outcome {
                None => self.queue.push(entry),
                Some(outcome) => {
                    unwind(&mut self.queued_per_uid, request.uid, 1);
                    self.pending.push(ReservationTransition {
                        id: request.id,
                        uid: request.uid,
                        outcome,
                    });
                }
            }
        }
    }

    fn is_managed(&self, slot: Slot) -> bool {
        self.states.get(slot.at()) == Some(&SlotState::Managed)
    }

    fn managed_count(&self) -> u32 {
        self.states
            .iter()
            .filter(|&&state| state == SlotState::Managed)
            .count() as u32
    }

    fn slots(&self) -> impl Iterator<Item = Slot> {
        (0u32..).take(self.states.len()).map(Slot)
    }

    /// Validates a request against the machine, independent of current usage.
    fn reject_impossible(&self, request: &ReservationRequest) -> Option<ReservationOutcome> {
        let device_count = self.managed_count();
        if device_count == 0 {
            return Some(ReservationOutcome::denied(
                DenyReason::UNSATISFIABLE,
                "this host has no GPUs",
            ));
        }
        match request.selection {
            // `ExplicitUuids` is resolved to slots by the service first,
            // and the service submits only managed slots, so an unmanaged
            // one here was removed by a reload.
            GpuSelection::ExplicitList | GpuSelection::ExplicitUuids => request
                .gpus
                .iter()
                .find(|&&slot| !self.is_managed(slot))
                .map(|&slot| {
                    ReservationOutcome::denied(
                        DenyReason::UNSATISFIABLE,
                        match self.uuids.get(slot.at()) {
                            Some(uuid) => format!("{uuid} is not managed by this daemon"),
                            None => format!(
                                "daemon GPU {} does not exist; this host has {}",
                                slot.0,
                                plural_gpus(device_count)
                            ),
                        },
                    )
                }),
            // Queuing this would wait forever, so fail fast instead.
            GpuSelection::Count if request.num_gpus > device_count => {
                Some(ReservationOutcome::denied(
                    DenyReason::UNSATISFIABLE,
                    format!(
                        "requested {} but this host has only {}",
                        plural_gpus(request.num_gpus),
                        plural_gpus(device_count)
                    ),
                ))
            }
            GpuSelection::Count => {
                let candidates = request.candidates.as_ref()?;
                let usable = candidates
                    .iter()
                    .filter(|&&slot| self.is_managed(slot))
                    .count() as u32;
                (request.num_gpus > usable).then(|| {
                    ReservationOutcome::denied(
                        DenyReason::UNSATISFIABLE,
                        format!(
                            "requested {} but only {} visible to the client and managed by gpudogd",
                            plural_gpus(request.num_gpus),
                            plural_gpus(usable)
                        ),
                    )
                })
            }
        }
    }

    /// Devices that would satisfy `request` right now without any slot in
    /// `reserved`, or `None` if it cannot be satisfied yet.
    fn try_allocate(&self, request: &ReservationRequest, reserved: &[bool]) -> Option<Vec<Slot>> {
        if self.allocation_paused {
            return None;
        }
        let is_free = |slot: Slot| {
            self.is_managed(slot)
                && matches!(self.owner.get(slot.at()), Some(None))
                && self.observed_busy.get(slot.at()) == Some(&false)
                && reserved.get(slot.at()) == Some(&false)
        };
        match request.selection {
            // `ExplicitUuids` is resolved to slots by the service first.
            GpuSelection::ExplicitList | GpuSelection::ExplicitUuids => request
                .gpus
                .iter()
                .all(|&slot| is_free(slot))
                .then(|| request.gpus.clone()),
            GpuSelection::Count => {
                // Lowest free slots first, which are also the lowest daemon
                // GPU numbers, so allocation is deterministic and a machine
                // fills up predictably.
                let chosen: Vec<Slot> = self
                    .slots()
                    .filter(|&slot| {
                        request
                            .candidates
                            .as_ref()
                            .is_none_or(|candidates| candidates.contains(&slot))
                            && is_free(slot)
                    })
                    .take(request.num_gpus as usize)
                    .collect();
                (chosen.len() == request.num_gpus as usize).then_some(chosen)
            }
        }
    }

    fn commit(&mut self, request: &ReservationRequest, gpus: Vec<Slot>, now: Instant) {
        for &slot in &gpus {
            self.owner[slot.at()] = Some(request.id);
        }
        *self.held_per_uid.entry(request.uid).or_default() += gpus.len() as u32;
        self.held.insert(
            request.id,
            Holder {
                gpus,
                uid: request.uid,
                granted_at: now,
            },
        );
    }

    fn held_by(&self, uid: u32) -> u32 {
        self.held_per_uid.get(&uid).copied().unwrap_or(0)
    }

    /// Submits a request: denies it, or queues it and walks the queue once,
    /// exactly as [`Scheduler::tick`] would, and grants it if that walk does.
    ///
    /// Submitting an `id` that already holds or awaits a reservation is a
    /// protocol violation by the caller and is denied.
    ///
    /// Callers must [`Scheduler::tick`] afterwards: the same walk may grant
    /// or expire other requests, and those outcomes wait in `pending`. Until
    /// then, such a request already [`Scheduler::holds`] its devices.
    pub fn submit(&mut self, request: &ReservationRequest, now: Instant) -> ReservationOutcome {
        if self.holds(request.id) || self.is_queued(request.id) {
            // One reservation per connection. A second would orphan the
            // first, since release is keyed by id alone.
            return ReservationOutcome::denied(
                DenyReason::INVALID_REQUEST,
                "this connection already has a reservation",
            );
        }

        if let Some(rejection) = self.reject_impossible(request) {
            return rejection;
        }

        // Per-uid caps. Counting queued requests as well as held devices stops
        // one user filling the queue.
        if request.wanted() > self.limits.max_held_per_uid {
            return self.over_cap_on_its_own(request);
        }
        let held_now = self.held_by(request.uid);
        if held_now.saturating_add(request.wanted()) > self.limits.max_held_per_uid {
            return ReservationOutcome::denied(
                DenyReason::INVALID_REQUEST,
                format!(
                    "this user already holds {}; the per-user cap is {}",
                    plural_gpus(held_now),
                    plural_gpus(self.limits.max_held_per_uid)
                ),
            );
        }
        let queued_now = self.queued_per_uid.get(&request.uid).copied().unwrap_or(0);
        if queued_now >= self.limits.max_queued_per_uid {
            return ReservationOutcome::denied(
                DenyReason::INVALID_REQUEST,
                format!(
                    "this user already has {} waiting; the per-user cap is {}",
                    plural_requests(queued_now),
                    plural_requests(self.limits.max_queued_per_uid)
                ),
            );
        }

        // A deadline that has already passed must not be granted just because
        // a device happens to be free; the client asked to give up by now.
        if request.deadline.is_some_and(|deadline| deadline <= now) {
            return ReservationOutcome::denied(
                DenyReason::TIMED_OUT,
                "timeout elapsed before scheduling",
            );
        }

        // Queued first, then one walk decides: whatever `tick` would do
        // with this request, `submit` does, so the two never disagree.
        let seq = self.next_seq;
        self.next_seq += 1;
        self.queue.push(QueuedRequest {
            request: request.clone(),
            queued_at: now,
            seq,
        });
        *self.queued_per_uid.entry(request.uid).or_default() += 1;
        let mut outcome = None;
        for transition in self.advance(now) {
            if transition.id == request.id {
                outcome = Some(transition.outcome);
            } else {
                self.pending.push(transition);
            }
        }
        match outcome {
            Some(outcome) => outcome,
            None => ReservationOutcome::Queued {
                position: self.queue_position(request.id).unwrap_or(0),
            },
        }
    }

    /// Releases whatever `id` holds, and cancels it if it is still queued.
    ///
    /// Idempotent: releasing an unknown or already-released id is a no-op.
    /// Connections close in ways that are hard to sequence, so double release
    /// must be harmless. Returns true if anything was released or cancelled.
    pub fn release(&mut self, id: RequestId) -> bool {
        let mut changed = false;
        if let Some(holder) = self.held.remove(&id) {
            for &slot in &holder.gpus {
                if let Some(owner) = self.owner.get_mut(slot.at()) {
                    if *owner == Some(id) {
                        *owner = None;
                    }
                }
            }
            unwind(&mut self.held_per_uid, holder.uid, holder.gpus.len() as u32);
            changed = true;
        }
        if let Some(index) = self.index_of(id) {
            let entry = self.queue.remove(index);
            unwind(&mut self.queued_per_uid, entry.request.uid, 1);
            changed = true;
        }
        changed
    }

    /// Advances time: expires deadlines, then grants what it can.
    ///
    /// Must be called after every `release` and whenever a deadline may have
    /// passed. Returns every request whose state changed, so the caller can
    /// notify those connections, including outcomes decided earlier by
    /// [`Scheduler::submit`], [`Scheduler::set_limits`] and
    /// [`Scheduler::set_managed`].
    #[must_use]
    pub fn tick(&mut self, now: Instant) -> Vec<ReservationTransition> {
        let mut transitions = std::mem::take(&mut self.pending);
        transitions.extend(self.advance(now));
        transitions
    }

    /// Expires deadlines, then walks the queue once and grants what it can.
    /// Shared by `tick` and `submit`, so both apply the same rules.
    fn advance(&mut self, now: Instant) -> Vec<ReservationTransition> {
        let mut transitions = Vec::new();

        // Expire first: a request whose deadline has passed must not be
        // granted below, and must not count towards its user's load.
        let (expired, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|entry| {
                entry
                    .request
                    .deadline
                    .is_some_and(|deadline| deadline <= now)
            });
        self.queue = waiting;
        for entry in expired {
            unwind(&mut self.queued_per_uid, entry.request.uid, 1);
            transitions.push(ReservationTransition {
                id: entry.request.id,
                uid: entry.request.uid,
                // Not "timed out waiting for GPUs": the client prefixes that
                // phrase already, and the detail is what the access log records.
                outcome: ReservationOutcome::denied(
                    DenyReason::TIMED_OUT,
                    "no GPU became available before the deadline",
                ),
            });
        }
        if self.allocation_paused {
            return transitions;
        }

        // The order is computed once. Recomputing it after each grant would
        // give the same order: a grant to user u moves what that request
        // wanted from u's earlier waiting requests into u's held GPUs, so
        // the keys of u's later requests do not change, and nobody else's
        // do. Grants take only free slots, which are not observed busy, and
        // the observation does not change during a walk.
        let order = self.walk_order();
        let managed = self.managed();
        let mut blocked_users: HashSet<u32> = HashSet::new();
        let mut reserved = vec![false; self.states.len()];
        let mut granted: HashSet<RequestId> = HashSet::new();
        for index in order {
            let request = self.queue[index].request.clone();
            if blocked_users.contains(&request.uid) {
                continue;
            }
            // Waits for its own user's holders, not for devices: it
            // reserves nothing, but its user's later requests wait too.
            if self.over_held_cap(&request) {
                blocked_users.insert(request.uid);
                continue;
            }
            if let Some(gpus) = self.try_allocate(&request, &reserved) {
                self.commit(&request, gpus.clone(), now);
                granted.insert(request.id);
                transitions.push(ReservationTransition {
                    id: request.id,
                    uid: request.uid,
                    outcome: ReservationOutcome::Granted { gpus },
                });
                continue;
            }
            blocked_users.insert(request.uid);
            for slot in self.blocking_slots(&request) {
                reserved[slot.at()] = true;
            }
            if managed.iter().all(|slot| reserved[slot.at()]) {
                break;
            }
        }
        for entry in std::mem::take(&mut self.queue) {
            if granted.contains(&entry.request.id) {
                unwind(&mut self.queued_per_uid, entry.request.uid, 1);
            } else {
                self.queue.push(entry);
            }
        }
        transitions
    }

    /// Queue indices in service order: lowest load first, then arrival.
    fn walk_order(&self) -> Vec<usize> {
        let mut base: HashMap<u32, u32> = HashMap::new();
        let mut keyed: Vec<(u32, u64, usize)> = Vec::with_capacity(self.queue.len());
        for (index, entry) in self.queue.iter().enumerate() {
            let uid = entry.request.uid;
            let load = base.entry(uid).or_insert_with(|| self.load_now(uid));
            keyed.push((*load, entry.seq, index));
            *load = load.saturating_add(entry.request.wanted());
        }
        keyed.sort_unstable();
        keyed.into_iter().map(|(_, _, index)| index).collect()
    }

    /// GPUs `uid` uses now: held, plus free managed slots observed busy with
    /// its processes.
    fn load_now(&self, uid: u32) -> u32 {
        let busy = self
            .slots()
            .filter(|&slot| {
                self.is_managed(slot)
                    && self.owner[slot.at()].is_none()
                    && self.observed_busy[slot.at()]
                    && self.busy_owners[slot.at()].contains(&uid)
            })
            .count() as u32;
        self.held_by(uid).saturating_add(busy)
    }

    /// True when `uid` is using `slot`: it holds it, or the slot is free but
    /// observed busy with at least one of that user's processes. Others on
    /// the same slot do not change that: counting a slot as someone's only
    /// ever makes that user's requests reserve less, so nobody can widen
    /// another user's reservation by starting a process next to theirs.
    fn uses(&self, uid: u32, slot: Slot) -> bool {
        match self.owner.get(slot.at()) {
            Some(Some(holder)) => self.held.get(holder).is_some_and(|h| h.uid == uid),
            Some(None) => self.busy_outside(slot) && self.busy_owners[slot.at()].contains(&uid),
            None => false,
        }
    }

    /// True when `slot` is free of reservations but busy with processes
    /// started outside GPUDog, which nothing tells the daemon will ever end.
    fn busy_outside(&self, slot: Slot) -> bool {
        matches!(self.owner.get(slot.at()), Some(None))
            && self.observed_busy.get(slot.at()) == Some(&true)
    }

    /// Managed slots `request` could be granted.
    fn usable_slots(&self, request: &ReservationRequest) -> Vec<Slot> {
        match request.selection {
            GpuSelection::ExplicitList | GpuSelection::ExplicitUuids => request
                .gpus
                .iter()
                .copied()
                .filter(|&slot| self.is_managed(slot))
                .collect(),
            GpuSelection::Count => self
                .slots()
                .filter(|&slot| {
                    self.is_managed(slot)
                        && request
                            .candidates
                            .as_ref()
                            .is_none_or(|candidates| candidates.contains(&slot))
                })
                .collect(),
        }
    }

    /// The slots a request that could not be granted keeps from requests
    /// behind it. Normally every slot it could use, so a large request is
    /// not starved by small ones. But if it cannot be met without a slot its
    /// own user is using, or without one busy outside GPUDog, it keeps only
    /// those slots, its user's and the busy ones: it waits for its own user
    /// or for a process that may never end, and must not keep the free
    /// slots idle meanwhile. Those it keeps are held or busy, so keeping
    /// them takes nothing from anyone.
    fn blocking_slots(&self, request: &ReservationRequest) -> Vec<Slot> {
        let usable = self.usable_slots(request);
        let (special, rest): (Vec<Slot>, Vec<Slot>) = usable
            .iter()
            .partition(|&&slot| self.uses(request.uid, slot) || self.busy_outside(slot));
        let needs_special = !special.is_empty()
            && match request.selection {
                // An explicit request needs every slot it names.
                GpuSelection::ExplicitList | GpuSelection::ExplicitUuids => true,
                GpuSelection::Count => (rest.len() as u32) < request.num_gpus,
            };
        if needs_special {
            special
        } else {
            usable
        }
    }

    /// The denial for a request that alone asks for more than
    /// `max_held_per_uid`, whatever its user holds.
    fn over_cap_on_its_own(&self, request: &ReservationRequest) -> ReservationOutcome {
        ReservationOutcome::denied(
            DenyReason::INVALID_REQUEST,
            format!(
                "requested {} but the per-user cap is {}",
                plural_gpus(request.wanted()),
                plural_gpus(self.limits.max_held_per_uid)
            ),
        )
    }

    /// True when granting `request` now would take its user over
    /// `max_held_per_uid`.
    fn over_held_cap(&self, request: &ReservationRequest) -> bool {
        self.held_by(request.uid).saturating_add(request.wanted()) > self.limits.max_held_per_uid
    }

    /// Earliest deadline still in the queue, for scheduling the next `tick`.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.queue
            .iter()
            .filter_map(|entry| entry.request.deadline)
            .min()
    }

    /// Daemon GPUs shown to clients: managed and retiring slots.
    pub fn device_count(&self) -> u32 {
        self.shown.len() as u32
    }

    /// The daemon GPU number of `slot`, or `None` if it is not shown.
    pub fn display_of(&self, slot: Slot) -> Option<u32> {
        self.shown
            .binary_search(&slot)
            .ok()
            .map(|number| number as u32)
    }

    /// Slots shown to clients, in daemon GPU number order.
    pub fn shown(&self) -> &[Slot] {
        &self.shown
    }

    pub fn slot_state(&self, slot: Slot) -> Option<SlotState> {
        self.states.get(slot.at()).copied()
    }

    /// Managed slots, in order.
    pub fn managed(&self) -> Vec<Slot> {
        self.slots().filter(|&slot| self.is_managed(slot)).collect()
    }

    pub fn free_device_count(&self) -> u32 {
        if self.allocation_paused {
            return 0;
        }
        self.slots()
            .filter(|&slot| {
                self.is_managed(slot)
                    && self.owner[slot.at()].is_none()
                    && !self.observed_busy[slot.at()]
            })
            .count() as u32
    }

    pub fn is_held(&self, slot: Slot) -> bool {
        matches!(self.owner.get(slot.at()), Some(Some(_)))
    }

    pub fn holds(&self, id: RequestId) -> bool {
        self.held.contains_key(&id)
    }

    pub fn is_queued(&self, id: RequestId) -> bool {
        self.index_of(id).is_some()
    }

    pub fn queue_size(&self) -> usize {
        self.queue.len()
    }

    /// Observation is separate from ownership. Never revoke an existing
    /// reservation; pause/mark devices only for new allocation decisions.
    pub fn set_observed_usage(&mut self, paused: bool, busy: &[Slot]) {
        self.observed_busy.fill(false);
        self.allocation_paused = paused;
        for &slot in busy {
            if let Some(flag) = self.observed_busy.get_mut(slot.at()) {
                *flag = true;
            } else {
                self.allocation_paused = true; // inconsistent observation fails closed
            }
        }
    }

    /// Records who was observed using each busy slot, for ordering and for
    /// [`Scheduler::uses`]. Slots not listed are unattributed. Never makes a
    /// slot allocatable: that is [`Scheduler::set_observed_usage`]'s job.
    pub fn set_busy_owners(&mut self, owners: &[BusyOwners]) {
        for entry in &mut self.busy_owners {
            entry.clear();
        }
        for owner in owners {
            if let Some(entry) = self.busy_owners.get_mut(owner.slot.at()) {
                let mut uids = owner.uids.clone();
                uids.sort_unstable();
                uids.dedup();
                *entry = uids;
            }
        }
    }

    pub fn allocation_paused(&self) -> bool {
        self.allocation_paused
    }

    /// One entry per held *device*.
    pub fn holdings(&self) -> Vec<DeviceHolding> {
        self.owner
            .iter()
            .enumerate()
            .filter_map(|(index, owner)| {
                let holder = (*owner)?;
                Some(DeviceHolding {
                    slot: Slot(index as u32),
                    holder,
                    uid: self.held.get(&holder).map_or(0, |h| h.uid),
                })
            })
            .collect()
    }

    /// One entry per *reservation* holding devices, ordered by id so the
    /// dashboard's rows do not jump between refreshes.
    pub fn held_reservations(&self) -> Vec<ReservationSnapshot> {
        let mut out: Vec<_> = self
            .held
            .iter()
            .map(|(&id, holder)| ReservationSnapshot {
                id,
                uid: holder.uid,
                gpus: holder.gpus.clone(),
                num_gpus: holder.gpus.len() as u32,
                since: holder.granted_at,
                position: 0,
            })
            .collect();
        out.sort_unstable_by_key(|snapshot| snapshot.id);
        out
    }

    /// The queue, in service order.
    pub fn queued_reservations(&self) -> Vec<ReservationSnapshot> {
        self.walk_order()
            .into_iter()
            .map(|index| &self.queue[index])
            .enumerate()
            .map(|(position, entry)| ReservationSnapshot {
                id: entry.request.id,
                uid: entry.request.uid,
                gpus: Vec::new(),
                num_gpus: entry.request.wanted(),
                since: entry.queued_at,
                position: position as u32,
            })
            .collect()
    }

    /// Position of `id` in service order, or `None` if it is not queued.
    /// The order follows the users' loads, so it can change while waiting.
    pub fn queue_position(&self, id: RequestId) -> Option<u32> {
        let index = self.index_of(id)?;
        self.walk_order()
            .iter()
            .position(|&candidate| candidate == index)
            .map(|position| position as u32)
    }

    /// Index of `id` in the arrival-ordered `queue`, for removing it.
    fn index_of(&self, id: RequestId) -> Option<usize> {
        self.queue.iter().position(|entry| entry.request.id == id)
    }
}

#[cfg(test)]
mod tests;
