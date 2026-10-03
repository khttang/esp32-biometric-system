//! Hardware-independent logic shared by the firmware.
//!
//! Nothing in here touches ESP-IDF, so it builds and tests on the host:
//! `cd crates/biometric-core && cargo test`

pub mod contract;
pub mod geometry;
pub mod matching;
pub mod stats;
