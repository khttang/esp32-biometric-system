//! Hardware-independent logic shared by the firmware.
//!
//! Nothing in here touches ESP-IDF, so it builds and tests on the host:
//! `cd crates/biometric-core && cargo test`

pub mod activation;
pub mod contract;
pub mod enrollment;
pub mod eval_protocol;
pub mod evaluation;
pub mod geometry;
pub mod manifest;
pub mod matching;
pub mod stats;
pub mod template;
