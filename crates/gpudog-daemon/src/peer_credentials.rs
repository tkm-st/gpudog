//! Kernel-supplied identity of the process on the other end of a Unix socket.
//!
//! This is the whole authentication story for `gpudogd`. `SO_PEERCRED` is
//! filled in by the kernel at `connect(2)` time from the peer's real
//! credentials; a client cannot forge it, which is why the socket carries no
//! tokens and why its file mode is not the access-control mechanism. See
//! `notes/ipc-design.md`.

use std::fmt;
use std::os::fd::AsFd;
use std::sync::OnceLock;

use nix::sys::socket::{getsockopt, sockopt};

/// Identity of a connected peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    pub uid: u32,
    pub gid: u32,
    /// The peer's PID in the receiver's namespace, possibly 0 when it is not
    /// visible there. Opaque display metadata, not a host PID guarantee:
    /// never signal it or resolve it through an unrelated procfs namespace.
    pub pid: i32,
}

impl fmt::Display for PeerCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "uid={} gid={} pid={}", self.uid, self.gid, self.pid)
    }
}

/// Why peer credentials were refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CredentialsError {
    /// `getsockopt(SO_PEERCRED)` itself failed.
    #[error("peer credentials unavailable")]
    Unavailable,
    /// The uid is the kernel overflow uid, or `(uid_t)-1`.
    ///
    /// User-namespace remapping produces the overflow uid for a host uid with
    /// no mapping into the daemon's namespace. Accepting it would silently
    /// collapse every distinct human into one identity, destroying per-user
    /// fairness and isolation.
    #[error("peer uid is unmapped in this user namespace")]
    UnmappedUser,
}

const DEFAULT_OVERFLOW_UID: u32 = 65534;

/// `SO_PEERCRED` reports `(uid_t)-1` when the socket has no peer. A sentinel
/// bit pattern, so it is compared as `u32`.
const NO_PEER_UID: u32 = u32::MAX;

/// The kernel overflow uid, used when an id cannot be mapped into the
/// caller's user namespace. Read once from `/proc/sys/kernel/overflowuid`,
/// falling back to the conventional 65534.
pub fn overflow_uid() -> u32 {
    static VALUE: OnceLock<u32> = OnceLock::new();
    *VALUE.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/kernel/overflowuid")
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(DEFAULT_OVERFLOW_UID)
    })
}

/// Reads `SO_PEERCRED` from a connected Unix domain socket (a tokio or std
/// `UnixStream`, or any fd).
pub fn credentials_of(socket: impl AsFd) -> Result<PeerCredentials, CredentialsError> {
    let raw =
        getsockopt(&socket, sockopt::PeerCredentials).map_err(|_| CredentialsError::Unavailable)?;
    // An unmapped id in a user namespace surfaces as the overflow uid rather
    // than as an error, so this check is the only thing standing between
    // "every user is distinct" and "everyone is nobody".
    if raw.uid() == NO_PEER_UID || raw.uid() == overflow_uid() {
        return Err(CredentialsError::UnmappedUser);
    }
    Ok(PeerCredentials {
        uid: raw.uid(),
        gid: raw.gid(),
        pid: raw.pid(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::{socket, AddressFamily, SockFlag, SockType};
    use nix::unistd::{getgid, getuid};

    #[test]
    fn report_this_process_on_a_socket_pair() {
        let (first, _second) = std::os::unix::net::UnixStream::pair().unwrap();
        let credentials = credentials_of(&first).unwrap();
        assert_eq!(credentials.uid, getuid().as_raw());
        assert_eq!(credentials.gid, getgid().as_raw());
        // Both ends are this process, so the peer pid is our own.
        assert_eq!(credentials.pid as u32, std::process::id());
    }

    #[tokio::test]
    async fn work_on_a_tokio_socket() {
        let (first, _second) = tokio::net::UnixStream::pair().unwrap();
        assert_eq!(credentials_of(&first).unwrap().uid, getuid().as_raw());
    }

    #[test]
    fn are_unavailable_on_a_non_socket_descriptor() {
        let file = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(credentials_of(&file), Err(CredentialsError::Unavailable));
    }

    // A closed descriptor (-1) needs no test: `AsFd` cannot represent -1,
    // so the type system rules it out.

    #[test]
    fn reject_an_unconnected_socket() {
        // No peer: SO_PEERCRED yields uid/gid of -1, which must not be
        // mistaken for a real identity.
        let fd = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::empty(),
            None,
        )
        .unwrap();
        assert_eq!(credentials_of(&fd), Err(CredentialsError::UnmappedUser));
    }

    #[test]
    fn overflow_uid_is_read_from_the_kernel_and_is_not_a_real_user() {
        let value = overflow_uid();
        assert_ne!(value, getuid().as_raw());
        assert!(value > 0);
    }

    #[test]
    fn render_for_logging() {
        assert!(CredentialsError::UnmappedUser
            .to_string()
            .contains("unmapped"));
        let credentials = PeerCredentials {
            uid: 1000,
            gid: 1001,
            pid: 42,
        };
        assert_eq!(credentials.to_string(), "uid=1000 gid=1001 pid=42");
    }
}
