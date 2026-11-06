// SPDX-License-Identifier: Apache-2.0

//! One streaming session: virtual output -> capture -> encode -> USB -> tablet.

use anyhow::{bail, Context, Result};
use capture::session::{BufferMode, Capture, CaptureConfig};
use encoder::{Encoder, EncoderConfig, PacketSource};
use std::collections::VecDeque;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use transport::{adb, stream_header, Sender};

use crate::display::{self, Layout};
use crate::output::{Placement, Side, VirtualOutput, FALLBACK_POSITION};
use crate::touch;

const APP_PACKAGE: &str = "com.moreland.display";
const APP_ACTIVITY: &str = "com.moreland.display/.DisplayActivity";

#[derive(Debug, Clone)]
pub struct Config {
    /// Explicit resolution. `None` matches the device's own panel aspect ratio.
    pub resolution: Option<(u32, u32)>,
    /// Cap on the auto-detected width, to keep encoder load sane.
    pub max_width: u32,
    pub fps: u32,
    /// `None` lets each pipeline pick its own default.
    pub bitrate_kbps: Option<u32>,
    /// Explicit corner of the virtual output. When neither is set the output
    /// goes against `side` of the main screen instead.
    pub position_x: Option<i32>,
    pub position_y: Option<i32>,
    /// Edge of the main screen to sit against; right when unset.
    pub side: Option<Side>,
    /// Output rotation, 1 = 90 degrees clockwise. Must be repeated in the
    /// monitor rule the daemon issues, or Hyprland resets it to 0.
    pub transform: i32,
    pub output_name: String,
    /// Composite the mouse cursor into the streamed frames.
    pub paint_cursor: bool,
    /// Ignore fingers while the pen is in range.
    pub palm_rejection: bool,
    /// Emit round-trip latency statistics on exit.
    pub stats: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            resolution: None,
            max_width: 1920,
            // Measured on Renoir + Pad 6 at 1920x1200, once capture and encode
            // were on the same GPU: median round trip 33.5 ms at 60, 25.5 ms at
            // 90, 18.8 ms at 120. Higher rates *lower* latency (a shorter frame
            // interval means less waiting for the next slot) and cost fewer
            // bytes, since smaller inter-frame deltas compress better. 144
            // saturates: 163 of 1764 frames unacked and a 208 ms stall.
            fps: 90,
            bitrate_kbps: None,
            // No explicit corner: KDE sits the output against `side` of the
            // main screen; elsewhere it goes below a primary at 0x0, at
            // y = 1080 (FALLBACK_POSITION). That assumes the primary is no
            // taller than 1080px. Override with `--position`/`--position-y`
            // if that does not hold for your layout.
            position_x: None,
            position_y: None,
            side: None,
            transform: 0,
            output_name: capture::VIRTUAL_OUTPUT_NAME.to_string(),
            paint_cursor: false,
            palm_rejection: true,
            stats: false,
        }
    }
}

impl Config {
    /// The output geometry for a session streaming at `mode`.
    ///
    /// Shared by the session and `--print-rule`, so the rule printed for the
    /// config file is built exactly like the one issued on connect.
    pub fn layout(&self, mode: (u32, u32)) -> Layout {
        Layout {
            name: self.output_name.clone(),
            mode,
            refresh: self.fps,
            position: match self.placement() {
                Placement::Coords(x, y) => (x, y),
                // Only KDE reads the live layout to honour a side.
                Placement::Side(_) => FALLBACK_POSITION,
            },
            scale: 1.0,
            transform: self.transform,
        }
    }

    /// Where the output goes. Explicit coordinates win; otherwise it sits
    /// against an edge of the main screen, on the right unless told
    /// differently. A missing axis of a partial pair keeps the legacy
    /// default (0 across, 1080 down).
    pub fn placement(&self) -> Placement {
        match (self.position_x, self.position_y) {
            (None, None) => Placement::Side(self.side.unwrap_or(Side::Right)),
            (x, y) => Placement::Coords(
                x.unwrap_or(FALLBACK_POSITION.0),
                y.unwrap_or(FALLBACK_POSITION.1),
            ),
        }
    }
}

/// The base GStreamer's video encoders add to every timestamp leaving them
/// (1000 hours, so a negative DTS cannot occur), in microseconds.
const ENCODER_PTS_BASE_US: u64 = 3_600_000_000_000;

