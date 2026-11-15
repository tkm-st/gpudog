//! The `gpudogd` listener: accepts connections and serves reservations.
//!
//! Designed so that three lifetime bugs cannot occur:
//!
//! * A queued session races "the scheduler reached me" against "the peer
//!   went away" with `select!`, which drops the losing branch whichever way
//!   it finished, so a dead peer cannot hang the session.
//! * The wakeup is a `oneshot` registered under the same lock as `submit`,
//!   and a `oneshot` keeps a value sent before anyone awaits it, so no
//!   wakeup is lost.
//! * Session cleanup (release, counters, pump) lives in [`SessionGuard`]'s
//!   `Drop`, which also runs when a task is cancelled at shutdown, and the
//!   guard owns an `Arc` of the state it touches, so cleanup never reaches
//!   freed state.
//!
//! All scheduler state sits behind one `std::sync::Mutex` that is never held
//! across an `.await`, so the runtime may be multi-threaded.

use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures_util::{FutureExt, SinkExt, StreamExt};
use nix::errno::Errno;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Notify};
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

use crate::gpu_inventory::{GpuDevice, GpuInventory};
use crate::gpu_processes::{GpuProcessSource, ManagedNvmlSource, ProcFs, ProcessIdentity, Seeds};
use crate::peer_credentials::{credentials_of, PeerCredentials};
use crate::process_watcher::{ObservedProcess, ProcessTracker, ReservationRoot};
use crate::scheduler::{
    BusyOwners, RequestId, ReservationOutcome, ReservationRequest, ReservationSnapshot, Scheduler,
    SchedulerLimits, Slot, SlotState,
};
use gpudog_core::protocol::codec::{CodecError, FrameCodec};
use gpudog_core::protocol::{
    self, sanitize_display_text, Acquire, Denied, DenyReason, DeviceInfo, GpuSelection, Granted,
    HelloAck, Message, ProtocolError, Queued, ReservationInfo, StatusReply, PROTOCOL_VERSION,
};

pub const DEFAULT_SOCKET_MODE: u32 = 0o666;
pub const DEFAULT_MAX_CONNECTIONS: usize = 256;
pub const DEFAULT_MAX_CONNECTIONS_PER_UID: usize = 16;
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_WATCH_UNMANAGED_MS: u64 = 1000;
pub const MAX_WATCH_UNMANAGED_MS: u64 = 3_600_000;

/// How long to wait before retrying a failed accept, and how many
/// consecutive failures to swallow between log lines.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
const ACCEPT_FAILURE_LOG_INTERVAL: u64 = 100;

/// How often a too-large process tree is reported again.
const TRUNCATION_LOG_INTERVAL: Duration = Duration::from_secs(3600);

/// How often one uid may log each kind of [`Noise`].
const NOISE_WINDOW: Duration = Duration::from_secs(10);
/// Uids tracked separately before [`LogThrottle`] falls back to one shared
/// window per kind, so a client switching uids cannot grow the map.
const NOISE_MAX_KEYS: usize = 4096;

/// Warnings and errors a client can provoke at will, with a connect loop or
/// garbage frames. Each kind is logged at most once per [`NOISE_WINDOW`] per
/// uid, so one user cannot fill the bounded log queue or the journal's rate
/// limit and push out everyone else's lines. Access lines (acquire, grant,
/// denial, release) are never throttled: they are the audit trail, and each
/// needs a real request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Noise {
    ConnectionLimit,
    UserConnectionLimit,
    RejectedPeer,
    VersionMismatch,
    IdentityUnavailable,
    UnmanagedDevice,
    HandshakeTimeout,
    TransportError,
    ProtocolViolation,
}

#[derive(Debug)]
struct NoiseWindow {
    opened: Instant,
    suppressed: u64,
}

/// `message`, noting how many similar lines were left out before it.
fn with_suppressed(message: &str, suppressed: u64) -> String {
    match suppressed {
        0 => message.to_owned(),
        1 => format!("{message} (1 similar line not logged)"),
        n => format!("{message} ({n} similar lines not logged)"),
    }
}

/// Per-uid, per-kind log windows for [`Noise`].
#[derive(Debug, Default)]
struct LogThrottle {
    windows: HashMap<(Option<u32>, Noise), NoiseWindow>,
}

impl LogThrottle {
    /// `Some(n)` when a line of this kind may be logged now, where `n` lines
    /// were suppressed since the last one; `None` when it must be dropped.
    fn admit(&mut self, uid: Option<u32>, noise: Noise, now: Instant) -> Option<u64> {
        let open =
            |window: &NoiseWindow| now.saturating_duration_since(window.opened) < NOISE_WINDOW;
        let mut key = (uid, noise);
        if !self.windows.contains_key(&key) && self.windows.len() >= NOISE_MAX_KEYS {
            self.windows.retain(|_, window| open(window));
            if self.windows.len() >= NOISE_MAX_KEYS {
                key = (None, noise);
            }
        }
        match self.windows.get_mut(&key) {
            Some(window) if open(window) => {
                window.suppressed += 1;
                None
            }
            Some(window) => {
                let suppressed = window.suppressed;
                *window = NoiseWindow {
                    opened: now,
                    suppressed: 0,
                };
                Some(suppressed)
            }
            None => {
                self.windows.insert(
                    key,
                    NoiseWindow {
                        opened: now,
                        suppressed: 0,
                    },
                );
                Some(0)
            }
        }
    }
}

/// Listener settings: everything `config.toml` can set except the obsolete
/// `log_directory` and `devices`, which the scheduler applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Bound socket path. The parent directory must already exist.
    pub socket_path: PathBuf,
    /// Mode applied with `chmod` after `bind`. Deliberately permissive: every
    /// local user may *ask*. Authorisation comes from `SO_PEERCRED`, not from
    /// this mode. `bind` applies the umask, so it must be set explicitly.
    pub socket_mode: u32,
    /// Refuse to start if the socket's directory is writable by anyone but its
    /// owner. A writable directory lets a local user unlink the socket, bind
    /// their own, and man-in-the-middle every client.
    pub require_secure_directory: bool,
    /// Upper bound on concurrent connections. The fd *is* the lock, so
    /// unbounded connections mean unbounded GPU claims.
    pub max_connections: usize,
    /// Upper bound on concurrent connections from one uid.
    pub max_connections_per_uid: usize,
    /// Deadline for the hello message and for reads while a connection holds
    /// no reservation. A connection holding or awaiting a reservation parks
    /// forever by design: that is the lock.
    pub handshake_timeout: Duration,
    pub scheduler_limits: SchedulerLimits,
    pub watch_unmanaged_ms: u64,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("/run/gpudog/gpudogd.sock"),
            socket_mode: DEFAULT_SOCKET_MODE,
            require_secure_directory: true,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_connections_per_uid: DEFAULT_MAX_CONNECTIONS_PER_UID,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            scheduler_limits: SchedulerLimits::default(),
            watch_unmanaged_ms: DEFAULT_WATCH_UNMANAGED_MS,
        }
    }
}

/// Severity-tagged events the service emits, so the daemon can tag its log
/// lines without this layer depending on a logger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Access,
    Warning,
    Error,
}

pub type LogSink = Arc<dyn Fn(LogLevel, &str) + Send + Sync>;

/// A sink that drops every event.
pub fn discard_log() -> LogSink {
    Arc::new(|_, _| {})
}

/// Why [`Service::start`] refused.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct StartError(pub String);

/// What a session told us about itself, kept only for the dashboard.
/// Untrusted: `command` is sanitised client text, and `pid` is a host pid
/// this daemon cannot resolve.
#[derive(Debug)]
struct SessionInfo {
    pid: i32,
    command: String,
    identity: Option<ProcessIdentity>,
}

