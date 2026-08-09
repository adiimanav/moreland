# Compatibility

**Verified working on exactly one setup.** Everything else below is an
assessment of what would be required, not a claim that it works. KDE Plasma has
since been tested and is **verified blocked** — the reasons are recorded below.
GNOME and Sway remain untested.

Run [`scripts/moreland-doctor.sh`](../scripts/moreland-doctor.sh) to get this
answer for your own machine: it checks the compositor, the capture protocol,
the VA-API encoder and the ADB link, and names what blocks you.

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

### KDE Plasma / KWin — tested, blocked

Tested on **KWin 6.7.4**, Plasma 6.7.4, Wayland session, EndeavourOS. Both
questions this section used to pose are now answered.

**1. Does KWin implement `ext-image-copy-capture-v1`? No.**

```console
$ wayland-info | grep -E 'image_copy_capture|image_capture_source'
$ grep -rl ext_image_copy_capture_manager_v1 /usr/lib/ /usr/bin/
$
```

Neither global is advertised, and the interface name does not appear in any
installed binary — so this is not a privileged-client restriction that a
different client could work around. The protocol is simply not implemented.
`zwlr_screencopy_manager_v1` is absent too, so there is no legacy fallback.
**`crates/capture` has nothing to bind to on KDE**, and no amount of work in
`output.rs` changes that.

`zwp_linux_dmabuf_v1` v5 *is* present, so the zero-copy import path itself is
fine. Capture is the only missing piece.

**2. How does KWin create a virtual output?** Through the same protocol that
solves the first problem — which is the useful finding here:

```console
$ strings -a /usr/lib/qt6/plugins/kwin/plugins/screencast.so | grep -i virtualoutput
_ZN4KWin21ScreencastV1Interface32virtualOutputScreencastRequestedEPNS_...
```

`zkde_screencast_unstable_v1` has a `stream_virtual_output` request that
**creates a virtual output and returns a PipeWire stream of it in one call** —
the mechanism behind KDE's own "virtual monitor" feature. So KDE does not need
the two separate backends this document originally assumed. It needs one
PipeWire capture path, and virtual-output creation comes free with it.

`kpipewire` is already installed on a normal Plasma system and exposes exactly
the right surface (`pipewiresourcestream.h`, `dmabufhandler.h`) for reference.

The protocol is privileged and absent from a plain registry listing, but **not
portal-only** — a client binds it directly after declaring it in a desktop
entry:

```ini
X-KDE-Wayland-Interfaces=zkde_screencast_unstable_v1
```

Verified by A/B/A test on KWin 6.7.4: a user entry under
`~/.local/share/applications` is enough, so this needs **no root**, and KWin
matches the client's executable path rather than how it was launched, so a
systemd-launched daemon qualifies. `Exec` must be an absolute path — the bare
form is silently denied. `install.sh` installs this entry; the mechanics and
the traps are in [06-plasma-backend.md](06-plasma-backend.md).

Note that the portal is *not* the answer here: it appears unable to create
virtual monitors, which is why KRDP ships a Plasma-specific session alongside
its portal one. What remains unimplemented is the PipeWire capture backend
behind the grant.

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

## Distributions

**The distribution is very nearly irrelevant.** It is a tempting axis because
it is the one users know they have, but nothing in the pipeline asks what
distro it is on. What actually decides the answer is three things, and every
mainstream distro can supply all three:

| Requirement | Minimum | Why |
|---|---|---|
| A compositor implementing `ext-image-copy-capture-v1` | Hyprland, or wlroots 0.18+ | Capture has nothing to bind to otherwise |
| GStreamer with the `va` plugin | 1.22+ (developed against 1.28) | `vapostproc` and `vah264enc` |
| A VA-API driver | Mesa `radeonsi`/`iHD`, or `nvidia-vaapi-driver` | Hardware H.264 encode |

A distro influences the outcome only indirectly, in two ways: **which desktop
it installs by default**, and **how old its GStreamer is**. So "does Fedora
work?" is not really a question about Fedora — Fedora Workstation ships GNOME
and is blocked for the same reason Plasma is, while Fedora with Hyprland
installed should work. The same holds for Ubuntu, Debian and openSUSE.

The one genuine distro-level trap is GStreamer age: the `va` plugin element set
only became usable around 1.22, so a conservative stable release (Debian
oldstable, older Ubuntu LTS, RHEL) can fail on that alone even under Hyprland.

Package names, since those *are* distro-specific — only the Arch line is
verified, the rest are best-effort and untested:

```bash
# Arch / EndeavourOS / Manjaro — verified
sudo pacman -S --needed rust gstreamer gst-plugins-base gst-plugins-good \
                        gst-plugin-va libva libva-utils android-tools \
                        android-udev wayland-utils

# Fedora — UNVERIFIED
sudo dnf install rust cargo gstreamer1-plugins-base gstreamer1-plugins-good \
                 gstreamer1-plugins-bad-free libva libva-utils android-tools \
                 wayland-utils

# Debian / Ubuntu — UNVERIFIED, check GStreamer is 1.22+
sudo apt install rustc cargo gstreamer1.0-plugins-base \
                 gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
                 libva2 vainfo adb wayland-utils
```

`scripts/moreland-doctor.sh` detects the distro and prints the matching line
for whatever is missing.

## Contributing a compositor backend

1. Confirm the capture protocols exist: `scripts/moreland-doctor.sh`, or by
   hand with `wayland-info | grep -E 'image_copy_capture|image_capture_source'`.
   If they are absent, stop — the work is a PipeWire capture backend, not a
   compositor backend, and `output.rs` is not where it goes.
2. Add a variant to `Compositor` in `crates/daemon/src/output.rs` and detect it
   from the environment
3. Implement create/remove for it
4. Verify capture alone first — `capture-probe <output> --dmabuf --frames 120`
   isolates the compositor from the rest of the pipeline

Please report what you find even if it does not work; a documented failure is
more useful than silence.
