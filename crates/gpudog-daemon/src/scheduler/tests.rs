use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::*;
use crate::gpu_inventory::StaticInventory;

/// A fixed instant, so tests never depend on wall-clock progress.
fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    // Offset so `at(-1)` stays representable.
    *EPOCH.get_or_init(|| Instant::now() + Duration::from_secs(1))
}

fn at(milliseconds: i64) -> Instant {
    let offset = Duration::from_millis(milliseconds.unsigned_abs());
    if milliseconds >= 0 {
        epoch() + offset
    } else {
        epoch() - offset
    }
}

fn want(id: RequestId, count: u32) -> ReservationRequest {
    ReservationRequest {
        id,
        uid: 1000,
        selection: GpuSelection::Count,
        num_gpus: count,
        gpus: vec![],
        candidates: None,
        deadline: None,
    }
}

fn want_by(id: RequestId, count: u32, deadline: Instant) -> ReservationRequest {
    ReservationRequest {
        deadline: Some(deadline),
        ..want(id, count)
    }
}

fn want_exactly(id: RequestId, gpus: &[u32]) -> ReservationRequest {
    ReservationRequest {
        selection: GpuSelection::ExplicitList,
        num_gpus: gpus.len() as u32,
        gpus: slots(gpus),
        ..want(id, 0)
    }
}

fn slots(numbers: &[u32]) -> Vec<Slot> {
    numbers.iter().copied().map(Slot).collect()
}

fn scheduler(devices: u32) -> Scheduler {
    Scheduler::new(
        &StaticInventory::with_count(devices),
        SchedulerLimits::default(),
    )
}

#[test]
fn observed_usage_excludes_devices_without_changing_reservation_ownership() {
    let mut sched = scheduler(2);
    sched.set_observed_usage(false, &[Slot(0)]);
    assert!(matches!(
        sched.submit(&want_exactly(1, &[0]), at(0)),
        ReservationOutcome::Queued { .. }
    ));
    assert_eq!(sched.free_device_count(), 1);
    sched.set_observed_usage(false, &[]);
    assert_eq!(sched.tick(at(1)).len(), 1);
    assert!(sched.holds(1));
}

#[test]
fn observation_failure_pauses_grants_but_keeps_holders_and_deadlines() {
    let mut sched = scheduler(2);
    assert!(matches!(
        sched.submit(&want(1, 1), at(0)),
        ReservationOutcome::Granted { .. }
    ));
    sched.set_observed_usage(true, &[]);
    assert!(matches!(
        sched.submit(&want_by(2, 1, at(20)), at(1)),
        ReservationOutcome::Queued { .. }
    ));
    let transitions = sched.tick(at(20));
    assert!(matches!(
        transitions[0].outcome,
        ReservationOutcome::Denied {
            reason: DenyReason::TIMED_OUT,
            ..
        }
    ));
    assert!(sched.holds(1));
    sched.set_observed_usage(false, &[]);
    assert!(matches!(
        sched.submit(&want(3, 1), at(21)),
        ReservationOutcome::Granted { .. }
    ));
}

#[test]
fn inconsistent_observation_indices_fail_closed() {
    let mut sched = scheduler(2);
    sched.set_observed_usage(false, &[Slot(2)]);
    assert!(sched.allocation_paused());
    assert!(matches!(
        sched.submit(&want(1, 1), at(0)),
        ReservationOutcome::Queued { .. }
    ));
}

fn limited(devices: u32, max_held_per_uid: u32, max_queued_per_uid: u32) -> Scheduler {
    Scheduler::new(
        &StaticInventory::with_count(devices),
        SchedulerLimits {
            max_held_per_uid,
            max_queued_per_uid,
        },
    )
}

fn is_granted(outcome: &ReservationOutcome) -> bool {
    matches!(outcome, ReservationOutcome::Granted { .. })
}

fn is_queued(outcome: &ReservationOutcome) -> bool {
    matches!(outcome, ReservationOutcome::Queued { .. })
}

fn deny_reason(outcome: &ReservationOutcome) -> Option<DenyReason> {
    match outcome {
        ReservationOutcome::Denied { reason, .. } => Some(*reason),
        _ => None,
    }
}

fn granted_gpus(outcome: &ReservationOutcome) -> Vec<u32> {
    match outcome {
        ReservationOutcome::Granted { gpus } => gpus.iter().map(|slot| slot.0).collect(),
        other => panic!("expected a grant, got {other:?}"),
    }
}

fn submit_granted(state: &mut Scheduler, request: ReservationRequest, now: Instant) {
    let outcome = state.submit(&request, now);
    assert!(
        is_granted(&outcome),
        "request {} not granted: {outcome:?}",
        request.id
    );
}

fn submit_queued(state: &mut Scheduler, request: ReservationRequest, now: Instant) {
    let outcome = state.submit(&request, now);
    assert!(
        is_queued(&outcome),
        "request {} not queued: {outcome:?}",
        request.id
    );
}

/// Every device has at most one holder, and the holder table agrees with the
/// device table. Checked after each mutation in the busier tests.
fn check_no_double_booking(state: &Scheduler) {
    let holdings = state.holdings();
    let mut seen = std::collections::HashSet::new();
    for holding in &holdings {
        assert!(
            seen.insert(holding.slot),
            "device {:?} double-booked",
            holding.slot
        );
        assert!(state.is_held(holding.slot));
    }
    assert_eq!(
        state.free_device_count() as usize + holdings.len(),
        state.device_count() as usize
    );
}

#[test]
fn grants_an_immediate_request_on_an_idle_machine() {
    let mut state = scheduler(4);
    let outcome = state.submit(&want(1, 2), epoch());
    assert_eq!(granted_gpus(&outcome), [0, 1]);
    assert_eq!(state.free_device_count(), 2);
    assert!(state.holds(1));
    check_no_double_booking(&state);
}

#[test]
fn holds_gpus_exclusively() {
    let mut state = scheduler(2);
    submit_granted(&mut state, want(1, 2), epoch());
    assert_eq!(
        state.submit(&want(2, 1), epoch()),
        ReservationOutcome::Queued { position: 0 }
    );
    assert_eq!(state.free_device_count(), 0);
    check_no_double_booking(&state);
}

#[test]
fn grants_a_waiter_when_the_holder_releases() {
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, want(2, 1), epoch());

    assert!(state.release(1));
    let transitions = state.tick(at(1));
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].id, 2);
    assert_eq!(granted_gpus(&transitions[0].outcome), [0]);
    assert!(state.holds(2));
    check_no_double_booking(&state);
}

#[test]
fn release_is_idempotent() {
    // A double release must never free a device since handed to someone else.
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    assert!(state.release(1));
    assert!(!state.release(1));
    assert!(!state.release(1));

    submit_granted(&mut state, want(2, 1), epoch());
    // A late release from the previous holder must not touch the new one.
    assert!(!state.release(1));
    assert!(state.holds(2));
    assert!(state.is_held(Slot(0)));
    check_no_double_booking(&state);
}

#[test]
fn ignores_release_of_an_unknown_id() {
    let mut state = scheduler(2);
    assert!(!state.release(999));
    assert_eq!(state.free_device_count(), 2);
}

