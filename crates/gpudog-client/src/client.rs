//! Client side of the `gpudogd` protocol.
//!
//! [`Client::into_reservation`] is the handoff that makes "the fd is the
//! lock" work: it turns the connection into a plain descriptor, inheritable
//! across `exec`, so the child process holds the reservation for as long as
//! it runs, even if `gpudog` itself is killed.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use gpudog_core::device_visibility::{VisibilityError, VisibleDevices};
use gpudog_core::protocol::codec::{CodecError, FrameCodec};
use gpudog_core::protocol::{
    sanitize_display_text, Acquire, DenyReason, Granted, Hello, Message, StatusReply,
    PROTOCOL_VERSION,
};

/// The phrase for a deny reason, before the daemon's own detail.
pub fn describe(reason: DenyReason) -> &'static str {
    match reason {
        DenyReason::VERSION_MISMATCH => "protocol version mismatch",
        DenyReason::TIMED_OUT => "timed out waiting for GPUs",
        DenyReason::INVALID_REQUEST => "invalid request",
        DenyReason::UNSATISFIABLE => "request can never be satisfied",
        DenyReason::SHUTTING_DOWN => "the daemon is shutting down",
        DenyReason::INTERNAL_ERROR => "the daemon reported an internal error",
        // Unspecified, or a reason newer than this build: the detail still
        // explains it.
        _ => "request refused",
    }
}

/// How long the client waits for `gpudogd` to accept, to complete the
/// handshake, and to answer each request. The daemon answers at once, even
/// when it queues a request, so only a stuck daemon takes this long; waiting
/// in the queue afterwards is not bounded by it.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The socket could not be reached; usually the daemon is not running.
    #[error("{}", errno_text(.0))]
    Connect(io::Error),
    /// The daemon answered with `Denied`. `detail` is already sanitised: it
    /// comes from the daemon, and a terminal must not be handed raw bytes.
    #[error("{}{}", describe(*.reason), if .detail.is_empty() { String::new() } else { format!(": {}", .detail) })]
    Denied { reason: DenyReason, detail: String },
    /// The daemon sent something the client cannot make sense of.
    #[error("{0}")]
    Unexpected(&'static str),
    /// The daemon did not answer in time: it is running but stuck, or
    /// overloaded.
    #[error("gpudogd did not answer within {} seconds", .0.as_secs_f64())]
    Unresponsive(Duration),
    #[error("{0}")]
    Transport(#[from] CodecError),
    #[error("{0}")]
    Visibility(#[from] VisibilityError),
}

impl ClientError {
    /// True when the daemon is most likely simply not running.
    pub fn is_daemon_absent(&self) -> bool {
        matches!(self, Self::Connect(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused))
    }
}

fn errno_text(error: &io::Error) -> String {
    error.raw_os_error().map_or_else(
        || error.to_string(),
        |raw| Errno::from_raw(raw).desc().to_owned(),
    )
}

/// A connection to `gpudogd` that has completed the handshake.
pub struct Client {
    line: Framed<UnixStream, FrameCodec>,
    /// Bound on each request's first reply; see [`REPLY_TIMEOUT`].
    timeout: Duration,
    state: ClientState,
}

#[derive(Clone, Copy)]
enum ClientState {
    Ready,
    Holding,
    Closed,
}

struct Acquisition<'a> {
    client: &'a mut Client,
    granted: bool,
}

impl Drop for Acquisition<'_> {
    fn drop(&mut self) {
        if self.granted {
            self.client.state = ClientState::Holding;
        } else {
            // Future cancellation must cancel the request even when Client survives.
            self.client.state = ClientState::Closed;
            let _ = nix::sys::socket::shutdown(
                self.client.line.get_ref().as_raw_fd(),
                nix::sys::socket::Shutdown::Both,
            );
        }
    }
}

impl Client {
    /// Connects and performs the `Hello`/`HelloAck` exchange.
    pub async fn connect(socket_path: &Path) -> Result<Self, ClientError> {
        Self::connect_with_timeout(socket_path, REPLY_TIMEOUT).await
    }

    /// [`Client::connect`] with another bound than [`REPLY_TIMEOUT`] on the
    /// connection, the handshake and each request's first reply.
    pub async fn connect_with_timeout(
        socket_path: &Path,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        // One bound for connect and handshake: a full listen backlog makes
        // `connect` itself wait.
        tokio::time::timeout(timeout, async {
            let stream = UnixStream::connect(socket_path)
                .await
                .map_err(ClientError::Connect)?;
            let mut client = Self {
                line: Framed::new(stream, FrameCodec),
                timeout,
                state: ClientState::Ready,
            };
            client.line.send(Message::from(Hello::default())).await?;
            match client.recv().await? {
                Message::HelloAck(ack) if ack.version == PROTOCOL_VERSION => Ok(client),
                _ => Err(ClientError::Unexpected(
                    "daemon did not answer the handshake",
                )),
            }
        })
        .await
        .map_err(|_| ClientError::Unresponsive(timeout))?
    }

    /// The next message. A `Denied` is turned into an error here, since it
    /// is terminal whatever was asked.
    async fn recv(&mut self) -> Result<Message, ClientError> {
        match self.line.next().await {
            None => Err(ClientError::Unexpected("gpudogd closed the connection")),
            Some(Err(error)) => Err(error.into()),
            Some(Ok(Message::Denied(denied))) => Err(ClientError::Denied {
                reason: denied.reason,
                detail: sanitize_display_text(&denied.detail),
            }),
            Some(Ok(message)) => Ok(message),
        }
    }

