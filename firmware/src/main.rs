// Import auto-generated FFI bindings from build.rs
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
mod touch;
mod storage;
mod ota;
mod thingsboard;
mod power;
mod touch_test;
mod mic_test;
mod speaker_test;

use anyhow::{Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::http::Method;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::timer::EspTaskTimerService;
use esp_idf_sys::{esp_pm_config_t, esp_pm_configure};
use log::{info, error};
use std::thread;
use std::time::Duration;
use system::{SystemState, SystemResources, GroupMember};
use hdmi_audio::HdmiAudioPlayer;


const MIC_I2S_PORT: i32 = 0;         // I2S0 for microphone input
const HDMI_I2S_PORT: i32 = 0;        // I2S1 for HDMI audio output
const MIC_SAMPLE_RATE: u32 = 16_000; // e.g., 16kHz for biometrics/voice
const MIC_BCLK_PIN: i32 = 4;         // Bit Clock GPIO
const MIC_WS_PIN: i32 = 5;           // Word Select / LRCLK GPIO
const MIC_DIN_PIN: i32 = 6;          // Data In GPIO

const WIFI_SSID: &str = "YOUR_WIFI_SSID";
const WIFI_PASS: &str = "YOUR_WIFI_PASSWORD";

pub const FACE_EMBEDDING_DIM: usize = 512;

fn main() -> anyhow::Result<()>{
    // 1. Mandatory ESP-IDF patch linking & logger initialization
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    info!("============================================");
    info!("  ESP32-P4 Biometric Gateway Firmware v0.1.0");
    info!("============================================");

    // 2. Mark running app image as valid (prevents automatic OTA rollback)
    system::validate_running_app();
    info!("[Boot] Marked running firmware application as valid.");

    // 3. Take low-level ESP-IDF system services and hardware peripherals
    let nvs = EspDefaultNvsPartition::take()
        .context("Failed to take default NVS partition")?;
    let event_loop = EspSystemEventLoop::take()
        .context("Failed to take system event loop")?;
    let timer_service = EspTaskTimerService::new()
        .context("Failed to create task timer service")?;
    let peripherals = Peripherals::take()
        .context("Failed to take ESP32-P4 peripherals")?;

    // 4. Initialize HDMI Audio Player
    let hdmi_player = HdmiAudioPlayer::new(0);

    // Bundle touch pins
    let pins = system::HardwarePins {
        i2c0: peripherals.i2c0,
        sda: peripherals.pins.gpio7,
        scl: peripherals.pins.gpio8,
        int_pin: peripherals.pins.gpio0,
    };

    // 5. Instantiate SystemResources container (InactivityTimer is initialized automatically inside)
    let mut resources = SystemResources::new(nvs, event_loop, timer_service, hdmi_player, pins);
    info!("[Boot] SystemResources container allocated & Hardware drivers & worker threads initialized. Launching state machine loop...");

    // 8. Main Application State Loop
    let mut current_state = SystemState::Initialize;

    loop {
        match resources.process_state_machine(current_state) {
            Ok(next_state) => {
                current_state = next_state;
            }
            Err(err) => {
                error!("[Main Loop] Unhandled state machine error: {:?}", err);
                current_state = SystemState::Error(format!("{:?}", err));
            }
        }

        // Throttle state machine execution thread
        thread::sleep(Duration::from_millis(50));
    }
}

/// Fetches JSON biometric templates from laptop HTTP endpoint and parses into PSRAM GroupMember structs
pub async fn fetch_templates_from_laptop(url: &str) -> Result<Vec<GroupMember>> {
    let url_string = url.to_string();

    tokio::task::spawn_blocking(move || {
        log::info!("[HTTP] Connecting to server: {}", url_string);

        // 1. Create HTTP connection with default config
        let config = HttpConfig {
            use_global_ca_store: false,
            ..Default::default()
        };
        let mut connection = EspHttpConnection::new(&config)
            .context("Failed to create EspHttpConnection")?;

        // 2. Send GET request
        connection
            .initiate_request(Method::Get, &url_string, &[])
            .context("Failed to initiate HTTP GET request")?;

        // 3. Complete request and retrieve response headers
        connection
            .initiate_response()
            .context("Failed to get HTTP response")?;

        let status = connection.status();
        if status != 200 {
            anyhow::bail!("Server returned non-200 HTTP status code: {}", status);
        }

        // 4. Read body bytes directly into buffer
        let mut buf = vec![0u8; 64 * 1024];
        let mut offset = 0;

        loop {
            let bytes_read = connection
                .read(&mut buf[offset..])
                .context("Error reading response stream")?;

            if bytes_read == 0 {
                break;
            }
            offset += bytes_read;
        }

        // 5. Parse JSON payload into Vec<GroupMember>
        let members: Vec<GroupMember> = serde_json::from_slice(&buf[..offset])
            .context("Failed to parse JSON template payload into Vec<GroupMember>")?;

        for member in &members {
            if member.face_embedding.len() != FACE_EMBEDDING_DIM {
                anyhow::bail!(
                    "Member '{}' has invalid face_embedding dimension: {} (expected {})",
                    member.name,
                    member.face_embedding.len(),
                    FACE_EMBEDDING_DIM
                );
            }
        }

        Ok(members)
    })
    .await?
}

pub fn init_power_management() -> anyhow::Result<()> {
    let pm_config = esp_pm_config_t {
        max_freq_mhz: 400,
        min_freq_mhz: 40,
        light_sleep_enable: true,
    };

    unsafe {
        let ret = esp_pm_configure(&pm_config as *const _ as *const _);
        if ret != 0 {
            log::warn!("Failed to enable dynamic frequency scaling: {}", ret);
        } else {
            log::info!("Dynamic Frequency Scaling initialized (40MHz - 400MHz with Light Sleep).");
        }
    }
    Ok(())
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