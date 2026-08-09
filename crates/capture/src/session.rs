// SPDX-License-Identifier: Apache-2.0

//! Persistent capture session over `ext-image-copy-capture-v1`.
//!
//! The session and its buffer pool are created once and reused for every
//! frame; only the short-lived `frame` object is per-capture, because the
//! protocol permits just one in flight at a time.

use anyhow::{bail, Context, Result};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

use crate::dmabuf::{Dmabuf, DmabufAllocator};
use crate::shm::ShmBuffer;

/// Which buffer type to hand the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferMode {
    /// CPU-visible shared memory. Costs a full frame copy; bring-up only.
    Shm,
    /// GPU buffer shared by fd. Zero-copy into the encoder.
    Dmabuf,
}

/// How to set up a capture session.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub mode: BufferMode,
    pub pool_size: usize,
    /// DRM format modifiers the *consumer* of these buffers can accept, in
    /// preference order. Empty means "let GBM choose".
    ///
    /// This matters more than it looks. Left to itself GBM picks the most
    /// capable modifier the compositor offers, which on AMD is a DCC
    /// (delta-colour-compressed) tiling — and the VCN video encoder cannot read
    /// compressed surfaces. The result is not an error but a silent fallback to
    /// a CPU copy, which defeats the entire point of the DMA-BUF path.
    pub allowed_modifiers: Vec<u64>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            mode: BufferMode::Dmabuf,
            pool_size: 2,
            allowed_modifiers: Vec::new(),
        }
    }
}

/// What the compositor advertised for this source.
#[derive(Debug, Default, Clone)]
pub struct SessionCaps {
    pub buffer_size: Option<(u32, u32)>,
    pub shm_formats: Vec<wl_shm::Format>,
    pub dmabuf_formats: Vec<(u32, Vec<u64>)>,
    pub dmabuf_device: Option<u64>,
}

/// Timing for one captured frame.
#[derive(Debug, Clone, Copy)]
pub struct FrameTiming {
    /// Time from issuing `capture` to receiving `ready`. At a steady frame
    /// rate this is dominated by waiting for the compositor's next frame, so
    /// it measures the polling interval rather than added pipeline latency.
    pub latency: Duration,
    /// Compositor presentation timestamp (CLOCK_MONOTONIC nanoseconds).
    pub presentation_ns: Option<u64>,
    /// How old the frame already was when `ready` reached us. This is the
    /// figure that actually enters the end-to-end latency budget.
    pub age: Option<Duration>,
    /// Index into the buffer pool that received this frame.
    pub buffer_index: usize,
}

/// CLOCK_MONOTONIC now, matching the clock Wayland reports presentation in.
fn monotonic_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

struct OutputEntry {
    proxy: wl_output::WlOutput,
    name: String,
}

#[derive(Default)]
struct State {
    outputs: Vec<OutputEntry>,
    caps: SessionCaps,
    session_done: bool,
    /// Set once the current constraint set has been used to build a pool. The
    /// next constraint event of *any* kind then begins a fresh generation.
    ///
    /// Keying this off `buffer_size` specifically would be a bug: the protocol
    /// does not guarantee which constraint event arrives first, so a
    /// `dmabuf_format` leading the batch would be discarded by a later reset,
    /// leaving an empty format list.
    caps_consumed: bool,
    session_stopped: bool,
    frame_ready: bool,
    frame_failed: Option<ext_image_copy_capture_frame_v1::FailureReason>,
    frame_failed_raw: Option<u32>,
    presentation_ns: Option<u64>,
}

