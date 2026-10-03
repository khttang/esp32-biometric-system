//! Model partition image format: an ESP-DL model plus a manifest, written to a flash partition
//! so models can be updated without rebuilding the firmware.
//!
//! ```text
//! offset 0                                   partition_len - 4096        partition_len
//! ├─ packed .espdl model (`size` bytes) ─ 0xFF padding ─┼─ manifest JSON ─ 0xFF padding ─┤
//! ```
//!
//! ESP-DL reads the model from offset 0. The manifest lives in the partition's last 4 KiB
//! sector so it can be rewritten without touching the model, and so an erased (all-0xFF)
//! partition reads as "no manifest" rather than as a corrupt one.
//!
//! The firmware verifies a partition with [`verify`] before handing it to ESP-DL; the host
//! packer builds images with [`build_image`]. Both use this module, so they cannot disagree.

use core::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Size of the trailing sector that holds the manifest (one flash erase sector).
pub const MANIFEST_SECTOR_SIZE: usize = 4096;

/// Manifest layout version understood by this firmware.
pub const MANIFEST_FORMAT: u32 = 1;

/// Value of erased NOR flash.
const ERASED: u8 = 0xFF;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelManifest {
    /// Layout version of this manifest ([`MANIFEST_FORMAT`]).
    pub format: u32,
    /// Model identifier the firmware expects, e.g. `human_face_feat_mfn_s8_v1`.
    pub model: String,
    /// Free-form release identifier, e.g. `human_face_recognition 0.3.2`. Recorded with
    /// enrolled templates so they are only compared against embeddings from the same model.
    pub version: String,
    /// Length of the model data at the start of the partition, in bytes.
    pub size: u32,
    /// Lower-case hex SHA-256 of the model data.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// The partition cannot hold a manifest sector plus any model data.
    PartitionTooSmall {
        partition_len: usize,
    },
    /// The manifest sector is erased: nothing has been written to this partition.
    Missing,
    /// The manifest sector holds data that is not a valid manifest.
    Malformed(String),
    UnsupportedFormat(u32),
    WrongModel {
        expected: String,
        found: String,
    },
    /// The model data would overlap the manifest sector (or is empty).
    SizeOutOfRange {
        size: u64,
        capacity: usize,
    },
    /// The `sha256` field is not 64 hex characters.
    InvalidDigest,
    /// The model data does not match the manifest's SHA-256.
    HashMismatch,
    /// The manifest does not fit in its sector.
    ManifestTooLarge {
        len: usize,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PartitionTooSmall { partition_len } => {
                write!(
                    f,
                    "partition of {partition_len} bytes is too small for a model and manifest"
                )
            }
            Self::Missing => write!(f, "no manifest (partition is erased or was never written)"),
            Self::Malformed(e) => write!(f, "malformed manifest: {e}"),
            Self::UnsupportedFormat(v) => {
                write!(
                    f,
                    "unsupported manifest format {v} (expected {MANIFEST_FORMAT})"
                )
            }
            Self::WrongModel { expected, found } => {
                write!(f, "partition holds model `{found}`, expected `{expected}`")
            }
            Self::SizeOutOfRange { size, capacity } => {
                write!(f, "model size {size} is outside 1..={capacity} bytes")
            }
            Self::InvalidDigest => write!(f, "sha256 must be 64 hex characters"),
            Self::HashMismatch => write!(f, "model data does not match the manifest's SHA-256"),
            Self::ManifestTooLarge { len } => {
                write!(
                    f,
                    "manifest of {len} bytes does not fit in {MANIFEST_SECTOR_SIZE} bytes"
                )
            }
        }
    }
}

impl std::error::Error for ManifestError {}

/// Bytes available for model data in a partition of `partition_len` bytes.
pub fn data_capacity(partition_len: usize) -> Result<usize, ManifestError> {
    partition_len
        .checked_sub(MANIFEST_SECTOR_SIZE)
        .filter(|&capacity| capacity > 0)
        .ok_or(ManifestError::PartitionTooSmall { partition_len })
}

