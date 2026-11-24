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

use crate::common::note;

/// Starts reading `reservation`. The receiver yields the idle time the
/// daemon reported, once; it is dropped unsent when the connection ends or
/// carries anything else, which this client never asked for.
pub(crate) fn watch(reservation: &OwnedFd) -> std::io::Result<oneshot::Receiver<u64>> {
    let stream = UnixStream::from(reservation.try_clone()?);
    let (sender, receiver) = oneshot::channel();
    std::thread::Builder::new()
        .name("idle-stop".into())
        .spawn(move || {
            match next_idle_stop(stream) {
                Ok(idle_ms) => {
                    let _ = sender.send(idle_ms);
                }
                // The daemon went away: the reservation, and so the limit,
                // ended with it.
                Err(Ending::Closed) => {}
                Err(Ending::Garbled) => note(
                    "unexpected data on the reservation's connection (does the command read it?); \
                     the idle limit is no longer watched",
                ),
            }
        })?;
    Ok(receiver)
}

/// Why [`next_idle_stop`] gave up.
#[derive(Debug, PartialEq, Eq)]
enum Ending {
    /// The connection ended between frames.
    Closed,
    /// A frame was cut short, malformed or not one the daemon sends.
    Garbled,
}

/// Reads frames until the first `IdleStop`.
fn next_idle_stop(mut stream: impl Read) -> Result<u64, Ending> {
    loop {
        let mut prefix = [0; LENGTH_PREFIX_SIZE];
        loop {
            match stream.read(&mut prefix[..1]) {
                Ok(0) => return Err(Ending::Closed),
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                // The command made the shared socket non-blocking.
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                // A reset or similar: the daemon has gone.
                Err(_) => return Err(Ending::Closed),
            }
        }
        read_all(&mut stream, &mut prefix[1..])?;
        let size = decode_body_size(&prefix).map_err(|_| Ending::Garbled)? as usize;
        let mut body = vec![0; size];
        read_all(&mut stream, &mut body)?;
        match decode_body(&body).map_err(|_| Ending::Garbled)? {
            Message::IdleStop(stop) => return Ok(stop.idle_ms),
            // An answer to a ping the command sent on its own descriptor.
            Message::Pong => {}
            _ => return Err(Ending::Garbled),
        }
    }
}

/// `read_exact` that also waits out a socket the command made
/// non-blocking. A frame cut short is [`Ending::Garbled`].
fn read_all(stream: &mut impl Read, mut buffer: &mut [u8]) -> Result<(), Ending> {
    while !buffer.is_empty() {
        match stream.read(buffer) {
            Ok(0) => return Err(Ending::Garbled),
            Ok(read) => buffer = &mut buffer[read..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return Err(Ending::Garbled),
        }
    }
    Ok(())
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
        assert_eq!(next_idle_stop(&bytes[..]), Ok(61_000));
    }

    #[test]
    fn a_closed_connection_and_garbage_end_the_wait_differently() {
        assert_eq!(next_idle_stop(&[][..]), Err(Ending::Closed));
        assert_eq!(
            next_idle_stop(&frames(&[Message::Pong])[..]),
            Err(Ending::Closed)
        );
        assert_eq!(
            next_idle_stop(&frames(&[Message::Status])[..]),
            Err(Ending::Garbled)
        );
        let bytes = frames(&[IdleStop { idle_ms: 5 }.into()]);
        assert_eq!(
            next_idle_stop(&bytes[..bytes.len() - 1]),
            Err(Ending::Garbled)
        );
        assert_eq!(next_idle_stop(&bytes[..2]), Err(Ending::Garbled));
    }
}
