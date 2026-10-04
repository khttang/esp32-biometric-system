//! Model partition image format: an ESP-DL model plus a manifest, written to a flash partition
//! so models can be updated without rebuilding the firmware.
//!
//! ```text
//! offset 0                                   partition_len - 4096              partition_len
//! ├─ packed .espdl model (`size` bytes) ─ 0xFF padding ─┼─ manifest JSON ─ 0xFF ─ signature ─┤
//! ```
//!
//! ESP-DL reads the model from offset 0. The manifest lives in the partition's last 4 KiB
//! sector so it can be rewritten without touching the model, and so an erased (all-0xFF)
//! partition reads as "no manifest" rather than as a corrupt one.
//!
//! The manifest's SHA-256 shows that the model data is intact. The signature, in the sector's
//! last 68 bytes (`SIG1` + Ed25519 over the manifest JSON), shows who published it: the
//! manifest covers the data's hash, so the signature covers the whole image.
//!
//! The firmware verifies a partition with [`verify_signed`] before handing it to ESP-DL; the
//! host packer builds images with [`build_image`] and [`sign_image`]. Both use this module, so
//! they cannot disagree.

use core::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::signing::{self, PublicKey, SecretKey, SignatureError, SIGNATURE_LEN};

/// Size of the trailing sector that holds the manifest (one flash erase sector).
pub const MANIFEST_SECTOR_SIZE: usize = 4096;

/// Manifest layout version written by [`build_image`]. Format 2 added `golden_sha256`.
pub const MANIFEST_FORMAT: u32 = 2;
/// Oldest layout version still accepted (a format 1 manifest is a format 2 one without a golden).
pub const MIN_MANIFEST_FORMAT: u32 = 1;

/// Value of erased NOR flash.
const ERASED: u8 = 0xFF;

/// Marks a signature in the last bytes of the manifest sector.
const SIGNATURE_MAGIC: [u8; 4] = *b"SIG1";
/// The signature trailer: magic, then an Ed25519 signature over the manifest JSON.
const SIGNATURE_TRAILER_LEN: usize = SIGNATURE_MAGIC.len() + SIGNATURE_LEN;
/// Longest manifest JSON: it stops short of the trailer and leaves one erased byte to end it.
pub const MANIFEST_MAX_LEN: usize = MANIFEST_SECTOR_SIZE - SIGNATURE_TRAILER_LEN - 1;
/// Purpose string of a manifest signature (see [`crate::signing`]).
const SIGNATURE_CONTEXT: &[u8] = b"esp32-biometric-system model manifest";

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
    /// Lower-case hex SHA-256 the model's outputs must have in the firmware's golden run
    /// (`firmware/src/models.rs`). A new image without one is never activated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub golden_sha256: Option<String>,
}

impl ModelManifest {
    /// Identifies this exact image: a SHA-256 over the manifest, which in turn covers the
    /// model data's hash, the version and the golden. Repackaging the same model data with a
    /// different version or golden therefore counts as a new image.
    pub fn image_id(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        for field in [
            self.model.as_bytes(),
            self.version.as_bytes(),
            self.sha256.as_bytes(),
            self.golden_sha256.as_deref().unwrap_or("").as_bytes(),
        ] {
            // Length-prefixed, so moving bytes between fields changes the id.
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field);
        }
        hasher.finalize().into()
    }

    /// The golden digest, if the manifest has one. [`verify`] has already checked its syntax.
    pub fn golden(&self) -> Option<[u8; 32]> {
        self.golden_sha256
            .as_deref()
            .and_then(|hex| decode_hex_digest(hex).ok())
    }
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
    /// `sha256` or `golden_sha256` is not 64 hex characters.
    InvalidDigest,
    /// The model data does not match the manifest's SHA-256.
    HashMismatch,
    /// The manifest does not fit in its sector.
    ManifestTooLarge {
        len: usize,
    },
    /// The image carries no signature.
    Unsigned,
    /// The image's signature was not made by a trusted key, or the manifest was changed.
    Signature(SignatureError),
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
                    "unsupported manifest format {v} \
                     (expected {MIN_MANIFEST_FORMAT}..={MANIFEST_FORMAT})"
                )
            }
            Self::WrongModel { expected, found } => {
                write!(f, "partition holds model `{found}`, expected `{expected}`")
            }
            Self::SizeOutOfRange { size, capacity } => {
                write!(f, "model size {size} is outside 1..={capacity} bytes")
            }
            Self::InvalidDigest => write!(f, "digests must be 64 hex characters"),
            Self::HashMismatch => write!(f, "model data does not match the manifest's SHA-256"),
            Self::ManifestTooLarge { len } => {
                write!(
                    f,
                    "manifest of {len} bytes does not fit in {MANIFEST_MAX_LEN} bytes"
                )
            }
            Self::Unsigned => write!(f, "image is not signed"),
            Self::Signature(e) => write!(f, "image {e}"),
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
    parse_manifest(manifest_json(partition)?)
}

