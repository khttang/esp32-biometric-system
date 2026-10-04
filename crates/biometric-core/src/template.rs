//! Template format v2: how enrolled members and their templates are stored on the device.
//!
//! A member and each of its templates are separate records, so a member can have several
//! templates (one per modality and model release) or, after a model change, none.
//!
//! Member record:
//!
//! ```text
//! offset  size  field
//!      0     4  magic "FMBR"
//!      4     1  format version (2)
//!      5     1  role (0 = admin, 1 = user, 2 = guest)
//!      6     1  id length in bytes (1..=64)
//!      7     1  name length in bytes (1..=64)
//!      8     …  id, name (UTF-8)
//! ```
//!
//! Template record:
//!
//! ```text
//! offset  size  field
//!      0     4  magic "FTPL"
//!      4     1  format version (2)
//!      5     1  modality (0 = face, 1 = voice)
//!      6     1  member id length in bytes (1..=64)
//!      7     1  model version length in bytes (1..=64)
//!      8     2  reserved (written as 0, ignored when read)
//!     10     2  embedding dimension, little-endian
//!     12     …  member id, model version (UTF-8), then the embedding as little-endian f32
//! ```
//!
//! A template names its member, so a record is meaningful on its own (and can later be sent to
//! a server as it is). A record is exactly as long as its header says: truncated records and
//! trailing bytes are both rejected. The format carries no checksum of its own; the firmware
//! stores records as NVS blobs, which are CRC-protected.
//!
//! Format v1 ([`crate::template_v1`]) kept one face embedding inside the member record;
//! [`migrate_v1`] converts such a record.

use crate::enrollment::migrated_member_id;
use crate::matching::{GroupMember, Modality, Template};
use crate::template_v1::{field_len, role_from_byte, role_to_byte, stored_len, text, V1Record};
pub use crate::template_v1::{TemplateError, MAX_ID_LEN, MAX_MODEL_VERSION_LEN, MAX_NAME_LEN};

pub const MEMBER_MAGIC: [u8; 4] = *b"FMBR";
pub const TEMPLATE_MAGIC: [u8; 4] = *b"FTPL";
/// Record layout version written by this firmware.
pub const TEMPLATE_FORMAT: u8 = 2;

/// Templates a member can hold on the device: enough for a face template for the model in use
/// plus one for a model being introduced. Bounded by the size of the template partition.
pub const MAX_TEMPLATES_PER_MEMBER: usize = 2;

const MEMBER_HEADER_LEN: usize = 8;
const TEMPLATE_HEADER_LEN: usize = 12;

/// Largest member record; sizes the firmware's read buffer.
pub const MAX_MEMBER_LEN: usize = MEMBER_HEADER_LEN + MAX_ID_LEN + MAX_NAME_LEN;

/// Largest template record for an embedding of `dim` values.
pub const fn max_template_len(dim: usize) -> usize {
    TEMPLATE_HEADER_LEN + MAX_ID_LEN + MAX_MODEL_VERSION_LEN + dim * size_of::<f32>()
}

fn modality_to_byte(modality: Modality) -> u8 {
    match modality {
        Modality::Face => 0,
        Modality::Voice => 1,
    }
}

fn modality_from_byte(byte: u8) -> Result<Modality, TemplateError> {
    match byte {
        0 => Ok(Modality::Face),
        1 => Ok(Modality::Voice),
        other => Err(TemplateError::UnknownModality(other)),
    }
}

fn check_header(record: &[u8], magic: [u8; 4], header_len: usize) -> Result<(), TemplateError> {
    if record.len() < header_len {
        return Err(TemplateError::Truncated { len: record.len() });
    }
    if record[..4] != magic {
        return Err(TemplateError::BadMagic);
    }
    if record[4] != TEMPLATE_FORMAT {
        return Err(TemplateError::UnsupportedFormat(record[4]));
    }
    Ok(())
}

fn check_len(record: &[u8], expected: usize) -> Result<(), TemplateError> {
    if record.len() != expected {
        return Err(TemplateError::LengthMismatch {
            expected,
            found: record.len(),
        });
    }
    Ok(())
}

