//! On-device enrollment: turning a few live embeddings of one face into a stored template.
//!
//! A single frame can be blurred or badly posed, so a template is the normalised mean of
//! [`ENROLL_SAMPLES`] embeddings. Every sample after the first must resemble the ones already
//! collected; that keeps a second person walking into view from being averaged in.

use core::fmt;

use crate::contract::EMBEDDING_DIM;
use crate::matching::{dot, is_normalised, GroupMember, Modality};
use crate::template::MAX_NAME_LEN;

/// Embeddings averaged into one template.
pub const ENROLL_SAMPLES: u8 = 5;

/// Members the device stores; bounds flash use and the per-frame matching cost.
pub const MAX_MEMBERS: usize = 32;

/// Cosine similarity a sample must reach to the mean of the samples collected before it.
/// Separate from [`crate::matching::MATCH_THRESHOLD`]: that one decides who a live face is,
/// this one whether the samples of an enrollment show the same face.
const CONSISTENCY_THRESHOLD: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SampleError {
    /// The session already holds [`ENROLL_SAMPLES`] samples.
    Complete,
    /// The sample is not a finite, L2-normalised vector.
    NotNormalised,
    /// The sample is too unlike the samples collected so far (cosine similarity to their mean).
    Inconsistent { similarity: f32 },
}

impl fmt::Display for SampleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Complete => write!(f, "enrollment already has {ENROLL_SAMPLES} samples"),
            Self::NotNormalised => write!(f, "sample is not a finite unit vector"),
            Self::Inconsistent { similarity } => write!(
                f,
                "sample similarity {similarity:.2} to earlier samples is below \
                 {CONSISTENCY_THRESHOLD}"
            ),
        }
    }
}

impl std::error::Error for SampleError {}

/// Accumulates the samples of one enrollment. Holds no heap memory, so the inference thread
/// can keep one on its stack.
#[derive(Debug, Clone)]
pub struct Enrollment {
    sum: [f32; EMBEDDING_DIM],
    collected: u8,
}

impl Enrollment {
    pub const fn new() -> Self {
        Self {
            sum: [0.0; EMBEDDING_DIM],
            collected: 0,
        }
    }

    fn is_complete(&self) -> bool {
        self.collected >= ENROLL_SAMPLES
    }

    /// Adds one live embedding; returns the number of samples collected so far. A rejected
    /// sample leaves the session unchanged.
    pub fn add(&mut self, sample: &[f32; EMBEDDING_DIM]) -> Result<u8, SampleError> {
        if self.is_complete() {
            return Err(SampleError::Complete);
        }
        if !is_normalised(sample) {
            return Err(SampleError::NotNormalised);
        }
        if self.collected > 0 {
            // `sample` is a unit vector, so dividing by the sum's length gives the cosine.
            let similarity = dot(sample, &self.sum) / norm(&self.sum);
            if similarity.is_nan() || similarity < CONSISTENCY_THRESHOLD {
                return Err(SampleError::Inconsistent { similarity });
            }
        }
        for (acc, value) in self.sum.iter_mut().zip(sample) {
            *acc += value;
        }
        self.collected += 1;
        Ok(self.collected)
    }

    /// The L2-normalised mean of the samples, once the session is complete.
    pub fn template(&self) -> Option<Vec<f32>> {
        if !self.is_complete() {
            return None;
        }
        let length = norm(&self.sum);
        (length.is_finite() && length > 0.0).then(|| self.sum.iter().map(|v| v / length).collect())
    }
}

fn norm(v: &[f32]) -> f32 {
    dot(v, v).sqrt()
}

/// Identifier of the `sequence`-th member enrolled on the device `device_id`, e.g.
/// `80f1b2d2da2e-0007`. The device id is the factory MAC address as lower-case hex, so ids
/// from different devices never collide.
pub fn member_id(device_id: &str, sequence: u32) -> String {
    format!("{device_id}-{sequence:04}")
}

/// A name as it is stored: without surrounding or control whitespace, and cut to what a member
/// record can hold. Empty if nothing usable was entered.
fn clean_name(entered: &str) -> String {
    let cleaned: String = entered
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    let mut end = cleaned.len().min(MAX_NAME_LEN);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    cleaned[..end].trim_end().to_owned()
}

/// Name stored for a member: the entered text cleaned as it is for every stored name, or
/// `Member <sequence>` if nothing usable was entered.
pub fn member_name(entered: &str, sequence: u32) -> String {
    let name = clean_name(entered);
    if name.is_empty() {
        return format!("Member {sequence}");
    }
    name
}

/// Position in `members` of the member a new face template for `model_version` should be
/// added to, instead of creating a new member: the one whose name is `entered` (cleaned the
/// way names are stored) and who has no face template for that model release yet.
///
/// This is how a member is enrolled again after a model change: their old template cannot
/// identify them to the new model, so the admin names them. A member who already has a
/// template for the model is never chosen, so a name cannot be used to overwrite someone.
/// Names need not be unique; of several members with the same name, the first is chosen.
pub fn reenrollment_target<'a>(
    members: impl IntoIterator<Item = &'a GroupMember>,
    entered: &str,
    model_version: &str,
) -> Option<usize> {
    let entered = clean_name(entered);
    if entered.is_empty() {
        return None;
    }
    members.into_iter().position(|member| {
        member.name == entered && member.template(Modality::Face, model_version).is_none()
    })
}