#[derive(Default)]
struct Observation {
    enabled: bool,
    period: Duration,
    completed: Option<Instant>,
    error: String,
    busy: Vec<Slot>,
    /// `row.gpu` holds a slot number; [`snapshot`] translates it.
    rows: Vec<ObservedProcess>,
    /// An observation had processes, none with a readable owner, and it
    /// was logged; once per daemon, as the condition comes and goes with
    /// every job.
    blind_warned: bool,
    /// When a too-large process tree was last reported.
    truncated_logged: Option<Instant>,
    tracker: ProcessTracker,
    /// Bumped whenever the managed slots change. A query started under an
    /// older generation looked at the wrong GPUs, so its result is dropped.
    generation: u64,
}

/// A scheduler outcome in the client's terms: daemon GPU numbers and UUIDs.
/// Converted under the lock that decided it, so a reload cannot renumber
/// the devices in between.
#[derive(Debug, Clone)]
enum Reply {
    Granted { gpus: Vec<u32>, uuids: Vec<String> },
    Queued { position: u32 },
    Denied { reason: DenyReason, detail: String },
}

/// How a reload changed the managed devices, by UUID.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceChanges {
    /// Newly managed.
    pub added: Vec<String>,
    /// No longer managed, and free.
    pub removed: Vec<String>,
    /// No longer managed, but held: kept by the holder, never allocated
    /// again.
    pub retiring: Vec<String>,
}

impl DeviceChanges {
    pub fn changed(&self) -> bool {
        !(self.added.is_empty() && self.removed.is_empty() && self.retiring.is_empty())
    }
}

/// Why [`Service::reconfigure`] refused a configuration. Nothing changed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ReconfigureError(pub String);

/// One daemon GPU, as [`Service::devices`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownDevice {
    /// The device, with `index` set to its daemon GPU number.
    pub device: GpuDevice,
    /// Removed from `devices` by a reload; never allocated again.
    pub retiring: bool,
    /// Held by a reservation.
    pub held: bool,
}

/// Everything sessions share, behind one lock.
struct State {
    scheduler: Scheduler,
    /// The startup device list. A slot is an index into it.
    inventory: Arc<dyn GpuInventory>,
    /// Queued sessions, woken by [`State::pump`].
    waiters: HashMap<RequestId, oneshot::Sender<Reply>>,
    /// Per-reservation display metadata, erased with the reservation.
    sessions: HashMap<RequestId, SessionInfo>,
    active: usize,
    per_uid: HashMap<u32, usize>,
    next_id: RequestId,
    observation: Observation,
    stopped: bool,
    /// The configuration in force. A reload ([`Service::reconfigure`])
    /// replaces the limits here; the socket settings keep their startup
    /// values, which [`Shared::socket_path`] also holds.
    config: ServiceConfig,
}

impl State {
    /// Submits `request` and, if it queues, registers its waiter, both under
    /// the caller's one lock so no pump can run in between and find nobody
    /// to wake. Returns the reply and, for a queued request, the receiver of
    /// its later outcome.
    fn submit(
        &mut self,
        request: &ReservationRequest,
        now: Instant,
    ) -> (Reply, Option<oneshot::Receiver<Reply>>) {
        // First, so an observation that lapsed meanwhile cannot grant this
        // request in the pump below, right after it was told it queued.
        self.ensure_fresh(now);
        let outcome = self.scheduler.submit(request, now);
        let wakeup = matches!(outcome, ReservationOutcome::Queued { .. }).then(|| {
            let (sender, receiver) = oneshot::channel();
            self.waiters.insert(request.id, sender);
            receiver
        });
        let reply = self.reply(outcome);
        // The submit's walk may have granted or expired other requests;
        // their outcomes wait in the scheduler until this pump reports them.
        // If this request queued, its waiter is already registered.
        self.pump(now);
        (reply, wakeup)
    }

    /// Runs the scheduler forward and wakes every session whose state
    /// changed.
    fn pump(&mut self, now: Instant) {
        self.ensure_fresh(now);
        loop {
            let mut reclaimed = false;
            for transition in self.scheduler.tick(now) {
                let granted = matches!(transition.outcome, ReservationOutcome::Granted { .. });
                let reply = self.reply(transition.outcome);
                let delivered = self
                    .waiters
                    .remove(&transition.id)
                    .is_some_and(|waiter| waiter.send(reply).is_ok());
                // Defence in depth: a grant nobody can receive (the session is
                // already unwinding) must not leak its devices until the guard
                // gets round to releasing them.
                if !delivered && granted {
                    reclaimed |= self.scheduler.release(transition.id);
                }
            }
            // Reclaimed devices may satisfy a waiting request; tick again
            // here rather than rely on the unwinding guard's own pump.
            if !reclaimed {
                break;
            }
        }
    }

    fn ensure_fresh(&mut self, now: Instant) {
        if self.stopped {
            self.scheduler
                .set_observed_usage(true, &self.observation.busy);
            return;
        }
        if self.observation.enabled
            && (self
                .observation
                .completed
                .is_none_or(|time| now.saturating_duration_since(time) > self.observation.period)
                || !self.observation.error.is_empty())
        {
            self.scheduler
                .set_observed_usage(true, &self.observation.busy);
        }
    }

    fn roots(&self) -> Vec<ReservationRoot> {
        self.scheduler
            .held_reservations()
            .into_iter()
            .map(|held| ReservationRoot {
                id: held.id,
                uid: held.uid,
                gpus: held.gpus.iter().map(|slot| slot.0).collect(),
                identity: self.sessions.get(&held.id).and_then(|s| s.identity),
            })
            .collect()
    }

    /// The managed devices, whose `index` is their slot, for an observation.
    fn managed_devices(&self) -> Vec<GpuDevice> {
        self.scheduler
            .managed()
            .into_iter()
            .filter_map(|slot| self.inventory.devices().get(slot.0 as usize).cloned())
            .collect()
    }

    fn uuid(&self, slot: Slot) -> Option<&str> {
        self.inventory
            .devices()
            .get(slot.0 as usize)
            .map(|device| device.uuid.as_str())
    }

    /// `outcome` in the client's terms, under the current numbering.
    fn reply(&self, outcome: ReservationOutcome) -> Reply {
        match outcome {
            ReservationOutcome::Granted { gpus } => {
                // Invariant: the scheduler grants only managed slots, which
                // are shown and have UUIDs. Should that ever break, refuse
                // rather than panic or hand out a wrong device.
                let named: Option<(Vec<u32>, Vec<String>)> = gpus
                    .iter()
                    .map(|&slot| {
                        Some((
                            self.scheduler.display_of(slot)?,
                            self.uuid(slot)?.to_owned(),
                        ))
                    })
                    .collect();
                match named {
                    Some((gpus, uuids)) => Reply::Granted { gpus, uuids },
                    None => Reply::Denied {
                        reason: DenyReason::INTERNAL_ERROR,
                        detail: "granted GPU has no UUID".into(),
                    },
                }
            }
            ReservationOutcome::Queued { position } => Reply::Queued { position },
            ReservationOutcome::Denied { reason, detail } => Reply::Denied { reason, detail },
        }
    }

    fn resume_observed_usage(&mut self, busy: &[Slot]) {
        if !self.stopped {
            self.scheduler.set_observed_usage(false, busy);
        }
    }
}

