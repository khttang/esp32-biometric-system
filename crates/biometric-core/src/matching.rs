//! Enrolled-member templates and face-embedding matching.

use serde::{Deserialize, Serialize};

/// Cosine similarity a live embedding must reach to count as a match.
pub const MATCH_THRESHOLD: f32 = 0.75;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Role {
    Admin,
    User,
    Guest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupMember {
    /// "First_Last"
    pub id: String,
    pub name: String,
    pub role: Role,
    /// L2-normalised face embedding.
    pub face_embedding: Vec<f32>,
}

/// Returns the enrolled member most similar to `embedding`, if any reaches MATCH_THRESHOLD.
///
/// Templates whose length differs from `embedding` are skipped rather than compared on a
/// truncated prefix.
pub fn best_match(embedding: &[f32], members: &[GroupMember]) -> Option<GroupMember> {
    members
        .iter()
        .filter(|m| m.face_embedding.len() == embedding.len())
        .map(|m| (m, cosine_similarity(embedding, &m.face_embedding)))
        .filter(|&(_, sim)| sim >= MATCH_THRESHOLD)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(m, _)| m.clone())
}

/// Dot product, which equals cosine similarity for L2-normalised vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str, embedding: &[f32]) -> GroupMember {
        GroupMember {
            id: id.into(),
            name: id.into(),
            role: Role::User,
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
        assert_eq!(best_match(&live, &members).unwrap().id, "closest");
    }

    #[test]
    fn no_match_below_threshold() {
        let live = normalized(&[1.0, 0.0]);
        let members = [member("orthogonal", &[0.0, 1.0])];
        assert_eq!(best_match(&live, &members), None);
    }

    #[test]
    fn empty_template_list_never_matches() {
        assert_eq!(best_match(&[1.0, 0.0], &[]), None);
    }

    #[test]
    fn templates_with_wrong_dimension_are_ignored() {
        // A 128-d template must not be compared against a 512-d live embedding prefix.
        let live = normalized(&[1.0, 0.0, 0.0, 0.0]);
        let members = [member("short", &[1.0, 0.0])];
        assert_eq!(best_match(&live, &members), None);
    }

    #[test]
    fn parses_server_template_json() {
        let json =
            r#"[{"id":"Ada_Lovelace","name":"Ada","role":"ADMIN","face_embedding":[0.6,0.8]}]"#;
        let members: Vec<GroupMember> = serde_json::from_str(json).unwrap();
        assert_eq!(members[0].role, Role::Admin);
        assert_eq!(members[0].face_embedding, vec![0.6, 0.8]);
    }

    #[test]
    fn rejects_unknown_role() {
        let json = r#"[{"id":"x","name":"x","role":"ROOT","face_embedding":[]}]"#;
        assert!(serde_json::from_str::<Vec<GroupMember>>(json).is_err());
    }
}
