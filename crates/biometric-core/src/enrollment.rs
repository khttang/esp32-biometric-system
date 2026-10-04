//! On-device enrollment: turning a few live embeddings of one face into a stored template.
//!
//! A single frame can be blurred or badly posed, so a template is the normalised mean of
//! [`ENROLL_SAMPLES`] embeddings. Every sample after the first must resemble the ones already
//! collected; that keeps a second person walking into view from being averaged in.

use core::fmt;

use crate::matching::{cosine_similarity, GroupMember, Modality, MATCH_THRESHOLD};
use crate::template::MAX_NAME_LEN;

/// Embeddings averaged into one template.
pub const ENROLL_SAMPLES: u8 = 5;

/// Members the device stores; bounds flash use and the per-frame matching cost.
pub const MAX_MEMBERS: usize = 32;

/// Tolerance on a sample's squared length; the feature model emits L2-normalised vectors.
const UNIT_NORM_TOLERANCE: f32 = 0.05;

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
                "sample similarity {similarity:.2} to earlier samples is below {MATCH_THRESHOLD}"
            ),
        }
    }
}

impl std::error::Error for SampleError {}

/// Accumulates the samples of one enrollment. Holds no heap memory, so the inference thread
/// can keep one on its stack.
#[derive(Debug, Clone)]
pub struct Enrollment<const N: usize> {
    sum: [f32; N],
    collected: u8,
}

impl<const N: usize> Default for Enrollment<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Enrollment<N> {
    pub const fn new() -> Self {
        Self {
            sum: [0.0; N],
            collected: 0,
        }
    }

    /// Samples accepted so far.
    pub fn collected(&self) -> u8 {
        self.collected
    }

    pub fn is_complete(&self) -> bool {
        self.collected >= ENROLL_SAMPLES
    }

