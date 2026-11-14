//! Every process using a GPU, and whether a reservation accounts for it.
//!
//! Production queries run in the daemon for its managed UUIDs, with bounded
//! host procfs facts and PID/start-time tracking in `process_watcher`.
//! Labels are display metadata, but all observed GPU usage blocks new grants
//! regardless of classification. Dash consumes daemon snapshots; clients
//! never query their own unrelated NVML/procfs process view.
//!
//! The stateless `classify` helper remains for legacy library callers; it
//! cannot track PID reuse/reparenting like the daemon's `ProcessTracker`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;

use nvml_wrapper::enums::device::UsedGpuMemory;
use nvml_wrapper::error::NvmlError as RawNvmlError;
use nvml_wrapper::struct_wrappers::device::ProcessInfo;
use nvml_wrapper::Nvml;

use crate::gpu_inventory::{load_nvml, NvmlError};
use crate::protocol::{ReservationInfo, StatusReply};

/// One process holding a context on one GPU, as NVML reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuProcess {
    /// NVML's device index, or, for a source built by `probe_selected`, the
    /// position of `gpu_uuid` in the selected UUID list.
    pub nvml_index: u32,
    /// Matched against the daemon's device list, so indices agree even if
    /// the two disagree on ordering.
    pub gpu_uuid: String,
    pub pid: u32,
    /// Bytes, when the driver reports it.
    pub used_memory: Option<u64>,
}

/// Where the daemon's process list comes from: NVML in production, a fixed
/// list in tests.
pub trait GpuProcessSource: Send + Sync {
    /// Processes on the GPUs named by `uuids`, which the daemon passes on
    /// every call because a configuration reload can change them.
    fn processes(&self, uuids: &[String]) -> Result<Vec<GpuProcess>, NvmlError>;
}

/// Process list backed by a live NVML session, shut down on drop.
pub struct NvmlProcessSource {
    nvml: Nvml,
    selected: Option<Vec<String>>,
}

impl NvmlProcessSource {
    pub fn probe() -> Result<Self, NvmlError> {
        Ok(Self {
            nvml: load_nvml()?,
            selected: None,
        })
    }

    pub fn probe_selected(uuids: Vec<String>) -> Result<Self, NvmlError> {
        Ok(Self {
            nvml: load_nvml()?,
            selected: Some(uuids),
        })
    }
}

fn memory(info: &ProcessInfo) -> Option<u64> {
    match info.used_gpu_memory {
        UsedGpuMemory::Used(bytes) => Some(bytes),
        UsedGpuMemory::Unavailable => None,
    }
}

impl NvmlProcessSource {
    /// Processes on the selected GPUs, or on every GPU NVML lists.
    pub fn processes(&self) -> Result<Vec<GpuProcess>, NvmlError> {
        let count = match &self.selected {
            Some(uuids) => uuids.len() as u32,
            None => self
                .nvml
                .device_count()
                .map_err(|e| NvmlError(format!("nvmlDeviceGetCount failed: {e}")))?,
        };
        let mut found: Vec<GpuProcess> = Vec::new();
        for index in 0..count {
            // Errors name what was asked for: the UUID, or NVML's index.
            let (device, what) = match &self.selected {
                Some(uuids) => {
                    let uuid = uuids[index as usize].as_str();
                    let device = self.nvml.device_by_uuid(uuid).map_err(|e| {
                        NvmlError(format!("nvmlDeviceGetHandleByUUID failed for {uuid}: {e}"))
                    })?;
                    (device, uuid.to_owned())
                }
                None => {
                    let device = self.nvml.device_by_index(index).map_err(|e| {
                        NvmlError(format!(
                            "nvmlDeviceGetHandleByIndex failed for device {index}: {e}"
                        ))
                    })?;
                    (device, format!("device {index}"))
                }
            };
            let uuid = device
                .uuid()
                .map_err(|e| NvmlError(format!("nvmlDeviceGetUUID failed for {what}: {e}")))?;
            if self
                .selected
                .as_ref()
                .is_some_and(|uuids| !crate::protocol::same_uuid(&uuids[index as usize], &uuid))
            {
                return Err(NvmlError(
                    "NVML observer UUID differs from the managed CUDA view".into(),
                ));
            }
            let compute = device.running_compute_processes().map_err(|e| {
                NvmlError(format!(
                    "nvmlDeviceGetComputeRunningProcesses failed for {what}: {e}"
                ))
            })?;
            // Compute-only boards (most datacentre parts) do not support the
            // graphics query; that is not an error worth hiding the view for.
            let graphics = match device.running_graphics_processes() {
                Ok(list) => list,
                Err(RawNvmlError::NotSupported) => Vec::new(),
                Err(e) => {
                    return Err(NvmlError(format!(
                        "nvmlDeviceGetGraphicsRunningProcesses failed for {what}: {e}"
                    )))
                }
            };
            // A process can appear in both lists; one row per (GPU, pid).
            let start = found.len();
            for info in compute.iter().chain(&graphics) {
                let used = memory(info);
                match found[start..].iter_mut().find(|p| p.pid == info.pid) {
                    Some(existing) => {
                        existing.used_memory = existing.used_memory.max(used);
                    }
                    None => found.push(GpuProcess {
                        nvml_index: index,
                        gpu_uuid: uuid.clone(),
                        pid: info.pid,
                        used_memory: used,
                    }),
                }
            }
        }
        Ok(found)
    }
}

