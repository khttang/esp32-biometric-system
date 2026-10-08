//! Wire format of the on-board evaluation harness (firmware feature `eval`, `crates/face-eval`).
//!
//! The host sends an image over the console UART and the board answers with the embedding each
//! loaded feature model computes for the largest face in it, so recognition accuracy can be
//! measured on the real hardware with a public dataset.
//!
//! Request (binary): [`REQUEST_HEADER_LEN`] header bytes, then the pixels.
//!
//! | Offset | Size | Field                                             |
//! |-------:|-----:|---------------------------------------------------|
//! | 0      | 4    | magic `EVQ1`                                      |
//! | 4      | 2    | width, little endian                              |
//! | 6      | 2    | height                                            |
//! | 8      | 4    | pixel bytes = width × height × 3                  |
//! | 12     | 4    | CRC-32 of the pixel bytes                         |
//!
//! Pixels are packed B, G, R (the detector's input layout, see [`crate::contract`]).
//!
//! Response (text, one line), so that it survives next to log output on the same UART:
//! `EVR <body> *<CRC-32 of body, 8 hex digits>` where the body is `ok <detector score> <hex>…`
//! (one hex field of little-endian `f32`s per feature model), `noface`, or `err <reason>`.

use core::fmt::Write as _;

use crate::contract::{DETECTOR_FORMAT, DETECTOR_HEIGHT, DETECTOR_WIDTH};
use crate::geometry::image_len;

pub const REQUEST_MAGIC: [u8; 4] = *b"EVQ1";
pub const REQUEST_HEADER_LEN: usize = 16;
const RESPONSE_PREFIX: &str = "EVR ";
/// Printed by the board at the console's boot baud rate ([`CONSOLE_BAUD`]) once the models are
/// loaded, followed by ` baud=<rate> models=<n>`; both sides then switch to that rate.
pub const READY_PREFIX: &str = "EVAL READY";
/// Printed before the ready line, once per feature model: ` <index> <model id>`.
pub const MODEL_PREFIX: &str = "EVAL MODEL";
/// Baud rate of the console before the ready line.
pub const CONSOLE_BAUD: u32 = 115_200;
/// Baud rate of the image transfer.
pub const TRANSFER_BAUD: u32 = 921_600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    BadMagic,
    /// Zero-sized, or larger than a detector frame.
    BadDimensions,
    /// The length field is not width × height × 3.
    BadLength,
    Malformed,
    ChecksumMismatch,
}

impl core::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::BadMagic => "bad magic",
            Self::BadDimensions => "image dimensions out of range",
            Self::BadLength => "length does not match the dimensions",
            Self::Malformed => "malformed response",
            Self::ChecksumMismatch => "checksum mismatch",
        })
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub width: u16,
    pub height: u16,
    /// CRC-32 of the pixel bytes that follow the header.
    pub crc: u32,
}

impl Request {
    /// Header for `pixels`, a packed `width` × `height` image.
    pub fn for_image(width: u16, height: u16, pixels: &[u8]) -> Result<Self, ProtocolError> {
        let request = Self {
            width,
            height,
            crc: crc32(pixels),
        };
        if request.pixel_bytes()? != pixels.len() {
            return Err(ProtocolError::BadLength);
        }
        Ok(request)
    }

    /// Number of pixel bytes that follow the header.
    pub fn pixel_bytes(&self) -> Result<usize, ProtocolError> {
        let (w, h) = (u32::from(self.width), u32::from(self.height));
        if w == 0 || h == 0 || w > DETECTOR_WIDTH || h > DETECTOR_HEIGHT {
            return Err(ProtocolError::BadDimensions);
        }
        Ok(image_len(w, h, DETECTOR_FORMAT))
    }

    pub fn encode(&self) -> Result<[u8; REQUEST_HEADER_LEN], ProtocolError> {
        let len = self.pixel_bytes()? as u32;
        let mut header = [0; REQUEST_HEADER_LEN];
        header[..4].copy_from_slice(&REQUEST_MAGIC);
        header[4..6].copy_from_slice(&self.width.to_le_bytes());
        header[6..8].copy_from_slice(&self.height.to_le_bytes());
        header[8..12].copy_from_slice(&len.to_le_bytes());
        header[12..].copy_from_slice(&self.crc.to_le_bytes());
        Ok(header)
    }

