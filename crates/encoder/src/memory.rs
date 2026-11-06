// SPDX-License-Identifier: Apache-2.0

//! H.264 encoding of CPU-memory frames, for machines where the zero-copy
//! VA-API chain in [`crate::Encoder`] is not an option.
//!
//! ```text
//! appsrc  video/x-raw  BGRx (system memory)
//!   -> nvh264enc      NVENC; converts colour on the GPU
//!      | videoconvert -> x264enc     CPU fallback
//!   -> h264parse      Annex-B, SPS/PPS ahead of every IDR
//!   -> appsink
//! ```
//!
//! The output is the same byte stream [`crate::Encoder`] produces, so the
//! transport and the device app cannot tell the two apart.

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::{AppSink, AppSrc};
use std::time::Duration;

use crate::EncodedPacket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// NVIDIA's hardware encoder, through `nvh264enc`.
    Nvenc,
    /// libx264 on the CPU.
    X264,
}

impl Backend {
    /// The best backend whose GStreamer element is installed.
    ///
    /// Presence of the element is not proof the encoder works — `nvh264enc`
    /// is registered even when no NVIDIA device is usable — so a failure to
    /// start the pipeline is reported by [`MemoryEncoder::new`], not here.
    pub fn detect() -> Option<Self> {
        gst::init().ok()?;
        if gst::ElementFactory::find("nvh264enc").is_some() {
            Some(Self::Nvenc)
        } else if gst::ElementFactory::find("x264enc").is_some() {
            Some(Self::X264)
        } else {
            None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Nvenc => "nvenc",
            Self::X264 => "x264",
        }
    }
}

impl std::str::FromStr for Backend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "nvenc" => Ok(Self::Nvenc),
            "x264" => Ok(Self::X264),
            other => bail!("unknown encoder backend {other:?} (expected nvenc or x264)"),
        }
    }
}

/// NVENC preset, `p1` (fastest) to `p7` (best quality). `p5` is the default:
/// a GTX 1650 holds 60 fps at 1920x1200 with it. Override with
/// `MORELAND_NVENC_PRESET` to trade latency for sharpness.
fn nvenc_preset() -> String {
    std::env::var("MORELAND_NVENC_PRESET")
        .ok()
        .filter(|p| matches!(p.as_str(), "p1" | "p2" | "p3" | "p4" | "p5" | "p6" | "p7"))
        .unwrap_or_else(|| "p5".to_string())
}

pub struct MemoryEncoderConfig {
    pub backend: Backend,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    /// Distance between keyframes, in frames. See `EncoderConfig`.
    pub keyframe_interval: u32,
}

/// Default bitrate. Well above the VA-API path's 20 Mbps: the link is USB, so
/// bandwidth is cheap, and desktop content (text, thin lines) is exactly what
/// low bitrates smear.
pub const DEFAULT_BITRATE_KBPS: u32 = 60_000;

impl MemoryEncoderConfig {
    pub fn new(backend: Backend, width: u32, height: u32, framerate: u32) -> Self {
        Self {
            backend,
            width,
            height,
            framerate,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            keyframe_interval: 600,
        }
    }
}

/// Bytes per pixel of the formats accepted: all four-byte packed RGB.
const BYTES_PER_PIXEL: usize = 4;

pub struct MemoryEncoder {
    pipeline: gst::Pipeline,
    appsrc: AppSrc,
    appsink: AppSink,
    width: u32,
    height: u32,
}

