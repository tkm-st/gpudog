//! Observation-only process-tree tracking. Labels never authorize or signal
//! a process, and usage blocks allocation independently of the label.
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::gpu_inventory::GpuDevice;
use crate::gpu_processes::{
    GpuProcess, Ownership, ProcessFacts, ProcessIdentity, ProcessRow, MAX_DESCENDANTS,
};
use crate::protocol::{same_uuid, sanitize_display_text, MAX_COMMAND_SIZE};

#[derive(Debug, Clone)]
pub struct ReservationRoot {
    pub id: u64,
    pub identity: Option<ProcessIdentity>,
    pub uid: u32,
    /// Held devices, as the `index` of the observed [`GpuDevice`]s.
    pub gpus: Vec<u32>,
}

#[derive(Debug, Clone)]
pub struct ObservedProcess {
    /// `row.gpu` is the `index` of the observed [`GpuDevice`], which the
    /// daemon translates before showing it.
    pub row: ProcessRow,
    pub start_time: Option<u64>,
    pub owner: Option<u64>,
}

#[derive(Default)]
pub struct ProcessTracker {
    known: HashMap<ProcessIdentity, u64>,
}

impl ProcessTracker {
    /// `(identity, reservation id)` of every process known to belong to a
    /// reservation, for [`crate::gpu_processes::ProcFs::collect`] to read
    /// again.
    pub fn known(&self) -> Vec<(ProcessIdentity, u64)> {
        self.known
            .iter()
            .map(|(&identity, &owner)| (identity, owner))
            .collect()
    }

    /// Classifies `processes` on `devices`; processes on any other GPU are
    /// left out. Busy devices and rows name devices by their `index`.
    pub fn observe(
        &mut self,
        processes: &[GpuProcess],
        devices: &[GpuDevice],
        roots: &[ReservationRoot],
        facts: &BTreeMap<u32, ProcessFacts>,
    ) -> (Vec<u32>, Vec<ObservedProcess>) {
        let claims: HashMap<_, _> = roots.iter().map(|root| (root.id, root)).collect();
        self.known.retain(|identity, owner| {
            claims.contains_key(owner)
                && facts
                    .get(&identity.pid)
                    .is_some_and(|f| f.identity == *identity)
        });
        for root in roots {
            if let Some(identity) = root.identity {
                if identity.pid > 1
                    && facts
                        .get(&identity.pid)
                        .is_some_and(|f| f.identity == identity && f.uid == Some(root.uid))
                {
                    self.known.insert(identity, root.id);
                }
            }
        }
        // Remember every process found to descend from a root, not just
        // GPU processes, so a child stays associated after its parent exits
        // and it is reparented. Each reservation remembers at most its share
        // of `MAX_DESCENDANTS`, so one job's many processes cannot crowd out
        // another's; entries already known are never evicted.
        // Counted as `ProcFs::collect` counts them, so both use one share.
        let followed = roots
            .iter()
            .filter(|root| root.identity.is_some_and(|identity| identity.pid > 1))
            .count();
        let share = MAX_DESCENDANTS / followed.max(1);
        let mut counts: HashMap<u64, usize> = HashMap::new();
        for owner in self.known.values() {
            *counts.entry(*owner).or_default() += 1;
        }
        for fact in facts.values() {
            if self.known.contains_key(&fact.identity) {
                continue;
            }
            if let Ok(Some(owner)) = self.owner(fact.identity.pid, facts) {
                let count = counts.entry(owner).or_default();
                if *count < share {
                    *count += 1;
                    self.known.insert(fact.identity, owner);
                }
            }
        }
        let unresolved_root = roots.iter().any(|root| {
            root.identity.is_none_or(|identity| {
                !facts
                    .get(&identity.pid)
                    .is_some_and(|f| f.identity == identity && f.uid == Some(root.uid))
            })
        });
        let mut busy = Vec::new();
        let mut rows = Vec::new();
        for process in processes {
            let Some(device) = devices
                .iter()
                .find(|d| same_uuid(&d.uuid, &process.gpu_uuid))
            else {
                continue;
            };
            busy.push(device.index);
            let fact = facts.get(&process.pid);
            let owner = self.owner(process.pid, facts);
            let ownership = match owner {
                Ok(Some(owner)) => match claims.get(&owner) {
                    Some(root) if root.gpus.contains(&device.index) => Ownership::Managed,
                    Some(_) => Ownership::WrongGpu,
                    None => Ownership::Unknown,
                },
                Ok(None) if !unresolved_root => Ownership::NotManaged,
                _ => Ownership::Unknown,
            };
            let command = fact.and_then(|f| f.command.as_ref()).map(|bytes| {
                sanitize_display_text(&bytes[..bytes.len().min(MAX_COMMAND_SIZE as usize)])
                    .into_bytes()
            });
            rows.push(ObservedProcess {
                start_time: fact.map(|f| f.identity.start_time),
                owner: owner.ok().flatten(),
                row: ProcessRow {
                    gpu: Some(device.index),
                    pid: process.pid,
                    uid: fact.and_then(|f| f.uid),
                    used_memory: process.used_memory,
                    command,
                    ownership,
                },
            });
        }
        busy.sort_unstable();
        busy.dedup();
        rows.sort_by_key(|p| (p.row.gpu, p.row.pid));
        (busy, rows)
    }

    fn owner(&self, pid: u32, facts: &BTreeMap<u32, ProcessFacts>) -> Result<Option<u64>, ()> {
        let mut current = pid;
        let mut seen = HashSet::new();
        for _ in 0..128 {
            if !seen.insert(current) {
                return Err(());
            }
            let fact = facts.get(&current).ok_or(())?;
            if let Some(owner) = self.known.get(&fact.identity) {
                return Ok(Some(*owner));
            }
            if fact.parent <= 1 {
                return Ok(None);
            }
            current = fact.parent;
        }
        Err(())
    }
}

#[cfg(test)]
mod tests;
