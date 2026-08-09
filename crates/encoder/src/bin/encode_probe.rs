// SPDX-License-Identifier: Apache-2.0

//! Stage 2 verification: capture -> encode, end to end, measured.
//!
//!   encode-probe <output> [--frames N] [--bitrate KBPS]
//!                         [--rate-control cbr|vbr|cqp] [--out FILE]
//!
//! Capture and encode run concurrently. A serialised probe measures the sum of
//! both stages and reports half the real frame rate, because a hardware encoder
//! pipelines internally — submitting frame N and waiting for it before
//! capturing N+1 defeats that.

use anyhow::{Context, Result};
use capture::session::{BufferMode, Capture, CaptureConfig};
use capture::FALLBACK_MODIFIERS;
use encoder::{Encoder, EncoderConfig};
use std::collections::VecDeque;
use std::io::Write;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let mut output = None;
    let mut frames = 300usize;
    let mut bitrate = 20_000u32;
    let mut rate_control = "cbr".to_string();
    let mut out_path: Option<String> = None;
    let mut target_usage = 6u32;
    let mut cabac = true;
    let mut num_slices = 1u32;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => frames = it.next().and_then(|v| v.parse().ok()).unwrap_or(300),
            "--bitrate" => bitrate = it.next().and_then(|v| v.parse().ok()).unwrap_or(20_000),
            "--rate-control" => rate_control = it.next().unwrap_or_else(|| "cbr".into()),
            "--target-usage" => target_usage = it.next().and_then(|v| v.parse().ok()).unwrap_or(6),
            "--no-cabac" => cabac = false,
            "--slices" => num_slices = it.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            "--out" => out_path = it.next(),
            other => output = Some(other.to_string()),
        }
    }
    let output = output.context("usage: encode-probe <output> [--frames N]")?;

    let capture_config = CaptureConfig {
        mode: BufferMode::Dmabuf,
        pool_size: 3,
        allowed_modifiers: FALLBACK_MODIFIERS.to_vec(),
    };
    let mut capture = Capture::new(&output, &capture_config)?;

    println!("=== Capture ===");
    println!("  {}x{}  modifier 0x{:016x}", capture.width, capture.height, capture.modifier.unwrap_or(0));

    let encoder_config = EncoderConfig {
        width: capture.width,
        height: capture.height,
        framerate: 60,
        bitrate_kbps: bitrate,
        fourcc: capture.format,
        modifier: capture.modifier.unwrap_or(0),
        rate_control: rate_control.clone(),
        target_usage,
        cabac,
        num_slices,
        ..Default::default()
    };
    let encoder = Arc::new(Encoder::new(&encoder_config)?);

    println!("\n=== Encoder ===");
    println!(
        "  H.264 {rate_control}  {bitrate} kbps  target-usage {target_usage}  \
         cabac {cabac}  slices {num_slices}"
    );

    let frame_duration_ns = 1_000_000_000u64 / 60;
    // Match submissions to packets by *order*, not by PTS.
    //
    // GStreamer rewrites the timestamps on the way through — output PTS come
    // back offset by a constant ~3.6e15 ns running-time base — so keying on
    // PTS silently matches nothing. Order is reliable here precisely because
    // the encoder cannot emit B-frames, so nothing is ever reordered.
    let submitted: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));
    let done = Arc::new(AtomicBool::new(false));

    // Puller thread: drains encoded packets as the hardware produces them.
    let puller = {
        let encoder = Arc::clone(&encoder);
        let submitted = Arc::clone(&submitted);
        let done = Arc::clone(&done);
        let out_path = out_path.clone();
        std::thread::spawn(move || -> Result<(Vec<Duration>, Vec<usize>, usize)> {
            let mut file = match &out_path {
                Some(p) => Some(std::fs::File::create(p)?),
                None => None,
            };
            let mut latencies = Vec::new();
            let mut sizes = Vec::new();
            let mut keyframes = 0usize;

            while !done.load(Ordering::Relaxed) {
                match encoder.pull_packet(Duration::from_millis(100))? {
                    Some(packet) => {
                        if let Some(sent) = submitted.lock().unwrap().pop_front() {
                            latencies.push(sent.elapsed());
                        }
                        sizes.push(packet.data.len());
                        if packet.keyframe {
                            keyframes += 1;
                        }
                        if let Some(file) = file.as_mut() {
                            file.write_all(&packet.data)?;
                        }
                    }
                    None => continue,
                }
            }
            // Drain whatever is still in flight.
            while let Some(packet) = encoder.pull_packet(Duration::from_millis(200))? {
                if let Some(sent) = submitted.lock().unwrap().pop_front() {
                    latencies.push(sent.elapsed());
                }
                sizes.push(packet.data.len());
                if packet.keyframe {
                    keyframes += 1;
                }
                if let Some(file) = file.as_mut() {
                    file.write_all(&packet.data)?;
                }
            }
            Ok((latencies, sizes, keyframes))
        })
    };

    // Warm-up: encoder init and the first IDR are not representative.
    for i in 0..20u64 {
        let timing = capture.capture_frame()?;
        let dmabuf = capture.dmabuf(timing.buffer_index).unwrap();
        encoder.push_frame(dmabuf.planes[0].fd.as_fd(), i * frame_duration_ns)?;
    }
    std::thread::sleep(Duration::from_millis(200));
    submitted.lock().unwrap().clear();

    println!("\n=== Encoding {frames} frames ===");
    let wall_start = Instant::now();
    for i in 0..frames as u64 {
        let timing = capture.capture_frame()?;
        let dmabuf = capture
            .dmabuf(timing.buffer_index)
            .context("capture returned no DMA-BUF")?;
        let pts = (i + 100) * frame_duration_ns;
        submitted.lock().unwrap().push_back(Instant::now());
        encoder.push_frame(dmabuf.planes[0].fd.as_fd(), pts)?;
    }
    let wall = wall_start.elapsed();

    std::thread::sleep(Duration::from_millis(300));
    done.store(true, Ordering::Relaxed);
    let (mut latencies, sizes, keyframes) = puller
        .join()
        .map_err(|_| anyhow::anyhow!("puller thread panicked"))??;

    if sizes.is_empty() {
        anyhow::bail!("encoder produced no packets");
    }

    println!("\n  throughput");
    println!("    rate      {:>8.2} fps", frames as f64 / wall.as_secs_f64());
    println!("    packets   {} ({keyframes} keyframes)", sizes.len());

    if !latencies.is_empty() {
        latencies.sort_unstable();
        println!("\n  encode latency (push -> packet, pipelined)");
        println!("    min       {:>8.2} ms", ms(latencies[0]));
        println!("    median    {:>8.2} ms", ms(latencies[latencies.len() / 2]));
        println!("    p95       {:>8.2} ms", ms(latencies[latencies.len() * 95 / 100]));
        println!("    max       {:>8.2} ms", ms(latencies[latencies.len() - 1]));
    }

    let total: usize = sizes.iter().sum();
    let mut sorted = sizes.clone();
    sorted.sort_unstable();
    println!("\n  bitstream");
    println!(
        "    frame     min {} B, median {} B, max {} B",
        sorted[0],
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1]
    );
    println!(
        "    bitrate   {:.2} Mbps  ({:.1}% of measured 275 Mbps USB)",
        (total as f64 * 8.0) / wall.as_secs_f64() / 1e6,
        (total as f64 * 8.0) / wall.as_secs_f64() / 275e6 * 100.0
    );

    if let Some(path) = &out_path {
        println!("\nWrote {path}");
    }
    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