/// Reopen NVML after failures, and when the managed GPUs change.
/// Construction is infallible so a transient watcher failure pauses grants
/// rather than stopping the daemon.
#[derive(Default)]
pub struct ManagedNvmlSource {
    session: Mutex<Option<NvmlProcessSource>>,
}

impl ManagedNvmlSource {
    pub fn new() -> Self {
        Self::default()
    }
}

impl GpuProcessSource for ManagedNvmlSource {
    fn processes(&self, uuids: &[String]) -> Result<Vec<GpuProcess>, NvmlError> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| NvmlError("NVML observer lock poisoned".into()))?;
        if session
            .as_ref()
            .is_none_or(|open| open.selected.as_deref() != Some(uuids))
        {
            *session = None;
            *session = Some(NvmlProcessSource::probe_selected(uuids.to_vec())?);
        }
        let result = session
            .as_ref()
            .ok_or_else(|| NvmlError("NVML observer unavailable".into()))?
            .processes();
        if result.is_err() {
            *session = None;
        }
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
}

#[derive(Debug, Clone)]
pub struct ProcessFacts {
    pub identity: ProcessIdentity,
    pub parent: u32,
    pub uid: Option<u32>,
    pub command: Option<Vec<u8>>,
}

/// Reads process facts from a procfs tree. `/proc` in production; a fake
/// tree in tests.
#[derive(Debug, Clone)]
pub struct ProcFs {
    root: PathBuf,
}

impl Default for ProcFs {
    fn default() -> Self {
        Self::new("/proc")
    }
}

