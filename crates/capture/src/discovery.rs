//! Enumerate Wayland globals and outputs.

use anyhow::{Context, Result};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

/// A compositor global, as advertised by the registry.
#[derive(Debug, Clone)]
pub struct GlobalInfo {
    pub interface: String,
    pub version: u32,
}

/// Current state of a `wl_output`.
#[derive(Debug, Clone, Default)]
pub struct OutputInfo {
    pub name: String,
    pub description: String,
    pub width: i32,
    pub height: i32,
    /// Refresh rate in mHz, as reported by `wl_output.mode`.
    pub refresh_mhz: i32,
}

impl OutputInfo {
    pub fn refresh_hz(&self) -> f64 {
        f64::from(self.refresh_mhz) / 1000.0
    }

    /// Hyprland names virtual outputs `HEADLESS-N`.
    pub fn is_headless(&self) -> bool {
        self.name.starts_with("HEADLESS-")
    }
}

#[derive(Default)]
struct Discovery {
    outputs: Vec<OutputInfo>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Discovery {
    fn event(
        _state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, usize> for Discovery {
    fn event(
        state: &mut Self,
        _output: &wl_output::WlOutput,
        event: wl_output::Event,
        &index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(info) = state.outputs.get_mut(index) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => info.name = name,
            wl_output::Event::Description { description } => info.description = description,
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                // An output advertises every mode it supports; only the one
                // flagged `current` describes what we would actually capture.
                if let WEnum::Value(flags) = flags {
                    if flags.contains(wl_output::Mode::Current) {
                        info.width = width;
                        info.height = height;
                        info.refresh_mhz = refresh;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Connect to the compositor and enumerate its globals and outputs.
pub fn discover() -> Result<(Vec<GlobalInfo>, Vec<OutputInfo>)> {
    let conn = Connection::connect_to_env().context("connecting to Wayland compositor")?;
    let (globals, mut queue) =
        registry_queue_init::<Discovery>(&conn).context("initialising Wayland registry")?;
    let qh = queue.handle();

    let mut advertised: Vec<GlobalInfo> = globals.contents().with_list(|list| {
        list.iter()
            .map(|g| GlobalInfo {
                interface: g.interface.clone(),
                version: g.version,
            })
            .collect()
    });
    advertised.sort_by(|a, b| a.interface.cmp(&b.interface));

    let output_globals: Vec<(u32, u32)> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| (g.name, g.version))
            .collect()
    });

    let mut state = Discovery::default();
    let registry = globals.registry();
    for (name, version) in output_globals {
        let index = state.outputs.len();
        state.outputs.push(OutputInfo::default());
        // v4 is where `name`/`description` arrive; we never need anything newer.
        registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), &qh, index);
    }

    // Two round trips: the first delivers the bind, the second the resulting
    // property events up to `done`.
    queue.roundtrip(&mut state).context("wayland roundtrip")?;
    queue.roundtrip(&mut state).context("wayland roundtrip")?;

    Ok((advertised, state.outputs))
}
