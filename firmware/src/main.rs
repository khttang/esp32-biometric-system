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
mod speaker;
mod power;
mod video;
mod biometrics;

use anyhow::Result;
use log::info;
use esp_idf_svc::hal::delay::FreeRtos;
use system::SystemResources;
use crate::biometrics::BiometricSystem;



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

    // 3. Instantiate SystemResources container (InactivityTimer is initialized automatically inside)
    let mut resources = match SystemResources::builder()?.build() {
        Ok(res) => {
            crate::power::reset_boot_crash_counter();
            info!("[Boot] SystemResources container allocated.");
            res
        }
        Err(e) => {
            crate::power::handle_fatal_init_error(e);
        }
    };

    // 4. Instantiate BiometricSystem instance
    let mut biometric_system = BiometricSystem::new();
    info!("[Boot] SystemResources initialized, Hardware drivers & worker threads initialized. Launching state machine loop...");
    loop {
        let admin_pressed = resources.is_admin_pressed();
        let has_network_update = false; // Check OTA/server flags here

        // Advance state machine by one tick
        biometric_system.tick(&mut resources, admin_pressed, has_network_update);

        // Yield CPU 0 and feed FreeRTOS watchdog (~60 FPS)
        FreeRtos::delay_ms(16);
    }
}