/// Rounding slack when matching a packet to its frame: both sides are
/// truncated to microseconds.
const MATCH_SLACK_US: u64 = 1_000;

/// Packets to wait for before giving up on learning the encoder's shift.
const LEARN_LIMIT: usize = 30;

/// What is known about how the encoder shifted timestamps.
enum Shift {
    /// Packets seen so far, with when they arrived.
    Learning(Vec<(u64, Instant)>),
    /// packet timestamp + shift = frame timestamp.
    Known(i64),
    /// No single shift fitted; encode times are not reported.
    Unknown,
}

/// Timestamps of one frame on the host.
#[derive(Clone, Copy)]
struct FrameTrace {
    captured: Instant,
    pushed: Instant,
    encoded: Option<Instant>,
    sent: Option<Instant>,
}

/// Per-frame timing, recorded only when `--stats` asks for it so a normal
/// session pays nothing.
struct Trace {
    enabled: bool,
    /// Keyed by presentation timestamp in microseconds, as pushed into the
    /// encoder.
    frames: Mutex<std::collections::HashMap<u64, FrameTrace>>,
    shift: Mutex<Shift>,
    tablet: Mutex<Vec<protocol::Timing>>,
}

impl Trace {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            frames: Mutex::new(std::collections::HashMap::new()),
            shift: Mutex::new(Shift::Learning(Vec::new())),
            tablet: Mutex::new(Vec::new()),
        }
    }

    fn pushed(&self, pts_us: u64, captured: Instant) {
        if self.enabled {
            self.frames.lock().unwrap().insert(
                pts_us,
                FrameTrace {
                    captured,
                    pushed: Instant::now(),
                    encoded: None,
                    sent: None,
                },
            );
        }
    }

    /// Note that the encoder produced a packet and return the key of the frame
    /// it belongs to.
    ///
    /// The encoder re-stamps its output: on top of the fixed base, the
    /// timestamps come out shifted by a constant, the timestamp of the first
    /// frame that reached it (seen on KDE: every packet exactly 66.9 ms before
    /// its frame). Matching to the nearest frame instead pairs each packet with
    /// one from that far back and reports the shift as encode time, so the
    /// shift is learned from the first packets and then matched exactly.
    fn encoded(&self, pts_us: u64) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        let now = Instant::now();
        let packet = pts_us.saturating_sub(ENCODER_PTS_BASE_US);
        let mut frames = self.frames.lock().unwrap();
        let mut state = self.shift.lock().unwrap();
        let shift = match std::mem::replace(&mut *state, Shift::Unknown) {
            Shift::Known(shift) => {
                *state = Shift::Known(shift);
                shift
            }
            Shift::Unknown => return None,
            Shift::Learning(mut seen) => {
                seen.push((packet, now));
                let mut keys: Vec<u64> = frames
                    .iter()
                    .filter(|(_, f)| f.encoded.is_none())
                    .map(|(k, _)| *k)
                    .collect();
                keys.sort_unstable();
                let packets: Vec<u64> = seen.iter().map(|(p, _)| *p).collect();
                let Some(shift) = learn_shift(&keys, &packets) else {
                    if seen.len() < LEARN_LIMIT {
                        *state = Shift::Learning(seen);
                    }
                    return None;
                };
                *state = Shift::Known(shift);
                // Credit the packets that arrived while the shift was unknown.
                let mut last = None;
                for (packet, at) in seen {
                    last = match_frame(&mut frames, packet, shift, at);
                }
                return last;
            }
        };
        match_frame(&mut frames, packet, shift, now)
    }

    fn sent(&self, key: Option<u64>) {
        if let Some(key) = key.filter(|_| self.enabled) {
            if let Some(frame) = self.frames.lock().unwrap().get_mut(&key) {
                frame.sent = Some(Instant::now());
            }
        }
    }

    fn tablet(&self, timing: protocol::Timing) {
        if self.enabled {
            self.tablet.lock().unwrap().push(timing);
        }
    }
}

