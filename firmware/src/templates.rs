//! Enrolled members and their templates in flash: NVS blobs in the dedicated `templates`
//! partition.
//!
//! NVS gives what a hand-rolled file would have to reinvent: wear levelling, a CRC per entry,
//! and writes that either complete or leave the old state after a power cut. A member occupies
//! a storage slot `NN`: its identity is the blob `mNN` and its templates are the blobs `tNN_0`
//! and `tNN_1` (`biometric_core::template`). A `next_seq` counter numbers the
//! members enrolled on this device, so an id is never reused after a deletion.
//!
//! The member record is the commit point. Templates are written before it and erased after
//! it, so a power cut leaves either a complete member or template blobs without a member,
//! which are removed at the next start.
//!
//! The partition is not encrypted (see README, Enrollment & Templates): anyone who can read
//! the flash can read the templates.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use biometric_core::contract::{EMBEDDING_DIM, TEMPLATE_PARTITION};
use biometric_core::enrollment::{
    first_free_slot, member_id, member_name, reenrollment_target, template_slot, MAX_MEMBERS,
};
use biometric_core::hex;
use biometric_core::matching::{GroupMember, Modality, Role, Template};
use biometric_core::template::{self, MAX_MEMBER_LEN, MAX_TEMPLATES_PER_MEMBER};
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsCustom};
use log::{info, warn};

use crate::ffi;

const NAMESPACE: &str = "face_tpl";
const NEXT_SEQ_KEY: &str = "next_seq";

pub struct TemplateStore {
    nvs: EspNvs<NvsCustom>,
    /// This device's part of the ids of members enrolled here.
    device_id: String,
    /// Storage slot and member, in slot order; the admin list shows them in this order.
    entries: Vec<(u8, Arc<GroupMember>)>,
}

/// NVS keys are limited to 15 characters.
fn member_key(slot: u8) -> String {
    format!("m{slot:02}")
}

fn template_key(slot: u8, index: usize) -> String {
    format!("t{slot:02}_{index}")
}

fn template_buffer() -> Vec<u8> {
    vec![0u8; template::max_template_len(EMBEDDING_DIM)]
}

/// The factory MAC address, which makes ids of members enrolled here unique across devices.
fn read_device_id() -> Result<String> {
    let mut mac = [0u8; 6];
    // Safety: writes six bytes into `mac`.
    let ret = unsafe { ffi::esp_efuse_mac_get_default(mac.as_mut_ptr()) };
    anyhow::ensure!(ret == 0, "reading the factory MAC address failed: {ret}");
    Ok(hex::encode(&mac))
}

impl TemplateStore {
    /// Opens the partition (formatting it on first use) and loads every valid member.
    /// Unreadable records are reported and skipped.
    pub fn open() -> Result<Self> {
        let partition = EspNvsPartition::<NvsCustom>::take(TEMPLATE_PARTITION)
            .with_context(|| format!("NVS partition `{TEMPLATE_PARTITION}` unavailable"))?;
        let nvs = EspNvs::new(partition, NAMESPACE, true)
            .with_context(|| format!("opening NVS namespace `{NAMESPACE}`"))?;
        let mut store = Self {
            nvs,
            device_id: read_device_id()?,
            entries: Vec::new(),
        };
        store.load();
        let without_face = store
            .entries
            .iter()
            .filter(|(_, m)| !m.templates.iter().any(|t| t.modality == Modality::Face))
            .count();
        info!(
            "[Templates] device {}: {} of {MAX_MEMBERS} member slots in use, {without_face} \
             without a face template",
            store.device_id,
            store.entries.len()
        );
        Ok(store)
    }

    /// Loads every member with its templates; removes template blobs that belong to no member.
    /// A slot whose member record exists but cannot be read is skipped with its templates left
    /// in place: only a missing record means the templates are orphans.
    fn load(&mut self) {
        let mut member_buf = [0u8; MAX_MEMBER_LEN];
        for slot in 0..MAX_MEMBERS as u8 {
            let key = member_key(slot);
            let member = match self.nvs.get_blob(&key, &mut member_buf) {
                Ok(Some(record)) => match template::decode_member(record) {
                    Ok(member) => Some(member),
                    Err(e) => {
                        warn!("[Templates] {key}: {e}; slot skipped");
                        continue;
                    }
                },
                Ok(None) => None,
                Err(e) => {
                    warn!("[Templates] {key}: read failed: {e}; slot skipped");
                    continue;
                }
            };
            let stored = self.stored_templates(slot);
            let Some(mut member) = member else {
                // Left behind by an interrupted enrollment or deletion.
                for (index, _) in stored.iter().enumerate().filter(|(_, t)| t.is_some()) {
                    self.remove_template(slot, index, "has no member");
                }
                continue;
            };
            for (index, entry) in stored.into_iter().enumerate() {
                match entry {
                    Some((owner, template)) if owner == member.id => member.set_template(template),
                    Some(_) => self.remove_template(slot, index, "belongs to another member"),
                    None => {}
                }
            }
            self.entries.push((slot, Arc::new(member)));
        }
    }

