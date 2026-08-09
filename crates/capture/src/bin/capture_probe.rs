// SPDX-License-Identifier: Apache-2.0

//! Stage 1 verification and capture benchmark.
//!
//!   capture-probe
//!   capture-probe <output> [--dmabuf|--shm] [--frames N] [--png PATH]

use anyhow::{Context, Result};
use capture::session::{BufferMode, Capture, CaptureConfig};
use capture::{discovery, FALLBACK_MODIFIERS, REQUIRED_GLOBALS};
use std::time::Duration;

struct Args {
    output: Option<String>,
    mode: BufferMode,
    frames: usize,
    png: Option<String>,
    /// Skip the encoder-modifier constraint, to show what GBM picks unaided.
    any_modifier: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        output: None,
        mode: BufferMode::Dmabuf,
        frames: 120,
        png: None,
        any_modifier: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dmabuf" => args.mode = BufferMode::Dmabuf,
            "--shm" => args.mode = BufferMode::Shm,
            "--any-modifier" => args.any_modifier = true,
            "--frames" => {
                args.frames = it.next().and_then(|v| v.parse().ok()).unwrap_or(120);
            }
            "--png" => args.png = it.next(),
            other => args.output = Some(other.to_string()),
        }
    }
    args
}

fn main() -> Result<()> {
    let args = parse_args();
    let (globals, outputs) = discovery::discover()?;

    let mut missing = Vec::new();
    for required in REQUIRED_GLOBALS {
        if !globals.iter().any(|g| g.interface == *required) {
            missing.push(*required);
        }
    }
    if !missing.is_empty() {
        anyhow::bail!("missing required globals: {}", missing.join(", "));
    }

    let Some(target) = args.output else {
        println!("Outputs:");
        for output in &outputs {
            println!(
                "  {:<12} {}x{} @ {:.2} Hz",
                output.name,
                output.width,
                output.height,
                output.refresh_hz()
            );
        }
        println!("\nPass an output name to benchmark capture.");
        return Ok(());
    };

    let mode_label = match args.mode {
        BufferMode::Dmabuf => "DMA-BUF (zero-copy)",
        BufferMode::Shm => "shm (CPU copy)",
    };
    println!("=== Session: {target} / {mode_label} ===");

    let config = CaptureConfig {
        mode: args.mode,
        pool_size: 2,
        allowed_modifiers: if args.any_modifier {
            Vec::new()
        } else {
            FALLBACK_MODIFIERS.to_vec()
        },
    };
    let mut capture = Capture::new(&target, &config)?;
    println!("  buffer      {}x{}", capture.width, capture.height);
    println!("  format      {}", fourcc_name(capture.format));
    if let Some(modifier) = capture.modifier {
        println!("  modifier    0x{modifier:016x}");
    }
    if let Some(path) = &capture.device_path {
        println!("  drm device  {}", path.display());
    }

    // Discard the first few frames: session warm-up is not representative.
    for _ in 0..5 {
        capture.capture_frame()?;
    }

    println!("\n=== Capturing {} frames ===", args.frames);
    let mut latencies = Vec::with_capacity(args.frames);
    let mut ages = Vec::with_capacity(args.frames);
    let mut presentation = Vec::with_capacity(args.frames);
    let wall_start = std::time::Instant::now();

    for _ in 0..args.frames {
        let timing = capture.capture_frame()?;
        latencies.push(timing.latency);
        if let Some(age) = timing.age {
            ages.push(age);
        }
        if let Some(ns) = timing.presentation_ns {
            presentation.push(ns);
        }
    }
    let wall = wall_start.elapsed();

    latencies.sort_unstable();
    println!("\n  capture latency (request -> ready)");
    println!("    min       {:>8.2} ms", ms(latencies[0]));
    println!("    median    {:>8.2} ms", ms(latencies[latencies.len() / 2]));
    println!(
        "    p95       {:>8.2} ms",
        ms(latencies[latencies.len() * 95 / 100])
    );
    println!("    max       {:>8.2} ms", ms(latencies[latencies.len() - 1]));

    if !ages.is_empty() {
        ages.sort_unstable();
        println!("\n  frame age at delivery (presentation -> ready)  <-- enters latency budget");
        println!("    min       {:>8.2} ms", ms(ages[0]));
        println!("    median    {:>8.2} ms", ms(ages[ages.len() / 2]));
        println!("    p95       {:>8.2} ms", ms(ages[ages.len() * 95 / 100]));
        println!("    max       {:>8.2} ms", ms(ages[ages.len() - 1]));
    }

    println!("\n  throughput");
    println!(
        "    wall      {:>8.2} ms for {} frames",
        wall.as_secs_f64() * 1000.0,
        args.frames
    );
    println!(
        "    rate      {:>8.2} fps",
        args.frames as f64 / wall.as_secs_f64()
    );

    if presentation.len() > 2 {
        let mut deltas: Vec<f64> = presentation
            .windows(2)
            .map(|w| (w[1].saturating_sub(w[0])) as f64 / 1e6)
            .collect();
        deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = deltas[deltas.len() / 2];
        println!("\n  frame pacing (compositor presentation deltas)");
        println!("    median    {median:>8.2} ms  ({:.1} fps)", 1000.0 / median);
        println!("    min       {:>8.2} ms", deltas[0]);
        println!("    max       {:>8.2} ms", deltas[deltas.len() - 1]);
    }

    if let Some(path) = args.png {
        let timing = capture.capture_frame()?;
        match capture.shm_bytes(timing.buffer_index) {
            Some((bytes, stride)) => {
                write_png(&path, bytes, stride, capture.width, capture.height)?;
                println!("\nWrote {path}");
            }
            None => println!("\n--png needs --shm (DMA-BUF pixels are never CPU-mapped)"),
        }
    }

    Ok(())
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn write_png(path: &str, pixels: &[u8], stride: u32, width: u32, height: u32) -> Result<()> {
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        let row = (y * stride) as usize;
        for x in 0..width {
            let px = row + (x * 4) as usize;
            rgb.push(pixels[px + 2]);
            rgb.push(pixels[px + 1]);
            rgb.push(pixels[px]);
        }
    }
    let file = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()?
        .write_image_data(&rgb)
        .context("writing PNG data")?;
    Ok(())
}

fn fourcc_name(code: u32) -> String {
    let bytes = code.to_le_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic()) {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("0x{code:08x}")
    }
}
