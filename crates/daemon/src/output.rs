// SPDX-License-Identifier: Apache-2.0

//! Virtual output creation, per compositor.
//!
//! On a compositor that implements `ext-image-copy-capture-v1` this is the
//! **only** compositor-specific part of the project: capture uses that standard
//! protocol, encoding uses VA-API, and transport uses ADB, all of them
//! compositor-agnostic. Adding such a compositor means implementing one thing —
//! "create a headless output with this name and mode, and remove it later".
//!
//! A compositor *without* the protocol is a different and much larger problem,
//! and it is not solved here. KWin 6.7 and Mutter both implement no `ext-` or
//! `wlr-` capture protocol at all, so they need a second, PipeWire-based
//! capture backend before this file is even reached.
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
    ///
    /// The markers alone are not enough. A systemd user service inherits the
    /// environment of the session that started it and outlives it, so
    /// `HYPRLAND_INSTANCE_SIGNATURE` can still be set — naming an instance
    /// that exited hours ago — while the user is logged into something else
    /// entirely. Believing it there costs the honest "unsupported compositor"
    /// error and replaces it with `hyprctl ... failed:` and an empty stderr,
    /// retried on a backoff forever. So every marker is confirmed against a
    /// live IPC round-trip before it is trusted.
    pub fn detect() -> Self {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();

        if (std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
            || desktop.eq_ignore_ascii_case("Hyprland"))
            && run("hyprctl", &["version"]).is_ok()
        {
            return Compositor::Hyprland;
        }
        if (std::env::var_os("SWAYSOCK").is_some() || desktop.eq_ignore_ascii_case("sway"))
            && run("swaymsg", &["-t", "get_version"]).is_ok()
        {
            return Compositor::Sway;
        }
        Compositor::Unsupported
    }

    pub fn name(self) -> &'static str {
        match self {
            Compositor::Hyprland => "Hyprland",
            Compositor::Sway => "Sway",
            Compositor::Unsupported => "unsupported",
        }
    }

    /// Fail unless this compositor can host a session, so the daemon can
    /// refuse at startup instead of once per device event.
    pub fn ensure_supported(self) -> Result<()> {
        match self {
            Compositor::Hyprland => Ok(()),
            Compositor::Sway => bail!(
                "Sway support is not implemented yet.\n\
                 `swaymsg create_output` exists, but Sway names the result \
                 itself, so the daemon must diff `swaymsg -t get_outputs` to \
                 discover it. See docs/COMPATIBILITY.md."
            ),
            Compositor::Unsupported => {
                let desktop =
                    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "unset".to_string());
                bail!(
                    "unsupported compositor (XDG_CURRENT_DESKTOP={desktop}).\n\
                     This needs a compositor that can create a headless output \
                     and implements ext-image-copy-capture-v1.\n\
                     Verified: Hyprland. KDE Plasma and GNOME implement neither \
                     and need a PipeWire capture backend first.\n\
                     Run scripts/moreland-doctor.sh for a full report, and see \
                     docs/COMPATIBILITY.md."
                )
            }
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

#[derive(Debug, Clone)]
struct MonitorState {
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    refresh_rate: f64,
    scale: f64,
    transform: i32,
}

fn hyprland_monitor_state() -> Result<Vec<MonitorState>> {
    let json =
        run("hyprctl", &["monitors", "-j"]).context("reading Hyprland monitor state")?;

    let monitors: serde_json::Value =
        serde_json::from_str(&json).context("parsing Hyprland monitor JSON")?;

    let monitors = monitors
        .as_array()
        .context("Hyprland monitor JSON is not an array")?;

    let mut states = Vec::with_capacity(monitors.len());

    for monitor in monitors {
        let name = monitor
            .get("name")
            .and_then(|v| v.as_str())
            .context("Hyprland monitor has no name")?;

        let x = monitor
            .get("x")
            .and_then(|v| v.as_i64())
            .context("Hyprland monitor has no x position")?;

        let y = monitor
            .get("y")
            .and_then(|v| v.as_i64())
            .context("Hyprland monitor has no y position")?;

        let width = monitor
            .get("width")
            .and_then(|v| v.as_u64())
            .context("Hyprland monitor has no width")?;

        let height = monitor
            .get("height")
            .and_then(|v| v.as_u64())
            .context("Hyprland monitor has no height")?;

        let refresh_rate = monitor
            .get("refreshRate")
            .and_then(|v| v.as_f64())
            .context("Hyprland monitor has no refresh rate")?;

        let scale = monitor
            .get("scale")
            .and_then(|v| v.as_f64())
            .context("Hyprland monitor has no scale")?;

        let transform = monitor
            .get("transform")
            .and_then(|v| v.as_i64())
            .context("Hyprland monitor has no transform")?;

        states.push(MonitorState {
            name: name.to_string(),
            x: i32::try_from(x).context("Hyprland monitor x position is out of range")?,
            y: i32::try_from(y).context("Hyprland monitor y position is out of range")?,
            width: u32::try_from(width).context("Hyprland monitor width is out of range")?,
            height: u32::try_from(height).context("Hyprland monitor height is out of range")?,
            refresh_rate,
            scale,
            transform: i32::try_from(transform)
                .context("Hyprland monitor transform is out of range")?,
        });
    }

    Ok(states)
}

fn lua_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');

    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            _ => escaped.push(ch),
        }
    }

    escaped.push('"');
    escaped
}

