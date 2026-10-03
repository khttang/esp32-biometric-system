#ifndef BIOMETRICS_WRAPPER_BINDINGS_H
#define BIOMETRICS_WRAPPER_BINDINGS_H

// Extra headers for esp-idf-sys to generate Rust bindings from (see Cargo.toml
// `bindings_header`). They are merged into `esp_idf_sys`, used as `crate::ffi` in Rust.
#include "biometrics_wrapper.h" // our C/C++ wrapper API
#include "driver/ppa.h"         // Pixel-Processing Accelerator (driven from Rust)
#include "esp_heap_caps.h"      // aligned PSRAM allocation for DMA buffers

#endif // BIOMETRICS_WRAPPER_BINDINGS_H