/// The manifest JSON bytes exactly as stored: what a signature covers.
fn manifest_json(partition: &[u8]) -> Result<&[u8], ManifestError> {
    let capacity = data_capacity(partition.len())?;
    let sector = &partition[capacity..capacity + MANIFEST_MAX_LEN + 1];
    // The JSON ends at the first erased byte (or NUL); everything after it is padding.
    let end = sector
        .iter()
        .position(|&b| b == ERASED || b == 0)
        .unwrap_or(sector.len());
    if end == 0 {
        return Err(ManifestError::Missing);
    }
    Ok(&sector[..end])
}

fn parse_manifest(json: &[u8]) -> Result<ModelManifest, ManifestError> {
    let manifest: ModelManifest =
        serde_json::from_slice(json).map_err(|e| ManifestError::Malformed(e.to_string()))?;
    if !(MIN_MANIFEST_FORMAT..=MANIFEST_FORMAT).contains(&manifest.format) {
        return Err(ManifestError::UnsupportedFormat(manifest.format));
    }
    Ok(manifest)
}

/// The signature trailer's bytes within a partition image.
fn trailer_range(partition_len: usize) -> core::ops::Range<usize> {
    partition_len - SIGNATURE_TRAILER_LEN..partition_len
}

/// The image's signature; `None` if it has none (the trailer is erased).
pub fn signature(partition: &[u8]) -> Result<Option<[u8; SIGNATURE_LEN]>, ManifestError> {
    data_capacity(partition.len())?;
    let trailer = &partition[trailer_range(partition.len())];
    if trailer.iter().all(|&b| b == ERASED) {
        return Ok(None);
    }
    let (magic, signature) = trailer.split_at(SIGNATURE_MAGIC.len());
    if magic != SIGNATURE_MAGIC {
        return Err(ManifestError::Malformed("bad signature trailer".into()));
    }
    Ok(signature.try_into().ok())
}

/// Signs a built image in place with `secret`, replacing any signature it had.
pub fn sign_image(image: &mut [u8], secret: &SecretKey) -> Result<(), ManifestError> {
    let signature = signing::sign(secret, SIGNATURE_CONTEXT, manifest_json(image)?);
    let range = trailer_range(image.len());
    let trailer = &mut image[range];
    trailer[..SIGNATURE_MAGIC.len()].copy_from_slice(&SIGNATURE_MAGIC);
    trailer[SIGNATURE_MAGIC.len()..].copy_from_slice(&signature);
    Ok(())
}

/// Checks that the manifest was signed by one of the `trusted` keys and returns it. The
/// signature is checked over the stored bytes before they are parsed, so an unauthenticated
/// manifest never reaches the JSON parser. The model data is not checked: use
/// [`verify_signed`] before loading a model.
pub fn authenticate(
    partition: &[u8],
    trusted: &[PublicKey],
) -> Result<ModelManifest, ManifestError> {
    let json = manifest_json(partition)?;
    let signature = signature(partition)?.ok_or(ManifestError::Unsigned)?;
    signing::verify(trusted, SIGNATURE_CONTEXT, json, &signature)
        .map_err(ManifestError::Signature)?;
    parse_manifest(json)
}