fn restore_hyprland_monitor_state(
    states: &[MonitorState],
    excluded_output: &str,
) -> Result<()> {
    let mut lua = String::new();

    for monitor in states {
        if monitor.name == excluded_output {
            continue;
        }

        let refresh = format!("{:.6}", monitor.refresh_rate)
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string();

        let output = lua_string(&monitor.name);

        lua.push_str(&format!(
            "hl.monitor({{ output = {}, \
             mode = \"{}x{}@{}\", \
             position = \"{}x{}\", \
             scale = {}, \
             transform = {} }}); ",
            output,
            monitor.width,
            monitor.height,
            refresh,
            monitor.x,
            monitor.y,
            monitor.scale,
            monitor.transform
        ));
    }

    if lua.ends_with("; ") {
        lua.truncate(lua.len() - 2);
    }

    run("hyprctl", &["eval", &lua]).context("restoring Hyprland monitor state")?;

    Ok(())
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
                let saved_states = hyprland_monitor_state()
                    .context("saving Hyprland monitor state")?;

                if saved_states.iter().any(|monitor| monitor.name == name) {
                    tracing::debug!("reusing existing output {name}");
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
                let reply = run("hyprctl", &["keyword", "monitor", &spec])
                    .with_context(|| format!("configuring output as {spec}"))?;

                // Hyprland's Lua config parser (0.56+) refuses `keyword`
                // outright — and refuses it on stdout with a zero exit
                // status, so `run` reports success and the output silently
                // keeps the compositor's defaults. Re-issue the same rule
                // through `eval`, which that parser does accept.
                if reply.contains("non-legacy parsers") {
                    let output = lua_string(name);
                    let lua = format!(
                        "hl.monitor({{ output = {}, \
                         mode = \"{width}x{height}@{refresh}\", \
                         position = \"{x}x{y}\", scale = 1 }})",
                        output
                    );
                    run("hyprctl", &["eval", &lua])
                        .with_context(|| format!("configuring output as {lua}"))?;
                }

                restore_hyprland_monitor_state(&saved_states, name)
                    .context("restoring Hyprland monitor state")?;
            }
            // Sway is UNTESTED and everything else is unsupported; both cases
            // report themselves.
            other => other.ensure_supported()?,
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
            Ok(_) => tracing::debug!("removed output {}", self.name),
            Err(e) => tracing::warn!("failed to remove output {}: {e}", self.name),
        }
    }
}