struct Shared {
    /// Fixed at startup: the socket cannot move without a restart.
    socket_path: PathBuf,
    log: LogSink,
    state: Mutex<State>,
    /// Tells the deadline task that the earliest deadline may have changed.
    rearm: Notify,
    /// Wakes the observation task early, after a reload changed its period.
    reobserve: Notify,
    procfs: ProcFs,
    /// Separate from `state`, so throttling a log line never waits for the
    /// scheduler.
    throttle: Mutex<LogThrottle>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves the scheduler as consistent
        // as the panicking code left it; serving on beats wedging every
        // session.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn log(&self, level: LogLevel, message: &str) {
        (self.log)(level, message);
    }

    /// Logs a line a client can provoke at will, at most once per
    /// [`NOISE_WINDOW`] per uid and kind (see [`Noise`]). `uid` is `None`
    /// before the peer is known.
    fn log_noise(&self, uid: Option<u32>, noise: Noise, level: LogLevel, message: &str) {
        let admitted = self
            .throttle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admit(uid, noise, Instant::now());
        if let Some(suppressed) = admitted {
            self.log(level, &with_suppressed(message, suppressed));
        }
    }

    fn pump(&self) {
        self.lock().pump(Instant::now());
        self.rearm.notify_one();
    }
}

/// Unwinds one session exactly once, on every exit path: normal return,
/// error, or the task being dropped at shutdown.
struct SessionGuard {
    shared: Arc<Shared>,
    id: RequestId,
    uid: Option<u32>,
    peer_text: String,
    identity: Option<ProcessIdentity>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        // The single most important code in the daemon: whatever happened,
        // the connection is over, so its GPUs go back.
        let released = {
            let mut state = self.shared.lock();
            // Counted in `accept_loop` before this guard existed, so zero
            // here is a bookkeeping bug: assert in debug, never wrap.
            debug_assert!(state.active > 0, "active connection count underflow");
            state.active = state.active.saturating_sub(1);
            if let Some(uid) = self.uid {
                if let Some(count) = state.per_uid.get_mut(&uid) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        state.per_uid.remove(&uid);
                    }
                }
            }
            state.waiters.remove(&self.id);
            state.sessions.remove(&self.id);
            let released = state.scheduler.release(self.id);
            state.pump(Instant::now());
            released
        };
        self.shared.rearm.notify_one();
        if released {
            self.shared.log(
                LogLevel::Access,
                &format!("{} released its reservation", self.peer_text),
            );
        }
    }
}

/// How a session ended abnormally.
#[derive(Debug)]
enum SessionError {
    Transport(io::Error),
    HandshakeTimeout,
    Protocol(ProtocolError),
}

impl From<CodecError> for SessionError {
    fn from(error: CodecError) -> Self {
        match error {
            CodecError::Io(error) => Self::Transport(error),
            CodecError::Protocol(error) => Self::Protocol(error),
        }
    }
}

type Line = Framed<UnixStream, FrameCodec>;

/// What a read produced: a message, or a clean end of stream.
async fn read(line: &mut Line) -> Result<Option<Message>, SessionError> {
    line.next().await.transpose().map_err(SessionError::from)
}

async fn write(line: &mut Line, message: impl Into<Message>) -> Result<(), SessionError> {
    line.send(message.into()).await.map_err(SessionError::from)
}

/// [`read`] with a deadline. Dropping the pending read on expiry is safe:
/// `Framed` keeps any partial frame in its buffer.
async fn read_within(line: &mut Line, timeout: Duration) -> Result<Option<Message>, SessionError> {
    tokio::time::timeout(timeout, read(line))
        .await
        .map_err(|_| SessionError::HandshakeTimeout)?
}

/// [`write`] with a deadline, for the pre-acquire phase.
async fn write_within(
    timeout: Duration,
    line: &mut Line,
    message: impl Into<Message>,
) -> Result<(), SessionError> {
    tokio::time::timeout(timeout, write(line, message))
        .await
        .map_err(|_| SessionError::HandshakeTimeout)?
}

fn denied(reason: DenyReason, detail: impl Into<String>) -> Denied {
    Denied {
        reason,
        detail: detail.into().into_bytes(),
    }
}

fn to_message(reply: Reply) -> Message {
    match reply {
        Reply::Granted { gpus, uuids } => Granted { gpus, uuids }.into(),
        Reply::Queued { position } => Queued { position }.into(),
        Reply::Denied { reason, detail } => denied(reason, detail).into(),
    }
}

