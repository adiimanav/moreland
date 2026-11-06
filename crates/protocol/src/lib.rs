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
//! Device → host traffic is a sequence of messages, each a type byte followed
//! by a fixed-length payload:
//!
//! ```text
//! 0x01  ack      8 bytes   pts u64, nanoseconds
//! 0x02  pointer  28 bytes  see [`PointerEvent`]
//! 0x03  timing   24 bytes  see [`Timing`]
//! ```
//!
//! Every multi-byte field is **big-endian**. That is deliberate: Kotlin's
//! `DataInputStream` reads big-endian natively, so the device side needs no
//! byte-swapping code at all.

use anyhow::{bail, Result};

pub const MAGIC: [u8; 4] = *b"MRLD";
pub const VERSION: u16 = 3;
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

/// Device → host message types.
pub const MSG_ACK: u8 = 0x01;
pub const MSG_POINTER: u8 = 0x02;
pub const MSG_TIMING: u8 = 0x03;

/// Device → host acknowledgement, sent when a frame is handed to the display.
///
/// This exists purely to make one-way latency measurable. Host and tablet have
/// unrelated monotonic clocks, so timestamps cannot be compared directly; a
/// round trip can be. The host measures send→ack and subtracts a baseline ping
/// RTT to isolate transport plus decode.
pub const ACK_LEN: usize = 8;

pub fn encode_ack(pts_ns: u64) -> [u8; 1 + ACK_LEN] {
    let mut buf = [0u8; 1 + ACK_LEN];
    buf[0] = MSG_ACK;
    buf[1..].copy_from_slice(&pts_ns.to_be_bytes());
    buf
}

/// What a pointer did, mirroring Android's `MotionEvent` actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Action {
    Down = 0,
    Up = 1,
    Move = 2,
    /// The gesture was taken away, e.g. by palm rejection. Not a lift.
    Cancel = 3,
    /// A pen moving above the glass without touching it.
    HoverMove = 4,
    /// The pen left detection range.
    HoverExit = 5,
}

impl Action {
    fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Action::Down,
            1 => Action::Up,
            2 => Action::Move,
            3 => Action::Cancel,
            4 => Action::HoverMove,
            5 => Action::HoverExit,
            other => bail!("unknown pointer action {other}"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Tool {
    Finger = 0,
    Pen = 1,
    /// The pen's eraser end.
    Eraser = 2,
}

impl Tool {
    fn from_u8(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Tool::Finger,
            1 => Tool::Pen,
            2 => Tool::Eraser,
            other => bail!("unknown pointer tool {other}"),
        })
    }
}

/// Pen button bits in [`PointerEvent::buttons`].
pub const BUTTON_STYLUS_PRIMARY: u8 = 1 << 0;
pub const BUTTON_STYLUS_SECONDARY: u8 = 1 << 1;

/// Set on the last pointer of one `MotionEvent`. The host applies a whole
/// event's pointers together, so multitouch contacts update in one frame.
pub const POINTER_FLAG_LAST: u8 = 1 << 0;

pub const POINTER_LEN: usize = 28;

/// One pointer of one touch or pen event.
///
/// Positions are fractions of the streamed surface, 0.0 to 1.0 from the top
/// left, so the host needs no knowledge of the tablet's pixel size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerEvent {
    pub action: Action,
    pub tool: Tool,
    /// Android's pointer id; stable for the life of one contact.
    pub pointer_id: u8,
    pub buttons: u8,
    pub flags: u8,
    pub x: f32,
    pub y: f32,
    /// 0.0 to 1.0.
    pub pressure: f32,
    /// Angle from perpendicular, radians, 0 to pi/2.
    pub tilt: f32,
    /// Direction of the tilt, radians, -pi to pi.
    pub orientation: f32,
}

impl PointerEvent {
    pub fn is_last(&self) -> bool {
        self.flags & POINTER_FLAG_LAST != 0
    }

    pub fn encode(&self) -> [u8; 1 + POINTER_LEN] {
        let mut buf = [0u8; 1 + POINTER_LEN];
        buf[0] = MSG_POINTER;
        buf[1] = self.action as u8;
        buf[2] = self.tool as u8;
        buf[3] = self.pointer_id;
        buf[4] = self.buttons;
        buf[5] = self.flags;
        // 6..9 reserved
        buf[9..13].copy_from_slice(&self.x.to_be_bytes());
        buf[13..17].copy_from_slice(&self.y.to_be_bytes());
        buf[17..21].copy_from_slice(&self.pressure.to_be_bytes());
        buf[21..25].copy_from_slice(&self.tilt.to_be_bytes());
        buf[25..29].copy_from_slice(&self.orientation.to_be_bytes());
        buf
    }

    fn decode(payload: &[u8]) -> Result<Self> {
        let f32_at = |at: usize| f32::from_be_bytes(payload[at..at + 4].try_into().unwrap());
        Ok(Self {
            action: Action::from_u8(payload[0])?,
            tool: Tool::from_u8(payload[1])?,
            pointer_id: payload[2],
            buttons: payload[3],
            flags: payload[4],
            x: f32_at(8),
            y: f32_at(12),
            pressure: f32_at(16),
            tilt: f32_at(20),
            orientation: f32_at(24),
        })
    }
}

pub const TIMING_LEN: usize = 24;