#[test]
fn orders_the_queue_by_what_each_user_already_uses() {
    // uid 1000 holds the only device and waits for more; uid 2000 and 3000
    // hold nothing. The users with less are served first, in arrival order.
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, want(2, 1), epoch());
    submit_queued(&mut state, by(2000, want(3, 1)), epoch());
    submit_queued(&mut state, by(3000, want(4, 1)), epoch());

    assert_eq!(state.queue_position(3), Some(0));
    assert_eq!(state.queue_position(4), Some(1));
    assert_eq!(state.queue_position(2), Some(2));

    // Once uid 1000 releases, it holds nothing either, and its request is
    // the oldest. Then uid 1000 holds one again, so 3 and 4 go first.
    state.release(1);
    assert_eq!(ids(&state.tick(at(1))), [2]);
    assert_eq!(state.queue_position(3), Some(0));
    assert_eq!(state.queue_position(4), Some(1));
}

#[test]
fn a_users_own_earlier_requests_count_towards_its_load() {
    // uid 1000 queues two requests before uid 2000 queues one. Its second
    // counts the GPU its first wants, so uid 2000's goes between them.
    let mut state = scheduler(1);
    submit_granted(&mut state, by(9000, want(1, 1)), epoch());
    submit_queued(&mut state, want(2, 1), epoch());
    submit_queued(&mut state, want(3, 1), epoch());
    submit_queued(&mut state, by(2000, want(4, 1)), epoch());
    assert_eq!(state.queue_position(2), Some(0));
    assert_eq!(state.queue_position(4), Some(1));
    assert_eq!(state.queue_position(3), Some(2));
}

#[test]
fn is_fifo_among_equal_loads() {
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    for id in 2..=4 {
        submit_queued(&mut state, want(id, 1), epoch());
    }
    assert_eq!(state.queue_position(2), Some(0));
    assert_eq!(state.queue_position(3), Some(1));
    assert_eq!(state.queue_position(4), Some(2));

    state.release(1);
    let transitions = state.tick(at(1));
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].id, 2);
}

#[test]
fn reports_the_queue_position_it_inserted_at() {
    // Submit's `Queued { position }` must agree with the queue itself, also
    // when a request of a user with less lands ahead of existing waiters.
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    assert_eq!(
        state.submit(&want(2, 1), epoch()),
        ReservationOutcome::Queued { position: 0 }
    );
    assert_eq!(
        state.submit(&want(3, 1), epoch()),
        ReservationOutcome::Queued { position: 1 }
    );
    assert_eq!(
        state.submit(&by(2000, want(4, 1)), epoch()),
        ReservationOutcome::Queued { position: 0 }
    );
    assert_eq!(
        state.submit(&want(5, 1), epoch()),
        ReservationOutcome::Queued { position: 3 }
    );
    assert_eq!(state.queue_position(2), Some(1));
}

#[test]
fn a_smaller_request_does_not_take_what_an_earlier_waiter_needs() {
    let mut state = scheduler(2);
    submit_granted(&mut state, want(1, 2), epoch());
    submit_queued(&mut state, want(2, 2), epoch());
    submit_queued(&mut state, want(3, 1), epoch());

    assert!(state.release(1));
    submit_queued(&mut state, want(4, 1), epoch());

    // Head (id 2) wants two devices and both are free, so it is granted
    // first; nobody jumped ahead of it.
    let transitions = state.tick(at(1));
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].id, 2);
    assert_eq!(state.queue_position(3), Some(0));
    check_no_double_booking(&state);
}

#[test]
fn blocks_a_small_request_behind_a_larger_one() {
    let mut state = scheduler(4);
    submit_granted(&mut state, want(1, 3), epoch());
    // Head needs 4, only 1 is free.
    submit_queued(&mut state, want(2, 4), epoch());
    // Would fit right now, but must not overtake.
    submit_queued(&mut state, want(3, 1), epoch());

    assert!(state.tick(at(1)).is_empty());
    assert_eq!(state.free_device_count(), 1);
    assert_eq!(state.queue_position(2), Some(0));
    assert_eq!(state.queue_position(3), Some(1));
}

#[test]
fn times_out_a_queued_request() {
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, want_by(2, 1, at(100)), epoch());

    assert!(state.tick(at(50)).is_empty());
    let transitions = state.tick(at(100));
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].id, 2);
    assert_eq!(
        deny_reason(&transitions[0].outcome),
        Some(DenyReason::TIMED_OUT)
    );
    assert_eq!(state.queue_size(), 0);
}

#[test]
fn expiry_unblocks_the_request_behind_it() {
    let mut state = scheduler(2);
    submit_granted(&mut state, want(1, 1), epoch());
    // Head wants both devices and will give up at t=100.
    submit_queued(&mut state, want_by(2, 2, at(100)), epoch());
    submit_queued(&mut state, want(3, 1), epoch());

    let transitions = state.tick(at(100));
    assert_eq!(transitions.len(), 2);
    assert_eq!(transitions[0].id, 2);
    assert!(deny_reason(&transitions[0].outcome).is_some());
    assert_eq!(transitions[1].id, 3);
    assert!(is_granted(&transitions[1].outcome));
    check_no_double_booking(&state);
}

#[test]
fn denies_a_deadline_that_has_already_passed() {
    let mut state = scheduler(2);
    let outcome = state.submit(&want_by(1, 1, at(-1)), epoch());
    assert_eq!(deny_reason(&outcome), Some(DenyReason::TIMED_OUT));
    // Nothing was taken despite devices being free.
    assert_eq!(state.free_device_count(), 2);
}

#[test]
fn denies_a_request_larger_than_the_machine() {
    let mut state = scheduler(2);
    let outcome = state.submit(&want(1, 3), epoch());
    assert_eq!(deny_reason(&outcome), Some(DenyReason::UNSATISFIABLE));
    assert_eq!(state.queue_size(), 0);
}

#[test]
fn denies_an_explicit_index_that_does_not_exist() {
    let mut state = scheduler(2);
    let outcome = state.submit(&want_exactly(1, &[0, 7]), epoch());
    assert_eq!(deny_reason(&outcome), Some(DenyReason::UNSATISFIABLE));
    assert_eq!(state.queue_size(), 0);
}

#[test]
fn denies_everything_on_a_gpu_less_host() {
    let mut state = scheduler(0);
    let outcome = state.submit(&want(1, 1), epoch());
    assert_eq!(deny_reason(&outcome), Some(DenyReason::UNSATISFIABLE));
}

#[test]
fn denial_details_read_well() {
    // These strings reach the client and the access log.
    let detail = |outcome: ReservationOutcome| match outcome {
        ReservationOutcome::Denied { detail, .. } => detail,
        other => panic!("expected a denial, got {other:?}"),
    };
    assert_eq!(
        detail(scheduler(0).submit(&want(1, 1), epoch())),
        "this host has no GPUs"
    );
    assert_eq!(
        detail(scheduler(1).submit(&want(1, 3), epoch())),
        "requested 3 GPUs but this host has only 1 GPU"
    );
    assert_eq!(
        detail(scheduler(2).submit(&want_exactly(1, &[5]), epoch())),
        "daemon GPU 5 does not exist; this host has 2 GPUs"
    );
    let mut state = limited(10, 4, 1);
    submit_granted(&mut state, want(1, 4), epoch());
    assert_eq!(
        detail(state.submit(&want(2, 1), epoch())),
        "this user already holds 4 GPUs; the per-user cap is 4 GPUs"
    );
    // The waiting cap counts requests, not GPUs.
    let mut state = limited(2, 8, 1);
    submit_granted(&mut state, want(1, 2), epoch());
    let _ = state.submit(&want(2, 1), epoch());
    assert_eq!(
        detail(state.submit(&want(3, 1), epoch())),
        "this user already has 1 request waiting; the per-user cap is 1 request"
    );
}

