//! Idle detection for `idle_stop_ms`: whether a reservation's GPUs are
//! unused, judged from one GPU observation, and how long that has lasted.
//!
//! A reservation is idle when none of its processes is on its GPUs, or
//! when every one of them that is there used at most the utilization
//! threshold.
//! Anything the observation cannot attribute makes the reservation
//! [`Activity::Unknown`], which never counts as idle: a wrong answer here
//! stops somebody's job.

use std::time::{Duration, Instant};

use crate::process_watcher::{ObservedProcess, ReservationRoot};

/// What one observation says about one reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Active,
    Idle,
    /// The observation cannot tell; treated like [`Activity::Active`].
    Unknown,
}

/// Judges `root` from the rows of one successful observation.
///
/// `resolved` says whether the reservation's own process was found in
/// procfs; without it, its processes cannot be told apart from anyone
/// else's. `threshold` is the highest utilization, in percent, that still
/// counts as idle.
pub fn activity(
    root: &ReservationRoot,
    resolved: bool,
    rows: &[ObservedProcess],
    threshold: u32,
) -> Activity {
    if !resolved {
        return Activity::Unknown;
    }
    let mut own = Vec::new();
    for process in rows {
        let mine = process.owner == Some(root.id);
        let on_my_gpu = process.row.gpu.is_some_and(|gpu| root.gpus.contains(&gpu));
        if mine && !on_my_gpu {
            // Its work runs on a GPU it was not given; it is not idle, and
            // what to do about that is not this feature's business.
            return Activity::Active;
        }
        if !on_my_gpu {
            continue;
        }
        if mine {
            own.push(process);
            continue;
        }
        // Only work known to belong to another reservation is not this
        // one's. Anything else on its GPU may be doing its work without
        // descending from it, whatever its uid: `docker run --gpus` (root,
        // or a rootless container's subordinate uid), a double-forked
        // worker, or an MPS server run by root or a service account.
        if process.owner.is_none() {
            return Activity::Unknown;
        }
    }
    let busy = own.iter().any(|process| {
        process
            .utilization
            .is_none_or(|percent| percent > threshold)
    });
    if own.is_empty() || !busy {
        Activity::Idle
    } else {
        Activity::Active
    }
}

/// How long a reservation has been idle without a break.
///
/// Starts at the grant, as if idle, so a job that never touches its GPUs
/// is stopped `limit` after it was granted. Any observation that is not
/// [`Activity::Idle`], and any failed or discarded observation, restarts
/// it.
#[derive(Debug, Clone)]
pub struct IdleClock {
    limit: Duration,
    since: Option<Instant>,
    fired: bool,
}

impl IdleClock {
    /// `limit` of zero never fires.
    pub fn new(limit: Duration, granted: Instant) -> Self {
        Self {
            limit,
            since: Some(granted),
            fired: false,
        }
    }

    /// Records one successful observation made at `now`. Returns the idle
    /// time when the limit is reached, once per reservation.
    pub fn observe(&mut self, activity: Activity, now: Instant) -> Option<Duration> {
        if activity != Activity::Idle {
            self.since = None;
            return None;
        }
        let since = *self.since.get_or_insert(now);
        let idle = now.saturating_duration_since(since);
        if self.limit.is_zero() || self.fired || idle < self.limit {
            return None;
        }
        self.fired = true;
        Some(idle)
    }

    /// An observation failed or was discarded: nothing is known, so the
    /// idle time starts again at the next idle observation.
    pub fn reset(&mut self) {
        self.since = None;
    }

    /// Idle time at `now`, or zero when not idle.
    pub fn idle(&self, now: Instant) -> Duration {
        self.since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since))
    }
}

#[cfg(test)]
mod tests;
