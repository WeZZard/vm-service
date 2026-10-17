//! Guest session frames (`docs/arch/guest-session.md`, GS-4).
//!
//! A frame is a 20-byte header with its own CRC-32, a payload of at most
//! [`MAX_PAYLOAD`] bytes, and the payload's CRC-32. All integers are
//! big-endian. Any malformed frame is an error; the caller ends the session.

use std::io::{self, Read, Write};

use serde_json::Value;
use thiserror::Error;

use crate::crc::crc32;

/// The first two bytes of every frame.
pub const MAGIC: u16 = 0x5653;
/// The largest payload a frame may carry.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// Header bytes covered by the header checksum.
const HEADER: usize = 16;

/// The frame types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Agent → host on start: `{protocol, build, os}`.
    Hello,
    /// Either side: an 8-byte nonce to echo.
    Ping,
    /// The echo of a ping.
    Pong,
    /// Agent → host, no payload: sent at the interval the hello announces,
    /// so the host can tell a silent link from a slow command (GS-1).
    Alive,
    /// Host → agent: `{command, stdin, timeout_s}`, then data and end.
    Exec,
    /// Host → agent: `{path, name, mode}`, then data and end.
    Write,
    /// Host → agent: `{path}`; answered by data and end.
    Read,
    /// Raw bytes of a request's stream.
    Data,
    /// `{size, sha256}` of the data frames before it.
    End,
    /// Agent → host: raw bytes of a command's combined output.
    Output,
    /// Agent → host: `{rc, signal, timed_out, cancelled}`.
    Exit,
    /// Agent → host: `{size, sha256}` of a written file.
    Done,
    /// Host → agent: end a running exec.
    Cancel,
    /// Agent → host: `{code, message}`; ends the request.
    Error,
}

impl Kind {
    /// The type's byte on the wire.
    pub fn byte(self) -> u8 {
        match self {
            Kind::Hello => 0x01,
            Kind::Ping => 0x02,
            Kind::Pong => 0x03,
            Kind::Alive => 0x04,
            Kind::Exec => 0x10,
            Kind::Write => 0x20,
            Kind::Read => 0x30,
            Kind::Data => 0x40,
            Kind::End => 0x41,
            Kind::Output => 0x42,
            Kind::Exit => 0x50,
            Kind::Done => 0x51,
            Kind::Cancel => 0x60,
            Kind::Error => 0x7F,
        }
    }

    /// The type for a wire byte, if any.
    pub fn from_byte(byte: u8) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.byte() == byte)
    }

    /// Every frame type.
    pub const ALL: [Kind; 14] = [
        Kind::Hello,
        Kind::Ping,
        Kind::Pong,
        Kind::Alive,
        Kind::Exec,
        Kind::Write,
        Kind::Read,
        Kind::Data,
        Kind::End,
        Kind::Output,
        Kind::Exit,
        Kind::Done,
        Kind::Cancel,
        Kind::Error,
    ];
}

/// One frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The frame type.
    pub kind: Kind,
    /// The request the frame belongs to; 0 for the session itself.
    pub request: u64,
    /// JSON for control frames, raw bytes for data frames.
    pub payload: Vec<u8>,
}

/// A malformed frame or a failed stream.
#[derive(Debug, Error)]
pub enum FrameError {
    /// The first two bytes were not [`MAGIC`].
    #[error("bad frame magic {0:#06x}")]
    BadMagic(u16),
    /// The header failed its checksum.
    #[error("frame header checksum mismatch")]
    HeaderChecksum,
    /// The payload failed its checksum.
    #[error("frame payload checksum mismatch")]
    PayloadChecksum,
    /// The type byte names no frame type.
    #[error("unknown frame type {0:#04x}")]
    UnknownKind(u8),
    /// The reserved flags byte was not zero.
    #[error("nonzero frame flags {0:#04x}")]
    Flags(u8),
    /// The payload length exceeds [`MAX_PAYLOAD`].
    #[error("frame payload of {0} bytes exceeds the limit")]
    TooLarge(usize),
    /// The stream ended inside a frame.
    #[error("stream ended inside a frame")]
    Truncated,
    /// A control frame's payload was not the expected JSON.
    #[error("invalid frame payload: {0}")]
    Payload(String),
    /// Reading the stream failed.
    #[error("{0}")]
    Io(#[from] io::Error),
}

impl Frame {
    /// A frame with a raw payload.
    pub fn new(kind: Kind, request: u64, payload: impl Into<Vec<u8>>) -> Frame {
        Frame {
            kind,
            request,
            payload: payload.into(),
        }
    }