/// Serialises `member`'s identity (id, name, role) as a member record. Its templates are
/// stored separately with [`encode_template`].
pub fn encode_member(member: &GroupMember) -> Result<Vec<u8>, TemplateError> {
    let id = field_len("id", &member.id, MAX_ID_LEN)?;
    let name = field_len("name", &member.name, MAX_NAME_LEN)?;
    let mut record = Vec::with_capacity(MEMBER_HEADER_LEN + member.id.len() + member.name.len());
    record.extend_from_slice(&MEMBER_MAGIC);
    record.extend_from_slice(&[TEMPLATE_FORMAT, role_to_byte(member.role), id, name]);
    record.extend_from_slice(member.id.as_bytes());
    record.extend_from_slice(member.name.as_bytes());
    Ok(record)
}

/// Parses a member record; the member has no templates yet.
pub fn decode_member(record: &[u8]) -> Result<GroupMember, TemplateError> {
    check_header(record, MEMBER_MAGIC, MEMBER_HEADER_LEN)?;
    let role = role_from_byte(record[5])?;
    let id_len = stored_len("id", record[6], MAX_ID_LEN)?;
    let name_len = stored_len("name", record[7], MAX_NAME_LEN)?;
    check_len(record, MEMBER_HEADER_LEN + id_len + name_len)?;
    let (id, name) = record[MEMBER_HEADER_LEN..].split_at(id_len);
    Ok(GroupMember {
        id: text("id", id)?,
        name: text("name", name)?,
        role,
        templates: Vec::new(),
    })
}

/// Serialises `template` of the member `member_id` as a template record.
pub fn encode_template(member_id: &str, template: &Template) -> Result<Vec<u8>, TemplateError> {
    let id = field_len("member id", member_id, MAX_ID_LEN)?;
    let version = field_len(
        "model version",
        &template.model_version,
        MAX_MODEL_VERSION_LEN,
    )?;
    let dim = template.embedding.len();
    let dim_field = u16::try_from(dim)
        .ok()
        .filter(|&d| d > 0)
        .ok_or(TemplateError::EmbeddingDimension(dim))?;
    if template.embedding.iter().any(|v| !v.is_finite()) {
        return Err(TemplateError::NonFiniteEmbedding);
    }
    let mut record = Vec::with_capacity(
        TEMPLATE_HEADER_LEN
            + member_id.len()
            + template.model_version.len()
            + dim * size_of::<f32>(),
    );
    record.extend_from_slice(&TEMPLATE_MAGIC);
    record.extend_from_slice(&[
        TEMPLATE_FORMAT,
        modality_to_byte(template.modality),
        id,
        version,
        0,
        0,
    ]);
    record.extend_from_slice(&dim_field.to_le_bytes());
    record.extend_from_slice(member_id.as_bytes());
    record.extend_from_slice(template.model_version.as_bytes());
    for value in &template.embedding {
        record.extend_from_slice(&value.to_le_bytes());
    }
    Ok(record)
}

/// Parses a template record: the id of the member it belongs to, and the template.
pub fn decode_template(record: &[u8]) -> Result<(String, Template), TemplateError> {
    check_header(record, TEMPLATE_MAGIC, TEMPLATE_HEADER_LEN)?;
    let modality = modality_from_byte(record[5])?;
    let id_len = stored_len("member id", record[6], MAX_ID_LEN)?;
    let version_len = stored_len("model version", record[7], MAX_MODEL_VERSION_LEN)?;
    let dim = usize::from(u16::from_le_bytes([record[10], record[11]]));
    if dim == 0 {
        return Err(TemplateError::EmbeddingDimension(dim));
    }
    check_len(
        record,
        TEMPLATE_HEADER_LEN + id_len + version_len + dim * size_of::<f32>(),
    )?;
    let (id, rest) = record[TEMPLATE_HEADER_LEN..].split_at(id_len);
    let (version, values) = rest.split_at(version_len);
    // `check_len` guarantees a whole number of values.
    let embedding: Vec<f32> = values
        .as_chunks::<{ size_of::<f32>() }>()
        .0
        .iter()
        .map(|&bytes| f32::from_le_bytes(bytes))
        .collect();
    if embedding.iter().any(|v| !v.is_finite()) {
        return Err(TemplateError::NonFiniteEmbedding);
    }
    let template = Template {
        modality,
        model_version: text("model version", version)?,
        embedding,
    };
    Ok((text("member id", id)?, template))
}

