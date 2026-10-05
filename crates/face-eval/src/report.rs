//! Accuracy report from captured embeddings.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use biometric_core::matching::MATCH_THRESHOLD;

use crate::evaluation::Scores;

use crate::results::Row;

/// False accept rates at which the report gives the threshold and the resulting FRR.
const FAR_TARGETS: [f64; 3] = [1e-2, 1e-3, 1e-4];

/// Person labels of the rows that have embeddings, in row order.
pub fn labels(rows: &[Row]) -> Vec<u32> {
    let mut ids = BTreeMap::new();
    rows.iter()
        .filter(|row| row.score.is_some())
        .map(|row| {
            let next = ids.len() as u32;
            *ids.entry(row.person.as_str()).or_insert(next)
        })
        .collect()
}

fn percent(rate: Option<f64>) -> String {
    rate.map_or_else(|| "n/a".to_owned(), |r| format!("{:.2}%", r * 100.0))
}

fn mean(scores: &[f32]) -> String {
    if scores.is_empty() {
        return "n/a".to_owned();
    }
    format!(
        "{:.3}",
        scores.iter().map(|&s| f64::from(s)).sum::<f64>() / scores.len() as f64
    )
}

/// A Markdown report: one column per model.
pub fn render(rows: &[Row], models: &[(String, Scores)]) -> String {
    let detected = rows.iter().filter(|row| row.score.is_some()).count();
    let people = labels(rows)
        .into_iter()
        .max()
        .map_or(0, |max| max as usize + 1);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Images: {} sent, {detected} with a detected face ({} without).",
        rows.len(),
        rows.len() - detected
    );
    let _ = writeln!(out, "People with a detected face: {people}.");
    if let Some((_, scores)) = models.first() {
        let _ = writeln!(
            out,
            "Pairs: {} genuine, {} impostor.",
            scores.genuine().len(),
            scores.impostor().len()
        );
    }
    out.push('\n');

    let mut table: Vec<(String, Vec<String>)> = Vec::new();
    let mut add = |label: String, cell: &dyn Fn(&Scores) -> String| {
        table.push((
            label,
            models.iter().map(|(_, scores)| cell(scores)).collect(),
        ));
    };
    add("Equal error rate".into(), &|s| {
        percent(s.equal_error().map(|(_, rate)| rate))
    });
    add("Threshold at the equal error rate".into(), &|s| {
        s.equal_error()
            .map_or_else(|| "n/a".to_owned(), |(t, _)| format!("{t:.3}"))
    });
    add(
        format!("False accepts at threshold {MATCH_THRESHOLD}"),
        &|s| percent(s.far(MATCH_THRESHOLD)),
    );
    add(
        format!("False rejects at threshold {MATCH_THRESHOLD}"),
        &|s| percent(s.frr(MATCH_THRESHOLD)),
    );
    for target in FAR_TARGETS {
        add(
            format!("Threshold for {}% false accepts", target * 100.0),
            &|s| {
                s.threshold_for_far(target)
                    .map_or_else(|| "n/a".to_owned(), |t| format!("{t:.3}"))
            },
        );
        add(
            format!("False rejects at that threshold ({}%)", target * 100.0),
            &|s| percent(s.threshold_for_far(target).and_then(|t| s.frr(t))),
        );
    }
    add("Mean genuine score".into(), &|s| mean(s.genuine()));
    add("Mean impostor score".into(), &|s| mean(s.impostor()));
    add("Highest impostor score".into(), &|s| {
        s.impostor()
            .last()
            .map_or_else(|| "n/a".to_owned(), |t| format!("{t:.3}"))
    });

    let _ = write!(out, "| Metric |");
    for (name, _) in models {
        let _ = write!(out, " {name} |");
    }
    let _ = write!(out, "\n|---|");
    for _ in models {
        out.push_str("---|");
    }
    out.push('\n');
    for (label, cells) in table {
        let _ = writeln!(out, "| {label} | {} |", cells.join(" | "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(person: &str, score: Option<f32>) -> Row {
        Row {
            person: person.into(),
            image: "x.jpg".into(),
            score,
        }
    }

    #[test]
    fn labels_skip_rows_without_a_face() {
        let rows = [
            row("b", Some(0.9)),
            row("a", None),
            row("a", Some(0.8)),
            row("b", Some(0.7)),
        ];
        assert_eq!(labels(&rows), [0, 1, 0]);
    }

    #[test]
    fn report_has_a_column_per_model() {
        let rows = [
            row("a", Some(0.9)),
            row("a", Some(0.9)),
            row("b", Some(0.9)),
            row("c", None),
        ];
        let labels = labels(&rows);
        let good = Scores::from_pairs(&[vec![1.0, 0.0], vec![1.0, 0.0], vec![0.0, 1.0]], &labels);
        let bad = Scores::from_pairs(&[vec![1.0, 0.0], vec![0.0, 1.0], vec![1.0, 0.0]], &labels);
        let report = render(&rows, &[("good".into(), good), ("bad".into(), bad)]);
        assert!(
            report.contains("Images: 4 sent, 3 with a detected face (1 without)."),
            "{report}"
        );
        assert!(
            report.contains("People with a detected face: 2."),
            "{report}"
        );
        assert!(report.contains("Pairs: 1 genuine, 2 impostor."), "{report}");
        assert!(report.contains("| Metric | good | bad |"), "{report}");
        assert!(
            report.contains("| Equal error rate | 0.00% | 75.00% |"),
            "{report}"
        );
        assert!(
            report.contains("| False accepts at threshold 0.5 | 0.00% | 50.00% |"),
            "{report}"
        );
        assert!(
            report.contains("| False rejects at threshold 0.5 | 0.00% | 100.00% |"),
            "{report}"
        );
    }

    #[test]
    fn report_without_pairs_says_so() {
        let report = render(&[], &[("m".into(), Scores::default())]);
        assert!(report.contains("| Equal error rate | n/a |"), "{report}");
    }
}