    /// What each template blob of `slot` holds: the owning member's id and the template.
    /// Unreadable blobs are reported and count as empty.
    fn stored_templates(&self, slot: u8) -> [Option<(String, Template)>; MAX_TEMPLATES_PER_MEMBER] {
        let mut buf = template_buffer();
        core::array::from_fn(|index| {
            let key = template_key(slot, index);
            match self.nvs.get_blob(&key, &mut buf) {
                Ok(Some(record)) => template::decode_template(record)
                    .inspect_err(|e| warn!("[Templates] {key}: {e}; ignored"))
                    .ok(),
                Ok(None) => None,
                Err(e) => {
                    warn!("[Templates] {key}: read failed: {e}; ignored");
                    None
                }
            }
        })
    }

    fn remove_template(&self, slot: u8, index: usize, why: &str) {
        let key = template_key(slot, index);
        match self.nvs.remove(&key) {
            Ok(_) => warn!("[Templates] {key} {why}; erased"),
            Err(e) => warn!("[Templates] {key} {why}; erasing it failed: {e}"),
        }
    }

    fn write_member(&self, slot: u8, member: &GroupMember) -> Result<()> {
        let record =
            template::encode_member(member).map_err(|e| anyhow!("encoding member: {e}"))?;
        self.nvs
            .set_blob(&member_key(slot), &record)
            .context("writing the member")
    }

    fn write_template(&self, slot: u8, index: usize, id: &str, template: &Template) -> Result<()> {
        let record = template::encode_template(id, template)
            .map_err(|e| anyhow!("encoding template: {e}"))?;
        self.nvs
            .set_blob(&template_key(slot, index), &record)
            .context("writing the template")
    }

    /// The enrolled members, in the order used by [`Self::delete`].
    pub fn members(&self) -> Vec<Arc<GroupMember>> {
        self.entries
            .iter()
            .map(|(_, member)| member.clone())
            .collect()
    }

    /// Persists a face template `embedding` from the feature model release `model_version`
    /// and returns the member it belongs to.
    ///
    /// If `entered_name` names a member who has no face template for that release, the
    /// template is added to that member (enrolling again after a model change). Otherwise a
    /// new member is created; `entered_name` may then be empty.
    pub fn enroll(
        &mut self,
        entered_name: &str,
        embedding: Vec<f32>,
        model_version: &str,
    ) -> Result<Arc<GroupMember>> {
        let face = Template {
            modality: Modality::Face,
            model_version: model_version.to_owned(),
            embedding,
        };
        let members = self.entries.iter().map(|(_, member)| member.as_ref());
        if let Some(position) = reenrollment_target(members, entered_name, model_version) {
            return self.add_template(position, face);
        }

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
            id: member_id(&self.device_id, sequence),
            name: member_name(entered_name, sequence),
            role: Role::User,
            templates: vec![face],
        };

        // Counter first: if power fails between the writes an id is skipped, never reused.
        // The member record last: it is what makes the member exist.
        self.nvs
            .set_u32(NEXT_SEQ_KEY, sequence.wrapping_add(1))
            .context("writing the enrollment counter")?;
        self.write_template(slot, 0, &member.id, &member.templates[0])?;
        self.write_member(slot, &member)?;

        let member = Arc::new(member);
        let position = self.entries.partition_point(|&(s, _)| s < slot);
        self.entries.insert(position, (slot, member.clone()));
        info!(
            "[Templates] enrolled {} ({}) in slot {slot}",
            member.name, member.id
        );
        Ok(member)
    }

    /// Adds `new` to the member at `position`, replacing a template it supersedes.
    fn add_template(&mut self, position: usize, new: Template) -> Result<Arc<GroupMember>> {
        let (slot, current) = &self.entries[position];
        let slot = *slot;
        let stored = self.stored_templates(slot);
        let described: Vec<Option<(Modality, &str)>> = stored
            .iter()
            .map(|entry| {
                entry
                    .as_ref()
                    .map(|(_, t)| (t.modality, t.model_version.as_str()))
            })
            .collect();
        let index = template_slot(&described, new.modality, &new.model_version);
        self.write_template(slot, index, &current.id, &new)?;

        let mut member = GroupMember::clone(current);
        if let Some((_, replaced)) = &stored[index] {
            member.templates.retain(|t| t != replaced);
        }
        member.set_template(new);
        let member = Arc::new(member);
        self.entries[position].1 = member.clone();
        info!(
            "[Templates] added a face template to {} ({}) in slot {slot}, blob {index}",
            member.name, member.id
        );
        Ok(member)
    }

    /// Erases the member at `index` of [`Self::members`] and returns it.
    pub fn delete(&mut self, index: usize) -> Result<Arc<GroupMember>> {
        let &(slot, _) = self
            .entries
            .get(index)
            .with_context(|| format!("no member at index {index}"))?;
        // The member record first: without it the templates are erased at the next start
        // even if erasing them here is interrupted.
        self.nvs
            .remove(&member_key(slot))
            .context("erasing the member")?;
        for template in 0..MAX_TEMPLATES_PER_MEMBER {
            if let Err(e) = self.nvs.remove(&template_key(slot, template)) {
                warn!("[Templates] erasing {}: {e}", template_key(slot, template));
            }
        }
        let (_, member) = self.entries.remove(index);
        info!(
            "[Templates] deleted {} ({}) from slot {slot}",
            member.name, member.id
        );
        Ok(member)
    }
}
