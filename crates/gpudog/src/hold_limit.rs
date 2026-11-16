//! The hold limit the daemon sends with a grant (`max_hold_ms`). When it
//! ends, `gpudog run` stops the command and the processes it started:
//! SIGTERM first, then SIGKILL for whatever is left after a grace period.
//!
//! The daemon never revokes a reservation: a GPU is free only once the
//! processes using it have exited, so stopping them is the client's job.
//! Signalled are the processes descended from the command when the limit
//! ends, those descended from the command or from any of them still alive
//! during the grace period, and the command itself. A process that left the
//! tree (its parent exited, or it double-forked) is not found.

use std::collections::HashMap;
use std::time::Duration;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::time::Instant;

use crate::common::note;

/// How long the command has between SIGTERM and SIGKILL.
pub(crate) const GRACE: Duration = Duration::from_secs(30);

/// Shortens [`GRACE`] for tests. Only a shorter value is honoured: a longer
/// one would let a command outlive its limit by choice. Documented for
/// contributors in `docs/developer/building.md`, not for users.
const GRACE_OVERRIDE: &str = "GPUDOG_TEST_HOLD_GRACE_MS";

/// How often a stopped command's processes are checked while they exit.
/// This reads only their own `/proc/<pid>/stat`.
const POLL: Duration = Duration::from_millis(50);

/// How often, during the grace period, `/proc` is searched for processes
/// they have started. That reads every process on the host, so it runs
/// less often than [`POLL`]; [`HoldLimit::on_deadline`] searches once more
/// before SIGKILL.
const SCAN: Duration = Duration::from_millis(500);

fn grace() -> Duration {
    std::env::var(GRACE_OVERRIDE)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .filter(|grace| *grace < GRACE)
        .unwrap_or(GRACE)
}

/// `ms` in the units `--timeout` accepts, largest first: `1h30m`, `45s`,
/// `250ms`.
pub(crate) fn describe(ms: u64) -> String {
    const UNITS: [(u64, &str); 5] = [
        (86_400_000, "d"),
        (3_600_000, "h"),
        (60_000, "m"),
        (1_000, "s"),
        (1, "ms"),
    ];
    if ms == 0 {
        return "0ms".to_owned();
    }
    let mut rest = ms;
    let mut text = String::new();
    for (size, unit) in UNITS {
        if rest >= size {
            text.push_str(&format!("{}{unit}", rest / size));
            rest %= size;
        }
    }
    text
}

/// A process, told apart from a later one that reuses its pid by its start
/// time (clock ticks since boot, from `/proc/<pid>/stat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Process {
    pid: i32,
    start: u64,
}

/// The fields of `/proc/<pid>/stat` this module needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stat {
    state: char,
    ppid: i32,
    start: u64,
}

/// Parses a `/proc/<pid>/stat` line. The command name is in parentheses
/// and may itself contain spaces and parentheses, so fields are counted
/// from the last `)`.
fn parse_stat(text: &str) -> Option<Stat> {
    let (_, rest) = text.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `rest` starts at field 3 (state); ppid is field 4, starttime 22.
    Some(Stat {
        state: fields.first()?.chars().next()?,
        ppid: fields.get(1)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
    })
}

fn stat_of(pid: i32) -> Option<Stat> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// Every process this client can see, by pid.
fn process_table() -> HashMap<i32, Stat> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return HashMap::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|pid| Some((pid, stat_of(pid)?)))
        .collect()
}

/// One process found under the command, with its parent as the table
/// recorded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Found {
    process: Process,
    ppid: i32,
    zombie: bool,
}

/// The descendants of `root` in `table`, not `root` itself, parents before
/// children. Zombies are included, so that their children are still found.
fn descendants(table: &HashMap<i32, Stat>, root: i32) -> Vec<Found> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for (&pid, stat) in table {
        children.entry(stat.ppid).or_default().push(pid);
    }
    let mut found: Vec<Found> = Vec::new();
    let mut pending: Vec<i32> = children.get(&root).cloned().unwrap_or_default();
    while let Some(pid) = pending.pop() {
        let Some(stat) = table.get(&pid) else {
            continue;
        };
        if pid == root || found.iter().any(|entry| entry.process.pid == pid) {
            continue;
        }
        found.push(Found {
            process: Process {
                pid,
                start: stat.start,
            },
            ppid: stat.ppid,
            zombie: stat.state == 'Z',
        });
        pending.extend(children.get(&pid).into_iter().flatten());
    }
    found
}

