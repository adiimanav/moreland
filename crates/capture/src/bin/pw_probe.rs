// SPDX-License-Identifier: Apache-2.0

//! Stage 6 verification: consume the PipeWire stream of a KDE virtual output.
//!
//!   pw-probe [--width PX] [--height PX] [--seconds N] [--mirror NAME]
//!
//! Creates the monitor with [`PlasmaVirtualOutput`], connects to its PipeWire
//! node, and reports the negotiated format and the delivered frame rate. It
//! encodes nothing and sends nothing.
//!
//! An idle compositor sends few frames: KWin only produces one when the
//! output changes. Drag a window onto the new monitor to see the full rate.

use anyhow::{Context, Result};
use capture::plasma::stream::{Event, PwCapture};
use capture::plasma::{CursorMode, PlasmaVirtualOutput};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let (mut width, mut height) = (1920i32, 1200i32);
    let mut seconds = 10u64;
    let mut mirror: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut next = || args.next().context("missing value");
        match arg.as_str() {
            "--width" => width = next()?.parse().context("--width")?,
            "--height" => height = next()?.parse().context("--height")?,
            "--seconds" => seconds = next()?.parse().context("--seconds")?,
            "--mirror" => mirror = Some(next()?),
            "--help" | "-h" => {
                println!("{}", env!("CARGO_BIN_NAME"));
                println!("  --width PX --height PX --seconds N --mirror NAME");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }

    let mut output = match &mirror {
        Some(name) => PlasmaVirtualOutput::mirror(name, CursorMode::Embedded)?,
        None => PlasmaVirtualOutput::create(
            "moreland",
            "Moreland tablet monitor",
            width,
            height,
            1.0,
            CursorMode::Embedded,
        )?,
    };
    println!("pipewire node {}", output.node_id());

    let capture = PwCapture::start(output.node_id())?;

    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let (mut frames, mut bytes) = (0u64, 0u64);
    let mut last_report = started;
    let mut frames_at_report = 0u64;

    while Instant::now() < deadline {
        // Keep the Wayland side alive: KWin removes the monitor if we stop
        // answering, and noticing a close is better than streaming silence.
        if !output.poll()? {
            println!("KWin closed the stream early");
            break;
        }
        match capture.next_event(Duration::from_millis(50))? {
            Some(Event::Format(f)) => println!(
                "format  {}x{} {:?} @ {}/{}",
                f.width, f.height, f.format, f.framerate_num, f.framerate_denom
            ),
            Some(Event::Frame(frame)) => {
                frames += 1;
                bytes += frame.data.len() as u64;
                if frames == 1 {
                    println!(
                        "first frame after {:?}: {} bytes, stride {}",
                        started.elapsed(),
                        frame.data.len(),
                        frame.stride
                    );
                }
            }
            Some(Event::Error(e)) => anyhow::bail!("{e}"),
            None => {}
        }
        if last_report.elapsed() >= Duration::from_secs(1) {
            println!("  {} fps", frames - frames_at_report);
            frames_at_report = frames;
            last_report = Instant::now();
        }
    }

    println!(
        "{frames} frames, {:.1} MB in {:.1}s",
        bytes as f64 / 1e6,
        started.elapsed().as_secs_f64()
    );
    drop(capture);
    drop(output);
    Ok(())
}