#[test]
fn queues_an_explicit_request_whose_devices_are_busy() {
    let mut state = scheduler(3);
    submit_granted(&mut state, want_exactly(1, &[1]), epoch());
    assert!(state.is_held(Slot(1)));
    assert!(!state.is_held(Slot(0)));

    // Device 1 is taken, so this waits even though 0 and 2 are free.
    submit_queued(&mut state, want_exactly(2, &[1, 2]), epoch());

    state.release(1);
    let transitions = state.tick(at(1));
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].id, 2);
    assert_eq!(granted_gpus(&transitions[0].outcome), [1, 2]);
    check_no_double_booking(&state);
}

#[test]
fn grants_an_explicit_request_for_free_devices_immediately() {
    let mut state = scheduler(4);
    let outcome = state.submit(&want_exactly(1, &[2, 3]), epoch());
    assert_eq!(granted_gpus(&outcome), [2, 3]);
    assert!(state.is_held(Slot(2)));
    assert!(state.is_held(Slot(3)));
    assert!(!state.is_held(Slot(0)));
}

#[test]
fn rejects_a_second_reservation_on_one_connection() {
    let mut state = scheduler(4);
    submit_granted(&mut state, want(1, 1), epoch());
    let second = state.submit(&want(1, 1), epoch());
    assert_eq!(deny_reason(&second), Some(DenyReason::INVALID_REQUEST));
    assert_eq!(state.free_device_count(), 3);

    // Same rule while queued.
    let mut tight = scheduler(1);
    submit_granted(&mut tight, want(1, 1), epoch());
    submit_queued(&mut tight, want(2, 1), epoch());
    assert!(deny_reason(&tight.submit(&want(2, 1), epoch())).is_some());
    assert_eq!(tight.queue_size(), 1);
}

#[test]
fn cancels_a_queued_request_on_release() {
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, want(2, 1), epoch());
    submit_queued(&mut state, want(3, 1), epoch());

    assert!(state.release(2));
    assert_eq!(state.queue_size(), 1);
    assert!(!state.is_queued(2));
    assert_eq!(state.queue_position(3), Some(0));

    state.release(1);
    let transitions = state.tick(at(1));
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].id, 3);
}

#[test]
fn reports_the_earliest_deadline() {
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    assert_eq!(state.next_deadline(), None);

    submit_queued(&mut state, want_by(2, 1, at(500)), epoch());
    submit_queued(&mut state, want_by(3, 1, at(200)), epoch());
    submit_queued(&mut state, want(4, 1), epoch());
    assert_eq!(state.next_deadline(), Some(at(200)));

    state.release(3);
    assert_eq!(state.next_deadline(), Some(at(500)));
}

#[test]
fn never_double_books_under_churn() {
    let mut state = scheduler(4);
    for id in 1..=200u64 {
        let size = 1 + (id % 3) as u32;
        let uid = 1000 + (id % 5) as u32;
        let _ = state.submit(&by(uid, want(id, size)), at(id as i64));
        if id % 3 == 0 {
            state.release(id - 2);
        }
        if id % 7 == 0 {
            state.release(id);
        }
        for transition in state.tick(at(id as i64)) {
            if let ReservationOutcome::Granted { gpus } = &transition.outcome {
                assert!(!gpus.is_empty());
            }
        }
        check_no_double_booking(&state);
    }
}

#[test]
fn frees_every_device_once_all_holders_release() {
    let mut state = scheduler(3);
    submit_granted(&mut state, want(1, 1), epoch());
    submit_granted(&mut state, want(2, 2), epoch());
    assert_eq!(state.free_device_count(), 0);

    state.release(1);
    state.release(2);
    assert_eq!(state.free_device_count(), 3);
    assert!(state.holdings().is_empty());
    assert!((0..3).all(|i| !state.is_held(Slot(i))));
}

#[test]
fn caps_how_many_devices_one_user_may_hold() {
    let mut state = limited(10, 4, 4);
    submit_granted(&mut state, want(1, 4), epoch());

    // A second request would take this uid to 6 devices: over the cap.
    let outcome = state.submit(&want(2, 2), epoch());
    assert_eq!(deny_reason(&outcome), Some(DenyReason::INVALID_REQUEST));
    assert_eq!(state.queue_size(), 0);
    assert_eq!(state.free_device_count(), 6);

    // Releasing the first makes room again.
    state.release(1);
    submit_granted(&mut state, want(3, 2), epoch());
}

#[test]
fn rejects_a_single_request_above_the_per_user_cap() {
    let mut state = limited(10, 4, 4);
    assert!(deny_reason(&state.submit(&want(1, 5), epoch())).is_some());
    assert_eq!(state.free_device_count(), 10);
}

#[test]
fn caps_how_many_requests_one_user_may_queue() {
    let mut state = limited(1, 8, 2);
    submit_granted(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, want(2, 1), epoch());
    submit_queued(&mut state, want(3, 1), epoch());

    let outcome = state.submit(&want(4, 1), epoch());
    assert_eq!(deny_reason(&outcome), Some(DenyReason::INVALID_REQUEST));
    assert_eq!(state.queue_size(), 2);

    // A queued slot frees up when one request is granted or cancelled.
    state.release(1);
    let _ = state.tick(at(1));
    submit_queued(&mut state, want(4, 1), epoch());
    assert_eq!(state.queue_size(), 2);
}

#[test]
fn caps_are_per_user_not_global() {
    let mut state = limited(8, 4, 4);
    submit_granted(
        &mut state,
        ReservationRequest {
            uid: 2000,
            ..want(1, 4)
        },
        epoch(),
    );
    submit_granted(&mut state, want(2, 4), epoch());
    assert_eq!(state.free_device_count(), 0);
}

#[test]
fn tick_rechecks_the_per_user_held_cap_when_granting() {
    // A holder from another uid takes 8 of 12 devices, leaving 4 free: too
    // few for either request below, so both pass submit's check and queue.
    let mut state = limited(12, 8, 8);
    submit_granted(
        &mut state,
        ReservationRequest {
            uid: 2000,
            ..want(1, 8)
        },
        epoch(),
    );
    submit_queued(&mut state, want(2, 6), epoch());
    submit_queued(&mut state, want(3, 6), epoch());

    state.release(1);
    let transitions = state.tick(at(1));
    // The first is granted (6 <= 8); the second would take the uid to 12.
    assert_eq!(transitions.len(), 1);
    assert_eq!(transitions[0].id, 2);
    assert!(is_granted(&transitions[0].outcome));
    assert_eq!(state.queue_size(), 1);
    assert!(state.is_queued(3));

    state.release(2);
    let again = state.tick(at(2));
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].id, 3);
    assert!(is_granted(&again[0].outcome));
}

