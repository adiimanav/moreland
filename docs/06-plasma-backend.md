# Stage 6 — KDE Plasma backend

**Status: working end to end.** `moreland` run on KDE Plasma creates a real
second monitor, captures it through PipeWire, encodes it and streams it to the
tablet. Build with `cargo build --release --features plasma`.

Verified on KWin 6.7.4 with an NVIDIA GTX 1650 (NVENC) and a Galaxy Tab S9 FE+
over USB. This is **one setup**; treat everything below as measured there.

```console
$ moreland --once --seconds 40 --fps 60
  frames captured   1259
  bytes sent        51.7 MB
  round trip: host send -> device render -> host ack
    min           9.11 ms
    median       22.88 ms
    p95          42.83 ms
    max          93.27 ms
```

The sections below were written while the PipeWire half was still missing, and
record how the Wayland half was established. They are kept because the KWin
findings are still true; where they say "not implemented", read "see *What
was built* at the end".

Getting there needed a workaround for a KWin bug, described below.

## Why the existing capture path cannot work

KWin implements neither `ext-image-copy-capture-v1` nor the wlr equivalents,
and this is a decision rather than a gap. On [bug 513785][bug], David
Edmundson:

> Our priority is portals and improving this experience. Supporting two methods
> fragments this, we won't be adding this.

Verified on KWin 6.7.4:

```console
$ wayland-info | grep -E 'image_copy_capture|image_capture_source'
$ grep -rl ext_image_copy_capture_manager_v1 /usr/lib/ /usr/bin/
$
```

Absent from the registry *and* from every installed binary, so this is not a
privileged-client restriction that a different client could work around.
`zwlr_screencopy_manager_v1` is absent too — no legacy fallback. There is
nothing for `crates/capture` to bind to, and no change to `output.rs` alters
that.

`zwp_linux_dmabuf_v1` v5 **is** present, so the zero-copy import survives.
Capture is the only broken stage; see *What already works* below.

[bug]: https://bugs.kde.org/show_bug.cgi?id=513785

## The mechanism KDE does provide

`zkde_screencast_unstable_v1` (v6 on KWin 6.7.4) has a `stream_virtual_output`
request that **creates a virtual output and returns its PipeWire node in one
call**. This is what KDE's own KRDP uses:

```cpp
d->request = d->m_screencasting.createVirtualMonitorStream(
    vm->name, vm->size, vm->dpr, Screencasting::Metadata);
connect(d->request, &ScreencastingStream::created, this, [this](uint nodeId) {
    setNodeId(nodeId);
});
```

So the two problems `COMPATIBILITY.md` originally posed separately — capture,
and virtual-output creation — collapse into one call.

**KRDP ships both `PortalSession.cpp` and `PlasmaScreencastV1Session.cpp`.**
KDE's own developers did not find the portal sufficient, almost certainly
because virtual monitors are not reachable through it. Locally that matches:
the ScreenCast portal reports `AvailableSourceTypes = 0` and
`xdg-desktop-portal-kde` contains no virtual-monitor symbols. **For a second
screen on KDE, the Plasma protocol is the path that works and the portal is
the path that does not.**

## The grant, and its two traps

The protocol is privileged: a client that does not declare it never sees the
global. KWin's rejection path is `not in X-KDE-Wayland-Interfaces of`, and it
produces **no diagnostic the client can observe** — the global is simply
missing. Spectacle, plasmashell and `xdg-desktop-portal-kde` all declare it.

Established by an A/B/A test against a binary in `~/.local/bin`:

| Condition | Result |
|---|---|
| `.desktop` present, absolute `Exec` | granted (`zkde_screencast_unstable_v1` v6) |
| `.desktop` removed, `kbuildsycoca6` re-run | denied |
| Fresh binary that never had a `.desktop` | denied |
| `.desktop` restored | granted |

Three findings, all load-bearing:

1. **No root.** A user entry under `~/.local/share/applications` is honoured,
   so `install.sh` keeps its "nothing outside `$HOME`" promise.
