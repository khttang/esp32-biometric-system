//! How enrolled members and their templates are stored on the device (record format 2).
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
//! A template names its member, so a record is meaningful on its own. A record is exactly as
//! long as its header says: truncated records and trailing bytes are both rejected. The format
//! carries no checksum of its own; the firmware stores records as NVS blobs, which are
//! CRC-protected.
//!
//! The model version is part of a template record because an embedding only means something
//! to the model that produced it (see [`crate::matching`]). Format 1, one record per member
//! with a single face embedding inside it, is no longer read.

use core::fmt;

use crate::matching::{is_normalised, GroupMember, Modality, Role, Template};

const MEMBER_MAGIC: [u8; 4] = *b"FMBR";
const TEMPLATE_MAGIC: [u8; 4] = *b"FTPL";
/// Record layout version written and understood by this firmware.
const TEMPLATE_FORMAT: u8 = 2;

const MAX_ID_LEN: usize = 64;
pub(crate) const MAX_NAME_LEN: usize = 64;
const MAX_MODEL_VERSION_LEN: usize = 64;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// Shorter than its header.
    Truncated {
        len: usize,
        header: usize,
    },
    BadMagic,
    UnsupportedFormat(u8),
    UnknownRole(u8),
    UnknownModality(u8),
    /// A text field is empty or longer than its limit.
    FieldLength {
        field: &'static str,
        len: usize,
        max: usize,
    },
    /// The embedding is empty or has more values than the header can describe.
    EmbeddingDimension(usize),
    /// The record's length disagrees with its header.
    LengthMismatch {
        expected: usize,
        found: usize,
    },
    InvalidUtf8(&'static str),
    /// The embedding contains NaN or infinity.
    NonFiniteEmbedding,
    /// The embedding is not L2-normalised, so its similarity scores would mean nothing.
    NotNormalised,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { len, header } => {
                write!(
                    f,
                    "record of {len} bytes is shorter than a {header}-byte header"
                )
            }
            Self::BadMagic => write!(f, "not a member or template record (bad magic)"),
            Self::UnsupportedFormat(v) => {
                write!(f, "unsupported record format {v}")
            }
            Self::UnknownRole(r) => write!(f, "unknown role {r}"),
            Self::UnknownModality(m) => write!(f, "unknown modality {m}"),
            Self::FieldLength { field, len, max } => {
                write!(f, "{field} of {len} bytes is outside 1..={max} bytes")
            }
            Self::EmbeddingDimension(dim) => {
                write!(f, "embedding dimension {dim} is outside 1..={}", u16::MAX)
            }
            Self::LengthMismatch { expected, found } => {
                write!(
                    f,
                    "record is {found} bytes, its header describes {expected}"
                )
            }
            Self::InvalidUtf8(field) => write!(f, "{field} is not valid UTF-8"),
            Self::NonFiniteEmbedding => write!(f, "embedding contains NaN or infinity"),
            Self::NotNormalised => write!(f, "embedding is not a unit vector"),
        }
    }
}

impl std::error::Error for TemplateError {}

fn field_len(field: &'static str, value: &str, max: usize) -> Result<u8, TemplateError> {
    u8::try_from(value.len())
        .ok()
        .filter(|&len| len > 0 && usize::from(len) <= max)
        .ok_or(TemplateError::FieldLength {
            field,
            len: value.len(),
            max,
        })
}

fn stored_len(field: &'static str, len: u8, max: usize) -> Result<usize, TemplateError> {
    let len = usize::from(len);
    if len == 0 || len > max {
        return Err(TemplateError::FieldLength { field, len, max });
    }
    Ok(len)
}

fn text(field: &'static str, bytes: &[u8]) -> Result<String, TemplateError> {
    core::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| TemplateError::InvalidUtf8(field))
}

fn role_to_byte(role: Role) -> u8 {
    match role {
        Role::Admin => 0,
        Role::User => 1,
        Role::Guest => 2,
    }
}

fn role_from_byte(byte: u8) -> Result<Role, TemplateError> {
    match byte {
        0 => Ok(Role::Admin),
        1 => Ok(Role::User),
        2 => Ok(Role::Guest),
        other => Err(TemplateError::UnknownRole(other)),
    }
}

/// What every stored embedding must be: finite and L2-normalised.
fn check_embedding(embedding: &[f32]) -> Result<(), TemplateError> {
    if embedding.iter().any(|v| !v.is_finite()) {
        return Err(TemplateError::NonFiniteEmbedding);
    }
    if !is_normalised(embedding) {
        return Err(TemplateError::NotNormalised);
    }
    Ok(())
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
        return Err(TemplateError::Truncated {
            len: record.len(),
            header: header_len,
        });
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
    check_embedding(&template.embedding)?;
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
    check_embedding(&embedding)?;
    let template = Template {
        modality,
        model_version: text("model version", version)?,
        embedding,
    };
    Ok((text("member id", id)?, template))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let header = if is_member { 8 } else { 12 };
            assert_eq!(
                decode(&record[..3]),
                Err(TemplateError::Truncated { len: 3, header })
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
    fn embeddings_that_are_not_unit_vectors_are_rejected_both_ways() {
        let mut t = template();
        t.embedding = vec![2.0, 0.0];
        assert_eq!(encode_template("m", &t), Err(TemplateError::NotNormalised));
        t.embedding = vec![0.0, 0.0];
        assert_eq!(encode_template("m", &t), Err(TemplateError::NotNormalised));

        // A stored record whose first value was changed from 0.6 to 3.0.
        let mut record = encode_template("m", &template()).unwrap();
        let first = record.len() - 4 * 4;
        record[first..first + 4].copy_from_slice(&3.0f32.to_le_bytes());
        assert_eq!(decode_template(&record), Err(TemplateError::NotNormalised));
    }

    #[test]
    fn a_format_1_record_is_rejected() {
        let mut record = encode_template("m", &template()).unwrap();
        record[4] = 1;
        assert_eq!(
            decode_template(&record),
            Err(TemplateError::UnsupportedFormat(1))
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
}