fn by(uid: u32, request: ReservationRequest) -> ReservationRequest {
    ReservationRequest { uid, ..request }
}

/// Queues `requests` while every free device looks busy, then clears the
/// usage, so `tick` decides them all at once.
fn queue_while_busy(state: &mut Scheduler, busy: &[u32], requests: &[ReservationRequest]) {
    state.set_observed_usage(false, &slots(busy));
    for request in requests {
        submit_queued(state, request.clone(), epoch());
    }
    state.set_observed_usage(false, &[]);
}

fn ids(transitions: &[ReservationTransition]) -> Vec<RequestId> {
    transitions.iter().map(|t| t.id).collect()
}

/// uid 1000 holds 8 of 10 devices, its cap. Its request 3 waits at the head
/// of the queue for its own holders, not for devices; slots 8 and 9 are free.
fn queue_head_over_its_own_cap() -> Scheduler {
    let mut state = limited(10, 8, 8);
    submit_granted(&mut state, want(1, 7), epoch());
    // Each fits the cap when submitted; once 2 is granted, 3 does not.
    queue_while_busy(&mut state, &[7, 8, 9], &[want(2, 1), want(3, 1)]);
    assert_eq!(ids(&state.tick(at(1))), [2]);
    assert_eq!(state.queue_position(3), Some(0));
    state
}

#[test]
fn a_head_over_its_users_cap_does_not_stall_other_users() {
    let mut state = queue_head_over_its_own_cap();
    // Another user's request behind it is granted from the free devices,
    // whether it arrives now or was already waiting.
    submit_granted(&mut state, by(2000, want(4, 1)), at(2));
    queue_while_busy(&mut state, &[9], &[by(2000, want(5, 1))]);
    assert_eq!(ids(&state.tick(at(3))), [5]);
    assert_eq!(
        state.queue_position(3),
        Some(0),
        "the capped request keeps its place"
    );
}

#[test]
fn a_capped_request_is_granted_once_its_user_releases() {
    let mut state = queue_head_over_its_own_cap();
    state.release(1);
    assert_eq!(ids(&state.tick(at(2))), [3]);
}

#[test]
fn a_user_whose_request_is_passed_over_keeps_its_later_requests_in_order() {
    // uid 1000 holds 6 of 10 with a cap of 8, and queues 2 (1 GPU), 3 (2 GPUs)
    // and 4 (1 GPU). Once 2 is granted, 3 is over the cap. 4 would fit, but
    // must not overtake its own user's earlier request.
    let mut state = limited(10, 8, 8);
    submit_granted(&mut state, want(1, 6), epoch());
    queue_while_busy(
        &mut state,
        &[6, 7, 8, 9],
        &[want(2, 1), want(3, 2), want(4, 1)],
    );
    assert_eq!(ids(&state.tick(at(1))), [2]);
    assert!(state.is_queued(3) && state.is_queued(4));

    // A new request of that user queues behind them too, although it fits
    // the cap (7 + 1) and devices are free: only because 3 was passed over.
    submit_queued(&mut state, want(5, 1), at(2));

    // Its holders release: requests are granted in that user's order.
    state.release(1);
    assert_eq!(ids(&state.tick(at(3))), [3, 4, 5]);
}

#[test]
fn once_the_capped_user_releases_the_queue_is_served_in_order() {
    // Once the capped user releases, everyone is served by load: uid 3000
    // (nothing held), uid 1000 (1 left) and uid 2000 (2 held).
    let mut state = queue_head_over_its_own_cap();
    submit_granted(&mut state, by(2000, want(4, 2)), at(2));
    submit_queued(&mut state, by(2000, want(5, 2)), at(2));
    submit_queued(&mut state, by(3000, want(6, 1)), at(2));
    state.release(1);
    assert_eq!(ids(&state.tick(at(3))), [6, 3, 5]);
}

#[test]
fn a_grant_that_caps_a_waiter_lets_the_walk_serve_the_one_behind() {
    // 7 devices, cap 3. uid 1000 holds 2 and uid 4000 holds 2; 3 are free.
    // uid 2000 (load 0) is served first, then uid 1000's request 2, after
    // which its request 3 would pass the cap and is passed over.
    let mut state = limited(7, 3, 8);
    submit_granted(&mut state, want(1, 2), epoch());
    submit_granted(&mut state, by(4000, want(5, 2)), epoch());
    queue_while_busy(
        &mut state,
        &[4, 5, 6],
        &[want(2, 1), want(3, 1), by(2000, want(4, 1))],
    );
    assert_eq!(ids(&state.tick(at(1))), [4, 2]);
    assert!(state.is_queued(3));
}

#[test]
fn a_device_wait_behind_a_capped_request_is_not_overtaken() {
    let mut state = queue_head_over_its_own_cap();
    submit_granted(&mut state, by(2000, want(4, 2)), at(2));
    submit_queued(&mut state, by(2000, want(5, 2)), at(2));
    submit_queued(&mut state, by(3000, want(6, 1)), at(2));
    assert!(state.tick(at(3)).is_empty());
    // By load: uid 3000 (nothing), uid 2000 (2 held), uid 1000 (8 held).
    assert_eq!(state.queue_position(6), Some(0));
    assert_eq!(state.queue_position(5), Some(1));
    assert_eq!(state.queue_position(3), Some(2));
}

#[test]
fn per_user_counters_unwind_on_release_and_expiry() {
    // If a counter leaks on any exit path, a legitimate user is eventually
    // locked out by phantom usage. Each round exercises held release, queued
    // release, and queued expiry.
    let mut state = limited(1, 2, 2);
    for round in 0..5 {
        submit_granted(&mut state, want(1, 1), epoch());
        submit_queued(&mut state, want(2, 1), epoch());
        state.release(2);
        state.release(1);
        let _ = state.tick(at(round));

        submit_granted(&mut state, want(3, 1), epoch());
        submit_queued(&mut state, want_by(4, 1, at(round + 1)), epoch());
        let _ = state.tick(at(round + 1));
        state.release(3);
        let _ = state.tick(at(round + 1));
        assert_eq!(state.free_device_count(), 1);
        assert_eq!(state.queue_size(), 0);
    }
    assert!(state.held_per_uid.is_empty());
    assert!(state.queued_per_uid.is_empty());
}

#[test]
fn held_reservations_are_reported_one_row_per_reservation() {
    let mut state = scheduler(4);
    submit_granted(&mut state, want(1, 2), epoch());
    submit_granted(&mut state, want(2, 1), epoch());

    let held = state.held_reservations();
    assert_eq!(held.len(), 2);
    // holdings() is one entry per device, so the two views must differ.
    assert_eq!(state.holdings().len(), 3);

    assert_eq!(held[0].id, 1);
    assert_eq!(held[0].uid, 1000);
    assert_eq!(held[0].gpus, slots(&[0, 1]));
    assert_eq!(held[0].num_gpus, 2);
    assert_eq!(held[0].position, 0);
    assert_eq!(held[1].id, 2);
    assert_eq!(held[1].gpus, slots(&[2]));
}