impl ProcFs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn read(&self, pid: u32, file: &str) -> Option<Vec<u8>> {
        let file = std::fs::File::open(self.root.join(pid.to_string()).join(file)).ok()?;
        let mut bytes = Vec::new();
        file.take(65_537).read_to_end(&mut bytes).ok()?;
        (bytes.len() <= 65_536).then_some(bytes)
    }

    /// The parent pid, from `stat`. The command name in field 2 may hold
    /// spaces and parentheses, so parse after the last `)`.
    pub fn parent(&self, pid: u32) -> Option<u32> {
        let stat = self.read(pid, "stat")?;
        let close = stat.iter().rposition(|&b| b == b')')?;
        let rest = std::str::from_utf8(&stat[close + 1..]).ok()?;
        // Fields after the name: state, ppid, ...
        rest.split_ascii_whitespace().nth(1)?.parse().ok()
    }

    /// The real uid, from the `Uid:` line of `status`.
    pub fn uid(&self, pid: u32) -> Option<u32> {
        let status = self.read(pid, "status")?;
        let status = String::from_utf8_lossy(&status);
        status
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))?
            .split_ascii_whitespace()
            .next()?
            .parse()
            .ok()
    }

    /// The short command name, raw bytes. Untrusted: any user picks it.
    pub fn comm(&self, pid: u32) -> Option<Vec<u8>> {
        let mut comm = self.read(pid, "comm")?;
        if comm.last() == Some(&b'\n') {
            comm.pop();
        }
        Some(comm)
    }

    pub fn identity(&self, pid: u32) -> Option<ProcessIdentity> {
        let stat = self.read(pid, "stat")?;
        let close = stat.iter().rposition(|&b| b == b')')?;
        let rest = std::str::from_utf8(&stat[close + 1..]).ok()?;
        let start_time = rest.split_ascii_whitespace().nth(19)?.parse().ok()?;
        Some(ProcessIdentity { pid, start_time })
    }

    /// One process's facts, or `None` if it is gone or unreadable: an
    /// expected race. The identity is read before and after, so facts of a
    /// pid reused in between are not mixed.
    fn fact(&self, pid: u32) -> Option<ProcessFacts> {
        let identity = self.identity(pid)?;
        let (uid, command, parent) = (self.uid(pid), self.comm(pid), self.parent(pid));
        (self.identity(pid) == Some(identity)).then_some(ProcessFacts {
            identity,
            parent: parent?,
            uid,
            command,
        })
    }

    /// Whether the procfs root can be read at all.
    pub fn readable(&self) -> bool {
        std::fs::read_dir(&self.root).is_ok()
    }

    /// Whether this kernel lists each thread's children
    /// (`/proc/<pid>/task/<tid>/children`, `CONFIG_PROC_CHILDREN`). Without
    /// it, descendants are found only through their parents.
    pub fn lists_children(&self) -> bool {
        self.root.join("thread-self/children").exists()
    }

    /// Appends the children of `pid`, over all its threads, to `out`, using
    /// up to `budget` entries (threads count too). Returns false if the
    /// budget ran out first.
    fn children(&self, pid: u32, budget: &mut usize, out: &mut Vec<u32>) -> bool {
        let Ok(threads) = std::fs::read_dir(self.root.join(pid.to_string()).join("task")) else {
            return true; // gone: a race
        };
        for thread in threads.flatten() {
            if *budget == 0 {
                return false;
            }
            *budget -= 1;
            let Ok(file) = std::fs::File::open(thread.path().join("children")) else {
                continue;
            };
            // Each entry is at most 10 digits and a space; read no more
            // than the budget can use, so a huge list cannot cost more.
            let cap = (*budget as u64 + 1) * 11;
            let mut text = Vec::new();
            if file.take(cap).read_to_end(&mut text).is_err() {
                continue;
            }
            let complete = (text.len() as u64) < cap;
            // Past the cap, the last word may have been cut short.
            let end = if complete {
                text.len()
            } else {
                text.iter().rposition(u8::is_ascii_whitespace).unwrap_or(0)
            };
            let parsed: Vec<u32> = text[..end]
                .split(u8::is_ascii_whitespace)
                .filter_map(|word| std::str::from_utf8(word).ok()?.parse().ok())
                .collect();
            for child in parsed {
                if *budget == 0 {
                    return false;
                }
                *budget -= 1;
                out.push(child);
            }
            if !complete {
                return false;
            }
        }
        true
    }

    /// Reads the facts the observer needs, and no more: every GPU process
    /// and its ancestors (up to [`MAX_ANCESTRY`]), every reservation root
    /// and process already known to belong to one, and the descendants of
    /// those, found depth first through the kernel's children lists. Every
    /// thread and every listed child, known or new, uses budget. Descendants are
    /// bounded by [`MAX_DESCENDANTS`], shared equally among the roots, so
    /// however many processes a host or a job has, the cost is bounded and
    /// one job cannot use up another's share.
    ///
    /// Only an unreadable procfs is an error; a process that vanishes is an
    /// expected race, and running out of budget only leaves some labels
    /// best-effort.
    pub fn collect(&self, seeds: &Seeds<'_>) -> Result<Collected, String> {
        std::fs::read_dir(&self.root).map_err(|e| format!("cannot read procfs: {e}"))?;
        let mut facts = BTreeMap::new();
        let mut truncated = false;
        let share = MAX_DESCENDANTS / seeds.roots.len().max(1);
        let children = self.lists_children();
        for &(root, root_identity) in seeds.roots {
            // A seed whose pid now names another process (the root or a
            // known descendant exited and the pid was reused) is not
            // followed, so a stranger cannot spend this job's budget.
            let candidates = std::iter::once(root_identity).chain(
                seeds
                    .known
                    .iter()
                    .filter(|&&(identity, owner)| owner == root && identity != root_identity)
                    .map(|&(identity, _)| identity),
            );
            let mut queue = Vec::new();
            for identity in candidates {
                if let Some(fact) = self.fact(identity.pid).filter(|f| f.identity == identity) {
                    facts.entry(identity.pid).or_insert(fact);
                    queue.push(identity.pid);
                }
            }
            // The walk takes from the end: walk the root's subtree first,
            // so known processes with many threads cannot use up the budget
            // before it. A known process inside that subtree is skipped there
            // as already collected, and its children are walked when it is
            // popped later, so none are lost.
            queue.reverse();
            if !children {
                continue;
            }
            let mut budget = share;
            while let Some(pid) = queue.pop() {
                let mut found = Vec::new();
                if !self.children(pid, &mut budget, &mut found) {
                    truncated = true;
                }
                for child in found {
                    if facts.contains_key(&child) {
                        continue;
                    }
                    // A pid reused since the list was read is someone
                    // else's: do not follow it.
                    if let Some(fact) = self.fact(child).filter(|f| f.parent == pid) {
                        facts.insert(child, fact);
                        queue.push(child);
                    }
                }
                if budget == 0 {
                    truncated |= !queue.is_empty();
                    break;
                }
            }
        }
        // Known processes of reservations no longer listed are left out:
        // the tracker drops them anyway. Ancestors share one budget, so many
        // GPU processes at the bottom of deep chains cost a bounded amount;
        // a chain cut short only leaves its process unlabelled.
        let mut ancestors = MAX_ANCESTORS;
        let mut gpu_pids = seeds.gpu_pids.to_vec();
        gpu_pids.sort_unstable();
        gpu_pids.dedup();
        'pids: for pid in gpu_pids {
            let mut current = pid;
            for _ in 0..MAX_ANCESTRY {
                if facts.contains_key(&current) {
                    break;
                }
                if ancestors == 0 {
                    break 'pids;
                }
                ancestors -= 1;
                let Some(fact) = self.fact(current) else {
                    break;
                };
                let parent = fact.parent;
                facts.insert(current, fact);
                if parent <= 1 || parent == current {
                    break;
                }
                current = parent;
            }
        }
        Ok(Collected {
            facts,
            truncated: truncated.then_some(share),
        })
    }

    /// Every process's facts, for tests that compare [`ProcFs::collect`]
    /// with the whole tree.
    #[cfg(test)]
    pub fn scan(&self) -> Result<BTreeMap<u32, ProcessFacts>, String> {
        let entries =
            std::fs::read_dir(&self.root).map_err(|e| format!("cannot read procfs: {e}"))?;
        Ok(entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
            .filter_map(|pid| Some((pid, self.fact(pid)?)))
            .collect())
    }
}

