//! Face-model partitions: verify before load, and choose between each model's two slots.
//!
//! The ESP-DL models live in their own flash partitions so they can be updated without
//! reflashing the firmware (see `crates/model-packer`). ESP-DL aborts the chip if a partition
//! cannot be mapped and has no integrity check, so every partition is verified here first:
//! the manifest in its last sector must name the expected model and match the data's SHA-256
//! (`biometric_core::manifest`).
//!
//! Each model has an A and a B slot. [`ModelStore::select`] picks the one to load, following
//! `biometric_core::activation`: a new image found in the standby slot is given a *golden run*
//! (the model is run on a fixed input and the SHA-256 of its outputs must equal the manifest's
//! `golden_sha256`) and only becomes active if it passes. The state is persisted in NVS
//! before the run, so a model that crashes the chip is rejected after a bounded number of
//! attempts instead of boot-looping the device.
//!
//! The golden run executes at the highest task priority. On this chip a context switch in the
//! middle of an inference can change the feature model's output by a few quantisation steps
//! (measured: identical digests at top priority, differing ones when the camera thread
//! preempts the run), and the comparison is exact.
//!
//! What the golden run proves: this firmware's ESP-DL build loads the model and computes what
//! the publisher's did. The SHA-256 already proves the bytes are intact; the golden run catches
//! a model that is intact but needs operators or memory this firmware does not have. The
//! digest is exact, so it must come from a device running the same ESP-DL version.

use core::ffi::{c_void, CStr};
use core::ptr::NonNull;
use std::ffi::CString;
use std::time::Instant;

use anyhow::{bail, ensure, Context, Result};
use biometric_core::activation::{self, Activation, Decision, Slot};
use biometric_core::contract::ModelSpec;
use biometric_core::manifest::{self, encode_hex, ModelManifest};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use log::{error, info, warn};

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

/// NVS namespace of the per-model activation records.
const NAMESPACE: &str = "models";

/// The slot chosen for a model: verified, and safe to hand to ESP-DL.
pub struct Selected {
    pub partition: CString,
    pub manifest: ModelManifest,
}

/// Chooses which slot of each model to load; keeps the choice in NVS.
pub struct ModelStore {
    nvs: EspNvs<NvsDefault>,
}

impl ModelStore {
    pub fn open(partition: EspDefaultNvsPartition) -> Result<Self> {
        let nvs = EspNvs::new(partition, NAMESPACE, true)
            .with_context(|| format!("opening NVS namespace `{NAMESPACE}`"))?;
        Ok(Self { nvs })
    }

    /// Verifies both slots of `spec`, runs a trial if the standby slot holds a new image, and
    /// returns the slot to load. `None` means the model is unavailable.
    pub fn select(&self, spec: &ModelSpec) -> Option<Selected> {
        let labels = spec
            .partitions
            .map(|label| CString::new(label).expect("partition labels contain no NUL"));
        let mut manifests = [Slot::A, Slot::B].map(|slot| {
            verify(&labels[slot.index()], spec.id)
                .inspect_err(|e| info!("[Models] {} slot {slot} not usable: {e:#}", spec.key))
                .ok()
        });
        let images = [0, 1].map(|i| manifests[i].as_ref().map(ModelManifest::image_id));

        let saved = self.load(spec.key);
        let mut state = saved;
        let mut decision = state.begin(&images);
        if let Decision::Trial(slot) = decision {
            let candidate = manifests[slot.index()]
                .as_ref()
                .expect("a trial slot holds a verified image");
            // The marker must be in flash before the model runs: see the module comment.
            let passed = match self.save(spec.key, &state) {
                Ok(()) => golden_run(spec.key, slot, &labels[slot.index()], candidate),
                Err(e) => {
                    error!(
                        "[Models] {}: cannot record the trial ({e:#}); not run",
                        spec.key
                    );
                    false
                }
            };
            decision = state.conclude(&images, passed);
        }
        if state != saved {
            if state.active != saved.active {
                info!("[Models] {}: active slot is now {}", spec.key, state.active);
            }
            if let Err(e) = self.save(spec.key, &state) {
                // Harmless but repeated: the same decision is taken again on the next boot.
                warn!(
                    "[Models] {}: cannot save the activation state: {e:#}",
                    spec.key
                );
            }
        }

        match decision {
            Decision::Use(slot) => {
                let manifest = manifests[slot.index()].take()?;
                info!(
                    "[Models] {}: using slot {slot}: {} ({})",
                    spec.key, manifest.model, manifest.version
                );
                let [a, b] = labels;
                Some(Selected {
                    partition: if slot == Slot::A { a } else { b },
                    manifest,
                })
            }
            Decision::Trial(_) | Decision::Unavailable => {
                error!("[Models] {}: no usable image in either slot", spec.key);
                None
            }
        }
    }