/// The one shift that maps every packet so far onto a pushed frame, once the
/// packets pin it down.
///
/// Frames arrive unevenly (KWin sends one only when the screen changes), so two
/// packets usually leave a single candidate. No shift at all is preferred when
/// it fits, which is what an encoder that keeps timestamps gives.
fn learn_shift(keys: &[u64], packets: &[u64]) -> Option<i64> {
    if packets.len() < 2 {
        return None;
    }
    let near = |at: i64| {
        let i = keys.partition_point(|&k| (k as i64) < at - MATCH_SLACK_US as i64);
        keys.get(i)
            .is_some_and(|&k| (k as i64).abs_diff(at) <= MATCH_SLACK_US)
    };
    let fits = |shift: i64| packets.iter().all(|&p| near(p as i64 + shift));
    if fits(0) {
        return Some(0);
    }
    let first = packets[0] as i64;
    let mut candidates = keys.iter().map(|&k| k as i64 - first).filter(|&s| fits(s));
    let shift = candidates.next()?;
    candidates.next().is_none().then_some(shift)
}

/// Mark the not yet matched frame at `packet + shift` as encoded at `at`.
fn match_frame(
    frames: &mut std::collections::HashMap<u64, FrameTrace>,
    packet: u64,
    shift: i64,
    at: Instant,
) -> Option<u64> {
    let target = packet as i64 + shift;
    let key = frames
        .iter()
        .filter(|(_, f)| f.encoded.is_none())
        .map(|(k, _)| *k)
        .filter(|k| (*k as i64).abs_diff(target) <= MATCH_SLACK_US)
        .min_by_key(|k| (*k as i64).abs_diff(target))?;
    frames.get_mut(&key)?.encoded = Some(at);
    Some(key)
}

/// Everything between the virtual output and the encoded packets: how frames
/// are captured and which encoder they feed. Compositors differ here, not in
/// the transport.
trait Pipeline {
    /// Size of the frames being streamed, as negotiated with the compositor.
    fn size(&self) -> (u32, u32);
    /// Where encoded packets come out.
    fn packets(&self) -> Arc<dyn PacketSource>;
    /// Frames the capture side discarded because encoding fell behind.
    fn dropped(&self) -> u64 {
        0
    }
    /// Capture one frame and submit it for encoding, returning whether one was
    /// submitted. `index` numbers submitted frames from zero; `frame_ns` is the
    /// nominal frame interval.
    fn step(
        &mut self,
        output: &mut VirtualOutput,
        index: u64,
        frame_ns: u64,
        trace: &Trace,
    ) -> Result<bool>;
}

/// `ext-image-copy-capture` into the zero-copy VA-API encoder. Hyprland, labwc.
struct ExtPipeline {
    capture: Capture,
    encoder: Arc<Encoder>,
}

impl ExtPipeline {
    fn open(output: &VirtualOutput, config: &Config) -> Result<Self> {
        // Probe what this machine's encoder can actually import rather than
        // assuming; the accepted modifier set is GPU-vendor specific.
        let allowed_modifiers = encoder::supported_modifiers(capture::XR24);
        tracing::debug!("encoder accepts modifiers {allowed_modifiers:02x?}");

        let capture = Capture::new(
            output.name(),
            &CaptureConfig {
                mode: BufferMode::Dmabuf,
                pool_size: 3,
                allowed_modifiers,
                paint_cursor: config.paint_cursor,
            },
        )?;
        let encoder = Arc::new(Encoder::new(&EncoderConfig {
            width: capture.width,
            height: capture.height,
            framerate: config.fps,
            bitrate_kbps: config.bitrate_kbps.unwrap_or(20_000),
            fourcc: capture.format,
            modifier: capture.modifier.unwrap_or(0),
            ..Default::default()
        })?);
        Ok(Self { capture, encoder })
    }
}

impl Pipeline for ExtPipeline {
    fn size(&self) -> (u32, u32) {
        (self.capture.width, self.capture.height)
    }

    fn packets(&self) -> Arc<dyn PacketSource> {
        self.encoder.clone()
    }

    fn step(
        &mut self,
        _output: &mut VirtualOutput,
        index: u64,
        frame_ns: u64,
        trace: &Trace,
    ) -> Result<bool> {
        let timing = self.capture.capture_frame()?;
        let captured = Instant::now();
        let dmabuf = self
            .capture
            .dmabuf(timing.buffer_index)
            .context("capture produced no DMA-BUF")?;
        self.encoder.push_frame(
            dmabuf.planes[0].fd.as_fd(),
            dmabuf.planes[0].offset,
            dmabuf.planes[0].stride,
            index * frame_ns,
        )?;
        trace.pushed(index * frame_ns / 1000, captured);
        Ok(true)
    }
}

