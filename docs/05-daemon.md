# Stage 5 — Daemon and Automation

**Status: complete and verified.**

Turn the pipeline into the plug-and-play behaviour the project set out to build:
plug the tablet in and a monitor appears, unplug it and the monitor disappears.

## Device detection: `adb track-devices`, not udev

The ADB server exposes `host:track-devices`, which streams a fresh device list
every time one changes. The daemon holds that connection open and blocks on it.

This was chosen over udev for a concrete reason found in Stage 0: **the tablet's
USB product ID is not stable.** It moved `2717:ff40` → `2717:ff48` simply from
enabling USB debugging, and changing the USB mode moves it again. A udev rule
matching on product ID would silently stop firing.

`track-devices` matches on ADB's own notion of device state, which is what
actually determines whether we can forward to it. It also distinguishes
`device` from `unauthorized` and `offline` — states that look connected but
reject every request.

Cost of holding the connection: nothing measurable. The daemon is blocked in a
read.

### Protocol

ADB frames every message as a 4-digit hex length followed by the payload:

```
-> 0012host:track-devices
<- OKAY
<- 0016ABCD1234<TAB>device<LF>      (repeated on every change)
```

Implemented directly against port 5037 in `tracker.rs` rather than shelling out
to `adb devices` on a timer — no polling latency, no wakeups.

## Lifecycle

Everything the daemon creates is RAII-scoped, so a session cleans up however it
ends — normally, by error, or by signal:

| Resource | Guard | Removed by |
|---|---|---|
| Hyprland virtual output | `VirtualOutput` | `Drop` → `hyprctl output remove` |
| `adb forward` rule | `adb::Forward` | `Drop` → `adb forward --remove` |
| Encoder / capture session | `Encoder`, `Capture` | `Drop` |
| Tablet app | explicit | `am force-stop` at session end |

`SIGTERM` and `SIGINT` set a shutdown flag that the capture loop checks, so
stopping the service unwinds those guards rather than killing the process
mid-session. **This matters:** a hard kill leaves Hyprland with a phantom
monitor that has to be removed by hand. The unit sets `TimeoutStopSec=15` to
give the unwind room.

## Verification

### Automatic detection

```
INFO moreland: watching for devices
INFO moreland: device ABCD1234 connected
INFO moreland::session: virtual output moreland at 1920x1200@60
INFO moreland::session: streaming to ABCD1234
```

### Recovery from device loss

Killing the ADB server mid-session is the same failure shape as a physical
unplug: the stream breaks and the tracker's connection dies. Observed:

```
streaming to ABCD1234
removed output moreland
session ended: sending frame: Broken pipe (os error 32)
device tracker lost: reading ADB length; reconnecting
device ABCD1234 connected
streaming to ABCD1234            <- resumed with no intervention
removed output moreland
session ended cleanly
shutting down
```

After `SIGTERM`:

```
monitors with moreland: 0
adb forwards: []
```

No residue.

> Physical unplug/replug was not tested here — it needs hands on the cable.
> The failure path is identical (device disappears from `track-devices`, socket
> breaks), and that path is verified, but the physical case is worth confirming
> once.

### Steady state

Three consecutive 8-second runs under identical conditions:

| Run | min | median |
|---|---|---|
| 1 | 19.93 ms | 23.81 ms |
| 2 | 19.51 ms | 23.81 ms |
| 3 | 19.90 ms | 23.76 ms |

Reproducible to within 0.05 ms. An earlier run measured 33.4 ms — that was a
**cold-start outlier**, taken immediately after launching the content window,
and is not representative. Recording it here rather than quietly keeping the
better number: the honest figure is ~23.8 ms with occasional cold-start
excursions.

Tablet panel confirmed at **144 Hz** during streaming.

## Installing

```bash
./install.sh
```

Nothing needs root and nothing is written outside `$HOME`:

- `~/.local/bin/moreland`
- `~/.config/systemd/user/moreland.service`

