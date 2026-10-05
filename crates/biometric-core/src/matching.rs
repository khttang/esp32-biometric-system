//! Enrolled-member templates and face-embedding matching.
//!
//! An embedding is only comparable with embeddings from the same model: a different model (or
//! a retrained release of the same one) maps faces into a different vector space. Every
//! template therefore records the `model_version` that produced it, and matching skips
//! templates from any other version instead of producing meaningless scores.

use core::borrow::Borrow;

/// Cosine similarity a live embedding must reach to count as a match.
///
/// This is the default of Espressif's `HumanFaceRecognizer` for the same model. Measured on
/// the board with LFW, one image against one image, it gave no false accepts and 21.2 % false
/// rejects (see README, "Measuring Accuracy on the Board"). The firmware also logs the similarity to the closest
/// template, so the threshold can be judged against what the device sees.
pub const MATCH_THRESHOLD: f32 = 0.5;

/// How far the squared length of an embedding may be from 1 for it to count as L2-normalised.
const UNIT_NORM_TOLERANCE: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    User,
    Guest,
}

/// What a template was computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Modality {
    Face,
    Voice,
}

/// One embedding of a member, for one modality and one model release.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub modality: Modality,
    /// Release of the model that produced `embedding` (the model manifest's `version`).
    pub model_version: String,
    /// L2-normalised embedding.
    pub embedding: Vec<f32>,
}

/// A person known to the device, with the templates that recognise them.
///
/// A member outlives its templates: after a model change the old template no longer applies,
/// and until a new one exists the member is known but cannot be recognised ("needs
/// enrollment"). A member holds at most one template per modality and model release.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupMember {
    /// Unique across devices, e.g. `80f1b2d2da2e-0001` for a member enrolled on that device.
    pub id: String,
    pub name: String,
    pub role: Role,
    pub templates: Vec<Template>,
}

impl GroupMember {
    /// The member's template for `modality` from model release `model_version`, if any.
    pub fn template(&self, modality: Modality, model_version: &str) -> Option<&Template> {
        self.templates
            .iter()
            .find(|t| t.modality == modality && t.model_version == model_version)
    }

    /// Adds `template`, replacing the one for the same modality and model release.
    pub fn set_template(&mut self, template: Template) {
        self.templates.retain(|t| {
            t.modality != template.modality || t.model_version != template.model_version
        });
        self.templates.push(template);
    }
}

