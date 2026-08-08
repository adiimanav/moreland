# Stage 2 — Hardware H.264 Encoding

**Status: complete and measured.**

Take the DMA-BUFs produced by Stage 1 and turn them into an H.264 Annex-B
bitstream on the GPU, without the pixels ever reaching the CPU.

## Pipeline

```
appsrc      video/x-raw(memory:DMABuf), format=DMA_DRM,
            drm-format=XR24:0x0200000000000901
  -> vapostproc   XR24 tiled -> NV12, in-GPU
  -> capsfilter   video/x-raw(memory:VAMemory), NV12
  -> vah264enc    VBR, no B-frames, target-usage 3
  -> h264parse    config-interval=-1
  -> capsfilter   byte-stream / au
  -> appsink
```

`vah264enc` reports `device-path = /dev/dri/renderD129` — the same GPU the
compositor allocates capture buffers on, so the whole chain stays on one device.

> Running `gst-launch` prints `pci id for fd N: 10de:1f9d, driver (null)` lines.
> That is `10de` — NVIDIA. It is libva probe noise from `LIBVA_DRIVER_NAME=radeonsi`
> failing against the NVIDIA node during enumeration, **not** a misbinding.
> Confirmed via the element's `device-path` property.

## Final configuration

| Property | Value | Why |
|---|---|---|
| `rate-control` | `vbr` | Half CBR's bitrate at identical latency |
| `bitrate` | 20000 kbps ceiling | Actual usage settles ~16 Mbps |
| `target-usage` | **3** | See the cliff below |
| `b-frames` | 0 | Hardware cannot do them anyway; nothing to reorder |
| `ref-frames` | 1 | Matches the hardware's `l0=1` limit |
| `key-int-max` | 600 | USB is lossless; keyframes buy nothing |
| `cabac` | true | Better compression, free in hardware |
| `aud` | true | Access-unit delimiters help the decoder find boundaries |
| `cpb-size` | bitrate/2 | Bounds keyframe spikes without intra-refresh |

## Measurements

1920×1200@60, release build, 300-frame runs against continuously-damaging content.

```
rate        59.99 fps
packets     320 (1 keyframe)

encode latency (push -> packet, pipelined)
  min        5.08 ms
  median     6.26 ms
  p95        6.89 ms
  max        8.49 ms

bitstream
  frame      min 14298 B, median 32357 B, max 108943 B
  bitrate    16.22 Mbps   (5.9% of the measured 275 Mbps USB ceiling)
```

Output verified decodable: **H.264 High profile, Level 5.0, 1920×1200,
`has_b_frames=0`**.

## Three findings that cost real time

### 1. `target-usage` latency is not monotonic — and the naming is backwards

The property is documented as speed/quality balance, 1 = quality … 7 = speed.
Measured on VCN 2.x, the nominal *speed* presets are the **slow** ones, and
there is a cliff between 3 and 4:

| target-usage | Median latency | p95 |
|---|---|---|
| 1 | 7.31 ms | 8.19 ms |
| **2** | **6.21 ms** | 6.81 ms |
| **3** | **6.19 ms** | **6.56 ms** |
| 4 | 10.76 ms | 11.17 ms |
| 7 | 10.74 ms | 11.31 ms |

Reproduced across repeated runs. Taking the "obvious" speed preset costs
**4.5 ms** — over 70% more latency than TU=3, and more than the entire capture
stage. TU=2/3 compress slightly worse (median frame 32 KB vs 22 KB at TU=1),
which is irrelevant at 5.9% link utilisation.

CAVLC (`cabac=false`) was also tested: 6.83 ms at TU=1, better than CABAC's
7.29 ms there, but still worse than TU=3 with CABAC. Not worth the compression
loss.

### 2. A serialised probe reports half the real frame rate

The first version of the benchmark captured a frame, pushed it, then blocked
waiting for the packet. That reported **30.00 fps** and **21 ms** encode
latency — both wrong.

Hardware encoders pipeline internally: you submit frame N and receive N-1 or
N-2. Waiting for each frame before capturing the next serialises two stages that
should overlap, and 16.67 ms capture + 21 ms encode ≈ 33 ms ≈ exactly the 30 fps
observed. Running capture and packet-drain concurrently gave 60 fps and 6.3 ms.

The lesson generalises: **never measure a pipelined stage synchronously.**

### 3. GStreamer rewrites PTS, so match packets by order

Output buffers come back with a constant offset of ~3.6 × 10¹⁵ ns (GStreamer's
live-source running-time base) added to the submitted PTS. Keying a
submit-time map on PTS matches *nothing*, and the failure is silent — the
benchmark simply reported no latency data at all.

Matching submissions to packets by **order** is correct here, and is reliable
precisely because the encoder cannot emit B-frames, so nothing is reordered.

## Buffer ownership

`gst_dmabuf_allocator_alloc` takes ownership of the fd via `IntoRawFd` and
closes it when the memory is freed. The capture pool needs to keep its fds, so
`Encoder::push_frame` **dups** the fd before handing it over. Without that,
GStreamer closes the compositor's buffer out from under the capture session.

## Rate control comparison

Measured at target-usage 6 before the TU finding, so read the latencies
relatively rather than absolutely:

| Mode | Median | p95 | Median frame | Bitrate |
|---|---|---|---|---|
| CBR | 10.80 ms | 11.59 ms | 41,673 B | 20.34 Mbps |
| **VBR** | 10.73 ms | **11.15 ms** | 18,709 B | **9.93 Mbps** |
| CQP | 10.95 ms | 12.54 ms | 6,224 B | 3.39 Mbps |

CBR stuffs every frame to hit its target — median frame size was *identical* to
the maximum, a giveaway. VBR gives the same latency for half the bits. CQP is
cheapest but has the worst p95 and lets quality drift with content, which is
not worth it at 5.9% link utilisation.

## Code

```
crates/encoder/
  src/lib.rs                 pipeline construction, push/pull API
  src/bin/encode_probe.rs    concurrent capture->encode benchmark
```

## Verifying

```bash
# benchmark with defaults
./target/release/encode-probe moreland --frames 300

# sweep a parameter
./target/release/encode-probe moreland --rate-control vbr --target-usage 3

# capture a bitstream and check it decodes
./target/release/encode-probe moreland --frames 300 --out /tmp/out.h264
gst-launch-1.0 filesrc location=/tmp/out.h264 ! h264parse ! avdec_h264 ! fakesink
ffprobe -show_entries stream=profile,width,height,has_b_frames /tmp/out.h264
```

The output needs continuous damage on the virtual output, or you are measuring
the idle path (see [01-capture.md](01-capture.md)):

```bash
hyprctl dispatch exec "[workspace 3 silent] kitty sh -c 'while true; do date +%s.%N; done'"
```

## Latency budget after this stage

| Stage | Budget |
|---|---|
| Capture → DMA-BUF | **2.1 ms** (measured) |
| **VAAPI encode** | **6.3 ms** (measured) |
| USB transport | 3–8 ms (estimated) |
| MediaCodec decode | 5–12 ms (estimated) |
| SurfaceFlinger + panel | 8–25 ms (estimated) |
| **Total** | **~24–53 ms** |

8.4 ms of the budget is now measured rather than guessed, and both stages came
in at or under estimate.
