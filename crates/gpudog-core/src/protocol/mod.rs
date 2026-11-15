//! Wire protocol spoken between the `gpudog` client and the `gpudogd` daemon
//! over the Unix domain socket described in `notes/ipc-design.md`.
//!
//! This module is free of I/O apart from [`codec`]: it defines the message
//! types and pure encode/decode functions over byte buffers.
//!
//! Frame layout, little-endian throughout so the format does not depend on
//! host byte order:
//!
//! ```text
//! [u32 body_size][u16 message_type][payload...]
//!                \_________ body_size bytes _________/
//! ```
//!
//! A client, for example in a container image, can run a different release
//! from the daemon, so every connection begins with a `Hello`/`HelloAck`
//! exchange carrying [`PROTOCOL_VERSION`], and mismatched peers refuse each
//! other. Any change to the layout of an existing
//! message, or to the numeric value of an existing discriminator, is a
//! breaking change and must bump [`PROTOCOL_VERSION`]. Appending a new
//! [`MessageType`] is backward compatible: older peers reject it as
//! [`ProtocolError::UnknownMessageType`].
//!
//! Wire strings are `Vec<u8>`, not `String`: the peer may send any bytes, and
//! the bounds below are byte counts. Use [`sanitize_display_text`] before
//! showing or logging one.

pub mod codec;

#[cfg(test)]
mod tests;

/// Incremented on any breaking change to the layout below.
///
/// v2 added `Acquire::command` and the `Status`/`StatusReply` pair that
/// `gpudog dash` reads.
///
/// v3 added `Granted::uuids`, and `Acquire::uuids` with
/// [`GpuSelection::ExplicitUuids`] and `Acquire::candidates`. Indices are
/// the daemon's *logical* numbering of the devices it manages; the UUIDs are what the client exports
/// as `CUDA_VISIBLE_DEVICES`. See `notes/device-identity.md`.
/// v4 requires UUID-only wire selection and non-empty count candidates.
/// The retained `gpus` field is reserved/empty in Acquire; numeric selection
/// is an internal parsed/scheduler representation, never accepted on the wire.
/// v5 appends daemon GPU process observation to `StatusReply` and requires
/// canonical bool/option discriminators (0 or 1).
/// v6 removes the client-chosen priority from `Acquire` and
/// `ReservationInfo`: the daemon orders its queue itself.
/// v7 adds `hold_limit_ms` to `Granted` and `ReservationInfo`: how long the
/// daemon's `max_hold_ms` lets the reservation be held.
pub const PROTOCOL_VERSION: u32 = 7;

/// Size of the little-endian length prefix that precedes every frame body.
pub const LENGTH_PREFIX_SIZE: usize = 4;

/// Hard cap on a frame body. The length prefix is attacker-controlled input to
/// a privileged daemon, so it is validated against this bound *before* any
/// buffer is sized from it.
pub const MAX_BODY_SIZE: u32 = 64 * 1024;

/// Upper bound on the number of GPU indices in one message, and an exclusive
/// upper bound on any single GPU index. Also bounds `Acquire::num_gpus`.
pub const MAX_GPU_COUNT: u32 = 1024;

/// Upper bound on any length-prefixed string on the wire.
pub const MAX_STRING_SIZE: u32 = 4096;

/// Upper bound on a GPU UUID on the wire. NVML's own buffer for one is 96
/// bytes (`NVML_DEVICE_UUID_V2_BUFFER_SIZE`, NUL included).
pub const MAX_UUID_SIZE: u32 = 96;

/// Upper bound on `Acquire::command`, which is attacker-controlled text that
/// the daemon logs and the dashboard renders.
pub const MAX_COMMAND_SIZE: u32 = 256;

/// Upper bound on device, reservation and process lists in a `StatusReply`.
pub const MAX_STATUS_ENTRIES: u32 = 256;

/// Upper bound on `Acquire::timeout_ms` (~49.7 days), so the daemon can add it
/// to a monotonic clock without overflow.
pub const MAX_TIMEOUT_MS: u64 = 0xFFFF_FFFF;

/// Upper bound on `Granted::hold_limit_ms` and `ReservationInfo::hold_limit_ms`,
/// for the same reason as [`MAX_TIMEOUT_MS`].
pub const MAX_HOLD_LIMIT_MS: u64 = MAX_TIMEOUT_MS;

/// Discriminator stored in the first two bytes of a frame body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum MessageType {
    Hello = 1,
    HelloAck = 2,
    Acquire = 3,
    Queued = 4,
    Granted = 5,
    Denied = 6,
    Ping = 7,
    Pong = 8,
    Status = 9,
    StatusReply = 10,
}