    pub fn decode(header: &[u8; REQUEST_HEADER_LEN]) -> Result<Self, ProtocolError> {
        if header[..4] != REQUEST_MAGIC {
            return Err(ProtocolError::BadMagic);
        }
        let u16_at = |i: usize| u16::from_le_bytes([header[i], header[i + 1]]);
        let u32_at =
            |i: usize| u32::from_le_bytes([header[i], header[i + 1], header[i + 2], header[i + 3]]);
        let request = Self {
            width: u16_at(4),
            height: u16_at(6),
            crc: u32_at(12),
        };
        if request.pixel_bytes()? != u32_at(8) as usize {
            return Err(ProtocolError::BadLength);
        }
        Ok(request)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// The detector's score for the face, and one embedding per loaded feature model.
    Embeddings {
        score: f32,
        embeddings: Vec<Vec<f32>>,
    },
    NoFace,
    Error(String),
}

/// Appends an `ok` response line (without a newline) to `line`.
pub fn encode_embeddings(line: &mut String, score: f32, embeddings: &[&[f32]]) {
    let start = begin(line);
    let _ = write!(line, "ok {score}");
    for embedding in embeddings {
        line.push(' ');
        for value in *embedding {
            for byte in value.to_le_bytes() {
                let _ = write!(line, "{byte:02x}");
            }
        }
    }
    finish(line, start);
}

/// Appends a `noface` response line to `line`.
pub fn encode_no_face(line: &mut String) {
    let start = begin(line);
    line.push_str("noface");
    finish(line, start);
}

/// Appends an `err` response line to `line`; `reason` must not contain a newline.
pub fn encode_error(line: &mut String, reason: &str) {
    let start = begin(line);
    let _ = write!(line, "err {reason}");
    finish(line, start);
}

fn begin(line: &mut String) -> usize {
    line.push_str(RESPONSE_PREFIX);
    line.len()
}

fn finish(line: &mut String, body_start: usize) {
    let crc = crc32(&line.as_bytes()[body_start..]);
    let _ = write!(line, " *{crc:08x}");
}

/// Parses a response line. `None` if `line` is not a response at all (log output).
pub fn decode_response(line: &str) -> Option<Result<Response, ProtocolError>> {
    let rest = line.trim_end().strip_prefix(RESPONSE_PREFIX)?;
    Some(decode_body(rest))
}

fn decode_body(rest: &str) -> Result<Response, ProtocolError> {
    let (body, crc) = rest.rsplit_once(" *").ok_or(ProtocolError::Malformed)?;
    let crc = u32::from_str_radix(crc, 16).map_err(|_| ProtocolError::Malformed)?;
    if crc32(body.as_bytes()) != crc {
        return Err(ProtocolError::ChecksumMismatch);
    }
    let (kind, fields) = body.split_once(' ').unwrap_or((body, ""));
    match kind {
        "noface" if fields.is_empty() => Ok(Response::NoFace),
        "err" => Ok(Response::Error(fields.to_owned())),
        "ok" => {
            let mut fields = fields.split(' ');
            let score = fields
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or(ProtocolError::Malformed)?;
            let embeddings = fields.map(decode_floats).collect::<Result<Vec<_>, _>>()?;
            if embeddings.is_empty() {
                return Err(ProtocolError::Malformed);
            }
            Ok(Response::Embeddings { score, embeddings })
        }
        _ => Err(ProtocolError::Malformed),
    }
}

fn decode_floats(hex: &str) -> Result<Vec<f32>, ProtocolError> {
    let (values, rest) = hex.as_bytes().as_chunks::<8>();
    if values.is_empty() || !rest.is_empty() {
        return Err(ProtocolError::Malformed);
    }
    values
        .iter()
        .map(|digits| {
            let digits = core::str::from_utf8(digits).map_err(|_| ProtocolError::Malformed)?;
            let bits = u32::from_str_radix(digits, 16).map_err(|_| ProtocolError::Malformed)?;
            // The digits are the bytes in little-endian order, so read as a number they are
            // byte-swapped.
            Ok(f32::from_le_bytes(bits.to_be_bytes()))
        })
        .collect()
}

/// CRC-32 (IEEE 802.3, as used by zlib and Ethernet).
pub fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = i as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
                bit += 1;
            }
            table[i] = crc;
            i += 1;
        }
        table
    };
    !data.iter().fold(!0u32, |crc, &byte| {
        TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn request_round_trips() {
        let pixels = vec![7u8; 4 * 2 * 3];
        let request = Request::for_image(4, 2, &pixels).unwrap();
        let header = request.encode().unwrap();
        assert_eq!(&header[..4], b"EVQ1");
        assert_eq!(header[8..12], 24u32.to_le_bytes());
        assert_eq!(Request::decode(&header), Ok(request));
        assert_eq!(request.pixel_bytes(), Ok(24));
    }

    #[test]
    fn request_rejects_a_wrong_pixel_count() {
        assert_eq!(
            Request::for_image(4, 2, &[0; 23]),
            Err(ProtocolError::BadLength)
        );
    }

    #[test]
    fn request_rejects_bad_headers() {
        let good = Request::for_image(2, 2, &[0; 12])
            .unwrap()
            .encode()
            .unwrap();

        let mut bad = good;
        bad[0] = b'X';
        assert_eq!(Request::decode(&bad), Err(ProtocolError::BadMagic));

        let mut bad = good;
        bad[8] += 1;
        assert_eq!(Request::decode(&bad), Err(ProtocolError::BadLength));

        let mut bad = good;
        bad[4..6].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(Request::decode(&bad), Err(ProtocolError::BadDimensions));
    }

    #[test]
    fn request_is_limited_to_a_detector_frame() {
        let (w, h) = (DETECTOR_WIDTH as u16, DETECTOR_HEIGHT as u16);
        let full = Request {
            width: w,
            height: h,
            crc: 0,
        };
        assert_eq!(full.pixel_bytes(), Ok(w as usize * h as usize * 3));
        let wide = Request {
            width: w + 1,
            height: h,
            crc: 0,
        };
        assert_eq!(wide.pixel_bytes(), Err(ProtocolError::BadDimensions));
        assert_eq!(wide.encode(), Err(ProtocolError::BadDimensions));
        let tall = Request {
            width: w,
            height: h + 1,
            crc: 0,
        };
        assert_eq!(tall.pixel_bytes(), Err(ProtocolError::BadDimensions));
    }

    #[test]
    fn embeddings_round_trip_bit_exactly() {
        let a = [0.25f32, -1.5, 1.0e-7, f32::MIN_POSITIVE];
        let b = [0.6f32, 0.8];
        let mut line = String::new();
        encode_embeddings(&mut line, 0.875, &[&a, &b]);
        assert!(line.starts_with("EVR ok 0.875 0000803e"), "{line}");
        let response = decode_response(&line).unwrap().unwrap();
        assert_eq!(
            response,
            Response::Embeddings {
                score: 0.875,
                embeddings: vec![a.to_vec(), b.to_vec()]
            }
        );
    }

    #[test]
    fn no_face_and_error_round_trip() {
        let mut line = String::new();
        encode_no_face(&mut line);
        assert_eq!(decode_response(&line), Some(Ok(Response::NoFace)));

        let mut line = String::new();
        encode_error(&mut line, "crc mismatch");
        assert_eq!(
            decode_response(&format!("{line}\r\n")),
            Some(Ok(Response::Error("crc mismatch".into())))
        );
    }

    #[test]
    fn log_lines_are_not_responses() {
        assert_eq!(
            decode_response("I (123) p4_face: Face embedder loaded"),
            None
        );
        assert_eq!(decode_response(""), None);
    }

    #[test]
    fn corrupted_responses_are_rejected() {
        let mut line = String::new();
        encode_embeddings(&mut line, 0.5, &[&[1.0f32, 2.0]]);

        let flipped = line.replacen("0000803f", "0000803e", 1);
        assert_eq!(
            decode_response(&flipped),
            Some(Err(ProtocolError::ChecksumMismatch))
        );

        let truncated = &line[..line.len() - 12];
        assert!(matches!(decode_response(truncated), Some(Err(_))));

        assert_eq!(
            decode_response("EVR ok 0.5"),
            Some(Err(ProtocolError::Malformed))
        );
    }

    #[test]
    fn malformed_bodies_with_a_valid_checksum_are_rejected() {
        for body in [
            "ok 0.5",
            "ok x 0000803f",
            "ok 0.5 0000803",
            "ok 0.5 zz00803f",
            "what",
            "noface x",
        ] {
            let line = format!("EVR {body} *{:08x}", crc32(body.as_bytes()));
            assert_eq!(
                decode_response(&line),
                Some(Err(ProtocolError::Malformed)),
                "{body}"
            );
        }
    }
}