    /// A frame with a JSON payload.
    pub fn json(kind: Kind, request: u64, value: &Value) -> Frame {
        Frame::new(kind, request, value.to_string().into_bytes())
    }

    /// The payload parsed as a JSON object.
    pub fn value(&self) -> Result<Value, FrameError> {
        let value: Value = serde_json::from_slice(&self.payload)
            .map_err(|error| FrameError::Payload(error.to_string()))?;
        if value.is_object() {
            Ok(value)
        } else {
            Err(FrameError::Payload("expected a JSON object".to_string()))
        }
    }

    /// The frame's bytes on the wire.
    ///
    /// # Panics
    ///
    /// Panics if the payload exceeds [`MAX_PAYLOAD`]; senders chunk their data.
    pub fn encode(&self) -> Vec<u8> {
        assert!(self.payload.len() <= MAX_PAYLOAD, "frame payload too large");
        let mut bytes = Vec::with_capacity(HEADER + 8 + self.payload.len());
        bytes.extend_from_slice(&MAGIC.to_be_bytes());
        bytes.push(self.kind.byte());
        bytes.push(0);
        bytes.extend_from_slice(&self.request.to_be_bytes());
        bytes.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        let header = crc32(&bytes);
        bytes.extend_from_slice(&header.to_be_bytes());
        bytes.extend_from_slice(&self.payload);
        bytes.extend_from_slice(&crc32(&self.payload).to_be_bytes());
        bytes
    }
}

/// Write one frame and flush it.
pub fn write_frame(writer: &mut (impl Write + ?Sized), frame: &Frame) -> io::Result<()> {
    writer.write_all(&frame.encode())?;
    writer.flush()
}

/// Read one frame. `Ok(None)` means the stream ended cleanly between frames.
pub fn read_frame(reader: &mut (impl Read + ?Sized)) -> Result<Option<Frame>, FrameError> {
    let mut header = [0u8; HEADER + 4];
    let mut filled = 0;
    while filled < header.len() {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(FrameError::Truncated),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    let magic = u16::from_be_bytes([header[0], header[1]]);
    if magic != MAGIC {
        return Err(FrameError::BadMagic(magic));
    }
    let expected = u32::from_be_bytes(header[HEADER..].try_into().expect("4 bytes"));
    if crc32(&header[..HEADER]) != expected {
        return Err(FrameError::HeaderChecksum);
    }
    let kind = Kind::from_byte(header[2]).ok_or(FrameError::UnknownKind(header[2]))?;
    if header[3] != 0 {
        return Err(FrameError::Flags(header[3]));
    }
    let request = u64::from_be_bytes(header[4..12].try_into().expect("8 bytes"));
    let length = u32::from_be_bytes(header[12..16].try_into().expect("4 bytes")) as usize;
    if length > MAX_PAYLOAD {
        return Err(FrameError::TooLarge(length));
    }
    let mut payload = vec![0u8; length + 4];
    reader.read_exact(&mut payload).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(error)
        }
    })?;
    let checksum = u32::from_be_bytes(payload[length..].try_into().expect("4 bytes"));
    payload.truncate(length);
    if crc32(&payload) != checksum {
        return Err(FrameError::PayloadChecksum);
    }
    Ok(Some(Frame {
        kind,
        request,
        payload,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample(kind: Kind) -> Frame {
        Frame::new(kind, 0x0102_0304_0506_0708, b"{\"a\":1}\n\x00\xff".to_vec())
    }

    #[test]
    fn every_type_round_trips() {
        for kind in Kind::ALL {
            let frame = sample(kind);
            let decoded = read_frame(&mut Cursor::new(frame.encode()))
                .expect("decode")
                .expect("a frame");
            assert_eq!(decoded, frame);
            assert_eq!(Kind::from_byte(kind.byte()), Some(kind));
        }
    }

    #[test]
    fn frames_follow_each_other_and_a_clean_end_is_none() {
        let mut bytes = sample(Kind::Data).encode();
        bytes.extend(Frame::new(Kind::End, 2, Vec::new()).encode());
        let mut reader = Cursor::new(bytes);
        assert_eq!(read_frame(&mut reader).unwrap().unwrap().kind, Kind::Data);
        let end = read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(
            (end.kind, end.request, end.payload.len()),
            (Kind::End, 2, 0)
        );
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn the_largest_payload_round_trips() {
        let frame = Frame::new(Kind::Data, 1, vec![7u8; MAX_PAYLOAD]);
        let decoded = read_frame(&mut Cursor::new(frame.encode()))
            .unwrap()
            .unwrap();
        assert_eq!(decoded.payload.len(), MAX_PAYLOAD);
    }

    /// GS-4: a corrupted frame is never accepted, and a corrupted length never
    /// makes the reader wait for bytes that are not coming.
    #[test]
    fn every_single_bit_flip_is_rejected() {
        let encoded = sample(Kind::Output).encode();
        for bit in 0..encoded.len() * 8 {
            let mut corrupted = encoded.clone();
            corrupted[bit / 8] ^= 1 << (bit % 8);
            match read_frame(&mut Cursor::new(&corrupted)) {
                Err(FrameError::Truncated) => panic!("bit {bit}: reader waited for more bytes"),
                Err(_) => {}
                Ok(frame) => panic!("bit {bit}: accepted {frame:?}"),
            }
        }
    }

    #[test]
    fn a_stream_cut_inside_a_frame_is_truncated() {
        let encoded = sample(Kind::Data).encode();
        for cut in 1..encoded.len() {
            assert!(
                matches!(
                    read_frame(&mut Cursor::new(&encoded[..cut])),
                    Err(FrameError::Truncated)
                ),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn an_oversized_length_is_rejected_before_reading_the_payload() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC.to_be_bytes());
        bytes.extend_from_slice(&[Kind::Data.byte(), 0]);
        bytes.extend_from_slice(&1u64.to_be_bytes());
        bytes.extend_from_slice(&((MAX_PAYLOAD + 1) as u32).to_be_bytes());
        let header = crc32(&bytes);
        bytes.extend_from_slice(&header.to_be_bytes());
        assert!(matches!(
            read_frame(&mut Cursor::new(bytes)),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn unknown_types_and_flags_are_rejected() {
        for (kind, flags) in [(0x99u8, 0u8), (Kind::Data.byte(), 1)] {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&MAGIC.to_be_bytes());
            bytes.extend_from_slice(&[kind, flags]);
            bytes.extend_from_slice(&1u64.to_be_bytes());
            bytes.extend_from_slice(&0u32.to_be_bytes());
            let header = crc32(&bytes);
            bytes.extend_from_slice(&header.to_be_bytes());
            bytes.extend_from_slice(&crc32(b"").to_be_bytes());
            assert!(read_frame(&mut Cursor::new(bytes)).is_err());
        }
    }

    #[test]
    fn value_requires_a_json_object() {
        assert!(Frame::new(Kind::Exec, 1, b"[1]".to_vec()).value().is_err());
        assert!(Frame::new(Kind::Exec, 1, b"{".to_vec()).value().is_err());
        let frame = Frame::json(Kind::Exec, 1, &serde_json::json!({"command": "true"}));
        assert_eq!(frame.value().unwrap()["command"], "true");
    }
}