impl TryFrom<u16> for MessageType {
    type Error = ProtocolError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => Self::Hello,
            2 => Self::HelloAck,
            3 => Self::Acquire,
            4 => Self::Queued,
            5 => Self::Granted,
            6 => Self::Denied,
            7 => Self::Ping,
            8 => Self::Pong,
            9 => Self::Status,
            10 => Self::StatusReply,
            _ => return Err(ProtocolError::UnknownMessageType),
        })
    }
}

/// True if `value` names a [`MessageType`] this build knows.
pub fn is_known_message_type(value: u16) -> bool {
    MessageType::try_from(value).is_ok()
}

/// Why the daemon refused a reservation.
///
/// A newtype rather than an enum: a newer daemon may deny for a reason this
/// client predates, and decoding must accept it (`Denied::detail` still
/// explains it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DenyReason(pub u16);

impl DenyReason {
    pub const UNSPECIFIED: Self = Self(0);
    pub const VERSION_MISMATCH: Self = Self(1);
    pub const TIMED_OUT: Self = Self(2);
    pub const INVALID_REQUEST: Self = Self(3);
    pub const UNSATISFIABLE: Self = Self(4);
    pub const SHUTTING_DOWN: Self = Self(5);
    pub const INTERNAL_ERROR: Self = Self(6);
}

/// Why a buffer could not be encoded or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ProtocolError {
    /// Buffer ended before the field being read was complete.
    #[error("frame truncated")]
    Truncated,
    /// Declared body size exceeds [`MAX_BODY_SIZE`].
    #[error("frame exceeds the maximum body size")]
    FrameTooLarge,
    /// The message type discriminator is not one this build knows.
    #[error("unknown message type")]
    UnknownMessageType,
    /// Bytes remain after the message was fully decoded.
    #[error("trailing bytes after the message")]
    TrailingBytes,
    /// A length or count field exceeds its documented bound.
    #[error("value out of range")]
    ValueOutOfRange,
    /// Fields decoded individually but the combination is not meaningful.
    #[error("inconsistent message")]
    InvalidMessage,
}

/// How an `Acquire` request names the GPUs it wants. Mirrors the mutually
/// exclusive `-n/--num_gpus` and `-g/--gpus` options in `README.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum GpuSelection {
    /// Any `num_gpus` free devices; `gpus` must be empty.
    #[default]
    Count = 0,
    /// Local parsed/scheduler representation only; forbidden on the v4 wire.
    ExplicitList = 1,
    /// Exactly the devices listed in `uuids`, which must hold no duplicates
    /// (compared case-insensitively). The daemon resolves them.
    ExplicitUuids = 2,
}

/// First message on every connection, client to daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
}

impl Default for Hello {
    fn default() -> Self {
        Self {
            version: PROTOCOL_VERSION,
        }
    }
}

/// Daemon's reply to `Hello`. A version mismatch is reported as `Denied`
/// with [`DenyReason::VERSION_MISMATCH`] rather than as a `HelloAck`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloAck {
    pub version: u32,
}

impl Default for HelloAck {
    fn default() -> Self {
        Self {
            version: PROTOCOL_VERSION,
        }
    }
}

/// Request for a GPU reservation. The reservation lives exactly as long as the
/// connection that requested it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acquire {
    pub selection: GpuSelection,
    /// Devices wanted, always in `1..=MAX_GPU_COUNT`. For `ExplicitList` it
    /// must equal `gpus.len()`, for `ExplicitUuids` `uuids.len()`.
    pub num_gpus: u32,
    /// Devices wanted by index; empty unless `selection` is `ExplicitList`.
    pub gpus: Vec<u32>,
    /// Devices wanted by `GPU-<uuid>`; empty unless `selection` is
    /// `ExplicitUuids`. Validated as ASCII, so `String` is safe here.
    pub uuids: Vec<String>,
    /// For `Count` only: the non-empty UUID candidate set allowed by the
    /// client's CUDA environment. Unmanaged candidates are intersected out.
    pub candidates: Vec<String>,
    /// Milliseconds to wait for the devices; 0 means wait indefinitely.
    pub timeout_ms: u64,
    /// The command the client is about to run, for the dashboard and the
    /// access log. Advisory, unverified display text — never an
    /// authorisation input. May be empty.
    pub command: Vec<u8>,
}

