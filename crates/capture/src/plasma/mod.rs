//! KDE Plasma capture backend — virtual output half.
//!
//! KWin implements no `ext-` or `wlr-` capture protocol and KDE has declined
//! to add one, so [`crate::session`] cannot work there at all. What KWin does
//! offer is `zkde_screencast_unstable_v1`, whose `stream_virtual_output`
//! request **creates a virtual output and returns a PipeWire node for it in a
//! single call** — the two problems the project treated separately on Hyprland
//! collapse into one here.
//!
//! This module is that call — plus a workaround, because on KWin 6.7.4 the
//! request creates the output *disabled* and then fails to find it. See
//! [`KWIN_ORPHANED_OUTPUT`]. Consuming the PipeWire node is the other half and
//! is not implemented yet; see `docs/06-plasma-backend.md`.
//!
//! **The protocol is privileged.** KWin only advertises the global to a client
//! whose desktop entry declares:
//!
//! ```ini
//! X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1
//! ```
//!
//! with an **absolute** `Exec` path. Get either wrong and the global is simply
//! absent — there is no error, no log line, nothing to grep for. That is why
//! [`PlasmaVirtualOutput::create`] spends its error message on the grant
//! rather than saying "global not found".
//!
//! The output's lifetime is the Wayland connection's. Dropping this struct, or
//! losing the connection, removes the monitor.

use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

include!(concat!(env!("OUT_DIR"), "/plasma_protocol.rs"));

use zkde_screencast::zkde_screencast_stream_unstable_v1::{
    Event as StreamEvent, ZkdeScreencastStreamUnstableV1,
};
use zkde_screencast::zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1;

/// How the cursor is handled in the stream. Mirrors the protocol's `pointer`
/// enum, which is a bitmask but is used as a mode in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CursorMode {
    /// No cursor in the stream.
    Hidden = 1,
    /// Cursor composited into the frames. What a second monitor wants: the
    /// pointer has to be visible once it moves onto the tablet.
    Embedded = 2,
    /// Cursor position sent as PipeWire metadata, for the consumer to draw.
    Metadata = 4,
}

/// The global's name, repeated here so the error path can name it exactly.
pub const INTERFACE: &str = "zkde_screencast_unstable_v1";

/// `stream_virtual_output` arrived in version 2; nothing below that is usable.
const MIN_VERSION: u32 = 2;
const MAX_VERSION: u32 = 6;
/// `stream_virtual_output_with_description` arrived in version 4.
const DESCRIPTION_SINCE: u32 = 4;

/// Give KWin a bounded window to answer with `created`/`serial`/`failed`.
const CREATE_TIMEOUT: Duration = Duration::from_secs(5);

/// KWin 6.7.4 creates the virtual output, fails to look it back up, and
/// reports this while leaving the output in place. Reproduced with KDE's own
/// `krfb-virtualmonitor`, so it is the compositor's bug and not ours. Matched
/// as a string because the protocol offers no error code.
const KWIN_ORPHANED_OUTPUT: &str = "Could not find output";

/// How long to wait for an enabled output to be advertised as a global.
const ENABLE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Default)]
struct State {
    node_id: Option<u32>,
    /// Preferred over `node_id` since version 6: node ids are recycled.
    object_serial: Option<u64>,
    failed: Option<String>,
    closed: bool,
    /// Only populated by [`PlasmaVirtualOutput::mirror`].
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,
}

impl State {
    /// Whether the compositor has said anything conclusive yet.
    fn settled(&self) -> bool {
        self.node_id.is_some() || self.failed.is_some() || self.closed
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, usize> for State {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // `name` is the connector string (eDP-1, HDMI-A-1); since version 4.
        if let wl_output::Event::Name { name } = event {
            if let Some(slot) = state.outputs.get_mut(*index) {
                slot.0 = proxy.clone();
                slot.1 = Some(name);
            }
        }
    }
}

impl Dispatch<ZkdeScreencastUnstableV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZkdeScreencastUnstableV1,
        _event: <ZkdeScreencastUnstableV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The manager has no events.
    }
}

impl Dispatch<ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ZkdeScreencastStreamUnstableV1,
        event: StreamEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            StreamEvent::Created { node } => state.node_id = Some(node),
            StreamEvent::Serial {
                object_serial_hi,
                object_serial_low,
            } => {
                state.object_serial =
                    Some((u64::from(object_serial_hi) << 32) | u64::from(object_serial_low));
            }
            StreamEvent::Failed { error } => state.failed = Some(error),
            StreamEvent::Closed => state.closed = true,
        }
    }
}

