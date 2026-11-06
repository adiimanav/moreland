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

use crate::display::{lua_string, Layout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compositor {
    Hyprland,
    /// wlroots-based with a Sway-compatible IPC (`swaymsg create_output`).
    Sway,
    /// labwc, queried through `wlr-randr`. Unlike the other two it has no
    /// runtime IPC to *create* an output, so the session attaches to one the
    /// compositor was started with. See `docs/COMPATIBILITY.md`.
    Labwc,
    /// KDE Plasma. KWin implements no capture protocol, but its own
    /// `zkde_screencast_unstable_v1` creates the output and hands back a
    /// PipeWire node in one call. Needs the `plasma` cargo feature.
    Kwin,
    Unsupported,
}

/// Which edge of the main screen the tablet's monitor sits against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Right,
    Left,
    Above,
    Below,
}

impl std::str::FromStr for Side {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "right" => Ok(Side::Right),
            "left" => Ok(Side::Left),
            "above" | "top" | "up" => Ok(Side::Above),
            "below" | "bottom" | "down" => Ok(Side::Below),
            other => Err(format!(
                "unknown side {other:?}; expected right, left, above or below"
            )),
        }
    }
}

/// Where the virtual monitor goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Exact top-left corner in layout pixels.
    Coords(i32, i32),
    /// Against one edge of the main screen, worked out from the live layout.
    /// Only KDE reads the layout; elsewhere this falls back to the legacy
    /// position of directly below a 1080p primary.
    Side(Side),
}

/// Legacy position for compositors that cannot read the layout: below a
/// primary at 0x0 that is no taller than 1080px.
pub(crate) const FALLBACK_POSITION: (i32, i32) = (0, 1080);