/// KWin's PipeWire stream into a system-memory encoder (NVENC, else x264).
/// How long KWin may hold the stream paused before the session is given up.
/// Long enough to ride out a renegotiation, short enough that a dead stream
/// reads as a brief blank rather than a frozen tablet.
#[cfg(feature = "plasma")]
const STREAM_LOST_AFTER: Duration = Duration::from_secs(2);

#[cfg(feature = "plasma")]
struct PlasmaPipeline {
    capture: capture::plasma::stream::PwCapture,
    encoder: Arc<encoder::memory::MemoryEncoder>,
    size: (u32, u32),
}

#[cfg(feature = "plasma")]
impl PlasmaPipeline {
    /// How long to wait for KWin to announce the stream's format.
    const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

    fn open(output: &mut VirtualOutput, config: &Config) -> Result<Self> {
        use capture::plasma::stream::{Event, PwCapture};
        use encoder::memory::{Backend, MemoryEncoder, MemoryEncoderConfig};

        let node = output
            .pipewire_node()
            .context("the KDE output has no PipeWire node")?;
        let capture = PwCapture::start(node, output.pipewire_size())?;

        // The encoder needs the real frame size and pixel format, which only
        // the stream knows, so wait for it to say.
        let deadline = Instant::now() + Self::FORMAT_TIMEOUT;
        let format = loop {
            if !output.poll()? {
                bail!("KWin removed the virtual output while the stream was starting");
            }
            match capture.next_event(Duration::from_millis(50))? {
                Some(Event::Format(format)) => break format,
                _ if Instant::now() >= deadline => {
                    bail!("KWin did not announce a stream format within 5 s")
                }
                _ => {}
            }
        };

        let backend = match std::env::var("MORELAND_ENCODER") {
            Ok(name) => name.parse::<Backend>()?,
            Err(_) => Backend::detect().context(
                "no H.264 encoder available — install gst-plugins-bad (nvh264enc) or \
                 gst-plugins-ugly (x264enc)",
            )?,
        };
        tracing::info!("encoding with {}", backend.name());

        let name = format!("{:?}", format.format);
        let name = name.trim_start_matches("VideoFormat::");
        tracing::info!("KDE stream {}x{} {name}", format.width, format.height);
        let mut encoder_config =
            MemoryEncoderConfig::new(backend, format.width, format.height, config.fps);
        if let Some(bitrate) = config.bitrate_kbps {
            encoder_config.bitrate_kbps = bitrate;
        }
        let encoder = Arc::new(MemoryEncoder::new(&encoder_config, name)?);

        Ok(Self {
            capture,
            encoder,
            size: (format.width, format.height),
        })
    }
}

#[cfg(feature = "plasma")]
impl Pipeline for PlasmaPipeline {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn dropped(&self) -> u64 {
        self.capture.dropped()
    }

    fn packets(&self) -> Arc<dyn PacketSource> {
        self.encoder.clone()
    }

    fn step(
        &mut self,
        output: &mut VirtualOutput,
        _index: u64,
        _frame_ns: u64,
        trace: &Trace,
    ) -> Result<bool> {
        use capture::plasma::stream::Event;

        if !output.poll()? {
            bail!("KWin removed the virtual output");
        }
        if let Some(paused) = self.capture.paused_for() {
            if paused >= STREAM_LOST_AFTER {
                bail!(
                    "KWin stopped streaming the output (paused {:.1} s; setting it to \
                     mirror another screen does this); restarting",
                    paused.as_secs_f64()
                );
            }
        }
        // KWin sends a frame only when the screen changes, so a quiet desktop
        // legitimately yields nothing; the short timeout just keeps the
        // shutdown flag and the Wayland connection serviced.
        match self.capture.next_event(Duration::from_millis(20))? {
            Some(Event::Frame(frame)) => {
                let (pts_ns, captured) = (frame.pts.as_nanos() as u64, frame.captured_at);
                self.encoder.push_owned(frame.data, frame.stride, pts_ns)?;
                trace.pushed(pts_ns / 1000, captured);
                return Ok(true);
            }
            Some(Event::Format(format)) if (format.width, format.height) != self.size => {
                bail!(
                    "stream resized to {}x{} mid-session; restarting",
                    format.width,
                    format.height
                );
            }
            _ => {}
        }
        Ok(false)
    }
}

