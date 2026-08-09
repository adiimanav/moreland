//! Wayland screen capture for the Hyprland virtual output.
//!
//! Stage 1a: discovery only — enumerate globals and outputs so we can confirm
//! the compositor exposes everything the capture pipeline needs before we
//! build the pipeline on top of it.

pub mod discovery;
pub mod dmabuf;
/// KDE Plasma backend. Needs `plasma-wayland-protocols` at build time, so it
/// is behind a feature and Hyprland builds never see it.
#[cfg(feature = "plasma")]
pub mod plasma;
pub mod session;
pub mod shm;

/// Name we give the Hyprland virtual output. Hyprland accepts an explicit name
/// on `output create headless`, so we never have to guess `HEADLESS-N` — its
/// counter increments across creates and does not reset.
pub const VIRTUAL_OUTPUT_NAME: &str = "moreland";

/// `XR24` — opaque 8-bit BGRX, the format captured from the virtual output.
pub const XR24: u32 = u32::from_le_bytes(*b"XR24");

/// Fallback modifiers when the encoder cannot be probed.
///
/// Prefer [`encoder::supported_modifiers`], which asks the local VA stack what
/// it can import. A hardcoded set is correct on exactly one GPU: compositors
/// offer modifiers the encoder cannot read — on AMD, DCC-compressed tilings —
/// and GBM will happily prefer one, silently degrading the zero-copy path to a
/// CPU copy with no error.
pub const FALLBACK_MODIFIERS: &[u64] = &[
    0x0000_0000_0000_0000, // DRM_FORMAT_MOD_LINEAR — universally importable
];

/// Wayland globals the capture pipeline depends on.
///
/// `ext_output_image_capture_source_manager_v1` turns a `wl_output` into a
/// capture source; `ext_image_copy_capture_manager_v1` drives the actual frame
/// copies; `zwp_linux_dmabuf_v1` is what makes those copies zero-copy.
pub const REQUIRED_GLOBALS: &[&str] = &[
    "wl_shm",
    "zwp_linux_dmabuf_v1",
    "ext_image_copy_capture_manager_v1",
    "ext_output_image_capture_source_manager_v1",
];