/// Keeps the live processes of `found` whose own and whose parent's start
/// times, read again through `current`, are still the ones recorded. The
/// table is not one atomic snapshot: if a parent exited and its pid was
/// reused while `/proc` was read, the new owner's children would otherwise
/// look like the command's. A process whose parent exited in that window
/// is dropped too: it has been reparented, so it has left the tree and is
/// not found again.
///
/// `root_start` is the root's recorded start time, checked like any other
/// parent's; `None` for the command, an unreaped child whose pid cannot be
/// reused.
fn confirmed(
    found: &[Found],
    root: i32,
    root_start: Option<u64>,
    current: impl Fn(i32) -> Option<Stat>,
) -> Vec<Process> {
    let root_holds = root_start.is_none_or(|start| current(root).is_some_and(|s| s.start == start));
    let mut accepted: Vec<Found> = Vec::new();
    for entry in found {
        let parent_holds = (entry.ppid == root && root_holds)
            || accepted.iter().any(|parent| {
                parent.process.pid == entry.ppid
                    && current(entry.ppid).is_some_and(|stat| stat.start == parent.process.start)
            });
        let still_itself = current(entry.process.pid)
            .is_some_and(|stat| stat.start == entry.process.start && stat.ppid == entry.ppid);
        if parent_holds && still_itself {
            accepted.push(*entry);
        }
    }
    accepted
        .into_iter()
        .filter(|entry| !entry.zombie)
        .map(|entry| entry.process)
        .collect()
}

/// True when `/proc` shows this process's own PID namespace, so that pids
/// read there are the ones `kill` reaches: `/proc/self` names this process,
/// and the command's parent there is this process. Not so, for example,
/// under `unshare --pid --fork` without `--mount-proc`.
fn proc_is_ours(table: &HashMap<i32, Stat>, command: i32) -> bool {
    proc_self_is_ours()
        && table
            .get(&command)
            .is_some_and(|stat| stat.ppid as u32 == std::process::id())
}

fn proc_self_is_ours() -> bool {
    std::fs::read_link("/proc/self")
        .ok()
        .and_then(|link| link.to_str()?.parse::<u32>().ok())
        == Some(std::process::id())
}

/// Adds to `targets` the live descendants of every target still alive: a
/// process signalled at the limit may start others during the grace period,
/// even after the command itself has exited. One that has already outlived
/// the process that started it is reparented, and not found.
fn grow(targets: &mut Vec<Process>) {
    // A dead target can neither be signalled nor start anything, and a
    // survivor that keeps starting short-lived processes would otherwise
    // grow the list for the whole grace period.
    targets.retain(|target| is_alive(*target));
    if targets.is_empty() || !proc_self_is_ours() {
        return;
    }
    let table = process_table();
    let parents: Vec<Process> = targets
        .iter()
        .copied()
        .filter(|target| {
            table
                .get(&target.pid)
                .is_some_and(|stat| stat.state != 'Z' && stat.start == target.start)
        })
        .collect();
    for parent in parents {
        let found = descendants(&table, parent.pid);
        for process in confirmed(&found, parent.pid, Some(parent.start), stat_of) {
            if !targets.contains(&process) {
                targets.push(process);
            }
        }
    }
}

/// The command's live descendants, or none if `/proc` cannot be trusted to
/// name them.
fn command_tree(command: Pid) -> Vec<Process> {
    let table = process_table();
    if !proc_is_ours(&table, command.as_raw()) {
        return Vec::new();
    }
    confirmed(
        &descendants(&table, command.as_raw()),
        command.as_raw(),
        None,
        stat_of,
    )
}

fn is_alive(process: Process) -> bool {
    stat_of(process.pid).is_some_and(|stat| stat.state != 'Z' && stat.start == process.start)
}

/// Signals the command itself. It is this process's child and has not been
/// reaped while `command` is `Some`, so its pid cannot have been reused.
///
/// Returns true only if it was still running: `kill` also succeeds on a
/// zombie, a command that has exited but that `gpudog` has not yet waited
/// for.
fn signal_command(command: Option<Pid>, signal: Signal) -> bool {
    command.is_some_and(|pid| {
        let running = stat_of(pid.as_raw()).is_none_or(|stat| stat.state != 'Z');
        kill(pid, signal).is_ok() && running
    })
}