impl State {
    /// Start accumulating a new constraint generation, if the previous one has
    /// already been used to build a buffer pool.
    fn begin_generation(&mut self) {
        if self.caps_consumed {
            self.caps = SessionCaps::default();
            self.caps_consumed = false;
            self.session_done = false;
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        &index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(entry) = state.outputs.get_mut(index) {
                entry.name = name;
            }
        }
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                state.begin_generation();
                state.caps.buffer_size = Some((width, height));
            }
            Event::ShmFormat { format } => {
                state.begin_generation();
                if let WEnum::Value(format) = format {
                    state.caps.shm_formats.push(format);
                }
            }
            Event::DmabufDevice { device } => {
                state.begin_generation();
                if device.len() >= 8 {
                    state.caps.dmabuf_device =
                        Some(u64::from_ne_bytes(device[..8].try_into().unwrap()));
                }
            }
            Event::DmabufFormat { format, modifiers } => {
                state.begin_generation();
                let modifiers = modifiers
                    .chunks_exact(8)
                    .map(|c| u64::from_ne_bytes(c.try_into().unwrap()))
                    .collect();
                state.caps.dmabuf_formats.push((format, modifiers));
            }
            Event::Done => state.session_done = true,
            Event::Stopped => state.session_stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let secs = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                state.presentation_ns = Some(secs * 1_000_000_000 + u64::from(tv_nsec));
            }
            Event::Ready => state.frame_ready = true,
            Event::Failed { reason } => match reason {
                WEnum::Value(reason) => state.frame_failed = Some(reason),
                WEnum::Unknown(raw) => state.frame_failed_raw = Some(raw),
            },
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore ZwpLinuxDmabufV1);
delegate_noop!(State: ignore ZwpLinuxBufferParamsV1);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ExtImageCaptureSourceV1);
delegate_noop!(State: ExtOutputImageCaptureSourceManagerV1);
delegate_noop!(State: ExtImageCopyCaptureManagerV1);

enum PoolBuffer {
    Shm {
        wl: wl_buffer::WlBuffer,
        backing: ShmBuffer,
        pool: wl_shm_pool::WlShmPool,
    },
    Dmabuf {
        wl: wl_buffer::WlBuffer,
        backing: Dmabuf,
    },
}

impl PoolBuffer {
    fn wl(&self) -> &wl_buffer::WlBuffer {
        match self {
            PoolBuffer::Shm { wl, .. } | PoolBuffer::Dmabuf { wl, .. } => wl,
        }
    }

    fn destroy(self) {
        match self {
            PoolBuffer::Shm { wl, pool, .. } => {
                wl.destroy();
                pool.destroy();
            }
            PoolBuffer::Dmabuf { wl, .. } => wl.destroy(),
        }
    }
}

/// Distinguishes recoverable capture failures from fatal ones.
enum CaptureError {
    /// The pool no longer matches the session; re-allocate and retry.
    BufferConstraints,
    Stopped,
    Other(anyhow::Error),
}

impl From<CaptureError> for anyhow::Error {
    fn from(e: CaptureError) -> Self {
        match e {
            CaptureError::BufferConstraints => {
                anyhow::anyhow!("capture failed: buffer constraints changed")
            }
            CaptureError::Stopped => {
                anyhow::anyhow!("capture session was stopped by the compositor")
            }
            CaptureError::Other(e) => e,
        }
    }
}

/// A live capture session bound to one output.
pub struct Capture {
    queue: EventQueue<State>,
    state: State,
    qh: QueueHandle<State>,
    session: ExtImageCopyCaptureSessionV1,
    buffers: Vec<PoolBuffer>,
    next_buffer: usize,
    pub width: u32,
    pub height: u32,
    pub mode: BufferMode,
    pub caps: SessionCaps,
    /// Format actually negotiated (a `wl_shm` format, or a DRM fourcc).
    pub format: u32,
    pub modifier: Option<u64>,
    pub device_path: Option<std::path::PathBuf>,

    // Retained so the pool can be rebuilt when the compositor changes its
    // buffer constraints mid-session.
    pool_size: usize,
    allowed_modifiers: Vec<u64>,
    geometry_changed: bool,
    shm: wl_shm::WlShm,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    allocator: Option<DmabufAllocator>,

    // Held to keep the protocol objects alive for the session's lifetime.
    _conn: Connection,
    _source: ExtImageCaptureSourceV1,
}

impl Capture {
    pub fn new(output_name: &str, config: &CaptureConfig) -> Result<Self> {
        let (mode, pool_size) = (config.mode, config.pool_size.max(1));
        let conn = Connection::connect_to_env().context("connecting to Wayland compositor")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).context("initialising Wayland registry")?;
        let qh = queue.handle();
        let mut state = State::default();

