//! Ed25519 signatures: who is allowed to publish what the device loads.
//!
//! The publisher signs with a secret key that never leaves the signing machine; the firmware
//! is built with the matching public keys and accepts only what one of them signed. A hash
//! alone (as in the model manifest) shows that data is intact, not who made it: whoever can
//! write a model can also write its hash.
//!
//! Signing and verification are the same code on the host and on the board.

use core::fmt;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

pub const KEY_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;

/// An Ed25519 public key.
pub type PublicKey = [u8; KEY_LEN];
/// An Ed25519 secret key (the 32-byte seed).
pub type SecretKey = [u8; KEY_LEN];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    /// No key is trusted, so nothing can be accepted.
    NoTrustedKeys,
    /// No trusted key produced this signature for this message.
    Invalid,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoTrustedKeys => "no trusted keys",
            Self::Invalid => "signature is not from a trusted key",
        })
    }
}

impl std::error::Error for SignatureError {}

/// The public key that belongs to `secret`.
pub fn public_key(secret: &SecretKey) -> PublicKey {
    SigningKey::from_bytes(secret).verifying_key().to_bytes()
}

/// What is actually signed: the context, a NUL, then the message. The context names the kind
/// of thing being signed, so a signature made for one purpose is not valid for another.
fn framed(context: &[u8], message: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(context.len() + 1 + message.len());
    framed.extend_from_slice(context);
    framed.push(0);
    framed.extend_from_slice(message);
    framed
}

/// Signs `message` for the purpose `context` (which must not contain a NUL byte).
pub fn sign(secret: &SecretKey, context: &[u8], message: &[u8]) -> [u8; SIGNATURE_LEN] {
    SigningKey::from_bytes(secret)
        .sign(&framed(context, message))
        .to_bytes()
}

/// Checks `signature` over `message` for `context`; returns the index of the trusted key that
/// made it.
pub fn verify(
    trusted: &[PublicKey],
    context: &[u8],
    message: &[u8],
    signature: &[u8; SIGNATURE_LEN],
) -> Result<usize, SignatureError> {
    if trusted.is_empty() {
        return Err(SignatureError::NoTrustedKeys);
    }
    let framed = framed(context, message);
    let signature = Signature::from_bytes(signature);
    trusted
        .iter()
        // A trusted entry that is not a valid key matches nothing.
        .position(|key| {
            VerifyingKey::from_bytes(key)
                .is_ok_and(|key| key.verify_strict(&framed, &signature).is_ok())
        })
        .ok_or(SignatureError::Invalid)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyFileError {
    /// The line (1-based) is not 64 hex characters.
    BadKey { line: usize },
    /// The line (1-based) is hex, but not a valid Ed25519 public key.
    NotAPublicKey { line: usize },
    /// A secret key file must hold exactly one key.
    ExpectedOneKey { found: usize },
}

impl fmt::Display for KeyFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadKey { line } => write!(f, "line {line}: a key is 64 hex characters"),
            Self::NotAPublicKey { line } => write!(f, "line {line}: not a valid public key"),
            Self::ExpectedOneKey { found } => write!(f, "expected exactly one key, found {found}"),
        }
    }
}

impl std::error::Error for KeyFileError {}

/// Keys in a key file: one 64-character hex key per line; blank lines and `#` comments are
/// ignored. Yields the 1-based line number with each key.
fn parse_lines(text: &str) -> Result<Vec<(usize, [u8; KEY_LEN])>, KeyFileError> {
    text.lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line.split('#').next().unwrap_or("").trim()))
        .filter(|(_, key)| !key.is_empty())
        .map(|(line, key)| {
            crate::manifest::decode_hex_digest(key)
                .map(|key| (line, key))
                .map_err(|_| KeyFileError::BadKey { line })
        })
        .collect()
}

/// Parses a file of trusted public keys (see `firmware/trusted-model-keys.txt`).
pub fn parse_public_keys(text: &str) -> Result<Vec<PublicKey>, KeyFileError> {
    parse_lines(text)?
        .into_iter()
        .map(|(line, key)| {
            VerifyingKey::from_bytes(&key)
                .map(|_| key)
                .map_err(|_| KeyFileError::NotAPublicKey { line })
        })
        .collect()
}