/// Checks that `partition` holds `expected_model`, signed by one of the `trusted` keys and
/// with intact data; returns its manifest. This is the check the firmware performs.
pub fn verify_signed(
    partition: &[u8],
    expected_model: &str,
    trusted: &[PublicKey],
) -> Result<ModelManifest, ManifestError> {
    authenticate(partition, trusted)?;
    verify(partition, expected_model)
}

/// Checks that `partition` holds `expected_model` with intact data; returns its manifest.
/// This shows integrity only, not who made the image: the firmware uses [`verify_signed`].
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
    if let Some(golden) = &manifest.golden_sha256 {
        decode_hex_digest(golden)?;
    }
    if Sha256::digest(&partition[..size]).as_slice() != expected_digest {
        return Err(ManifestError::HashMismatch);
    }
    Ok(manifest)
}

/// Builds a complete, unsigned partition image (model + padding + manifest) for
/// `partition_len`; [`sign_image`] adds the signature.
pub fn build_image(
    model_data: &[u8],
    model: &str,
    version: &str,
    golden: Option<&[u8; 32]>,
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
        golden_sha256: golden.map(|digest| encode_hex(digest)),
    };
    let json =
        serde_json::to_vec(&manifest).map_err(|e| ManifestError::Malformed(e.to_string()))?;
    if json.len() > MANIFEST_MAX_LEN {
        return Err(ManifestError::ManifestTooLarge { len: json.len() });
    }

    let mut image = vec![ERASED; partition_len];
    image[..model_data.len()].copy_from_slice(model_data);
    image[capacity..capacity + json.len()].copy_from_slice(&json);
    Ok(image)
}

/// Lower-case hex of `bytes`.
pub fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 0x0f)] as char);
    }
    s
}

