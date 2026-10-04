// C/C++ bindings: esp-idf-sys generates them from components/biometrics_wrapper/include/bindings.h
// (our wrapper API + PPA/heap headers) alongside the regular ESP-IDF bindings.
use esp_idf_svc::sys as ffi;

mod audio_worker;
mod biometrics;
mod camera;
#[cfg(feature = "eval")]
mod eval;
mod models;
mod pipeline;
mod power;
mod ppa;
mod speaker;
mod system;
mod templates;
mod ui;

use crate::biometrics::BiometricSystem;
use anyhow::Result;
use esp_idf_svc::hal::delay::FreeRtos;
use log::info;
use system::SystemResources;

fn main() -> Result<()> {
    // 1. Mandatory ESP-IDF patch linking & logger initialization
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("============================================");
    info!("  ESP32-P4 Biometric Agent Firmware v0.1.0  ");
    info!("============================================");

    // Evaluation build: serve images from the host instead of running the device (see eval.rs).
    #[cfg(feature = "eval")]
    if cfg!(feature = "eval") {
        return eval::run();
    }

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