fn open_pipeline(output: &mut VirtualOutput, config: &Config) -> Result<Box<dyn Pipeline>> {
    #[cfg(feature = "plasma")]
    if output.pipewire_node().is_some() {
        return Ok(Box::new(PlasmaPipeline::open(output, config)?));
    }
    Ok(Box::new(ExtPipeline::open(output, config)?))
}

pub fn app_installed(serial: &str) -> Result<bool> {
    let out = adb::shell(serial, &format!("pm list packages {APP_PACKAGE}"))?;
    Ok(out.contains(APP_PACKAGE))
}

/// Stream until `shutdown` is set, the device vanishes, or the app disconnects.
///
/// Every resource here is RAII-scoped: the virtual output and the adb forward
/// are removed when this function returns, however it returns.
pub fn run(serial: &str, config: &Config, shutdown: &AtomicBool) -> Result<()> {
    if !app_installed(serial)? {
        bail!(
            "app not installed on the tablet.\n\
             Build it with `cd android && ANDROID_HOME=/opt/android-sdk ./gradlew assembleRelease`,\n\
             then `adb install -r android/app/build/outputs/apk/release/app-release.apk`."
        );
    }

    // Match the tablet's own aspect ratio unless told otherwise, so the image
    // fills its screen instead of letterboxing.
    let (width, height) = match config.resolution {
        Some(explicit) => explicit,
        None => {
            let panel = adb::display_size(serial)?;
            let scaled = display::panel_mode(panel, config.max_width);
            tracing::info!(
                "device panel {}x{} (landscape) -> streaming {}x{}",
                panel.0,
                panel.1,
                scaled.0,
                scaled.1
            );
            scaled
        }
    };

    let layout = config.layout((width, height));

    let mut output = VirtualOutput::create(&layout, config.placement(), config.paint_cursor)?;
    if output.applied_mode() {
        let (logical_width, logical_height) = layout.logical_size();
        tracing::info!(
            "virtual output {} at {}x{}@{} transform {} -> logical {}x{}",
            output.name(),
            width,
            height,
            config.fps,
            layout.transform,
            logical_width,
            logical_height
        );
    } else {
        tracing::info!(
            "using existing output {} with its own mode; encoding at whatever \
             size it reports",
            output.name()
        );
    }

    let forward = adb::Forward::new(
        serial,
        protocol::DEFAULT_PORT,
        &format!("localabstract:{}", protocol::SOCKET_NAME),
    )?;
    let port = forward.local_port()?;

    // The abstract socket name is process-wide on Android, so a stale instance
    // from a previous run would keep it bound and answer with a dead surface.
    let _ = adb::shell(serial, &format!("am force-stop {APP_PACKAGE}"));
    std::thread::sleep(Duration::from_millis(500));

    // Wake the tablet first. A sleeping or locked device starts the activity
    // without ever making it visible, so no surface is created and the session
    // dies with "no valid surface available" — which looks like a crash but is
    // just a dark screen. The manifest's showWhenLocked/turnScreenOn only help
    // once the activity is actually being brought up.
    let _ = adb::shell(serial, "input keyevent KEYCODE_WAKEUP");
    let _ = adb::shell(serial, "wm dismiss-keyguard");
    std::thread::sleep(Duration::from_millis(400));

    adb::shell(serial, &format!("am start -n {APP_ACTIVITY}"))
        .context("launching the display app")?;
    std::thread::sleep(Duration::from_millis(2500));

    let mut pipeline = open_pipeline(&mut output, config)?;
    let (frame_width, frame_height) = pipeline.size();
    let packets = pipeline.packets();

    let mut sender = Sender::connect(
        port,
        &stream_header(frame_width, frame_height, config.fps),
    )
    .context("connecting to the app — is it in the foreground on the tablet?")?;
    tracing::info!("streaming to {serial}");

    let running = Arc::new(AtomicBool::new(true));
    let sent_at: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));
    let round_trips: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
    let trace = Arc::new(Trace::new(config.stats));

    // Touch is a bonus: a session without it still streams the picture.
    let mut remote_input = match output.kwin_connector() {
        Some(connector) => match touch::create_mapped(&connector, config.palm_rejection) {
            Ok(remote) => Some(remote),
            Err(e) => {
                tracing::warn!("touch and pen disabled: {e:#}");
                None
            }
        },
        None => None,
    };

    let ack_thread = {
        let mut reader = sender.ack_reader()?;
        let sent_at = Arc::clone(&sent_at);
        let round_trips = Arc::clone(&round_trips);
        let running = Arc::clone(&running);
        let trace = Arc::clone(&trace);
        std::thread::spawn(move || {
            let mut parser = protocol::MessageParser::new();
            let mut buf = [0u8; 512];
            while running.load(Ordering::Relaxed) {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        continue
                    }
                    Err(_) => break,
                };
                let messages = match parser.push(&buf[..n]) {
                    Ok(messages) => messages,
                    Err(e) => {
                        tracing::warn!("device sent garbage ({e}); closing the link");
                        break;
                    }
                };
                for message in messages {
                    match message {
                        protocol::DeviceMessage::Ack { .. } => {
                            if let Some(sent) = sent_at.lock().unwrap().pop_front() {
                                round_trips.lock().unwrap().push(sent.elapsed());
                            }
                        }
                        protocol::DeviceMessage::Timing(timing) => trace.tablet(timing),
                        protocol::DeviceMessage::Pointer(pointer) => {
                            if let Some(remote) = remote_input.as_mut() {
                                if let Err(e) = remote.handle(&pointer) {
                                    tracing::warn!("touch input failed: {e:#}");
                                }
                            }
                        }
                    }
                }
            }
            // A touch or pen still down when the link drops would stay stuck.
            if let Some(remote) = remote_input.as_mut() {
                let _ = remote.release_all();
            }
        })
    };

    // The thread that pulls packets out of the encoder also sends them. They
    // used to be handed back to the capture loop first, which only looked for
    // them between frames — and waiting for the next frame can take a whole
    // frame interval or more on a quiet desktop, so a finished packet sat idle
    // for that long. Sending from here puts it on the wire the instant the
    // encoder produces it.
    let link_down = Arc::new(AtomicBool::new(false));
    let drain_thread = {
        let running = Arc::clone(&running);
        let link_down = Arc::clone(&link_down);
        let sent_at = Arc::clone(&sent_at);
        let trace = Arc::clone(&trace);
        std::thread::spawn(move || -> (Sender, Result<()>) {
            let outcome = (|| -> Result<()> {
                while running.load(Ordering::Relaxed) {
                    match packets.pull_packet(Duration::from_millis(100))? {
                        Some(p) => {
                            let key = trace.encoded(p.pts_ns / 1000);
                            sent_at.lock().unwrap().push_back(Instant::now());
                            sender.send_frame(&p.data, p.pts_ns, p.keyframe)?;
                            trace.sent(key);
                        }
                        None => continue,
                    }
                }
                Ok(())
            })();
            if outcome.is_err() {
                link_down.store(true, Ordering::Relaxed);
            }
            (sender, outcome)
        })
    };

    let frame_duration_ns = 1_000_000_000u64 / u64::from(config.fps);
    let mut index = 0u64;
    let result = (|| -> Result<()> {
        while !shutdown.load(Ordering::Relaxed) && !link_down.load(Ordering::Relaxed) {
            if pipeline.step(&mut output, index, frame_duration_ns, &trace)? {
                index += 1;
            }
        }
        Ok(())
    })();

    let dropped = pipeline.dropped();
    running.store(false, Ordering::Relaxed);
    let (sender, sent) = drain_thread
        .join()
        .map_err(|_| anyhow::anyhow!("the packet thread panicked"))?;
    let _ = ack_thread.join();
    // A broken link is the more useful error: it is why the loop stopped.
    let result = sent.and(result);

    if config.stats {
        report(
            &round_trips.lock().unwrap(),
            index,
            sender.bytes_sent(),
            dropped,
            &trace,
        );
    }

    // Leaving the app in the foreground with a dead socket is confusing; send
    // it home so the tablet returns to a normal state.
    let _ = adb::shell(serial, &format!("am force-stop {APP_PACKAGE}"));

    drop(forward);
    drop(output);
    result
}

