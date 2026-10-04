//! Choosing images from a dataset laid out as `<root>/<person>/<image>.jpg` (as LFW is).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub person: String,
    pub path: PathBuf,
}

/// Every person's images, both sorted by name, so a selection is the same on every machine.
pub fn scan(root: &Path) -> io::Result<BTreeMap<String, Vec<PathBuf>>> {
    let mut people = BTreeMap::new();
    for entry in fs::read_dir(root)? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        let mut images: Vec<PathBuf> = fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg")
                })
            })
            .collect();
        images.sort();
        if let Some(name) = dir.file_name().and_then(|n| n.to_str()) {
            people.insert(name.to_owned(), images);
        }
    }
    Ok(people)
}

/// Up to `per_person` images of up to `people` persons who have at least two images (one
/// image gives no genuine pair). Persons are taken at even intervals through the sorted
/// names, not from the front, so the selection is not skewed towards one part of the alphabet.
pub fn select(all: &BTreeMap<String, Vec<PathBuf>>, people: usize, per_person: usize) -> Vec<Item> {
    let eligible: Vec<(&String, &Vec<PathBuf>)> =
        all.iter().filter(|(_, images)| images.len() >= 2).collect();
    let count = people.min(eligible.len());
    (0..count)
        .map(|i| eligible[i * eligible.len() / count])
        .flat_map(|(person, images)| {
            images.iter().take(per_person.max(2)).map(|path| Item {
                person: person.clone(),
                path: path.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn people(counts: &[(&str, usize)]) -> BTreeMap<String, Vec<PathBuf>> {
        counts
            .iter()
            .map(|&(name, n)| {
                (
                    name.to_owned(),
                    (1..=n)
                        .map(|i| PathBuf::from(format!("{name}/{name}_{i:04}.jpg")))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn people_with_a_single_image_are_skipped() {
        let all = people(&[("a", 1), ("b", 3), ("c", 1), ("d", 2)]);
        let items = select(&all, 10, 4);
        let names: Vec<&str> = items.iter().map(|i| i.person.as_str()).collect();
        assert_eq!(names, ["b", "b", "b", "d", "d"]);
    }

    #[test]
    fn images_per_person_are_capped_but_never_below_two() {
        let all = people(&[("a", 5)]);
        assert_eq!(select(&all, 1, 3).len(), 3);
        assert_eq!(select(&all, 1, 1).len(), 2);
    }

    #[test]
    fn people_are_spread_over_the_whole_name_range() {
        let all = people(&[("a", 2), ("b", 2), ("c", 2), ("d", 2), ("e", 2), ("f", 2)]);
        let items = select(&all, 3, 2);
        let mut names: Vec<&str> = items.iter().map(|i| i.person.as_str()).collect();
        names.dedup();
        assert_eq!(names, ["a", "c", "e"]);
    }

    #[test]
    fn selection_is_empty_without_eligible_people() {
        assert!(select(&people(&[("a", 1)]), 5, 4).is_empty());
        assert!(select(&people(&[("a", 2)]), 0, 4).is_empty());
    }
}
