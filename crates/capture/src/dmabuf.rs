// SPDX-License-Identifier: Apache-2.0

//! GBM-backed DMA-BUF allocation for zero-copy capture.
//!
//! The compositor tells us which DRM device it wants buffers allocated on
//! (`dmabuf_device`) and which format/modifier pairs it can write. We allocate
//! matching buffers through GBM and hand them back as `wl_buffer`s. The same
//! underlying DMA-BUF then feeds VAAPI in Stage 2 without ever touching the CPU.

use anyhow::{bail, Context, Result};
use drm_fourcc::DrmFourcc;
use gbm::{BufferObject, BufferObjectFlags, Device as GbmDevice};
use std::fs::{File, OpenOptions};
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// One plane of an exported DMA-BUF.
pub struct DmabufPlane {
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// A GBM buffer object exported as DMA-BUF planes.
pub struct Dmabuf {
    /// Kept alive because the exported fds reference its storage.
    _bo: BufferObject<()>,
    pub planes: Vec<DmabufPlane>,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub modifier: u64,
}

pub struct DmabufAllocator {
    device: GbmDevice<File>,
    pub path: PathBuf,
    /// The `dev_t` this was opened for, so a re-negotiation can tell whether
    /// the compositor switched devices or merely changed formats.
    pub dev_t: Option<u64>,
}

impl DmabufAllocator {
    /// Open the DRM node the compositor advertised via `dmabuf_device`.
    pub fn from_dev_t(dev: u64) -> Result<Self> {
        let path = resolve_render_node(dev)?;
        let mut allocator = Self::open(&path)?;
        allocator.dev_t = Some(dev);
        Ok(allocator)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening DRM node {}", path.display()))?;
        let device = GbmDevice::new(file)
            .with_context(|| format!("creating GBM device on {}", path.display()))?;
        Ok(Self {
            device,
            path: path.to_path_buf(),
            dev_t: None,
        })
    }

    /// Allocate a scanout-capable buffer, letting GBM pick the best modifier
    /// from the set the compositor offered.
    pub fn allocate(
        &self,
        width: u32,
        height: u32,
        format: u32,
        modifiers: &[u64],
    ) -> Result<Dmabuf> {
        let fourcc = DrmFourcc::try_from(format)
            .map_err(|_| anyhow::anyhow!("unknown DRM fourcc 0x{format:08x}"))?;

        let bo = if modifiers.is_empty() {
            self.device
                .create_buffer_object::<()>(
                    width,
                    height,
                    fourcc,
                    BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR,
                )
                .context("allocating GBM buffer object")?
        } else {
            let mods: Vec<drm_fourcc::DrmModifier> = modifiers
                .iter()
                .map(|m| drm_fourcc::DrmModifier::from(*m))
                .collect();
            self.device
                .create_buffer_object_with_modifiers2::<()>(
                    width,
                    height,
                    fourcc,
                    mods.into_iter(),
                    BufferObjectFlags::RENDERING,
                )
                .context("allocating GBM buffer object with modifiers")?
        };

        let modifier: u64 = bo.modifier().into();
        let plane_count = bo.plane_count();

        let mut planes = Vec::with_capacity(plane_count as usize);
        for plane in 0..plane_count as i32 {
            planes.push(DmabufPlane {
                fd: bo
                    .fd_for_plane(plane)
                    .with_context(|| format!("exporting DMA-BUF fd for plane {plane}"))?,
                offset: bo.offset(plane),
                stride: bo.stride_for_plane(plane),
            });
        }

        Ok(Dmabuf {
            _bo: bo,
            planes,
            width,
            height,
            format,
            modifier,
        })
    }
}

/// Map a `dev_t` to a usable DRM node path.
///
/// The compositor may name either the primary (`cardN`) or render (`renderD*`)
/// node. We always want the render node: it needs no DRM master and is the one
/// VAAPI will later open.
fn resolve_render_node(dev: u64) -> Result<PathBuf> {
    let mut matched: Option<PathBuf> = None;
    for entry in std::fs::read_dir("/dev/dri").context("listing /dev/dri")? {
        let entry = entry?;
        let meta = match std::fs::metadata(entry.path()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.rdev() == dev {
            matched = Some(entry.path());
            break;
        }
    }

    let matched = matched
        .with_context(|| format!("no /dev/dri node matches dev_t {dev} advertised by compositor"))?;

    let name = matched
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if name.starts_with("renderD") {
        return Ok(matched);
    }

    // Primary node: hop through sysfs to its sibling render node.
    let sysfs = PathBuf::from("/sys/class/drm").join(name).join("device/drm");
    if let Ok(entries) = std::fs::read_dir(&sysfs) {
        for entry in entries.flatten() {
            let sibling = entry.file_name();
            let sibling = sibling.to_string_lossy();
            if sibling.starts_with("renderD") {
                return Ok(PathBuf::from("/dev/dri").join(sibling.as_ref()));
            }
        }
    }
    bail!("could not find a render node for {}", matched.display())
}
