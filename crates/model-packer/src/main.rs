//! Packages an ESP-DL model into a flash partition image with a manifest
//! (see `biometric_core::manifest`), so the firmware can verify it before loading.
//!
//! ```text
//! model-packer --input human_face_feat_mfn_s8_v1.espdl --model human_face_feat_mfn_s8_v1 \
//!     --version "human_face_recognition 0.3.2" --label face_feat_b \
//!     --partitions ../../firmware/partitions.csv --out face_feat.bin \
//!     --golden <sha256 the firmware logged for this model's golden run> \
//!     --key <secret key file>
//! model-packer keygen --out <secret key file>     # new signing key; prints its public key
//! model-packer pubkey --key <secret key file>     # public key of an existing signing key
//! ```
//!
//! `--key` signs the image. The firmware only loads images signed by a key listed in
//! `firmware/trusted-model-keys.txt`; without `--key` the image is unsigned and is rejected.
//! The secret key file must stay off the device and out of the repository.
//!
//! `--golden` is optional, but the firmware only activates a new image that has one (see
//! `biometric_core::activation`). To obtain it, flash the image without a golden into the
//! standby slot: the firmware runs it, logs the digest, and leaves the active model in place.
//!
//! The partition's size comes from the partition table, so the image always matches the
//! layout the firmware was built with. The tool prints the `espflash write-bin` command that
//! writes the image to the right offset.

mod partitions;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use biometric_core::hex;
use biometric_core::manifest;
use biometric_core::signing::{self, SecretKey};

const USAGE: &str = "\
usage: model-packer --input <model.espdl> --model <id> --version <release>
                    --label <partition> --partitions <partitions.csv> --out <image.bin>
                    [--golden <sha256 hex>] [--key <secret key file>]
       model-packer keygen --out <secret key file>
       model-packer pubkey --key <secret key file>";

#[derive(Debug)]
struct Args {
    input: PathBuf,
    model: String,
    version: String,
    label: String,
    partitions: PathBuf,
    out: PathBuf,
    golden: Option<[u8; 32]>,
    /// Secret key file to sign the image with.
    key: Option<PathBuf>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut input, mut model, mut version, mut label, mut table, mut out, mut golden, mut key) =
        (None, None, None, None, None, None, None, None);
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--input" => &mut input,
            "--model" => &mut model,
            "--version" => &mut version,
            "--label" => &mut label,
            "--partitions" => &mut table,
            "--out" => &mut out,
            "--golden" => &mut golden,
            "--key" => &mut key,
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
            .map(|digest| {
                hex::decode_32(&digest).ok_or("--golden: a digest is 64 hex characters".to_owned())
            })
            .transpose()?,
        key: key.map(PathBuf::from),
    })
}

