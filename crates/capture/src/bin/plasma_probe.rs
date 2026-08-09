// SPDX-License-Identifier: Apache-2.0

//! Stage 6 verification: create a real virtual output on KDE Plasma.
//!
//!   plasma-probe [--width PX] [--height PX] [--scale N]
//!                [--seconds N] [--cursor hidden|embedded|metadata]
//!
//! Creates the monitor, prints its PipeWire node, holds it for `--seconds`,
//! then removes it. While it is up the output is real: it appears in System
//! Settings under Display Configuration and windows can be dragged onto it.
//!
//! This deliberately does **not** consume the PipeWire stream — that is the
//! unimplemented half. The monitor will therefore show nothing anywhere; the
//! point is to prove the output and the node id, not to display them.

use anyhow::{Context, Result};
use capture::plasma::{CursorMode, PlasmaVirtualOutput};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let (mut width, mut height, mut scale) = (1920i32, 1200i32, 1.0f64);
    let mut seconds = 10u64;
    let mut cursor = CursorMode::Embedded;
    let mut mirror: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut next = || args.next().context("missing value");
        match arg.as_str() {
            "--width" => width = next()?.parse().context("--width")?,
            "--height" => height = next()?.parse().context("--height")?,
            "--scale" => scale = next()?.parse().context("--scale")?,
            "--seconds" => seconds = next()?.parse().context("--seconds")?,
            // Diagnostic: stream an existing monitor instead of making one.
            "--mirror" => mirror = Some(next()?),
            "--cursor" => {
                cursor = match next()?.as_str() {
                    "hidden" => CursorMode::Hidden,
                    "embedded" => CursorMode::Embedded,
                    "metadata" => CursorMode::Metadata,
                    other => anyhow::bail!("unknown cursor mode {other:?}"),
                }
            }
            "--help" | "-h" => {
                println!("{}", env!("CARGO_BIN_NAME"));
                println!("  --width PX --height PX --scale N --seconds N --cursor MODE");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }

    let mut output = match &mirror {
        Some(name) => {
            println!("mirroring existing output {name} (diagnostic, not a second screen)...");
            PlasmaVirtualOutput::mirror(name, cursor)?
        }
        None => {
            println!("creating virtual output {width}x{height} @ scale {scale}...");
            PlasmaVirtualOutput::create(
                "moreland",
                "Moreland tablet monitor",
                width,
                height,
                scale,
                cursor,
            )?
        }
    };

    println!();
    println!("  name           {}", output.name());
    println!("  pipewire node  {}", output.node_id());
    match output.object_serial() {
        Some(serial) => println!("  object serial  {serial}"),
        None => println!("  object serial  (not sent; compositor below v6)"),
    }
    println!();
    println!("Check it with:  kscreen-doctor -o");
    println!("Holding for {seconds}s, then removing it.");
    println!();

    // Keep dispatching: KWin can close the stream from its side, and noticing
    // that is the difference between "removed" and "silently dead".
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if !output.poll()? {
            println!("KWin closed the stream early — output removed from its side.");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    println!("removing...");
    drop(output);
    Ok(())
}
