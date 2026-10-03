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

/// Identifies the feature model that produced an embedding. Enrolled templates are only
/// comparable with live embeddings from the same model version.
pub const FEATURE_MODEL_VERSION: &str = "human_face_feat_mfn_s8_v1";