fn report(trips: &[Duration], frames: u64, bytes: u64, dropped: u64, trace: &Trace) {
    println!("\n  frames captured   {frames}");
    println!("  bytes sent        {:.1} MB", bytes as f64 / 1e6);
    println!("  acks received     {}", trips.len());
    if dropped > 0 {
        println!("  frames dropped    {dropped}  (queue full; mostly at startup)");
    }
    if trips.is_empty() {
        println!("\n  no acknowledgements — check `adb logcat -s Moreland`");
        return;
    }
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let mut round: Vec<f64> = trips.iter().map(|d| ms(*d)).collect();

    println!("\n  where the time goes                   median      p95      max");
    row("host: capture -> encoder in", &mut stage(trace, |f| Some((f.pushed - f.captured).as_secs_f64())));
    row("host: encode", &mut stage(trace, |f| f.encoded.map(|e| (e - f.pushed).as_secs_f64())));
    row("host: socket write", &mut stage(trace, |f| match (f.encoded, f.sent) {
        (Some(e), Some(s)) => Some((s - e).as_secs_f64()),
        _ => None,
    }));
    row("round trip: send -> tablet -> ack", &mut round);

    let tablet = trace.tablet.lock().unwrap();
    if tablet.is_empty() {
        println!("\n  (no tablet timings: is the app up to date?)");
        return;
    }
    let col = |f: fn(&protocol::Timing) -> u32| -> Vec<f64> {
        tablet.iter().map(|t| f64::from(f(t)) / 1000.0).collect()
    };
    println!("\n  on the tablet");
    row("wait for a decoder input buffer", &mut col(|t| t.queue_wait_us));
    row("decode", &mut col(|t| t.decode_us));
    row("wait for the screen refresh", &mut col(|t| t.display_wait_us));

    // The interval between frames reaching the glass is the stutter measure.
    let mut intervals: Vec<f64> = tablet
        .iter()
        .filter(|t| t.render_interval_us > 0)
        .map(|t| f64::from(t.render_interval_us) / 1000.0)
        .collect();
    if intervals.len() > 10 {
        let mut sorted = intervals.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let typical = sorted[sorted.len() / 2];
        let late = intervals.iter().filter(|v| **v > typical * 1.5).count();
        let early = intervals.iter().filter(|v| **v < typical * 0.5).count();
        println!("\n  frames reaching the screen, spacing");
        row("interval between frames", &mut intervals);
        println!(
            "    typical {typical:.1} ms; {late} late (>1.5x), {early} bunched (<0.5x) of {} \
             ({:.1}% uneven)",
            sorted.len(),
            100.0 * (late + early) as f64 / sorted.len() as f64
        );
    }
}

