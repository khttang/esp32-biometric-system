//! Enrolled templates in flash: one NVS blob per member, in the dedicated `templates` partition.
//!
//! NVS gives what a hand-rolled file would have to reinvent: wear levelling, a CRC per entry,
//! and writes that either complete or leave the old state after a power cut. Each blob is a
//! `biometric_core::template` record under the key `tplNN`; a `next_seq` counter numbers the
//! members enrolled on this device, so an id is never reused after a deletion.
//!
//! The partition is not encrypted yet (planned with flash encryption in M4): anyone who can
//! read the flash can read the templates.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use biometric_core::contract::{EMBEDDING_DIM, TEMPLATE_PARTITION};
use biometric_core::enrollment::{first_free_slot, local_member_id, member_name, MAX_MEMBERS};
use biometric_core::matching::{GroupMember, Role};
use biometric_core::template;
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsCustom};
use log::{info, warn};

const NAMESPACE: &str = "face_tpl";
const NEXT_SEQ_KEY: &str = "next_seq";

pub struct TemplateStore {
    nvs: EspNvs<NvsCustom>,
    /// Storage slot and member, in slot order; the admin list shows them in this order.
    entries: Vec<(u8, Arc<GroupMember>)>,
}

impl TemplateStore {
    /// Opens the partition (formatting it on first use) and loads every valid template.
    /// Unreadable records are reported and skipped; their slots are reused by later enrollments.
    pub fn open() -> Result<Self> {
        let partition = EspNvsPartition::<NvsCustom>::take(TEMPLATE_PARTITION)
            .with_context(|| format!("NVS partition `{TEMPLATE_PARTITION}` unavailable"))?;
        let nvs = EspNvs::new(partition, NAMESPACE, true)
            .with_context(|| format!("opening NVS namespace `{NAMESPACE}`"))?;

        let mut buf = vec![0u8; template::max_encoded_len(EMBEDDING_DIM)];
        let mut entries = Vec::new();
        for slot in 0..MAX_MEMBERS as u8 {
            let key = slot_key(slot);
            match nvs.get_blob(&key, &mut buf) {
                Ok(None) => {}
                Ok(Some(record)) => match template::decode(record) {
                    Ok(member) => entries.push((slot, Arc::new(member))),
                    Err(e) => warn!("[Templates] {key}: {e}; ignored"),
                },
                Err(e) => warn!("[Templates] {key}: read failed: {e}; ignored"),
            }
        }
        info!(
            "[Templates] {} of {MAX_MEMBERS} slots in use",
            entries.len()
        );
        Ok(Self { nvs, entries })
    }

    /// The enrolled members, in the order used by [`Self::delete`].
    pub fn members(&self) -> Vec<Arc<GroupMember>> {
        self.entries
            .iter()
            .map(|(_, member)| member.clone())
            .collect()
    }

    /// Persists a new member with `embedding` (from the feature model release
    /// `model_version`) and returns it. `entered_name` may be empty.
    pub fn enroll(
        &mut self,
        entered_name: &str,
        embedding: Vec<f32>,
        model_version: &str,
    ) -> Result<Arc<GroupMember>> {
        let used = self.entries.iter().map(|&(slot, _)| slot);
        let Some(slot) = first_free_slot(used, MAX_MEMBERS) else {
            bail!("all {MAX_MEMBERS} member slots are in use");
        };
        let sequence = self
            .nvs
            .get_u32(NEXT_SEQ_KEY)
            .context("reading the enrollment counter")?
            .unwrap_or(1);
        let member = GroupMember {
            id: local_member_id(sequence),
            name: member_name(entered_name, sequence),
            role: Role::User,
            model_version: model_version.to_owned(),
            face_embedding: embedding,
        };
        let record = template::encode(&member).map_err(|e| anyhow!("encoding template: {e}"))?;

        // Counter first: if power fails between the two writes an id is skipped, never reused.
        self.nvs
            .set_u32(NEXT_SEQ_KEY, sequence.wrapping_add(1))
            .context("writing the enrollment counter")?;
        self.nvs
            .set_blob(&slot_key(slot), &record)
            .context("writing the template")?;

        let member = Arc::new(member);
        let position = self.entries.partition_point(|&(s, _)| s < slot);
        self.entries.insert(position, (slot, member.clone()));
        info!(
            "[Templates] enrolled {} ({}) in slot {slot}, {} bytes",
            member.name,
            member.id,
            record.len()
        );
        Ok(member)
    }

    /// Erases the member at `index` of [`Self::members`] and returns it.
    pub fn delete(&mut self, index: usize) -> Result<Arc<GroupMember>> {
        let &(slot, _) = self
            .entries
            .get(index)
            .with_context(|| format!("no member at index {index}"))?;
        self.nvs
            .remove(&slot_key(slot))
            .context("erasing the template")?;
        let (_, member) = self.entries.remove(index);
        info!(
            "[Templates] deleted {} ({}) from slot {slot}",
            member.name, member.id
        );
        Ok(member)
    }
}

/// NVS key of a storage slot (keys are limited to 15 characters).
fn slot_key(slot: u8) -> String {
    format!("tpl{slot:02}")
}