/// Parses the manifest from the last sector of a partition image.
pub fn read_manifest(partition: &[u8]) -> Result<ModelManifest, ManifestError> {
    let capacity = data_capacity(partition.len())?;
    let sector = &partition[capacity..];
    // The JSON ends at the first erased byte (or NUL); everything after it is padding.
    let end = sector
        .iter()
        .position(|&b| b == ERASED || b == 0)
        .unwrap_or(sector.len());
    if end == 0 {
        return Err(ManifestError::Missing);
    }
    let manifest: ModelManifest = serde_json::from_slice(&sector[..end])
        .map_err(|e| ManifestError::Malformed(e.to_string()))?;
    if manifest.format != MANIFEST_FORMAT {
        return Err(ManifestError::UnsupportedFormat(manifest.format));
    }
    Ok(manifest)
}

/// Checks that `partition` holds `expected_model` with intact data; returns its manifest.
pub fn verify(partition: &[u8], expected_model: &str) -> Result<ModelManifest, ManifestError> {
    let manifest = read_manifest(partition)?;
    if manifest.model != expected_model {
        return Err(ManifestError::WrongModel {
            expected: expected_model.to_owned(),
            found: manifest.model,
        });
    }
    let capacity = data_capacity(partition.len())?;
    let size = manifest.size as usize;
    if size == 0 || size > capacity {
        return Err(ManifestError::SizeOutOfRange {
            size: u64::from(manifest.size),
            capacity,
        });
    }
    let expected_digest = decode_hex_digest(&manifest.sha256)?;
    if Sha256::digest(&partition[..size]).as_slice() != expected_digest {
        return Err(ManifestError::HashMismatch);
    }
    Ok(manifest)
}

/// Builds a complete partition image (model + padding + manifest) for `partition_len`.
pub fn build_image(
    model_data: &[u8],
    model: &str,
    version: &str,
    partition_len: usize,
) -> Result<Vec<u8>, ManifestError> {
    let capacity = data_capacity(partition_len)?;
    if model_data.is_empty() || model_data.len() > capacity {
        return Err(ManifestError::SizeOutOfRange {
            size: model_data.len() as u64,
            capacity,
        });
    }
    let manifest = ModelManifest {
        format: MANIFEST_FORMAT,
        model: model.to_owned(),
        version: version.to_owned(),
        size: u32::try_from(model_data.len()).map_err(|_| ManifestError::SizeOutOfRange {
            size: model_data.len() as u64,
            capacity,
        })?,
        sha256: encode_hex(&Sha256::digest(model_data)),
    };
    let json =
        serde_json::to_vec(&manifest).map_err(|e| ManifestError::Malformed(e.to_string()))?;
    if json.len() >= MANIFEST_SECTOR_SIZE {
        return Err(ManifestError::ManifestTooLarge { len: json.len() });
    }

    let mut image = vec![ERASED; partition_len];
    image[..model_data.len()].copy_from_slice(model_data);
    image[capacity..capacity + json.len()].copy_from_slice(&json);
    Ok(image)
}

fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 0x0f)] as char);
    }
    s
}