/// Sends `signal` to each process still alive as itself. Checking the
/// start time first keeps a pid reused since the table was read from being
/// signalled; the check and the signal are not atomic, but a pid is reused
/// only after the kernel has cycled through the others.
fn signal_all(processes: &[Process], signal: Signal) -> usize {
    let own = std::process::id() as i32;
    processes
        .iter()
        .filter(|process| process.pid > 1 && process.pid != own && is_alive(**process))
        .filter(|process| kill(Pid::from_raw(process.pid), signal).is_ok())
        .count()
}

enum State {
    /// The command runs; the limit ends at `ends`, if there is one.
    Running {
        ends: Option<Instant>,
    },
    /// SIGTERM was sent to `targets`; SIGKILL follows at `kill_at`.
    Stopping {
        targets: Vec<Process>,
        kill_at: Instant,
    },
    Done,
}

/// Enforces one grant's hold limit on the command `gpudog run` started.
pub(crate) struct HoldLimit {
    limit_ms: u64,
    state: State,
}

impl HoldLimit {
    /// `limit_ms` is `Granted::hold_limit_ms`; 0 means no limit. Counted
    /// from now, just after the grant arrived.
    pub(crate) fn new(limit_ms: u64) -> Self {
        let ends = (limit_ms > 0).then(|| Instant::now() + Duration::from_millis(limit_ms));
        Self {
            limit_ms,
            state: State::Running { ends },
        }
    }

