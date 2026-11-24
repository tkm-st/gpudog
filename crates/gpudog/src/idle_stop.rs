//! Waits for the daemon's `IdleStop` on the reservation's connection while
//! the command runs.
//!
//! The descriptor is shared with the command, which inherits it, so it is
//! read on a duplicate in a thread of its own with blocking reads. It must
//! stay blocking: the file status flags (`O_NONBLOCK`) belong to the open
//! socket, which the command shares. The thread is detached and never
//! joined: it ends with the process, which then closes its duplicate, so it
//! cannot keep `gpudog run` from exiting or its reservation from ending.

use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

use gpudog_core::protocol::{decode_body, decode_body_size, Message, LENGTH_PREFIX_SIZE};
use tokio::sync::oneshot;

/// Starts reading `reservation`. The receiver yields the idle time the
/// daemon reported, once; it is dropped unsent when the connection ends or
/// carries anything else, which this client never asked for.
pub(crate) fn watch(reservation: &OwnedFd) -> std::io::Result<oneshot::Receiver<u64>> {
    let stream = UnixStream::from(reservation.try_clone()?);
    let (sender, receiver) = oneshot::channel();
    std::thread::Builder::new()
        .name("idle-stop".into())
        .spawn(move || {
            if let Some(idle_ms) = next_idle_stop(stream) {
                let _ = sender.send(idle_ms);
            }
        })?;
    Ok(receiver)
}

/// Reads frames until the first `IdleStop`.
fn next_idle_stop(mut stream: impl Read) -> Option<u64> {
    loop {
        let mut prefix = [0; LENGTH_PREFIX_SIZE];
        stream.read_exact(&mut prefix).ok()?;
        let size = decode_body_size(&prefix).ok()? as usize;
        let mut body = vec![0; size];
        stream.read_exact(&mut body).ok()?;
        match decode_body(&body).ok()? {
            Message::IdleStop(stop) => return Some(stop.idle_ms),
            // An answer to a ping the command sent on its own descriptor.
            Message::Pong => {}
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpudog_core::protocol::{encode, IdleStop};

    fn frames(messages: &[Message]) -> Vec<u8> {
        messages.iter().flat_map(|m| encode(m).unwrap()).collect()
    }

    #[test]
    fn the_first_idle_stop_is_reported() {
        let bytes = frames(&[
            Message::Pong,
            IdleStop { idle_ms: 61_000 }.into(),
            IdleStop { idle_ms: 1 }.into(),
        ]);
        assert_eq!(next_idle_stop(&bytes[..]), Some(61_000));
    }

    #[test]
    fn anything_else_ends_the_wait() {
        assert_eq!(next_idle_stop(&[][..]), None);
        assert_eq!(next_idle_stop(&frames(&[Message::Status])[..]), None);
        let bytes = frames(&[IdleStop { idle_ms: 5 }.into()]);
        assert_eq!(next_idle_stop(&bytes[..bytes.len() - 1]), None);
    }
}
