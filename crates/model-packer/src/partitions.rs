//! Minimal reader for ESP-IDF partition table CSV files (`firmware/partitions.csv`).

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    pub name: String,
    pub offset: u32,
    pub size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableError {
    NotFound(String),
    MissingField { line: usize, field: &'static str },
    BadNumber { line: usize, value: String },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(label) => write!(f, "no partition labelled `{label}`"),
            Self::MissingField { line, field } => write!(f, "line {line}: missing {field}"),
            Self::BadNumber { line, value } => write!(f, "line {line}: invalid number `{value}`"),
        }
    }
}

impl std::error::Error for TableError {}

/// Finds the partition named `label`. Offsets and sizes may be decimal, `0x` hex, or carry a
/// `K`/`M` suffix, as in ESP-IDF partition tables. Blank offsets are not supported (the
/// firmware's table spells every offset out).
pub fn find(csv: &str, label: &str) -> Result<Partition, TableError> {
    for (index, raw) in csv.lines().enumerate() {
        let line = index + 1;
        let content = raw.split('#').next().unwrap_or("").trim();
        if content.is_empty() {
            continue;
        }
        let fields: Vec<&str> = content.split(',').map(str::trim).collect();
        if fields.first() != Some(&label) {
            continue;
        }
        let field = |i: usize, name: &'static str| {
            fields
                .get(i)
                .filter(|v| !v.is_empty())
                .copied()
                .ok_or(TableError::MissingField { line, field: name })
        };
        return Ok(Partition {
            name: label.to_owned(),
            offset: parse_number(field(3, "offset")?, line)?,
            size: parse_number(field(4, "size")?, line)?,
        });
    }
    Err(TableError::NotFound(label.to_owned()))
}

fn parse_number(value: &str, line: usize) -> Result<u32, TableError> {
    let bad = || TableError::BadNumber {
        line,
        value: value.to_owned(),
    };
    let (digits, multiplier) = match value.as_bytes().last() {
        Some(b'K' | b'k') => (&value[..value.len() - 1], 1024),
        Some(b'M' | b'm') => (&value[..value.len() - 1], 1024 * 1024),
        _ => (value, 1),
    };
    let n = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => digits.parse::<u32>(),
    }
    .map_err(|_| bad())?;
    n.checked_mul(multiplier).ok_or_else(bad)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
# Name,   Type, SubType, Offset,   Size
nvs,      data, nvs,     0x9000,   0x10000
ota_0,    app,  ota_0,   0x20000,  5M
face_feat, data, spiffs, 0xA60000, 0x200000  # trailing comment
storage,  data, spiffs,  0xC60000, 3712K
";

    #[test]
    fn finds_hex_entries_and_ignores_comments() {
        assert_eq!(
            find(TABLE, "face_feat").unwrap(),
            Partition {
                name: "face_feat".into(),
                offset: 0xA60000,
                size: 0x200000
            }
        );
    }

    #[test]
    fn understands_k_and_m_suffixes() {
        assert_eq!(find(TABLE, "ota_0").unwrap().size, 5 * 1024 * 1024);
        assert_eq!(find(TABLE, "storage").unwrap().size, 3712 * 1024);
    }

    #[test]
    fn label_must_match_exactly() {
        assert_eq!(
            find(TABLE, "face"),
            Err(TableError::NotFound("face".into()))
        );
    }

    #[test]
    fn reports_bad_or_missing_numbers() {
        assert!(matches!(
            find("x, data, spiffs, 0xZZ, 0x1000", "x"),
            Err(TableError::BadNumber { .. })
        ));
        assert!(matches!(
            find("x, data, spiffs, , 0x1000", "x"),
            Err(TableError::MissingField { .. })
        ));
        assert!(matches!(
            find("x, data, spiffs, 0x0, 99999999M", "x"),
            Err(TableError::BadNumber { .. })
        ));
    }

    #[test]
    fn reads_the_firmware_partition_table() {
        let csv = include_str!("../../../firmware/partitions.csv");
        use biometric_core::contract::{FEATURE_MODEL, MNP_MODEL, MSR_MODEL};
        // Model partitions are memory-mapped by ESP-DL, which needs 64 KiB alignment.
        for model in [MSR_MODEL, MNP_MODEL, FEATURE_MODEL] {
            let [a, b] = model.partitions.map(|label| find(csv, label).unwrap());
            assert_eq!(a.size, b.size, "{} slots differ in size", model.key);
            for p in [a, b] {
                assert_eq!(
                    p.offset % 0x10000,
                    0,
                    "{} offset not 64 KiB aligned",
                    p.name
                );
                assert_eq!(p.size % 0x10000, 0, "{} size not 64 KiB aligned", p.name);
            }
        }
    }

    #[test]
    fn firmware_partitions_do_not_overlap_and_fit_the_flash() {
        let csv = include_str!("../../../firmware/partitions.csv");
        let mut all: Vec<Partition> = csv
            .lines()
            .filter_map(|line| line.split('#').next()?.split(',').next().map(str::trim))
            .filter(|name| !name.is_empty())
            .map(|name| find(csv, name).unwrap())
            .collect();
        all.sort_by_key(|p| p.offset);
        for pair in all.windows(2) {
            assert!(
                pair[0].offset + pair[0].size <= pair[1].offset,
                "{} overlaps {}",
                pair[0].name,
                pair[1].name
            );
        }
        let last = all.last().unwrap();
        assert!(last.offset + last.size <= 16 * 1024 * 1024, "past 16 MB");
    }

    #[test]
    fn evaluation_table_differs_only_in_the_second_firmware_slot() {
        // The evaluation firmware reads the models from the regular slots, so every other
        // partition must sit exactly where the regular table puts it.
        fn entries(csv: &str) -> Vec<&str> {
            csv.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .collect()
        }
        let regular = entries(include_str!("../../../firmware/partitions.csv"));
        let evaluation = entries(include_str!("../../../firmware/partitions-eval.csv"));
        assert_eq!(regular.len(), evaluation.len());
        let changed: Vec<(&str, &str)> = regular
            .into_iter()
            .zip(evaluation)
            .filter(|(regular, evaluation)| regular != evaluation)
            .collect();
        let [(regular, evaluation)] = changed[..] else {
            panic!("expected exactly one changed entry, got {changed:?}");
        };
        assert!(regular.starts_with("ota_1,"), "{regular}");
        let csv = include_str!("../../../firmware/partitions-eval.csv");
        let candidate = find(csv, "eval_feat").unwrap();
        let slot = find(include_str!("../../../firmware/partitions.csv"), "ota_1").unwrap();
        assert_eq!(
            (candidate.offset, candidate.size),
            (slot.offset, slot.size),
            "{evaluation}"
        );
        // ESP-DL memory-maps model partitions: 64 KiB aligned.
        assert_eq!(candidate.offset % 0x10000, 0);
    }
}
