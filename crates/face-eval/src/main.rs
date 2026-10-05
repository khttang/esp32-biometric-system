//! Measures face-recognition accuracy on the board.
//!
//!   face-eval capture --port <serial port> --dataset <dir> --out <dir>
//!                     [--people <n>] [--per-person <n>]
//!   face-eval report  --results <dir>
//!
//! `capture` sends images of a dataset laid out as `<dir>/<person>/<image>.jpg` to a board
//! running the firmware's `eval` feature and stores the embeddings it returns; an interrupted
//! capture continues where it stopped. `report` compares every pair of embeddings and prints
//! false accept and false reject rates for each feature model on the board.
//!
//! The usual entry point is `tools/face-eval.sh`.

mod dataset;
mod evaluation;
mod link;
mod report;
mod results;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use crate::evaluation::Scores;
use biometric_core::eval_protocol::Response;

const USAGE: &str = "usage: face-eval capture --port <serial port> --dataset <dir> --out <dir> \
                     [--people <n>] [--per-person <n>]\n       face-eval report --results <dir>";
const DEFAULT_PEOPLE: usize = 400;
const DEFAULT_PER_PERSON: usize = 4;
/// Progress is printed once per this many images.
const PROGRESS_EVERY: usize = 50;

type Outcome<T> = Result<T, Box<dyn Error>>;

/// Values of `--key value` options; rejects unknown keys and missing values.
fn options(args: &[String], known: &[&str]) -> Outcome<Vec<Option<String>>> {
    let mut values = vec![None; known.len()];
    let mut args = args.iter();
    while let Some(key) = args.next() {
        let slot = known
            .iter()
            .position(|k| k == key)
            .ok_or_else(|| format!("unknown option {key}\n{USAGE}"))?;
        values[slot] = Some(
            args.next()
                .ok_or_else(|| format!("{key} needs a value"))?
                .clone(),
        );
    }
    Ok(values)
}

fn required(value: &Option<String>, key: &str) -> Outcome<String> {
    value
        .clone()
        .ok_or_else(|| format!("{key} is required\n{USAGE}").into())
}

fn number(value: &Option<String>, key: &str, default: usize) -> Outcome<usize> {
    match value {
        None => Ok(default),
        Some(text) => text
            .parse()
            .map_err(|_| format!("{key}: `{text}` is not a number").into()),
    }
}

/// Decodes an image file into packed B, G, R pixels (the detector's input layout).
fn load_bgr(path: &Path) -> Outcome<(u16, u16, Vec<u8>)> {
    let image = image::open(path)?.to_rgb8();
    let (width, height) = (
        u16::try_from(image.width())?,
        u16::try_from(image.height())?,
    );
    let mut pixels = image.into_raw();
    for pixel in pixels.as_chunks_mut::<3>().0 {
        pixel.swap(0, 2);
    }
    Ok((width, height, pixels))
}

fn capture(args: &[String]) -> Outcome<()> {
    let values = options(
        args,
        &["--port", "--dataset", "--out", "--people", "--per-person"],
    )?;
    let port = required(&values[0], "--port")?;
    let dataset = PathBuf::from(required(&values[1], "--dataset")?);
    let out = PathBuf::from(required(&values[2], "--out")?);
    let people = number(&values[3], "--people", DEFAULT_PEOPLE)?;
    let per_person = number(&values[4], "--per-person", DEFAULT_PER_PERSON)?;

    let items = dataset::select(&dataset::scan(&dataset)?, people, per_person);
    if items.is_empty() {
        return Err(format!(
            "no person with two or more images under {}",
            dataset.display()
        )
        .into());
    }
    let image_name = |item: &dataset::Item| {
        item.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };

    // Resume: the rows already captured must be the start of this selection.
    let done = results::read_index(&out)?;
    let same = done.len() <= items.len()
        && done
            .iter()
            .zip(&items)
            .all(|(row, item)| row.person == item.person && row.image == image_name(item));
    if !same {
        return Err(format!(
            "{} holds results of a different selection; use another directory",
            out.display()
        )
        .into());
    }

    let mut link = link::Link::open(&port)?;
    println!("Board models: {}", link.models.join(", "));
    let mut writer = results::Writer::open(&out, &link.models)?;
    println!(
        "{} images selected, {} already captured",
        items.len(),
        done.len()
    );

    let started = Instant::now();
    let mut without_face = 0usize;
    for (sent, item) in items[done.len()..].iter().enumerate() {
        let (width, height, pixels) = load_bgr(&item.path)?;
        let (score, embeddings) = match link.embed(width, height, &pixels)? {
            Response::Embeddings { score, embeddings } => (Some(score), embeddings),
            Response::NoFace => {
                without_face += 1;
                (None, Vec::new())
            }
            Response::Error(reason) => return Err(format!("board error: {reason}").into()),
        };
        writer.append(
            &results::Row {
                person: item.person.clone(),
                image: image_name(item),
                score,
            },
            &embeddings,
        )?;
        let sent = sent + 1;
        let left = items.len() - done.len() - sent;
        if sent % PROGRESS_EVERY == 0 || left == 0 {
            let per_image = started.elapsed().as_secs_f64() / sent as f64;
            println!(
                "{}/{} images, {without_face} without a face this run, {per_image:.2} s per image, about {:.0} min left",
                done.len() + sent,
                items.len(),
                per_image * left as f64 / 60.0
            );
        }
    }
    Ok(())
}

fn report(args: &[String]) -> Outcome<()> {
    let values = options(args, &["--results"])?;
    let dir = PathBuf::from(required(&values[0], "--results")?);
    let rows = results::read_index(&dir)?;
    let labels = report::labels(&rows);
    let models = results::read_model_names(&dir)?
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let embeddings = results::read_embeddings(&dir, index, labels.len())?;
            Ok((name, Scores::from_pairs(&embeddings, &labels)))
        })
        .collect::<Outcome<Vec<_>>>()?;
    print!("{}", report::render(&rows, &models));
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((command, rest)) if command == "capture" => capture(rest),
        Some((command, rest)) if command == "report" => report(rest),
        _ => Err(USAGE.into()),
    };
    match result {
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

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_are_matched_by_name() {
        let values = options(
            &args(&["--out", "o", "--port", "p"]),
            &["--port", "--out", "--people"],
        )
        .unwrap();
        assert_eq!(values, [Some("p".to_owned()), Some("o".to_owned()), None]);
    }

    #[test]
    fn unknown_options_and_missing_values_are_errors() {
        assert!(options(&args(&["--nope", "1"]), &["--port"]).is_err());
        assert!(options(&args(&["--port"]), &["--port"]).is_err());
    }

    #[test]
    fn numbers_default_and_validate() {
        assert_eq!(number(&None, "--people", 7).unwrap(), 7);
        assert_eq!(number(&Some("12".into()), "--people", 7).unwrap(), 12);
        assert!(number(&Some("many".into()), "--people", 7).is_err());
        assert!(required(&None, "--port").is_err());
    }
}
