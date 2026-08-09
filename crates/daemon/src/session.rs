// SPDX-License-Identifier: Apache-2.0

//! One streaming session: virtual output -> capture -> encode -> USB -> tablet.

use anyhow::{bail, Context, Result};
use capture::session::{BufferMode, Capture, CaptureConfig};
use encoder::{Encoder, EncoderConfig};
use std::collections::VecDeque;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use transport::{adb, stream_header, Sender};

use crate::output::VirtualOutput;

const APP_PACKAGE: &str = "com.moreland.display";
const APP_ACTIVITY: &str = "com.moreland.display/.DisplayActivity";

#[derive(Debug, Clone)]
pub struct Config {
    /// Explicit resolution. `None` matches the device's own panel aspect ratio.
    pub resolution: Option<(u32, u32)>,
    /// Cap on the auto-detected width, to keep encoder load sane.
    pub max_width: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub position_x: i32,
    pub position_y: i32,
    pub output_name: String,
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
            bitrate_kbps: 20_000,
            // Left of a primary at 0x0: the output spans [-width, 0), so the
            // origin is -width. Negative because Hyprland places the primary
            // at 0x0 and grows the layout both ways from there. Override with
            // `--position` to sit on the right instead.
            position_x: -1920,
            position_y: 0,
            output_name: capture::VIRTUAL_OUTPUT_NAME.to_string(),
            stats: false,
        }
    }
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
            let scaled = adb::stream_resolution(panel, config.max_width);
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

    let output = VirtualOutput::create(
        &config.output_name,
        width,
        height,
        config.fps,
        config.position_x,
        config.position_y,
    )?;
    tracing::info!(
        "virtual output {} at {}x{}@{}",
        output.name(),
        width,
        height,
        config.fps
    );

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

    // Probe what this machine's encoder can actually import rather than
    // assuming; the accepted modifier set is GPU-vendor specific.
    let allowed_modifiers = encoder::supported_modifiers(capture::XR24);
    tracing::debug!("encoder accepts modifiers {allowed_modifiers:02x?}");

    let mut capture = Capture::new(
        output.name(),
        &CaptureConfig {
            mode: BufferMode::Dmabuf,
            pool_size: 3,
            allowed_modifiers,
        },
    )?;
    let encoder = Arc::new(Encoder::new(&EncoderConfig {
        width: capture.width,
        height: capture.height,
        framerate: config.fps,
        bitrate_kbps: config.bitrate_kbps,
        fourcc: capture.format,
        modifier: capture.modifier.unwrap_or(0),
        ..Default::default()
    })?);

    let mut sender = Sender::connect(
        port,
        &stream_header(capture.width, capture.height, config.fps),
    )
    .context("connecting to the app — is it in the foreground on the tablet?")?;
    tracing::info!("streaming to {serial}");

    let running = Arc::new(AtomicBool::new(true));
    let sent_at: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));
    let round_trips: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));

    let ack_thread = {
        let mut reader = sender.ack_reader()?;
        let sent_at = Arc::clone(&sent_at);
        let round_trips = Arc::clone(&round_trips);
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            let mut buf = [0u8; protocol::ACK_LEN];
            let mut filled = 0usize;
            while running.load(Ordering::Relaxed) {
                match reader.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => {
                        filled += n;
                        if filled == protocol::ACK_LEN {
                            filled = 0;
                            if let Some(sent) = sent_at.lock().unwrap().pop_front() {
                                round_trips.lock().unwrap().push(sent.elapsed());
                            }
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
            }
        })
    };

    let (packet_tx, packet_rx) = std::sync::mpsc::channel::<(Vec<u8>, u64, bool)>();
    let drain_thread = {
        let encoder = Arc::clone(&encoder);
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match encoder.pull_packet(Duration::from_millis(100)) {
                    Ok(Some(p)) => {
                        if packet_tx.send((p.data, p.pts_ns, p.keyframe)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::error!("encoder: {e}");
                        break;
                    }
                }
            }
        })
    };

    let frame_duration_ns = 1_000_000_000u64 / u64::from(config.fps);
    let mut index = 0u64;
    let result = (|| -> Result<()> {
        while !shutdown.load(Ordering::Relaxed) {
            let timing = capture.capture_frame()?;
            let dmabuf = capture
                .dmabuf(timing.buffer_index)
                .context("capture produced no DMA-BUF")?;
            encoder.push_frame(dmabuf.planes[0].fd.as_fd(), index * frame_duration_ns)?;
            index += 1;

            while let Ok((data, pts, keyframe)) = packet_rx.try_recv() {
                sent_at.lock().unwrap().push_back(Instant::now());
                sender.send_frame(&data, pts, keyframe)?;
            }
        }
        Ok(())
    })();

    running.store(false, Ordering::Relaxed);
    let _ = drain_thread.join();
    let _ = ack_thread.join();

    if config.stats {
        report(&round_trips.lock().unwrap(), index, sender.bytes_sent());
    }

    // Leaving the app in the foreground with a dead socket is confusing; send
    // it home so the tablet returns to a normal state.
    let _ = adb::shell(serial, &format!("am force-stop {APP_PACKAGE}"));

    drop(forward);
    drop(output);
    result
}

fn report(trips: &[Duration], frames: u64, bytes: u64) {
    println!("\n  frames captured   {frames}");
    println!("  bytes sent        {:.1} MB", bytes as f64 / 1e6);
    println!("  acks received     {}", trips.len());
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