/// Descendant processes and threads [`ProcFs::collect`] follows per
/// observation, shared equally among the reservations. The same share bounds
/// how many processes the tracker adds per reservation; entries it learned
/// under a larger share are kept. Known processes' own facts are read again
/// without counting against the share; their threads and children count.
pub const MAX_DESCENDANTS: usize = 4096;

/// Processes [`ProcFs::collect`] reads for GPU processes and their parent
/// chains, in total per observation.
pub const MAX_ANCESTORS: usize = 16_384;

/// What [`ProcFs::collect`] starts from.
#[derive(Debug, Default)]
pub struct Seeds<'a> {
    /// Pids NVML reports on the observed GPUs.
    pub gpu_pids: &'a [u32],
    /// `(reservation id, identity)` of each reservation's root process.
    pub roots: &'a [(u64, ProcessIdentity)],
    /// `(identity, reservation id)` of processes already known to belong
    /// to one.
    pub known: &'a [(ProcessIdentity, u64)],
}

/// The facts [`ProcFs::collect`] read.
#[derive(Debug, Default)]
pub struct Collected {
    pub facts: BTreeMap<u32, ProcessFacts>,
    /// `Some(share)` when some reservation had more descendants to follow
    /// than its share of [`MAX_DESCENDANTS`].
    pub truncated: Option<usize>,
}

