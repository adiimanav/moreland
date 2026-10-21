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
use crate::output::VirtualOutput;
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
    pub position_x: i32,
    pub position_y: i32,
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
            // Below a primary at 0x0, starting at y = 1080. This assumes the
            // primary is no taller than 1080px — on a taller display the
            // virtual output would overlap it rather than sit below. Override
            // with `--position`/`--position-y` if that does not hold for your
            // layout.
            position_x: 0,
            position_y: 1080,
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
            position: (self.position_x, self.position_y),
            scale: 1.0,
            transform: self.transform,
        }
    }
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
    fn step(&mut self, output: &mut VirtualOutput, index: u64, frame_ns: u64) -> Result<bool>;
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

    fn step(&mut self, _output: &mut VirtualOutput, index: u64, frame_ns: u64) -> Result<bool> {
        let timing = self.capture.capture_frame()?;
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
        Ok(true)
    }
}

/// KWin's PipeWire stream into a system-memory encoder (NVENC, else x264).
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
        let capture = PwCapture::start(node)?;

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

    fn step(&mut self, output: &mut VirtualOutput, _index: u64, _frame_ns: u64) -> Result<bool> {
        use capture::plasma::stream::Event;

        if !output.poll()? {
            bail!("KWin removed the virtual output");
        }
        // KWin sends a frame only when the screen changes, so a quiet desktop
        // legitimately yields nothing; the short timeout just keeps the
        // shutdown flag and the Wayland connection serviced.
        match self.capture.next_event(Duration::from_millis(20))? {
            Some(Event::Frame(frame)) => {
                self.encoder
                    .push_frame(&frame.data, frame.stride, frame.pts.as_nanos() as u64)?;
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

    let mut output = VirtualOutput::create(&layout, config.paint_cursor)?;
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
        std::thread::spawn(move || -> (Sender, Result<()>) {
            let outcome = (|| -> Result<()> {
                while running.load(Ordering::Relaxed) {
                    match packets.pull_packet(Duration::from_millis(100))? {
                        Some(p) => {
                            sent_at.lock().unwrap().push_back(Instant::now());
                            sender.send_frame(&p.data, p.pts_ns, p.keyframe)?;
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
            if pipeline.step(&mut output, index, frame_duration_ns)? {
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
        report(&round_trips.lock().unwrap(), index, sender.bytes_sent(), dropped);
    }

    // Leaving the app in the foreground with a dead socket is confusing; send
    // it home so the tablet returns to a normal state.
    let _ = adb::shell(serial, &format!("am force-stop {APP_PACKAGE}"));

    drop(forward);
    drop(output);
    result
}

fn report(trips: &[Duration], frames: u64, bytes: u64, dropped: u64) {
    println!("\n  frames captured   {frames}");
    println!("  bytes sent        {:.1} MB", bytes as f64 / 1e6);
    println!("  acks received     {}", trips.len());
    if dropped > 0 {
        println!("  frames dropped    {dropped}  (encoder could not keep up)");
    }
    if trips.is_empty() {
        println!("\n  no acknowledgements — check `adb logcat -s Moreland`");
        return;
    }
    let mut trips = trips.to_vec();
    trips.sort_unstable();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!("\n  round trip: host send -> device render -> host ack");
    println!("    min       {:>8.2} ms", ms(trips[0]));
    println!("    median    {:>8.2} ms", ms(trips[trips.len() / 2]));
    println!("    p95       {:>8.2} ms", ms(trips[trips.len() * 95 / 100]));
    println!("    max       {:>8.2} ms", ms(trips[trips.len() - 1]));
}