fn join(gpus: &[u32]) -> String {
    gpus.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn describe(request: &Acquire) -> String {
    match request.selection {
        GpuSelection::ExplicitList => format!("gpus=[{}]", join(&request.gpus)),
        // Already validated by the codec: GPU-, letters, digits, hyphens.
        GpuSelection::ExplicitUuids => format!("uuids=[{}]", request.uuids.join(",")),
        // Candidates are validated by the codec, so safe to log as-is.
        GpuSelection::Count if !request.candidates.is_empty() => format!(
            "num_gpus={} candidates=[{}]",
            request.num_gpus,
            request.candidates.join(",")
        ),
        GpuSelection::Count => format!("num_gpus={}", request.num_gpus),
    }
}

/// An `Acquire` in scheduler terms.
struct Resolved {
    selection: GpuSelection,
    gpus: Vec<Slot>,
    candidates: Option<Vec<Slot>>,
}

/// Why an `Acquire` names a device that cannot be resolved: the denial, and
/// the warning to log, if any.
type Unresolved = (Denied, Option<String>);

impl State {
    fn slot_by_uuid(&self, uuid: &str) -> Option<Slot> {
        self.inventory
            .devices()
            .iter()
            .position(|device| protocol::same_uuid(&device.uuid, uuid))
            .map(|position| Slot(position as u32))
    }

    /// Maps the request's devices onto slots. A slot resolved here may stop
    /// being managed before submission; the scheduler checks again.
    fn resolve(&self, request: &Acquire) -> Result<Resolved, Unresolved> {
        // The scheduler only knows slots; a UUID request is turned into a
        // list here so it queues, is ordered and times out identically.
        let (selection, gpus) = match request.selection {
            GpuSelection::ExplicitUuids => {
                let mut gpus = Vec::new();
                for uuid in &request.uuids {
                    match self
                        .slot_by_uuid(uuid)
                        .filter(|&slot| self.scheduler.slot_state(slot) == Some(SlotState::Managed))
                    {
                        Some(slot) => gpus.push(slot),
                        None => {
                            return Err((
                                denied(
                                    DenyReason::INVALID_REQUEST,
                                    format!("{uuid} is not managed by this daemon"),
                                ),
                                Some(format!("requested unmanaged device {uuid}")),
                            ))
                        }
                    }
                }
                (GpuSelection::ExplicitList, gpus)
            }
            // The codec refuses it on the wire: daemon GPU numbers change
            // at a reload, so they are no way to name a device.
            GpuSelection::ExplicitList => {
                return Err((
                    denied(
                        DenyReason::INVALID_REQUEST,
                        "requests by daemon GPU number are not accepted",
                    ),
                    None,
                ))
            }
            GpuSelection::Count => (GpuSelection::Count, Vec::new()),
        };
        // Candidates this daemon never saw are dropped, not refused: the
        // caller's CUDA_VISIBLE_DEVICES may name GPUs another daemon owns.
        // Unmanaged ones are kept, since a reload may add them; the
        // scheduler allocates only managed ones. A non-empty list that
        // resolves to nothing stays `Some(empty)`, which the scheduler
        // refuses as unsatisfiable.
        let candidates = (!request.candidates.is_empty()).then(|| {
            let mut slots: Vec<Slot> = Vec::new();
            for slot in request
                .candidates
                .iter()
                .filter_map(|uuid| self.slot_by_uuid(uuid))
            {
                if !slots.contains(&slot) {
                    slots.push(slot);
                }
            }
            slots
        });
        Ok(Resolved {
            selection,
            gpus,
            candidates,
        })
    }
}

fn errno_text(error: &io::Error) -> String {
    error.raw_os_error().map_or_else(
        || error.to_string(),
        |raw| Errno::from_raw(raw).desc().to_owned(),
    )
}

/// Accepts connections and serves the protocol. Dropping it stops accepting
/// and removes the socket; live sessions end when their connections close.
pub struct Service {
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
    socket_bound: bool,
}

/// Which slots of `inventory` the `devices` allow-list names: every slot
/// when it is unset. Refuses a UUID the inventory does not have.
fn managed_flags(
    inventory: &dyn GpuInventory,
    devices: Option<&[String]>,
) -> Result<Vec<bool>, String> {
    let all = inventory.devices();
    let Some(devices) = devices else {
        return Ok(vec![true; all.len()]);
    };
    if let Some(missing) = devices.iter().find(|uuid| {
        !all.iter()
            .any(|device| protocol::same_uuid(&device.uuid, uuid))
    }) {
        return Err(format!(
            "configured device {missing} was not found when gpudogd started"
        ));
    }
    Ok(all
        .iter()
        .map(|device| {
            devices
                .iter()
                .any(|uuid| protocol::same_uuid(&device.uuid, uuid))
        })
        .collect())
}

impl Service {
    /// Validates the socket directory, binds, chmods, and starts accepting.
    /// Must be called within a tokio runtime.
    ///
    /// `inventory` is every device the daemon can see; `devices`, when set,
    /// is the allow-list of UUIDs to manage. Only devices in `inventory` can
    /// be managed, then or after a reload.
    pub fn start(
        inventory: Arc<dyn GpuInventory>,
        devices: Option<&[String]>,
        config: ServiceConfig,
        log: LogSink,
    ) -> Result<Self, StartError> {
        let source: Option<Arc<dyn GpuProcessSource>> = (inventory.source() != "static")
            .then(|| Arc::new(ManagedNvmlSource::new()) as Arc<dyn GpuProcessSource>);
        Self::start_inner(inventory, devices, config, log, source, ProcFs::default())
    }

    /// Inject observation providers for deterministic tests or trusted
    /// integrations. Every device of `inventory` is managed. Production
    /// always uses NVML for the managed UUID subset.
    pub fn start_with_observer(
        inventory: Arc<dyn GpuInventory>,
        config: ServiceConfig,
        log: LogSink,
        source: Arc<dyn GpuProcessSource>,
        procfs: ProcFs,
    ) -> Result<Self, StartError> {
        Self::start_inner(inventory, None, config, log, Some(source), procfs)
    }

    fn start_inner(
        inventory: Arc<dyn GpuInventory>,
        devices: Option<&[String]>,
        config: ServiceConfig,
        log: LogSink,
        source: Option<Arc<dyn GpuProcessSource>>,
        procfs: ProcFs,
    ) -> Result<Self, StartError> {
        if config.watch_unmanaged_ms == 0 || config.watch_unmanaged_ms > MAX_WATCH_UNMANAGED_MS {
            return Err(StartError(
                "watch_unmanaged_ms must be in 1..=3600000".into(),
            ));
        }
        let managed = managed_flags(inventory.as_ref(), devices).map_err(StartError)?;
        let observing = source.is_some();
        let socket_mode = config.socket_mode;
        let mut scheduler = Scheduler::new(inventory.as_ref(), config.scheduler_limits);
        // Every slot starts managed, so none is reported as newly added.
        let _ = scheduler.set_managed(&managed);
        scheduler.set_observed_usage(observing, &[]);
        let path = config.socket_path.clone();
        if config.require_secure_directory {
            let directory = socket_directory(&path);
            if let Some(complaint) = directory_complaint(directory) {
                return Err(StartError(format!("refusing to bind: {complaint}")));
            }
        }

        // A leftover socket from a crashed daemon must be removed, but only
        // after proving nobody is listening on it, or two daemons fight over
        // one path and the second silently steals every client. Anything
        // that is not a socket is left alone: a mistyped `socket_path` must
        // not delete a real file.
        if let Ok(existing) = path.symlink_metadata() {
            if !existing.file_type().is_socket() {
                return Err(StartError(format!(
                    "{} exists and is not a socket",
                    path.display()
                )));
            }
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                return Err(StartError(format!(
                    "another gpudogd is already listening on {}",
                    path.display()
                )));
            }
            let _ = std::fs::remove_file(&path);
        }

        let listener = bind(&path).map_err(|e| {
            StartError(format!(
                "cannot bind {}: {}",
                path.display(),
                errno_text(&e)
            ))
        })?;
        let mut service = Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    scheduler,
                    inventory,
                    waiters: HashMap::new(),
                    sessions: HashMap::new(),
                    active: 0,
                    per_uid: HashMap::new(),
                    next_id: 1,
                    stopped: false,
                    observation: Observation {
                        enabled: observing,
                        period: Duration::from_millis(config.watch_unmanaged_ms),
                        ..Observation::default()
                    },
                    config,
                }),
                socket_path: path.clone(),
                log,
                rearm: Notify::new(),
                reobserve: Notify::new(),
                procfs,
                throttle: Mutex::new(LogThrottle::default()),
            }),
            tasks: Vec::new(),
            socket_bound: true,
        };

        // bind(2) applies the umask, so the mode has to be set explicitly or
        // every non-root client gets EACCES.
        let mode = socket_mode;
        if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)) {
            service.stop();
            return Err(StartError(format!(
                "cannot chmod {}: {}",
                path.display(),
                errno_text(&error)
            )));
        }

        let device_count = service.shared.lock().scheduler.device_count();
        service.shared.log(
            LogLevel::Access,
            &format!("listening on {} with {device_count} GPU(s)", path.display()),
        );
        // An unreadable procfs is reported by the observation itself.
        if observing && service.shared.procfs.readable() && !service.shared.procfs.lists_children()
        {
            service.shared.log(
                LogLevel::Access,
                "this kernel does not list child processes; processes a job leaves behind may show as not managed",
            );
        }
        service
            .tasks
            .push(tokio::spawn(accept_loop(service.shared.clone(), listener)));
        service
            .tasks
            .push(tokio::spawn(deadline_loop(service.shared.clone())));
        if let Some(source) = source {
            service.tasks.push(tokio::spawn(observation_loop(
                service.shared.clone(),
                source,
            )));
        }
        Ok(service)
    }

    /// Stops accepting, stops expiring deadlines, and removes the socket
    /// file. Idempotent.
    ///
    /// Live sessions are not ended: they run until their connections close
    /// (or the runtime is dropped). Queued ones are never granted after
    /// stop, and no longer time out without the clock task.
    pub fn stop(&mut self) {
        {
            let mut state = self.shared.lock();
            state.stopped = true;
            let busy = state.observation.busy.clone();
            state.scheduler.set_observed_usage(true, &busy);
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
        if std::mem::take(&mut self.socket_bound) {
            let _ = std::fs::remove_file(&self.shared.socket_path);
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.shared.socket_path
    }

    /// The configuration in force, including anything `config.toml` or a
    /// reload changed.
    pub fn config(&self) -> ServiceConfig {
        self.shared.lock().config.clone()
    }

    /// Applies a reloaded configuration's limits and `devices` allow-list,
    /// from the next connection, request or observation on.
    ///
    /// Forward only: no connection is closed and no reservation is taken
    /// back. A held device that is no longer listed keeps its holder and is
    /// never allocated again (see [`Scheduler::set_managed`]). Waiting
    /// requests keep waiting, except those the new limits or devices make
    /// impossible, which are denied. A newly managed device counts as busy
    /// until an observation that includes it succeeds. The socket settings
    /// cannot change without a restart and are ignored here; the caller
    /// reports them.
    ///
    /// Refuses, changing nothing, a `devices` entry that was not in the
    /// startup inventory: CUDA's device list is fixed for the process's
    /// lifetime, so a GPU that appeared since needs a restart.
    ///
    /// `config` must already be validated ([`crate::config::DaemonConfig::validate`]).
    pub fn reconfigure(
        &self,
        config: &ServiceConfig,
        devices: Option<&[String]>,
    ) -> Result<DeviceChanges, ReconfigureError> {
        let changes = {
            let mut guard = self.shared.lock();
            let state = &mut *guard;
            let managed =
                managed_flags(state.inventory.as_ref(), devices).map_err(ReconfigureError)?;
            let before: Vec<Option<SlotState>> = (0u32..)
                .take(managed.len())
                .map(|slot| state.scheduler.slot_state(Slot(slot)))
                .collect();
            let added = state.scheduler.set_managed(&managed);
            let mut changes = DeviceChanges::default();
            for (index, was) in (0u32..).zip(before) {
                let slot = Slot(index);
                let uuid = state.uuid(slot).unwrap_or_default().to_owned();
                match (was, state.scheduler.slot_state(slot)) {
                    (Some(SlotState::Managed), Some(SlotState::Managed)) => {}
                    (_, Some(SlotState::Managed)) => changes.added.push(uuid),
                    (Some(SlotState::Managed), Some(SlotState::Retiring)) => {
                        changes.retiring.push(uuid);
                    }
                    (Some(SlotState::Managed), _) => changes.removed.push(uuid),
                    _ => {}
                }
            }
            let observing = state.observation.enabled;
            if observing && changes.changed() {
                let observation = &mut state.observation;
                observation.generation += 1;
                // Unknown until observed: busy, so nothing is granted on a
                // GPU that may be running someone else's job.
                observation.busy.extend(&added);
                // Rows of a device no longer observed would never refresh.
                let scheduler = &state.scheduler;
                state.observation.rows.retain(|process| {
                    process.row.gpu.is_some_and(|slot| {
                        scheduler.slot_state(Slot(slot)) == Some(SlotState::Managed)
                    })
                });
                let paused = state.scheduler.allocation_paused();
                let busy = state.observation.busy.clone();
                state.scheduler.set_observed_usage(paused, &busy);
            }
            let live = &mut state.config;
            live.max_connections = config.max_connections;
            live.max_connections_per_uid = config.max_connections_per_uid;
            live.handshake_timeout = config.handshake_timeout;
            live.scheduler_limits = config.scheduler_limits;
            live.watch_unmanaged_ms = config.watch_unmanaged_ms;
            state.observation.period = Duration::from_millis(config.watch_unmanaged_ms);
            state.scheduler.set_limits(config.scheduler_limits);
            state.pump(Instant::now());
            changes
        };
        self.shared.rearm.notify_one();
        // A shorter period must not leave the task asleep on the old one
        // while freshness is judged by the new one, and a newly managed GPU
        // should be observed, and so usable, at once.
        self.shared.reobserve.notify_one();
        Ok(changes)
    }

    /// The daemon GPUs, in daemon GPU number order, as clients see them.
    pub fn devices(&self) -> Vec<ShownDevice> {
        let state = self.shared.lock();
        state
            .scheduler
            .shown()
            .iter()
            .zip(0u32..)
            .filter_map(|(&slot, number)| {
                let mut device = state.inventory.devices().get(slot.0 as usize)?.clone();
                device.index = number;
                Some(ShownDevice {
                    device,
                    retiring: state.scheduler.slot_state(slot) == Some(SlotState::Retiring),
                    held: state.scheduler.is_held(slot),
                })
            })
            .collect()
    }

    pub fn active_connections(&self) -> usize {
        self.shared.lock().active
    }

    /// Live connections for `uid`, including ones still handshaking.
    pub fn connections_per_uid(&self, uid: u32) -> usize {
        self.shared.lock().per_uid.get(&uid).copied().unwrap_or(0)
    }

    /// Runs `f` against the scheduler, for inspection.
    pub fn with_scheduler<T>(&self, f: impl FnOnce(&Scheduler) -> T) -> T {
        f(&self.shared.lock().scheduler)
    }

    /// A read-only snapshot of the whole machine, as `gpudog dash` shows it.
    pub fn snapshot(&self) -> StatusReply {
        snapshot(&self.shared)
    }
}

/// One in-flight NVML/procfs query per observer. Heavy IO and ancestry work
/// run off the runtime/state mutex. Failures never kill jobs or revoke locks.
async fn observation_loop(shared: Arc<Shared>, source: Arc<dyn GpuProcessSource>) {
    loop {
        let (roots, mut tracker, devices, generation) = {
            let mut state = shared.lock();
            if state.stopped {
                return;
            }
            let busy = state.observation.busy.clone();
            state.scheduler.set_observed_usage(true, &busy);
            (
                state.roots(),
                std::mem::take(&mut state.observation.tracker),
                // Retiring devices are not observed: one that fails while
                // its holder hangs on must not pause the whole machine.
                state.managed_devices(),
                state.observation.generation,
            )
        };
        let provider = source.clone();
        let procfs = shared.procfs.clone();
        let result = tokio::task::spawn_blocking(move || {
            let uuids: Vec<String> = devices.iter().map(|d| d.uuid.clone()).collect();
            let observed = provider
                .processes(&uuids)
                .map_err(|e| e.0)
                .and_then(|processes| {
                    let gpu_pids: Vec<u32> = processes.iter().map(|p| p.pid).collect();
                    let root_pids: Vec<(u64, ProcessIdentity)> = roots
                        .iter()
                        .filter_map(|root| {
                            Some((root.id, root.identity?)).filter(|(_, identity)| identity.pid > 1)
                        })
                        .collect();
                    let known = tracker.known();
                    let collected = procfs.collect(&Seeds {
                        gpu_pids: &gpu_pids,
                        roots: &root_pids,
                        known: &known,
                    })?;
                    Ok((
                        tracker.observe(&processes, &devices, &roots, &collected.facts),
                        collected.truncated,
                    ))
                });
            (tracker, observed)
        })
        .await;
        let (tracker, observed) = result.unwrap_or_else(|_| {
            (
                ProcessTracker::default(),
                Err("GPU observation worker failed".into()),
            )
        });
        let mut report = None;
        let mut unattributed = None;
        let mut truncation = None;
        let mut again = false;
        {
            let mut state = shared.lock();
            if state.stopped {
                return;
            }
            state.observation.tracker = tracker;
            if state.observation.generation != generation {
                // A reload changed the managed devices meanwhile. Allocation
                // stays paused, as it has been since this query started,
                // until a query of the current devices succeeds.
                again = true;
                report = Some((
                    LogLevel::Access,
                    "the managed GPUs changed during a GPU observation; observing again".to_owned(),
                ));
            } else {
                match observed {
                    Ok(((busy, mut rows), truncated)) => {
                        let now = Instant::now();
                        if let Some(share) = truncated.filter(|_| {
                            state.observation.truncated_logged.is_none_or(|at| {
                                now.saturating_duration_since(at) >= TRUNCATION_LOG_INTERVAL
                            })
                        }) {
                            state.observation.truncated_logged = Some(now);
                            truncation = Some(share);
                        }
                        let busy: Vec<Slot> = busy.into_iter().map(Slot).collect();
                        // A holder can release while IO is in flight, but no new
                        // grant is made while paused. Do not retain its old label.
                        for process in &mut rows {
                            if process.owner.is_some_and(|id| !state.scheduler.holds(id)) {
                                process.row.ownership = crate::gpu_processes::Ownership::NotManaged;
                            }
                        }
                        let recovered = !state.observation.error.is_empty();
                        state.observation.completed = Some(Instant::now());
                        state.observation.error.clear();
                        let owners = busy_owners(&busy, &rows);
                        // No process with a readable owner at all: almost
                        // always a containerized daemon without --pid=host.
                        // Busy GPUs then count as nobody's, so waiting on
                        // your own GPU holds up others; say so once.
                        let blind = !rows.is_empty() && rows.iter().all(|p| p.row.uid.is_none());
                        if blind && !state.observation.blind_warned {
                            state.observation.blind_warned = true;
                            unattributed = Some(
                                "no observed GPU process has a readable owner; busy GPUs cannot be \
                                 counted as anyone's (run a containerized gpudogd with --pid=host)",
                            );
                        }
                        state.observation.rows = rows;
                        state.observation.busy = busy.clone();
                        state.scheduler.set_busy_owners(&owners);
                        state.resume_observed_usage(&busy);
                        if recovered {
                            report = Some((
                                LogLevel::Access,
                                "GPU observation recovered; new grants resumed".to_owned(),
                            ));
                        }
                    }
                    Err(error) => {
                        let error = sanitize_display_text(
                            &error.as_bytes()
                                [..error.len().min(protocol::MAX_COMMAND_SIZE as usize)],
                        );
                        let error = if error.is_empty() {
                            "GPU observation failed without details".to_owned()
                        } else {
                            error
                        };
                        if state.observation.error != error {
                            report = Some((
                                LogLevel::Error,
                                format!("GPU observation failed; new grants paused: {error}"),
                            ));
                        }
                        state.observation.error = error;
                        let busy = state.observation.busy.clone();
                        state.scheduler.set_observed_usage(true, &busy);
                    }
                }
            }
            state.pump(Instant::now()); // deadlines still expire while paused
        }
        if let Some((level, message)) = report {
            shared.log(level, &message);
        }
        if let Some(message) = unattributed {
            shared.log(LogLevel::Warning, message);
        }
        if let Some(share) = truncation {
            shared.log(
                LogLevel::Access,
                &format!(
                    "a job has more than {share} processes to follow; some of its processes may show as not managed"
                ),
            );
        }
        shared.rearm.notify_one();
        if again {
            continue;
        }
        let period = shared.lock().observation.period;
        tokio::select! {
            () = tokio::time::sleep(period) => {}
            () = shared.reobserve.notified() => {}
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Binds with the kernel's maximum backlog (`SOMAXCONN`); std's default of
/// 128 refuses bursts earlier.
fn bind(path: &Path) -> io::Result<UnixListener> {
    use nix::sys::socket::{self, AddressFamily, Backlog, SockFlag, SockType, UnixAddr};
    let fd = socket::socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
        None,
    )?;
    socket::bind(fd.as_raw_fd(), &UnixAddr::new(path)?)?;
    socket::listen(&fd, Backlog::MAXCONN)?;
    UnixListener::from_std(std::os::unix::net::UnixListener::from(fd))
}

/// The directory holding `socket_path`. A bare file name has an empty
/// parent, which means the working directory, not a path to stat.
fn socket_directory(socket_path: &Path) -> &Path {
    match socket_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Checks that `directory` is safe to hold the socket. Returns the reason it
/// is not, or `None` when it is.
pub fn directory_complaint(directory: &Path) -> Option<String> {
    let shown = directory.display();
    let info = match std::fs::metadata(directory) {
        Ok(info) => info,
        Err(error) => return Some(format!("cannot stat {shown}: {}", errno_text(&error))),
    };
    if !info.is_dir() {
        return Some(format!("{shown} is not a directory"));
    }
    // Owned by root, or by whoever we are running as. Anything else means
    // another account controls the path the socket lives on.
    let euid = nix::unistd::geteuid().as_raw();
    if info.uid() != 0 && info.uid() != euid {
        return Some(format!(
            "{shown} is owned by uid {}, which is neither root nor this process",
            info.uid()
        ));
    }
    // The sticky bit does not save a writable directory: it stops unlinking
    // but not an attacker pre-binding the path before we do.
    if info.mode() & 0o022 != 0 {
        return Some(format!(
            "{shown} is writable by group or others, which would allow a local user to replace the socket"
        ));
    }
    None
}

async fn accept_loop(shared: Arc<Shared>, listener: UnixListener) {
    // A persistent accept failure (e.g. fd exhaustion) retries every 100 ms;
    // log the first and then only every hundredth.
    let mut failures: u64 = 0;
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                // EMFILE, ECONNABORTED, a transient ENFILE: none of them may
                // kill the loop, or the daemon silently stops serving.
                if failures.is_multiple_of(ACCEPT_FAILURE_LOG_INTERVAL) {
                    shared.log(
                        LogLevel::Warning,
                        &format!("accept failed ({} consecutive): {error}", failures + 1),
                    );
                }
                failures += 1;
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };
        failures = 0;

        // Counted here, and the guard created here, so the count is unwound
        // even if the session task is dropped before it first runs.
        let (admitted, max_connections) = {
            let mut state = shared.lock();
            let max_connections = state.config.max_connections;
            let admitted = if state.active >= max_connections {
                None
            } else {
                state.active += 1;
                let id = state.next_id;
                state.next_id += 1;
                Some(id)
            };
            (admitted, max_connections)
        };
        let Some(id) = admitted else {
            // Refuse politely rather than dropping silently, so the client
            // can say why. The cap exists because an open fd is a claim on a
            // GPU.
            shared.log_noise(
                None,
                Noise::ConnectionLimit,
                LogLevel::Warning,
                &format!("connection limit of {max_connections} reached"),
            );
            tokio::spawn(async move {
                let mut line = Framed::new(stream, FrameCodec);
                let _ = write(
                    &mut line,
                    denied(DenyReason::INTERNAL_ERROR, "connection limit reached"),
                )
                .await;
            });
            continue;
        };
        let guard = SessionGuard {
            shared: shared.clone(),
            id,
            uid: None,
            peer_text: "uid=? pid=?".to_owned(),
            identity: None,
        };
        tokio::spawn(serve(stream, guard));
    }
}

/// Grants and expiries on a clock, so a queued client with a timeout hears
/// back even when no other session is active.
async fn deadline_loop(shared: Arc<Shared>) {
    loop {
        let next = shared.lock().scheduler.next_deadline();
        match next {
            Some(deadline) => {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline.into()) => shared.pump(),
                    () = shared.rearm.notified() => {}
                }
            }
            None => shared.rearm.notified().await,
        }
    }
}