impl MemoryEncoder {
    /// Start an encoder for frames of `format` (a GStreamer name such as
    /// `BGRx`).
    pub fn new(config: &MemoryEncoderConfig, format: &str) -> Result<Self> {
        gst::init().context("initialising GStreamer")?;

        let src_caps = gst::Caps::builder("video/x-raw")
            .field("format", format)
            .field("width", config.width as i32)
            .field("height", config.height as i32)
            .field("framerate", gst::Fraction::new(config.framerate as i32, 1))
            .build();

        let appsrc = gst::ElementFactory::make("appsrc")
            .property("caps", &src_caps)
            .property_from_str("format", "time")
            .property("is-live", true)
            // Never queue frames: a stale frame is worse than a dropped one.
            .property("max-buffers", 1u64)
            .property_from_str("leaky-type", "downstream")
            .build()
            .context("creating appsrc")?;

        let mut elements = vec![appsrc.clone()];

        match config.backend {
            Backend::Nvenc => {
                // One frame of buffering keeps VBR from spending a burst on a
                // scene change that the link then has to absorb as a stall.
                let vbv = config.bitrate_kbps / config.framerate.max(1);
                let encoder = gst::ElementFactory::make("nvh264enc")
                    .property_from_str("rc-mode", "vbr")
                    .property("bitrate", config.bitrate_kbps)
                    .property("max-bitrate", config.bitrate_kbps + config.bitrate_kbps / 2)
                    .property("vbv-buffer-size", vbv.max(1) * 2)
                    .property("gop-size", config.keyframe_interval as i32)
                    .property("bframes", 0u32)
                    .property("aud", true)
                    .property("repeat-sequence-header", true)
                    .property("zerolatency", true)
                    .property("cabac", true)
                    // Redistributes bits towards flat areas and text edges,
                    // which is where blocking shows on a desktop.
                    .property("spatial-aq", true)
                    // p1..p7 with a tune replaces the deprecated named presets.
                    .property_from_str("preset", &nvenc_preset())
                    .property_from_str("tune", "low-latency")
                    .build()
                    .context("creating nvh264enc — is the NVIDIA driver loaded?")?;
                elements.push(encoder);

                // NVENC emits Constrained Baseline unless told otherwise, the
                // least efficient profile; every Android hardware decoder
                // handles High.
                let high = gst::ElementFactory::make("capsfilter")
                    .property(
                        "caps",
                        gst::Caps::builder("video/x-h264")
                            .field("profile", "high")
                            .build(),
                    )
                    .build()
                    .context("creating profile capsfilter")?;
                elements.push(high);
            }
            Backend::X264 => {
                let convert = gst::ElementFactory::make("videoconvert")
                    .build()
                    .context("creating videoconvert")?;
                let encoder = gst::ElementFactory::make("x264enc")
                    .property("bitrate", config.bitrate_kbps)
                    .property("key-int-max", config.keyframe_interval)
                    .property("bframes", 0u32)
                    .property("aud", true)
                    .property("byte-stream", true)
                    .property("sliced-threads", true)
                    .property_from_str("speed-preset", "ultrafast")
                    .property_from_str("tune", "zerolatency")
                    .build()
                    .context("creating x264enc")?;
                elements.push(convert);
                elements.push(encoder);
            }
        }

        let parser = gst::ElementFactory::make("h264parse")
            // -1 repeats SPS/PPS before every keyframe, so MediaCodec can
            // configure itself from the stream without out-of-band csd.
            .property("config-interval", -1i32)
            .build()
            .context("creating h264parse")?;
        let parsed_caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let parsed_filter = gst::ElementFactory::make("capsfilter")
            .property("caps", &parsed_caps)
            .build()
            .context("creating h264 capsfilter")?;
        let appsink = gst::ElementFactory::make("appsink")
            .property("sync", false)
            .property("max-buffers", 4u32)
            .property("drop", false)
            .build()
            .context("creating appsink")?;

        elements.extend([parser, parsed_filter, appsink.clone()]);

        let pipeline = gst::Pipeline::new();
        pipeline.add_many(&elements).context("adding elements")?;
        gst::Element::link_many(&elements).context("linking pipeline")?;

        let appsrc = appsrc
            .dynamic_cast::<AppSrc>()
            .map_err(|_| anyhow::anyhow!("appsrc is not an AppSrc"))?;
        let appsink = appsink
            .dynamic_cast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("appsink is not an AppSink"))?;

        pipeline
            .set_state(gst::State::Playing)
            .with_context(|| format!("starting the {} pipeline", config.backend.name()))?;