#[test]
fn held_reservations_come_back_in_a_stable_order() {
    let mut state = scheduler(8);
    for id in (1..=8).rev() {
        submit_granted(&mut state, want(id, 1), epoch());
    }
    let ids: Vec<_> = state.held_reservations().iter().map(|s| s.id).collect();
    assert_eq!(ids, (1..=8).collect::<Vec<_>>());
}

#[test]
fn a_held_reservation_records_when_it_was_granted() {
    let mut state = scheduler(2);
    submit_granted(&mut state, want(1, 1), at(500));
    let held = state.held_reservations();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].since, at(500));
}

#[test]
fn the_queue_can_be_enumerated_in_service_order() {
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, want(2, 1), at(100));
    submit_queued(&mut state, by(2000, want(3, 1)), at(200));

    let queued = state.queued_reservations();
    assert_eq!(queued.len(), 2);
    // By load: the later request of a user with nothing is served first.
    assert_eq!(queued[0].id, 3);
    assert_eq!(queued[0].position, 0);
    assert_eq!(queued[0].since, at(200));
    assert_eq!(queued[1].id, 2);
    assert_eq!(queued[1].position, 1);
    assert_eq!(queued[1].since, at(100));
    // A queued request holds nothing yet, but knows what it wants.
    assert!(queued[0].gpus.is_empty());
    assert_eq!(queued[0].num_gpus, 1);
}

#[test]
fn an_explicit_list_request_reports_the_number_of_devices_it_wants() {
    let mut state = scheduler(4);
    submit_granted(&mut state, want(1, 4), epoch());
    submit_queued(
        &mut state,
        ReservationRequest {
            uid: 1001,
            ..want_exactly(2, &[1, 2])
        },
        epoch(),
    );
    let queued = state.queued_reservations();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].num_gpus, 2);
    assert_eq!(queued[0].uid, 1001);
}

#[test]
fn releasing_a_reservation_drops_it_from_the_held_view() {
    let mut state = scheduler(2);
    submit_granted(&mut state, want(1, 1), epoch());
    assert_eq!(state.held_reservations().len(), 1);
    assert!(state.release(1));
    assert!(state.held_reservations().is_empty());
    assert!(state.queued_reservations().is_empty());
}

#[test]
fn a_granted_request_leaves_the_queue_view_and_enters_the_held_view() {
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, want(2, 1), at(100));
    assert_eq!(state.queued_reservations().len(), 1);

    assert!(state.release(1));
    assert_eq!(state.tick(at(200)).len(), 1);
    assert!(state.queued_reservations().is_empty());
    let held = state.held_reservations();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].id, 2);
    // The clock reading is the grant, not the original submission.
    assert_eq!(held[0].since, at(200));
}

#[test]
fn an_empty_scheduler_reports_empty_views_rather_than_failing() {
    let state = scheduler(2);
    assert!(state.held_reservations().is_empty());
    assert!(state.queued_reservations().is_empty());
}

fn want_from(id: RequestId, count: u32, candidates: &[u32]) -> ReservationRequest {
    ReservationRequest {
        candidates: Some(slots(candidates)),
        ..want(id, count)
    }
}

#[test]
fn allocates_only_from_a_requests_candidates() {
    let mut state = scheduler(4);
    let outcome = state.submit(&want_from(1, 2, &[3, 1]), epoch());
    assert_eq!(granted_gpus(&outcome), [1, 3]);
    check_no_double_booking(&state);
}

#[test]
fn queues_a_candidate_request_while_other_devices_are_free() {
    let mut state = scheduler(3);
    submit_granted(&mut state, want_exactly(1, &[2]), epoch());
    submit_queued(&mut state, want_from(2, 1, &[2]), epoch());

    assert!(state.release(1));
    let transitions = state.tick(at(1));
    assert_eq!(transitions.len(), 1);
    assert_eq!(granted_gpus(&transitions[0].outcome), [2]);
    check_no_double_booking(&state);
}

#[test]
fn refuses_more_devices_than_the_candidates_hold() {
    let mut state = scheduler(4);
    let outcome = state.submit(&want_from(1, 2, &[0]), epoch());
    assert_eq!(
        outcome,
        ReservationOutcome::Denied {
            reason: DenyReason::UNSATISFIABLE,
            detail: "requested 2 GPUs but only 1 GPU visible to the client and managed by gpudogd"
                .into(),
        }
    );
}

#[test]
fn lowering_the_caps_keeps_holders_and_waiters_that_still_fit() {
    let mut state = limited(4, 8, 4);
    submit_granted(&mut state, want(1, 3), epoch());
    submit_queued(&mut state, want(2, 2), epoch());
    submit_queued(&mut state, want(3, 2), epoch());
    state.set_limits(SchedulerLimits {
        max_held_per_uid: 2,
        max_queued_per_uid: 1,
    });
    // Nothing held is taken back, and both waiters still fit the new cap on
    // their own, so they keep waiting even though the uid is over the
    // queued cap now.
    assert!(state.tick(epoch()).is_empty());
    assert!(state.holds(1));
    assert_eq!(state.queue_size(), 2);
    // A new request meets the new caps.
    let outcome = state.submit(&want(4, 1), epoch());
    assert_eq!(deny_reason(&outcome), Some(DenyReason::INVALID_REQUEST));
    check_no_double_booking(&state);
}

#[test]
fn a_request_over_the_held_cap_on_its_own_says_so() {
    // Before, this read "already holds 0 GPUs", which blamed held GPUs.
    let mut state = limited(4, 2, 8);
    let outcome = state.submit(&want(1, 3), epoch());
    assert!(
        matches!(&outcome, ReservationOutcome::Denied { reason, detail }
            if *reason == DenyReason::INVALID_REQUEST
                && detail == "requested 3 GPUs but the per-user cap is 2 GPUs"),
        "{outcome:?}"
    );
    // More GPUs than the machine has is still reported as that.
    let outcome = state.submit(&want(2, 5), epoch());
    assert!(
        matches!(&outcome, ReservationOutcome::Denied { detail, .. }
            if detail.starts_with("requested 5 GPUs but this host has only")),
        "{outcome:?}"
    );
}

#[test]
fn a_waiter_over_a_lowered_held_cap_is_denied_instead_of_blocking_the_queue() {
    let mut state = limited(4, 8, 4);
    submit_granted(&mut state, want(1, 4), epoch());
    submit_queued(&mut state, want(2, 3), epoch());
    let mut other = want(3, 1);
    other.uid = 2000;
    submit_queued(&mut state, other, epoch());
    state.set_limits(SchedulerLimits {
        max_held_per_uid: 2,
        max_queued_per_uid: 4,
    });
    let transitions = state.tick(epoch());
    assert_eq!(transitions.len(), 1, "{transitions:?}");
    assert_eq!(transitions[0].id, 2);
    assert!(matches!(
        &transitions[0].outcome,
        ReservationOutcome::Denied { reason, detail }
            if *reason == DenyReason::INVALID_REQUEST
                && detail == "requested 3 GPUs but the per-user cap is 2 GPUs"
    ));
    assert!(!state.is_queued(2));
    assert!(state.holds(1));
    // The request behind it is now the head and is granted when GPUs free up.
    assert!(state.release(1));
    let transitions = state.tick(epoch());
    assert_eq!(transitions.len(), 1, "{transitions:?}");
    assert_eq!(transitions[0].id, 3);
    assert!(is_granted(&transitions[0].outcome));
    check_no_double_booking(&state);
}

