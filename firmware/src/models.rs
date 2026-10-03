//! Face-model partitions: verify before load.
//!
//! The ESP-DL models live in their own flash partitions so they can be updated without
//! reflashing the firmware (see `crates/model-packer`). ESP-DL aborts the chip if a partition
//! cannot be mapped and has no integrity check, so every partition is verified here first:
//! the manifest in its last sector must name the expected model and match the data's SHA-256
//! (`biometric_core::manifest`).

use core::ffi::{c_void, CStr};
use core::ptr::NonNull;
use std::time::Instant;

use anyhow::{bail, ensure, Result};
use biometric_core::manifest::{self, ModelManifest};
use log::info;

use crate::ffi;

/// A flash partition memory-mapped read-only for the lifetime of this value.
struct MappedPartition {
    data: NonNull<u8>,
    len: usize,
    handle: ffi::esp_partition_mmap_handle_t,
}

impl MappedPartition {
    fn map(label: &CStr) -> Result<Self> {
        // Safety: plain lookup; the returned descriptor is static for the program lifetime.
        let partition = unsafe {
            ffi::esp_partition_find_first(
                ffi::esp_partition_type_t_ESP_PARTITION_TYPE_DATA,
                ffi::esp_partition_subtype_t_ESP_PARTITION_SUBTYPE_ANY,
                label.as_ptr(),
            )
        };
        // Safety: non-null results point to a valid, static partition descriptor.
        let Some(partition) = (unsafe { partition.as_ref() }) else {
            bail!("partition {label:?} not found in the partition table");
        };
        let len = partition.size as usize;
        let mut data: *const c_void = core::ptr::null();
        let mut handle: ffi::esp_partition_mmap_handle_t = 0;
        // Safety: maps the whole partition read-only; unmapped in Drop.
        let ret = unsafe {
            ffi::esp_partition_mmap(
                partition,
                0,
                len,
                ffi::esp_partition_mmap_memory_t_ESP_PARTITION_MMAP_DATA,
                &mut data,
                &mut handle,
            )
        };
        ensure!(ret == 0, "mapping partition {label:?} failed: {ret}");
        let data = NonNull::new(data as *mut u8)
            .ok_or_else(|| anyhow::anyhow!("mapping partition {label:?} returned null"))?;
        Ok(Self { data, len, handle })
    }

    fn as_slice(&self) -> &[u8] {
        // Safety: `data` is a valid read-only mapping of `len` bytes until Drop.
        unsafe { core::slice::from_raw_parts(self.data.as_ptr(), self.len) }
    }
}

impl Drop for MappedPartition {
    fn drop(&mut self) {
        // Safety: `handle` came from a successful esp_partition_mmap and is released once.
        unsafe { ffi::esp_partition_munmap(self.handle) };
    }
}

/// Verifies that partition `label` holds an intact copy of `expected_model`; returns its
/// manifest. The mapping is released before returning, so ESP-DL can map it afresh.
pub fn verify(label: &CStr, expected_model: &str) -> Result<ModelManifest> {
    let started = Instant::now();
    let partition = MappedPartition::map(label)?;
    let manifest = manifest::verify(partition.as_slice(), expected_model)
        .map_err(|e| anyhow::anyhow!("partition {label:?}: {e}"))?;
    info!(
        "[Models] {label:?}: {} ({}), {} bytes, SHA-256 verified in {} ms",
        manifest.model,
        manifest.version,
        manifest.size,
        started.elapsed().as_millis()
    );
    Ok(manifest)
}
