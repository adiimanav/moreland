# Stage 3 — Wire Protocol and USB Transport

**Status: host side complete and verified. One-way latency deferred to Stage 4.**

## Wire format

One stream header at connect, then a header + Annex-B payload per access unit.

```text
StreamHeader (16 bytes, once)
  0..4   magic  "MRLD"
  4..6   version          u16
  6..8   width            u16
  8..10  height           u16
 10..12  framerate        u16
    12   codec            u8    0 = H.264, 1 = H.265
 13..16  reserved

FrameHeader (16 bytes, repeated)
  0..4   payload length   u32
  4..12  pts, nanoseconds u64
    12   flags            u8    bit 0 = keyframe
 13..16  reserved
```

**All multi-byte fields are big-endian.** That is a deliberate concession to the
device side: Kotlin's `DataInputStream` reads big-endian natively, so the
Android app needs no byte-swapping code at all.

Round-trip and bad-magic rejection are unit-tested in `crates/protocol`.

## Transport

`adb forward tcp:27183 localabstract:moreland` in production — the device app
listens on an abstract Unix socket, so traffic never touches Android's TCP stack
or `netd`. The host connects to the forwarded local port.

Two details that matter for latency:

- **`TCP_NODELAY` is set.** Nagle's algorithm batches small writes waiting for
  an ACK; on a per-frame video stream that is pure added latency.
- **Header and payload go out in a single vectored write**, so they cannot be
  split into separate segments. `Write::write_all_vectored` is still unstable,
  so `IoSlice::advance_slices` handles partial writes by hand.

`adb::Forward` is an RAII guard: it clears any stale rule first, installs its
own, and removes it on drop.

## Measurements

Synthetic frames shaped like real Stage 2 output (median 32 KB, periodic 108 KB
keyframe), through `adb forward` to a `nc` sink on the tablet.

### Paced at 60 fps — the production case

```
rate        60.00 fps,  16.5 Mbps
write()     min 0.018 / median 0.064 / p95 0.116 / max 0.250 ms
sent        20,575,057 bytes
received    20,575,057 bytes on device
status      byte-exact
```

### Flood — maximum sustainable throughput

```
written     51.5 MB in 1.27 s   (40.4 MB/s apparent — this is buffering)
effective   27.86 MB/s (223 Mbps) including drain
received    51,494,876 bytes    byte-exact
```

Sustained transport capacity is **27.9 MB/s**, somewhat below the 34.4 MB/s that
`adb push` achieved in Stage 0 — the forwarded-socket path has more hops than a
bulk file push. Our 16.5 Mbps stream uses **7.4%** of it.

## What these numbers do *not* say

`write()` latency of 0.064 ms is **not** transport latency. It measures only how
long the kernel took to accept bytes into a socket buffer. Nothing about when
they reach the tablet.

True one-way latency requires the device to timestamp arrival, which requires
the app. **The 3–8 ms transport estimate in the budget stands unmeasured until
Stage 4.** What Stage 3 does establish is that the link has ample headroom and
delivers byte-exact at production rates.

## The bug that faked success

The first working-looking run reported **2387 MB/s** — seventy times the USB 2.0
ceiling. Physically impossible, and the only reason it was caught.

toybox `nc` forwards stdin to the socket as well as socket to stdout, and quits
on stdin EOF. A backgrounded `adb shell` has no tty, so nc saw EOF immediately,
accepted the connection, and closed it before reading a byte. Meanwhile `adb
forward` cheerfully accepted everything the host wrote and drained it into a
dead channel — **no error, no broken pipe, no warning.** Every host-side metric
looked excellent while zero bytes reached the tablet.

Two fixes, both now permanent:

1. `sleep 3600 | nc -l -p PORT > sink` holds stdin open.
2. The probe **verifies against the device**, comparing `stat -c %s` of the sink
   file to bytes sent. A fast `write()` proves nothing; only the tablet can
   confirm arrival.

The generalisation: when a benchmark reports a number that violates a known
physical limit, the benchmark is wrong. The 275 Mbps USB ceiling measured in
Stage 0 is what made this detectable at a glance.

A smaller instance of the same lesson followed immediately: the first verified
run reported a byte "MISMATCH" of exactly **+16 bytes received**. That was not
loss — it was `bytes_sent` failing to count the 16-byte stream header. Fixed in
`Sender::connect`.

## Code

```
crates/protocol/
  src/lib.rs                    wire types, encode/decode, unit tests
crates/transport/
  src/adb.rs                    adb CLI wrapper, RAII forward guard
  src/lib.rs                    framed Sender, TCP_NODELAY, vectored writes
  src/bin/transport_probe.rs    benchmark with device-side verification
```

## Verifying

```bash
cargo test -p protocol

# production-rate pacing, verified byte-exact against the device
./target/release/transport-probe --frames 600

# maximum sustainable throughput
./target/release/transport-probe --frames 1500 --flood
```

The probe spawns and cleans up its own device-side sink. If a run is
interrupted, clear leftovers manually:

```bash
adb shell "pkill -f 'nc -l'; rm -f /data/local/tmp/moreland-sink.bin"
adb forward --remove-all
```

## Latency budget after this stage

| Stage | Budget |
|---|---|
| Capture → DMA-BUF | **2.1 ms** (measured) |
| VAAPI encode | **6.3 ms** (measured) |
| USB transport | 3–8 ms (estimated — still) |
| MediaCodec decode | 5–12 ms (estimated) |
| SurfaceFlinger + panel | 8–25 ms (estimated) |
| **Total** | **~24–53 ms** |

Unchanged, deliberately. Stage 3 proved capacity and correctness, not latency.
