// SPDX-License-Identifier: Apache-2.0

//! Hardware H.264 encoding of captured DMA-BUF frames.
//!
//! ```text
//! appsrc  video/x-raw(memory:DMABuf) DMA_DRM XR24:<modifier>
//!   -> vapostproc     XR24 -> NV12, on the GPU
//!   -> vah264enc      CBR, no B-frames, sparse keyframes
//!   -> h264parse      Annex-B, SPS/PPS ahead of every IDR
//!   -> appsink
//! ```
//!
//! The DMA-BUF fds handed in here are the same ones the compositor wrote into,
//! so pixels never touch the CPU between compositor and encoder.

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_allocators::prelude::*;
use gstreamer_allocators::DmaBufAllocator;
use gstreamer_app::{AppSink, AppSrc};
use std::os::fd::BorrowedFd;
use std::time::Duration;

/// DRM format modifiers the VA encoder chain can import for `fourcc`, probed
/// from `vapostproc`'s advertised `memory:DMABuf` sink caps.
///
/// This must be probed, never hardcoded. Compositors offer modifiers the
/// encoder cannot read — on AMD, DCC-compressed tilings — and GBM will happily
/// prefer one. The result is not an error but a silent fallback to a CPU copy,
/// which defeats the whole zero-copy pipeline. The accepted set is also
/// vendor-specific, so a hardcoded value works on exactly one machine.
///
/// LINEAR is appended as a universal fallback.
pub fn supported_modifiers(fourcc: u32) -> Vec<u64> {
    let wanted = fourcc_name(fourcc);
    let mut modifiers = Vec::new();

    if gst::init().is_ok() {
        if let Some(factory) = gst::ElementFactory::find("vapostproc") {
            for template in factory.static_pad_templates() {
                if template.direction() != gst::PadDirection::Sink {
                    continue;
                }
                let caps = template.caps();
                for i in 0..caps.size() {
                    let Some(structure) = caps.structure(i) else {
                        continue;
                    };
                    let Ok(value) = structure.value("drm-format") else {
                        continue;
                    };
                    collect_modifiers(&value, &wanted, &mut modifiers);
                }
            }
        }
    }

    if !modifiers.contains(&0) {
        modifiers.push(0); // DRM_FORMAT_MOD_LINEAR
    }
    modifiers
}

/// `drm-format` is either a single string or a list of `FOURCC:0xMODIFIER`.
fn collect_modifiers(value: &gst::glib::Value, wanted: &str, out: &mut Vec<u64>) {
    if let Ok(text) = value.get::<String>() {
        if let Some(modifier) = parse_drm_format(&text, wanted) {
            if !out.contains(&modifier) {
                out.push(modifier);
            }
        }
        return;
    }
    if let Ok(list) = value.get::<gst::List>() {
        for item in list.iter() {
            collect_modifiers(item, wanted, out);
        }
    }
}

fn parse_drm_format(text: &str, wanted: &str) -> Option<u64> {
    let text = text.trim();
    let (fourcc, modifier) = match text.split_once(':') {
        Some((f, m)) => (f, m),
        // A bare fourcc with no modifier means linear.
        None => (text, "0"),
    };
    if fourcc.trim() != wanted {
        return None;
    }
    let modifier = modifier.trim();
    let parsed = modifier
        .strip_prefix("0x")
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .or_else(|| modifier.parse::<u64>().ok())?;
    Some(parsed)
}

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    /// DRM fourcc of the incoming frames (e.g. `XR24`).
    pub fourcc: u32,
    /// DRM format modifier of the incoming frames.
    pub modifier: u64,
    /// Distance between keyframes, in frames.
    ///
    /// USB is lossless, so frequent keyframes buy nothing — they exist for
    /// error recovery on lossy links. One at stream start is enough, and a
    /// large value keeps the bitrate flat. See `docs/02-encode.md`.
    pub keyframe_interval: u32,
    /// Encoder speed/quality balance, 1 (quality) .. 7 (speed).
    ///
    /// Do not trust the naming — measured on VCN 2.x, latency is *not*
    /// monotonic in this value. There is a cliff between 3 and 4:
    /// 1 → 7.3 ms, 2 → 6.2 ms, 3 → 6.2 ms, 4 → 10.8 ms, 7 → 10.7 ms.
    /// The nominal "speed" presets are the slow ones. See `docs/02-encode.md`.
    pub target_usage: u32,
    /// `cbr`, `vbr`, or `cqp`.
    pub rate_control: String,
    /// CABAC entropy coding. Compresses better; CAVLC may decode marginally faster.
    pub cabac: bool,
    /// Slices per frame. More slices allow partial-frame transmission.
    pub num_slices: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1200,
            framerate: 60,
            bitrate_kbps: 20_000,
            fourcc: u32::from_le_bytes(*b"XR24"),
            modifier: 0x0200_0000_0000_0901,
            keyframe_interval: 600,
            target_usage: 3,
            rate_control: "vbr".to_string(),
            cabac: true,
            num_slices: 1,
        }
    }
}

/// One encoded access unit.
pub struct EncodedPacket {
    pub data: Vec<u8>,
    /// Presentation timestamp as handed in with the source frame, nanoseconds.
    pub pts_ns: u64,
    pub keyframe: bool,
}