/// The member a format v1 record becomes on the device `device_id`: the same person with the
/// record's embedding as a face template, under an id that is unique across devices.
pub fn migrate_v1(record: V1Record, device_id: &str) -> GroupMember {
    GroupMember {
        id: migrated_member_id(&record.id, device_id),
        name: record.name,
        role: record.role,
        templates: vec![Template {
            modality: Modality::Face,
            model_version: record.model_version,
            embedding: record.embedding,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::Role;
    use crate::template_v1;

    const DEVICE: &str = "80f1b2d2da2e";

    fn template() -> Template {
        Template {
            modality: Modality::Face,
            model_version: "human_face_recognition 0.3.2".into(),
            embedding: vec![0.6, -0.8, 0.0, 1.0e-7],
        }
    }

    fn member() -> GroupMember {
        GroupMember {
            id: format!("{DEVICE}-0001"),
            name: "Ada Lovelace".into(),
            role: Role::Admin,
            templates: vec![template()],
        }
    }

    #[test]
    fn member_round_trips_without_its_templates() {
        let record = encode_member(&member()).unwrap();
        assert_eq!(&record[..5], b"FMBR\x02");
        assert_eq!(record.len(), 8 + 17 + 12);
        let decoded = decode_member(&record).unwrap();
        assert_eq!(
            decoded,
            GroupMember {
                templates: vec![],
                ..member()
            }
        );
        assert!(record.len() <= MAX_MEMBER_LEN);
    }

    #[test]
    fn template_round_trips_bit_exactly() {
        let record = encode_template(&member().id, &template()).unwrap();
        assert_eq!(&record[..5], b"FTPL\x02");
        assert_eq!(record.len(), 12 + 17 + 28 + 4 * 4);
        assert_eq!(decode_template(&record), Ok((member().id, template())));
        assert!(record.len() <= max_template_len(4));
    }

    #[test]
    fn modality_is_stored() {
        let voice = Template {
            modality: Modality::Voice,
            ..template()
        };
        let record = encode_template("m", &voice).unwrap();
        assert_eq!(record[5], 1);
        assert_eq!(
            decode_template(&record).unwrap().1.modality,
            Modality::Voice
        );
        let mut unknown = record;
        unknown[5] = 9;
        assert_eq!(
            decode_template(&unknown),
            Err(TemplateError::UnknownModality(9))
        );
    }

    #[test]
    fn records_of_the_other_kind_or_format_are_rejected() {
        let member_record = encode_member(&member()).unwrap();
        let template_record = encode_template("m", &template()).unwrap();
        assert_eq!(
            decode_template(&member_record),
            Err(TemplateError::BadMagic)
        );
        assert_eq!(
            decode_member(&template_record),
            Err(TemplateError::BadMagic)
        );

        let mut future = template_record.clone();
        future[4] = 3;
        assert_eq!(
            decode_template(&future),
            Err(TemplateError::UnsupportedFormat(3))
        );
        let mut future = member_record;
        future[4] = 3;
        assert_eq!(
            decode_member(&future),
            Err(TemplateError::UnsupportedFormat(3))
        );
    }

    #[test]
    fn a_v1_record_is_not_a_v2_template_and_vice_versa() {
        let v1 = template_v1::encode(&V1Record {
            id: "local-0001".into(),
            name: "Ada".into(),
            role: Role::User,
            model_version: "m".into(),
            embedding: vec![1.0],
        })
        .unwrap();
        assert_eq!(
            decode_template(&v1),
            Err(TemplateError::UnsupportedFormat(1))
        );
        let v2 = encode_template("m", &template()).unwrap();
        assert_eq!(
            template_v1::decode(&v2),
            Err(TemplateError::UnsupportedFormat(2))
        );
    }

    #[test]
    fn truncated_and_padded_records_are_rejected() {
        for record in [
            encode_member(&member()).unwrap(),
            encode_template("m", &template()).unwrap(),
        ] {
            let is_member = record[..4] == MEMBER_MAGIC;
            let decode = |bytes: &[u8]| -> Result<(), TemplateError> {
                if is_member {
                    decode_member(bytes).map(|_| ())
                } else {
                    decode_template(bytes).map(|_| ())
                }
            };
            assert_eq!(
                decode(&record[..3]),
                Err(TemplateError::Truncated { len: 3 })
            );
            assert!(matches!(
                decode(&record[..record.len() - 1]),
                Err(TemplateError::LengthMismatch { .. })
            ));
            let mut padded = record.clone();
            padded.push(0);
            assert!(matches!(
                decode(&padded),
                Err(TemplateError::LengthMismatch { .. })
            ));
            assert_eq!(decode(&record), Ok(()));
        }
    }

    #[test]
    fn field_limits_are_enforced_when_encoding() {
        let long = "x".repeat(MAX_ID_LEN + 1);
        let mut m = member();
        m.id = long.clone();
        assert!(matches!(
            encode_member(&m),
            Err(TemplateError::FieldLength { field: "id", .. })
        ));
        m = member();
        m.name.clear();
        assert!(matches!(
            encode_member(&m),
            Err(TemplateError::FieldLength { field: "name", .. })
        ));
        assert!(matches!(
            encode_template(&long, &template()),
            Err(TemplateError::FieldLength {
                field: "member id",
                ..
            })
        ));
        let mut t = template();
        t.model_version.clear();
        assert!(matches!(
            encode_template("m", &t),
            Err(TemplateError::FieldLength {
                field: "model version",
                ..
            })
        ));
        // The longest allowed fields fit the buffers the firmware sizes from these constants.
        m = member();
        m.id = "i".repeat(MAX_ID_LEN);
        m.name = "n".repeat(MAX_NAME_LEN);
        assert_eq!(encode_member(&m).unwrap().len(), MAX_MEMBER_LEN);
        t = template();
        t.model_version = "v".repeat(MAX_MODEL_VERSION_LEN);
        assert_eq!(
            encode_template(&m.id, &t).unwrap().len(),
            max_template_len(4)
        );
    }

    #[test]
    fn bad_embeddings_are_rejected_both_ways() {
        let mut t = template();
        t.embedding.clear();
        assert_eq!(
            encode_template("m", &t),
            Err(TemplateError::EmbeddingDimension(0))
        );
        t.embedding = vec![f32::NAN];
        assert_eq!(
            encode_template("m", &t),
            Err(TemplateError::NonFiniteEmbedding)
        );

        let mut record = encode_template("m", &template()).unwrap();
        let last = record.len() - 4;
        record[last..].copy_from_slice(&f32::INFINITY.to_le_bytes());
        assert_eq!(
            decode_template(&record),
            Err(TemplateError::NonFiniteEmbedding)
        );
    }

    #[test]
    fn invalid_utf8_and_unknown_role_are_rejected() {
        let mut record = encode_member(&member()).unwrap();
        record[8] = 0xFF;
        assert_eq!(
            decode_member(&record),
            Err(TemplateError::InvalidUtf8("id"))
        );
        let mut record = encode_member(&member()).unwrap();
        record[5] = 7;
        assert_eq!(decode_member(&record), Err(TemplateError::UnknownRole(7)));
        let mut record = encode_template("member", &template()).unwrap();
        record[12] = 0xFF;
        assert_eq!(
            decode_template(&record),
            Err(TemplateError::InvalidUtf8("member id"))
        );
    }

    #[test]
    fn reserved_bytes_are_ignored_when_reading() {
        let mut record = encode_template("m", &template()).unwrap();
        record[8] = 0xAA;
        record[9] = 0x55;
        assert_eq!(decode_template(&record), Ok(("m".into(), template())));
    }

    #[test]
    fn a_v1_record_migrates_to_a_member_with_one_face_template() {
        let v1 = V1Record {
            id: "local-0007".into(),
            name: "Ada".into(),
            role: Role::User,
            model_version: "human_face_recognition 0.3.2".into(),
            embedding: vec![0.6, 0.8],
        };
        // Through the stored bytes, as the firmware does it.
        let stored = template_v1::encode(&v1).unwrap();
        let migrated = migrate_v1(template_v1::decode(&stored).unwrap(), DEVICE);
        assert_eq!(migrated.id, "80f1b2d2da2e-0007");
        assert_eq!((migrated.name.as_str(), migrated.role), ("Ada", Role::User));
        assert_eq!(
            migrated.templates,
            [Template {
                modality: Modality::Face,
                model_version: "human_face_recognition 0.3.2".into(),
                embedding: vec![0.6, 0.8],
            }]
        );
        // And what was migrated can be stored in the new format.
        let member_record = encode_member(&migrated).unwrap();
        let template_record = encode_template(&migrated.id, &migrated.templates[0]).unwrap();
        assert_eq!(decode_member(&member_record).unwrap().id, migrated.id);
        assert_eq!(decode_template(&template_record).unwrap().0, migrated.id);
    }
}
