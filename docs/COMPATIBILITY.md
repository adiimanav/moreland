# Compatibility

**Verified on exactly one setup.** Everything else below is an assessment of
what would be required, not a claim that it works. Nothing here has been tested
on KDE Plasma, GNOME, or Sway.

## Verified

| | |
|---|---|
| Compositor | Hyprland 0.56.2 |
| GPU | AMD Cezanne / Vega (VCN 2.x), VA-API |
| Host OS | EndeavourOS / Arch, kernel 7.1 |
| Tablet | Xiaomi Pad 6 (Snapdragon 870), Android 14 |
| Link | USB 2.0 |

## What is actually compositor-specific

Less than you would expect. Three of the four pipeline stages are portable:

| Stage | Portability |
|---|---|
| Capture | `ext-image-copy-capture-v1` — a **standard** staging protocol, not wlroots-specific |
| Encode | VA-API via GStreamer — any GPU with a VA driver; modifiers are probed at runtime |
| Transport | ADB — identical everywhere |
| **Virtual output creation** | **compositor-specific — this is the whole problem** |

The compositor dependency is isolated in `crates/daemon/src/output.rs`, and the
interface is one idea: *create a headless output with this name and mode, and
remove it later*. Adding a compositor means implementing that and nothing else.

## Per-compositor assessment

### Hyprland — works

```bash
hyprctl output create headless moreland
hyprctl keyword monitor "moreland,1920x1200@60,1920x0,1"
hyprctl output remove moreland
```

Hyprland accepts an **explicit name**, which makes the result deterministic.
That matters more than it sounds: the unnamed form allocates `HEADLESS-N` from
a counter that persists across creates and never resets, so any code guessing
the name is a latent bug. It bit this project during development.

### Sway and wlroots compositors — likely straightforward, unimplemented

`ext-image-copy-capture-v1` is implemented by wlroots 0.18+, so **capture should
work unchanged**. Output creation exists too:

```bash
swaymsg create_output
```

The obstacle is naming: Sway names the output itself (`HEADLESS-N`) with no way
to choose, so the daemon must diff `swaymsg -t get_outputs` before and after to
learn what it just made. Mechanical, but it needs writing and testing.

Stub in place; `VirtualOutput::create` returns a clear error rather than
pretending.

### KDE Plasma / KWin — needs investigation

Two open questions, and I could not test either:

1. **Does KWin implement `ext-image-copy-capture-v1`?** KWin has historically
   exposed screen capture through its own `zkde_screencast_unstable_v1` and
   xdg-desktop-portal rather than the wlr/ext capture protocols. If it does not,
   the capture backend needs a second implementation (see *the portable route*
   below).
2. **How does KWin create a virtual output?** There is no `hyprctl` equivalent.
   KWin's DBus surface is the place to look.

Check the first question on a KDE system with:

```bash
wayland-info | grep -E 'image_copy_capture|image_capture_source'
```

If both appear, capture works as-is and only output creation needs writing.

### GNOME / Mutter — hardest

Mutter deliberately does **not** implement the wlr or ext capture protocols; it
exposes screen capture only through `org.gnome.Mutter.ScreenCast` and
xdg-desktop-portal. So the current capture backend cannot work at all — this is
not a small patch.

Virtual monitors are reachable through Mutter's remote-desktop DBus interfaces
in recent GNOME versions, but pairing that with portal-based capture is
essentially a second backend.

## The portable route: xdg-desktop-portal + PipeWire

If broad compositor support matters more than the last few milliseconds, the
answer is the portal:

- **Works everywhere** — GNOME, KDE, wlroots, all of it
- Still delivers **DMA-BUF**, so the zero-copy path into VA-API survives
- Costs a permission dialog on first use (avoidable afterwards with a restore
  token) and a small amount of latency for the extra PipeWire hop

That would make the capture stage universal, leaving only virtual-output
creation per-compositor. It is the right move if this project wants to support
more than one desktop, and it is not currently implemented.

## GPU compatibility

Should be broader than the compositor story.

The encoder chain is `vapostproc` → `vah264enc`, which works on any GPU with a
VA-API driver — AMD (VCN), Intel (QuickSync), and NVIDIA via `nvidia-vaapi-driver`.

The one thing that *was* hardcoded is now probed at runtime:
`encoder::supported_modifiers()` asks the local VA stack which DRM format
modifiers it can import, instead of assuming AMD's. This matters because
compositors offer modifiers the encoder cannot read — on AMD, DCC-compressed
tilings — and GBM will happily prefer one. The failure mode is not an error but
a **silent fallback to a CPU copy**, which quietly destroys the zero-copy
design. A hardcoded modifier is correct on exactly one GPU.

Untested on Intel and NVIDIA. The probing makes it plausible, not proven.

## Android compatibility

The most portable part of the project.

- **API 29+** (`minSdk`), tested on API 34
- Needs a hardware H.264 decoder — universal on anything from the last decade
- Nothing vendor-specific is required. The Qualcomm low-latency hint is set
  opportunistically and ignored elsewhere

The default **1920×1200** is a clean 1.5× upscale for the Xiaomi Pad 6's
2880×1800 16:10 panel. For a 16:9 tablet, prefer `--width 1920 --height 1080`;
a mismatched aspect ratio will letterbox.

USB 2.0 was the measured link here and had **13× headroom**, so USB 3 devices
gain nothing — bandwidth was never the constraint.

## Contributing a compositor backend

1. Confirm the capture protocols exist:
   `wayland-info | grep -E 'image_copy_capture|image_capture_source'`
2. Add a variant to `Compositor` in `crates/daemon/src/output.rs` and detect it
   from the environment
3. Implement create/remove for it
4. Verify capture alone first — `capture-probe <output> --dmabuf --frames 120`
   isolates the compositor from the rest of the pipeline

Please report what you find even if it does not work; a documented failure is
more useful than silence.