    /// Adds one live embedding; returns the number of samples collected so far. A rejected
    /// sample leaves the session unchanged.
    pub fn add(&mut self, sample: &[f32; N]) -> Result<u8, SampleError> {
        if self.is_complete() {
            return Err(SampleError::Complete);
        }
        let norm_sq = cosine_similarity(sample, sample);
        if !norm_sq.is_finite() || (norm_sq - 1.0).abs() > UNIT_NORM_TOLERANCE {
            return Err(SampleError::NotNormalised);
        }
        if self.collected > 0 {
            // `sample` is a unit vector, so dividing by the sum's length gives the cosine.
            let similarity = cosine_similarity(sample, &self.sum) / norm(&self.sum);
            // Written so that a NaN similarity is rejected too.
            if !matches!(similarity.partial_cmp(&MATCH_THRESHOLD), Some(o) if o.is_ge()) {
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
    cosine_similarity(v, v).sqrt()
}

/// Identifier of the `sequence`-th member enrolled on the device `device_id`, e.g.
/// `80f1b2d2da2e-0007`. Unique across devices as long as device ids are (see
/// [`device_id`]), so members enrolled on different devices can be merged by a server.
pub fn member_id(device_id: &str, sequence: u32) -> String {
    format!("{device_id}-{sequence:04}")
}

/// A device's identifier in member ids: its factory MAC address as lower-case hex.
pub fn device_id(mac: &[u8; 6]) -> String {
    mac.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The id a member enrolled under template format v1 (`local-0007`) gets on this device.
/// Ids of any other shape are kept.
pub fn migrated_member_id(old: &str, device_id: &str) -> String {
    match old.strip_prefix("local-") {
        Some(sequence) => format!("{device_id}-{sequence}"),
        None => old.to_owned(),
    }
}

/// Name stored for a member: the entered text without surrounding or control whitespace, cut
/// to what a template can hold, or `Member <sequence>` if nothing usable was entered.
pub fn member_name(entered: &str, sequence: u32) -> String {
    let cleaned: String = entered
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return format!("Member {sequence}");
    }
    let mut end = cleaned.len().min(MAX_NAME_LEN);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    cleaned[..end].trim_end().to_owned()
}

/// The member a new face template for `model_version` should be added to, instead of
/// creating a new member: the one whose name is `entered` (ignoring surrounding whitespace)
/// and who has no face template for that model release yet.
///
/// This is how a member is enrolled again after a model change: their old template cannot
/// identify them to the new model, so the admin names them. A member who already has a
/// template for the model is never chosen, so a name cannot be used to overwrite someone.
pub fn reenrollment_target<M: AsRef<GroupMember>>(
    members: &[M],
    entered: &str,
    model_version: &str,
) -> Option<usize> {
    let entered = entered.trim();
    if entered.is_empty() {
        return None;
    }
    members.iter().map(AsRef::as_ref).position(|member| {
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

    fn unit(v: [f32; 3]) -> [f32; 3] {
        let n = norm(&v);
        v.map(|x| x / n)
    }

    fn complete(samples: &[[f32; 3]]) -> Enrollment<3> {
        let mut session = Enrollment::new();
        for sample in samples {
            session.add(sample).unwrap();
        }
        session
    }

    #[test]
    fn counts_samples_until_complete() {
        let sample = unit([1.0, 0.2, 0.0]);
        let mut session = Enrollment::<3>::new();
        for expected in 1..=ENROLL_SAMPLES {
            assert!(!session.is_complete());
            assert_eq!(session.template(), None);
            assert_eq!(session.add(&sample), Ok(expected));
        }
        assert!(session.is_complete());
        assert_eq!(session.add(&sample), Err(SampleError::Complete));
        assert_eq!(session.collected(), ENROLL_SAMPLES);
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
        assert_eq!(template[2], 0.0);
    }

    #[test]
    fn rejects_a_different_face_and_keeps_the_session() {
        let mut session = Enrollment::<3>::new();
        session.add(&unit([1.0, 0.0, 0.0])).unwrap();
        let other = unit([0.0, 1.0, 0.0]);
        assert_eq!(
            session.add(&other),
            Err(SampleError::Inconsistent { similarity: 0.0 })
        );
        assert_eq!(session.collected(), 1);
        assert_eq!(session.add(&unit([1.0, 0.1, 0.0])), Ok(2));
    }

    #[test]
    fn consistency_is_measured_against_the_mean_of_all_samples() {
        // Drifting 50° per sample stays within the threshold of the previous sample (cos 50° ≈
        // 0.64) but not of the mean of everything collected so far.
        let at = |degrees: f32| {
            let r = degrees.to_radians();
            [r.cos(), r.sin(), 0.0]
        };
        let mut session = Enrollment::<3>::new();
        session.add(&at(0.0)).unwrap();
        session.add(&at(50.0)).unwrap();
        assert!(matches!(
            session.add(&at(100.0)),
            Err(SampleError::Inconsistent { .. })
        ));
    }

    #[test]
    fn rejects_samples_that_are_not_unit_vectors() {
        let mut session = Enrollment::<3>::new();
        for bad in [
            [0.0, 0.0, 0.0],
            [2.0, 0.0, 0.0],
            [f32::NAN, 0.0, 0.0],
            [f32::INFINITY, 0.0, 0.0],
        ] {
            assert_eq!(session.add(&bad), Err(SampleError::NotNormalised));
        }
        assert_eq!(session.collected(), 0);
    }

    #[test]
    fn local_ids_are_zero_padded_and_grow() {
        let device = device_id(&[0x80, 0xf1, 0xb2, 0xd2, 0xda, 0x2e]);
        assert_eq!(device, "80f1b2d2da2e");
        assert_eq!(member_id(&device, 7), "80f1b2d2da2e-0007");
        assert_eq!(member_id(&device, 123_456), "80f1b2d2da2e-123456");
        // A migrated id is the one the same sequence number would get today.
        assert_eq!(
            migrated_member_id("local-0007", &device),
            member_id(&device, 7)
        );
        assert_eq!(migrated_member_id("Ada_Lovelace", &device), "Ada_Lovelace");
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
        assert_eq!(reenrollment_target::<GroupMember>(&[], "Ada", "v1"), None);
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