        let output_globals: Vec<(u32, u32)> = globals.contents().with_list(|list| {
            list.iter()
                .filter(|g| g.interface == "wl_output")
                .map(|g| (g.name, g.version))
                .collect()
        });
        let registry = globals.registry();
        for (name, version) in output_globals {
            let index = state.outputs.len();
            let proxy =
                registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), &qh, index);
            state.outputs.push(OutputEntry {
                proxy,
                name: String::new(),
            });
        }
        queue.roundtrip(&mut state)?;
        queue.roundtrip(&mut state)?;

        let output = state
            .outputs
            .iter()
            .find(|o| o.name == output_name)
            .map(|o| o.proxy.clone())
            .with_context(|| {
                let known: Vec<&str> = state.outputs.iter().map(|o| o.name.as_str()).collect();
                format!("no output named {output_name:?} (have: {})", known.join(", "))
            })?;

        let source_manager: ExtOutputImageCaptureSourceManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("binding ext_output_image_capture_source_manager_v1")?;
        let capture_manager: ExtImageCopyCaptureManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("binding ext_image_copy_capture_manager_v1")?;
        let shm: wl_shm::WlShm = globals.bind(&qh, 1..=2, ()).context("binding wl_shm")?;

        let source = source_manager.create_source(&output, &qh, ());
        let session = capture_manager.create_session(
            &source,
            ext_image_copy_capture_manager_v1::Options::empty(),
            &qh,
            (),
        );

        while !state.session_done {
            queue.blocking_dispatch(&mut state)?;
            if state.session_stopped {
                bail!("capture session stopped during negotiation");
            }
        }

        let dmabuf_global = if mode == BufferMode::Dmabuf {
            Some(
                globals
                    .bind::<ZwpLinuxDmabufV1, _, _>(&qh, 3..=5, ())
                    .context("binding zwp_linux_dmabuf_v1")?,
            )
        } else {
            None
        };

        let mut capture = Self {
            queue,
            state,
            qh,
            session,
            buffers: Vec::new(),
            next_buffer: 0,
            width: 0,
            height: 0,
            mode,
            caps: SessionCaps::default(),
            format: 0,
            modifier: None,
            device_path: None,
            pool_size,
            allowed_modifiers: config.allowed_modifiers.clone(),
            geometry_changed: false,
            shm,
            dmabuf: dmabuf_global,
            allocator: None,
            _conn: conn,
            _source: source,
        };
        capture.allocate_pool()?;
        Ok(capture)
    }

    /// Build (or rebuild) the buffer pool from the session's current constraints.
    fn allocate_pool(&mut self) -> Result<()> {
        for buffer in self.buffers.drain(..) {
            buffer.destroy();
        }

        let caps = self.state.caps.clone();
        let (width, height) = caps
            .buffer_size
            .context("compositor never sent buffer_size")?;

        let mut buffers = Vec::with_capacity(self.pool_size);
        let mut modifier = None;
        let format;

        match self.mode {
            BufferMode::Shm => {
                let shm_format = pick_shm_format(&caps.shm_formats)?;
                format = shm_format as u32;
                for _ in 0..self.pool_size {
                    let backing = ShmBuffer::new(width, height)?;
                    let pool = self.shm.create_pool(
                        backing.fd.as_fd(),
                        backing.size() as i32,
                        &self.qh,
                        (),
                    );
                    let wl = pool.create_buffer(
                        0,
                        width as i32,
                        height as i32,
                        backing.stride as i32,
                        shm_format,
                        &self.qh,
                        (),
                    );
                    buffers.push(PoolBuffer::Shm { wl, backing, pool });
                }
            }
            BufferMode::Dmabuf => {
                let dmabuf = self
                    .dmabuf
                    .as_ref()
                    .context("DMA-BUF mode without a linux-dmabuf global")?;
                let (fourcc, modifiers) =
                    pick_dmabuf_format(&caps.dmabuf_formats, &self.allowed_modifiers)?;
                format = fourcc;

                let dev = caps
                    .dmabuf_device
                    .context("compositor offered DMA-BUF but no dmabuf_device")?;
                // The device can change between generations, so resolve it each time.
                let alloc = match self.allocator.take() {
                    Some(existing) if existing.dev_t == Some(dev) => existing,
                    _ => DmabufAllocator::from_dev_t(dev)?,
                };

                for _ in 0..self.pool_size {
                    let backing = alloc.allocate(width, height, fourcc, &modifiers)?;
                    modifier = Some(backing.modifier);

                    let params = dmabuf.create_params(&self.qh, ());
                    for (index, plane) in backing.planes.iter().enumerate() {
                        params.add(
                            plane.fd.as_fd(),
                            index as u32,
                            plane.offset,
                            plane.stride,
                            (backing.modifier >> 32) as u32,
                            (backing.modifier & 0xffff_ffff) as u32,
                        );
                    }
                    let wl = params.create_immed(
                        width as i32,
                        height as i32,
                        fourcc,
                        zwp_linux_buffer_params_v1::Flags::empty(),
                        &self.qh,
                        (),
                    );
                    params.destroy();
                    buffers.push(PoolBuffer::Dmabuf { wl, backing });
                }

                self.device_path = Some(alloc.path.clone());
                self.allocator = Some(alloc);
            }
        }

        self.buffers = buffers;
        self.next_buffer = 0;
        self.width = width;
        self.height = height;
        self.caps = caps;
        self.format = format;
        self.modifier = modifier;
        // This generation is now in use; the next constraint event starts a
        // fresh one.
        self.state.caps_consumed = true;
        Ok(())
    }

    /// Re-run constraint negotiation after the compositor invalidates our pool.
    ///
    /// The compositor emits a fresh `buffer_size` / format set followed by
    /// `done`; the first constraint event resets `session_done`, so waiting for
    /// it again is what synchronises us with the new generation.
    fn renegotiate(&mut self) -> Result<()> {
        let (old_width, old_height) = (self.width, self.height);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.state.session_done {
            if self.state.session_stopped {
                bail!("capture session stopped during renegotiation");
            }
            if Instant::now() > deadline {
                bail!("timed out waiting for new buffer constraints");
            }
            self.queue.blocking_dispatch(&mut self.state)?;
        }
        self.allocate_pool()?;

        // A format or modifier change is absorbed here — that is the common
        // case, raised by things like a wallpaper daemon claiming the output.
        //
        // A *geometry* change cannot be: the encoder and the device's decoder
        // were both configured for the old dimensions, and neither is told
        // about this. Silently continuing produces exactly the failure this
        // was written to fix — frames captured but nothing rendered. Report it
        // so the caller rebuilds the whole pipeline.
        if (self.width, self.height) != (old_width, old_height) {
            self.geometry_changed = true;
            bail!(
                "capture geometry changed {}x{} -> {}x{}; pipeline must restart",
                old_width,
                old_height,
                self.width,
                self.height
            );
        }

        tracing::info!(
            "capture renegotiated: {}x{} format {} modifier {:?}",
            self.width,
            self.height,
            fourcc_name(self.format),
            self.modifier.map(|m| format!("0x{m:016x}")),
        );
        Ok(())
    }

    /// True when the session ended because the output was resized under us.
    pub fn geometry_changed(&self) -> bool {
        self.geometry_changed
    }

    /// Capture one frame into the next pool buffer, blocking until it is ready.
    ///
    /// Transparently recovers from `buffer_constraints`, which the compositor
    /// raises whenever its buffer requirements change — a wallpaper daemon
    /// claiming the output, a mode change, a scanout path switch. The protocol
    /// specifies re-allocating and retrying, so that is not an error path.
    pub fn capture_frame(&mut self) -> Result<FrameTiming> {
        match self.try_capture_frame() {
            Err(CaptureError::BufferConstraints) => {
                tracing::info!("buffer constraints changed; re-allocating capture pool");
                self.renegotiate()?;
                self.try_capture_frame().map_err(Into::into)
            }
            other => other.map_err(Into::into),
        }
    }

    fn try_capture_frame(&mut self) -> std::result::Result<FrameTiming, CaptureError> {
        if self.state.session_stopped {
            return Err(CaptureError::Stopped);
        }
        if self.buffers.is_empty() {
            return Err(CaptureError::Other(anyhow::anyhow!("capture pool is empty")));
        }

        let index = self.next_buffer;
        self.next_buffer = (self.next_buffer + 1) % self.buffers.len();

        self.state.frame_ready = false;
        self.state.frame_failed = None;
        self.state.frame_failed_raw = None;
        self.state.presentation_ns = None;

        let frame = self.session.create_frame(&self.qh, ());
        frame.attach_buffer(self.buffers[index].wl());
        frame.damage_buffer(0, 0, self.width as i32, self.height as i32);
        frame.capture();

        let started = Instant::now();
        while !self.state.frame_ready
            && self.state.frame_failed.is_none()
            && self.state.frame_failed_raw.is_none()
        {
            self.queue
                .blocking_dispatch(&mut self.state)
                .map_err(|e| CaptureError::Other(e.into()))?;
        }
        let latency = started.elapsed();
        let received_ns = monotonic_ns();

        // Exactly one frame may exist per session at a time.
        frame.destroy();

        if let Some(reason) = self.state.frame_failed {
            use ext_image_copy_capture_frame_v1::FailureReason;
            return Err(match reason {
                FailureReason::BufferConstraints => CaptureError::BufferConstraints,
                FailureReason::Stopped => CaptureError::Stopped,
                other => CaptureError::Other(anyhow::anyhow!("capture failed: {other:?}")),
            });
        }
        if let Some(raw) = self.state.frame_failed_raw {
            return Err(CaptureError::Other(anyhow::anyhow!(
                "capture failed with unknown reason {raw}"
            )));
        }

        let age = self
            .state
            .presentation_ns
            .map(|p| Duration::from_nanos(received_ns.saturating_sub(p)));

        Ok(FrameTiming {
            latency,
            presentation_ns: self.state.presentation_ns,
            age,
            buffer_index: index,
        })
    }

    /// The DMA-BUF backing a pool slot, for handing to a consumer such as the
    /// encoder. Returns `None` in shm mode.
    pub fn dmabuf(&self, index: usize) -> Option<&Dmabuf> {
        match self.buffers.get(index)? {
            PoolBuffer::Dmabuf { backing, .. } => Some(backing),
            PoolBuffer::Shm { .. } => None,
        }
    }

    /// Bytes of a shm pool buffer. Returns `None` in DMA-BUF mode, where the
    /// pixels live on the GPU and are never mapped.
    pub fn shm_bytes(&self, index: usize) -> Option<(&[u8], u32)> {
        match self.buffers.get(index)? {
            PoolBuffer::Shm { backing, .. } => {
                Some((&backing.map[..backing.size()], backing.stride))
            }
            PoolBuffer::Dmabuf { .. } => None,
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        for buffer in self.buffers.drain(..) {
            buffer.destroy();
        }
        self.session.destroy();
    }
}

