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

/// Model partitions (one model each); must match `firmware/partitions.csv` and
/// `face_inference.cpp`.
pub const MSR_PARTITION: &str = "face_msr";
pub const MNP_PARTITION: &str = "face_mnp";
pub const FEATURE_PARTITION: &str = "face_feat";

/// Model identifiers the firmware accepts in each partition's manifest
/// (see [`crate::manifest`]); the manifest's `version` identifies the specific release.
/// They are the `.espdl` file stems in Espressif's model components.
pub const MSR_MODEL_ID: &str = "human_face_detect_msr_s8_v1";
pub const MNP_MODEL_ID: &str = "human_face_detect_mnp_s8_v1";
pub const FEATURE_MODEL_ID: &str = "human_face_feat_mfn_s8_v1";