async fn serve(stream: UnixStream, mut guard: SessionGuard) {
    let shared = guard.shared.clone();
    // A panic is logged as "session failed", then the guard unwinds the
    // session as on any other exit.
    let result = AssertUnwindSafe(session(&shared, stream, &mut guard))
        .catch_unwind()
        .await;
    let peer = &guard.peer_text;
    let uid = guard.uid;
    let Ok(result) = result else {
        shared.log(LogLevel::Error, &format!("{peer} session failed: panic"));
        return;
    };
    match result {
        Ok(()) => {}
        Err(SessionError::Transport(error)) if is_hang_up(&error) => {}
        Err(SessionError::Transport(error)) => {
            shared.log_noise(
                uid,
                Noise::TransportError,
                LogLevel::Error,
                &format!("{peer} transport error: {error}"),
            );
        }
        Err(SessionError::HandshakeTimeout) => {
            shared.log_noise(
                uid,
                Noise::HandshakeTimeout,
                LogLevel::Warning,
                &format!("{peer} handshake timed out"),
            );
        }
        Err(SessionError::Protocol(error)) => {
            shared.log_noise(
                uid,
                Noise::ProtocolViolation,
                LogLevel::Error,
                &format!("{peer} protocol violation: {error}"),
            );
        }
    }
    // `guard` drops here: release, unwind counters, pump.
}