/// Where one frame spent its time on the tablet, in microseconds. All four
/// figures are differences between readings of the tablet's own clock, so they
/// mean something even though host and tablet clocks are unrelated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Identifies the frame: the presentation timestamp it was sent with,
    /// truncated to microseconds the way the decoder reports it.
    pub pts_us: u64,
    /// From the whole frame arriving to the decoder accepting it. Non-zero
    /// when the decoder had no free input buffer.
    pub queue_wait_us: u32,
    /// From the decoder accepting the frame to the picture being ready.
    pub decode_us: u32,
    /// From the picture being ready to it actually reaching the screen, which
    /// the display only does on a refresh boundary.
    pub display_wait_us: u32,
    /// Time since the previous frame reached the screen; 0 for the first. The
    /// direct measure of stutter: an even stream has near-constant values.
    pub render_interval_us: u32,
}

impl Timing {
    fn encode(&self) -> [u8; 1 + TIMING_LEN] {
        let mut buf = [0u8; 1 + TIMING_LEN];
        buf[0] = MSG_TIMING;
        buf[1..9].copy_from_slice(&self.pts_us.to_be_bytes());
        buf[9..13].copy_from_slice(&self.queue_wait_us.to_be_bytes());
        buf[13..17].copy_from_slice(&self.decode_us.to_be_bytes());
        buf[17..21].copy_from_slice(&self.display_wait_us.to_be_bytes());
        buf[21..25].copy_from_slice(&self.render_interval_us.to_be_bytes());
        buf
    }

    fn decode(payload: &[u8]) -> Self {
        let u32_at = |at: usize| u32::from_be_bytes(payload[at..at + 4].try_into().unwrap());
        Self {
            pts_us: u64::from_be_bytes(payload[0..8].try_into().unwrap()),
            queue_wait_us: u32_at(8),
            decode_us: u32_at(12),
            display_wait_us: u32_at(16),
            render_interval_us: u32_at(20),
        }
    }
}

/// Wire encoding of a [`Timing`] message, for tests and tools.
pub fn encode_timing(timing: &Timing) -> [u8; 1 + TIMING_LEN] {
    timing.encode()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeviceMessage {
    Ack { pts_ns: u64 },
    Pointer(PointerEvent),
    Timing(Timing),
}

/// Reassembles device → host messages from a byte stream that may deliver them
/// split across reads or several at once.
#[derive(Default)]
pub struct MessageParser {
    buf: Vec<u8>,
}

impl MessageParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every message completed by them. An unknown type
    /// byte is an error because the stream cannot be resynchronised.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<DeviceMessage>> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;
        while at < self.buf.len() {
            let payload_len = match self.buf[at] {
                MSG_ACK => ACK_LEN,
                MSG_POINTER => POINTER_LEN,
                MSG_TIMING => TIMING_LEN,
                other => bail!("unknown device message type 0x{other:02x}"),
            };
            if self.buf.len() - at < 1 + payload_len {
                break;
            }
            let payload = &self.buf[at + 1..at + 1 + payload_len];
            out.push(match self.buf[at] {
                MSG_ACK => DeviceMessage::Ack {
                    pts_ns: u64::from_be_bytes(payload.try_into().unwrap()),
                },
                MSG_TIMING => DeviceMessage::Timing(Timing::decode(payload)),
                _ => DeviceMessage::Pointer(PointerEvent::decode(payload)?),
            });
            at += 1 + payload_len;
        }
        self.buf.drain(..at);
        Ok(out)
    }
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

    fn sample_pointer() -> PointerEvent {
        PointerEvent {
            action: Action::Move,
            tool: Tool::Pen,
            pointer_id: 3,
            buttons: BUTTON_STYLUS_PRIMARY,
            flags: POINTER_FLAG_LAST,
            x: 0.25,
            y: 0.75,
            pressure: 0.5,
            tilt: 0.4,
            orientation: -1.5,
        }
    }

    #[test]
    fn pointer_round_trips() {
        let mut parser = MessageParser::new();
        let out = parser.push(&sample_pointer().encode()).unwrap();
        assert_eq!(out, vec![DeviceMessage::Pointer(sample_pointer())]);
    }

    #[test]
    fn parser_handles_split_and_batched_messages() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&encode_ack(42));
        bytes.extend_from_slice(&sample_pointer().encode());
        bytes.extend_from_slice(&encode_ack(43));

        // Feed one byte at a time: the worst case for a TCP-like stream.
        let mut parser = MessageParser::new();
        let mut out = Vec::new();
        for byte in &bytes {
            out.extend(parser.push(&[*byte]).unwrap());
        }
        assert_eq!(
            out,
            vec![
                DeviceMessage::Ack { pts_ns: 42 },
                DeviceMessage::Pointer(sample_pointer()),
                DeviceMessage::Ack { pts_ns: 43 },
            ]
        );
    }

    #[test]
    fn timing_round_trips_between_other_messages() {
        let timing = Timing {
            pts_us: 123_456_789,
            queue_wait_us: 1,
            decode_us: 7_800,
            display_wait_us: 9_100,
            render_interval_us: 16_667,
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&encode_ack(1));
        bytes.extend_from_slice(&encode_timing(&timing));
        bytes.extend_from_slice(&sample_pointer().encode());

        let mut parser = MessageParser::new();
        let mut out = Vec::new();
        for byte in &bytes {
            out.extend(parser.push(&[*byte]).unwrap());
        }
        assert_eq!(out[1], DeviceMessage::Timing(timing));
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn parser_rejects_unknown_type() {
        assert!(MessageParser::new().push(&[0x7f]).is_err());
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