pub struct Encoder {
    pipeline: gst::Pipeline,
    appsrc: AppSrc,
    appsink: AppSink,
    allocator: DmaBufAllocator,
    frame_size: usize,
}

impl Encoder {
    pub fn new(config: &EncoderConfig) -> Result<Self> {
        gst::init().context("initialising GStreamer")?;

        let drm_format = format!(
            "{}:0x{:016x}",
            fourcc_name(config.fourcc),
            config.modifier
        );

        // DMA_DRM caps carry the tiling in `drm-format`; the encoder chain
        // stays on the GPU from here to the bitstream.
        let src_caps = gst::Caps::builder("video/x-raw")
            .features(["memory:DMABuf"])
            .field("format", "DMA_DRM")
            .field("drm-format", &drm_format)
            .field("width", config.width as i32)
            .field("height", config.height as i32)
            .field(
                "framerate",
                gst::Fraction::new(config.framerate as i32, 1),
            )
            .build();

        let pipeline = gst::Pipeline::new();

        let appsrc = gst::ElementFactory::make("appsrc")
            .property("caps", &src_caps)
            .property_from_str("format", "time")
            .property("is-live", true)
            // Never queue frames: a stale frame is worse than a dropped one.
            .property("max-buffers", 1u64)
            .property_from_str("leaky-type", "downstream")
            .build()
            .context("creating appsrc")?;

        let postproc = gst::ElementFactory::make("vapostproc")
            .build()
            .context("creating vapostproc")?;

        let nv12_caps = gst::Caps::builder("video/x-raw")
            .features(["memory:VAMemory"])
            .field("format", "NV12")
            .build();
        let capsfilter = gst::ElementFactory::make("capsfilter")
            .property("caps", &nv12_caps)
            .build()
            .context("creating capsfilter")?;

        let encoder = gst::ElementFactory::make("vah264enc")
            .property_from_str("rate-control", &config.rate_control)
            .property("bitrate", config.bitrate_kbps)
            // A tight coded-picture buffer forces bits to be spread evenly,
            // which is what keeps keyframe spikes bounded now that
            // intra-refresh is unavailable through gst-plugin-va.
            .property("cpb-size", config.bitrate_kbps / 2)
            .property("b-frames", 0u32)
            .property("ref-frames", 1u32)
            .property("key-int-max", config.keyframe_interval)
            .property("target-usage", config.target_usage)
            .property("cabac", config.cabac)
            .property("num-slices", config.num_slices)
            .property("aud", true)
            .build()
            .context("creating vah264enc")?;

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

        let elements = [
            &appsrc,
            &postproc,
            &capsfilter,
            &encoder,
            &parser,
            &parsed_filter,
            &appsink,
        ];
        pipeline.add_many(elements).context("adding elements")?;
        gst::Element::link_many(elements).context("linking pipeline")?;

        let appsrc = appsrc.dynamic_cast::<AppSrc>().unwrap();
        let appsink = appsink.dynamic_cast::<AppSink>().unwrap();

        pipeline
            .set_state(gst::State::Playing)
            .context("starting pipeline")?;

        Ok(Self {
            pipeline,
            appsrc,
            appsink,
            allocator: DmaBufAllocator::new(),
            // Conservative upper bound; the kernel clamps the mapping to the
            // real dmabuf size and tiled layouts are never larger than this.
            frame_size: (config.width as usize) * (config.height as usize) * 4,
        })
    }

    /// Submit a captured frame.
    ///
    /// `alloc_dmabuf` takes ownership of the fd and closes it with the memory,
    /// so the fd is duplicated first — otherwise GStreamer would close the
    /// capture pool's buffer out from under the compositor.
    pub fn push_frame(&self, fd: BorrowedFd<'_>, pts_ns: u64) -> Result<()> {
        let owned = fd
            .try_clone_to_owned()
            .context("duplicating DMA-BUF fd for the encoder")?;
        let memory = unsafe {
            self.allocator
                .alloc_dmabuf(owned, self.frame_size)
                .context("wrapping DMA-BUF fd as GstMemory")?
        };

        let mut buffer = gst::Buffer::new();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.append_memory(memory);
            buffer.set_pts(gst::ClockTime::from_nseconds(pts_ns));
        }

        self.appsrc
            .push_buffer(buffer)
            .map_err(|e| anyhow::anyhow!("pushing buffer into appsrc: {e:?}"))?;
        Ok(())
    }

    /// Pull the next encoded access unit, waiting up to `timeout`.
    pub fn pull_packet(&self, timeout: Duration) -> Result<Option<EncodedPacket>> {
        let sample = match self
            .appsink
            .try_pull_sample(gst::ClockTime::from_nseconds(timeout.as_nanos() as u64))
        {
            Some(sample) => sample,
            None => {
                self.check_bus()?;
                return Ok(None);
            }
        };

        let buffer = sample.buffer().context("sample carried no buffer")?;
        let map = buffer.map_readable().context("mapping encoded buffer")?;
        let keyframe = !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT);

        Ok(Some(EncodedPacket {
            data: map.as_slice().to_vec(),
            pts_ns: buffer.pts().map(|t| t.nseconds()).unwrap_or(0),
            keyframe,
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

impl Drop for Encoder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn fourcc_name(code: u32) -> String {
    String::from_utf8_lossy(&code.to_le_bytes()).into_owned()
}
