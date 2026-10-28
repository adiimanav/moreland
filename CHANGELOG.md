# Changelog

All notable changes to moreland are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/). Before 1.0, a minor bump may change
defaults or flags.

## [0.2.0] - Unreleased

The daemon and the Android app still speak the same wire protocol as 0.1.0,
so either side can be upgraded on its own.

### Added

- **labwc backend.** labwc cannot create outputs at runtime, so moreland
  attaches to a headless output that already exists, set up through
  `WLR_HEADLESS_OUTPUTS`, and leaves it in place on exit. Use it with
  `--output-name HEADLESS-N`. See [COMPATIBILITY.md](docs/COMPATIBILITY.md).
  (#10)
- `--output-name` overrides the virtual output's name (the default is
  `moreland`). (#10)
- `--show-cursor` paints the mouse cursor into the stream. It is off by
  default. (#10)
- `--position-y` sets the vertical offset of the virtual output, alongside
  `--position`. (#8)
- `--transform <0-7>` rotates the virtual output, for example `1` for a tablet
  on its side. The stream size does not change. (#12)
- `--print-rule` prints the static monitor rule for both `hyprland.conf` and
  `hyprland.lua`. It now echoes every argument it was given, so re-running the
  printed command reproduces the same rule.
- `scripts/moreland-doctor.sh` checks the compositor, the capture protocol,
  the VA-API encoder and the ADB link, and names whatever blocks moreland on
  the machine.
- **KDE Plasma groundwork (experimental, not usable yet).** An off-by-default
  `plasma` Cargo feature and a `plasma-probe` binary can create a real
  virtual output on KWin 6.7.4 and get its PipeWire node. Nothing reaches the
  tablet yet, because the PipeWire capture side is unwritten, and the daemon
  still refuses to start on KDE. On a Plasma session, `install.sh` installs
  the desktop entry that KWin requires for the screencast interface. See
  [06-plasma-backend.md](docs/06-plasma-backend.md).

### Changed

- **Default refresh rate is now 90 fps** (was 60). Pass `--fps 60` to keep the
  old behaviour. (#2)
- **Default position is now below the primary display**, at `0,1080` (was
  `1920,0`, to the right). This assumes a primary no taller than 1080 px; use
  `--position` and `--position-y` to place the output elsewhere. (#7, #8)
- The daemon exits with an error on an unsupported compositor. Before, it
  retried forever: on one KDE login it spun every 16 seconds for 25 hours.
- On labwc, `--transform` is applied at every session start, so a rotation
  left over from an earlier run is reset.

### Fixed

- Each captured DMA-BUF frame now carries its plane offset and stride to the
  encoder. Before, the encoder assumed a tightly packed buffer at offset 0.
  (#6)
- Hyprland: the other monitors' layout is saved when the session starts and
  restored when it ends. (#7)
- Hyprland with the Lua config parser: monitor rules are re-issued through
  `hyprctl eval`. Before, the parser rejected them while reporting success,
  so the output kept the compositor's defaults.
- A daemon started by systemd no longer trusts a stale
  `HYPRLAND_INSTANCE_SIGNATURE` left over from an exited session. Each
  compositor marker is confirmed with a live IPC round-trip, so a later KDE
  session reports "unsupported compositor" instead of a confusing `hyprctl`
  failure.

### Documentation

- Added [06-plasma-backend.md](docs/06-plasma-backend.md) and expanded
  [COMPATIBILITY.md](docs/COMPATIBILITY.md) with KDE, labwc and
  distribution guidance.
- [REVERT.md](docs/REVERT.md) covers the desktop entry and resetting a
  rotated output.
- Source files carry SPDX license identifiers.

## [0.1.0] - 2026-08-08

First public release: an Android tablet as a wired second monitor for
Hyprland over USB.

- Plug-and-play: a virtual monitor appears when the tablet is connected and
  disappears when it is unplugged.
- Zero-copy pipeline: DMA-BUF capture through `ext-image-copy-capture-v1`,
  VA-API hardware encode, USB through `adb forward` (loopback only), and
  hardware decode on the tablet.
- The resolution is matched to the tablet panel's aspect ratio, with
  `--native`, `--width`/`--height` and `--max-width` overrides.
- Verified on Hyprland, AMD VA-API and a Xiaomi Pad 6: about 33–48 ms
  glass-to-glass, 60 fps at 1920x1200, 16 Mbps.

[0.2.0]: https://github.com/adiimanav/moreland/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/adiimanav/moreland/releases/tag/v0.1.0