fn transitions_of(state: &mut Scheduler) -> Vec<(RequestId, Option<DenyReason>, String)> {
    state
        .tick(at(0))
        .into_iter()
        .map(|transition| match transition.outcome {
            ReservationOutcome::Denied { reason, detail } => (transition.id, Some(reason), detail),
            other => (transition.id, None, format!("{other:?}")),
        })
        .collect()
}

#[test]
fn removing_a_free_device_renumbers_the_rest_and_never_allocates_it() {
    let mut state = scheduler(3);
    assert_eq!(state.set_managed(&[true, false, true]), []);
    assert_eq!(state.device_count(), 2);
    assert_eq!(state.shown(), slots(&[0, 2]));
    assert_eq!(state.display_of(Slot(2)), Some(1));
    assert_eq!(state.display_of(Slot(1)), None);
    assert_eq!(state.slot_state(Slot(1)), Some(SlotState::Unmanaged));
    assert_eq!(granted_gpus(&state.submit(&want(1, 2), at(0))), [0, 2]);
    assert_eq!(
        deny_reason(&state.submit(&want(2, 1), at(0))),
        None,
        "queued, not denied: two GPUs are managed"
    );
}

#[test]
fn a_held_device_that_is_removed_drains_and_keeps_its_number_until_the_next_change() {
    let mut state = scheduler(2);
    submit_granted(&mut state, want_exactly(1, &[0]), at(0));
    assert_eq!(state.set_managed(&[false, true]), []);
    assert_eq!(state.slot_state(Slot(0)), Some(SlotState::Retiring));
    assert!(state.holds(1), "a reload never takes a device back");
    assert_eq!(state.device_count(), 2);
    assert_eq!(state.free_device_count(), 1);

    state.release(1);
    assert!(state.tick(at(1)).is_empty());
    // Released, but still shown, so GPU 1 keeps its number.
    assert_eq!(state.device_count(), 2);
    assert_eq!(state.display_of(Slot(1)), Some(1));
    assert_eq!(state.free_device_count(), 1);
    submit_granted(&mut state, want(2, 1), at(2));
    submit_queued(&mut state, want(3, 1), at(2));

    // The next change drops it and renumbers.
    assert_eq!(state.set_managed(&[false, true]), []);
    assert_eq!(state.slot_state(Slot(0)), Some(SlotState::Unmanaged));
    assert_eq!(state.display_of(Slot(1)), Some(0));
    check_no_double_booking(&state);
}

#[test]
fn readding_a_device_reports_it_as_new() {
    let mut state = scheduler(2);
    submit_granted(&mut state, want_exactly(1, &[1]), at(0));
    assert_eq!(state.set_managed(&[true, false]), []);
    assert_eq!(state.set_managed(&[true, true]), [Slot(1)]);
    assert_eq!(state.slot_state(Slot(1)), Some(SlotState::Managed));
    assert!(state.holds(1));
}

#[test]
fn waiters_a_removal_makes_impossible_are_denied_as_at_submit() {
    let mut state = scheduler(3);
    submit_granted(&mut state, want(1, 3), at(0));
    submit_queued(&mut state, want_exactly(2, &[1]), at(0));
    submit_queued(&mut state, want(3, 3), at(0));
    submit_queued(&mut state, want_from(4, 1, &[0, 1]), at(0));
    submit_queued(&mut state, want_from(5, 1, &[1]), at(0));
    submit_queued(&mut state, want(6, 2), at(0));
    let _ = state.set_managed(&[true, false, true]);
    assert_eq!(
        transitions_of(&mut state),
        [
            (
                2,
                Some(DenyReason::UNSATISFIABLE),
                "GPU-static-1 is not managed by this daemon".to_owned()
            ),
            (
                3,
                Some(DenyReason::UNSATISFIABLE),
                "requested 3 GPUs but this host has only 2 GPUs".to_owned()
            ),
            (
                5,
                Some(DenyReason::UNSATISFIABLE),
                "requested 1 GPU but only 0 GPUs visible to the client and managed by gpudogd"
                    .to_owned()
            ),
        ]
    );
    // Still satisfiable: a candidate is left, or enough GPUs are.
    assert!(state.is_queued(4));
    assert!(state.is_queued(6));
    assert_eq!(state.queue_size(), 2);
}

#[test]
fn a_candidate_added_by_a_reload_becomes_allocatable() {
    let mut state = scheduler(2);
    let _ = state.set_managed(&[true, false]);
    submit_granted(&mut state, want(1, 1), at(0));
    submit_queued(&mut state, want_from(2, 1, &[0, 1]), at(0));
    assert_eq!(state.set_managed(&[true, true]), [Slot(1)]);
    let transitions = state.tick(at(1));
    assert_eq!(granted_gpus(&transitions[0].outcome), [1]);
}

// A waiting request blocks only the GPUs it could use, and a request that
// waits on its own user blocks only that user's GPUs (security review S1).

/// Marks `busy` slots observed in use by the given uids (`None`: a process
/// of unknown uid).
fn observe(state: &mut Scheduler, busy: &[(u32, &[Option<u32>])]) {
    let slots: Vec<Slot> = busy.iter().map(|&(slot, _)| Slot(slot)).collect();
    let owners: Vec<BusyOwners> = busy
        .iter()
        .map(|&(slot, uids)| BusyOwners {
            slot: Slot(slot),
            uids: uids.iter().flatten().copied().collect(),
        })
        .collect();
    state.set_busy_owners(&owners);
    state.set_observed_usage(false, &slots);
}

#[test]
fn waiting_for_your_own_gpu_holds_up_nobody_else() {
    // `gpudog run -g 0` while holding GPU 0, e.g. a nested `gpudog run`.
    let mut state = scheduler(2);
    submit_granted(&mut state, want_exactly(1, &[0]), epoch());
    submit_queued(&mut state, want_exactly(2, &[0]), epoch());
    assert_eq!(
        granted_gpus(&state.submit(&by(2000, want(3, 1)), epoch())),
        [1]
    );
    // Sequential jobs on one GPU still work: 2 gets GPU 0 once 1 ends.
    state.release(1);
    assert_eq!(ids(&state.tick(at(1))), [2]);
    check_no_double_booking(&state);
}

#[test]
fn waiting_for_another_users_gpu_blocks_only_that_gpu() {
    let mut state = scheduler(3);
    submit_granted(&mut state, by(2000, want_exactly(1, &[0])), epoch());
    submit_queued(&mut state, want_exactly(2, &[0]), epoch());
    submit_queued(&mut state, by(4000, want_from(3, 1, &[0])), epoch());
    // Neither waiter could use GPUs 1 and 2, so a later request gets one.
    assert_eq!(
        granted_gpus(&state.submit(&by(3000, want(4, 1)), epoch())),
        [1]
    );
    check_no_double_booking(&state);
}