2. **`Exec` must be an absolute path.** `Exec=wayland-info` is silently not
   matched; `Exec=/usr/bin/wayland-info` is. This cost an hour and produces no
   error anywhere — the interface is just absent.
3. **KWin matches on the client's executable path, not how it was launched.**
   The probe was granted the interface when run straight from a shell rather
   than through its desktop entry, so a daemon started by systemd is the same
   case. This was the open question that gated the whole design.

A fourth, procedural: **KWin reads the KService cache, not the directory.** A
stale cache both hides a new grant and preserves a revoked one — the first
control run in this investigation produced a false positive for exactly that
reason. Always `kbuildsycoca6 --noincremental` before trusting a result.

## What already works on KDE

Measured on this machine, with capture bypassed by feeding the device a
synthetic H.264 stream over the real wire protocol:

| Stage | Result |
|---|---|
| Capture | **fails** — `missing required globals` |
| Encode (VA-API) | 300 frames in 4.2 s, ~71 fps at 1920×1200 |
| Transport (ADB/USB) | 10.3 MB byte-exact, median `write()` 0.053 ms |
| Device app | 599/600 frames acknowledged at a sustained 60 fps |

Three of four stages are already healthy. The backend is the only gap.

## Design

Mirror KRDP's structure rather than assuming one universal portal backend:

```
crates/capture/src/
  lib.rs        trait FrameSource { fn next_frame(&mut self) -> Result<DmabufFrame> }
  ext/          existing ext-image-copy-capture — Hyprland/wlroots, still the default
  plasma/       NEW  zkde_screencast + PipeWire — KDE, no dialog, virtual output included
  portal/       LATER  ScreenCast/RemoteDesktop + PipeWire — GNOME, sandboxed fallback
```

`crates/encoder` should need **no changes**: PipeWire delivers
`SPA_DATA_DmaBuf` buffers, which are the same fds and DRM modifiers
`Encoder::push_frame` already dups and imports. That is the whole reason this
is tractable.

`output.rs` gains a `Kwin` variant whose create/drop are protocol calls rather
than `hyprctl`, and whose lifetime is the screencast stream itself.

New dependencies: `pipewire`, a Wayland binding generated from
`zkde-screencast-unstable-v1.xml` (ships in `plasma-wayland-protocols`, **not
installed by default**).

## What was built, and what it proved

`crates/capture/src/plasma/`, behind an off-by-default `plasma` cargo feature.
The feature gates a build-time dependency on the protocol XML, so a Hyprland
build is unchanged — verified by building the daemon with
`MORELAND_ZKDE_SCREENCAST_XML` pointed at a nonexistent file.

The XML is **not vendored**: it is LGPL-2.1-or-later against this project's
Apache-2.0, so `build.rs` locates the system copy and writes a wrapper module
into `OUT_DIR` with the resolved path baked in. `wayland-scanner`'s macros take
a path literal, which a discovered path cannot otherwise be.

`plasma-probe` also has a `--mirror` mode that streams an existing output. It
was built to isolate a client bug from a compositor one, and it did exactly
that: mirroring `eDP-1` returned a node while creating a virtual output failed,
through identical binding, grant, event handling and timeout logic. It remains
useful, both as a diagnostic and as the shape of a future mirroring feature.

`WAYLAND_DEBUG=1` confirmed the failing request was well formed:

```
-> zkde_screencast_unstable_v1@3.stream_virtual_output_with_description(
     stream@5, "moreland", "Moreland tablet monitor", 1920, 1200, 1.0000, 2)
<- zkde_screencast_stream_unstable_v1@5.failed, ("Could not find output")
```

**This means the grant mechanism is fully validated on a real project binary.**
The desktop entry, the absolute-`Exec` requirement and the executable-path
matching all work exactly as documented above.

## The KWin bug, and the workaround