fn read_secret_key(path: &Path) -> Result<SecretKey, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    signing::parse_secret_key(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Creates a new secret key file; never overwrites one, since the old key would be lost.
fn keygen(out: &Path) -> Result<(), String> {
    let mut secret: SecretKey = [0; signing::KEY_LEN];
    getrandom::fill(&mut secret).map_err(|e| format!("no system randomness: {e}"))?;
    if let Some(dir) = out.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(out)
        .map_err(|e| format!("{}: {e}", out.display()))?;
    let text = format!(
        "# Model signing key (SECRET). Keep it off the device and out of the repository.\n{}\n",
        hex::encode(&secret)
    );
    std::io::Write::write_all(&mut file, text.as_bytes())
        .map_err(|e| format!("{}: {e}", out.display()))?;
    println!("{}", hex::encode(signing::public_key(&secret).as_bytes()));
    Ok(())
}

fn pubkey(key: &Path) -> Result<(), String> {
    println!(
        "{}",
        hex::encode(signing::public_key(&read_secret_key(key)?).as_bytes())
    );
    Ok(())
}

/// `keygen --out <file>` and `pubkey --key <file>`: one flag with one value.
fn single_path(mut args: impl Iterator<Item = String>, flag: &str) -> Result<PathBuf, String> {
    match (args.next(), args.next(), args.next()) {
        (Some(given), Some(value), None) if given == flag => Ok(value.into()),
        _ => Err(USAGE.to_owned()),
    }
}

fn dispatch(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let first = args.next();
    match first.as_deref() {
        Some("keygen") => keygen(&single_path(args, "--out")?),
        Some("pubkey") => pubkey(&single_path(args, "--key")?),
        _ => run(parse_args(first.into_iter().chain(args))?),
    }
}

fn run(args: Args) -> Result<(), String> {
    let csv = std::fs::read_to_string(&args.partitions)
        .map_err(|e| format!("{}: {e}", args.partitions.display()))?;
    let partition = partitions::find(&csv, &args.label).map_err(|e| e.to_string())?;
    let model = std::fs::read(&args.input).map_err(|e| format!("{}: {e}", args.input.display()))?;

    let mut image = manifest::build_image(
        &model,
        &args.model,
        &args.version,
        args.golden.as_ref(),
        partition.size as usize,
    )
    .map_err(|e| format!("{}: {e}", args.input.display()))?;
    // Self-check with the same verification the firmware performs.
    let (written, signer) = match &args.key {
        Some(path) => {
            let secret = read_secret_key(path)?;
            let public = signing::public_key(&secret);
            manifest::sign_image(&mut image, &secret).map_err(|e| e.to_string())?;
            let written = manifest::verify_signed(&image, &args.model, &[public])
                .map_err(|e| e.to_string())?;
            (
                written,
                format!("signed by {}", hex::encode(public.as_bytes())),
            )
        }
        None => {
            let written = manifest::verify(&image, &args.model).map_err(|e| e.to_string())?;
            (written, "UNSIGNED (the firmware will reject it)".to_owned())
        }
    };
    std::fs::write(&args.out, &image).map_err(|e| format!("{}: {e}", args.out.display()))?;

    let capacity = partition.size as usize - manifest::MANIFEST_SECTOR_SIZE;
    println!(
        "{}: {} ({}), {} of {} bytes ({:.0}%), sha256 {}, golden {}, {signer}",
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
    match dispatch(std::env::args().skip(1)) {
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
            key: None,
        })
        .unwrap();

        let image = std::fs::read(&out).unwrap();
        assert_eq!(image.len(), 0x20000);
        let written = manifest::verify(&image, "test_model").unwrap();
        assert_eq!(written.size, 3000);
        assert_eq!(written.golden(), Some([7; 32]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keygen_writes_a_key_that_signs_images_and_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("model-packer-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let key = dir.join("nested").join("signing.key");
        keygen(&key).unwrap();
        let secret = read_secret_key(&key).unwrap();
        assert_ne!(secret, [0; 32]);
        // A second keygen must not destroy the first key.
        assert!(keygen(&key).is_err());
        assert_eq!(read_secret_key(&key).unwrap(), secret);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "key file must not be readable by others");
        }

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
            golden: None,
            key: Some(key),
        })
        .unwrap();

        let image = std::fs::read(&out).unwrap();
        let trusted = [signing::public_key(&secret)];
        assert!(manifest::verify_signed(&image, "test_model", &trusted).is_ok());
        assert!(
            manifest::verify_signed(&image, "test_model", &[signing::public_key(&[9; 32])])
                .is_err()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unsigned_image_fails_the_firmware_check() {
        let dir =
            std::env::temp_dir().join(format!("model-packer-unsigned-{}", std::process::id()));
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
            golden: None,
            key: None,
        })
        .unwrap();
        let image = std::fs::read(&out).unwrap();
        assert_eq!(
            manifest::verify_signed(&image, "test_model", &[signing::public_key(&[9; 32])]),
            Err(manifest::ManifestError::Unsigned)
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn subcommands_take_exactly_one_flag() {
        let list = |items: &[&str]| {
            items
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert_eq!(
            single_path(list(&["--out", "k"]), "--out"),
            Ok(PathBuf::from("k"))
        );
        assert!(single_path(list(&["--key", "k"]), "--out").is_err());
        assert!(single_path(list(&["--out"]), "--out").is_err());
        assert!(single_path(list(&["--out", "k", "extra"]), "--out").is_err());
        assert!(read_secret_key(Path::new("/nonexistent/key")).is_err());
    }
}