The installer deliberately **does not build the APK** — that needs the Android
SDK, and a stale APK is worse than an absent one. It checks whether the app is
installed and prints the commands if not.

Start on login:

```bash
systemctl --user enable --now moreland.service
journalctl --user -u moreland -f
```

The unit is `PartOf=graphical-session.target`, so it starts with the session and
stops with it. It needs a running compositor — `hyprctl` has to find the
instance, and the daemon connects to Wayland as a capture client.

## Usage

```
moreland                 watch for the tablet; stream whenever plugged in
moreland --once          stream one session, then exit
moreland --seconds 15    stop after 15 s, print latency statistics
moreland --stats         print latency statistics on exit
moreland --max-width 2400        cap the auto-detected width
moreland --native                stream the tablet's full panel resolution
moreland --width 2400 --height 1500   pin an explicit size (both required)
moreland --bitrate 30000
moreland --transform 1 --position -1200 --position-y -840   upright tablet
moreland --transform 1 --print-rule  the matching Hyprland config line
```

## Rotated output (vertical layout)

A tablet panel is native landscape even when the tablet stands upright, so the
compositor is asked for the panel's normal mode and rotates it. The rotation
happens in the compositor from the captured framebuffer, not by scaling the
image, so a rotated stream is as sharp as an unrotated one.

`--transform` takes Hyprland's ids, 0-7; odd ids are quarter turns and swap the
logical width and height. Anything else is rejected with a warning and falls
back to 0 rather than being handed to the compositor.

Position is in the **post-rotation** layout. A `1920x1200` panel at transform 1
becomes a `1200x1920` output, so to sit left of a 1080-tall primary with the
bottom edges flush:

```
x = -1200                  one output width to the left
y = 1080 - 1920 = -840     primary height minus this output's height
```

`--max-width` still caps the panel's long edge, which stays the width of the
compositor mode even when the output is turned. Rotation therefore does not
change how many pixels are streamed, only which way they face; use `--width`
and `--height` if you want a specific stream size.

The daemon issues the rule on every connect. A static rule in the Hyprland
config is only the fallback for `hyprctl reload`, which reapplies the config
file and would otherwise reset the output to landscape: a `monitor=` line in
`hyprland.conf`, or an `hl.monitor({ ... })` call in `hyprland.lua` if the
compositor runs the Lua parser, which accepts neither `monitor=` lines nor
`hyprctl keyword`. Generate the rule instead of hand-writing it, so the two
cannot drift:

```
moreland --transform 1 --position -1200 --position-y -840 --print-rule
```

`--print-rule` prints the same layout three ways, each block commented in its
own syntax so it can be pasted as-is: the `monitor=` line for `hyprland.conf`,
the `hl.monitor` call for `hyprland.lua`, and, as a comment, the `wlr-randr`
command the daemon runs on labwc. It does not touch the running compositor.
Each block carries a `generated by:` line echoing the full command, so a stale
rule can be regenerated by running it again. The printed rule and the one the
daemon sends are built from the same `Layout` and rendered by `display.rs`,
which is the single place geometry is turned into a compositor dialect.

On labwc, where the output belongs to the session, only the rotation is applied
(`wlr-randr --output <name> --transform <value>`); mode and position stay as you
configured them, for the reason given in `COMPATIBILITY.md`. The rotation is set
on every session start, transform 0 included, so `--transform 0` turns an
output back to `normal` after an earlier rotated run.

Verified on the reference machine: `1920x1200@90` at `-1200x-840` with
transform 1, surviving both a daemon restart and `hyprctl reload`, with `grim -o
moreland` producing a `1200x1920` capture.

## Code

```
crates/daemon/
  src/main.rs         daemon loop, CLI, signal handling
  src/display.rs      geometry -> Hyprland rule / Lua / wlr-randr
  src/tracker.rs      ADB host:track-devices client
  src/session.rs      one streaming session, RAII-scoped
  src/output.rs       virtual output guard, per-compositor
  src/usage.txt
systemd/moreland.service
install.sh
```