`stream_virtual_output` fails with `Could not find output` — at every size and
scale, and for both request variants. In
`src/plugins/screencast/screencastmanager.cpp`:

```cpp
auto output = kwinApp()->outputBackend()->createVirtualOutput(name, description, size, scale);
streamOutput(stream, workspace()->findOutput(output), mode);   // -> null
```

**KDE's own `krfb-virtualmonitor` fails identically**, with the same single
request and the same error, which is what established this as a compositor bug
rather than a misuse of the protocol.

The diagnosis came from noticing that `kscreen-doctor` listed the output
anyway:

```
Output: 1 Virtual-vtest   disabled   connected   Modes: 1920x1200@60
```

**The output is created, but disabled.** A disabled output has no
`LogicalOutput`, so `workspace()->findOutput()` returns null — and it is never
advertised as a `wl_output` global either, which is the same fact seen from
the client side. Every symptom follows from that one thing.

So the workaround is to finish what KWin started: enable the output, wait for
its global to appear, and stream it with the ordinary `stream_output` request,
which has always worked. `enable_output` shells out to `kscreen-doctor`,
mirroring what the Hyprland backend already does with `hyprctl`; KWin exposes
no request a normal client can use for this short of reimplementing
`kde_output_management_v2`.

**KWin remembers the enabled state in memory, not on disk.** All virtual
outputs share one KScreen UUID, so once any of them has been enabled, later
ones in that session are created enabled and the first request simply succeeds
— which is why the failure can look intermittent. Nothing is written under
`~/.local/share/kscreen`, so a fresh login starts cold and fails again. Both
paths are verified: the cold one by disabling the output to reproduce it.

Still worth filing upstream, with the `WAYLAND_DEBUG` trace and the observation
that the output lands disabled.

## Risks

- `zkde_screencast_unstable_v1` is explicitly unstable; KDE may break it.
- It buys nothing on GNOME. This is a KDE-shaped solution, which is precisely
  why KRDP carries a portal session alongside it.
- The PipeWire hop adds latency the `ext-` path does not pay. Keep `ext-` as
  the Hyprland default and measure before assuming parity with the verified
  33–48 ms.

## Verification

```bash
scripts/moreland-doctor.sh          # reports the grant under "KDE backend prerequisites"
kbuildsycoca6 --noincremental       # after any change to the desktop entry
wayland-info | grep zkde_screencast # empty for an undeclared client; that is the gate working
```

To confirm the grant applies to a specific binary, copy `wayland-info` to that
path, point a desktop entry's absolute `Exec` at it, rebuild the cache, and run
it. Remember to remove the entry and rebuild again afterwards, or the stale
cache will keep reporting a grant that no longer exists.

## Result: the full pipeline

```
KWin  --zkde_screencast-->  virtual output + PipeWire node
      --PwCapture-------->  CPU-memory BGRx frames (crates/capture/src/plasma/stream.rs)
      --MemoryEncoder---->  nvh264enc | x264enc          (crates/encoder/src/memory.rs)
      --Sender----------->  ADB -> tablet                 (unchanged)
```

Three decisions worth knowing:

- **Frames are CPU memory, not DMA-BUFs.** That skips modifier negotiation with
  KWin and is what NVENC needs anyway when KWin renders on a different GPU than
  the encoder. On a single-GPU AMD or Intel machine a DMA-BUF path into VA-API
  would remove a copy; it is not written yet.
- **The encoder is chosen at runtime.** `nvh264enc` if installed, else
  `x264enc`. Override with `MORELAND_ENCODER=nvenc|x264`.
- **The Hyprland and labwc path is untouched.** Capture and encode sit behind a
  `Pipeline` trait in `crates/daemon/src/session.rs`; the KDE one is selected
  only when the output reports a PipeWire node.

Measured here: `PwCapture` delivered up to 100 fps while a window was being
dragged and ~0 when the monitor was idle (KWin sends a frame only when the
output changes). `MemoryEncoder::push_frame` cost about 1 ms per 1920x1200
frame. NVENC kept up with the capture rate: 1141 frames in, 1139 packets out
over 15 s.

