use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

/// A fake procfs under the workspace `tmp/`, removed on drop.
struct FakeProc(PathBuf);

impl FakeProc {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../tmp/fake-proc/{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn process(&self, pid: u32, ppid: u32, uid: u32, comm: &[u8]) -> &Self {
        let dir = self.0.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let mut stat = format!("{pid} (").into_bytes();
        stat.extend_from_slice(comm);
        stat.extend_from_slice(format!(") S {ppid} {pid} {pid} 0 -1").as_bytes());
        std::fs::write(dir.join("stat"), stat).unwrap();
        std::fs::write(
            dir.join("status"),
            format!("Name:\tx\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        let mut comm = comm.to_vec();
        comm.push(b'\n');
        std::fs::write(dir.join("comm"), comm).unwrap();
        self
    }

    fn procfs(&self) -> ProcFs {
        ProcFs::new(&self.0)
    }
}

impl Drop for FakeProc {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn identity_start_time_survives_parentheses_and_proc_reads_are_bounded() {
    let proc = FakeProc::new();
    proc.process(50, 42, 7, b"a) (b c)");
    let stat = format!("50 (a) (b c)) S 42 {} 987654 0 0", vec!["0"; 17].join(" "));
    std::fs::write(proc.0.join("50/stat"), stat).unwrap();
    assert_eq!(
        proc.procfs().identity(50),
        Some(ProcessIdentity {
            pid: 50,
            start_time: 987654
        })
    );
    let facts = proc.procfs().scan().unwrap();
    assert_eq!(facts[&50].identity.start_time, 987654);
    assert_eq!(facts[&50].parent, 42);
    std::fs::write(proc.0.join("50/stat"), vec![b'x'; 65537]).unwrap();
    assert!(proc.procfs().identity(50).is_none());
    assert!(proc.procfs().scan().unwrap().is_empty());
    assert!(ProcFs::new(proc.0.join("missing")).scan().is_err());
}

#[test]
fn stat_parsing_survives_a_command_name_with_spaces_and_parentheses() {
    let proc = FakeProc::new();
    proc.process(50, 42, 7, b"a) (b c)");
    let procfs = proc.procfs();
    assert_eq!(procfs.parent(50), Some(42));
    assert_eq!(procfs.uid(50), Some(7));
    assert_eq!(procfs.comm(50).unwrap(), b"a) (b c)");
    assert_eq!(procfs.parent(51), None);
}

#[test]
fn the_live_procfs_reads_this_process() {
    let procfs = ProcFs::default();
    let me = std::process::id();
    assert_eq!(procfs.parent(me), Some(std::os::unix::process::parent_id()));
    assert_eq!(procfs.uid(me), Some(nix::unistd::getuid().as_raw()));
    assert!(procfs.comm(me).is_some());
}

#[test]
fn nvml_process_source_either_lists_processes_or_explains_itself() {
    match NvmlProcessSource::probe() {
        Ok(source) => {
            for process in source.processes().unwrap() {
                assert!(process.gpu_uuid.starts_with("GPU-"), "{process:?}");
            }
        }
        Err(error) => assert!(!error.0.is_empty()),
    }
}

/// A fake procfs with real-looking `stat` (start time in field 22) and
/// per-thread children lists, for [`ProcFs::collect`].
struct Tree(FakeProc);

impl Tree {
    /// `children` says whether the kernel lists children at all.
    fn new(children: bool) -> Self {
        let proc = FakeProc::new();
        if children {
            std::fs::create_dir_all(proc.0.join("thread-self")).unwrap();
            std::fs::write(proc.0.join("thread-self/children"), "").unwrap();
        }
        Self(proc)
    }

