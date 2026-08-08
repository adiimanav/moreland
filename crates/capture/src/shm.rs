//! Anonymous shared-memory buffers for the shm capture path.
//!
//! This is the Stage 1b bring-up path only. It costs a full CPU-side copy of
//! every frame, which is exactly what the DMA-BUF path in Stage 1c exists to
//! avoid — but it is the shortest route to proving the protocol flow works and
//! that the pixels we get back are the ones we expect.

use anyhow::{Context, Result};
use memmap2::MmapMut;
use rustix::fs::{ftruncate, memfd_create, MemfdFlags};
use std::os::fd::OwnedFd;

pub struct ShmBuffer {
    pub fd: OwnedFd,
    pub map: MmapMut,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl ShmBuffer {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let stride = width
            .checked_mul(4)
            .context("frame stride overflowed")?;
        let size = stride
            .checked_mul(height)
            .context("frame size overflowed")? as u64;

        let fd = memfd_create("moreland-capture", MemfdFlags::CLOEXEC)
            .context("creating memfd for capture buffer")?;
        ftruncate(&fd, size).context("sizing capture buffer")?;

        // SAFETY: the memfd was just created and sized above, and this is the
        // only mapping of it.
        let map = unsafe { MmapMut::map_mut(&fd) }.context("mapping capture buffer")?;

        Ok(Self {
            fd,
            map,
            width,
            height,
            stride,
        })
    }

    pub fn size(&self) -> usize {
        (self.stride * self.height) as usize
    }
}
