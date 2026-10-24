//! Kernel firmware loader: `request_firmware(path)` reads binary microcode
//! from `/lib/firmware/<path>` in the mounted root filesystem.
//! Required by modern Intel (i915/xe) and AMD (amdgpu) GPU drivers.

#![allow(unused)]

use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::sync::spinlock::SpinLock;
use crate::fs::vfs::VNode;

#[derive(Debug)]
pub enum FirmwareError {
    NotFound,
    IoError,
    TooLarge,
}

const MAX_FIRMWARE_SIZE: usize = 64 * 1024 * 1024; // 64 MiB

/// In-memory cache of loaded firmware, keyed by `/lib/firmware/<path>`.
/// Prevents repeated disk I/O for frequently requested firmware blobs.
static FIRMWARE_CACHE: SpinLock<BTreeMap<alloc::string::String, Arc<Vec<u8>>>> =
    SpinLock::new(BTreeMap::new());

/// Load a firmware binary from `/lib/firmware/<path>` in the root filesystem.
/// The returned Vec owns the firmware bytes in kernel heap memory.
///
/// # Errors
/// Returns `FirmwareError::NotFound` if the file does not exist.
/// Returns `FirmwareError::TooLarge` if firmware exceeds 64 MiB.
/// Returns `FirmwareError::IoError` if the file cannot be read.
pub fn request_firmware(path: &str) -> Result<Vec<u8>, FirmwareError> {
    let cache_key = alloc::format!("/lib/firmware/{}", path);
    {
        let cache = FIRMWARE_CACHE.lock();
        if let Some(blob) = cache.get(&cache_key) {
            return Ok(blob.as_ref().clone());
        }
    }
    let buf = load_from_disk(&cache_key)?;
    let blob = Arc::new(buf);
    FIRMWARE_CACHE.lock().insert(cache_key, Arc::clone(&blob));
    Ok(blob.as_ref().clone())
}

fn load_from_disk(full_path: &str) -> Result<Vec<u8>, FirmwareError> {
    let vnode = crate::fs::lookup(full_path).ok_or(FirmwareError::NotFound)?;
    let size = match &vnode {
        crate::fs::vfs::VNode::FsNode { node, .. } => node.size(),
        crate::fs::vfs::VNode::InitrdFile { data, .. } => data.len(),
        _ => return Err(FirmwareError::NotFound),
    };
    if size > MAX_FIRMWARE_SIZE {
        return Err(FirmwareError::TooLarge);
    }
    let mut buf = Vec::with_capacity(size);
    buf.resize(size, 0u8);
    match &vnode {
        crate::fs::vfs::VNode::FsNode { node, .. } => {
            node.read_at(0, &mut buf).map_err(|_| FirmwareError::IoError)?;
        }
        crate::fs::vfs::VNode::InitrdFile { data, .. } => {
            buf.copy_from_slice(data);
        }
        _ => return Err(FirmwareError::NotFound),
    }
    Ok(buf)
}