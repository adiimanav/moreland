//! Virtual output creation, per compositor.
//!
//! This is the **only** compositor-specific part of the project. Capture uses
//! `ext-image-copy-capture-v1`, encoding uses VA-API, and transport uses ADB —
//! all compositor-agnostic. Adding a compositor means implementing one thing:
//! "create a headless output with this name and mode, and remove it later".
//!
//! See `docs/COMPATIBILITY.md` for what each compositor needs.

use anyhow::{bail, Context, Result};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compositor {
    Hyprland,
    /// wlroots-based with a Sway-compatible IPC (`swaymsg create_output`).
    Sway,
    Unsupported,
}

impl Compositor {
    /// Identify the running compositor from its environment markers.
    pub fn detect() -> Self {
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            return Compositor::Hyprland;
        }
        if std::env::var_os("SWAYSOCK").is_some() {
            return Compositor::Sway;
        }
        match std::env::var("XDG_CURRENT_DESKTOP").as_deref() {
            Ok(desktop) if desktop.eq_ignore_ascii_case("Hyprland") => Compositor::Hyprland,
            Ok(desktop) if desktop.eq_ignore_ascii_case("sway") => Compositor::Sway,
            _ => Compositor::Unsupported,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Compositor::Hyprland => "Hyprland",
            Compositor::Sway => "Sway",
            Compositor::Unsupported => "unsupported",
        }
    }
}

fn run(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running `{program} {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn output_exists(compositor: Compositor, name: &str) -> bool {
    match compositor {
        Compositor::Hyprland => run("hyprctl", &["monitors", "all"])
            .map(|out| out.contains(name))
            .unwrap_or(false),
        Compositor::Sway => run("swaymsg", &["-t", "get_outputs"])
            .map(|out| out.contains(name))
            .unwrap_or(false),
        Compositor::Unsupported => false,
    }
}

/// A headless output, removed when dropped.
pub struct VirtualOutput {
    compositor: Compositor,
    name: String,
}

impl VirtualOutput {
    pub fn create(
        name: &str,
        width: u32,
        height: u32,
        refresh: u32,
        x: i32,
        y: i32,
    ) -> Result<Self> {
        let compositor = Compositor::detect();
        match compositor {
            Compositor::Hyprland => {
                if output_exists(compositor, name) {
                    tracing::info!("reusing existing output {name}");
                } else {
                    // Hyprland accepts an explicit name here, so the result is
                    // deterministic. Without one it allocates HEADLESS-N from a
                    // counter that persists across creates and never resets —
                    // guessing the name is a latent bug.
                    run("hyprctl", &["output", "create", "headless", name])
                        .context("creating headless output")?;
                    std::thread::sleep(std::time::Duration::from_millis(400));
                }
                let spec = format!("{name},{width}x{height}@{refresh},{x}x{y},1");
                run("hyprctl", &["keyword", "monitor", &spec])
                    .with_context(|| format!("configuring output as {spec}"))?;
            }
            Compositor::Sway => {
                // UNTESTED. Sway's headless backend names outputs HEADLESS-N
                // itself and offers no way to choose, so this resolves the name
                // by diffing before and after. Reported working shapes welcome;
                // see docs/COMPATIBILITY.md.
                bail!(
                    "Sway support is not implemented yet.\n\
                     `swaymsg create_output` exists, but Sway names the result \
                     itself, so the daemon must diff `swaymsg -t get_outputs` to \
                     discover it. See docs/COMPATIBILITY.md."
                );
            }
            Compositor::Unsupported => {
                bail!(
                    "unsupported compositor.\n\
                     This needs a compositor that can create a headless output \
                     and implements ext-image-copy-capture-v1.\n\
                     Verified: Hyprland. See docs/COMPATIBILITY.md for what \
                     KDE Plasma, GNOME and Sway would each require."
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        Ok(Self {
            compositor,
            name: name.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        let result = match self.compositor {
            Compositor::Hyprland => run("hyprctl", &["output", "remove", &self.name]),
            Compositor::Sway => run("swaymsg", &["output", &self.name, "unplug"]),
            Compositor::Unsupported => return,
        };
        match result {
            Ok(_) => tracing::info!("removed output {}", self.name),
            Err(e) => tracing::warn!("failed to remove output {}: {e}", self.name),
        }
    }
}
