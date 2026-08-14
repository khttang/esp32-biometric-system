// Import auto-generated FFI bindings from build.rs (crucial for making ffi bindings work with Rust)
// Allow C-style type naming from bindgen FFI output
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(dead_code)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}

mod system;
mod audio_worker;
mod hdmi_audio;
mod storage;
mod ota;
mod thingsboard;
mod power;
mod video;
mod biometrics;
mod mic_test;
mod speaker_test;

use anyhow::{Context, Result};
use log::{info, error};
use std::thread;
use std::time::Duration;
use system::SystemResources;
use crate::biometrics::BiometricSystem;

const WIFI_SSID: &str = "YOUR_WIFI_SSID";
const WIFI_PASS: &str = "YOUR_WIFI_PASSWORD";

/*
fn main() -> Result<()>{
    // 1. Mandatory ESP-IDF patch linking & logger initialization
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("============================================");
    info!("  ESP32-P4 Biometric Agent Firmware v0.1.0  ");
    info!("============================================");

    // 2. Mark running app image as valid (prevents automatic OTA rollback)
    system::validate_running_app();
    info!("[Boot] Marked running firmware application as valid.");

    // 5. Instantiate SystemResources container (InactivityTimer is initialized automatically inside)
    let mut resources = match SystemResources::new() {
        Ok(res) => {
            crate::power::reset_boot_crash_counter();
            info!("[Boot] SystemResources container allocated.");
            res
        }
        Err(e) => {
            crate::power::handle_fatal_init_error(e);
        }
    };
    if let Err(e) = resources.init() {
        crate::power::handle_fatal_init_error(e);
    }

    let mut biometric_system = BiometricSystem::new();
    info!("[Boot] SystemResources initialized, Hardware drivers & worker threads initialized. Launching state machine loop...");
    loop {
        let admin_pressed = resources.is_admin_pressed();
        let has_network_update = false; // Check OTA/server flags here

        // Advance state machine by one tick
        biometric_system.tick(&mut resources, admin_pressed, has_network_update);

        // ~60 FPS loop rate
        thread::sleep(Duration::from_millis(16));
    }
}
*/

fn main() -> anyhow::Result<()> {
    // 1. Initialize ESP-IDF system drivers
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("Starting Display & Touch Hardware Test...");

    // 2. Initialize display and touch via C++ wrapper
    unsafe {
        let disp_err = ffi::init_display_with_bsp();
        if disp_err != 0 {
            info!("Display init failed with error code: {}", disp_err);
        } else {
            info!("Display initialized! Drawing test pattern...");
            ffi::p4_display_draw_test_pattern();
        }

        let touch_err = ffi::init_touch_with_bsp();
        if touch_err != 0 {
            info!("Touch init failed with error code: {}", touch_err);
        } else {
            info!("Touch controller initialized successfully!");
        }
    }

    // 3. Poll touch coordinates at 20 Hz
    let mut touch_data = ffi::p4_touch_data_t::default();
    loop {
        unsafe {
            if ffi::p4_touch_read(&mut touch_data) {
                info!(
                    "Touch Detected! X: {}, Y: {}, Points: {}, Strength: {}",
                    touch_data.x, touch_data.y, touch_data.points, touch_data.strength
                );
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn verify_face(live_embedding: &[f32; 128], enrolled_embedding: &[f32; 128], threshold: f32) -> bool {
    let dot_product: f32 = live_embedding.iter().zip(enrolled_embedding.iter()).map(|(a, b)| a * b).sum();
    let norm_a: f32 = live_embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = enrolled_embedding.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return false;
    }

    let similarity = dot_product / (norm_a * norm_b);
    similarity >= threshold
}
