//! Model contract: what the firmware and the face models must agree on.
//!
//! The firmware uses Espressif's pretrained ESP-DL models:
//!
//! | Stage | Model | Input | Output |
//! |---|---|---|---|
//! | Detection | `human_face_detect` MSR+MNP (`*_s8_v1`) | packed RGB888 image of any size (resized internally) | boxes, scores, 5 landmarks |
//! | Embedding | `human_face_recognition` MFN (`human_face_feat_mfn_s8_v1`) | same image + landmarks (aligned internally to 112×112) | L2-normalised `f32` vector |
//!
//! The firmware checks [`EMBEDDING_DIM`] against the loaded model at startup and disables
//! recognition on a mismatch rather than comparing incompatible vectors.

use crate::geometry::PixelFormat;

/// Detector input produced by the camera pipeline: the full sensor frame at half resolution,
/// in PPA RGB888 layout (B, G, R bytes; passed to ESP-DL as BGR888).
pub const DETECTOR_WIDTH: u32 = 640;
pub const DETECTOR_HEIGHT: u32 = 480;
pub const DETECTOR_FORMAT: PixelFormat = PixelFormat::Rgb888;

/// Facial landmarks reported per face (eyes, nose, mouth corners) and used for alignment.
pub const LANDMARK_COUNT: usize = 5;

/// Length of the face embedding produced by the feature model.
pub const EMBEDDING_DIM: usize = 512;

/// One face model: where its two flash slots are and which model they must hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelSpec {
    /// NVS key of the model's activation record (see [`crate::activation`]).
    pub key: &'static str,
    /// Model identifier required in the slot's manifest (see [`crate::manifest`]); the
    /// manifest's `version` identifies the specific release. It is the `.espdl` file stem in
    /// Espressif's model components.
    pub id: &'static str,
    /// Partition labels of slots A and B; must match `firmware/partitions.csv`.
    pub partitions: [&'static str; 2],
}

pub const MSR_MODEL: ModelSpec = ModelSpec {
    key: "msr",
    id: "human_face_detect_msr_s8_v1",
    partitions: ["face_msr_a", "face_msr_b"],
};
pub const MNP_MODEL: ModelSpec = ModelSpec {
    key: "mnp",
    id: "human_face_detect_mnp_s8_v1",
    partitions: ["face_mnp_a", "face_mnp_b"],
};
pub const FEATURE_MODEL: ModelSpec = ModelSpec {
    key: "feat",
    id: "human_face_feat_mfn_s8_v1",
    partitions: ["face_feat_a", "face_feat_b"],
};

/// NVS partition holding the enrolled templates (see [`crate::template`]); must match
/// `firmware/partitions.csv`.
pub const TEMPLATE_PARTITION: &str = "templates";
