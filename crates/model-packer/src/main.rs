//! Packages an ESP-DL model into a flash partition image with a manifest
//! (see `biometric_core::manifest`), so the firmware can verify it before loading.
//!
//! ```text
//! model-packer --input human_face_feat_mfn_s8_v1.espdl --model human_face_feat_mfn_s8_v1 \
//!     --version "human_face_recognition 0.3.2" --label face_feat \
//!     --partitions ../../firmware/partitions.csv --out face_feat.bin
//! ```
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
                    --label <partition> --partitions <partitions.csv> --out <image.bin>";

#[derive(Debug)]
struct Args {
    input: PathBuf,
    model: String,
    version: String,
    label: String,
    partitions: PathBuf,
    out: PathBuf,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut input, mut model, mut version, mut label, mut table, mut out) =
        (None, None, None, None, None, None);
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--input" => &mut input,
            "--model" => &mut model,
            "--version" => &mut version,
            "--label" => &mut label,
            "--partitions" => &mut table,
            "--out" => &mut out,
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
    })
}

fn run(args: Args) -> Result<(), String> {
    let csv = std::fs::read_to_string(&args.partitions)
        .map_err(|e| format!("{}: {e}", args.partitions.display()))?;
    let partition = partitions::find(&csv, &args.label).map_err(|e| e.to_string())?;
    let model = std::fs::read(&args.input).map_err(|e| format!("{}: {e}", args.input.display()))?;

    let image = manifest::build_image(&model, &args.model, &args.version, partition.size as usize)
        .map_err(|e| format!("{}: {e}", args.input.display()))?;
    // Self-check with the same verification the firmware performs.
    let written = manifest::verify(&image, &args.model).map_err(|e| e.to_string())?;
    std::fs::write(&args.out, &image).map_err(|e| format!("{}: {e}", args.out.display()))?;

    let capacity = partition.size as usize - manifest::MANIFEST_SECTOR_SIZE;
    println!(
        "{}: {} ({}), {} of {} bytes ({:.0}%), sha256 {}",
        args.out.display(),
        written.model,
        written.version,
        written.size,
        capacity,
        f64::from(written.size) * 100.0 / capacity as f64,
        written.sha256
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
        })
        .unwrap();

        let image = std::fs::read(&out).unwrap();
        assert_eq!(image.len(), 0x20000);
        assert_eq!(manifest::verify(&image, "test_model").unwrap().size, 3000);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