/// Which of a member's template storage slots a new template goes into. `stored` describes
/// what each slot holds (modality and model release), `None` if it is empty.
///
/// In order: the slot already holding this modality and model release; an empty slot; the
/// slot of another release of the same modality (it is superseded); the first slot.
pub fn template_slot(
    stored: &[Option<(Modality, &str)>],
    modality: Modality,
    model_version: &str,
) -> usize {
    stored
        .iter()
        .position(|slot| *slot == Some((modality, model_version)))
        .or_else(|| stored.iter().position(Option::is_none))
        .or_else(|| {
            stored
                .iter()
                .position(|slot| slot.is_some_and(|(m, _)| m == modality))
        })
        .unwrap_or(0)
}

/// Lowest storage slot below `capacity` not yielded by `used`.
pub fn first_free_slot(used: impl IntoIterator<Item = u8>, capacity: usize) -> Option<u8> {
    let mut occupied = [false; u8::MAX as usize + 1];
    for slot in used {
        occupied[usize::from(slot)] = true;
    }
    (0..=u8::MAX)
        .take(capacity)
        .find(|&slot| !occupied[usize::from(slot)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `v` as the first three values of an embedding; the rest is zero.
    fn wide(v: [f32; 3]) -> [f32; EMBEDDING_DIM] {
        let mut embedding = [0.0; EMBEDDING_DIM];
        embedding[..3].copy_from_slice(&v);
        embedding
    }

    fn unit(v: [f32; 3]) -> [f32; EMBEDDING_DIM] {
        let n = norm(&v);
        wide(v.map(|x| x / n))
    }

    fn complete(samples: &[[f32; EMBEDDING_DIM]]) -> Enrollment {
        let mut session = Enrollment::new();
        for sample in samples {
            session.add(sample).unwrap();
        }
        session
    }

    #[test]
    fn counts_samples_until_complete() {
        let sample = unit([1.0, 0.2, 0.0]);
        let mut session = Enrollment::new();
        for expected in 1..=ENROLL_SAMPLES {
            assert_eq!(session.template(), None);
            assert_eq!(session.add(&sample), Ok(expected));
        }
        assert!(session.template().is_some());
        assert_eq!(session.add(&sample), Err(SampleError::Complete));
    }

    #[test]
    fn template_is_the_normalised_mean() {
        let a = unit([1.0, 0.2, 0.0]);
        let b = unit([1.0, -0.2, 0.0]);
        let template = complete(&[a, b, a, b, unit([1.0, 0.0, 0.0])])
            .template()
            .unwrap();
        assert!((norm(&template) - 1.0).abs() < 1e-6);
        assert!((template[0] - 1.0).abs() < 1e-6);
        assert!(template[1].abs() < 1e-6);
        assert!(template[2..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn rejects_a_different_face_and_keeps_the_session() {
        let mut session = Enrollment::new();
        session.add(&unit([1.0, 0.0, 0.0])).unwrap();
        let other = unit([0.0, 1.0, 0.0]);
        assert_eq!(
            session.add(&other),
            Err(SampleError::Inconsistent { similarity: 0.0 })
        );
        assert_eq!(session.add(&unit([1.0, 0.1, 0.0])), Ok(2));
    }

    #[test]
    fn consistency_is_measured_against_the_mean_of_all_samples() {
        // Drifting 50° per sample stays within the threshold of the previous sample (cos 50° ≈
        // 0.64) but not of the mean of everything collected so far.
        let at = |degrees: f32| {
            let r = degrees.to_radians();
            wide([r.cos(), r.sin(), 0.0])
        };
        let mut session = Enrollment::new();
        session.add(&at(0.0)).unwrap();
        session.add(&at(50.0)).unwrap();
        assert!(matches!(
            session.add(&at(100.0)),
            Err(SampleError::Inconsistent { .. })
        ));
    }

    #[test]
    fn rejects_samples_that_are_not_unit_vectors() {
        let mut session = Enrollment::new();
        for bad in [
            [0.0, 0.0, 0.0],
            [2.0, 0.0, 0.0],
            [f32::NAN, 0.0, 0.0],
            [f32::INFINITY, 0.0, 0.0],
        ] {
            assert_eq!(session.add(&wide(bad)), Err(SampleError::NotNormalised));
        }
        assert_eq!(session.add(&unit([1.0, 0.0, 0.0])), Ok(1));
    }

    #[test]
    fn member_ids_are_zero_padded_and_grow() {
        assert_eq!(member_id("80f1b2d2da2e", 7), "80f1b2d2da2e-0007");
        assert_eq!(member_id("80f1b2d2da2e", 123_456), "80f1b2d2da2e-123456");
    }

    #[test]
    fn member_name_trims_and_falls_back() {
        assert_eq!(member_name("  Ada Lovelace \n", 3), "Ada Lovelace");
        assert_eq!(member_name("", 3), "Member 3");
        assert_eq!(member_name(" \t\n", 4), "Member 4");
        assert_eq!(member_name("a\nb", 1), "a b");
    }

    #[test]
    fn member_name_is_cut_on_a_character_boundary() {
        // 'é' is two bytes; an odd byte limit must not split it.
        let long = "é".repeat(MAX_NAME_LEN);
        let name = member_name(&long, 1);
        assert_eq!(name.len(), MAX_NAME_LEN);
        assert_eq!(name, "é".repeat(MAX_NAME_LEN / 2));

        let odd = format!("a{long}");
        assert_eq!(member_name(&odd, 1).len(), MAX_NAME_LEN - 1);
    }

    #[test]
    fn first_free_slot_fills_gaps_and_respects_capacity() {
        assert_eq!(first_free_slot([], 4), Some(0));
        assert_eq!(first_free_slot([0, 1, 3], 4), Some(2));
        assert_eq!(first_free_slot([3, 1, 0, 2], 4), None);
        assert_eq!(first_free_slot([], 0), None);
        // Slots beyond the capacity (e.g. from an older, larger layout) are not reused.
        assert_eq!(first_free_slot([0, 200], 2), Some(1));
    }

    fn person(name: &str, versions: &[&str]) -> GroupMember {
        GroupMember {
            id: name.to_lowercase(),
            name: name.into(),
            role: crate::matching::Role::User,
            templates: versions
                .iter()
                .map(|version| crate::matching::Template {
                    modality: Modality::Face,
                    model_version: (*version).into(),
                    embedding: vec![1.0],
                })
                .collect(),
        }
    }

    #[test]
    fn a_named_member_without_a_template_for_the_model_is_enrolled_again() {
        let members = [
            person("Ada", &["v1"]),
            person("Bo", &[]),
            person("Cy", &["v2"]),
        ];
        // Ada has no template for v2, Bo has none at all.
        assert_eq!(reenrollment_target(&members, "Ada", "v2"), Some(0));
        assert_eq!(reenrollment_target(&members, "  Bo ", "v2"), Some(1));
    }

    #[test]
    fn a_name_never_overwrites_an_existing_template() {
        let members = [person("Ada", &["v1"]), person("Cy", &["v2"])];
        assert_eq!(reenrollment_target(&members, "Cy", "v2"), None);
        assert_eq!(reenrollment_target(&members, "Ada", "v1"), None);
    }

    #[test]
    fn unknown_or_empty_names_create_a_new_member() {
        let members = [person("Ada", &[])];
        assert_eq!(reenrollment_target(&members, "", "v1"), None);
        assert_eq!(reenrollment_target(&members, "   ", "v1"), None);
        assert_eq!(reenrollment_target(&members, "ada", "v1"), None);
        assert_eq!(reenrollment_target(&members, "Eve", "v1"), None);
        assert_eq!(reenrollment_target(&[], "Ada", "v1"), None);
    }

    #[test]
    fn the_entered_name_is_compared_the_way_names_are_stored() {
        // Stored names are cut to MAX_NAME_LEN and have no control characters.
        let long = "n".repeat(MAX_NAME_LEN + 10);
        let members = [person(&member_name(&long, 1), &[]), person("a b", &[])];
        assert_eq!(reenrollment_target(&members, &long, "v1"), Some(0));
        assert_eq!(reenrollment_target(&members, "a\tb", "v1"), Some(1));
    }

    #[test]
    fn of_members_with_the_same_name_the_first_without_a_template_is_chosen() {
        let members = [
            person("Ada", &["v2"]),
            person("Ada", &[]),
            person("Ada", &[]),
        ];
        assert_eq!(reenrollment_target(&members, "Ada", "v2"), Some(1));
    }

    #[test]
    fn a_voice_template_does_not_count_as_a_face_template() {
        let mut ada = person("Ada", &["v1"]);
        ada.templates[0].modality = Modality::Voice;
        assert_eq!(reenrollment_target(&[ada], "Ada", "v1"), Some(0));
    }

    #[test]
    fn template_slot_prefers_same_then_empty_then_superseded() {
        let (face, voice) = (Modality::Face, Modality::Voice);
        // The same modality and release is replaced in place.
        assert_eq!(
            template_slot(&[Some((face, "v1")), Some((face, "v2"))], face, "v2"),
            1
        );
        // Otherwise an empty slot is used.
        assert_eq!(template_slot(&[Some((face, "v1")), None], face, "v2"), 1);
        assert_eq!(template_slot(&[None, None], face, "v1"), 0);
        // With no empty slot, another release of the same modality is superseded.
        assert_eq!(
            template_slot(&[Some((voice, "a")), Some((face, "v1"))], face, "v2"),
            1
        );
        // Nothing of the same modality: the first slot.
        assert_eq!(
            template_slot(&[Some((voice, "a")), Some((voice, "b"))], face, "v1"),
            0
        );
    }
}
