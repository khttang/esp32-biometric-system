//! Packages an ESP-DL model into a flash partition image with a manifest
//! (see `biometric_core::manifest`), so the firmware can verify it before loading.
//!
//! ```text
//! model-packer --input human_face_feat_mfn_s8_v1.espdl --model human_face_feat_mfn_s8_v1 \
//!     --version "human_face_recognition 0.3.2" --label face_feat_b \
//!     --partitions ../../firmware/partitions.csv --out face_feat.bin \
//!     --golden <sha256 the firmware logged for this model's golden run>
//! ```
//!
//! `--golden` is optional, but the firmware only activates a new image that has one (see
//! `biometric_core::activation`). To obtain it, flash the image without a golden into the
//! standby slot: the firmware runs it, logs the digest, and leaves the active model in place.
//!
//! The partition's size comes from the partition table, so the image always matches the
//! layout the firmware was built with. The tool prints the `espflash write-bin` command that
//! writes the image to the right offset.

mod partitions;

use std::path::PathBuf;
use std::process::ExitCode;

use biometric_core::manifest;

const USAGE: &str = "\
usage: model-packer --input <model.espdl> --model <id> --version <release>
                    --label <partition> --partitions <partitions.csv> --out <image.bin>
                    [--golden <sha256 hex>]";

#[derive(Debug)]
struct Args {
    input: PathBuf,
    model: String,
    version: String,
    label: String,
    partitions: PathBuf,
    out: PathBuf,
    golden: Option<[u8; 32]>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut input, mut model, mut version, mut label, mut table, mut out, mut golden) =
        (None, None, None, None, None, None, None);
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--input" => &mut input,
            "--model" => &mut model,
            "--version" => &mut version,
            "--label" => &mut label,
            "--partitions" => &mut table,
            "--out" => &mut out,
            "--golden" => &mut golden,
            "-h" | "--help" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        };
        *slot = Some(args.next().ok_or_else(|| format!("{flag} needs a value"))?);
    }
    let need = |v: Option<String>, name: &str| v.ok_or_else(|| format!("missing {name}\n{USAGE}"));
    Ok(Args {
        input: need(input, "--input")?.into(),
        model: need(model, "--model")?,
        version: need(version, "--version")?,
        label: need(label, "--label")?,
        partitions: need(table, "--partitions")?.into(),
        out: need(out, "--out")?.into(),
        golden: golden
            .map(|hex| manifest::decode_hex_digest(&hex).map_err(|e| format!("--golden: {e}")))
            .transpose()?,
    })
}

fn run(args: Args) -> Result<(), String> {
    let csv = std::fs::read_to_string(&args.partitions)
        .map_err(|e| format!("{}: {e}", args.partitions.display()))?;
    let partition = partitions::find(&csv, &args.label).map_err(|e| e.to_string())?;
    let model = std::fs::read(&args.input).map_err(|e| format!("{}: {e}", args.input.display()))?;

    let image = manifest::build_image(
        &model,
        &args.model,
        &args.version,
        args.golden.as_ref(),
        partition.size as usize,
    )
    .map_err(|e| format!("{}: {e}", args.input.display()))?;
    // Self-check with the same verification the firmware performs.
    let written = manifest::verify(&image, &args.model).map_err(|e| e.to_string())?;
    std::fs::write(&args.out, &image).map_err(|e| format!("{}: {e}", args.out.display()))?;

    let capacity = partition.size as usize - manifest::MANIFEST_SECTOR_SIZE;
    println!(
        "{}: {} ({}), {} of {} bytes ({:.0}%), sha256 {}, golden {}",
        args.out.display(),
        written.model,
        written.version,
        written.size,
        capacity,
        f64::from(written.size) * 100.0 / capacity as f64,
        written.sha256,
        written.golden_sha256.as_deref().unwrap_or("none")
    );
    println!(
        "flash with: espflash write-bin {:#x} {}",
        partition.offset,
        args.out.display()
    );
    Ok(())
}

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1)).and_then(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Args, String> {
        parse_args(list.iter().map(|s| s.to_string()))
    }

    #[test]
    fn golden_is_optional_and_validated() {
        let base = [
            "--input",
            "m",
            "--model",
            "id",
            "--version",
            "v",
            "--label",
            "l",
            "--partitions",
            "p",
            "--out",
            "o",
        ];
        assert_eq!(args(&base).unwrap().golden, None);

        let hex = "0f".repeat(32);
        let with = [&base[..], &["--golden", hex.as_str()]].concat();
        assert_eq!(args(&with).unwrap().golden, Some([0x0f; 32]));

        let bad = [&base[..], &["--golden", "xyz"]].concat();
        assert!(args(&bad).unwrap_err().starts_with("--golden:"));
    }

    #[test]
    fn parses_all_flags() {
        let a = args(&[
            "--input",
            "m.espdl",
            "--model",
            "id",
            "--version",
            "v1",
            "--label",
            "face_feat",
            "--partitions",
            "p.csv",
            "--out",
            "o.bin",
        ])
        .unwrap();
        assert_eq!(
            (a.model.as_str(), a.version.as_str(), a.label.as_str()),
            ("id", "v1", "face_feat")
        );
        assert_eq!(a.out, PathBuf::from("o.bin"));
    }

    #[test]
    fn rejects_missing_and_unknown_flags() {
        assert!(args(&["--input", "m.espdl"])
            .unwrap_err()
            .contains("missing --model"));
        assert!(args(&["--bogus", "x"])
            .unwrap_err()
            .contains("unknown argument"));
        assert!(args(&["--input"]).unwrap_err().contains("needs a value"));
    }

    #[test]
    fn end_to_end_image_matches_partition_and_verifies() {
        let dir = std::env::temp_dir().join(format!("model-packer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let table = dir.join("p.csv");
        std::fs::write(&table, "m, data, spiffs, 0x10000, 0x20000\n").unwrap();
        let input = dir.join("m.espdl");
        std::fs::write(&input, vec![0x5Au8; 3000]).unwrap();
        let out = dir.join("m.bin");

        run(Args {
            input,
            model: "test_model".into(),
            version: "1.0".into(),
            label: "m".into(),
            partitions: table,
            out: out.clone(),
            golden: Some([7; 32]),
        })
        .unwrap();

        let image = std::fs::read(&out).unwrap();
        assert_eq!(image.len(), 0x20000);
        let written = manifest::verify(&image, "test_model").unwrap();
        assert_eq!(written.size, 3000);
        assert_eq!(written.golden(), Some([7; 32]));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
