//! The hold limit the daemon sends with a grant (`max_hold_ms`). When it
//! ends, `gpudog run` stops the command and the processes it started:
//! SIGTERM first, then SIGKILL for whatever is left after a grace period.
//!
//! The daemon never revokes a reservation: a GPU is free only once the
//! processes using it have exited, so stopping them is the client's job.
//! Only processes descended from the command when the limit ends (or when
//! the grace period ends) are signalled. A process that left the tree
//! before then, for example by double-forking, is not found.

use std::collections::HashMap;
use std::time::Duration;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use tokio::time::Instant;

use crate::common::note;

/// How long the command has between SIGTERM and SIGKILL.
pub(crate) const GRACE: Duration = Duration::from_secs(30);

/// Shortens [`GRACE`] for tests. Only a shorter value is honoured: a longer
/// one would let a command outlive its limit by choice.
const GRACE_OVERRIDE: &str = "GPUDOG_TEST_HOLD_GRACE_MS";

/// How often a stopped command's processes are checked while they exit.
const POLL: Duration = Duration::from_millis(50);

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

/// `root` and its descendants in `table`, parents before children. A zombie
/// is left out: it has already exited.
fn descendants(table: &HashMap<i32, Stat>, root: i32) -> Vec<Process> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for (&pid, stat) in table {
        children.entry(stat.ppid).or_default().push(pid);
    }
    let mut found = Vec::new();
    let mut pending = vec![root];
    while let Some(pid) = pending.pop() {
        let Some(stat) = table.get(&pid) else {
            continue;
        };
        if found.iter().any(|process: &Process| process.pid == pid) {
            continue;
        }
        if stat.state != 'Z' {
            found.push(Process {
                pid,
                start: stat.start,
            });
        }
        pending.extend(children.get(&pid).into_iter().flatten());
    }
    found
}

fn is_alive(process: Process) -> bool {
    stat_of(process.pid).is_some_and(|stat| stat.state != 'Z' && stat.start == process.start)
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
                let targets = command
                    .map_or_else(Vec::new, |pid| descendants(&process_table(), pid.as_raw()));
                let grace = grace();
                note(format!(
                    "the hold limit of {} has ended; stopping the command (SIGKILL in {})",
                    describe(self.limit_ms),
                    describe(u64::try_from(grace.as_millis()).unwrap_or(u64::MAX)),
                ));
                signal_all(&targets, Signal::SIGTERM);
                self.state = State::Stopping {
                    targets,
                    kill_at: Instant::now() + grace,
                };
            }
            State::Stopping { mut targets, .. } => {
                // Anything the command started during the grace period too.
                if let Some(pid) = command {
                    for process in descendants(&process_table(), pid.as_raw()) {
                        if !targets.contains(&process) {
                            targets.push(process);
                        }
                    }
                }
                if signal_all(&targets, Signal::SIGKILL) > 0 {
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
        while let State::Stopping { targets, kill_at } = &self.state {
            if !targets.iter().any(|process| is_alive(*process)) {
                self.state = State::Done;
                return;
            }
            if Instant::now() >= *kill_at {
                self.on_deadline(None);
                return;
            }
            let wake = (Instant::now() + POLL).min(*kill_at);
            tokio::time::sleep_until(wake).await;
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

    #[test]
    fn descendants_follow_parents_and_skip_zombies() {
        let table = HashMap::from([
            (10, stat('S', 1, 100)),
            (11, stat('S', 10, 101)),
            (12, stat('Z', 10, 102)),
            (13, stat('R', 11, 103)),
            (20, stat('S', 1, 200)),
        ]);
        let mut found = descendants(&table, 10);
        found.sort_by_key(|process| process.pid);
        assert_eq!(
            found,
            [
                Process {
                    pid: 10,
                    start: 100
                },
                Process {
                    pid: 11,
                    start: 101
                },
                Process {
                    pid: 13,
                    start: 103
                },
            ]
        );
        assert!(descendants(&table, 99).is_empty());
    }

    #[test]
    fn a_zero_limit_never_fires() {
        assert!(HoldLimit::new(0).next_deadline().is_none());
        assert!(HoldLimit::new(1).next_deadline().is_some());
    }
}