#[test]
fn waiting_for_a_gpu_in_use_outside_gpudog_blocks_only_that_gpu() {
    let mut state = scheduler(3);
    observe(&mut state, &[(0, &[Some(2000)])]);
    submit_queued(&mut state, want_exactly(1, &[0]), epoch());
    assert_eq!(
        granted_gpus(&state.submit(&by(3000, want(2, 1)), epoch())),
        [1]
    );
}

#[test]
fn holding_one_gpu_and_waiting_for_many_reserves_only_your_own() {
    // 8 GPUs. uid 1000 holds 0 and asks for 0 to 6; uid 2000 holds 7. If the
    // request reserved 1 to 6 it would keep them idle while 1000 holds 0.
    let mut state = scheduler(8);
    submit_granted(&mut state, want_exactly(1, &[0]), epoch());
    submit_granted(&mut state, by(2000, want_exactly(2, &[7])), epoch());
    submit_queued(&mut state, want_exactly(3, &[0, 1, 2, 3, 4, 5, 6]), epoch());
    assert_eq!(
        granted_gpus(&state.submit(&by(2000, want(4, 1)), epoch())),
        [1]
    );
    check_no_double_booking(&state);
}

#[test]
fn a_count_that_needs_your_own_gpu_reserves_only_your_own() {
    // 4 GPUs. uid 1000 holds 0, and GPU 3 is busy with a process of unknown
    // uid. `-n 4` cannot be met without GPU 0, so it reserves only GPU 0.
    let mut state = scheduler(4);
    submit_granted(&mut state, want_exactly(1, &[0]), epoch());
    observe(&mut state, &[(3, &[None])]);
    submit_queued(&mut state, want(2, 4), epoch());
    assert_eq!(
        granted_gpus(&state.submit(&by(2000, want(3, 1)), epoch())),
        [1]
    );
}

#[test]
fn a_count_on_a_small_host_that_needs_your_own_gpu_does_not_stall_it() {
    // 4 GPUs, fewer than the cap: holding 1 and asking for all 4.
    let mut state = scheduler(4);
    submit_granted(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, want(2, 4), epoch());
    submit_granted(&mut state, by(2000, want(3, 2)), epoch());
    check_no_double_booking(&state);
}

#[test]
fn a_gpu_busy_with_your_own_outside_process_counts_as_yours() {
    // uid 1000 runs a process on GPU 0 outside GPUDog and asks for 0 to 6.
    let mut state = scheduler(8);
    observe(&mut state, &[(0, &[Some(1000)])]);
    submit_queued(&mut state, want_exactly(1, &[0, 1, 2, 3, 4, 5, 6]), epoch());
    assert_eq!(
        granted_gpus(&state.submit(&by(2000, want(2, 1)), epoch())),
        [1]
    );
}

#[test]
fn a_gpu_busy_with_your_outside_process_counts_towards_your_load() {
    let mut state = scheduler(2);
    observe(&mut state, &[(0, &[Some(1000)]), (1, &[Some(9000)])]);
    submit_queued(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, by(2000, want(2, 1)), epoch());
    // uid 1000 uses one GPU already; uid 2000 none.
    assert_eq!(state.queue_position(2), Some(0));
    assert_eq!(state.queue_position(1), Some(1));
}

#[test]
fn a_gpu_shared_with_another_user_is_still_yours() {
    // Another user starting a process next to yours must not turn your
    // wait for your own GPU into one that holds up everyone behind you.
    let mut state = scheduler(4);
    submit_granted(&mut state, by(9000, want_exactly(1, &[3])), epoch());
    observe(&mut state, &[(0, &[Some(1000), Some(2000)])]);
    submit_queued(&mut state, want_exactly(2, &[0, 1, 2]), epoch());
    assert_eq!(
        granted_gpus(&state.submit(&by(9000, want(3, 1)), epoch())),
        [1]
    );
}

#[test]
fn waiting_behind_an_outside_process_holds_up_only_its_gpu() {
    // GPU 0 is busy outside GPUDog, with no end in sight: another user's
    // process, or one whose user is unknown. A user who holds nothing asks
    // for all 4 GPUs; it keeps only GPU 0, so GPUs 1 to 3 stay usable.
    for owners in [&[Some(2000)][..], &[None][..], &[Some(2000), None][..]] {
        for request in [want(2, 4), want_exactly(2, &[0, 1, 2, 3])] {
            let mut state = scheduler(4);
            submit_granted(&mut state, by(9000, want_exactly(1, &[3])), epoch());
            observe(&mut state, &[(0, owners)]);
            submit_queued(&mut state, request.clone(), epoch());
            assert_eq!(
                granted_gpus(&state.submit(&by(9000, want(3, 1)), epoch())),
                [1],
                "{owners:?} {request:?}"
            );
        }
    }
}

#[test]
fn waiting_behind_another_users_reservation_still_holds_up_what_it_needs() {
    // Unlike an outside process, a reservation ends when its job does, so
    // the large request keeps the free GPU for itself meanwhile.
    let mut state = scheduler(3);
    submit_granted(&mut state, by(2000, want_exactly(1, &[0])), epoch());
    submit_granted(&mut state, by(9000, want_exactly(2, &[2])), epoch());
    submit_queued(&mut state, want_exactly(3, &[0, 1]), epoch());
    submit_queued(&mut state, by(9000, want(4, 1)), epoch());
    assert_eq!(state.free_device_count(), 1);
}

#[test]
fn one_user_cannot_reserve_more_gpus_with_more_requests() {
    // uid 9000 holds 4 to 7. uid 1000 queues three requests, each pairing a
    // held GPU with a free one. Only its first waiting request reserves.
    let mut state = scheduler(8);
    submit_granted(
        &mut state,
        by(9000, want_exactly(1, &[4, 5, 6, 7])),
        epoch(),
    );
    submit_queued(&mut state, want_exactly(2, &[4, 0]), epoch());
    submit_queued(&mut state, want_exactly(3, &[5, 1]), epoch());
    submit_queued(&mut state, want_exactly(4, &[6, 2]), epoch());
    assert_eq!(
        granted_gpus(&state.submit(&by(2000, want(5, 1)), epoch())),
        [1]
    );
    check_no_double_booking(&state);
}

#[test]
fn a_large_request_of_a_user_with_nothing_still_holds_up_what_it_needs() {
    // The trade-off that keeps large requests from starving: uid 1000, who
    // holds nothing, waits for both GPUs, so the free one is kept for it.
    let mut state = scheduler(2);
    submit_granted(&mut state, by(2000, want(1, 1)), epoch());
    submit_queued(&mut state, want(2, 2), epoch());
    submit_queued(&mut state, by(3000, want(3, 1)), epoch());
    state.release(1);
    assert_eq!(ids(&state.tick(at(1))), [2]);
}

#[test]
fn cancelling_a_request_whose_service_position_differs_removes_that_request() {
    let mut state = scheduler(1);
    submit_granted(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, want(2, 1), epoch());
    submit_queued(&mut state, by(2000, want(3, 1)), epoch());
    assert_eq!(state.queue_position(3), Some(0));
    assert!(state.release(3));
    assert!(state.is_queued(2) && !state.is_queued(3));
    assert_eq!(state.queue_position(2), Some(0));
}