/// Index of the member whose face template is closest to `embedding`, and the similarity,
/// whether or not it reaches [`MATCH_THRESHOLD`].
///
/// Only a face template produced by `model_version` and of the same length as `embedding` is
/// comparable; members without one are skipped rather than compared on something else.
pub fn closest<M: Borrow<GroupMember>>(
    embedding: &[f32],
    model_version: &str,
    members: &[M],
) -> Option<(usize, f32)> {
    members
        .iter()
        .map(Borrow::borrow)
        .enumerate()
        .filter_map(|(index, m): (usize, &GroupMember)| {
            let template = m.template(Modality::Face, model_version)?;
            (template.embedding.len() == embedding.len())
                .then(|| (index, dot(embedding, &template.embedding)))
        })
        .filter(|(_, similarity)| similarity.is_finite())
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// Index and similarity of the enrolled member matching `embedding`, if the closest comparable
/// template reaches [`MATCH_THRESHOLD`].
pub fn best_match<M: Borrow<GroupMember>>(
    embedding: &[f32],
    model_version: &str,
    members: &[M],
) -> Option<(usize, f32)> {
    closest(embedding, model_version, members)
        .filter(|&(_, similarity)| similarity >= MATCH_THRESHOLD)
}

/// Dot product of two vectors of the same length. For L2-normalised vectors it is their
/// cosine similarity.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Whether `v` is a finite, L2-normalised vector, as the feature model emits them.
pub fn is_normalised(v: &[f32]) -> bool {
    let norm_sq = dot(v, v);
    norm_sq.is_finite() && (norm_sq - 1.0).abs() <= UNIT_NORM_TOLERANCE
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    const MODEL: &str = "model 1.0";

    fn member(id: &str, embedding: &[f32]) -> GroupMember {
        GroupMember {
            id: id.into(),
            name: id.into(),
            role: Role::User,
            templates: vec![face(MODEL, embedding)],
        }
    }

    fn face(model_version: &str, embedding: &[f32]) -> Template {
        Template {
            modality: Modality::Face,
            model_version: model_version.into(),
            embedding: embedding.to_vec(),
        }
    }

    fn normalized(v: &[f32]) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    #[test]
    fn identical_unit_vectors_have_similarity_one() {
        let v = normalized(&[0.3, -0.4, 0.5, 0.1]);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn normalised_means_finite_and_of_unit_length() {
        assert!(is_normalised(&normalized(&[0.3, -0.4, 0.5])));
        assert!(is_normalised(&[1.02, 0.0]));
        assert!(!is_normalised(&[2.0, 0.0]));
        assert!(!is_normalised(&[0.0, 0.0]));
        assert!(!is_normalised(&[f32::NAN, 0.0]));
        assert!(!is_normalised(&[f32::INFINITY, 0.0]));
    }

    #[test]
    fn picks_the_most_similar_member_above_threshold() {
        let live = normalized(&[1.0, 0.1, 0.0]);
        let members = [
            member("close", &normalized(&[1.0, 0.3, 0.0])),
            member("closest", &normalized(&[1.0, 0.12, 0.0])),
            member("far", &normalized(&[0.0, 0.0, 1.0])),
        ];
        let (index, similarity) = best_match(&live, MODEL, &members).unwrap();
        assert_eq!(members[index].id, "closest");
        assert!(similarity > 0.99);
    }

    #[test]
    fn no_match_below_threshold() {
        let live = normalized(&[1.0, 0.0]);
        let members = [member("orthogonal", &[0.0, 1.0])];
        assert_eq!(best_match(&live, MODEL, &members), None);
    }

    #[test]
    fn threshold_is_inclusive() {
        let live = [1.0, 0.0];
        let at = [member("at", &[MATCH_THRESHOLD, 0.0])];
        assert_eq!(best_match(&live, MODEL, &at), Some((0, MATCH_THRESHOLD)));
        let below = [member("below", &[MATCH_THRESHOLD - 0.01, 0.0])];
        assert_eq!(best_match(&live, MODEL, &below), None);
    }

    #[test]
    fn closest_reports_similarity_below_the_threshold() {
        let live = normalized(&[1.0, 0.0]);
        let members = [member("far", &[0.0, 1.0]), member("nearer", &[0.3, 0.0])];
        let (index, similarity) = closest(&live, MODEL, &members).unwrap();
        assert_eq!(index, 1);
        assert!((similarity - 0.3).abs() < 1e-6);
    }

    #[test]
    fn empty_template_list_never_matches() {
        assert_eq!(best_match::<GroupMember>(&[1.0, 0.0], MODEL, &[]), None);
    }

    #[test]
    fn templates_with_wrong_dimension_are_ignored() {
        // A 128-d template must not be compared against a 512-d live embedding prefix.
        let live = normalized(&[1.0, 0.0, 0.0, 0.0]);
        let members = [member("short", &[1.0, 0.0])];
        assert_eq!(closest(&live, MODEL, &members), None);
    }

    #[test]
    fn templates_from_another_model_version_never_match() {
        let live = normalized(&[1.0, 0.0]);
        let mut stale = member("stale", &live);
        stale.templates = vec![face("model 0.9", &live)];
        assert_eq!(closest(&live, MODEL, &[stale.clone()]), None);

        // A weaker template from the current model wins over an identical stale one.
        let members = [stale, member("current", &normalized(&[1.0, 0.5]))];
        assert_eq!(best_match(&live, MODEL, &members).unwrap().0, 1);
    }

    #[test]
    fn non_finite_scores_do_not_mask_a_real_match() {
        let live = [1.0, 0.0];
        let members = [member("nan", &[f32::NAN, 0.0]), member("real", &[1.0, 0.0])];
        assert_eq!(best_match(&live, MODEL, &members), Some((1, 1.0)));
    }

    #[test]
    fn works_on_shared_members() {
        let live = normalized(&[1.0, 0.0]);
        let members = [Arc::new(member("shared", &live))];
        assert_eq!(best_match(&live, MODEL, &members).unwrap().0, 0);
    }

    #[test]
    fn member_with_templates_for_two_models_matches_under_each() {
        let (old, new) = (normalized(&[1.0, 0.0]), normalized(&[0.0, 1.0]));
        let mut both = member("both", &old);
        both.set_template(face("model 2.0", &new));
        let members = [both];
        assert_eq!(best_match(&old, MODEL, &members), Some((0, 1.0)));
        assert_eq!(best_match(&new, "model 2.0", &members), Some((0, 1.0)));
        // The old model's embedding means nothing to the new model's template.
        assert_eq!(best_match(&old, "model 2.0", &members), None);
    }

    #[test]
    fn member_without_templates_or_with_only_a_voice_template_never_matches_a_face() {
        let live = normalized(&[1.0, 0.0]);
        let mut nobody = member("needs enrollment", &live);
        nobody.templates.clear();
        assert_eq!(closest(&live, MODEL, &[nobody]), None);

        let mut voice = member("voice only", &live);
        voice.templates[0].modality = Modality::Voice;
        assert_eq!(closest(&live, MODEL, &[voice]), None);
    }

    #[test]
    fn set_template_replaces_only_the_same_modality_and_model() {
        let mut m = member("m", &[1.0, 0.0]);
        m.set_template(face(MODEL, &[0.0, 1.0]));
        assert_eq!(m.templates, [face(MODEL, &[0.0, 1.0])]);
        m.set_template(face("model 2.0", &[0.6, 0.8]));
        let mut voice = face(MODEL, &[0.5, 0.5]);
        voice.modality = Modality::Voice;
        m.set_template(voice.clone());
        assert_eq!(m.templates.len(), 3);
        assert_eq!(m.template(Modality::Voice, MODEL), Some(&voice));
        assert_eq!(m.template(Modality::Voice, "model 2.0"), None);
    }
}