/// Whether a transport error only means the peer hung up, even mid-frame
/// or before reading a reply. That is how a session normally ends, not an
/// error worth logging.
fn is_hang_up(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
    )
}

async fn session(
    shared: &Arc<Shared>,
    stream: UnixStream,
    guard: &mut SessionGuard,
) -> Result<(), SessionError> {
    // Everything before `Acquire` is bounded, reads and writes alike: a
    // client that never asks for anything, or never reads its replies, is
    // parked state holding a connection slot.
    // Read once per connection: a reload changes it for later connections
    // (`handle_acquire` reads it again to bound an immediate denial).
    let timeout = shared.lock().config.handshake_timeout;
    let credentials = credentials_of(&stream);
    let mut line = Framed::new(stream, FrameCodec);
    let peer = match credentials {
        Ok(peer) => peer,
        Err(error) => {
            // Refusing an unmapped uid is deliberate: accepting it would
            // silently collapse every distinct user into one identity.
            shared.log_noise(
                None,
                Noise::RejectedPeer,
                LogLevel::Warning,
                &format!("rejected connection: {error}"),
            );
            return write_within(
                timeout,
                &mut line,
                denied(DenyReason::INVALID_REQUEST, error.to_string()),
            )
            .await;
        }
    };
    guard.peer_text = peer.to_string();

    // Counted before anything can suspend, and owned by the guard at once,
    // so an over-limit rejection unwinds the very increment it inspected.
    let over_limit = {
        let mut state = shared.lock();
        let cap = state.config.max_connections_per_uid;
        let count = state.per_uid.entry(peer.uid).or_default();
        *count += 1;
        *count > cap
    };
    guard.uid = Some(peer.uid);
    if over_limit {
        shared.log_noise(
            Some(peer.uid),
            Noise::UserConnectionLimit,
            LogLevel::Warning,
            &format!("{} over its per-user connection limit", guard.peer_text),
        );
        return write_within(
            timeout,
            &mut line,
            denied(
                DenyReason::INVALID_REQUEST,
                "too many concurrent connections for this user",
            ),
        )
        .await;
    }

    let observing = shared.lock().observation.enabled;
    if observing {
        let procfs = shared.procfs.clone();
        guard.identity = tokio::task::spawn_blocking(move || {
            u32::try_from(peer.pid)
                .ok()
                .filter(|&pid| pid > 1)
                .and_then(|pid| procfs.identity(pid))
        })
        .await
        .ok()
        .flatten();
    }

    // The handshake and the pre-acquire phase share a deadline per read: a
    // dashboard may poll `Status` on one connection indefinitely, but each
    // message must arrive in time.
    let Some(first) = read_within(&mut line, timeout).await? else {
        return Ok(());
    };
    let Message::Hello(hello) = first else {
        return Err(SessionError::Protocol(ProtocolError::InvalidMessage));
    };
    if hello.version != PROTOCOL_VERSION {
        // Logged before the reply, so a client that hangs up at once still
        // leaves the line.
        shared.log_noise(
            guard.uid,
            Noise::VersionMismatch,
            LogLevel::Warning,
            &format!(
                "{} version mismatch (client {})",
                guard.peer_text, hello.version
            ),
        );
        write_within(
            timeout,
            &mut line,
            denied(
                DenyReason::VERSION_MISMATCH,
                format!(
                    "daemon speaks protocol version {PROTOCOL_VERSION}, client speaks {}",
                    hello.version
                ),
            ),
        )
        .await?;
        return Ok(());
    }
    write_within(timeout, &mut line, HelloAck::default()).await?;

    loop {
        let Some(request) = read_within(&mut line, timeout).await? else {
            return Ok(());
        };
        match request {
            Message::Ping => write_within(timeout, &mut line, Message::Pong).await?,
            // Read-only and connection-scoped: a dashboard may poll on one
            // connection without ever acquiring anything.
            Message::Status => write_within(timeout, &mut line, snapshot(shared)).await?,
            Message::Acquire(acquire) => {
                return handle_acquire(
                    shared,
                    &mut line,
                    guard.id,
                    guard.identity,
                    &peer,
                    &acquire,
                )
                .await;
            }
            // Anything else here is a client bug or a probe.
            _ => return Err(SessionError::Protocol(ProtocolError::InvalidMessage)),
        }
    }
}

