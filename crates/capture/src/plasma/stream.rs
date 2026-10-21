// SPDX-License-Identifier: Apache-2.0

//! KDE Plasma capture backend — PipeWire consumer half.
//!
//! [`super::PlasmaVirtualOutput`] creates the monitor and hands back a
//! PipeWire node id; this module connects to that node and delivers its frames.
//!
//! Frames arrive as CPU-mapped memory (SHM/memfd). That is deliberate for now:
//! it needs no modifier negotiation, and it is exactly what a non-VA-API
//! encoder such as NVENC wants. A DMA-BUF path for the zero-copy VA-API
//! encoder is a later optimisation, not a prerequisite.
//!
//! PipeWire owns its own main loop, so it runs on a dedicated thread and
//! frames cross to the caller over a bounded channel. The channel is
//! deliberately tiny and drops on overflow: a stale frame is worse than a
//! missing one, the same policy `crates/encoder` applies at its `appsrc`.

use anyhow::{anyhow, Context, Result};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::pod::Pod;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Frames buffered between the PipeWire thread and the consumer.
const QUEUE_DEPTH: usize = 2;

/// Largest frame either dimension may negotiate to.
const MAX_DIMENSION: u32 = 8192;

/// What the stream negotiated. Sent once before the first frame, and again if
/// the compositor renegotiates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    pub width: u32,
    pub height: u32,
    /// SPA video format, e.g. `BGRx`.
    pub format: spa::param::video::VideoFormat,
    pub framerate_num: u32,
    pub framerate_denom: u32,
}

/// One captured frame, copied out of PipeWire's buffer.
pub struct Frame {
    pub format: StreamFormat,
    pub stride: u32,
    /// Time since the stream started; stands in for a presentation timestamp.
    pub pts: Duration,
    pub data: Vec<u8>,
}

pub enum Event {
    Format(StreamFormat),
    Frame(Frame),
    /// The stream failed. No further events follow.
    Error(String),
}

struct Shared {
    format: Option<StreamFormat>,
    events: SyncSender<Event>,
    started: Instant,
    /// Frames dropped because the consumer was behind.
    dropped: Arc<AtomicU64>,
}

/// A running capture of one PipeWire node.
pub struct PwCapture {
    events: Receiver<Event>,
    dropped: Arc<AtomicU64>,
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl PwCapture {
    /// Connect to `node_id` and start delivering frames.
    pub fn start(node_id: u32) -> Result<Self> {
        let (tx, events) = sync_channel(QUEUE_DEPTH);
        let dropped = Arc::new(AtomicU64::new(0));
        let (quit, quit_rx) = pw::channel::channel::<()>();
        // The loop reports whether it could connect at all before the caller
        // gets a handle, so a bad node id fails here rather than as silence.
        let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);

        let dropped_for_thread = Arc::clone(&dropped);
        let thread = std::thread::Builder::new()
            .name("pipewire".into())
            .spawn(move || {
                if let Err(e) = run(node_id, tx.clone(), quit_rx, &ready_tx, dropped_for_thread) {
                    let msg = format!("{e:#}");
                    // `ready_tx` is only read once; a late error goes on the
                    // event channel instead.
                    if ready_tx.try_send(Err(msg.clone())).is_err() {
                        let _ = tx.try_send(Event::Error(msg));
                    }
                }
            })
            .context("spawning the PipeWire thread")?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                events,
                dropped,
                quit: Some(quit),
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(anyhow!("connecting to PipeWire node {node_id}: {e}")),
            Err(_) => Err(anyhow!(
                "timed out connecting to PipeWire node {node_id}; is pipewire running?"
            )),
        }
    }

    /// Frames discarded because the consumer had not taken the previous ones.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Wait up to `timeout` for the next event.
    pub fn next_event(&self, timeout: Duration) -> Result<Option<Event>> {
        match self.events.recv_timeout(timeout) {
            Ok(Event::Error(e)) => Err(anyhow!("PipeWire stream failed: {e}")),
            Ok(event) => Ok(Some(event)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("PipeWire thread exited")),
        }
    }
}

impl Drop for PwCapture {
    fn drop(&mut self) {
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    node_id: u32,
    events: SyncSender<Event>,
    quit_rx: pw::channel::Receiver<()>,
    ready: &SyncSender<Result<(), String>>,
    dropped: Arc<AtomicU64>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("creating the main loop")?;
    let _quit = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });

    let context = pw::context::ContextRc::new(&mainloop, None).context("creating context")?;
    let core = context
        .connect_rc(None)
        .context("connecting to the PipeWire daemon")?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "moreland-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("creating the stream")?;

    let shared = Shared {
        format: None,
        events,
        started: Instant::now(),
        dropped,
    };

    let _listener = stream
        .add_local_listener_with_user_data(shared)
        .state_changed(|_, shared, _old, new| {
            if let pw::stream::StreamState::Error(msg) = new {
                let _ = shared.events.try_send(Event::Error(msg));
            }
        })
        .param_changed(|_, shared, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, subtype)) = spa::param::format_utils::parse_format(param) else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video
                || subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            let format = StreamFormat {
                width: info.size().width,
                height: info.size().height,
                format: info.format(),
                framerate_num: info.framerate().num,
                framerate_denom: info.framerate().denom,
            };
            shared.format = Some(format);
            let _ = shared.events.try_send(Event::Format(format));
        })
        .process(|stream, shared| {
            // Always dequeue, even with nowhere to put the frame: an unreturned
            // buffer starves the producer.
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(format) = shared.format else { return };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };

            let (offset, size, stride) = {
                let chunk = data.chunk();
                (
                    chunk.offset() as usize,
                    chunk.size() as usize,
                    chunk.stride().max(0) as u32,
                )
            };
            // A zero-size chunk marks a buffer carrying no new picture.
            if size == 0 {
                return;
            }
            let Some(bytes) = data.data() else { return };
            let Some(pixels) = bytes.get(offset..offset + size) else {
                return;
            };

            let frame = Frame {
                format,
                stride,
                pts: shared.started.elapsed(),
                data: pixels.to_vec(),
            };
            match shared.events.try_send(Event::Frame(frame)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    shared.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        })
        .register()
        .context("registering the stream listener")?;

    let values = format_offer().context("building the format offer")?;
    let mut params = [Pod::from_bytes(&values).context("format offer is not a valid pod")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("connecting the stream")?;

    let _ = ready.try_send(Ok(()));
    mainloop.run();
    Ok(())
}

/// What we accept. No modifier is offered, which is what steers the producer
/// to plain memory buffers instead of DMA-BUFs.
fn format_offer() -> Result<Vec<u8>> {
    use spa::param::video::VideoFormat;

    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1200
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: MAX_DIMENSION,
                height: MAX_DIMENSION
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );

    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map_err(|e| anyhow!("serialising the format offer: {e:?}"))?
    .0
    .into_inner();
    Ok(bytes)
}