    fn process(&self, pid: u32, ppid: u32, uid: u32) -> &Self {
        let dir = self.0 .0.join(pid.to_string());
        std::fs::create_dir_all(dir.join(format!("task/{pid}"))).unwrap();
        // Field 22, the start time, is the 20th after the name.
        let tail = vec!["0"; 13].join(" ");
        std::fs::write(
            dir.join("stat"),
            format!("{pid} (p{pid}) S {ppid} 0 0 0 -1 {tail} {}", 1000 + pid),
        )
        .unwrap();
        std::fs::write(
            dir.join("status"),
            format!("Uid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        std::fs::write(dir.join("comm"), format!("p{pid}\n")).unwrap();
        let parent_task = self.0 .0.join(format!("{ppid}/task/{ppid}"));
        if parent_task.exists() {
            let file = parent_task.join("children");
            let mut list = std::fs::read_to_string(&file).unwrap_or_default();
            list.push_str(&format!("{pid} "));
            std::fs::write(file, list).unwrap();
        }
        self
    }

    fn procfs(&self) -> ProcFs {
        self.0.procfs()
    }
}

/// The identity [`Tree::process`] gives `pid`.
fn id(pid: u32) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_time: 1000 + u64::from(pid),
    }
}

fn pids(collected: &Collected) -> Vec<u32> {
    collected.facts.keys().copied().collect()
}

#[test]
fn collect_reads_roots_descendants_and_gpu_ancestry_only() {
    let tree = Tree::new(true);
    // 10 (root) -> 11 -> 12; 20 -> 21 (GPU process, unrelated); 30 unrelated.
    tree.process(10, 1, 1000)
        .process(11, 10, 1000)
        .process(12, 11, 1000)
        .process(20, 1, 2000)
        .process(21, 20, 2000)
        .process(30, 1, 3000);
    let collected = tree
        .procfs()
        .collect(&Seeds {
            gpu_pids: &[21],
            roots: &[(1, id(10))],
            known: &[],
        })
        .unwrap();
    assert_eq!(pids(&collected), [10, 11, 12, 20, 21]);
    assert!(collected.truncated.is_none());
    assert_eq!(collected.facts[&12].identity.start_time, 1012);
    // The same facts as the whole tree for those pids.
    let all = tree.procfs().scan().unwrap();
    for (pid, fact) in &collected.facts {
        assert_eq!(fact.identity, all[pid].identity);
        assert_eq!(fact.parent, all[pid].parent);
        assert_eq!(fact.uid, all[pid].uid);
    }
}

#[test]
fn collect_does_not_enumerate_procfs() {
    // Far more entries than the old scan allowed: none of them is read.
    let tree = Tree::new(true);
    tree.process(10, 1, 1000);
    for pid in 100_000..100_000 + 2_000 {
        std::fs::create_dir(tree.0 .0.join(pid.to_string())).unwrap();
    }
    let collected = tree
        .procfs()
        .collect(&Seeds {
            roots: &[(1, id(10))],
            ..Seeds::default()
        })
        .unwrap();
    assert_eq!(pids(&collected), [10]);
}

#[test]
fn a_missing_procfs_is_an_error() {
    let error = ProcFs::new("/nonexistent-procfs-root")
        .collect(&Seeds::default())
        .unwrap_err();
    assert!(error.starts_with("cannot read procfs: "), "{error}");
}

#[test]
fn known_processes_are_followed_after_they_are_reparented() {
    let tree = Tree::new(true);
    // 11 was found under root 10 before; it is now a child of init.
    tree.process(10, 1, 1000)
        .process(11, 1, 1000)
        .process(12, 11, 1000);
    let collected = tree
        .procfs()
        .collect(&Seeds {
            roots: &[(1, id(10))],
            known: &[(id(11), 1)],
            ..Seeds::default()
        })
        .unwrap();
    assert_eq!(pids(&collected), [10, 11, 12]);
}

#[test]
fn children_of_every_thread_are_found() {
    let tree = Tree::new(true);
    tree.process(10, 1, 1000).process(11, 1, 1000);
    // 11 was started by thread 99 of process 10.
    let thread = tree.0 .0.join("10/task/99");
    std::fs::create_dir_all(&thread).unwrap();
    std::fs::write(thread.join("children"), "11 ").unwrap();
    std::fs::write(
        tree.0 .0.join("11/stat"),
        format!("11 (p11) S 10 0 0 0 -1 {} 1011", vec!["0"; 13].join(" ")),
    )
    .unwrap();
    let collected = tree
        .procfs()
        .collect(&Seeds {
            roots: &[(1, id(10))],
            ..Seeds::default()
        })
        .unwrap();
    assert_eq!(pids(&collected), [10, 11]);
}

#[test]
fn a_listed_child_that_is_now_someone_elses_is_not_followed() {
    let tree = Tree::new(true);
    tree.process(10, 1, 1000).process(11, 1, 2000);
    // 10's list still names 11, but 11's parent is init: a reused pid.
    std::fs::write(tree.0 .0.join("10/task/10/children"), "11 ").unwrap();
    let collected = tree
        .procfs()
        .collect(&Seeds {
            roots: &[(1, id(10))],
            ..Seeds::default()
        })
        .unwrap();
    assert_eq!(pids(&collected), [10]);
}

#[test]
fn each_job_gets_its_share_of_descendants_and_running_out_is_not_an_error() {
    let tree = Tree::new(true);
    tree.process(10, 1, 1000)
        .process(20, 1, 2000)
        .process(21, 20, 2000);
    // Job 10 has far more children than its share; their list is written
    // directly, without the processes, so the test stays fast.
    let many: String = (1000..1000 + MAX_DESCENDANTS as u32)
        .map(|p| format!("{p} "))
        .collect();
    std::fs::write(tree.0 .0.join("10/task/10/children"), many).unwrap();
    let collected = tree
        .procfs()
        .collect(&Seeds {
            roots: &[(1, id(10)), (2, id(20))],
            ..Seeds::default()
        })
        .unwrap();
    assert_eq!(collected.truncated, Some(MAX_DESCENDANTS / 2));
    // Job 20's own child is still found.
    assert!(collected.facts.contains_key(&21));
}

#[test]
fn without_children_lists_only_gpu_ancestry_is_followed() {
    let tree = Tree::new(false);
    tree.process(10, 1, 1000)
        .process(11, 10, 1000)
        .process(12, 11, 1000);
    let procfs = tree.procfs();
    assert!(!procfs.lists_children());
    let collected = procfs
        .collect(&Seeds {
            gpu_pids: &[12],
            roots: &[(1, id(10))],
            ..Seeds::default()
        })
        .unwrap();
    assert_eq!(pids(&collected), [10, 11, 12]);
}

#[test]
fn collect_reads_the_real_procfs() {
    // This test process as a root, and a child it starts as a descendant.
    let mut child = std::process::Command::new("sleep")
        .arg("5")
        .spawn()
        .unwrap();
    let me = std::process::id();
    let procfs = ProcFs::default();
    let collected = procfs
        .collect(&Seeds {
            roots: &[(1, procfs.identity(me).unwrap())],
            ..Seeds::default()
        })
        .unwrap();
    assert!(collected.facts.contains_key(&me));
    if procfs.lists_children() {
        assert!(
            collected.facts.contains_key(&child.id()),
            "{:?}",
            pids(&collected)
        );
    }
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn a_seed_whose_pid_was_reused_is_not_followed() {
    let tree = Tree::new(true);
    tree.process(10, 1, 1000).process(11, 10, 1000);
    // The root started at another time: pid 10 is now someone else.
    let stale = ProcessIdentity {
        pid: 10,
        start_time: 1,
    };
    let collected = tree
        .procfs()
        .collect(&Seeds {
            roots: &[(1, stale)],
            ..Seeds::default()
        })
        .unwrap();
    assert!(collected.facts.is_empty(), "{:?}", pids(&collected));
}

#[test]
fn duplicate_gpu_pids_are_read_once() {
    // A process on several GPUs is listed once per GPU by NVML.
    let tree = Tree::new(false);
    tree.process(10, 1, 1000).process(11, 10, 1000);
    let gpu_pids: Vec<u32> = std::iter::repeat_n(11, 100).collect();
    let collected = tree
        .procfs()
        .collect(&Seeds {
            gpu_pids: &gpu_pids,
            ..Seeds::default()
        })
        .unwrap();
    assert_eq!(pids(&collected), [10, 11]);
}

#[test]
fn a_roots_own_children_are_read_before_its_known_processes() {
    // Known process 20 lists more children than job 1's whole share; the
    // root's own child 11 must still be found.
    let tree = Tree::new(true);
    tree.process(10, 1, 1000)
        .process(11, 10, 1000)
        .process(20, 1, 1000);
    let many: String = (1000..1000 + MAX_DESCENDANTS as u32)
        .map(|p| format!("{p} "))
        .collect();
    std::fs::write(tree.0 .0.join("20/task/20/children"), many).unwrap();
    let collected = tree
        .procfs()
        .collect(&Seeds {
            roots: &[(1, id(10))],
            known: &[(id(20), 1)],
            ..Seeds::default()
        })
        .unwrap();
    assert!(collected.facts.contains_key(&11), "{:?}", pids(&collected));
    assert!(collected.truncated.is_some());
}