/// Bind every `wl_output` and return the one with this connector name.
///
/// Rebinds from scratch each call rather than caching: a virtual output only
/// becomes a global at the moment it is created, so a list captured earlier
/// will not contain the very output being looked for.
fn find_output(
    globals: &GlobalList,
    queue: &mut EventQueue<State>,
    state: &mut State,
    qh: &QueueHandle<State>,
    wanted: &str,
    timeout: Duration,
) -> Result<wl_output::WlOutput> {
    let deadline = Instant::now() + timeout;

    loop {
        // The advertisement is asynchronous, and after enabling an output it
        // can trail the request by a beat, so poll rather than look once.
        queue.roundtrip(state).context("syncing the registry")?;

        state.outputs.clear();
        let registry = globals.registry();
        for global in globals
            .contents()
            .clone_list()
            .iter()
            .filter(|g| g.interface == "wl_output")
        {
            // The connector name arrives in wl_output::name, added in v4.
            if global.version < 4 {
                continue;
            }
            let index = state.outputs.len();
            let output: wl_output::WlOutput = registry.bind(global.name, 4, qh, index);
            state.outputs.push((output, None));
        }
        queue.roundtrip(state).context("enumerating outputs")?;

        if let Some((proxy, _)) = state
            .outputs
            .iter()
            .find(|(_, name)| name.as_deref() == Some(wanted))
        {
            return Ok(proxy.clone());
        }

        if Instant::now() >= deadline {
            let seen: Vec<_> = state.outputs.iter().filter_map(|(_, n)| n.clone()).collect();
            bail!("no output named {wanted:?} after {timeout:?}; found {seen:?}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Enable a KWin output that exists but is switched off.
///
/// Shelling out to `kscreen-doctor` mirrors what the Hyprland backend already
/// does with `hyprctl`, and KWin exposes no Wayland request a normal client
/// can use for this: `kde_output_management_v2` would mean reimplementing the
/// whole configuration-apply dance for one boolean.
fn enable_output(wl_name: &str) -> Result<()> {
    let output = std::process::Command::new("kscreen-doctor")
        .arg(format!("output.{wl_name}.enable"))
        .output()
        .context("running kscreen-doctor (part of libkscreen; needed on KDE)")?;
    if !output.status.success() {
        bail!(
            "kscreen-doctor could not enable {wl_name}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Pump events until the compositor says something conclusive about a stream.
///
/// The deadline is checked between dispatches rather than enforced against a
/// blocking read, so it bounds a compositor that answers slowly rather than
/// one that never answers at all. KWin always answers.
fn settle(
    queue: &mut EventQueue<State>,
    state: &mut State,
    stream: &ZkdeScreencastStreamUnstableV1,
    what: &str,
) -> Result<()> {
    let deadline = Instant::now() + CREATE_TIMEOUT;
    while !state.settled() {
        if Instant::now() >= deadline {
            stream.close();
            bail!("timed out after {CREATE_TIMEOUT:?} waiting for KWin to answer for {what:?}");
        }
        queue
            .blocking_dispatch(state)
            .context("dispatching Wayland events while awaiting the stream")?;
    }
    if let Some(error) = &state.failed {
        bail!("KWin refused {what:?}: {error}");
    }
    if state.closed {
        bail!("KWin closed the stream for {what:?} before it was created");
    }
    Ok(())
}

/// A KWin virtual output and its PipeWire stream, removed when dropped.
pub struct PlasmaVirtualOutput {
    // Field order is the drop order, and it matters: the stream must be closed
    // before the connection it lives on goes away.
    stream: ZkdeScreencastStreamUnstableV1,
    manager: ZkdeScreencastUnstableV1,
    queue: EventQueue<State>,
    state: State,
    _conn: Connection,
    name: String,
    node_id: u32,
}

impl PlasmaVirtualOutput {
    /// Ask KWin for a virtual output of this size and a PipeWire feed of it.
    ///
    /// `scale` is the output's scaling factor, not a resolution multiplier:
    /// the monitor is `width` x `height` logical pixels either way.
    pub fn create(
        name: &str,
        description: &str,
        width: i32,
        height: i32,
        scale: f64,
        cursor: CursorMode,
    ) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).context("initialising the Wayland registry")?;
        let qh = queue.handle();

        let manager: ZkdeScreencastUnstableV1 = globals
            .bind(&qh, MIN_VERSION..=MAX_VERSION, ())
            .map_err(|e| {
                anyhow::anyhow!(
                    "{INTERFACE} is not available ({e}).\n\
                     KWin only advertises it to a client whose desktop entry declares\n\
                     \x20   X-KDE-Wayland-Interfaces={INTERFACE}\n\
                     with an absolute Exec path, and it denies it silently otherwise.\n\
                     Check `scripts/moreland-doctor.sh`, and remember KWin reads the\n\
                     KService cache: run `kbuildsycoca6 --noincremental` after any change.\n\
                     If you are not on KDE, this backend is the wrong one."
                )
            })?;

        let mut state = State::default();

        // KWin's internal handler takes a description as well as a name, and
        // only `stream_virtual_output_with_description` supplies one. The
        // plain request exists since version 2 but is the poorer path, so
        // prefer the descriptive form wherever the compositor offers it.
        let stream = if manager.version() >= DESCRIPTION_SINCE {
            manager.stream_virtual_output_with_description(
                name.to_string(),
                description.to_string(),
                width,
                height,
                scale,
                cursor as u32,
                &qh,
                (),
            )
        } else {
            manager.stream_virtual_output(
                name.to_string(),
                width,
                height,
                scale,
                cursor as u32,
                &qh,
                (),
            )
        };
        conn.flush().ok();

        // KWin 6.7.4 creates the output and *then* fails to associate a stream
        // with it, reporting "Could not find output" while leaving a perfectly
        // good monitor behind. KDE's own krfb-virtualmonitor hits this too, so
        // it is a compositor bug rather than a misuse of the protocol.
        //
        // The output is real and lives as long as this connection, so recover
        // by streaming it the ordinary way. Note the failed stream is
        // deliberately not closed: closing it is what removes the output.
        let stream = match settle(&mut queue, &mut state, &stream, name) {
            Ok(()) => stream,
            Err(_) if state.failed.as_deref() == Some(KWIN_ORPHANED_OUTPUT) => {
                // The output exists but KWin left it *disabled*, and a disabled
                // output has no LogicalOutput — which is precisely why
                // findOutput failed and why no wl_output global appeared.
                // Switching it on completes what the compositor started.
                let wl_name = format!("Virtual-{name}");
                tracing::warn!(
                    "KWin created {wl_name} disabled and reported \
                     {KWIN_ORPHANED_OUTPUT:?}; enabling it and streaming directly"
                );
                enable_output(&wl_name)?;

                let target =
                    find_output(&globals, &mut queue, &mut state, &qh, &wl_name, ENABLE_TIMEOUT)
                        .with_context(|| {
                            format!("recovering from {KWIN_ORPHANED_OUTPUT:?} for {name:?}")
                        })?;

                state.failed = None;
                state.closed = false;
                let recovered = manager.stream_output(&target, cursor as u32, &qh, ());
                conn.flush().ok();
                settle(&mut queue, &mut state, &recovered, &wl_name)?;
                recovered
            }
            Err(e) => return Err(e),
        };

        let node_id = state
            .node_id
            .context("KWin reported neither a node nor a failure")?;

        tracing::info!(
            "created virtual output {name:?} {width}x{height}@{scale} \
             (pipewire node {node_id}, serial {:?})",
            state.object_serial
        );

        Ok(Self {
            stream,
            manager,
            queue,
            state,
            _conn: conn,
            name: name.to_string(),
            node_id,
        })
    }

    /// Stream an **existing** output instead of creating one.
    ///
    /// This is not a second monitor — it mirrors a display you already have.
    /// It exists as a diagnostic: it exercises the identical binding, grant
    /// and event path as [`Self::create`], differing only in which request is
    /// sent. If this succeeds while `create` fails, the client side is proven
    /// correct and the fault is in the compositor's virtual-output creation.
    pub fn mirror(output_name: &str, cursor: CursorMode) -> Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).context("initialising the Wayland registry")?;
        let qh = queue.handle();
        let mut state = State::default();

        let target = find_output(
            &globals,
            &mut queue,
            &mut state,
            &qh,
            output_name,
            Duration::ZERO,
        )?;

        let manager: ZkdeScreencastUnstableV1 = globals
            .bind(&qh, MIN_VERSION..=MAX_VERSION, ())
            .map_err(|e| anyhow::anyhow!("{INTERFACE} is not available ({e})"))?;

        let stream = manager.stream_output(&target, cursor as u32, &qh, ());
        conn.flush().ok();
        settle(&mut queue, &mut state, &stream, output_name)?;

        let node_id = state
            .node_id
            .context("KWin reported neither a node nor a failure")?;
        tracing::info!("mirroring {output_name} (pipewire node {node_id})");

        Ok(Self {
            stream,
            manager,
            queue,
            state,
            _conn: conn,
            name: output_name.to_string(),
            node_id,
        })
    }

    /// PipeWire node carrying this output's frames.
    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    /// PipeWire object serial, when the compositor is new enough to send one.
    ///
    /// Prefer this over [`Self::node_id`] for identifying the stream: node ids
    /// are recycled, which is why the protocol deprecated `created` at v6.
    pub fn object_serial(&self) -> Option<u64> {
        self.state.object_serial
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Drain pending events. Returns `false` once KWin has closed the stream,
    /// which is how an externally removed output is noticed.
    pub fn poll(&mut self) -> Result<bool> {
        self.queue
            .dispatch_pending(&mut self.state)
            .context("dispatching Wayland events for the virtual output")?;
        Ok(!self.state.closed)
    }
}

impl Drop for PlasmaVirtualOutput {
    fn drop(&mut self) {
        // Closing the stream is what removes the monitor; without it KWin
        // keeps a phantom output until the connection drops.
        self.stream.close();
        self.manager.destroy();
        if let Err(e) = self.queue.flush() {
            tracing::warn!("failed to flush the removal of {}: {e}", self.name);
        } else {
            tracing::info!("removed virtual output {}", self.name);
        }
    }
}
