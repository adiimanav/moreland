// SPDX-License-Identifier: Apache-2.0

//! Moreland daemon.
//!
//!   moreland                 watch for the tablet; stream whenever it is plugged in
//!   moreland --once          stream one session, then exit
//!   moreland --seconds N     stop after N seconds (implies --once, --stats)
//!
//! Plug the tablet in and a virtual monitor appears; unplug it and the monitor
//! disappears. No commands in between.

mod output;
mod session;
mod tracker;

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracker::DeviceTracker;

struct Args {
    config: session::Config,
    once: bool,
    seconds: Option<u64>,
}

fn parse_args() -> Args {
    let mut config = session::Config::default();
    let mut once = false;
    let mut seconds = None;
    let (mut width, mut height) = (None, None);

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next_u32 = |default: u32| {
            it.next().and_then(|v| v.parse().ok()).unwrap_or(default)
        };
        match arg.as_str() {
            "--width" => width = Some(next_u32(1920)),
            "--height" => height = Some(next_u32(1200)),
            "--max-width" => config.max_width = next_u32(1920),
            "--native" => config.max_width = u32::MAX,
            "--fps" => config.fps = next_u32(60),
            "--bitrate" => config.bitrate_kbps = next_u32(20_000),
            "--position" => {
                config.position_x = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(config.position_x);
            }
            "--once" => once = true,
            "--stats" => config.stats = true,
            "--seconds" => {
                seconds = it.next().and_then(|v| v.parse().ok());
                once = true;
                config.stats = true;
            }
            "--help" | "-h" => {
                println!("{}", include_str!("usage.txt"));
                std::process::exit(0);
            }
            _ => {}
        }
    }
    // Only an explicit pair pins the resolution; one alone still auto-detects,
    // since a half-specified size would silently distort the aspect ratio.
    config.resolution = match (width, height) {
        (Some(w), Some(h)) => Some((w, h)),
        (Some(_), None) | (None, Some(_)) => {
            eprintln!("--width and --height must be given together; auto-detecting instead");
            None
        }
        (None, None) => None,
    };

    Args {
        config,
        once,
        seconds,
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = parse_args();

    // Refuse to start rather than discovering this once per device event. The
    // compositor cannot change under a running daemon — the unit is
    // PartOf=graphical-session.target — so an unsupported one is fatal, not
    // transient, and the retry backoff would otherwise spin on it for as long
    // as the session lasts.
    let compositor = output::Compositor::detect();
    compositor.ensure_supported()?;
    tracing::info!("compositor: {}", compositor.name());

    let shutdown = Arc::new(AtomicBool::new(false));

    // SIGTERM matters here: systemd uses it to stop the service, and the
    // virtual output must come down cleanly or Hyprland is left with a
    // phantom monitor.
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
    }

    if let Some(seconds) = args.seconds {
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(seconds));
            shutdown.store(true, Ordering::Relaxed);
        });
    }

    let mut tracker = DeviceTracker::connect()?;
    tracing::info!("watching for devices");

    // The last device list we were told about.
    //
    // This must persist across iterations. `next_update` only returns when the
    // list *changes*, so after a session ends for any reason other than the
    // device vanishing — a dropped socket, the app being closed, a compositor
    // hiccup — no further update is ever sent. Blocking on one there strands
    // the daemon: alive, connected, and silently never retrying.
    let mut devices: Vec<tracker::Device> = Vec::new();
    let mut consecutive_failures = 0u32;
    let mut announced_idle = false;

    while !shutdown.load(Ordering::Relaxed) {
        // Short timeout: absence of an update is normal and just means the
        // list still stands.
        tracker.set_timeout(Some(Duration::from_millis(500)))?;
        match tracker.next_update() {
            Ok(update) => {
                devices = update;
                announced_idle = false;
                consecutive_failures = 0;
            }
            Err(e) if is_timeout(&e) => {}
            Err(e) => {
                tracing::warn!("device tracker lost: {e}; reconnecting");
                devices.clear();
                std::thread::sleep(Duration::from_secs(2));
                match DeviceTracker::connect() {
                    Ok(t) => {
                        tracker = t;
                        tracing::info!("device tracker reconnected");
                    }
                    Err(e) => tracing::warn!("reconnect failed: {e}"),
                }
                continue;
            }
        }

        let Some(device) = devices.iter().find(|d| d.is_ready()).cloned() else {
            if !announced_idle {
                if devices.is_empty() {
                    tracing::info!("no device connected");
                } else {
                    for device in &devices {
                        tracing::info!("device {} is {} — waiting", device.serial, device.state);
                    }
                }
                announced_idle = true;
            }
            continue;
        };

        tracing::info!("device {} connected", device.serial);
        let outcome = session::run(&device.serial, &args.config, &shutdown);

        if args.once || shutdown.load(Ordering::Relaxed) {
            match outcome {
                Ok(()) => tracing::info!("session ended cleanly"),
                Err(e) => tracing::warn!("session ended: {e:#}"),
            }
            break;
        }

        match outcome {
            Ok(()) => {
                tracing::info!("session ended cleanly");
                consecutive_failures = 0;
            }
            Err(e) => {
                consecutive_failures += 1;
                tracing::warn!("session ended: {e:#}");
            }
        }

        // Back off when sessions keep failing immediately — an unplugged
        // cable, an uninstalled app, a tablet that never wakes — instead of
        // spinning on `am start` and hyprctl.
        let backoff = Duration::from_secs(2 * u64::from(consecutive_failures.min(8)).max(1));
        if consecutive_failures > 2 {
            tracing::info!("retrying in {}s", backoff.as_secs());
        }
        std::thread::sleep(backoff);
    }

    tracing::info!("shutting down");
    Ok(())
}

fn is_timeout(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
    })
}