impl Default for Acquire {
    fn default() -> Self {
        Self {
            selection: GpuSelection::Count,
            num_gpus: 1,
            gpus: Vec::new(),
            uuids: Vec::new(),
            candidates: Vec::new(),
            timeout_ms: 0,
            command: Vec::new(),
        }
    }
}

/// Sent at most once, when a request cannot be satisfied immediately. The
/// position is the one at submission and is not updated later.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Queued {
    pub position: u32,
}

/// The reservation is held. The client may now exec its child process.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Granted {
    /// Never empty, and never contains a duplicate. Logical indices, for
    /// display.
    pub gpus: Vec<u32>,
    /// `uuids[i]` names `gpus[i]`. These, not the indices, go into
    /// `CUDA_VISIBLE_DEVICES`.
    pub uuids: Vec<String>,
    /// Milliseconds the client may hold the reservation, counted from the
    /// grant; 0 means no limit. The client stops its command when it ends;
    /// the daemon never revokes the reservation.
    pub hold_limit_ms: u64,
}

/// Terminal failure for a request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Denied {
    pub reason: DenyReason,
    pub detail: Vec<u8>,
}

/// One GPU as the daemon sees it, for the dashboard's device table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceInfo {
    pub index: u32,
    pub name: Vec<u8>,
    pub uuid: Vec<u8>,
}

/// One reservation, held or waiting, as the dashboard shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReservationInfo {
    /// Opaque to clients; stable for the life of the reservation.
    pub id: u64,
    pub uid: u32,
    /// Peer pid from `SO_PEERCRED`. Shown so a human can find the process,
    /// nothing more.
    pub pid: i32,
    /// Devices held. Empty while the request is still waiting.
    pub gpus: Vec<u32>,
    /// Devices wanted, which is what `gpus` will become once granted.
    pub num_gpus: u32,
    /// Milliseconds since the reservation was granted or queued.
    pub age_ms: u64,
    /// `Granted::hold_limit_ms` given to this holder; 0 means no limit, and
    /// it is always 0 for a waiting request.
    pub hold_limit_ms: u64,
    /// Queue position; always 0 for a holder.
    pub position: u32,
    /// `Acquire::command`, echoed verbatim. Untrusted text.
    pub command: Vec<u8>,
}

/// How an observed GPU process relates to the daemon's reservations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ProcessOwnership {
    Managed = 0,
    WrongGpu = 1,
    NotManaged = 2,
    #[default]
    Unknown = 3,
}

impl ProcessOwnership {
    pub fn label(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::WrongGpu => "managed (wrong GPU)",
            Self::NotManaged => "not managed",
            Self::Unknown => "unknown",
        }
    }
}

/// One process observed on a daemon logical GPU. Optional identity fields
/// distinguish unavailable metadata from valid zero values.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GpuProcessInfo {
    pub gpu: u32,
    pub pid: u32,
    pub uid: Option<u32>,
    pub start_time: Option<u64>,
    pub used_memory: Option<u64>,
    /// Advisory, untrusted display text; never an authorisation input.
    pub command: Vec<u8>,
    pub ownership: ProcessOwnership,
}

/// Reply to `Status`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusReply {
    /// Devices the daemon arbitrates, even if `devices` was truncated.
    pub device_count: u32,
    pub devices: Vec<DeviceInfo>,
    pub holders: Vec<ReservationInfo>,
    pub waiting: Vec<ReservationInfo>,
    /// True when lists were shortened to fit [`MAX_BODY_SIZE`].
    pub truncated: bool,
    pub observing: bool,
    pub watch_unmanaged_ms: u64,
    pub allocation_paused: bool,
    pub observation_age_ms: Option<u64>,
    /// Untrusted display text, bounded by [`MAX_COMMAND_SIZE`].
    pub observation_error: Vec<u8>,
    pub busy_gpus: Vec<u32>,
    pub processes: Vec<GpuProcessInfo>,
}

/// Every message on the wire. `Ping`/`Pong`/`Status` carry no payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Hello(Hello),
    HelloAck(HelloAck),
    Acquire(Acquire),
    Queued(Queued),
    Granted(Granted),
    Denied(Denied),
    Ping,
    Pong,
    /// Read-only snapshot request, used by `gpudog dash`. Carries nothing on
    /// purpose: selective queries about other users would be a disclosure knob.
    Status,
    StatusReply(StatusReply),
}