fn pick_shm_format(formats: &[wl_shm::Format]) -> Result<wl_shm::Format> {
    for wanted in [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888] {
        if formats.contains(&wanted) {
            return Ok(wanted);
        }
    }
    bail!("no supported shm format offered (got: {formats:?})")
}

/// Prefer XR24 — the virtual output is opaque, and dropping alpha keeps the
/// encoder's colour conversion trivial.
///
/// When the consumer restricts modifiers, intersect against its list *in the
/// consumer's preference order* and return exactly one, so GBM has no room to
/// substitute something the consumer cannot read.
fn pick_dmabuf_format(
    formats: &[(u32, Vec<u64>)],
    allowed: &[u64],
) -> Result<(u32, Vec<u64>)> {
    const XR24: u32 = fourcc(b"XR24");
    const AR24: u32 = fourcc(b"AR24");

    for wanted in [XR24, AR24] {
        let Some((fourcc, offered)) = formats.iter().find(|(f, _)| *f == wanted) else {
            continue;
        };
        if allowed.is_empty() {
            return Ok((*fourcc, offered.clone()));
        }
        if let Some(modifier) = allowed.iter().find(|m| offered.contains(m)) {
            return Ok((*fourcc, vec![*modifier]));
        }
    }

    if allowed.is_empty() {
        bail!("no supported DMA-BUF format offered");
    }
    bail!(
        "compositor and consumer share no DMA-BUF modifier \
         (consumer accepts {allowed:02x?}, compositor offers {:02x?})",
        formats
            .iter()
            .map(|(f, m)| (fourcc_name(*f), m))
            .collect::<Vec<_>>()
    )
}

fn fourcc_name(code: u32) -> String {
    let bytes = code.to_le_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic()) {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("0x{code:08x}")
    }
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}