#[test]
fn a_waiter_whose_candidates_grow_on_reload_keeps_what_it_needs() {
    // uid 1000 waits for 4 of GPUs 0 to 4 while uid 2000 holds GPU 0, and
    // GPU 4 is not managed yet. A reload manages it, and the service marks
    // it busy until observed. The waiter still cannot be met, and keeps the
    // free GPUs 1 to 3 from a later request.
    let mut state = scheduler(5);
    assert_eq!(state.set_managed(&[true, true, true, true, false]), []);
    submit_granted(&mut state, by(2000, want_exactly(1, &[0])), epoch());
    submit_queued(&mut state, want_from(2, 4, &[0, 1, 2, 3, 4]), epoch());
    assert_eq!(
        state.set_managed(&[true, true, true, true, true]),
        [Slot(4)]
    );
    state.set_observed_usage(false, &[Slot(4)]);
    submit_queued(&mut state, by(3000, want(3, 1)), epoch());
    assert_eq!(state.free_device_count(), 3);
    assert!(state.is_queued(2) && state.is_queued(3));
}

#[test]
fn a_reload_forgets_who_was_seen_on_a_slot() {
    // GPU 0 was busy with uid 1000's process, then left and rejoined the
    // managed set before any new observation: it is nobody's until then.
    let mut state = scheduler(2);
    observe(&mut state, &[(0, &[Some(1000)])]);
    assert_eq!(state.set_managed(&[false, true]), []);
    assert_eq!(state.set_managed(&[true, true]), [Slot(0)]);
    state.set_observed_usage(false, &[Slot(0), Slot(1)]);
    submit_queued(&mut state, want(1, 1), epoch());
    submit_queued(&mut state, by(2000, want(2, 1)), epoch());
    // uid 1000 no longer counts GPU 0 as its load: arrival order decides.
    assert_eq!(state.queue_position(1), Some(0));
}

#[test]
fn denials_decided_by_a_reload_are_reported_while_grants_are_paused() {
    let mut state = scheduler(2);
    submit_granted(&mut state, want(1, 2), epoch());
    submit_queued(&mut state, want(2, 2), epoch());
    state.set_observed_usage(true, &[]);
    state.set_limits(SchedulerLimits {
        max_held_per_uid: 1,
        ..SchedulerLimits::default()
    });
    let transitions = state.tick(at(1));
    assert_eq!(ids(&transitions), [2]);
    assert!(matches!(
        transitions[0].outcome,
        ReservationOutcome::Denied { .. }
    ));
}

#[test]
fn a_walk_repeated_on_the_same_state_changes_nothing() {
    // Random small states: after any submit and the tick that follows it,
    // another tick grants nothing, and a request `submit` queued is not
    // granted by that tick unless something else changed. Also checks that
    // no device is ever booked twice.
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    for round in 0..200 {
        let mut state = limited(4, 3, 4);
        let mut live: Vec<RequestId> = Vec::new();
        for step in 0..30u64 {
            let id = round * 100 + step + 1;
            match next(4) {
                0 if !live.is_empty() => {
                    let victim = live.remove(next(live.len() as u64) as usize);
                    state.release(victim);
                }
                _ => {
                    let uid = 1000 + next(3) as u32;
                    let request = match next(3) {
                        0 => by(uid, want(id, 1 + next(3) as u32)),
                        1 => by(uid, want_exactly(id, &[next(4) as u32])),
                        _ => by(uid, want_from(id, 1, &[next(4) as u32, next(4) as u32])),
                    };
                    // What `tick` would decide for it once queued, on a
                    // copy: `submit` must agree.
                    let mut twin = state.clone();
                    twin.queue.push(QueuedRequest {
                        request: request.clone(),
                        queued_at: at(step as i64),
                        seq: twin.next_seq,
                    });
                    twin.next_seq += 1;
                    *twin.queued_per_uid.entry(request.uid).or_default() += 1;
                    let twin_granted = twin.tick(at(step as i64)).iter().any(|t| {
                        t.id == id && matches!(t.outcome, ReservationOutcome::Granted { .. })
                    });
                    let outcome = state.submit(&request, at(step as i64));
                    if !matches!(outcome, ReservationOutcome::Denied { .. }) {
                        assert_eq!(
                            matches!(outcome, ReservationOutcome::Granted { .. }),
                            twin_granted,
                            "round {round}: submit and queue-then-tick disagree on {id}"
                        );
                    }
                    let after = state.tick(at(step as i64));
                    if matches!(outcome, ReservationOutcome::Queued { .. }) {
                        assert!(
                            !after.iter().any(|t| t.id == id),
                            "round {round}: submit queued {id} but the next tick granted it"
                        );
                    }
                    if !matches!(outcome, ReservationOutcome::Denied { .. }) {
                        live.push(id);
                    }
                }
            }
            let _ = state.tick(at(step as i64));
            assert!(
                state.tick(at(step as i64)).is_empty(),
                "round {round}: a second walk changed something"
            );
            check_no_double_booking(&state);
        }
    }
}

#[test]
fn a_request_needing_your_own_and_an_outside_gpu_keeps_only_those() {
    // 4 GPUs: uid 1000 holds 0, 1 is busy outside GPUDog, 2 is free and
    // uid 9000 holds 3. `-n 3` cannot be met without GPU 0 or GPU 1, so it
    // keeps only those two, and GPU 2 stays usable for others.
    let mut state = scheduler(4);
    submit_granted(&mut state, want_exactly(1, &[0]), epoch());
    submit_granted(&mut state, by(9000, want_exactly(2, &[3])), epoch());
    observe(&mut state, &[(1, &[None])]);
    submit_queued(&mut state, want(3, 3), epoch());
    assert_eq!(
        granted_gpus(&state.submit(&by(9000, want(4, 1)), epoch())),
        [2]
    );
}

#[test]
fn a_request_needing_your_own_gpu_and_an_outside_one_does_not_idle_the_free_one() {
    // The whole-codebase review's case: 3 GPUs, uid 1000 holds 0, uid
    // 2000's process runs on 1 outside GPUDog, 2 is free.
    let mut state = scheduler(3);
    submit_granted(&mut state, want_exactly(1, &[0]), epoch());
    observe(&mut state, &[(1, &[Some(2000)])]);
    submit_queued(&mut state, want(2, 2), epoch());
    assert_eq!(
        granted_gpus(&state.submit(&by(2000, want(3, 1)), epoch())),
        [2]
    );
}

#[test]
fn a_request_with_its_own_gpu_that_others_could_still_meet_keeps_every_usable_gpu() {
    // 3 GPUs: uid 1000 holds 0, uid 2000 holds 1, 2 is free. uid 1000's
    // `-n 2` can be met from 1 and 2 without its own GPU, so it is an
    // ordinary wait and keeps GPU 2 from a later request of uid 2000, who
    // uses as many GPUs and so comes after it.
    let mut state = scheduler(3);
    submit_granted(&mut state, want_exactly(1, &[0]), epoch());
    submit_granted(&mut state, by(2000, want_exactly(2, &[1])), epoch());
    submit_queued(&mut state, want(3, 2), epoch());
    submit_queued(&mut state, by(2000, want(4, 1)), epoch());
    assert!(!state.is_held(Slot(2)));
}
