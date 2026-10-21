// SPDX-License-Identifier: Apache-2.0

//! The tablet's touchscreen as a virtual input device on the host.
//!
//! The device is created through uinput and then pinned to the virtual output
//! over KWin's D-Bus API; without that, KWin would stretch a touch across all
//! monitors together. Only KDE is supported so far, because that is the only
//! compositor with a mapping step implemented here.

use anyhow::{bail, Context, Result};
use input::Touchscreen;
use std::process::Command;
use std::time::{Duration, Instant};

const DEVICE_NAME: &str = "Moreland Touchscreen";

/// How long to wait for the kernel node and for KWin to notice the device.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Create a touchscreen and map it to `connector` (e.g. `Virtual-moreland`).
///
/// Failure is not fatal to the session: the picture still streams, there is
/// simply no touch. The caller logs why.
pub fn create_mapped(connector: &str) -> Result<Touchscreen> {
    let mut screen = Touchscreen::create(DEVICE_NAME)?;

    let deadline = Instant::now() + SETTLE_TIMEOUT;
    let sysname = loop {
        if let Some(node) = screen.event_node()? {
            break node
                .file_name()
                .context("input node has no file name")?
                .to_string_lossy()
                .into_owned();
        }
        if Instant::now() >= deadline {
            bail!("the kernel did not create an input node for the touchscreen");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    // KWin registers the device asynchronously, so the D-Bus object may not
    // exist for a moment after the node does.
    let path = format!("/org/kde/KWin/InputDevice/{sysname}");
    let mut last_error = String::new();
    while Instant::now() < deadline {
        let result = Command::new("busctl")
            .args([
                "--user",
                "set-property",
                "org.kde.KWin",
                &path,
                "org.kde.KWin.InputDevice",
                "outputName",
                "s",
                connector,
            ])
            .output()
            .context("running busctl")?;
        if result.status.success() {
            tracing::info!("touchscreen {sysname} mapped to {connector}");
            return Ok(screen);
        }
        last_error = String::from_utf8_lossy(&result.stderr).trim().to_string();
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("could not map {sysname} to {connector}: {last_error}")
}
