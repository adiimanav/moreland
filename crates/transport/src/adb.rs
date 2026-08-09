// SPDX-License-Identifier: Apache-2.0

//! Thin wrapper over the `adb` CLI.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// Override the adb binary with `MORELAND_ADB` when it is not on `PATH`.
fn adb_binary() -> String {
    std::env::var("MORELAND_ADB").unwrap_or_else(|_| "adb".to_string())
}

fn adb(args: &[&str]) -> Result<String> {
    let output = Command::new(adb_binary())
        .args(args)
        .output()
        .with_context(|| format!("running `adb {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "adb {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[derive(Debug, Clone)]
pub struct Device {
    pub serial: String,
    pub model: Option<String>,
}

/// Devices in the `device` state. Ignores `unauthorized` and `offline`, which
/// cannot be forwarded to.
pub fn devices() -> Result<Vec<Device>> {
    let out = adb(&["devices", "-l"])?;
    let mut devices = Vec::new();
    for line in out.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(serial), Some(state)) = (fields.next(), fields.next()) else {
            continue;
        };
        if state != "device" {
            continue;
        }
        let model = fields
            .find(|f| f.starts_with("model:"))
            .map(|f| f.trim_start_matches("model:").to_string());
        devices.push(Device {
            serial: serial.to_string(),
            model,
        });
    }
    Ok(devices)
}

/// An `adb forward` rule, removed when dropped.
///
/// The remote spec is a `localabstract:` socket in production so traffic never
/// touches Android's TCP stack or `netd`; `tcp:` is used for host-side testing
/// before the app exists.
pub struct Forward {
    serial: String,
    local: String,
}

impl Forward {
    pub fn new(serial: &str, local_port: u16, remote: &str) -> Result<Self> {
        let local = format!("tcp:{local_port}");
        // Clear any stale rule from a previous run before installing ours.
        let _ = adb(&["-s", serial, "forward", "--remove", &local]);
        adb(&["-s", serial, "forward", &local, remote])
            .with_context(|| format!("forwarding {local} -> {remote}"))?;
        Ok(Self {
            serial: serial.to_string(),
            local,
        })
    }

    pub fn local_port(&self) -> Result<u16> {
        self.local
            .trim_start_matches("tcp:")
            .parse()
            .context("parsing forwarded port")
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        let _ = adb(&["-s", &self.serial, "forward", "--remove", &self.local]);
    }
}

pub fn shell(serial: &str, command: &str) -> Result<String> {
    adb(&["-s", serial, "shell", command])
}

/// The device's physical panel size, in **landscape** orientation.
///
/// `wm size` reports the panel as the hardware sees it, which on a tablet is
/// portrait (e.g. `1800x2880`). The virtual output is landscape, so the axes
/// are swapped here rather than at every call site.
pub fn display_size(serial: &str) -> Result<(u32, u32)> {
    let out = shell(serial, "wm size").context("querying device display size")?;

    // Output is one or two lines:
    //   Physical size: 1800x2880
    //   Override size: 1440x2304      (present only when a size override is set)
    // The override is what apps actually get, so prefer it when present.
    let parse = |prefix: &str| -> Option<(u32, u32)> {
        let line = out.lines().find(|l| l.trim().starts_with(prefix))?;
        let spec = line.split(':').nth(1)?.trim();
        let (w, h) = spec.split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    };

    let (w, h) = parse("Override size")
        .or_else(|| parse("Physical size"))
        .with_context(|| format!("could not parse `wm size` output: {out:?}"))?;

    if w == 0 || h == 0 {
        bail!("device reported a zero display dimension: {w}x{h}");
    }
    Ok(if w >= h { (w, h) } else { (h, w) })
}

/// Pick a streaming resolution matching the device's aspect ratio.
///
/// Streaming a tablet's native panel resolution is usually the wrong default:
/// a 2880x1800 panel is 2.5x the pixels of 1080p, which pushes the encoder hard
/// for detail no one can resolve on an 11" screen at arm's length. Scaling to
/// fit `max_width` while preserving aspect keeps the upscale on the device
/// clean and the encoder comfortable.
///
/// Both axes are rounded to even numbers, which is all NV12 chroma subsampling
/// requires. Aligning to 16 instead would be tempting for macroblock tidiness,
/// but it turns the extremely common 1080 into 1088 and breaks 16:9 exactly
/// where it matters — H.264 pads to macroblocks internally and signals the crop
/// in the SPS, so this is the encoder's problem, not ours.
pub fn stream_resolution(panel: (u32, u32), max_width: u32) -> (u32, u32) {
    let (pw, ph) = panel;
    let width = pw.min(max_width).max(2);
    let height = ((u64::from(width) * u64::from(ph)) / u64::from(pw)) as u32;
    (align2(width), align2(height.max(2)))
}

fn align2(value: u32) -> u32 {
    value & !1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_16_10_panel_cleanly() {
        // Xiaomi Pad 6: 2880x1800 -> exactly 1920x1200, a clean 1.5x upscale.
        assert_eq!(stream_resolution((2880, 1800), 1920), (1920, 1200));
    }

    #[test]
    fn leaves_smaller_panels_alone() {
        assert_eq!(stream_resolution((1920, 1080), 1920), (1920, 1080));
        assert_eq!(stream_resolution((1280, 800), 1920), (1280, 800));
    }

    #[test]
    fn preserves_16_9_exactly() {
        // 1080 must survive intact; rounding it up to 1088 would letterbox
        // every 16:9 tablet.
        assert_eq!(stream_resolution((2560, 1440), 1920), (1920, 1080));
        assert_eq!(stream_resolution((3840, 2160), 1920), (1920, 1080));
    }

    #[test]
    fn output_is_always_even() {
        for panel in [(2000, 1234), (1234, 999), (3000, 2001), (2160, 1080)] {
            let (w, h) = stream_resolution(panel, 1920);
            assert_eq!(w % 2, 0, "width {w} is odd");
            assert_eq!(h % 2, 0, "height {h} is odd");
        }
    }

    #[test]
    fn aspect_ratio_is_preserved_closely() {
        for panel in [(2880, 1800), (2560, 1440), (2000, 1234), (2160, 1620)] {
            let (w, h) = stream_resolution(panel, 1920);
            let want = f64::from(panel.0) / f64::from(panel.1);
            let got = f64::from(w) / f64::from(h);
            assert!(
                (want - got).abs() / want < 0.01,
                "aspect drift for {panel:?}: wanted {want:.4}, got {got:.4}"
            );
        }
    }
}