fn decode_hex_digest(hex: &str) -> Result<[u8; 32], ManifestError> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return Err(ManifestError::InvalidDigest);
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ManifestError::InvalidDigest),
    };
    let mut out = [0u8; 32];
    for (i, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARTITION: usize = 64 * 1024;
    const MODEL: &str = "human_face_feat_mfn_s8_v1";
    const VERSION: &str = "human_face_recognition 0.3.2";

    fn model_data() -> Vec<u8> {
        (0..10_000u32).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn image() -> Vec<u8> {
        build_image(&model_data(), MODEL, VERSION, PARTITION).unwrap()
    }

    #[test]
    fn built_image_verifies_and_round_trips_the_manifest() {
        let img = image();
        assert_eq!(img.len(), PARTITION);
        let m = verify(&img, MODEL).unwrap();
        assert_eq!(m.model, MODEL);
        assert_eq!(m.version, VERSION);
        assert_eq!(m.size as usize, model_data().len());
        assert_eq!(&img[..model_data().len()], model_data().as_slice());
    }

    #[test]
    fn model_data_and_manifest_are_separated_by_erased_padding() {
        let img = image();
        assert!(img[model_data().len()..PARTITION - MANIFEST_SECTOR_SIZE]
            .iter()
            .all(|&b| b == 0xFF));
        assert_eq!(img[PARTITION - MANIFEST_SECTOR_SIZE], b'{');
        assert_eq!(*img.last().unwrap(), 0xFF);
    }

    #[test]
    fn known_sha256_vector() {
        // SHA-256("abc"), FIPS 180-2 test vector.
        assert_eq!(
            encode_hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn erased_partition_reports_missing() {
        assert_eq!(
            verify(&vec![0xFF; PARTITION], MODEL),
            Err(ManifestError::Missing)
        );
    }

    #[test]
    fn single_flipped_model_bit_is_detected() {
        let mut img = image();
        img[1234] ^= 0x01;
        assert_eq!(verify(&img, MODEL), Err(ManifestError::HashMismatch));
    }

    #[test]
    fn wrong_model_is_rejected() {
        let err = verify(&image(), "human_face_detect_msr_s8_v1").unwrap_err();
        assert!(matches!(err, ManifestError::WrongModel { .. }));
    }

    #[test]
    fn corrupted_manifest_is_malformed() {
        let mut img = image();
        img[PARTITION - MANIFEST_SECTOR_SIZE + 2] = b'#';
        assert!(matches!(
            read_manifest(&img),
            Err(ManifestError::Malformed(_))
        ));
    }

    fn with_manifest(json: &str) -> Vec<u8> {
        let mut img = vec![0xFF; PARTITION];
        let at = PARTITION - MANIFEST_SECTOR_SIZE;
        img[at..at + json.len()].copy_from_slice(json.as_bytes());
        img
    }

    #[test]
    fn unsupported_format_is_rejected() {
        let img = with_manifest(&format!(
            r#"{{"format":2,"model":"{MODEL}","version":"v","size":1,"sha256":"{}"}}"#,
            "0".repeat(64)
        ));
        assert_eq!(
            read_manifest(&img),
            Err(ManifestError::UnsupportedFormat(2))
        );
    }

    #[test]
    fn size_overlapping_the_manifest_sector_is_rejected() {
        let img = with_manifest(&format!(
            r#"{{"format":1,"model":"{MODEL}","version":"v","size":{},"sha256":"{}"}}"#,
            PARTITION - MANIFEST_SECTOR_SIZE + 1,
            "0".repeat(64)
        ));
        assert!(matches!(
            verify(&img, MODEL),
            Err(ManifestError::SizeOutOfRange { .. })
        ));
    }

    #[test]
    fn zero_size_and_bad_digest_are_rejected() {
        let zero = with_manifest(&format!(
            r#"{{"format":1,"model":"{MODEL}","version":"v","size":0,"sha256":"{}"}}"#,
            "0".repeat(64)
        ));
        assert!(matches!(
            verify(&zero, MODEL),
            Err(ManifestError::SizeOutOfRange { .. })
        ));
        let bad = with_manifest(&format!(
            r#"{{"format":1,"model":"{MODEL}","version":"v","size":1,"sha256":"{}"}}"#,
            "zz".repeat(32)
        ));
        assert_eq!(verify(&bad, MODEL), Err(ManifestError::InvalidDigest));
    }

    #[test]
    fn unknown_manifest_fields_are_rejected() {
        let img = with_manifest(&format!(
            r#"{{"format":1,"model":"{MODEL}","version":"v","size":1,"sha256":"{}","extra":1}}"#,
            "0".repeat(64)
        ));
        assert!(matches!(
            read_manifest(&img),
            Err(ManifestError::Malformed(_))
        ));
    }

    #[test]
    fn build_rejects_models_that_do_not_fit() {
        let too_big = vec![0u8; PARTITION - MANIFEST_SECTOR_SIZE + 1];
        assert!(matches!(
            build_image(&too_big, MODEL, VERSION, PARTITION),
            Err(ManifestError::SizeOutOfRange { .. })
        ));
        assert!(matches!(
            build_image(&[], MODEL, VERSION, PARTITION),
            Err(ManifestError::SizeOutOfRange { .. })
        ));
        assert_eq!(
            build_image(&[1], MODEL, VERSION, MANIFEST_SECTOR_SIZE),
            Err(ManifestError::PartitionTooSmall {
                partition_len: MANIFEST_SECTOR_SIZE
            })
        );
    }

    #[test]
    fn exactly_full_partition_is_accepted() {
        let data = vec![7u8; PARTITION - MANIFEST_SECTOR_SIZE];
        let img = build_image(&data, MODEL, VERSION, PARTITION).unwrap();
        assert!(verify(&img, MODEL).is_ok());
    }
}
