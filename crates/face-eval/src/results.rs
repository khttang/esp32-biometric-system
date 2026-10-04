//! Captured embeddings on disk, written as they arrive so an interrupted run can resume.
//!
//! `index.tsv` has one line per image: person, image file name, status (`ok`, `noface`) and
//! the detector's score. `model<i>.f32` holds one little-endian `f32` row per `ok` line, in
//! the same order, for feature model `i`. `models.txt` names the models.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub person: String,
    pub image: String,
    /// The detector's score; `None` if no face was found.
    pub score: Option<f32>,
}

impl Row {
    fn to_line(&self) -> String {
        match self.score {
            Some(score) => format!("{}\t{}\tok\t{score}", self.person, self.image),
            None => format!("{}\t{}\tnoface\t", self.person, self.image),
        }
    }

    fn from_line(line: &str) -> Option<Self> {
        let mut fields = line.split('\t');
        let (person, image, status) = (fields.next()?, fields.next()?, fields.next()?);
        let score = match status {
            "ok" => Some(fields.next()?.parse().ok()?),
            "noface" => None,
            _ => return None,
        };
        Some(Self {
            person: person.to_owned(),
            image: image.to_owned(),
            score,
        })
    }
}

fn model_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("model{index}.f32"))
}

pub fn read_index(dir: &Path) -> io::Result<Vec<Row>> {
    let file = match File::open(dir.join("index.tsv")) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    BufReader::new(file)
        .lines()
        .map(|line| {
            let line = line?;
            Row::from_line(&line).ok_or_else(|| invalid(format!("bad index line: {line}")))
        })
        .collect()
}

pub fn read_model_names(dir: &Path) -> io::Result<Vec<String>> {
    Ok(fs::read_to_string(dir.join("models.txt"))?
        .lines()
        .map(str::to_owned)
        .collect())
}

/// The embeddings of model `index`, one per `ok` row of the index.
pub fn read_embeddings(dir: &Path, index: usize, rows: usize) -> io::Result<Vec<Vec<f32>>> {
    let bytes = fs::read(model_path(dir, index))?;
    if rows == 0 {
        return Ok(Vec::new());
    }
    let row_bytes = bytes.len() / rows;
    if row_bytes == 0 || !row_bytes.is_multiple_of(4) || row_bytes * rows != bytes.len() {
        return Err(invalid(format!(
            "model{index}.f32 does not hold {rows} equal rows"
        )));
    }
    Ok(bytes
        .chunks_exact(row_bytes)
        .map(|row| {
            row.as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect()
        })
        .collect())
}

pub struct Writer {
    index: File,
    models: Vec<File>,
}

impl Writer {
    /// Opens `dir` for appending. `names` must match an earlier run's models, if there was one.
    pub fn open(dir: &Path, names: &[String]) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        match read_model_names(dir) {
            Ok(existing) if existing != names => {
                return Err(invalid(format!(
                    "{} holds results of other models ({existing:?}); use another directory",
                    dir.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::write(dir.join("models.txt"), names.join("\n") + "\n")?;
            }
            Err(e) => return Err(e),
        }
        let append = |path: PathBuf| OpenOptions::new().create(true).append(true).open(path);
        Ok(Self {
            index: append(dir.join("index.tsv"))?,
            models: (0..names.len())
                .map(|i| append(model_path(dir, i)))
                .collect::<io::Result<_>>()?,
        })
    }

    /// Appends one image's result; `embeddings` has one entry per model for an `ok` row.
    pub fn append(&mut self, row: &Row, embeddings: &[Vec<f32>]) -> io::Result<()> {
        if row.score.is_some() {
            if embeddings.len() != self.models.len() {
                return Err(invalid("one embedding per model expected".into()));
            }
            // A run killed between these writes and the index line leaves a row the index does
            // not know; `read_embeddings` then reports the mismatch instead of misaligning rows.
            for (file, embedding) in self.models.iter_mut().zip(embeddings) {
                let bytes: Vec<u8> = embedding.iter().flat_map(|v| v.to_le_bytes()).collect();
                file.write_all(&bytes)?;
            }
        }
        writeln!(self.index, "{}", row.to_line())
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("face-eval-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn row(person: &str, score: Option<f32>) -> Row {
        Row {
            person: person.into(),
            image: format!("{person}_0001.jpg"),
            score,
        }
    }

    #[test]
    fn results_round_trip_and_resume() {
        let dir = temp_dir("roundtrip");
        let names = vec!["small".to_owned(), "large".to_owned()];
        {
            let mut writer = Writer::open(&dir, &names).unwrap();
            writer
                .append(&row("a", Some(0.9)), &[vec![1.0, 2.0], vec![3.0, 4.0, 5.0]])
                .unwrap();
            writer.append(&row("b", None), &[]).unwrap();
        }
        // A second run appends.
        {
            let mut writer = Writer::open(&dir, &names).unwrap();
            writer
                .append(
                    &row("c", Some(0.5)),
                    &[vec![6.0, 7.0], vec![8.0, 9.0, 10.0]],
                )
                .unwrap();
        }
        let rows = read_index(&dir).unwrap();
        assert_eq!(
            rows,
            [row("a", Some(0.9)), row("b", None), row("c", Some(0.5))]
        );
        assert_eq!(read_model_names(&dir).unwrap(), names);
        let detected = rows.iter().filter(|r| r.score.is_some()).count();
        assert_eq!(
            read_embeddings(&dir, 0, detected).unwrap(),
            [[1.0, 2.0], [6.0, 7.0]]
        );
        assert_eq!(
            read_embeddings(&dir, 1, detected).unwrap(),
            [[3.0, 4.0, 5.0], [8.0, 9.0, 10.0]]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_directory_of_other_models_is_refused() {
        let dir = temp_dir("othermodels");
        Writer::open(&dir, &["one".to_owned()]).unwrap();
        assert!(Writer::open(&dir, &["two".to_owned()]).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_index_is_an_empty_run() {
        assert_eq!(read_index(&temp_dir("missing")).unwrap(), []);
    }

    #[test]
    fn embeddings_that_do_not_match_the_index_are_rejected() {
        let dir = temp_dir("mismatch");
        let mut writer = Writer::open(&dir, &["m".to_owned()]).unwrap();
        writer
            .append(&row("a", Some(0.9)), &[vec![1.0, 2.0, 3.0]])
            .unwrap();
        // The index claims two rows, the file has three floats.
        assert!(read_embeddings(&dir, 0, 2).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_wrong_number_of_embeddings_is_refused() {
        let dir = temp_dir("count");
        let mut writer = Writer::open(&dir, &["m".to_owned(), "n".to_owned()]).unwrap();
        assert!(writer.append(&row("a", Some(0.9)), &[vec![1.0]]).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