/// Parses a 64-character hex SHA-256.
pub fn decode_hex_digest(hex: &str) -> Result<[u8; 32], ManifestError> {
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
        build_image(&model_data(), MODEL, VERSION, None, PARTITION).unwrap()
    }

    const GOLDEN: [u8; 32] = [0xAB; 32];

    #[test]
    fn golden_round_trips_and_is_optional() {
        assert_eq!(verify(&image(), MODEL).unwrap().golden(), None);
        let img = build_image(&model_data(), MODEL, VERSION, Some(&GOLDEN), PARTITION).unwrap();
        let m = verify(&img, MODEL).unwrap();
        assert_eq!(m.format, MANIFEST_FORMAT);
        assert_eq!(m.golden_sha256.as_deref(), Some("ab".repeat(32).as_str()));
        assert_eq!(m.golden(), Some(GOLDEN));
    }

    #[test]
    fn format_1_manifest_without_golden_is_still_accepted() {
        let data = model_data();
        let mut img = vec![0xFF; PARTITION];
        img[..data.len()].copy_from_slice(&data);
        let json = format!(
            r#"{{"format":1,"model":"{MODEL}","version":"v","size":{},"sha256":"{}"}}"#,
            data.len(),
            encode_hex(&Sha256::digest(&data))
        );
        let at = PARTITION - MANIFEST_SECTOR_SIZE;
        img[at..at + json.len()].copy_from_slice(json.as_bytes());
        assert_eq!(verify(&img, MODEL).unwrap().golden(), None);
    }

    #[test]
    fn malformed_golden_is_rejected() {
        let mut m = verify(&image(), MODEL).unwrap();
        m.golden_sha256 = Some("abc".into());
        let mut img = image();
        let at = PARTITION - MANIFEST_SECTOR_SIZE;
        let json = serde_json::to_vec(&m).unwrap();
        img[at..].fill(0xFF);
        img[at..at + json.len()].copy_from_slice(&json);
        assert_eq!(verify(&img, MODEL), Err(ManifestError::InvalidDigest));
    }

    #[test]
    fn image_id_changes_with_data_version_and_golden() {
        let base = verify(&image(), MODEL).unwrap();
        assert_eq!(base.image_id(), verify(&image(), MODEL).unwrap().image_id());

        let other_version =
            build_image(&model_data(), MODEL, "another release", None, PARTITION).unwrap();
        let with_golden =
            build_image(&model_data(), MODEL, VERSION, Some(&GOLDEN), PARTITION).unwrap();
        let mut data = model_data();
        data[0] ^= 1;
        let other_data = build_image(&data, MODEL, VERSION, None, PARTITION).unwrap();

        let ids: Vec<_> = [other_version, with_golden, other_data]
            .iter()
            .map(|img| verify(img, MODEL).unwrap().image_id())
            .collect();
        for (i, id) in ids.iter().enumerate() {
            assert_ne!(*id, base.image_id());
            assert!(ids[..i].iter().all(|earlier| earlier != id));
        }
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
            r#"{{"format":3,"model":"{MODEL}","version":"v","size":1,"sha256":"{}"}}"#,
            "0".repeat(64)
        ));
        assert_eq!(
            read_manifest(&img),
            Err(ManifestError::UnsupportedFormat(3))
        );
        let img = with_manifest(&format!(
            r#"{{"format":0,"model":"{MODEL}","version":"v","size":1,"sha256":"{}"}}"#,
            "0".repeat(64)
        ));
        assert_eq!(
            read_manifest(&img),
            Err(ManifestError::UnsupportedFormat(0))
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
            build_image(&too_big, MODEL, VERSION, None, PARTITION),
            Err(ManifestError::SizeOutOfRange { .. })
        ));
        assert!(matches!(
            build_image(&[], MODEL, VERSION, None, PARTITION),
            Err(ManifestError::SizeOutOfRange { .. })
        ));
        assert_eq!(
            build_image(&[1], MODEL, VERSION, None, MANIFEST_SECTOR_SIZE),
            Err(ManifestError::PartitionTooSmall {
                partition_len: MANIFEST_SECTOR_SIZE
            })
        );
    }

    #[test]
    fn exactly_full_partition_is_accepted() {
        let data = vec![7u8; PARTITION - MANIFEST_SECTOR_SIZE];
        let img = build_image(&data, MODEL, VERSION, None, PARTITION).unwrap();
        assert!(verify(&img, MODEL).is_ok());
    }

    const SECRET: SecretKey = [0x11; 32];

    fn trusted() -> [PublicKey; 1] {
        [signing::public_key(&SECRET)]
    }

    fn signed_image() -> Vec<u8> {
        let mut img = image();
        sign_image(&mut img, &SECRET).unwrap();
        img
    }

    #[test]
    fn signed_image_verifies_with_a_trusted_key() {
        let img = signed_image();
        let manifest = verify_signed(&img, MODEL, &trusted()).unwrap();
        assert_eq!(manifest.model, MODEL);
        // Signing changes neither the manifest nor the image's identity.
        assert_eq!(manifest, verify(&image(), MODEL).unwrap());
        assert_eq!(&img[PARTITION - 68..PARTITION - 64], b"SIG1");
        assert_eq!(img[..PARTITION - 68], image()[..PARTITION - 68]);
    }

    #[test]
    fn unsigned_image_is_rejected() {
        assert_eq!(signature(&image()), Ok(None));
        assert_eq!(
            verify_signed(&image(), MODEL, &trusted()),
            Err(ManifestError::Unsigned)
        );
        assert_eq!(
            authenticate(&image(), &trusted()),
            Err(ManifestError::Unsigned)
        );
    }

    #[test]
    fn image_signed_by_another_key_is_rejected() {
        let mut img = image();
        sign_image(&mut img, &[0x22; 32]).unwrap();
        assert_eq!(
            verify_signed(&img, MODEL, &trusted()),
            Err(ManifestError::Signature(SignatureError::Invalid))
        );
        assert_eq!(
            verify_signed(&signed_image(), MODEL, &[]),
            Err(ManifestError::Signature(SignatureError::NoTrustedKeys))
        );
    }

    #[test]
    fn changing_a_signed_manifest_is_detected() {
        // Swap the version for another of the same length: still valid JSON, intact data.
        let mut img = signed_image();
        let at = PARTITION - MANIFEST_SECTOR_SIZE;
        let json = String::from_utf8(manifest_json(&img).unwrap().to_vec()).unwrap();
        let changed = json.replace("0.3.2", "9.9.9");
        assert_ne!(json, changed);
        img[at..at + changed.len()].copy_from_slice(changed.as_bytes());
        assert_eq!(
            verify(&img, MODEL).unwrap().version,
            "human_face_recognition 9.9.9"
        );
        assert_eq!(
            verify_signed(&img, MODEL, &trusted()),
            Err(ManifestError::Signature(SignatureError::Invalid))
        );
    }

    #[test]
    fn changing_signed_model_data_is_detected() {
        let mut img = signed_image();
        img[100] ^= 1;
        assert_eq!(
            verify_signed(&img, MODEL, &trusted()),
            Err(ManifestError::HashMismatch)
        );
        // The manifest itself is still authentic.
        assert!(authenticate(&img, &trusted()).is_ok());
    }

    #[test]
    fn a_signature_cannot_be_moved_to_another_image() {
        let signed = signed_image();
        let mut other =
            build_image(&model_data(), MODEL, "other release", None, PARTITION).unwrap();
        other[PARTITION - 68..].copy_from_slice(&signed[PARTITION - 68..]);
        assert_eq!(
            verify_signed(&other, MODEL, &trusted()),
            Err(ManifestError::Signature(SignatureError::Invalid))
        );
    }

    #[test]
    fn corrupt_signature_trailer_is_rejected() {
        let mut img = signed_image();
        img[PARTITION - 68] = b'X';
        assert!(matches!(
            verify_signed(&img, MODEL, &trusted()),
            Err(ManifestError::Malformed(_))
        ));
        let mut img = signed_image();
        img[PARTITION - 1] ^= 1;
        assert_eq!(
            verify_signed(&img, MODEL, &trusted()),
            Err(ManifestError::Signature(SignatureError::Invalid))
        );
    }

    #[test]
    fn signing_again_replaces_the_signature() {
        let mut img = signed_image();
        sign_image(&mut img, &[0x22; 32]).unwrap();
        assert!(verify_signed(&img, MODEL, &trusted()).is_err());
        sign_image(&mut img, &SECRET).unwrap();
        assert_eq!(img, signed_image());
    }

    #[test]
    fn signed_image_with_the_wrong_model_is_rejected() {
        assert!(matches!(
            verify_signed(&signed_image(), "another_model", &trusted()),
            Err(ManifestError::WrongModel { .. })
        ));
    }

    #[test]
    fn erased_partition_cannot_be_signed() {
        let mut erased = vec![ERASED; PARTITION];
        assert_eq!(
            sign_image(&mut erased, &SECRET),
            Err(ManifestError::Missing)
        );
    }

    #[test]
    fn manifest_never_reaches_into_the_signature_trailer() {
        // A version long enough to push the JSON past the limit is refused when building.
        let long = "v".repeat(MANIFEST_MAX_LEN);
        assert!(matches!(
            build_image(&model_data(), MODEL, &long, None, PARTITION),
            Err(ManifestError::ManifestTooLarge { .. })
        ));
        // The longest JSON that fits still ends in an erased byte before the trailer.
        let base = manifest_json(&image()).unwrap().len() - VERSION.len();
        let fits = "v".repeat(MANIFEST_MAX_LEN - base);
        let mut img = build_image(&model_data(), MODEL, &fits, None, PARTITION).unwrap();
        assert_eq!(manifest_json(&img).unwrap().len(), MANIFEST_MAX_LEN);
        assert_eq!(img[PARTITION - 69], ERASED);
        sign_image(&mut img, &SECRET).unwrap();
        assert_eq!(
            verify_signed(&img, MODEL, &trusted()).unwrap().version,
            fits
        );
    }
}