/// How a GPU process relates to the daemon's reservations.
pub use crate::protocol::ProcessOwnership as Ownership;

/// One dashboard row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    /// The daemon's index for this GPU when the UUID matches; NVML's when
    /// the daemon lists no devices; `None` for a GPU the daemon does not list.
    pub gpu: Option<u32>,
    pub pid: u32,
    pub uid: Option<u32>,
    pub used_memory: Option<u64>,
    /// Raw `comm`; the renderer sanitises it.
    pub command: Option<Vec<u8>>,
    pub ownership: Ownership,
}

/// Bounds the parent walk: a corrupt or hostile tree cannot loop forever.
pub const MAX_ANCESTRY: usize = 128;

fn owning_holder<'a>(
    pid: u32,
    holders: &HashMap<u32, &'a ReservationInfo>,
    procfs: &ProcFs,
) -> Option<&'a ReservationInfo> {
    let mut current = pid;
    for _ in 0..MAX_ANCESTRY {
        // The uid check guards against a holder pid reused since the snapshot.
        if let Some(holder) = holders
            .get(&current)
            .filter(|h| procfs.uid(current) == Some(h.uid))
        {
            return Some(holder);
        }
        match procfs.parent(current) {
            Some(parent) if parent > 1 && parent != current => current = parent,
            _ => return None,
        }
    }
    None
}

/// Labels every GPU process against the snapshot's holders. Rows are
/// ordered by GPU, then pid.
pub fn classify(
    processes: &[GpuProcess],
    status: &StatusReply,
    procfs: &ProcFs,
) -> Vec<ProcessRow> {
    // Pids 0 and 1 cannot be a `gpudog run` client; matching them would
    // claim every process on the machine.
    let holders: HashMap<u32, &ReservationInfo> = status
        .holders
        .iter()
        .filter_map(|h| u32::try_from(h.pid).ok().filter(|&p| p > 1).map(|p| (p, h)))
        .collect();
    let mut rows: Vec<ProcessRow> = processes
        .iter()
        .map(|process| {
            // Without a device list (a daemon run with `--num-gpus`) NVML's
            // index is the best guess. With one, an unlisted UUID has no
            // daemon index at all, and guessing could claim a reservation.
            let gpu = if status.devices.is_empty() {
                Some(process.nvml_index)
            } else {
                status
                    .devices
                    .iter()
                    .find(|d| d.uuid == process.gpu_uuid.as_bytes())
                    .map(|d| d.index)
            };
            let ownership = if procfs.parent(process.pid).is_none() {
                Ownership::Unknown
            } else {
                match (gpu, owning_holder(process.pid, &holders, procfs)) {
                    (Some(gpu), Some(holder)) if holder.gpus.contains(&gpu) => Ownership::Managed,
                    (_, Some(_)) => Ownership::WrongGpu,
                    (_, None) => Ownership::NotManaged,
                }
            };
            ProcessRow {
                gpu,
                pid: process.pid,
                uid: procfs.uid(process.pid),
                used_memory: process.used_memory,
                command: procfs.comm(process.pid),
                ownership,
            }
        })
        .collect();
    rows.sort_by_key(|row| (row.gpu, row.pid));
    rows
}

/// What the dashboard shows in its process section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessView {
    Rows(Vec<ProcessRow>),
    /// NVML could not be loaded or queried; the reason is shown, sanitised.
    Unavailable(String),
}

/// All process identity/metadata comes from the daemon's host observation,
/// never from a client's unrelated PID namespace or local NVML instance.
pub fn daemon_view(status: &StatusReply) -> ProcessView {
    if !status.observing {
        return ProcessView::Unavailable("NVML observation bypassed (test inventory)".into());
    }
    if status.observation_age_ms.is_none() {
        return ProcessView::Unavailable(if status.observation_error.is_empty() {
            "waiting for the first GPU observation".into()
        } else {
            crate::protocol::sanitize_display_text(&status.observation_error)
        });
    }
    ProcessView::Rows(
        status
            .processes
            .iter()
            .map(|p| ProcessRow {
                gpu: Some(p.gpu),
                pid: p.pid,
                uid: p.uid,
                used_memory: p.used_memory,
                command: (!p.command.is_empty()).then(|| p.command.clone()),
                ownership: p.ownership,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests;
