//! Enrolled-member templates and face-embedding matching.
//!
//! An embedding is only comparable with embeddings from the same model: a different model (or
//! a retrained release of the same one) maps faces into a different vector space. Every
//! template therefore records the `model_version` that produced it, and matching skips
//! templates from any other version instead of producing meaningless scores.

use serde::{Deserialize, Serialize};

/// Cosine similarity a live embedding must reach to count as a match.
///
/// This is the default of Espressif's `HumanFaceRecognizer` for the same model. It has not yet
/// been measured on a test set with this pipeline; the firmware logs the similarity to the
/// closest template so it can be.
pub const MATCH_THRESHOLD: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Role {
    Admin,
    User,
    Guest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupMember {
    /// Unique identifier, e.g. `local-0001` for a member enrolled on the device.
    pub id: String,
    pub name: String,
    pub role: Role,
    /// Release of the feature model that produced `face_embedding` (the model manifest's
    /// `version`).
    pub model_version: String,
    /// L2-normalised face embedding.
    pub face_embedding: Vec<f32>,
}

impl AsRef<GroupMember> for GroupMember {
    fn as_ref(&self) -> &GroupMember {
        self
    }
}

/// Index and similarity of the comparable template closest to `embedding`, whether or not it
/// reaches [`MATCH_THRESHOLD`].
///
/// A template is comparable if it was produced by `model_version` and has the same length as
/// `embedding`; others are skipped rather than compared on a truncated prefix.
pub fn closest<M: AsRef<GroupMember>>(
    embedding: &[f32],
    model_version: &str,
    members: &[M],
) -> Option<(usize, f32)> {
    members
        .iter()
        .map(AsRef::as_ref)
        .enumerate()
        .filter(|(_, m)| {
            m.model_version == model_version && m.face_embedding.len() == embedding.len()
        })
        .map(|(index, m)| (index, cosine_similarity(embedding, &m.face_embedding)))
        .filter(|(_, similarity)| similarity.is_finite())
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// Index and similarity of the enrolled member matching `embedding`, if the closest comparable
/// template reaches [`MATCH_THRESHOLD`].
pub fn best_match<M: AsRef<GroupMember>>(
    embedding: &[f32],
    model_version: &str,
    members: &[M],
) -> Option<(usize, f32)> {
    closest(embedding, model_version, members)
        .filter(|&(_, similarity)| similarity >= MATCH_THRESHOLD)
}

/// Dot product, which equals cosine similarity for L2-normalised vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
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
            model_version: MODEL.into(),
            face_embedding: embedding.to_vec(),
        }
    }

    fn normalized(v: &[f32]) -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        v.iter().map(|x| x / n).collect()
    }

    #[test]
    fn identical_unit_vectors_have_similarity_one() {
        let v = normalized(&[0.3, -0.4, 0.5, 0.1]);
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
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
        stale.model_version = "model 0.9".into();
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
    fn parses_server_template_json() {
        let json = r#"[{"id":"Ada_Lovelace","name":"Ada","role":"ADMIN",
            "model_version":"model 1.0","face_embedding":[0.6,0.8]}]"#;
        let members: Vec<GroupMember> = serde_json::from_str(json).unwrap();
        assert_eq!(members[0].role, Role::Admin);
        assert_eq!(members[0].model_version, MODEL);
        assert_eq!(members[0].face_embedding, vec![0.6, 0.8]);
    }

    #[test]
    fn rejects_server_template_without_model_version() {
        let json = r#"[{"id":"x","name":"x","role":"USER","face_embedding":[1.0]}]"#;
        assert!(serde_json::from_str::<Vec<GroupMember>>(json).is_err());
    }

    #[test]
    fn rejects_unknown_role() {
        let json =
            r#"[{"id":"x","name":"x","role":"ROOT","model_version":"m","face_embedding":[]}]"#;
        assert!(serde_json::from_str::<Vec<GroupMember>>(json).is_err());
    }
}
