use super::*;
use crate::gpu_inventory::{GpuInventory, StaticInventory};

fn fact(pid: u32, parent: u32, start: u64) -> ProcessFacts {
    ProcessFacts {
        identity: ProcessIdentity {
            pid,
            start_time: start,
        },
        parent,
        uid: Some(1001),
        command: Some(b"worker\x1b".to_vec()),
    }
}
fn tree() -> BTreeMap<u32, ProcessFacts> {
    [
        (10, fact(10, 1, 100)),
        (11, fact(11, 10, 101)),
        (12, fact(12, 11, 102)),
    ]
    .into()
}
fn root() -> ReservationRoot {
    ReservationRoot {
        id: 1,
        identity: Some(ProcessIdentity {
            pid: 10,
            start_time: 100,
        }),
        uid: 1001,
        gpus: vec![0],
    }
}
fn gpu(pid: u32, index: u32) -> GpuProcess {
    GpuProcess {
        pid,
        nvml_index: index,
        gpu_uuid: format!("GPU-static-{index}"),
        used_memory: Some(42),
        utilization: None,
    }
}

#[test]
fn descendants_are_managed_and_display_text_is_sanitized() {
    let (_, rows) = ProcessTracker::default().observe(
        &[gpu(12, 0)],
        StaticInventory::with_count(2).devices(),
        &[root()],
        &tree(),
    );
    assert_eq!(rows[0].row.ownership, Ownership::Managed);
    assert_eq!(rows[0].start_time, Some(102));
    assert_eq!(rows[0].row.command, Some(b"worker?".to_vec()));
}

#[test]
fn usage_blocks_independently_of_ownership_and_wrong_gpu_is_visible() {
    let (busy, rows) = ProcessTracker::default().observe(
        &[gpu(12, 1), gpu(99, 0)],
        StaticInventory::with_count(2).devices(),
        &[root()],
        &tree(),
    );
    assert_eq!(busy, [0, 1]);
    assert_eq!(rows[0].row.ownership, Ownership::Unknown);
    assert_eq!(rows[1].row.ownership, Ownership::WrongGpu);
}

#[test]
fn previously_seen_children_remain_owned_after_parent_exit() {
    let mut tracker = ProcessTracker::default();
    let mut facts = tree();
    tracker.observe(
        &[],
        StaticInventory::with_count(2).devices(),
        &[root()],
        &facts,
    );
    facts.remove(&10);
    facts.get_mut(&11).unwrap().parent = 1;
    let (_, rows) = tracker.observe(
        &[gpu(12, 0)],
        StaticInventory::with_count(2).devices(),
        &[root()],
        &facts,
    );
    assert_eq!(rows[0].row.ownership, Ownership::Managed);
}

#[test]
fn reused_child_pid_and_released_reservation_do_not_reuse_ownership() {
    let mut tracker = ProcessTracker::default();
    let mut facts = tree();
    tracker.observe(
        &[],
        StaticInventory::with_count(2).devices(),
        &[root()],
        &facts,
    );
    facts.insert(12, fact(12, 1, 999));
    let (_, rows) = tracker.observe(
        &[gpu(12, 0)],
        StaticInventory::with_count(2).devices(),
        &[root()],
        &facts,
    );
    assert_eq!(rows[0].row.ownership, Ownership::NotManaged);
    let (_, rows) = tracker.observe(
        &[gpu(11, 0)],
        StaticInventory::with_count(2).devices(),
        &[],
        &facts,
    );
    assert_ne!(rows[0].row.ownership, Ownership::Managed);
}

#[test]
fn reused_root_pid_does_not_claim_new_descendants() {
    let mut tracker = ProcessTracker::default();
    let mut facts = tree();
    tracker.observe(
        &[],
        StaticInventory::with_count(2).devices(),
        &[root()],
        &facts,
    );
    facts.insert(10, fact(10, 1, 999));
    facts.insert(13, fact(13, 10, 1000));
    let (_, rows) = tracker.observe(
        &[gpu(13, 0)],
        StaticInventory::with_count(2).devices(),
        &[root()],
        &facts,
    );
    assert_eq!(rows[0].row.ownership, Ownership::Unknown);
}

#[test]
fn cycles_missing_facts_and_unknown_peer_identity_are_unknown() {
    let mut facts = tree();
    facts.get_mut(&10).unwrap().parent = 12;
    let mut claim = root();
    claim.identity = None;
    let (_, rows) = ProcessTracker::default().observe(
        &[gpu(12, 0)],
        StaticInventory::with_count(2).devices(),
        &[claim],
        &facts,
    );
    assert_eq!(rows[0].row.ownership, Ownership::Unknown);
}

#[test]
fn each_reservation_remembers_at_most_its_share_of_descendants() {
    // Two roots: each may remember MAX_DESCENDANTS / 2 processes. Root 10
    // has more descendants than that; root 20's one child is still kept.
    let share = MAX_DESCENDANTS / 2;
    let mut facts = BTreeMap::new();
    facts.insert(10, fact(10, 1, 100));
    facts.insert(
        20,
        ProcessFacts {
            uid: Some(2002),
            ..fact(20, 1, 200)
        },
    );
    // Above every pid of root 10's tree, so one cap shared by both roots
    // would be full before reaching it.
    facts.insert(
        90_000,
        ProcessFacts {
            uid: Some(2002),
            ..fact(90_000, 20, 201)
        },
    );
    for pid in 1000..1000 + share as u32 + 10 {
        facts.insert(pid, fact(pid, 10, u64::from(pid)));
    }
    let other = ReservationRoot {
        id: 2,
        identity: Some(ProcessIdentity {
            pid: 20,
            start_time: 200,
        }),
        uid: 2002,
        gpus: vec![1],
    };
    let mut tracker = ProcessTracker::default();
    let _ = tracker.observe(
        &[],
        StaticInventory::with_count(2).devices(),
        &[root(), other],
        &facts,
    );
    let known = tracker.known();
    let first = known.iter().filter(|&&(_, owner)| owner == 1).count();
    assert!(first <= share, "{first}");
    assert!(known
        .iter()
        .any(|&(identity, owner)| identity.pid == 90_000 && owner == 2));
}

#[test]
fn a_root_is_resolved_only_while_procfs_shows_it() {
    let mut tracker = ProcessTracker::default();
    let devices = StaticInventory::with_count(2).devices().to_vec();
    tracker.observe(&[], &devices, &[root()], &tree());
    assert!(tracker.resolved(&root()));
    let mut gone = tree();
    gone.remove(&10);
    tracker.observe(&[], &devices, &[root()], &gone);
    assert!(!tracker.resolved(&root()));
}