    fn load(&self, key: &str) -> Activation {
        let mut buf = [0u8; activation::ENCODED_LEN];
        match self.nvs.get_blob(key, &mut buf) {
            Ok(None) => Activation::default(),
            Ok(Some(record)) => Activation::decode(record).unwrap_or_else(|e| {
                warn!("[Models] {key}: activation record ignored: {e}");
                Activation::default()
            }),
            Err(e) => {
                warn!("[Models] {key}: reading the activation record failed: {e}");
                Activation::default()
            }
        }
    }

    fn save(&self, key: &str, state: &Activation) -> Result<()> {
        self.nvs
            .set_blob(key, &state.encode())
            .context("writing the activation record")
    }
}

/// Runs the candidate in `partition` on the fixed golden input and compares the digest of its
/// outputs with the manifest's. An image without a golden fails, but its digest is logged so
/// that it can be repackaged with one.
fn golden_run(key: &str, slot: Slot, partition: &CStr, candidate: &ModelManifest) -> bool {
    info!(
        "[Models] {key}: new image in slot {slot} ({}); golden run",
        candidate.version
    );
    let started = Instant::now();
    let mut digest = [0u8; 32];
    let ret = {
        let _uninterrupted = PriorityBoost::new();
        // Safety: `partition` was verified to hold an intact model image; `digest` is 32 bytes.
        unsafe { ffi::p4_model_golden(partition.as_ptr(), digest.as_mut_ptr()) }
    };
    if ret != 0 {
        error!("[Models] {key}: golden run failed: {ret}; image rejected");
        return false;
    }
    let computed = encode_hex(&digest);
    let elapsed = started.elapsed().as_millis();
    match candidate.golden() {
        Some(expected) if expected == digest => {
            info!("[Models] {key}: golden run passed in {elapsed} ms ({computed})");
            true
        }
        Some(expected) => {
            error!(
                "[Models] {key}: golden mismatch: computed {computed}, manifest {}; \
                 image rejected",
                encode_hex(&expected)
            );
            false
        }
        None => {
            warn!(
                "[Models] {key}: image has no golden and is not activated; \
                 this device computes {computed}"
            );
            false
        }
    }
}

/// Runs the calling task at the highest priority until dropped, so no other task on its core
/// can preempt it.
struct PriorityBoost {
    previous: ffi::UBaseType_t,
}

impl PriorityBoost {
    fn new() -> Self {
        // Safety: NULL addresses the calling task.
        let previous = unsafe { ffi::uxTaskPriorityGet(core::ptr::null_mut()) };
        // Safety: as above; the priority is within the configured range.
        unsafe { ffi::vTaskPrioritySet(core::ptr::null_mut(), ffi::configMAX_PRIORITIES - 1) };
        Self { previous }
    }
}

impl Drop for PriorityBoost {
    fn drop(&mut self) {
        // Safety: NULL addresses the calling task; restores the priority it had before.
        unsafe { ffi::vTaskPrioritySet(core::ptr::null_mut(), self.previous) };
    }
}
