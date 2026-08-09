// SPDX-License-Identifier: Apache-2.0

//! Stage 3 verification: framed transport over `adb forward`.
//!
//!   transport-probe [--frames N] [--flood] [--port P]
//!
//! Spawns `nc` on the device as a sink, forwards a port to it, and streams
//! synthetic frames shaped like real encoder output.
//!
//! **What this can and cannot measure.** Throughput and write back-pressure are
//! real. True one-way latency is *not* measurable here: without the device app
//! timestamping arrival, a fast `write()` only proves the kernel accepted the
//! bytes into a socket buffer, not that they reached the tablet. The transport
//! latency figure lands in Stage 4.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use transport::{adb, stream_header, Sender};

/// Device-side port `nc` listens on; the host port forwards to it.
const DEVICE_PORT: u16 = 27184;

const SINK_PATH: &str = "/data/local/tmp/moreland-sink.bin";

struct DeviceSink {
    child: Child,
    serial: String,
}

impl DeviceSink {
    fn spawn(serial: &str) -> Result<Self> {
        // Clear any listener left behind by an interrupted run.
        let _ = adb::shell(serial, "pkill -f 'nc -l'");
        let _ = adb::shell(serial, &format!("rm -f {SINK_PATH}"));
        std::thread::sleep(Duration::from_millis(300));

        // `sleep` holds nc's stdin open. Without it, toybox nc sees EOF on
        // stdin immediately (backgrounded `adb shell` has no tty), accepts the
        // connection, and closes it before reading a single byte — which looks
        // exactly like a working transport from the host side, because adb
        // happily drains everything written into a dead forward.
        let child = Command::new(std::env::var("MORELAND_ADB").unwrap_or_else(|_| "adb".into()))
            .args([
                "-s",
                serial,
                "shell",
                &format!("sleep 3600 | nc -l -p {DEVICE_PORT} > {SINK_PATH}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning device-side nc sink")?;
        std::thread::sleep(Duration::from_millis(900));
        Ok(Self {
            child,
            serial: serial.to_string(),
        })
    }

    /// Bytes that actually landed on the tablet.
    fn received_bytes(&self) -> Result<u64> {
        let out = adb::shell(
            &self.serial,
            &format!("stat -c %s {SINK_PATH} 2>/dev/null || echo 0"),
        )?;
        Ok(out.trim().parse().unwrap_or(0))
    }
}

impl Drop for DeviceSink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = adb::shell(&self.serial, "pkill -f 'nc -l'");
        let _ = adb::shell(&self.serial, &format!("rm -f {SINK_PATH}"));
    }
}

/// Frame sizes matching what Stage 2 actually produced at 1920x1200@60:
/// median ~32 KB, occasional ~108 KB keyframe.
fn synthetic_frame_size(index: usize) -> usize {
    if index % 600 == 0 {
        108_943
    } else if index % 7 == 0 {
        45_000
    } else {
        32_357
    }
}

fn main() -> Result<()> {
    let mut frames = 600usize;
    let mut flood = false;
    let mut port = protocol::DEFAULT_PORT;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--frames" => frames = it.next().and_then(|v| v.parse().ok()).unwrap_or(600),
            "--flood" => flood = true,
            "--port" => port = it.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            _ => {}
        }
    }

    let devices = adb::devices()?;
    let Some(device) = devices.first() else {
        bail!("no ADB device in `device` state — check `adb devices -l`");
    };
    println!("=== Device ===");
    println!(
        "  {} ({})",
        device.serial,
        device.model.clone().unwrap_or_else(|| "unknown".into())
    );

    let sink = DeviceSink::spawn(&device.serial)?;
    let _forward = adb::Forward::new(&device.serial, port, &format!("tcp:{DEVICE_PORT}"))?;
    println!("  forward     tcp:{port} -> tcp:{DEVICE_PORT}");

    let header = stream_header(1920, 1200, 60);
    let mut sender = Sender::connect(port, &header)?;
    println!("  connected, stream header sent");

    let payload = vec![0xA5u8; 128 * 1024];
    let mut write_times = Vec::with_capacity(frames);

    println!(
        "\n=== {} ===",
        if flood {
            "Flood: maximum throughput".to_string()
        } else {
            format!("Paced: {frames} frames at 60 fps")
        }
    );

    let frame_interval = Duration::from_nanos(1_000_000_000 / 60);
    let start = Instant::now();
    let mut next_deadline = start;

    for i in 0..frames {
        let size = synthetic_frame_size(i);
        let keyframe = i % 600 == 0;

        let write_start = Instant::now();
        sender.send_frame(&payload[..size], (i as u64) * 16_666_666, keyframe)?;
        write_times.push(write_start.elapsed());

        if !flood {
            next_deadline += frame_interval;
            let now = Instant::now();
            if next_deadline > now {
                std::thread::sleep(next_deadline - now);
            }
        }
    }
    let elapsed = start.elapsed();
    std::io::stdout().flush().ok();

    write_times.sort_unstable();
    let total = sender.bytes_sent();

    println!("\n  write() latency (host -> kernel socket buffer)");
    println!("    min       {:>8.3} ms", ms(write_times[0]));
    println!("    median    {:>8.3} ms", ms(write_times[write_times.len() / 2]));
    println!("    p95       {:>8.3} ms", ms(write_times[write_times.len() * 95 / 100]));
    println!("    max       {:>8.3} ms", ms(write_times[write_times.len() - 1]));

    println!("\n  throughput");
    println!("    frames    {}", sender.frames_sent());
    println!("    bytes     {:.1} MB", total as f64 / 1e6);
    println!("    wall      {:.2} s", elapsed.as_secs_f64());
    println!(
        "    rate      {:.2} MB/s ({:.1} Mbps)",
        total as f64 / elapsed.as_secs_f64() / 1e6,
        total as f64 * 8.0 / elapsed.as_secs_f64() / 1e6
    );
    if !flood {
        println!("    fps       {:.2}", frames as f64 / elapsed.as_secs_f64());
    }

    // Verify against the device, not against our own write() calls. A fast
    // write only proves the kernel took the bytes; only the tablet can say
    // they arrived.
    println!("\n  delivery verification");
    let mut received = 0;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(250));
        let now = sink.received_bytes()?;
        if now == received && now > 0 {
            break;
        }
        received = now;
    }
    let drain = start.elapsed();
    println!("    sent      {} bytes", total);
    println!("    received  {} bytes on device", received);
    if received == total {
        println!("    status    OK — byte-exact");
    } else {
        println!(
            "    status    MISMATCH — {} bytes missing ({:.1}%)",
            total.saturating_sub(received),
            (total.saturating_sub(received)) as f64 / total as f64 * 100.0
        );
    }
    println!(
        "    effective {:.2} MB/s including drain ({:.2} s total)",
        received as f64 / drain.as_secs_f64() / 1e6,
        drain.as_secs_f64()
    );

    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}
