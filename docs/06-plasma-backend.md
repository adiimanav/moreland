# Stage 6 — KDE Plasma backend

**Status: the Wayland half works. A real second monitor can be created on KDE
Plasma, with a PipeWire node for its contents.** The PipeWire consumer that
turns that node into frames is not written yet, so nothing reaches the tablet
and `moreland-doctor.sh` still reports `BLOCKED`.

Verified on KWin 6.7.4 — a genuine output at `1920,0 1920x1200`, enabled, with
PipeWire node 85, torn down cleanly when the client exits:

```console
$ plasma-probe
  name           moreland
  pipewire node  85
  object serial  1586

$ kscreen-doctor -o
Output: 1 Virtual-moreland   enabled   Geometry: 1920,0 1920x1200
Output: 2 eDP-1              enabled   Geometry: 0,0 1746x982
```

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