/// Parses a secret key file: exactly one key.
pub fn parse_secret_key(text: &str) -> Result<SecretKey, KeyFileError> {
    match parse_lines(text)?[..] {
        [(_, key)] => Ok(key),
        ref keys => Err(KeyFileError::ExpectedOneKey { found: keys.len() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::encode_hex;

    const SECRET: SecretKey = [7; KEY_LEN];
    const OTHER_SECRET: SecretKey = [8; KEY_LEN];
    const CONTEXT: &[u8] = b"test context";

    /// 32 bytes that are not a point on the curve (about half of all values are not).
    fn not_a_public_key() -> [u8; KEY_LEN] {
        (0u8..=255)
            .map(|first| {
                let mut key = [0; KEY_LEN];
                key[0] = first;
                key
            })
            .find(|key| VerifyingKey::from_bytes(key).is_err())
            .expect("some small value is off the curve")
    }

    #[test]
    fn matches_the_rfc_8032_test_vector() {
        // RFC 8032, section 7.1, test 1 (empty message). `sign` frames the message, so this
        // checks the key derivation here and the raw primitive through dalek directly.
        let secret = crate::manifest::decode_hex_digest(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        )
        .unwrap();
        assert_eq!(
            encode_hex(&public_key(&secret)),
            "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        );
        let signature = SigningKey::from_bytes(&secret).sign(b"").to_bytes();
        assert_eq!(
            encode_hex(&signature),
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
             5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
    }

    #[test]
    fn a_signature_verifies_with_its_public_key() {
        let signature = sign(&SECRET, CONTEXT, b"message");
        let trusted = [public_key(&OTHER_SECRET), public_key(&SECRET)];
        assert_eq!(verify(&trusted, CONTEXT, b"message", &signature), Ok(1));
    }

    #[test]
    fn a_changed_message_context_or_signature_is_rejected() {
        let trusted = [public_key(&SECRET)];
        let signature = sign(&SECRET, CONTEXT, b"message");
        assert_eq!(
            verify(&trusted, CONTEXT, b"messagf", &signature),
            Err(SignatureError::Invalid)
        );
        assert_eq!(
            verify(&trusted, b"other context", b"message", &signature),
            Err(SignatureError::Invalid)
        );
        for index in [0, 31, 32, 63] {
            let mut bad = signature;
            bad[index] ^= 1;
            assert_eq!(
                verify(&trusted, CONTEXT, b"message", &bad),
                Err(SignatureError::Invalid),
                "{index}"
            );
        }
    }

    #[test]
    fn context_and_message_cannot_be_shifted_into_each_other() {
        let trusted = [public_key(&SECRET)];
        let signature = sign(&SECRET, b"ab", b"c");
        assert_eq!(
            verify(&trusted, b"a", b"bc", &signature),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn an_untrusted_signer_is_rejected() {
        let signature = sign(&OTHER_SECRET, CONTEXT, b"message");
        assert_eq!(
            verify(&[public_key(&SECRET)], CONTEXT, b"message", &signature),
            Err(SignatureError::Invalid)
        );
    }

    #[test]
    fn nothing_verifies_without_trusted_keys() {
        let signature = sign(&SECRET, CONTEXT, b"message");
        assert_eq!(
            verify(&[], CONTEXT, b"message", &signature),
            Err(SignatureError::NoTrustedKeys)
        );
    }

    #[test]
    fn an_invalid_trusted_entry_matches_nothing() {
        let invalid = not_a_public_key();
        let signature = sign(&SECRET, CONTEXT, b"message");
        assert_eq!(
            verify(&[invalid], CONTEXT, b"message", &signature),
            Err(SignatureError::Invalid)
        );
        assert_eq!(
            verify(
                &[invalid, public_key(&SECRET)],
                CONTEXT,
                b"message",
                &signature
            ),
            Ok(1)
        );
    }

    #[test]
    fn key_files_allow_comments_and_blank_lines() {
        let (a, b) = (public_key(&SECRET), public_key(&OTHER_SECRET));
        let text = format!(
            "# trusted keys\n\n{}  # first\n  {}\n",
            encode_hex(&a),
            encode_hex(&b)
        );
        assert_eq!(parse_public_keys(&text), Ok(vec![a, b]));
        assert_eq!(parse_public_keys("# nothing\n"), Ok(vec![]));
    }

    #[test]
    fn key_files_report_the_bad_line() {
        assert_eq!(
            parse_public_keys("# c\nabcd\n"),
            Err(KeyFileError::BadKey { line: 2 })
        );
        let not_a_key = encode_hex(&not_a_public_key());
        assert_eq!(
            parse_public_keys(&not_a_key),
            Err(KeyFileError::NotAPublicKey { line: 1 })
        );
    }

    #[test]
    fn a_secret_key_file_holds_exactly_one_key() {
        let hex = encode_hex(&SECRET);
        assert_eq!(parse_secret_key(&format!("# secret\n{hex}\n")), Ok(SECRET));
        assert_eq!(
            parse_secret_key(""),
            Err(KeyFileError::ExpectedOneKey { found: 0 })
        );
        assert_eq!(
            parse_secret_key(&format!("{hex}\n{hex}\n")),
            Err(KeyFileError::ExpectedOneKey { found: 2 })
        );
    }

    #[test]
    fn the_firmware_key_file_holds_valid_keys() {
        // The firmware parses this file at run time; a mistake there would reject every model.
        let keys = parse_public_keys(include_str!("../../../firmware/trusted-model-keys.txt"));
        assert!(keys.is_ok_and(|keys| !keys.is_empty()));
    }
}