    /// Sends `message` and returns the first reply, within `timeout`.
    /// Dropping the pending I/O on expiry leaves the codec mid-frame, which
    /// is fine: the error is terminal for this connection.
    async fn request(&mut self, message: Message) -> Result<Message, ClientError> {
        let timeout = self.timeout;
        tokio::time::timeout(timeout, async {
            self.line.send(message).await?;
            self.recv().await
        })
        .await
        .map_err(|_| ClientError::Unresponsive(timeout))?
    }

    /// Requests GPUs and waits until the daemon grants or refuses them,
    /// calling `on_queued` with each queue position it reports. Returns the
    /// grant: logical indices and the matching UUIDs.
    /// Cancelling this future after preparation closes the connection. A successful
    /// acquisition permits only `ping` or `into_reservation` on this client.
    /// The `on_queued` callback runs inline and must return promptly.
    pub async fn acquire(
        &mut self,
        request: &Acquire,
        on_queued: impl FnMut(u32),
    ) -> Result<Granted, ClientError> {
        self.ready()?;
        let visible = tokio::task::spawn_blocking(VisibleDevices::probe)
            .await
            .map_err(|_| VisibilityError("CUDA visibility worker failed".into()))??;
        self.acquire_with_visibility(request, &visible, on_queued)
            .await
    }

    /// Uses an explicit provider snapshot (also useful for tests). Parsed
    /// local indices are resolved before writing; returned grants are checked
    /// against the same view. Callers must supply their own actual environment.
    /// Cancellation or an acquisition error after preparation closes the connection.
    /// The daemon alone enforces `timeout_ms`, timed from when it receives the
    /// request; once queued, a daemon that stops answering is waited for indefinitely.
    pub async fn acquire_with_visibility(
        &mut self,
        request: &Acquire,
        visible: &VisibleDevices,
        on_queued: impl FnMut(u32),
    ) -> Result<Granted, ClientError> {
        self.ready()?;
        let prepared = visible.prepare_acquire(request)?;
        let mut acquisition = Acquisition {
            client: self,
            granted: false,
        };
        let outcome = acquisition
            .client
            .wait_for_grant(&prepared, visible, on_queued)
            .await;
        acquisition.granted = outcome.is_ok();
        outcome
    }

    fn ready(&self) -> Result<(), ClientError> {
        match self.state {
            ClientState::Ready => Ok(()),
            ClientState::Holding => Err(ClientError::Unexpected(
                "client already holds GPUs; use a separate connection",
            )),
            ClientState::Closed => Err(ClientError::Unexpected("client connection is closed")),
        }
    }

    async fn wait_for_grant(
        &mut self,
        prepared: &Acquire,
        visible: &VisibleDevices,
        mut on_queued: impl FnMut(u32),
    ) -> Result<Granted, ClientError> {
        let mut reply = self.request(prepared.clone().into()).await?;
        loop {
            match reply {
                Message::Queued(queued) => on_queued(queued.position),
                Message::Granted(granted) => {
                    visible.validate_grant(prepared, &granted)?;
                    return Ok(granted);
                }
                _ => {
                    return Err(ClientError::Unexpected(
                        "daemon sent an unexpected reply to acquire",
                    ))
                }
            }
            reply = self.recv().await?;
        }
    }

    /// Round-trips a `Ping`, for `gpudog health`.
    pub async fn ping(&mut self) -> Result<(), ClientError> {
        if matches!(self.state, ClientState::Closed) {
            return Err(ClientError::Unexpected("client connection is closed"));
        }
        match self.request(Message::Ping).await? {
            Message::Pong => Ok(()),
            _ => Err(ClientError::Unexpected("daemon did not answer a ping")),
        }
    }

    /// A read-only snapshot of the machine. Never acquires anything, so it
    /// may be polled repeatedly before acquisition. After a grant, this returns
    /// an error without releasing the reservation. Use another connection instead.
    pub async fn status(&mut self) -> Result<StatusReply, ClientError> {
        self.ready()?;
        match self.request(Message::Status).await? {
            Message::StatusReply(reply) => Ok(reply),
            _ => Err(ClientError::Unexpected(
                "daemon sent an unexpected reply to status",
            )),
        }
    }

    /// Turns the connection into a blocking descriptor with `FD_CLOEXEC`
    /// cleared, so it survives `exec` and the reservation lasts as long as
    /// *any* holder: this process or the child it runs. That is deliberate:
    /// releasing the GPU while the child still computes on it is the
    /// contamination this tool exists to prevent.
    ///
    /// Requires a successful acquisition and no pending I/O on this client.
    pub fn into_reservation(self) -> io::Result<OwnedFd> {
        if !matches!(self.state, ClientState::Holding) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "client has no GPU reservation",
            ));
        }
        let stream = self.line.into_inner().into_std()?;
        stream.set_nonblocking(false)?;
        let fd = OwnedFd::from(stream);
        // tokio opens sockets close-on-exec. A silent failure to clear it
        // would end the reservation the moment the child starts, so it is
        // reported rather than ignored.
        fcntl(fd.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::empty()))?;
        Ok(fd)
    }
}

#[cfg(test)]
mod tests;