## Building on Fedora

- `sudo dnf install plasma-wayland-protocols pipewire-devel gstreamer1-devel
  gstreamer1-plugins-base-devel gstreamer1-plugins-bad-free-devel`
- The PipeWire bindings use `bindgen`, which on Fedora may not find the
  compiler's own headers (`'stdbool.h' file not found`). Point it at GCC's:
  `export BINDGEN_EXTRA_CLANG_ARGS="-I$(gcc -print-file-name=include)"`.
- Stock Fedora Mesa has no H.264 VA-API encode. That does not matter here if
  you have `nvh264enc` (NVIDIA) or `x264enc` (`gstreamer1-plugins-ugly`).
  `mesa-va-drivers-freeworld` from RPM Fusion would add it, but it tracks
  Mesa's version and can lag behind Fedora's updates.
- The Android app needs JDK 17-21 for Gradle 8.11; Fedora 44 ships only 25.
  Android Studio's bundled `jbr` works: `JAVA_HOME=~/android-studio/jbr
  ANDROID_HOME=~/Android/Sdk ./gradlew assembleDebug`.

## Known limits on KDE

- The tablet's refresh rate follows what KWin produces, not `--fps`.
- Idle desktops send almost no frames, so the tablet shows the last one.
- Latency is in the same range as the Hyprland path but has a longer tail
  (p95 43 ms, max 93 ms on the run above).
- The output is positioned with `kscreen-doctor output.Virtual-<name>.position`;
  if that fails the output still streams, at KWin's default position.

## Touch and pen

The tablet's touchscreen and S Pen are forwarded back to the host, so the
second monitor can be drawn on, not just looked at. KDE only for now.

```
Tab S9 FE+ MotionEvent --(protocol v2, device->host)--> daemon
   finger -> uinput multitouch screen   "Moreland Touchscreen"
   S Pen  -> uinput screen tablet       "Moreland Pen"
   both pinned to the virtual output over KWin's D-Bus API (outputName)
```

- **Wire format.** Device-to-host traffic is now typed messages (ack, pointer),
  so protocol version 2. A pointer carries position as a fraction of the
  surface, pressure, tilt, buttons and a "last in event" flag so a multitouch
  frame is applied atomically. The host-side parser tolerates split and batched
  reads and is tested one byte at a time.
- **Pen.** Proximity (`BTN_TOOL_PEN`/`BTN_TOOL_RUBBER`), pressure, hover, and the
  barrel buttons (`BTN_STYLUS`/`BTN_STYLUS2`) reach applications as a real
  tablet tool. Tilt is carried but a Tab S9 FE+ S Pen reports none. Krita binds
  the barrel button to panning by default; that is Krita's setting.
- **Palm rejection** is on the host: a finger that lands while the pen is in
  range is ignored, and fingers already down are cancelled when the tip touches.
  `--no-palm-rejection` turns it off. It also blocks pinch gestures while the
  pen hovers, which some people will not want.
- **Needs** `/dev/uinput` to be writable by your user (it is on Fedora with
  the default ACLs). If the devices cannot be created or mapped the session
  logs `touch and pen disabled` and still streams the picture.

## Latency notes

Two things mattered more than any encoder setting:

- Encoded packets used to be sent only between capture-loop iterations, and on
  a quiet desktop the loop waits for the next frame, so a finished packet could
  sit for up to a frame interval. The encoder thread now sends directly.
- On the tablet, Android batches touch input to the display refresh unless the
  view asks for unbuffered dispatch, and the app set only Qualcomm's decoder
  low-latency key. Exynos has its own (`vendor.rtc-ext-dec-low-latency.enable`).

Tuning knobs that remain: `MORELAND_NVENC_PRESET=p1..p7` (default `p5`),
`--bitrate`, `--fps` (default 60 on KDE; 90 micro-stuttered through the CPU
copy), and resolution.
