//! Template format v1: how one enrolled member is stored on the device.
//!
//! ```text
//! offset  size  field
//!      0     4  magic "FTPL"
//!      4     1  format version (1)
//!      5     1  role (0 = admin, 1 = user, 2 = guest)
//!      6     1  id length in bytes (1..=64)
//!      7     1  name length in bytes (1..=64)
//!      8     1  model version length in bytes (1..=64)
//!      9     1  reserved (written as 0, ignored when read)
//!     10     2  embedding dimension, little-endian
//!     12     …  id, name, model version (UTF-8), then the embedding as little-endian f32
//! ```
//!
//! A record is exactly as long as its header says: truncated records and trailing bytes are
//! both rejected. The format carries no checksum of its own; the firmware stores records as
//! NVS blobs, which are CRC-protected.
//!
//! The model version is part of the record because an embedding only means something to the
//! model that produced it (see [`crate::matching`]).

use core::fmt;

use crate::matching::{GroupMember, Role};

pub const TEMPLATE_MAGIC: [u8; 4] = *b"FTPL";
/// Record layout version written and understood by this firmware.
pub const TEMPLATE_FORMAT: u8 = 1;

pub const MAX_ID_LEN: usize = 64;
pub const MAX_NAME_LEN: usize = 64;
pub const MAX_MODEL_VERSION_LEN: usize = 64;

const HEADER_LEN: usize = 12;

/// Largest record for an embedding of `dim` values; sizes the firmware's read buffer.
pub const fn max_encoded_len(dim: usize) -> usize {
    HEADER_LEN + MAX_ID_LEN + MAX_NAME_LEN + MAX_MODEL_VERSION_LEN + dim * size_of::<f32>()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// Shorter than a header.
    Truncated {
        len: usize,
    },
    BadMagic,
    UnsupportedFormat(u8),
    UnknownRole(u8),
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
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { len } => {
                write!(
                    f,
                    "record of {len} bytes is shorter than a {HEADER_LEN}-byte header"
                )
            }
            Self::BadMagic => write!(f, "not a template record (bad magic)"),
            Self::UnsupportedFormat(v) => {
                write!(
                    f,
                    "unsupported template format {v} (expected {TEMPLATE_FORMAT})"
                )
            }
            Self::UnknownRole(r) => write!(f, "unknown role {r}"),
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
        }
    }
}

impl std::error::Error for TemplateError {}

/// Serialises `member` as a format v1 record.
pub fn encode(member: &GroupMember) -> Result<Vec<u8>, TemplateError> {
    let id = field_len("id", &member.id, MAX_ID_LEN)?;
    let name = field_len("name", &member.name, MAX_NAME_LEN)?;
    let model_version = field_len(
        "model version",
        &member.model_version,
        MAX_MODEL_VERSION_LEN,
    )?;
    let dim = member.face_embedding.len();
    let dim_field = u16::try_from(dim)
        .ok()
        .filter(|&d| d > 0)
        .ok_or(TemplateError::EmbeddingDimension(dim))?;
    if member.face_embedding.iter().any(|v| !v.is_finite()) {
        return Err(TemplateError::NonFiniteEmbedding);
    }

    let mut record = Vec::with_capacity(
        HEADER_LEN
            + member.id.len()
            + member.name.len()
            + member.model_version.len()
            + dim * size_of::<f32>(),
    );
    record.extend_from_slice(&TEMPLATE_MAGIC);
    record.extend_from_slice(&[
        TEMPLATE_FORMAT,
        role_to_byte(member.role),
        id,
        name,
        model_version,
        0,
    ]);
    record.extend_from_slice(&dim_field.to_le_bytes());
    record.extend_from_slice(member.id.as_bytes());
    record.extend_from_slice(member.name.as_bytes());
    record.extend_from_slice(member.model_version.as_bytes());
    for value in &member.face_embedding {
        record.extend_from_slice(&value.to_le_bytes());
    }
    Ok(record)
}