        Ok(Self {
            pipeline,
            appsrc,
            appsink,
            width: config.width,
            height: config.height,
        })
    }

    /// Submit a frame the caller no longer needs, without copying it again when
    /// its rows are already tightly packed. A 2560x1600 frame is 16 MB, and the
    /// capture side has already copied it once out of PipeWire's buffer.
    pub fn push_owned(&self, mut data: Vec<u8>, stride: u32, pts_ns: u64) -> Result<()> {
        let row = self.width as usize * BYTES_PER_PIXEL;
        let size = row * self.height as usize;
        if stride as usize == row && data.len() >= size {
            data.truncate(size);
            return self.push_packed(data, pts_ns);
        }
        self.push_frame(&data, stride, pts_ns)
    }

    /// Submit one frame. `stride` is the source row pitch in bytes.
    ///
    /// Rows are repacked when the pitch is not tight, because the pipeline's
    /// caps promise a tightly packed buffer and carry no per-buffer layout.
    pub fn push_frame(&self, data: &[u8], stride: u32, pts_ns: u64) -> Result<()> {
        let row = self.width as usize * BYTES_PER_PIXEL;
        let stride = stride as usize;
        let rows = self.height as usize;

        let packed = if stride == row {
            data.get(..row * rows)
                .context("frame is smaller than its dimensions")?
                .to_vec()
        } else {
            if stride < row || data.len() < stride * (rows - 1) + row {
                bail!(
                    "frame of {} bytes does not fit {}x{} at stride {stride}",
                    data.len(),
                    self.width,
                    self.height
                );
            }
            let mut out = Vec::with_capacity(row * rows);
            for y in 0..rows {
                out.extend_from_slice(&data[y * stride..y * stride + row]);
            }
            out
        };

        self.push_packed(packed, pts_ns)
    }

    fn push_packed(&self, packed: Vec<u8>, pts_ns: u64) -> Result<()> {
        let mut buffer = gst::Buffer::from_mut_slice(packed);
        buffer
            .get_mut()
            .context("fresh buffer is not writable")?
            .set_pts(gst::ClockTime::from_nseconds(pts_ns));

        self.appsrc
            .push_buffer(buffer)
            .map_err(|e| anyhow::anyhow!("pushing buffer into appsrc: {e:?}"))?;
        Ok(())
    }

    /// Pull the next encoded access unit, waiting up to `timeout`.
    pub fn pull_packet(&self, timeout: Duration) -> Result<Option<EncodedPacket>> {
        let Some(sample) = self
            .appsink
            .try_pull_sample(gst::ClockTime::from_nseconds(timeout.as_nanos() as u64))
        else {
            self.check_bus()?;
            return Ok(None);
        };

        let buffer = sample.buffer().context("sample carried no buffer")?;
        let map = buffer.map_readable().context("mapping encoded buffer")?;
        Ok(Some(EncodedPacket {
            data: map.as_slice().to_vec(),
            pts_ns: buffer.pts().map(|t| t.nseconds()).unwrap_or(0),
            keyframe: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
        }))
    }

    /// Surface any pipeline error rather than letting it stall silently.
    fn check_bus(&self) -> Result<()> {
        let Some(bus) = self.pipeline.bus() else {
            return Ok(());
        };
        while let Some(msg) = bus.pop() {
            if let gst::MessageView::Error(err) = msg.view() {
                bail!(
                    "pipeline error from {}: {} ({})",
                    err.src().map(|s| s.path_string()).unwrap_or_default(),
                    err.error(),
                    err.debug().unwrap_or_default()
                );
            }
        }
        Ok(())
    }

    pub fn stop(&self) {
        let _ = self.appsrc.end_of_stream();
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl crate::PacketSource for MemoryEncoder {
    fn pull_packet(&self, timeout: Duration) -> Result<Option<EncodedPacket>> {
        MemoryEncoder::pull_packet(self, timeout)
    }
}

impl Drop for MemoryEncoder {
    fn drop(&mut self) {
        self.stop();
    }
}