/// Top-left corner that puts a monitor of `size` against `side` of `primary`
/// (`x, y, width, height`). The edge being shared is flush and the other axis
/// is top- or left-aligned, which is where a pointer crosses most naturally.
#[cfg_attr(not(feature = "plasma"), allow(dead_code))]
pub fn place_beside(side: Side, primary: (i32, i32, i32, i32), size: (i32, i32)) -> (i32, i32) {
    let (px, py, pw, ph) = primary;
    match side {
        Side::Right => (px + pw, py),
        Side::Left => (px - size.0, py),
        Side::Below => (px, py + ph),
        Side::Above => (px, py - size.1),
    }
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
        // labwc has reported itself as both `labwc` and the generic `wlroots`
        // depending on version, so neither value alone identifies it. The
        // confirming round-trip is `wlr-randr`, which is also how this backend
        // reads output state later: if it cannot list outputs now, the backend
        // could not work anyway.
        if (desktop.eq_ignore_ascii_case("labwc") || desktop.eq_ignore_ascii_case("wlroots"))
            && run("wlr-randr", &[]).is_ok()
        {
            return Compositor::Labwc;
        }
        // KWin sets XDG_CURRENT_DESKTOP=KDE and has no instance marker of its
        // own, so the round-trip is `kscreen-doctor`, which is also how the
        // output is positioned afterwards.
        if desktop.split(':').any(|d| d.eq_ignore_ascii_case("KDE"))
            && run("kscreen-doctor", &["-o"]).is_ok()
        {
            return Compositor::Kwin;
        }
        Compositor::Unsupported
    }

    pub fn name(self) -> &'static str {
        match self {
            Compositor::Hyprland => "Hyprland",
            Compositor::Sway => "Sway",
            Compositor::Labwc => "labwc",
            Compositor::Kwin => "KDE Plasma",
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
            Compositor::Labwc => Ok(()),
            #[cfg(feature = "plasma")]
            Compositor::Kwin => Ok(()),
            #[cfg(not(feature = "plasma"))]
            Compositor::Kwin => bail!(
                "this is KDE Plasma, but moreland was built without KDE support.\n\
                 Rebuild with `cargo build --release --features plasma`."
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

/// Whether `wlr-randr` lists an output by this exact name.
///
/// It prints each output name at the start of a line and indents that output's
/// properties beneath it, so the first token of a line is the name. Comparing
/// the token rather than a prefix keeps `HEADLESS-1` from matching
/// `HEADLESS-10`.
fn labwc_output_exists(name: &str) -> bool {
    run("wlr-randr", &[])
        .map(|out| {
            out.lines()
                .any(|line| line.split_whitespace().next() == Some(name))
        })
        .unwrap_or(false)
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
    /// On KDE the output's lifetime is the Wayland connection that created it,
    /// so this is what keeps the monitor alive.
    #[cfg(feature = "plasma")]
    plasma: Option<capture::plasma::PlasmaVirtualOutput>,
}

impl VirtualOutput {
    // `paint_cursor` only matters to the KDE backend.
    #[cfg_attr(not(feature = "plasma"), allow(unused_variables))]
    pub fn create(layout: &Layout, placement: Placement, paint_cursor: bool) -> Result<Self> {
        let name = &layout.name;
        let compositor = Compositor::detect();
        match compositor {
            // labwc cannot create an output at runtime — wlroots only builds
            // headless outputs at backend init, from `WLR_HEADLESS_OUTPUTS`.
            // So the session attaches to an output that already exists and
            // leaves it alone afterwards, rather than owning its lifetime.
            // Nothing here sizes or positions it either: `wlr-randr` could,
            // but the mode is the tablet's and the user chose this output
            // deliberately, so silently reshaping their session is worse than
            // leaving it as configured.
            Compositor::Labwc => {
                if !labwc_output_exists(name) {
                    bail!(
                        "labwc output {name:?} does not exist.\n\
                         labwc cannot create one at runtime, so start it with a \
                         headless output — `WLR_HEADLESS_OUTPUTS=1 labwc` — and \
                         pass that output's name (`wlr-randr` lists it, usually \
                         HEADLESS-1) with --output-name.\n\
                         See docs/COMPATIBILITY.md."
                    );
                }
                // Mode and position stay the user's; rotation is the one
                // property `--transform` promises, so honour it here too.
                // Always, 0 included: the output outlives the session and
                // Drop leaves it alone, so this call is the only thing that
                // can undo a rotation an earlier run left behind.
                let args = layout.wlr_randr_args().with_context(|| {
                    format!(
                        "transform {} is not a rotation wlr-randr knows",
                        layout.transform
                    )
                })?;
                run("wlr-randr", &args).with_context(|| format!("rotating labwc output {name}"))?;
                return Ok(Self::new(compositor, name));
            }
            #[cfg(feature = "plasma")]
            Compositor::Kwin => {
                let (width, height) = layout.mode;
                return Self::create_kwin(name, width, height, layout.refresh, placement, paint_cursor);
            }
            Compositor::Hyprland => {
                let saved_states =
                    hyprland_monitor_state().context("saving Hyprland monitor state")?;

                if saved_states.iter().any(|monitor| monitor.name == *name) {
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

                let spec = layout.hyprland_rule();
                let reply = run("hyprctl", &["keyword", "monitor", &spec])
                    .with_context(|| format!("configuring output as {spec}"))?;

                // A compositor running the non-legacy (Lua) config parser
                // rejects `keyword` with "keyword can't work with non-legacy
                // parsers. Use eval." on stdout, with a zero exit status, so
                // `run` reports success and the output silently keeps the
                // compositor's defaults. Re-issue the same rule through
                // `eval`, which that parser accepts. The classic parser, which
                // the reference machine runs, applies the keyword directly.
                if reply.contains("non-legacy parsers") {
                    let lua = layout.hyprland_lua();
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
        Ok(Self::new(compositor, name))
    }

    fn new(compositor: Compositor, name: &str) -> Self {
        Self {
            compositor,
            name: name.to_string(),
            #[cfg(feature = "plasma")]
            plasma: None,
        }
    }

    /// Create the monitor through `zkde_screencast_unstable_v1` and place it.
    ///
    /// The protocol request carries no refresh rate, and KWin gives the output
    /// a single 60 Hz mode. Anything else is added afterwards as a custom mode
    /// through `kscreen-doctor` and selected.
    #[cfg(feature = "plasma")]
    fn create_kwin(
        name: &str,
        width: u32,
        height: u32,
        refresh: u32,
        placement: Placement,
        paint_cursor: bool,
    ) -> Result<Self> {
        use capture::plasma::{CursorMode, PlasmaVirtualOutput};

        let cursor = if paint_cursor {
            CursorMode::Embedded
        } else {
            CursorMode::Hidden
        };
        let plasma = PlasmaVirtualOutput::create(
            name,
            "Moreland tablet monitor",
            i32::try_from(width).context("width out of range")?,
            i32::try_from(height).context("height out of range")?,
            1.0,
            cursor,
        )?;

        // KWin prefixes virtual outputs with `Virtual-`. A failure to place the
        // output is not fatal: it still exists and still streams, only at
        // KWin's default spot beside the primary.
        let connector = format!("Virtual-{name}");
        let (x, y) = match placement {
            Placement::Coords(x, y) => (x, y),
            Placement::Side(side) => match kwin_primary_geometry(&connector) {
                Ok(primary) => {
                    let spot = place_beside(side, primary, (width as i32, height as i32));
                    tracing::info!("placing the output {side:?} of the main screen at {spot:?}");
                    spot
                }
                Err(e) => {
                    tracing::warn!("could not read the screen layout ({e:#}); using the default spot");
                    FALLBACK_POSITION
                }
            },
        };
        // KWin refuses a negative position, so a spot left of or above the
        // other screens moves them the other way instead, in the same request.
        let others = if x < 0 || y < 0 {
            kwin_positions(&connector).unwrap_or_else(|e| {
                tracing::warn!("could not read the other screens' positions ({e:#})");
                Vec::new()
            })
        } else {
            Vec::new()
        };
        let request = kwin_layout_request((&connector, x, y), &others);
        if request.len() > 1 {
            tracing::info!("shifting the other screens to make room at ({x}, {y})");
        }
        let args: Vec<String> = request
            .iter()
            .map(|(name, x, y)| format!("output.{name}.position.{x},{y}"))
            .collect();
        if let Err(e) = kscreen_apply(&args) {
            tracing::warn!("could not position the output: {e:#}");
        }

        if refresh != 60 {
            if let Err(e) = set_kwin_refresh(&format!("Virtual-{name}"), width, height, refresh) {
                tracing::warn!(
                    "could not set {refresh} Hz on the output; staying at 60 Hz: {e:#}"
                );
            }
        }

        let mut output = Self::new(Compositor::Kwin, name);
        output.plasma = Some(plasma);
        Ok(output)
    }

    /// PipeWire node carrying this output's frames. KDE only.
    #[cfg(feature = "plasma")]
    pub fn pipewire_node(&self) -> Option<u32> {
        self.plasma.as_ref().map(|p| p.node_id())
    }

    /// The KDE output's own size, which its stream should be asked for.
    #[cfg(feature = "plasma")]
    pub fn pipewire_size(&self) -> Option<(u32, u32)> {
        self.plasma.as_ref().and_then(|p| p.size())
    }

    /// Service the compositor connection. Returns false once the compositor
    /// has removed the output from its side. A no-op everywhere but KDE.
    #[cfg(feature = "plasma")]
    pub fn poll(&mut self) -> Result<bool> {
        match &mut self.plasma {
            Some(p) => p.poll(),
            None => Ok(true),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The name KWin gives this output, which is what its D-Bus API takes.
    /// `None` on every other compositor.
    pub fn kwin_connector(&self) -> Option<String> {
        (self.compositor == Compositor::Kwin).then(|| format!("Virtual-{}", self.name))
    }

    /// Whether the requested mode and position were actually applied. False on
    /// labwc, where the output is the session's rather than ours and keeps
    /// whatever geometry it was configured with.
    pub fn applied_mode(&self) -> bool {
        self.compositor != Compositor::Labwc
    }
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        let result = match self.compositor {
            Compositor::Hyprland => run("hyprctl", &["output", "remove", &self.name]),
            Compositor::Sway => run("swaymsg", &["output", &self.name, "unplug"]),
            // Never created here, so not ours to remove.
            Compositor::Labwc => return,
            // Dropping `plasma` removes the monitor.
            Compositor::Kwin => return,
            Compositor::Unsupported => return,
        };
        match result {
            Ok(_) => tracing::debug!("removed output {}", self.name),
            Err(e) => tracing::warn!("failed to remove output {}: {e}", self.name),
        }
    }
}

/// How close to the requested rate a mode must be to count as a match. The
/// tablet's panel runs at exactly the nominal rate, so the leftover difference
/// shows up as one repeated frame every 1 / difference seconds: 0.02 Hz is a
/// hitch about every 50 s, against every 8 s for the 89.88 Hz KWin tends to
/// produce first.
#[cfg(feature = "plasma")]
const REFRESH_TOLERANCE_HZ: f64 = 0.02;

/// Requested rates to try, in millihertz offsets from the target, when no
/// existing mode is close enough. KWin derives pixel-clock timings from the
/// request and rounds to its own steps, so the result is not the request; a
/// few nearby asks land on different roundings.
#[cfg(feature = "plasma")]
const REFRESH_PROBES_MHZ: [i32; 5] = [0, 20, 40, 60, 80];

/// All modes of `connector` at `width`x`height`, as `(id, refresh Hz)`.
#[cfg(feature = "plasma")]
fn kwin_modes(connector: &str, width: u32, height: u32) -> Result<Vec<(String, f64)>> {
    let json = run("kscreen-doctor", &["-j"]).context("reading the KWin output list")?;
    let doc: serde_json::Value = serde_json::from_str(&json).context("parsing kscreen JSON")?;
    let outputs = doc
        .get("outputs")
        .and_then(|o| o.as_array())
        .context("kscreen JSON has no outputs")?;
    let output = outputs
        .iter()
        .find(|o| o.get("name").and_then(|n| n.as_str()) == Some(connector))
        .with_context(|| format!("KWin has no output {connector}"))?;

    let mut modes = Vec::new();
    for mode in output
        .get("modes")
        .and_then(|m| m.as_array())
        .context("output has no modes")?
    {
        let size = mode.get("size");
        let dim = |key: &str| size.and_then(|s| s.get(key)).and_then(|v| v.as_u64());
        if dim("width") != Some(u64::from(width)) || dim("height") != Some(u64::from(height)) {
            continue;
        }
        let id = match mode.get("id") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => continue,
        };
        if let Some(hz) = mode.get("refreshRate").and_then(|r| r.as_f64()) {
            modes.push((id, hz));
        }
    }
    Ok(modes)
}

#[cfg(feature = "plasma")]
fn closest_mode(modes: &[(String, f64)], target: f64) -> Option<&(String, f64)> {
    modes
        .iter()
        .min_by(|a, b| (a.1 - target).abs().total_cmp(&(b.1 - target).abs()))
}

/// Switch `connector` to the available mode nearest `refresh` Hz, creating
/// custom modes only if nothing is within [`REFRESH_TOLERANCE_HZ`].
///
/// Custom modes are never removed in a session and every new one lengthens
/// KWin's list, so an existing close-enough mode is reused rather than added.
#[cfg(feature = "plasma")]
fn set_kwin_refresh(connector: &str, width: u32, height: u32, refresh: u32) -> Result<()> {
    let target = f64::from(refresh);
    let mut modes = kwin_modes(connector, width, height)?;

    for offset in REFRESH_PROBES_MHZ {
        let within = closest_mode(&modes, target)
            .is_some_and(|(_, hz)| (hz - target).abs() <= REFRESH_TOLERANCE_HZ);
        if within {
            break;
        }
        let mhz = refresh * 1000 + offset as u32;
        let add = format!("output.{connector}.addCustomMode.{width}.{height}.{mhz}.reduced");
        run("kscreen-doctor", &[&add]).context("adding a custom mode")?;
        modes = kwin_modes(connector, width, height)?;
    }

    let (id, hz) = closest_mode(&modes, target).context("no modes at this size")?;
    run(
        "kscreen-doctor",
        &[&format!("output.{connector}.mode.{id}")],
    )
    .with_context(|| format!("selecting mode {id} ({hz:.3} Hz)"))?;
    tracing::info!(
        "refresh {hz:.3} Hz selected (asked for {refresh}; {:+.3} Hz off)",
        hz - target
    );
    Ok(())
}

/// Every position to request so that `ours` lands where asked, ours first.
///
/// KWin's layout starts at 0,0 and refuses a negative position outright, so a
/// spot left of or above everything is reached by moving everything else right
/// or down by the same amount. A spot already in range asks for nothing else.
#[cfg_attr(not(feature = "plasma"), allow(dead_code))]
fn kwin_layout_request(
    ours: (&str, i32, i32),
    others: &[(String, i32, i32)],
) -> Vec<(String, i32, i32)> {
    let (name, x, y) = ours;
    let (dx, dy) = ((-x).max(0), (-y).max(0));
    let mut request = vec![(name.to_string(), x + dx, y + dy)];
    if dx > 0 || dy > 0 {
        request.extend(
            others
                .iter()
                .map(|(other, ox, oy)| (other.clone(), ox + dx, oy + dy)),
        );
    }
    request
}

/// Name and position of every enabled screen except `except`.
#[cfg(feature = "plasma")]
fn kwin_positions(except: &str) -> Result<Vec<(String, i32, i32)>> {
    let json = run("kscreen-doctor", &["-j"]).context("reading the KWin output list")?;
    let doc: serde_json::Value = serde_json::from_str(&json).context("parsing kscreen JSON")?;
    let outputs = doc
        .get("outputs")
        .and_then(|o| o.as_array())
        .context("kscreen JSON has no outputs")?;
    let int = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_i64()).map(|v| v as i32);
    Ok(outputs
        .iter()
        .filter(|o| o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false))
        .filter_map(|o| {
            let name = o.get("name").and_then(|n| n.as_str())?;
            let pos = o.get("pos");
            (name != except).then(|| {
                (
                    name.to_string(),
                    int(pos.and_then(|p| p.get("x"))).unwrap_or(0),
                    int(pos.and_then(|p| p.get("y"))).unwrap_or(0),
                )
            })
        })
        .collect())
}

/// Apply a `kscreen-doctor` request, treating a rejected one as an error.
///
/// kscreen-doctor reports a configuration KWin refused ("applying config
/// failed! ...") on stdout and still exits 0, so the exit status alone would
/// report success for a request that changed nothing.
#[cfg(feature = "plasma")]
fn kscreen_apply(args: &[String]) -> Result<()> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run("kscreen-doctor", &args)?;
    if let Some(line) = out.lines().find(|line| line.contains("failed")) {
        bail!("kscreen-doctor {}: {}", args.join(" "), line.trim());
    }
    Ok(())
}

/// Position and logical size `(x, y, width, height)` of the main screen: the
/// enabled output with the best (lowest) priority, ignoring `except`.
#[cfg(feature = "plasma")]
fn kwin_primary_geometry(except: &str) -> Result<(i32, i32, i32, i32)> {
    let json = run("kscreen-doctor", &["-j"]).context("reading the KWin output list")?;
    let doc: serde_json::Value = serde_json::from_str(&json).context("parsing kscreen JSON")?;
    let outputs = doc
        .get("outputs")
        .and_then(|o| o.as_array())
        .context("kscreen JSON has no outputs")?;

    let primary = outputs
        .iter()
        .filter(|o| o.get("name").and_then(|n| n.as_str()) != Some(except))
        .filter(|o| o.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false))
        .min_by_key(|o| o.get("priority").and_then(|p| p.as_i64()).unwrap_or(i64::MAX))
        .context("no enabled screen to place the tablet beside")?;

    let int = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_i64()).map(|v| v as i32);
    let x = int(primary.get("pos").and_then(|p| p.get("x"))).unwrap_or(0);
    let y = int(primary.get("pos").and_then(|p| p.get("y"))).unwrap_or(0);
    let mut width = int(primary.get("size").and_then(|s| s.get("width")))
        .context("screen has no width")?;
    let mut height = int(primary.get("size").and_then(|s| s.get("height")))
        .context("screen has no height")?;

    // `size` is the mode in physical pixels. The layout works in logical
    // pixels, so undo any scaling, and swap the axes for a screen turned on
    // its side (KWin's rotation codes 2 and 8 are 90 and 270 degrees).
    if matches!(primary.get("rotation").and_then(|r| r.as_i64()), Some(2) | Some(8)) {
        std::mem::swap(&mut width, &mut height);
    }
    let scale = primary
        .get("scale")
        .and_then(|s| s.as_f64())
        .filter(|s| *s > 0.0)
        .unwrap_or(1.0);
    Ok((
        x,
        y,
        (f64::from(width) / scale).round() as i32,
        (f64::from(height) / scale).round() as i32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn others() -> Vec<(String, i32, i32)> {
        vec![("eDP-1".to_string(), 0, 0)]
    }

    #[test]
    fn a_spot_in_range_moves_nothing_else() {
        assert_eq!(
            kwin_layout_request(("Virtual-moreland", 1746, 0), &others()),
            vec![("Virtual-moreland".to_string(), 1746, 0)]
        );
    }

    #[test]
    fn left_of_everything_moves_everything_else_right() {
        // What `--side left` asks for beside a laptop at 0,0; KWin refuses the
        // -1920 itself, so the laptop moves over instead.
        assert_eq!(
            kwin_layout_request(("Virtual-moreland", -1920, 0), &others()),
            vec![
                ("Virtual-moreland".to_string(), 0, 0),
                ("eDP-1".to_string(), 1920, 0),
            ]
        );
    }

    #[test]
    fn above_everything_moves_everything_else_down() {
        assert_eq!(
            kwin_layout_request(("Virtual-moreland", 0, -1200), &others()),
            vec![
                ("Virtual-moreland".to_string(), 0, 0),
                ("eDP-1".to_string(), 0, 1200),
            ]
        );
    }

    const LAPTOP: (i32, i32, i32, i32) = (0, 0, 1920, 1080);
    const TABLET: (i32, i32) = (2560, 1600);

    #[test]
    fn sits_against_each_edge_of_the_main_screen() {
        assert_eq!(place_beside(Side::Right, LAPTOP, TABLET), (1920, 0));
        assert_eq!(place_beside(Side::Below, LAPTOP, TABLET), (0, 1080));
        assert_eq!(place_beside(Side::Left, LAPTOP, TABLET), (-2560, 0));
        assert_eq!(place_beside(Side::Above, LAPTOP, TABLET), (0, -1600));
    }

    #[test]
    fn follows_a_main_screen_that_is_not_at_the_origin() {
        let offset = (1000, 200, 1920, 1080);
        assert_eq!(place_beside(Side::Right, offset, TABLET), (2920, 200));
        assert_eq!(place_beside(Side::Left, offset, TABLET), (-1560, 200));
    }

    #[test]
    fn side_names_parse_with_friendly_aliases() {
        assert_eq!("right".parse(), Ok(Side::Right));
        assert_eq!("LEFT".parse(), Ok(Side::Left));
        assert_eq!("top".parse(), Ok(Side::Above));
        assert_eq!("bottom".parse(), Ok(Side::Below));
        assert!("diagonal".parse::<Side>().is_err());
    }
}