async fn handle_acquire(
    shared: &Arc<Shared>,
    line: &mut Line,
    id: RequestId,
    identity: Option<ProcessIdentity>,
    peer: &PeerCredentials,
    request: &Acquire,
) -> Result<(), SessionError> {
    let (timeout, observing) = {
        let state = shared.lock();
        (state.config.handshake_timeout, state.observation.enabled)
    };
    if observing && identity.is_none() {
        shared.log_noise(
            Some(peer.uid),
            Noise::IdentityUnavailable,
            LogLevel::Warning,
            &format!("{peer} peer process identity unavailable; GPU processes cannot be matched to this reservation (run a containerized gpudogd with --pid=host)"),
        );
    }
    let resolved = shared.lock().resolve(request);
    let Resolved {
        selection,
        gpus,
        candidates,
    } = match resolved {
        Ok(resolved) => resolved,
        Err((denial, warning)) => {
            match warning {
                Some(warning) => shared.log_noise(
                    Some(peer.uid),
                    Noise::UnmanagedDevice,
                    LogLevel::Warning,
                    &format!("{peer} {warning}"),
                ),
                None => shared.log(
                    LogLevel::Access,
                    &format!("{peer} denied: {}", String::from_utf8_lossy(&denial.detail)),
                ),
            }
            return write_within(timeout, line, denial).await;
        }
    };

    let now = Instant::now();
    let submission = ReservationRequest {
        candidates,
        id,
        uid: peer.uid,
        selection,
        num_gpus: request.num_gpus,
        gpus,
        // Bounded by `MAX_TIMEOUT_MS` on the wire, so this cannot overflow.
        deadline: (request.timeout_ms > 0).then(|| now + Duration::from_millis(request.timeout_ms)),
    };

    // Sanitised once, here, at the boundary: the access log and the
    // dashboard then handle display text, not arbitrary client bytes.
    let command = sanitize_display_text(&request.command);
    let command_note = if command.is_empty() {
        String::new()
    } else {
        format!(" command={command}")
    };
    shared.log(
        LogLevel::Access,
        &format!("{peer} acquire {}{command_note}", describe(request)),
    );

    // Submit and register the waiter under one lock, so no pump can run in
    // between and find nobody to wake.
    let (outcome, wakeup) = {
        let mut state = shared.lock();
        state.ensure_fresh(now);
        state.sessions.insert(
            id,
            SessionInfo {
                pid: peer.pid,
                command,
                identity,
            },
        );
        state.submit(&submission, now)
    };
    shared.rearm.notify_one();

    let outcome = match wakeup {
        None => outcome,
        Some(wakeup) => {
            write(line, to_message(outcome)).await?;
            // Whichever finishes first ends the other.
            tokio::select! {
                biased;
                delivered = wakeup => match delivered {
                    Ok(outcome) => outcome,
                    // The waiter was removed: the session is unwinding.
                    Err(_) => return Ok(()),
                },
                // The peer went away while queued; the guard releases.
                () = peer_watch(shared, line, peer.uid) => return Ok(()),
            }
        }
    };

    // The access line comes before the reply, so it is logged even if the
    // client hangs up at once and the write fails.
    match &outcome {
        Reply::Denied { detail, .. } => {
            shared.log(LogLevel::Access, &format!("{peer} denied: {detail}"));
        }
        Reply::Granted { gpus, uuids } => {
            shared.log(
                LogLevel::Access,
                &format!(
                    "{peer} granted gpus=[{}]",
                    gpus.iter()
                        .zip(uuids)
                        .map(|(number, uuid)| format!("{number}:{uuid}"))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            );
        }
        Reply::Queued { .. } => unreachable!("the scheduler never re-queues"),
    }
    let granted = matches!(outcome, Reply::Granted { .. });
    write(line, to_message(outcome)).await?;
    if granted {
        // Hold the reservation until the client's fd closes. The fd *is*
        // the lock.
        await_peer_close(line).await
    } else {
        Ok(())
    }
}

/// Reads until the peer goes away, answering pings. A client holding or
/// awaiting a reservation has nothing else to say.
async fn await_peer_close(line: &mut Line) -> Result<(), SessionError> {
    while let Some(message) = read(line).await? {
        match message {
            Message::Ping => write(line, Message::Pong).await?,
            _ => return Err(SessionError::Protocol(ProtocolError::InvalidMessage)),
        }
    }
    Ok(())
}

/// [`await_peer_close`] for a queued session: every ending is a normal
/// return, and garbage is still logged as a protocol violation.
async fn peer_watch(shared: &Shared, line: &mut Line, uid: u32) {
    if let Err(SessionError::Protocol(error)) = await_peer_close(line).await {
        shared.log_noise(
            Some(uid),
            Noise::ProtocolViolation,
            LogLevel::Error,
            &format!("protocol violation from a queued client: {error}"),
        );
    }
}

/// Who was seen using each busy slot, from the observed process rows.
/// Processes of unknown uid add no owner.
fn busy_owners(busy: &[Slot], rows: &[ObservedProcess]) -> Vec<BusyOwners> {
    busy.iter()
        .map(|&slot| {
            let on_slot = rows.iter().filter(|p| p.row.gpu == Some(slot.0));
            BusyOwners {
                slot,
                uids: on_slot.filter_map(|process| process.row.uid).collect(),
            }
        })
        .collect()
}

/// Builds the dashboard snapshot. Lists are truncated, with `truncated` set,
/// if they would not fit in one frame: answering partially beats not
/// answering.
fn snapshot(shared: &Shared) -> StatusReply {
    // The reserve covers the fixed fields and the length prefix.
    const FRAME_RESERVE: usize = 8192; // busy bitmap + observation metadata
    let now = Instant::now();
    let mut state = shared.lock();
    state.ensure_fresh(now);
    let mut budget = protocol::MAX_BODY_SIZE as usize - FRAME_RESERVE;
    // Everything below is in daemon GPU numbers. Every held slot is shown,
    // so `numbers` drops nothing from a reservation.
    let scheduler = &state.scheduler;
    let numbers = |slots: &[Slot]| -> Vec<u32> {
        slots
            .iter()
            .filter_map(|&slot| scheduler.display_of(slot))
            .collect()
    };
    // A retiring GPU is never allocated, so it reads as busy, whether its
    // holder still runs or has released it since the reload.
    let mut busy: Vec<Slot> = state.observation.busy.clone();
    busy.extend(
        scheduler
            .shown()
            .iter()
            .filter(|&&slot| scheduler.slot_state(slot) == Some(SlotState::Retiring)),
    );
    busy.sort_unstable();
    busy.dedup();
    let mut reply = StatusReply {
        device_count: scheduler.device_count(),
        observing: state.observation.enabled,
        watch_unmanaged_ms: state.config.watch_unmanaged_ms,
        allocation_paused: scheduler.allocation_paused(),
        observation_age_ms: state.observation.completed.map(|t| {
            u64::try_from(now.saturating_duration_since(t).as_millis()).unwrap_or(u64::MAX)
        }),
        observation_error: state.observation.error.as_bytes().to_vec(),
        busy_gpus: numbers(&busy),
        ..StatusReply::default()
    };

    let mut convert =
        |entry: &ReservationSnapshot, reply: &mut StatusReply, waiting: bool| -> bool {
            let out = if waiting {
                &reply.waiting
            } else {
                &reply.holders
            };
            if out.len() >= protocol::MAX_STATUS_ENTRIES as usize {
                reply.truncated = true;
                return false;
            }
            let session = state.sessions.get(&entry.id);
            let command = session.map(|s| s.command.clone()).unwrap_or_default();
            // Fixed fields, the device list, and the command; matches the
            // reservation encoding in `protocol`.
            let cost = 40 + entry.gpus.len() * 4 + command.len();
            if cost > budget {
                reply.truncated = true;
                return false;
            }
            budget -= cost;
            let info = ReservationInfo {
                id: entry.id,
                uid: entry.uid,
                pid: session.map_or(0, |s| s.pid),
                gpus: numbers(&entry.gpus),
                num_gpus: entry.num_gpus,
                age_ms: u64::try_from(now.saturating_duration_since(entry.since).as_millis())
                    .unwrap_or(u64::MAX),
                position: entry.position,
                command: command.into_bytes(),
            };
            if waiting {
                reply.waiting.push(info);
            } else {
                reply.holders.push(info);
            }
            true
        };

    // Reservations before devices. They share one budget, and on the large
    // machine where truncation bites, the queue is what someone opened the
    // dashboard to see.
    for entry in scheduler.held_reservations() {
        if !convert(&entry, &mut reply, false) {
            break;
        }
    }
    for entry in scheduler.queued_reservations() {
        if !convert(&entry, &mut reply, true) {
            break;
        }
    }
    for (&slot, number) in scheduler.shown().iter().zip(0u32..) {
        let Some(device) = state.inventory.devices().get(slot.0 as usize) else {
            continue;
        };
        if reply.devices.len() >= protocol::MAX_STATUS_ENTRIES as usize {
            reply.truncated = true;
            break;
        }
        // Each string is capped as `encode` requires, so one oversized NVML
        // name shortens a row instead of failing the whole reply.
        let bounded = |text: &str| {
            let mut bytes = text.as_bytes().to_vec();
            bytes.truncate(protocol::MAX_STRING_SIZE as usize);
            bytes
        };
        let (name, uuid) = (bounded(&device.name), bounded(&device.uuid));
        // Index plus two length-prefixed strings.
        let cost = 12 + name.len() + uuid.len();
        if cost > budget {
            reply.truncated = true;
            break;
        }
        budget -= cost;
        reply.devices.push(DeviceInfo {
            index: number,
            name,
            uuid,
        });
    }
    for process in &state.observation.rows {
        let row = &process.row;
        let Some(gpu) = row.gpu.and_then(|slot| scheduler.display_of(Slot(slot))) else {
            continue;
        };
        let command = row.command.clone().unwrap_or_default();
        let cost = 40 + command.len();
        if reply.processes.len() >= protocol::MAX_STATUS_ENTRIES as usize || cost > budget {
            reply.truncated = true;
            break;
        }
        budget -= cost;
        reply.processes.push(protocol::GpuProcessInfo {
            gpu,
            pid: row.pid,
            uid: row.uid,
            start_time: process.start_time,
            used_memory: row.used_memory,
            command,
            ownership: row.ownership,
        });
    }
    reply
}

#[cfg(test)]
mod tests;