/// Parses a format v1 record.
pub fn decode(record: &[u8]) -> Result<GroupMember, TemplateError> {
    let Some((header, body)) = record.split_first_chunk::<HEADER_LEN>() else {
        return Err(TemplateError::Truncated { len: record.len() });
    };
    if header[..4] != TEMPLATE_MAGIC {
        return Err(TemplateError::BadMagic);
    }
    if header[4] != TEMPLATE_FORMAT {
        return Err(TemplateError::UnsupportedFormat(header[4]));
    }
    let role = role_from_byte(header[5])?;
    let id_len = stored_len("id", header[6], MAX_ID_LEN)?;
    let name_len = stored_len("name", header[7], MAX_NAME_LEN)?;
    let model_version_len = stored_len("model version", header[8], MAX_MODEL_VERSION_LEN)?;
    let dim = usize::from(u16::from_le_bytes([header[10], header[11]]));
    if dim == 0 {
        return Err(TemplateError::EmbeddingDimension(dim));
    }

    let expected = HEADER_LEN + id_len + name_len + model_version_len + dim * size_of::<f32>();
    if record.len() != expected {
        return Err(TemplateError::LengthMismatch {
            expected,
            found: record.len(),
        });
    }
    let (id, rest) = body.split_at(id_len);
    let (name, rest) = rest.split_at(name_len);
    let (model_version, embedding) = rest.split_at(model_version_len);

    // `expected` above guarantees a whole number of values.
    let (values, _) = embedding.as_chunks::<{ size_of::<f32>() }>();
    let face_embedding: Vec<f32> = values
        .iter()
        .map(|&bytes| f32::from_le_bytes(bytes))
        .collect();
    if face_embedding.iter().any(|v| !v.is_finite()) {
        return Err(TemplateError::NonFiniteEmbedding);
    }
    Ok(GroupMember {
        id: text("id", id)?,
        name: text("name", name)?,
        role,
        model_version: text("model version", model_version)?,
        face_embedding,
    })
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::EMBEDDING_DIM;

    fn member() -> GroupMember {
        GroupMember {
            id: "local-0007".into(),
            name: "Ada Lovelace".into(),
            role: Role::Admin,
            model_version: "human_face_recognition 0.3.2".into(),
            face_embedding: vec![0.6, -0.8, 0.0, 1.5e-3],
        }
    }

    #[test]
    fn round_trips_every_field() {
        let original = member();
        assert_eq!(decode(&encode(&original).unwrap()).unwrap(), original);
    }

    #[test]
    fn round_trips_every_role() {
        for role in [Role::Admin, Role::User, Role::Guest] {
            let original = GroupMember { role, ..member() };
            assert_eq!(decode(&encode(&original).unwrap()).unwrap().role, role);
        }
    }

    #[test]
    fn round_trips_non_ascii_names() {
        let original = GroupMember {
            name: "Zoë Åström 花".into(),
            ..member()
        };
        assert_eq!(decode(&encode(&original).unwrap()).unwrap(), original);
    }

    #[test]
    fn layout_is_stable() {
        // Guards the on-flash format: changing these bytes orphans enrolled templates.
        let record = encode(&GroupMember {
            id: "a".into(),
            name: "bc".into(),
            role: Role::User,
            model_version: "v1".into(),
            face_embedding: vec![1.0],
        })
        .unwrap();
        assert_eq!(
            record,
            [
                b'F', b'T', b'P', b'L', 1, 1, 1, 2, 2, 0, 1, 0, // header
                b'a', b'b', b'c', b'v', b'1', // id, name, model version
                0x00, 0x00, 0x80, 0x3F, // 1.0f32, little-endian
            ]
        );
    }

    #[test]
    fn full_size_record_fits_the_firmware_buffer() {
        let original = GroupMember {
            id: "i".repeat(MAX_ID_LEN),
            name: "n".repeat(MAX_NAME_LEN),
            role: Role::User,
            model_version: "v".repeat(MAX_MODEL_VERSION_LEN),
            face_embedding: vec![0.25; EMBEDDING_DIM],
        };
        let record = encode(&original).unwrap();
        assert_eq!(record.len(), max_encoded_len(EMBEDDING_DIM));
        assert_eq!(decode(&record).unwrap(), original);
    }

    #[test]
    fn encode_rejects_empty_and_oversized_fields() {
        for (broken, field) in [
            (
                GroupMember {
                    id: String::new(),
                    ..member()
                },
                "id",
            ),
            (
                GroupMember {
                    name: "n".repeat(MAX_NAME_LEN + 1),
                    ..member()
                },
                "name",
            ),
            (
                GroupMember {
                    model_version: String::new(),
                    ..member()
                },
                "model version",
            ),
        ] {
            assert!(matches!(
                encode(&broken),
                Err(TemplateError::FieldLength { field: f, .. }) if f == field
            ));
        }
    }

    #[test]
    fn encode_rejects_bad_embeddings() {
        let empty = GroupMember {
            face_embedding: vec![],
            ..member()
        };
        assert_eq!(encode(&empty), Err(TemplateError::EmbeddingDimension(0)));

        let nan = GroupMember {
            face_embedding: vec![0.0, f32::NAN],
            ..member()
        };
        assert_eq!(encode(&nan), Err(TemplateError::NonFiniteEmbedding));
    }

    #[test]
    fn decode_rejects_short_input() {
        assert_eq!(decode(&[]), Err(TemplateError::Truncated { len: 0 }));
        assert_eq!(
            decode(&encode(&member()).unwrap()[..HEADER_LEN - 1]),
            Err(TemplateError::Truncated {
                len: HEADER_LEN - 1
            })
        );
    }

    #[test]
    fn decode_rejects_erased_flash() {
        assert_eq!(decode(&[0xFF; 64]), Err(TemplateError::BadMagic));
    }

    #[test]
    fn decode_rejects_other_format_versions() {
        let mut record = encode(&member()).unwrap();
        record[4] = 2;
        assert_eq!(decode(&record), Err(TemplateError::UnsupportedFormat(2)));
    }

    #[test]
    fn decode_rejects_unknown_role() {
        let mut record = encode(&member()).unwrap();
        record[5] = 9;
        assert_eq!(decode(&record), Err(TemplateError::UnknownRole(9)));
    }

    #[test]
    fn decode_rejects_truncated_and_padded_records() {
        let record = encode(&member()).unwrap();
        assert!(matches!(
            decode(&record[..record.len() - 1]),
            Err(TemplateError::LengthMismatch { .. })
        ));
        let mut padded = record.clone();
        padded.push(0);
        assert_eq!(
            decode(&padded),
            Err(TemplateError::LengthMismatch {
                expected: record.len(),
                found: record.len() + 1
            })
        );
    }

    #[test]
    fn decode_rejects_zero_length_fields_and_dimension() {
        let record = encode(&member()).unwrap();
        for offset in [6, 7, 8] {
            let mut broken = record.clone();
            broken[offset] = 0;
            assert!(matches!(
                decode(&broken),
                Err(TemplateError::FieldLength { len: 0, .. })
            ));
        }
        let mut broken = record.clone();
        broken[10] = 0;
        broken[11] = 0;
        assert_eq!(decode(&broken), Err(TemplateError::EmbeddingDimension(0)));
    }

    #[test]
    fn decode_rejects_oversized_field_length() {
        let mut record = encode(&member()).unwrap();
        record[6] = MAX_ID_LEN as u8 + 1;
        assert!(matches!(
            decode(&record),
            Err(TemplateError::FieldLength { field: "id", .. })
        ));
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        let mut record = encode(&member()).unwrap();
        record[HEADER_LEN] = 0xFF; // first byte of the id
        assert_eq!(decode(&record), Err(TemplateError::InvalidUtf8("id")));
    }

    #[test]
    fn decode_rejects_non_finite_embedding() {
        let mut record = encode(&member()).unwrap();
        let last = record.len() - size_of::<f32>();
        record[last..].copy_from_slice(&f32::INFINITY.to_le_bytes());
        assert_eq!(decode(&record), Err(TemplateError::NonFiniteEmbedding));
    }

    #[test]
    fn reserved_byte_is_ignored() {
        let mut record = encode(&member()).unwrap();
        record[9] = 0xA5;
        assert_eq!(decode(&record).unwrap(), member());
    }
}
