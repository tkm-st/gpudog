use super::*;
use crate::gpu_processes::{Ownership, ProcessRow};

const UID: u32 = 1001;
const OTHER_UID: u32 = 1002;

fn root() -> ReservationRoot {
    ReservationRoot {
        id: 7,
        identity: None,
        uid: UID,
        gpus: vec![0, 1],
    }
}

fn row(
    gpu: u32,
    owner: Option<u64>,
    uid: Option<u32>,
    ownership: Ownership,
    utilization: Option<u32>,
) -> ObservedProcess {
    ObservedProcess {
        row: ProcessRow {
            gpu: Some(gpu),
            pid: 100 + gpu,
            uid,
            used_memory: Some(1),
            command: None,
            ownership,
        },
        start_time: Some(1),
        owner,
        utilization,
    }
}

fn own(gpu: u32, percent: Option<u32>) -> ObservedProcess {
    row(gpu, Some(7), Some(UID), Ownership::Managed, percent)
}

#[test]
fn no_process_on_its_gpus_is_idle() {
    assert_eq!(activity(&root(), true, &[], 0), Activity::Idle);
}

#[test]
fn an_unresolved_root_is_unknown() {
    assert_eq!(activity(&root(), false, &[], 0), Activity::Unknown);
}

#[test]
fn a_busy_process_on_any_of_its_gpus_is_active() {
    let rows = [own(0, Some(0)), own(1, Some(80))];
    assert_eq!(activity(&root(), true, &rows, 0), Activity::Active);
}

#[test]
fn processes_at_or_below_the_threshold_are_idle() {
    let rows = [own(0, Some(0)), own(1, Some(5))];
    assert_eq!(activity(&root(), true, &rows, 5), Activity::Idle);
    assert_eq!(activity(&root(), true, &rows, 4), Activity::Active);
}

#[test]
fn unreported_utilization_with_a_process_is_active() {
    assert_eq!(
        activity(&root(), true, &[own(0, None)], 99),
        Activity::Active
    );
}

#[test]
fn its_own_process_on_another_gpu_is_active() {
    let rows = [row(2, Some(7), Some(UID), Ownership::WrongGpu, Some(0))];
    assert_eq!(activity(&root(), true, &rows, 0), Activity::Active);
}

#[test]
fn unattributed_work_on_its_gpu_is_unknown() {
    let rows = [row(0, None, None, Ownership::Unknown, Some(0))];
    assert_eq!(activity(&root(), true, &rows, 0), Activity::Unknown);
}

#[test]
fn work_outside_its_tree_is_unknown_whatever_its_uid() {
    // `gpudog run docker run --gpus ...`: the container's process descends
    // from containerd, not from `gpudog run`, and may run as root or as a
    // rootless container's subordinate uid; an MPS server may be root's.
    for uid in [Some(UID), Some(0), Some(OTHER_UID), Some(100_000), None] {
        let rows = [row(1, None, uid, Ownership::NotManaged, Some(90))];
        assert_eq!(
            activity(&root(), true, &rows, 0),
            Activity::Unknown,
            "{uid:?}"
        );
    }
}

#[test]
fn another_reservations_work_on_its_gpu_does_not_count() {
    let rows = [row(
        0,
        Some(8),
        Some(OTHER_UID),
        Ownership::WrongGpu,
        Some(90),
    )];
    assert_eq!(activity(&root(), true, &rows, 0), Activity::Idle);
}

#[test]
fn processes_on_other_gpus_are_ignored() {
    let rows = [row(3, None, Some(UID), Ownership::NotManaged, Some(90))];
    assert_eq!(activity(&root(), true, &rows, 0), Activity::Idle);
}

#[test]
fn the_clock_starts_at_the_grant_and_fires_once() {
    let granted = Instant::now();
    let limit = Duration::from_secs(60);
    let mut clock = IdleClock::new(limit, granted);
    assert_eq!(
        clock.observe(Activity::Idle, granted + Duration::from_secs(59)),
        None
    );
    assert_eq!(clock.observe(Activity::Idle, granted + limit), Some(limit));
    assert_eq!(
        clock.observe(Activity::Idle, granted + Duration::from_secs(120)),
        None
    );
}

#[test]
fn activity_restarts_the_clock() {
    let granted = Instant::now();
    let limit = Duration::from_secs(60);
    for interruption in [Activity::Active, Activity::Unknown] {
        let mut clock = IdleClock::new(limit, granted);
        let at = |s| granted + Duration::from_secs(s);
        assert_eq!(clock.observe(interruption, at(30)), None);
        assert_eq!(clock.idle(), Duration::ZERO);
        assert_eq!(clock.observe(Activity::Idle, at(40)), None);
        assert_eq!(clock.observe(Activity::Idle, at(99)), None);
        assert_eq!(clock.idle(), Duration::from_secs(59));
        assert_eq!(
            clock.observe(Activity::Idle, at(100)),
            Some(Duration::from_secs(60))
        );
    }
}

#[test]
fn a_reset_restarts_the_clock() {
    let granted = Instant::now();
    let at = |s| granted + Duration::from_secs(s);
    let mut clock = IdleClock::new(Duration::from_secs(60), granted);
    clock.reset();
    assert_eq!(clock.idle(), Duration::ZERO);
    assert_eq!(clock.observe(Activity::Idle, at(70)), None);
    assert_eq!(clock.observe(Activity::Idle, at(100)), None);
    assert_eq!(clock.idle(), Duration::from_secs(30));
}

#[test]
fn the_idle_time_stops_at_the_latest_observation() {
    let granted = Instant::now();
    let at = |s| granted + Duration::from_secs(s);
    let mut clock = IdleClock::new(Duration::from_secs(600), granted);
    // Nothing observed since the grant: nothing to show yet.
    assert_eq!(clock.idle(), Duration::ZERO);
    assert_eq!(clock.observe(Activity::Idle, at(20)), None);
    // However long the next observation takes, the shown time stays put.
    assert_eq!(clock.idle(), Duration::from_secs(20));
    assert_eq!(clock.observe(Activity::Idle, at(25)), None);
    assert_eq!(clock.idle(), Duration::from_secs(25));
}

#[test]
fn a_zero_limit_never_fires() {
    let granted = Instant::now();
    let mut clock = IdleClock::new(Duration::ZERO, granted);
    assert_eq!(
        clock.observe(Activity::Idle, granted + Duration::from_secs(3600)),
        None
    );
}

#[test]
fn a_stalled_observation_leaves_too_long_a_gap() {
    let second = Duration::from_secs(1);
    assert!(!observation_gap_too_long(Duration::from_secs(8), second));
    assert!(observation_gap_too_long(Duration::from_secs(9), second));
    let hour = Duration::from_secs(3600);
    assert!(!observation_gap_too_long(3 * hour, hour));
    assert!(observation_gap_too_long(4 * hour, hour));
}