macro_rules! impl_from_payload {
    ($($variant:ident),*) => {
        $(impl From<$variant> for Message {
            fn from(value: $variant) -> Self {
                Self::$variant(value)
            }
        })*
    };
}
impl_from_payload!(
    Hello,
    HelloAck,
    Acquire,
    Queued,
    Granted,
    Denied,
    StatusReply
);

impl Message {
    /// Discriminator that [`encode`] writes for this message.
    pub fn message_type(&self) -> MessageType {
        match self {
            Self::Hello(_) => MessageType::Hello,
            Self::HelloAck(_) => MessageType::HelloAck,
            Self::Acquire(_) => MessageType::Acquire,
            Self::Queued(_) => MessageType::Queued,
            Self::Granted(_) => MessageType::Granted,
            Self::Denied(_) => MessageType::Denied,
            Self::Ping => MessageType::Ping,
            Self::Pong => MessageType::Pong,
            Self::Status => MessageType::Status,
            Self::StatusReply(_) => MessageType::StatusReply,
        }
    }
}

/// Replaces every byte outside printable ASCII with `?`.
///
/// `Acquire::command` is attacker-controlled text that the daemon logs and
/// the dashboard renders. Control characters can forge log lines and corrupt
/// a terminal, so the bytes are reduced to a set that is safe everywhere.
/// This is about display safety only; it is never an authorisation decision.
pub fn sanitize_display_text(text: &[u8]) -> String {
    text.iter()
        .map(|&b| {
            if (0x20..=0x7E).contains(&b) {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

/// True for `GPU-<id>`: the prefix matched case-insensitively as CUDA does,
/// then one or more ASCII letters, digits and hyphens, [`MAX_UUID_SIZE`]
/// bytes at most. Deliberately narrow: a UUID is copied into a child's
/// environment and into log lines.
pub fn is_valid_gpu_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() > 4
        && !exceeds(bytes.len(), MAX_UUID_SIZE)
        && bytes[..4].eq_ignore_ascii_case(b"GPU-")
        && bytes[4..]
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || b == b'-')
}

/// True if `a` and `b` name the same device. CUDA matches UUIDs
/// case-insensitively, so this does too.
pub fn same_uuid(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

// ---------------------------------------------------------------------------
// Validation, shared by encode and decode so neither side can put an invalid
// message on the wire or accept one from it.

fn exceeds(len: usize, bound: u32) -> bool {
    len > bound as usize
}

/// Bounds every index and rejects duplicates. A repeated index would let one
/// request inflate its apparent device count, and on the release path could
/// free a device another user still holds.
fn validate_gpu_list(gpus: &[u32]) -> Result<(), ProtocolError> {
    if exceeds(gpus.len(), MAX_GPU_COUNT) {
        return Err(ProtocolError::ValueOutOfRange);
    }
    let mut seen = [false; MAX_GPU_COUNT as usize];
    for &index in gpus {
        let slot = seen
            .get_mut(index as usize)
            .ok_or(ProtocolError::ValueOutOfRange)?;
        if std::mem::replace(slot, true) {
            return Err(ProtocolError::InvalidMessage);
        }
    }
    Ok(())
}

/// Every entry well-formed and no device named twice. A repeated UUID is the
/// same hazard as a repeated index.
fn validate_uuid_list(uuids: &[String]) -> Result<(), ProtocolError> {
    if exceeds(uuids.len(), MAX_GPU_COUNT) {
        return Err(ProtocolError::ValueOutOfRange);
    }
    for (i, uuid) in uuids.iter().enumerate() {
        // Quadratic, but bounded by MAX_GPU_COUNT.
        if !is_valid_gpu_uuid(uuid) || uuids[..i].iter().any(|seen| same_uuid(seen, uuid)) {
            return Err(ProtocolError::InvalidMessage);
        }
    }
    Ok(())
}

fn validate_acquire(msg: &Acquire) -> Result<(), ProtocolError> {
    if exceeds(msg.command.len(), MAX_COMMAND_SIZE) {
        return Err(ProtocolError::ValueOutOfRange);
    }
    validate_gpu_list(&msg.gpus)?;
    validate_uuid_list(&msg.uuids)?;
    validate_uuid_list(&msg.candidates)?;
    // A candidate set only narrows "any N"; an explicit list already names
    // its devices.
    if !msg.candidates.is_empty() && msg.selection != GpuSelection::Count {
        return Err(ProtocolError::InvalidMessage);
    }
    // Bound the count itself, not just the list, so a well-formed frame
    // cannot hand the scheduler `num_gpus == u32::MAX`.
    if msg.num_gpus == 0 || msg.num_gpus > MAX_GPU_COUNT {
        return Err(ProtocolError::ValueOutOfRange);
    }
    if msg.timeout_ms > MAX_TIMEOUT_MS {
        return Err(ProtocolError::ValueOutOfRange);
    }
    let consistent = match msg.selection {
        GpuSelection::Count => {
            msg.gpus.is_empty() && msg.uuids.is_empty() && !msg.candidates.is_empty()
        }
        GpuSelection::ExplicitList => false,
        GpuSelection::ExplicitUuids => {
            !msg.uuids.is_empty() && msg.num_gpus as usize == msg.uuids.len() && msg.gpus.is_empty()
        }
    };
    if consistent {
        Ok(())
    } else {
        Err(ProtocolError::InvalidMessage)
    }
}

fn validate_granted(msg: &Granted) -> Result<(), ProtocolError> {
    if msg.gpus.is_empty() {
        return Err(ProtocolError::ValueOutOfRange);
    }
    if msg.uuids.len() != msg.gpus.len() {
        return Err(ProtocolError::InvalidMessage);
    }
    if msg.hold_limit_ms > MAX_HOLD_LIMIT_MS {
        return Err(ProtocolError::ValueOutOfRange);
    }
    validate_gpu_list(&msg.gpus)?;
    validate_uuid_list(&msg.uuids)
}

fn validate_denied(msg: &Denied) -> Result<(), ProtocolError> {
    if exceeds(msg.detail.len(), MAX_STRING_SIZE) {
        return Err(ProtocolError::ValueOutOfRange);
    }
    Ok(())
}

/// Also run on decode, so a hostile *daemon* cannot hand a client a row that
/// overruns the dashboard's limits.
fn validate_reservation(info: &ReservationInfo) -> Result<(), ProtocolError> {
    validate_gpu_list(&info.gpus)?;
    if info.num_gpus > MAX_GPU_COUNT
        || info.hold_limit_ms > MAX_HOLD_LIMIT_MS
        || exceeds(info.command.len(), MAX_COMMAND_SIZE)
    {
        return Err(ProtocolError::ValueOutOfRange);
    }
    Ok(())
}

fn validate_status_reply(msg: &StatusReply) -> Result<(), ProtocolError> {
    if [
        msg.devices.len(),
        msg.holders.len(),
        msg.waiting.len(),
        msg.processes.len(),
    ]
    .into_iter()
    .any(|len| exceeds(len, MAX_STATUS_ENTRIES))
    {
        return Err(ProtocolError::ValueOutOfRange);
    }
    if exceeds(msg.observation_error.len(), MAX_COMMAND_SIZE)
        || (msg.observing && !(1..=3_600_000).contains(&msg.watch_unmanaged_ms))
    {
        return Err(ProtocolError::ValueOutOfRange);
    }
    validate_gpu_list(&msg.busy_gpus)?;
    if msg.busy_gpus.iter().any(|&gpu| gpu >= msg.device_count) {
        return Err(ProtocolError::ValueOutOfRange);
    }
    for process in &msg.processes {
        if process.gpu >= MAX_GPU_COUNT
            || process.gpu >= msg.device_count
            || exceeds(process.command.len(), MAX_COMMAND_SIZE)
        {
            return Err(ProtocolError::ValueOutOfRange);
        }
    }
    for device in &msg.devices {
        if device.index >= MAX_GPU_COUNT
            || exceeds(device.name.len(), MAX_STRING_SIZE)
            || exceeds(device.uuid.len(), MAX_STRING_SIZE)
        {
            return Err(ProtocolError::ValueOutOfRange);
        }
    }
    msg.holders
        .iter()
        .chain(&msg.waiting)
        .try_for_each(validate_reservation)
}

// ---------------------------------------------------------------------------
// Encoding.

struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn i32(&mut self, value: i32) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn option_u32(&mut self, value: Option<u32>) {
        self.u8(u8::from(value.is_some()));
        if let Some(value) = value {
            self.u32(value);
        }
    }
    fn option_u64(&mut self, value: Option<u64>) {
        self.u8(u8::from(value.is_some()));
        if let Some(value) = value {
            self.u64(value);
        }
    }
    /// Lengths are validated against bounds far below `u32::MAX` before any
    /// write; the assert catches a future field added without a bound.
    fn len(&mut self, len: usize) {
        debug_assert!(u32::try_from(len).is_ok(), "unbounded wire length {len}");
        self.u32(len as u32);
    }
    fn bytes(&mut self, value: &[u8]) {
        self.len(value.len());
        self.0.extend_from_slice(value);
    }
    fn gpu_list(&mut self, gpus: &[u32]) {
        self.len(gpus.len());
        for &index in gpus {
            self.u32(index);
        }
    }
    fn uuid_list(&mut self, uuids: &[String]) {
        self.len(uuids.len());
        for uuid in uuids {
            self.bytes(uuid.as_bytes());
        }
    }
    fn reservation(&mut self, info: &ReservationInfo) {
        self.u64(info.id);
        self.u32(info.uid);
        self.i32(info.pid);
        self.gpu_list(&info.gpus);
        self.u32(info.num_gpus);
        self.u64(info.age_ms);
        self.u64(info.hold_limit_ms);
        self.u32(info.position);
        self.bytes(&info.command);
    }
}

/// Serialise `msg` into a complete frame, length prefix included.
///
/// Fails with `ValueOutOfRange` if a field exceeds its documented bound,
/// `InvalidMessage` if a message is internally inconsistent, or
/// `FrameTooLarge` if the body would exceed [`MAX_BODY_SIZE`].
pub fn encode(msg: &Message) -> Result<Vec<u8>, ProtocolError> {
    let mut w = Writer(vec![0; LENGTH_PREFIX_SIZE]);
    w.u16(msg.message_type() as u16);
    match msg {
        Message::Hello(Hello { version }) | Message::HelloAck(HelloAck { version }) => {
            w.u32(*version)
        }
        Message::Acquire(value) => {
            validate_acquire(value)?;
            w.u8(value.selection as u8);
            w.u32(value.num_gpus);
            w.gpu_list(&value.gpus);
            w.uuid_list(&value.uuids);
            w.uuid_list(&value.candidates);
            w.u64(value.timeout_ms);
            w.bytes(&value.command);
        }
        Message::Queued(value) => w.u32(value.position),
        Message::Granted(value) => {
            validate_granted(value)?;
            w.gpu_list(&value.gpus);
            w.uuid_list(&value.uuids);
            w.u64(value.hold_limit_ms);
        }
        Message::Denied(value) => {
            validate_denied(value)?;
            w.u16(value.reason.0);
            w.bytes(&value.detail);
        }
        Message::StatusReply(value) => {
            validate_status_reply(value)?;
            w.u32(value.device_count);
            w.len(value.devices.len());
            for device in &value.devices {
                w.u32(device.index);
                w.bytes(&device.name);
                w.bytes(&device.uuid);
            }
            w.len(value.holders.len());
            value.holders.iter().for_each(|info| w.reservation(info));
            w.len(value.waiting.len());
            value.waiting.iter().for_each(|info| w.reservation(info));
            w.u8(u8::from(value.truncated));
            w.u8(u8::from(value.observing));
            w.u64(value.watch_unmanaged_ms);
            w.u8(u8::from(value.allocation_paused));
            w.option_u64(value.observation_age_ms);
            w.bytes(&value.observation_error);
            w.gpu_list(&value.busy_gpus);
            w.len(value.processes.len());
            for process in &value.processes {
                w.u32(process.gpu);
                w.u32(process.pid);
                w.option_u32(process.uid);
                w.option_u64(process.start_time);
                w.option_u64(process.used_memory);
                w.u8(process.ownership as u8);
                w.bytes(&process.command);
            }
        }
        Message::Ping | Message::Pong | Message::Status => {}
    }

    let mut frame = w.0;
    let body_size = frame.len() - LENGTH_PREFIX_SIZE;
    if exceeds(body_size, MAX_BODY_SIZE) {
        return Err(ProtocolError::FrameTooLarge);
    }
    frame[..LENGTH_PREFIX_SIZE].copy_from_slice(&(body_size as u32).to_le_bytes());
    Ok(frame)
}

// ---------------------------------------------------------------------------
// Decoding.

/// Bounds-checked forward cursor over a frame body. Every count or length is
/// checked against its bound and against the remaining bytes *before* any
/// allocation is sized from it.
struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        if self.buf.len() < n {
            return Err(ProtocolError::Truncated);
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.array::<1>()?[0])
    }
    fn bool(&mut self) -> Result<bool, ProtocolError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProtocolError::InvalidMessage),
        }
    }
    fn u16(&mut self) -> Result<u16, ProtocolError> {
        self.array().map(u16::from_le_bytes)
    }
    fn u32(&mut self) -> Result<u32, ProtocolError> {
        self.array().map(u32::from_le_bytes)
    }
    fn i32(&mut self) -> Result<i32, ProtocolError> {
        self.array().map(i32::from_le_bytes)
    }
    fn u64(&mut self) -> Result<u64, ProtocolError> {
        self.array().map(u64::from_le_bytes)
    }
    fn option_u32(&mut self) -> Result<Option<u32>, ProtocolError> {
        if self.bool()? {
            self.u32().map(Some)
        } else {
            Ok(None)
        }
    }
    fn option_u64(&mut self) -> Result<Option<u64>, ProtocolError> {
        if self.bool()? {
            self.u64().map(Some)
        } else {
            Ok(None)
        }
    }

    /// Reads a `u32` count, refusing one above `bound` or one the remaining
    /// bytes cannot hold at `min_entry_bytes` per entry.
    fn count(&mut self, bound: u32, min_entry_bytes: usize) -> Result<usize, ProtocolError> {
        let count = self.u32()?;
        if count > bound {
            return Err(ProtocolError::ValueOutOfRange);
        }
        let count = count as usize;
        if self.buf.len() < count * min_entry_bytes {
            return Err(ProtocolError::Truncated);
        }
        Ok(count)
    }

    fn bytes(&mut self) -> Result<Vec<u8>, ProtocolError> {
        self.bounded_bytes(MAX_STRING_SIZE)
    }

    fn bounded_bytes(&mut self, bound: u32) -> Result<Vec<u8>, ProtocolError> {
        let size = self.count(bound, 1)?;
        Ok(self.take(size)?.to_vec())
    }

    fn gpu_list(&mut self) -> Result<Vec<u32>, ProtocolError> {
        let count = self.count(MAX_GPU_COUNT, 4)?;
        (0..count)
            .map(|_| match self.u32()? {
                index if index < MAX_GPU_COUNT => Ok(index),
                _ => Err(ProtocolError::ValueOutOfRange),
            })
            .collect()
    }

    /// Content is validated by the caller. Bytes that are not UTF-8 become
    /// U+FFFD, which validation then refuses as `InvalidMessage`.
    fn uuid_list(&mut self) -> Result<Vec<String>, ProtocolError> {
        // Each entry is at least its own length prefix.
        let count = self.count(MAX_GPU_COUNT, 4)?;
        (0..count)
            .map(|_| Ok(String::from_utf8_lossy(&self.bytes()?).into_owned()))
            .collect()
    }

    fn devices(&mut self) -> Result<Vec<DeviceInfo>, ProtocolError> {
        /// An index plus two empty strings.
        const MIN_DEVICE_BYTES: usize = 4 + 4 + 4;
        let count = self.count(MAX_STATUS_ENTRIES, MIN_DEVICE_BYTES)?;
        (0..count)
            .map(|_| {
                Ok(DeviceInfo {
                    index: self.u32()?,
                    name: self.bytes()?,
                    uuid: self.bytes()?,
                })
            })
            .collect()
    }

    fn reservations(&mut self) -> Result<Vec<ReservationInfo>, ProtocolError> {
        /// The fixed fields plus two empty length prefixes.
        const MIN_RESERVATION_BYTES: usize = 8 + 4 + 4 + 4 + 4 + 8 + 8 + 4 + 4;
        let count = self.count(MAX_STATUS_ENTRIES, MIN_RESERVATION_BYTES)?;
        (0..count)
            .map(|_| {
                Ok(ReservationInfo {
                    id: self.u64()?,
                    uid: self.u32()?,
                    pid: self.i32()?,
                    gpus: self.gpu_list()?,
                    num_gpus: self.u32()?,
                    age_ms: self.u64()?,
                    hold_limit_ms: self.u64()?,
                    position: self.u32()?,
                    command: self.bytes()?,
                })
            })
            .collect()
    }

    fn processes(&mut self) -> Result<Vec<GpuProcessInfo>, ProtocolError> {
        // GPU, PID, three absent options, ownership and an empty command.
        const MIN_PROCESS_BYTES: usize = 4 + 4 + 1 + 1 + 1 + 1 + 4;
        let count = self.count(MAX_STATUS_ENTRIES, MIN_PROCESS_BYTES)?;
        (0..count)
            .map(|_| {
                let gpu = self.u32()?;
                let pid = self.u32()?;
                let uid = self.option_u32()?;
                let start_time = self.option_u64()?;
                let used_memory = self.option_u64()?;
                let ownership = match self.u8()? {
                    0 => ProcessOwnership::Managed,
                    1 => ProcessOwnership::WrongGpu,
                    2 => ProcessOwnership::NotManaged,
                    3 => ProcessOwnership::Unknown,
                    _ => return Err(ProtocolError::InvalidMessage),
                };
                Ok(GpuProcessInfo {
                    gpu,
                    pid,
                    uid,
                    start_time,
                    used_memory,
                    ownership,
                    command: self.bounded_bytes(MAX_COMMAND_SIZE)?,
                })
            })
            .collect()
    }
}