    /// When [`HoldLimit::on_deadline`] must run next, if ever.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        match &self.state {
            State::Running { ends } => *ends,
            State::Stopping { kill_at, .. } => Some(*kill_at),
            State::Done => None,
        }
    }

    /// Moves on once [`HoldLimit::next_deadline`] has passed. `command` is
    /// the command's pid while it has not exited.
    pub(crate) fn on_deadline(&mut self, command: Option<Pid>) {
        match std::mem::replace(&mut self.state, State::Done) {
            State::Running { .. } => {
                let targets = command.map_or_else(Vec::new, command_tree);
                let grace = grace();
                note(format!(
                    "the hold limit of {} has ended; stopping the command (SIGKILL in {})",
                    describe(self.limit_ms),
                    describe(u64::try_from(grace.as_millis()).unwrap_or(u64::MAX)),
                ));
                signal_command(command, Signal::SIGTERM);
                signal_all(&targets, Signal::SIGTERM);
                self.state = State::Stopping {
                    targets,
                    kill_at: Instant::now() + grace,
                };
            }
            State::Stopping { mut targets, .. } => {
                // Anything started during the grace period too.
                if let Some(pid) = command {
                    for process in command_tree(pid) {
                        if !targets.contains(&process) {
                            targets.push(process);
                        }
                    }
                }
                grow(&mut targets);
                let killed = signal_command(command, Signal::SIGKILL);
                if signal_all(&targets, Signal::SIGKILL) > 0 || killed {
                    note("the command did not stop after SIGTERM; sent SIGKILL");
                }
            }
            State::Done => {}
        }
    }

    /// After the command itself has exited: if it was being stopped, waits
    /// for the rest of what was signalled, and kills whatever is left when
    /// the grace period ends. Their GPUs are not free until they exit.
    pub(crate) async fn finish(&mut self) {
        let mut next_scan = Instant::now();
        loop {
            let State::Stopping { targets, kill_at } = &mut self.state else {
                return;
            };
            if Instant::now() >= next_scan {
                grow(targets);
                next_scan = Instant::now() + SCAN;
            } else {
                targets.retain(|target| is_alive(*target));
            }
            if targets.is_empty() {
                self.state = State::Done;
                return;
            }
            let kill_at = *kill_at;
            if Instant::now() >= kill_at {
                self.on_deadline(None);
                return;
            }
            tokio::time::sleep_until((Instant::now() + POLL).min(kill_at)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(state: char, ppid: i32, start: u64) -> Stat {
        Stat { state, ppid, start }
    }

    #[test]
    fn durations_are_described_in_the_timeout_units() {
        assert_eq!(describe(0), "0ms");
        assert_eq!(describe(250), "250ms");
        assert_eq!(describe(30_000), "30s");
        assert_eq!(describe(90_000), "1m30s");
        assert_eq!(describe(5_400_000), "1h30m");
        assert_eq!(describe(86_400_000), "1d");
        assert_eq!(describe(86_400_000 + 1_500), "1d1s500ms");
    }

    #[test]
    fn a_stat_line_is_read_from_after_the_last_parenthesis() {
        let line = "4242 (a (weird) name) S 17 4242 4242 0 -1 4194304 \
                    100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(parse_stat(line), Some(stat('S', 17, 987_654)));
        assert_eq!(parse_stat("4242 (short) S 17"), None);
        assert_eq!(parse_stat("no parenthesis"), None);
    }

    fn process(pid: i32, start: u64) -> Process {
        Process { pid, start }
    }

    fn pids(processes: &[Process]) -> Vec<i32> {
        let mut pids: Vec<i32> = processes.iter().map(|p| p.pid).collect();
        pids.sort_unstable();
        pids
    }

    #[test]
    fn descendants_follow_parents_and_skip_zombies_but_not_their_children() {
        let table = HashMap::from([
            (10, stat('S', 1, 100)),
            (11, stat('S', 10, 101)),
            (12, stat('Z', 10, 102)),
            (13, stat('R', 11, 103)),
            (14, stat('S', 12, 104)),
            (20, stat('S', 1, 200)),
        ]);
        let found = descendants(&table, 10);
        // The command itself is not among them; it is signalled by pid.
        assert!(found.iter().all(|entry| entry.process.pid != 10));
        let unchanged = |pid: i32| table.get(&pid).copied();
        let live = confirmed(&found, 10, None, unchanged);
        assert_eq!(pids(&live), [11, 13, 14]);
        assert!(descendants(&table, 99).is_empty());
    }

    #[test]
    fn children_of_a_reused_parent_pid_are_not_taken_for_the_commands() {
        // 11 was the command's child when the table read it; by the time 12
        // was read, 11 had exited and its pid belonged to another process,
        // whose child 12 is.
        let table = HashMap::from([
            (10, stat('S', 1, 100)),
            (11, stat('S', 10, 101)),
            (12, stat('S', 11, 900)),
        ]);
        let found = descendants(&table, 10);
        let now = HashMap::from([
            (10, stat('S', 1, 100)),
            (11, stat('S', 1, 899)),
            (12, stat('S', 11, 900)),
        ]);
        let live = confirmed(&found, 10, None, |pid| now.get(&pid).copied());
        assert!(live.is_empty(), "{live:?}");
    }

    #[test]
    fn a_process_whose_pid_was_reused_since_the_table_is_dropped() {
        let table = HashMap::from([(10, stat('S', 1, 100)), (11, stat('S', 10, 101))]);
        let found = descendants(&table, 10);
        let now = HashMap::from([(10, stat('S', 1, 100)), (11, stat('S', 10, 555))]);
        assert!(confirmed(&found, 10, None, |pid| now.get(&pid).copied()).is_empty());
        let same = confirmed(&found, 10, None, |pid| table.get(&pid).copied());
        assert_eq!(same, [process(11, 101)]);
    }

    #[test]
    fn a_survivor_root_whose_pid_was_reused_gives_no_children() {
        // A survivor searched during the grace period is not this process's
        // child, so its own pid can be reused while /proc is read.
        let table = HashMap::from([(10, stat('S', 1, 100)), (11, stat('S', 10, 101))]);
        let found = descendants(&table, 10);
        let reused = HashMap::from([(10, stat('S', 1, 777)), (11, stat('S', 10, 101))]);
        assert!(confirmed(&found, 10, Some(100), |pid| reused.get(&pid).copied()).is_empty());
        let same = confirmed(&found, 10, Some(100), |pid| table.get(&pid).copied());
        assert_eq!(same, [process(11, 101)]);
    }

    #[test]
    fn this_process_is_ours_only_through_its_own_proc() {
        let own = std::process::id() as i32;
        let table = HashMap::from([(4242, stat('S', own, 1))]);
        // Tests run with the host's or the container's own /proc.
        assert!(proc_is_ours(&table, 4242));
        let foreign = HashMap::from([(4242, stat('S', own + 1, 1))]);
        assert!(!proc_is_ours(&foreign, 4242));
        assert!(!proc_is_ours(&table, 4243));
    }

    #[test]
    fn a_zero_limit_never_fires() {
        assert!(HoldLimit::new(0).next_deadline().is_none());
        assert!(HoldLimit::new(1).next_deadline().is_some());
    }
}