/// One stage's durations across all fully traced frames, in milliseconds.
fn stage(trace: &Trace, pick: impl Fn(&FrameTrace) -> Option<f64>) -> Vec<f64> {
    trace
        .frames
        .lock()
        .unwrap()
        .values()
        .filter_map(|f| pick(f).map(|secs| secs * 1000.0))
        .collect()
}

fn row(label: &str, values: &mut Vec<f64>) {
    if values.is_empty() {
        println!("    {label:<34}  (no data)");
        return;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| values[((values.len() - 1) as f64 * q) as usize];
    println!(
        "    {label:<34} {:>7.2}  {:>7.2}  {:>7.2}",
        at(0.5),
        at(0.95),
        values[values.len() - 1]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames pushed and packets out in a real KDE session at 120 fps: every
    /// packet came out exactly 66 881 us before its frame.
    const PUSHED: [u64; 6] = [33_105, 50_247, 66_881, 488_384, 497_436, 507_447];

    #[test]
    fn learns_the_shift_two_packets_pin_down() {
        assert_eq!(learn_shift(&PUSHED, &[0, 421_503]), Some(66_881));
    }

    #[test]
    fn one_packet_is_not_enough() {
        assert_eq!(learn_shift(&PUSHED, &[0]), None);
    }

    #[test]
    fn an_encoder_that_keeps_timestamps_needs_no_shift() {
        assert_eq!(learn_shift(&[0, 8_333, 16_667], &[0, 8_333]), Some(0));
    }

    #[test]
    fn packets_are_matched_to_their_own_frames() {
        let trace = Trace::new(true);
        let now = Instant::now();
        for key in PUSHED {
            trace.pushed(key, now);
        }
        // Learning: the first packet alone cannot be placed.
        assert_eq!(trace.encoded(ENCODER_PTS_BASE_US), None);
        assert_eq!(trace.encoded(ENCODER_PTS_BASE_US + 421_503), Some(488_384));
        assert_eq!(trace.encoded(ENCODER_PTS_BASE_US + 430_555), Some(497_436));
        // The packet seen while learning was credited to its frame afterwards.
        let frames = trace.frames.lock().unwrap();
        assert!(frames[&66_881].encoded.is_some());
        assert!(frames[&33_105].encoded.is_none());
    }
}
