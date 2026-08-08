# Stage 0 — System Survey and Architecture

Everything here was probed on the target hardware. Where a claim is an estimate
rather than a measurement, it says so.

## Host

ASUS ROG Zephyrus G14, EndeavourOS, kernel 7.1.4.

| | |
|---|---|
| CPU | AMD Ryzen 7 5800HS (8C/16T) |
| RAM | 15 GiB |
| Compositor | Hyprland 0.56.2, Aquamarine 0.14.0 |
| Session | Wayland, `XDG_SESSION_TYPE=wayland` |

### Hybrid graphics — which GPU matters

The machine has two GPUs. This was **not** in the original project brief and it
changes the design, so it was verified rather than assumed:

| | Device | Nodes | Role |
|---|---|---|---|
| dGPU | NVIDIA GTX 1650 Mobile (TU117M) | `card1` / `renderD128` | idle |
| iGPU | AMD Cezanne Vega (Renoir) | `card2` / `renderD129` | **drives eDP-1, renders Hyprland** |

Verified via `/proc/$(pidof Hyprland)/fdinfo/*`, which reports exactly one
render driver: `amdgpu`. Hyprland holds file descriptors on both cards for
multi-GPU enumeration but renders only on AMD.

**Consequence: encode on AMD, ignore the NVIDIA GPU entirely.** Capture buffers
land on `renderD129`, which is where VCN — the video encode block — lives, so
capture→encode is zero-copy. Routing through NVENC instead would force a
cross-PCIe copy *and* spin up a 10–15 W discrete GPU on a laptop. VCN is a
dedicated block separate from the shader cores, so encoding does not steal
throughput from compositing.

### VAAPI encoder capabilities (`renderD129`)

```
H.264   Constrained Baseline / Main / High   encode + decode
HEVC    Main / Main10                        encode + decode
Max picture size                             4096x4096
Rate control                                 CBR / VBR / CQP
EncMaxRefFrames                              l0=1, l1=0
EncQualityRange                              32 levels
EncIntraRefresh                              rolling column / rolling row / P-frame
```

Two of these are load-bearing:

**`l0=1, l1=0` means the hardware cannot emit B-frames.** Normally a
limitation. Here it is exactly what we want — B-frames require the decoder to
buffer and reorder, which is a direct latency cost. The hardware makes that
mistake impossible. It also compensates for a gap on the tablet side (below).

**Rolling intra-refresh** would replace periodic IDR keyframes, spreading the
same work evenly across frames instead of spiking.

> **Correction (Stage 2).** The *hardware* supports this, but `gst-plugin-va`
> **does not expose an intra-refresh property** on `vah264enc`. Verified against
> the element's full property list. Reaching it would mean dropping GStreamer
> for raw libva.
>
> This matters much less than it would on a constrained link. Intra-refresh
> exists to avoid bitrate spikes when bandwidth is tight; we measured **13×
> headroom**. A 1920×1200 IDR frame is roughly 200–400 KB against ~40 KB for a
> P-frame, so at 34.4 MB/s a keyframe costs about 11 ms to transfer versus 1 ms
> — a one-off hitch, not sustained stutter.
>
> Mitigation without intra-refresh: USB is lossless, so frequent keyframes buy
> nothing. Set `key-int-max` very large (one keyframe at stream start), and use
> CBR with a tight `cpb-size` to cap the spike. Revisit with libva only if
> Stage 6 measurements show it actually hurts.

No AV1 (VCN 2.x predates it). Irrelevant — AV1 encode would be far too slow.

GStreamer exposes `vah264enc`, `vah265enc`, and `vapostproc` bound to
"AMD Radeon Graphics" via `gst-plugin-va`.

### Wayland protocols

All present, verified with `wayland-info`:

```
ext_output_image_capture_source_manager_v1   v1   wl_output -> capture source
ext_image_copy_capture_manager_v1            v1   frame copies, damage-aware
zwp_linux_dmabuf_v1                          v5   zero-copy buffers
zwlr_virtual_pointer_manager_v1              v2   touch return path (no root)
zwp_virtual_keyboard_manager_v1              v1
zwlr_screencopy_manager_v1                   v3   legacy fallback, unused
```

## Tablet

**Xiaomi Pad 6** (`pipa_global`, model 23043RP34G, serial redacted).

| | |
|---|---|
| SoC | Snapdragon 870 (SM8250 "kona"), Adreno 650 |
| OS | Android 14, API 34 |
| Panel | 1800×2880 native portrait → 2880×1800 landscape, 144 Hz |
| Modes | 30 / 48 / 50 / 60 / 90 / 120 / 144, all at full resolution |
| Hardware decoders | `OMX.qcom.video.decoder.avc`, `.hevc` |
| Max virtual display | 4096 |

### USB is 2.0, permanently

The device descriptor reports `bcdUSB 2.00`. The laptop has two idle USB 3.1
controllers with 10 Gbps root hubs, but the tablet's own USB controller is USB
2.0 only — **no cable or port change alters this.**

Measured with 200 MB of incompressible random data, ADB compression disabled
(`adb push -Z`):

```
HOST → DEVICE   34.4 MB/s   (275 Mbps)    ← our direction
DEVICE → HOST   39.3 MB/s   (314 Mbps)
```

That is near the practical ceiling for USB 2.0 bulk, and it includes flash-write
overhead on the device side. Against the ~20 Mbps a 1920×1200@60 H.264 stream
needs, that is **13× headroom**.

