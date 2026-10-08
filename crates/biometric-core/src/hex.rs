//! Lower-case hexadecimal text, as used for digests, keys and device ids.

/// Lower-case hex of `bytes`.
pub fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 0x0f)] as char);
    }
    s
}

/// Parses 64 hex characters (either case) as 32 bytes: a SHA-256 or an Ed25519 key.
pub fn decode_32(hex: &str) -> Option<[u8; 32]> {
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let (pairs, rest) = hex.as_bytes().as_chunks::<2>();
    if pairs.len() != 32 || !rest.is_empty() {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(pairs) {
        *byte = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_lower_case() {
        assert_eq!(encode(&[0x00, 0x9a, 0xff]), "009aff");
        assert_eq!(encode(&[]), "");
    }

    #[test]
    fn round_trips_32_bytes_in_either_case() {
        let bytes: [u8; 32] = core::array::from_fn(|i| (i * 9) as u8);
        let hex = encode(&bytes);
        assert_eq!(decode_32(&hex), Some(bytes));
        assert_eq!(decode_32(&hex.to_uppercase()), Some(bytes));
    }

    #[test]
    fn rejects_wrong_length_and_non_hex() {
        assert_eq!(decode_32(""), None);
        assert_eq!(decode_32(&"0".repeat(63)), None);
        assert_eq!(decode_32(&"0".repeat(66)), None);
        assert_eq!(decode_32(&format!("{}g", "0".repeat(63))), None);
    }
}
