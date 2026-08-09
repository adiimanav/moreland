// SPDX-License-Identifier: Apache-2.0

//! Wire format between the host daemon and the Android app.
//!
//! One stream header at connect, then a frame header + Annex-B payload per
//! access unit:
//!
//! ```text
//! StreamHeader (16 bytes)
//!   0..4   magic  "MRLD"
//!   4..6   version          u16
//!   6..8   width            u16
//!   8..10  height           u16
//!  10..12  framerate        u16
//!     12   codec            u8
//!  13..16  reserved
//!
//! FrameHeader (16 bytes), repeated
//!   0..4   payload length   u32
//!   4..12  pts, nanoseconds u64
//!     12   flags            u8   bit 0 = keyframe
//!  13..16  reserved
//! ```
//!
//! Every multi-byte field is **big-endian**. That is deliberate: Kotlin's
//! `DataInputStream` reads big-endian natively, so the device side needs no
//! byte-swapping code at all.

use anyhow::{bail, Result};

pub const MAGIC: [u8; 4] = *b"MRLD";
pub const VERSION: u16 = 1;
pub const STREAM_HEADER_LEN: usize = 16;
pub const FRAME_HEADER_LEN: usize = 16;

/// Default host-side port for `adb forward`.
pub const DEFAULT_PORT: u16 = 27183;
/// Abstract Unix socket the device app listens on.
pub const SOCKET_NAME: &str = "moreland";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    H264 = 0,
    H265 = 1,
}

impl Codec {
    pub fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Codec::H264,
            1 => Codec::H265,
            other => bail!("unknown codec id {other}"),
        })
    }

    /// MIME type MediaCodec expects.
    pub fn mime(self) -> &'static str {
        match self {
            Codec::H264 => "video/avc",
            Codec::H265 => "video/hevc",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StreamHeader {
    pub width: u16,
    pub height: u16,
    pub framerate: u16,
    pub codec: Codec,
}

impl StreamHeader {
    pub fn encode(&self) -> [u8; STREAM_HEADER_LEN] {
        let mut buf = [0u8; STREAM_HEADER_LEN];
        buf[0..4].copy_from_slice(&MAGIC);
        buf[4..6].copy_from_slice(&VERSION.to_be_bytes());
        buf[6..8].copy_from_slice(&self.width.to_be_bytes());
        buf[8..10].copy_from_slice(&self.height.to_be_bytes());
        buf[10..12].copy_from_slice(&self.framerate.to_be_bytes());
        buf[12] = self.codec as u8;
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < STREAM_HEADER_LEN {
            bail!("stream header too short: {} bytes", buf.len());
        }
        if buf[0..4] != MAGIC {
            bail!("bad magic: {:02x?}", &buf[0..4]);
        }
        let version = u16::from_be_bytes([buf[4], buf[5]]);
        if version != VERSION {
            bail!("unsupported protocol version {version} (expected {VERSION})");
        }
        Ok(Self {
            width: u16::from_be_bytes([buf[6], buf[7]]),
            height: u16::from_be_bytes([buf[8], buf[9]]),
            framerate: u16::from_be_bytes([buf[10], buf[11]]),
            codec: Codec::from_u8(buf[12])?,
        })
    }
}

pub const FLAG_KEYFRAME: u8 = 1 << 0;

#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub length: u32,
    pub pts_ns: u64,
    pub keyframe: bool,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        buf[0..4].copy_from_slice(&self.length.to_be_bytes());
        buf[4..12].copy_from_slice(&self.pts_ns.to_be_bytes());
        buf[12] = if self.keyframe { FLAG_KEYFRAME } else { 0 };
        buf
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.len() < FRAME_HEADER_LEN {
            bail!("frame header too short: {} bytes", buf.len());
        }
        Ok(Self {
            length: u32::from_be_bytes(buf[0..4].try_into().unwrap()),
            pts_ns: u64::from_be_bytes(buf[4..12].try_into().unwrap()),
            keyframe: buf[12] & FLAG_KEYFRAME != 0,
        })
    }
}

/// Device → host acknowledgement, sent when a frame is handed to the display.
///
/// This exists purely to make one-way latency measurable. Host and tablet have
/// unrelated monotonic clocks, so timestamps cannot be compared directly; a
/// round trip can be. The host measures send→ack and subtracts a baseline ping
/// RTT to isolate transport plus decode.
pub const ACK_LEN: usize = 8;

pub fn encode_ack(pts_ns: u64) -> [u8; ACK_LEN] {
    pts_ns.to_be_bytes()
}

pub fn decode_ack(buf: &[u8]) -> Result<u64> {
    if buf.len() < ACK_LEN {
        bail!("ack too short: {} bytes", buf.len());
    }
    Ok(u64::from_be_bytes(buf[..ACK_LEN].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_header_round_trips() {
        let header = StreamHeader {
            width: 1920,
            height: 1200,
            framerate: 60,
            codec: Codec::H264,
        };
        let decoded = StreamHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.width, 1920);
        assert_eq!(decoded.height, 1200);
        assert_eq!(decoded.framerate, 60);
        assert_eq!(decoded.codec, Codec::H264);
    }

    #[test]
    fn frame_header_round_trips() {
        let header = FrameHeader {
            length: 32357,
            pts_ns: 1_666_666_600,
            keyframe: true,
        };
        let decoded = FrameHeader::decode(&header.encode()).unwrap();
        assert_eq!(decoded.length, 32357);
        assert_eq!(decoded.pts_ns, 1_666_666_600);
        assert!(decoded.keyframe);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = StreamHeader {
            width: 1,
            height: 1,
            framerate: 60,
            codec: Codec::H264,
        }
        .encode();
        buf[0] = b'X';
        assert!(StreamHeader::decode(&buf).is_err());
    }
}