**Bandwidth is not the constraint. Latency is.** This conclusion drives the
transport choice below.

### Codec caveat, already mitigated

No `low-latency` feature flag is declared in any vendor codec XML, and this
device uses the **legacy OMX path** for hardware video (the `c2.*` entries are
Google *software* codecs). So `MediaFormat.KEY_LOW_LATENCY` may be ignored.

This turns out not to matter. The main thing that key buys is suppressing
decoder reorder buffering — and our encoder *cannot produce B-frames*, so there
is nothing to reorder and the decoder can output every frame immediately. We
set the key anyway (harmless if ignored) and use async MediaCodec callbacks.

### Enabling ADB

The tablet initially enumerated as `2717:ff40` with a single Imaging (MTP)
interface and `adb devices` was empty. After enabling USB debugging it became
`2717:ff48` with a second interface, class 255 / subclass 66 / protocol 1 — the
standard ADB signature.

Steps on HyperOS/MIUI: Settings → About tablet → tap **OS version** 7× →
Settings → Additional settings → Developer options → enable **USB debugging** →
replug → accept the RSA fingerprint prompt.

Known trap: `adb install` requires **"Install via USB"**, which MIUI gates
behind a Mi account login. If that blocks APK installation, sideload from the
tablet's own file manager instead.

## Transport evaluation

| Transport | Latency | Auto-detect | Complexity | Verdict |
|---|---|---|---|---|
| **`adb forward` → `localabstract`** | +3–6 ms | free | low | **chosen** |
| `adb reverse` TCP | +3–7 ms | free | low | same path, but through Android's TCP stack |
| USB Ethernet (NCM/RNDIS) | +2–5 ms | manual tethering toggle each plug-in | medium | breaks plug-and-play |
| USB accessory (AOA) | +1–3 ms | excellent (auto-launches app) | high | v2 candidate |
| Raw bulk endpoints | lowest | — | needs root gadget driver | not viable unrooted |
| PipeWire over TCP | high | — | — | audio transport, no Android client |
| scrcpy-style socket | — | — | — | *is* `adb forward`; pattern adopted |

**Chosen: `adb forward` to a `localabstract:` Unix socket.** The device-side app
listens on an abstract Unix socket and the host connects. This bypasses
Android's TCP/IP stack and `netd` entirely — slightly lower latency than
`adb reverse` and immune to firewall or VPN interference.

**Why not AOA, despite being fastest:** it saves ~2–4 ms out of a ~40 ms budget,
under 10% of the total, while optimising the *smallest* term. It costs ADB
entirely while active (accessory mode replaces the ADB interface), making
development and debugging painful, and Xiaomi's AOA implementation is
inconsistent. Revisit only if measurement shows the adb-server hop is material.

This contradicts the brief's stated priority ordering (latency above all).
The ordering is wrong: transport is not where the latency lives.

## Codec and resolution

**H.264, not HEVC.** HEVC would halve the bitrate, but with 13× bandwidth
headroom that buys nothing, and HEVC decode adds latency.

**1920×1200@60.** Exactly 16:10, so a clean 1.5× upscale to the panel's
2880×1800, sharp enough on an 11" display, and ~138 Mpixel/s — comfortable for
VCN 2.x. Bandwidth would permit 2880×1800@60 (311 Mpixel/s); whether the
*encoder* sustains it is an open question for Stage 6. Config value either way.

**120 fps is out of reach.** The panel runs at 144 Hz, but VCN 2.x realistically
sustains ~1080p120 or ~4K30 for H.264. 2880×1800 is 2.5× the pixels of 1080p.

## Why sub-20 ms is not achievable

The brief targets <20 ms end to end. Honest budget:

| Stage | Cost | Controllable? |
|---|---|---|
| Capture → DMA-BUF ready | 2.1 ms (measured) | yes |
| VAAPI encode | 4–8 ms | yes |
| Host → USB → adbd → app | 3–8 ms | yes |
| MediaCodec decode | 5–12 ms | partly |
| **SurfaceFlinger + panel scanout** | **8–25 ms** | **no** |
| **Total** | **~22–56 ms** | |

The dominant term is Android's display pipeline, which we do not control. For
calibration: **scrcpy** — the most heavily optimised project in this exact
problem space — measures 25–45 ms running the same pipeline in the opposite
direction. There is no mechanism by which we substantially beat it.

**Realistic target: 35–45 ms typical, ~30 ms best case.** Good for documents,
video, browsers, terminals, dashboards. Not good enough for gaming or precision
stylus work, and no compressed-video-over-USB architecture will be.

Agreed with the project owner as acceptable: the intended use is YouTube and
documents on an extended display.

## Detection strategy

**`adb track-devices`, not udev.** The ADB server already streams device
connect/disconnect events, and this is immune to USB product IDs shifting — the
tablet moved `ff40` → `ff48` simply from enabling debugging, and changing USB
mode shifts it again. A daemon idling on that socket costs no measurable CPU.
udev is therefore optional rather than load-bearing.

## Prerequisites installed

```bash
sudo pacman -S --needed vulkan-radeon android-udev wayland-utils
```

- `vulkan-radeon` — was missing; `vulkaninfo` previously reported *only* the
  NVIDIA GPU. Not required for VAAPI, but a real gap worth closing. Now reports
  `AMD Radeon Graphics (RADV RENOIR)`.
- `android-udev` — stable non-root ADB access and clean hotplug events.
- `wayland-utils` — `wayland-info`, for protocol introspection.