/// Read the length prefix at the start of `buffer`; anything beyond the
/// prefix is ignored. Callers must apply this check before allocating a read
/// buffer for the body.
pub fn decode_body_size(buffer: &[u8]) -> Result<u32, ProtocolError> {
    let prefix: [u8; LENGTH_PREFIX_SIZE] = buffer
        .get(..LENGTH_PREFIX_SIZE)
        .and_then(|p| p.try_into().ok())
        .ok_or(ProtocolError::Truncated)?;
    let size = u32::from_le_bytes(prefix);
    if size > MAX_BODY_SIZE {
        return Err(ProtocolError::FrameTooLarge);
    }
    Ok(size)
}

/// Decode exactly one frame body: `buffer` must be the bytes *after* the
/// length prefix, and must contain no more than that one body.
pub fn decode_body(buffer: &[u8]) -> Result<Message, ProtocolError> {
    if exceeds(buffer.len(), MAX_BODY_SIZE) {
        return Err(ProtocolError::FrameTooLarge);
    }
    let mut r = Reader { buf: buffer };
    let message = match MessageType::try_from(r.u16()?)? {
        MessageType::Hello => Message::Hello(Hello { version: r.u32()? }),
        MessageType::HelloAck => Message::HelloAck(HelloAck { version: r.u32()? }),
        MessageType::Acquire => {
            let raw_selection = r.u8()?;
            let num_gpus = r.u32()?;
            let gpus = r.gpu_list()?;
            let uuids = r.uuid_list()?;
            let candidates = r.uuid_list()?;
            let timeout_ms = r.u64()?;
            let command = r.bytes()?;
            let selection = match raw_selection {
                0 => GpuSelection::Count,
                1 => GpuSelection::ExplicitList,
                2 => GpuSelection::ExplicitUuids,
                _ => return Err(ProtocolError::InvalidMessage),
            };
            let value = Acquire {
                selection,
                num_gpus,
                gpus,
                uuids,
                candidates,
                timeout_ms,
                command,
            };
            validate_acquire(&value)?;
            Message::Acquire(value)
        }
        MessageType::Queued => Message::Queued(Queued { position: r.u32()? }),
        MessageType::Granted => {
            let value = Granted {
                gpus: r.gpu_list()?,
                uuids: r.uuid_list()?,
                hold_limit_ms: r.u64()?,
            };
            validate_granted(&value)?;
            Message::Granted(value)
        }
        MessageType::Denied => Message::Denied(Denied {
            reason: DenyReason(r.u16()?),
            detail: r.bytes()?,
        }),
        MessageType::Ping => Message::Ping,
        MessageType::Pong => Message::Pong,
        MessageType::Status => Message::Status,
        MessageType::StatusReply => {
            let value = StatusReply {
                device_count: r.u32()?,
                devices: r.devices()?,
                holders: r.reservations()?,
                waiting: r.reservations()?,
                truncated: r.bool()?,
                observing: r.bool()?,
                watch_unmanaged_ms: r.u64()?,
                allocation_paused: r.bool()?,
                observation_age_ms: r.option_u64()?,
                observation_error: r.bounded_bytes(MAX_COMMAND_SIZE)?,
                busy_gpus: r.gpu_list()?,
                processes: r.processes()?,
            };
            validate_status_reply(&value)?;
            Message::StatusReply(value)
        }
    };
    if !r.buf.is_empty() {
        return Err(ProtocolError::TrailingBytes);
    }
    Ok(message)
}

/// Decode a complete frame, length prefix included.
///
/// Bytes *after* the declared body are ignored, so a caller may pass a stream
/// buffer holding more than one frame. This is deliberately laxer than
/// [`decode_body`], which rejects trailing bytes inside a single body.
pub fn decode_frame(buffer: &[u8]) -> Result<Message, ProtocolError> {
    let size = decode_body_size(buffer)? as usize;
    let body = buffer
        .get(LENGTH_PREFIX_SIZE..LENGTH_PREFIX_SIZE + size)
        .ok_or(ProtocolError::Truncated)?;
    decode_body(body)
}
