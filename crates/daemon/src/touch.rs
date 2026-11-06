// SPDX-License-Identifier: Apache-2.0

//! The tablet's touchscreen and pen as virtual input devices on the host.
//!
//! The device is created through uinput and then pinned to the virtual output
//! over KWin's D-Bus API; without that, KWin would stretch a touch across all
//! monitors together. Only KDE is supported so far, because that is the only
//! compositor with a mapping step implemented here.

use anyhow::{bail, Context, Result};
use input::RemoteInput;
use std::process::Command;
use std::time::{Duration, Instant};

/// How long to wait for the kernel node and for KWin to notice the device.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Create the devices and map both to `connector` (e.g. `Virtual-moreland`).
///
/// Failure is not fatal to the session: the picture still streams, there is
/// simply no touch or pen. The caller logs why.
pub fn create_mapped(connector: &str, palm_rejection: bool) -> Result<RemoteInput> {
    let mut remote = RemoteInput::create(palm_rejection)?;

    let deadline = Instant::now() + SETTLE_TIMEOUT;
    let (touch_node, pen_node) = loop {
        if let Some(nodes) = remote.event_nodes()? {
            break nodes;
        }
        if Instant::now() >= deadline {
            bail!("the kernel did not create input nodes for the devices");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    for node in [touch_node, pen_node] {
        let sysname = node
            .file_name()
            .context("input node has no file name")?
            .to_string_lossy()
            .into_owned();
        map_to_output(&sysname, connector, deadline)?;
    }
    Ok(remote)
}

/// Pin one input device to an output over KWin's D-Bus API.
///
/// KWin registers a device asynchronously, so its D-Bus object may not exist
/// for a moment after the kernel node does.
fn map_to_output(sysname: &str, connector: &str, deadline: Instant) -> Result<()> {
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
            tracing::info!("input device {sysname} mapped to {connector}");
            return Ok(());
        }
        last_error = String::from_utf8_lossy(&result.stderr).trim().to_string();
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("could not map {sysname} to {connector}: {last_error}")
}
