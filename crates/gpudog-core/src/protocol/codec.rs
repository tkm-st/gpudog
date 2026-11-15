//! Framed async I/O for [`Message`] via `tokio_util::codec`.
//!
//! Use with `FramedRead`/`FramedWrite`/`Framed` over a `UnixStream`. Read
//! deadlines are the caller's job (`tokio::time::timeout`); dropping the
//! pending read future cancels it cleanly.

use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use super::{decode_body, decode_body_size, encode, Message, ProtocolError, LENGTH_PREFIX_SIZE};

/// Failure while reading or writing a frame.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Stateless codec for protocol frames.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = Message;
    type Error = CodecError;

    /// A stream that ends partway through a frame is an unexpected EOF, not a
    /// protocol violation: a client killed mid-write is not misbehaving.
    fn decode_eof(&mut self, src: &mut BytesMut) -> Result<Option<Message>, CodecError> {
        match self.decode(src)? {
            None if !src.is_empty() => {
                Err(CodecError::Io(std::io::ErrorKind::UnexpectedEof.into()))
            }
            decoded => Ok(decoded),
        }
    }

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>, CodecError> {
        if src.len() < LENGTH_PREFIX_SIZE {
            return Ok(None);
        }
        // Validated before `reserve`, so a bogus prefix cannot drive an
        // allocation.
        let frame_size = LENGTH_PREFIX_SIZE + decode_body_size(src)? as usize;
        if src.len() < frame_size {
            src.reserve(frame_size - src.len());
            return Ok(None);
        }
        src.advance(LENGTH_PREFIX_SIZE);
        let body = src.split_to(frame_size - LENGTH_PREFIX_SIZE);
        Ok(Some(decode_body(&body)?))
    }
}

impl Encoder<&Message> for FrameCodec {
    type Error = CodecError;

    fn encode(&mut self, item: &Message, dst: &mut BytesMut) -> Result<(), CodecError> {
        // An invalid message fails here rather than on the wire.
        dst.put_slice(&encode(item)?);
        Ok(())
    }
}

impl Encoder<Message> for FrameCodec {
    type Error = CodecError;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), CodecError> {
        Encoder::<&Message>::encode(self, &item, dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Granted, Queued, MAX_BODY_SIZE};

    fn granted(gpus: &[u32]) -> Granted {
        Granted {
            gpus: gpus.to_vec(),
            uuids: gpus.iter().map(|i| format!("GPU-static-{i}")).collect(),
            hold_limit_ms: 0,
        }
    }

    fn frame(msg: impl Into<Message>) -> Vec<u8> {
        encode(&msg.into()).unwrap()
    }

    #[test]
    fn waits_for_a_complete_frame_fed_one_byte_at_a_time() {
        let bytes = frame(granted(&[1, 2]));
        let mut codec = FrameCodec;
        let mut buf = BytesMut::new();
        for (i, &b) in bytes.iter().enumerate() {
            buf.put_u8(b);
            let decoded = codec.decode(&mut buf).unwrap();
            if i + 1 < bytes.len() {
                assert!(decoded.is_none(), "decoded early at byte {i}");
            } else {
                assert_eq!(decoded, Some(granted(&[1, 2]).into()));
            }
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn decodes_back_to_back_frames_from_one_buffer() {
        let mut buf = BytesMut::new();
        buf.put_slice(&frame(Queued { position: 1 }));
        buf.put_slice(&frame(Message::Pong));
        let mut codec = FrameCodec;
        assert_eq!(
            codec.decode(&mut buf).unwrap(),
            Some(Queued { position: 1 }.into())
        );
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Message::Pong));
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn refuses_an_oversized_prefix_without_reserving() {
        let mut buf = BytesMut::from(&(MAX_BODY_SIZE + 1).to_le_bytes()[..]);
        let capacity = buf.capacity();
        let err = FrameCodec.decode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            CodecError::Protocol(ProtocolError::FrameTooLarge)
        ));
        assert_eq!(buf.capacity(), capacity);
    }

    #[test]
    fn surfaces_a_malformed_body() {
        let mut buf = BytesMut::from(&[2, 0, 0, 0, 0xEE, 0][..]);
        let err = FrameCodec.decode(&mut buf).unwrap_err();
        assert!(matches!(
            err,
            CodecError::Protocol(ProtocolError::UnknownMessageType)
        ));
    }

    #[test]
    fn a_stream_ending_mid_frame_is_an_unexpected_eof() {
        let bytes = frame(granted(&[1]));
        let mut buf = BytesMut::from(&bytes[..bytes.len() - 1]);
        let err = FrameCodec.decode_eof(&mut buf).unwrap_err();
        assert!(matches!(err, CodecError::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof));
        assert_eq!(FrameCodec.decode_eof(&mut BytesMut::new()).unwrap(), None);
    }

    #[test]
    fn encoder_refuses_an_invalid_message() {
        let mut buf = BytesMut::new();
        let err = FrameCodec
            .encode(&Message::from(Granted::default()), &mut buf)
            .unwrap_err();
        assert!(matches!(
            err,
            CodecError::Protocol(ProtocolError::ValueOutOfRange)
        ));
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn round_trips_over_a_unix_socket_pair() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut a, mut b) = tokio::net::UnixStream::pair().unwrap();
        let mut out = BytesMut::new();
        FrameCodec.encode(Message::Ping, &mut out).unwrap();
        a.write_all(&out).await.unwrap();
        drop(a);

        let mut buf = BytesMut::new();
        b.read_buf(&mut buf).await.unwrap();
        assert_eq!(FrameCodec.decode(&mut buf).unwrap(), Some(Message::Ping));
    }
}
