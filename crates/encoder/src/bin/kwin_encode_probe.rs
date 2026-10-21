// SPDX-License-Identifier: Apache-2.0

//! Stage 6 verification: KDE virtual output -> PipeWire -> H.264 file.
//!
//!   kwin-encode-probe [--backend nvenc|x264] [--seconds N] [--out FILE]
//!                     [--fps N] [--bitrate KBPS]
//!
//! Exercises capture and encode together, without the tablet. The result plays
//! in any H.264-capable player: `mpv out.h264`. Drag a window onto the new
//! monitor while it runs, or the file will be nearly empty.

use anyhow::{Context, Result};
use capture::plasma::stream::{Event, PwCapture};
use capture::plasma::{CursorMode, PlasmaVirtualOutput};
use encoder::memory::{Backend, MemoryEncoder, MemoryEncoderConfig};
use std::io::Write;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut seconds = 10u64;
    let mut out_path = "kwin-probe.h264".to_string();
    let mut backend = Backend::detect().context("no H.264 encoder element installed")?;
    let (mut fps, mut bitrate) = (60u32, encoder::memory::DEFAULT_BITRATE_KBPS);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut next = || args.next().context("missing value");
        match arg.as_str() {
            "--seconds" => seconds = next()?.parse().context("--seconds")?,
            "--out" => out_path = next()?,
            "--backend" => backend = next()?.parse()?,
            "--fps" => fps = next()?.parse().context("--fps")?,
            "--bitrate" => bitrate = next()?.parse().context("--bitrate")?,
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }

    let mut output = PlasmaVirtualOutput::create(
        "moreland",
        "Moreland tablet monitor",
        1920,
        1200,
        1.0,
        CursorMode::Embedded,
    )?;
    let capture = PwCapture::start(output.node_id())?;
    println!("backend {}, writing {out_path}", backend.name());

    let mut file = std::fs::File::create(&out_path).context("creating the output file")?;
    let mut encoder: Option<MemoryEncoder> = None;

    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let (mut frames_in, mut packets, mut bytes) = (0u64, 0u64, 0u64);
    let mut push_cost = Duration::ZERO;

    while Instant::now() < deadline {
        if !output.poll()? {
            println!("KWin closed the stream early");
            break;
        }
        match capture.next_event(Duration::from_millis(20))? {
            Some(Event::Format(f)) => {
                // BGRx is what KWin gave us; anything else needs a new offer.
                let name = format!("{:?}", f.format);
                let name = name.trim_start_matches("VideoFormat::").to_string();
                println!("format {}x{} {name}", f.width, f.height);
                encoder = Some(MemoryEncoder::new(
                    &{
                        let mut c = MemoryEncoderConfig::new(backend, f.width, f.height, fps);
                        c.bitrate_kbps = bitrate;
                        c
                    },
                    &name,
                )?);
            }
            Some(Event::Frame(frame)) => {
                frames_in += 1;
                if let Some(enc) = &encoder {
                    let t = Instant::now();
                    enc.push_frame(&frame.data, frame.stride, frame.pts.as_nanos() as u64)?;
                    push_cost += t.elapsed();
                }
            }
            Some(Event::Error(e)) => anyhow::bail!("{e}"),
            None => {}
        }
        if let Some(enc) = &encoder {
            while let Some(p) = enc.pull_packet(Duration::ZERO)? {
                packets += 1;
                bytes += p.data.len() as u64;
                file.write_all(&p.data)?;
            }
        }
    }

    println!(
        "{frames_in} frames in, {packets} packets out, {:.2} MB, {:.1}s",
        bytes as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );
    if frames_in > 0 {
        println!(
            "mean push_frame cost {:.2} ms",
            push_cost.as_secs_f64() * 1000.0 / frames_in as f64
        );
    }
    drop(encoder);
    drop(capture);
    drop(output);
    Ok(())
}
