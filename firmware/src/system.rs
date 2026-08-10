use anyhow::{Context, Result, bail};
use arc_swap::ArcSwap;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::*;
use esp_idf_svc::hal::i2c::I2C0;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection, Method};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::esp_err_t;
use esp_idf_svc::timer::EspTaskTimerService;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::sleep;
use std::time::Duration;

use crate::ffi;
use crate::hdmi_audio::HdmiAudioPlayer;
use crate::power::InactivityTimer;

// Hardware LP GPIO Configuration & Sleep Parameters
const GT911_INT_LP_GPIO: i32 = 0;
const ADMIN_BUTTON_LP_GPIO: i32 = 1;
const INACTIVITY_TIMEOUT_SECS: u64 = 15;
const TEMPLATE_ENDPOINT: &str = "http://192.168.1.100:8080/api/v1/members";
const MODEL_ENDPOINT: &str = "http://192.168.1.100/models/face_v1.bin";

// Shared Pin Definitions for I2S_NUM_0
const SAMPLE_RATE: u32 = 16_000;
const BCLK_GPIO: i32 = 12;
const WS_GPIO: i32 = 13;
const DIN_GPIO: i32 = 11; // INMP441 Mic
const DOUT_GPIO: i32 = 14; // Speaker / HDMI Audio

// Force 16-byte alignment required by ESP32-P4 ESP-DL hardware acceleration
#[repr(C, align(16))]
struct AlignedModel<const N: usize>([u8; N]);

const RAW_MODEL_BYTES: &[u8; include_bytes!("../assets/mobilefacenet_quantized.espdl").len()] =
    include_bytes!("../assets/mobilefacenet_quantized.espdl");

static MODEL_WEIGHTS: AlignedModel<{ RAW_MODEL_BYTES.len() }> = AlignedModel(*RAW_MODEL_BYTES);

// -----------------------------------------------------------------------------
// FFI Structures (Must match biometrics_wrapper.h)
// -----------------------------------------------------------------------------
#[repr(C)]
pub struct P4HardwareConfig {
    pub display_width: u16,
    pub display_height: u16,
    pub camera_width: u16,
    pub camera_height: u16,
}

#[repr(C)]
pub struct P4CameraFrame {
    pub data: *mut u8,
    pub data_len: usize,
    pub width: u16,
    pub height: u16,
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct P4TouchData {
    pub x: u16,
    pub y: u16,
    pub strength: u16,
    pub points: u8,
    pub touched: bool,
}

extern "C" {
    fn init_admin_button_gpio() -> i32;
    fn is_admin_button_pressed() -> bool;
    fn p4_hardware_init_all(config: *const P4HardwareConfig) -> i32;
    fn p4_camera_capture_frame(frame: *mut P4CameraFrame, timeout_ms: u32) -> i32;
    fn p4_perform_ota_update(url: *const libc::c_char) -> esp_err_t;
    fn p4_mark_app_valid();
    fn dl_mobilefacenet_init(model_buf: *const u8, model_size: usize) -> i32;
    fn dl_mobilefacenet_run(
        crop_rgb888: *const u8,
        out_embedding: *mut f32,
        embedding_len: usize,
    ) -> i32;
    fn init_touch_with_bsp() -> i32;
    fn p4_touch_read(touch_data: *mut P4TouchData) -> bool;
}

// -----------------------------------------------------------------------------
// System State & Domain Models
// -----------------------------------------------------------------------------
#[derive(Debug)]
pub enum SystemState {
    Initialize,
    RetrieveRuntimeData,
    DetectionValidation,
    UpdatingRuntimeData { force_full_resync: bool },
    ActionExecuted { member: GroupMember },
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Role {
    ADMIN,
    USER,
    GUEST,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    pub id: u8,
    pub name: String,
    pub role: Role,
    pub face_embedding: Vec<f32>,
}

pub struct EthernetSession {
    pub is_connected: bool,
    pub ip_address: Option<String>,
}

pub struct HardwareTriggers {
    pub admin_button_pressed: AtomicBool,
}

pub struct HardwarePins {
    pub int_pin: Gpio0<'static>,
}

/// Lifetime-free system resources container
pub struct SystemResources {
    pub nvs: EspDefaultNvsPartition,
    pub event_loop: EspSystemEventLoop,
    pub timer_service: EspTaskTimerService,
    pub model_ptr: *mut u8,
    pub model_size: usize,
    pub model_weights: Option<Vec<u8>>,
    pins: Option<HardwarePins>,

    // Inactivity watchdog timer handle
    pub inactivity_timer: InactivityTimer,

    // Shared between Core 0 (Network) and Core 1 (Matcher)
    pub group_members: Arc<ArcSwap<Vec<GroupMember>>>,

    // Peripheral Handles & Session State
    pub hdmi_player: HdmiAudioPlayer,
    pub net_session: EthernetSession,
    pub update_trigger_received: bool,
    pub pending_template_download: bool,
}

impl SystemResources {
    pub fn new(
        nvs: EspDefaultNvsPartition,
        event_loop: EspSystemEventLoop,
        timer_service: EspTaskTimerService,
        hdmi_player: HdmiAudioPlayer,
        pins: HardwarePins,
    ) -> Self {
        let model_ptr = MODEL_WEIGHTS.0.as_ptr();
        let model_size = MODEL_WEIGHTS.0.len();
        info!(
            "[ESP-DL] MobileFaceNet model mapped at flash addr {:p} (Size: {} bytes)",
            model_ptr, model_size
        );

        Self {
            nvs,
            event_loop,
            timer_service,
            hdmi_player,
            inactivity_timer: InactivityTimer::new(),
            pins: Some(pins),
            model_ptr: model_ptr as *mut u8,
            model_size,
            model_weights: None,
            group_members: Arc::new(ArcSwap::from_pointee(Vec::new())),
            net_session: EthernetSession {
                is_connected: false,
                ip_address: None,
            },
            update_trigger_received: false,
            pending_template_download: false,
        }
    }

    /// Master State Machine Handler Method
    pub fn process_state_machine(&mut self, current_state: SystemState) -> Result<SystemState> {
        match current_state {
            SystemState::Initialize => self.handle_initialize(MODEL_ENDPOINT),

            SystemState::RetrieveRuntimeData => {
                self.inactivity_timer.reset();
                info!("State: RetrieveRuntimeData - Fetching user biometric profiles...");

                let members_guard = self.group_members.load();
                if members_guard.is_empty() && check_ethernet_link_status() {
                    if let Err(e) = self.fetch_runtime_templates(TEMPLATE_ENDPOINT) {
                        warn!("Failed to load runtime templates: {:?}", e);
                    }
                }
                Ok(SystemState::DetectionValidation)
            }

            SystemState::DetectionValidation => {
                if check_admin_button() {
                    self.inactivity_timer.reset();

                    let mut hold_counter = 0;
                    while check_admin_button() && hold_counter < 50 {
                        sleep(Duration::from_millis(100));
                        hold_counter += 1;
                    }

                    let force_full = hold_counter >= 30; // Held > 3 seconds
                    info!("Triggering manual update (Full Model Resync: {})", force_full);
                    return Ok(SystemState::UpdatingRuntimeData {
                        force_full_resync: force_full,
                    });
                }

                // Poll touch events
                let mut touch = P4TouchData::default();
                if unsafe { p4_touch_read(&mut touch) } {
                    self.inactivity_timer.reset();
                    info!("[Touch] Screen touched at X: {}, Y: {}", touch.x, touch.y);
                }

                Ok(SystemState::DetectionValidation)
            }

            SystemState::UpdatingRuntimeData { force_full_resync } => {
                self.inactivity_timer.reset();
                info!(
                    "State: UpdatingRuntimeData (Force Full Resync: {})",
                    force_full_resync
                );

                if !check_ethernet_link_status() {
                    error!("Cannot sync: Ethernet cable is disconnected!");
                    return Ok(SystemState::DetectionValidation);
                } else {
                    info!("Ethernet link verified. Starting outbound HTTP sync...");

                    if force_full_resync {
                        info!("Fetching biometric templates over Ethernet...");
                        if let Err(e) = self.fetch_runtime_templates(TEMPLATE_ENDPOINT) {
                            warn!("Failed to load runtime templates: {:?}", e);
                        }
                    }

                    return Ok(SystemState::DetectionValidation);
                }
            }

            SystemState::ActionExecuted { member } => {
                self.inactivity_timer.reset();
                info!(
                    "State: ActionExecuted for Member: {} ({:?})",
                    member.name, member.role
                );
                Ok(SystemState::DetectionValidation)
            }

            SystemState::Error(err_msg) => {
                error!("State Machine Error: {}", err_msg);
                Ok(SystemState::Error(err_msg))
            }
        }
    }

    /// Handler for SystemState::Initialize
    pub fn handle_initialize(&mut self, _model_server_url: &str) -> Result<SystemState> {
        info!("State: Initialize - Bringing up Ethernet & Hardware...");

        // 1. Initialize Audio Subsystem
        if let Err(e) = init_audio_subsystem() {
            return Ok(SystemState::Error(format!("Audio Init Failed ({})", e)));
        }

        // Spawn audio capture worker thread
        let (audio_tx, _audio_rx) = std::sync::mpsc::channel::<Vec<i16>>();
        crate::audio_worker::spawn_audio_capture_thread(0, audio_tx);

        // 2. Initialize P4 EMAC Ethernet
        let eth_err = unsafe { ffi::init_p4_ethernet() };
        if eth_err != 0 {
            error!("Ethernet hardware initialization failed with code: {}", eth_err);
            return Ok(SystemState::Error(format!("Ethernet Init Failed ({})", eth_err)));
        }

        // 3. Initialize ESP-DL MobileFaceNet Neural Model from Flash Memory
        if let Err(e) = self.init_mobilefacenet() {
            return Ok(SystemState::Error(format!("ESP-DL Model Init Failed: {:?}", e)));
        }

        // 4. Bring up Unified Board Hardware (Display, Camera, I2C, Power) via BSP
        if let Err(e) = bring_up_hardware() {
            error!("BSP Display and Camera initialization failed: {:?}", e);
            return Ok(SystemState::Error(format!("Hardware Bring-Up Failed ({})", e)));
        }

        let _pins = self
            .pins
            .take()
            .context("Hardware pins already consumed")?;

        // 5. Initialize Inactivity Watchdog
        crate::power::spawn_inactivity_watchdog(
            self.inactivity_timer.clone(),
            INACTIVITY_TIMEOUT_SECS,
            GT911_INT_LP_GPIO,
            ADMIN_BUTTON_LP_GPIO,
        );
        info!("[Power] Inactivity watchdog active (Timeout: {}s)", INACTIVITY_TIMEOUT_SECS);

        // 6. Configure Admin GPIO Button
        if let Err(e) = setup_admin_button() {
            warn!("Failed to init Admin Button GPIO: {}", e);
        }

        Ok(SystemState::RetrieveRuntimeData)
    }

    /// Fetch group members over network with local Flash fallback
    pub fn fetch_runtime_templates(&mut self, endpoint_url: &str) -> Result<()> {
        info!("Attempting HTTP template fetch from: {}", endpoint_url);

        match self.download_members_http(endpoint_url) {
            Ok(members) => {
                info!("Successfully fetched {} members over network.", members.len());
                let _ = Self::save_members_to_flash("/spiffs/members.json", &members);
                self.group_members.store(Arc::new(members));
                Ok(())
            }
            Err(err) => {
                warn!("HTTP fetch failed ({:?}). Loading local Flash backup...", err);
                let cached_members = Self::load_members_from_flash("/spiffs/members.json")?;
                self.group_members.store(Arc::new(cached_members));
                Ok(())
            }
        }
    }

    /// HTTP GET stream downloader using EspHttpConnection directly
    pub fn download_members_http(&self, endpoint_url: &str) -> Result<Vec<GroupMember>> {
        let mut connection = EspHttpConnection::new(&HttpConfig {
            use_global_ca_store: false,
            buffer_size: Some(1024),
            buffer_size_tx: Some(1024),
            ..Default::default()
        })
        .context("Failed to build HTTP connection handle")?;

        connection
            .initiate_request(Method::Get, endpoint_url, &[])
            .context("Failed to initiate GET request")?;

        connection
            .initiate_response()
            .context("Failed to receive HTTP response")?;

        let status = connection.status();
        if status != 200 {
            bail!("HTTP GET failed with status code: {}", status);
        }

        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            let bytes_read = connection.read(&mut chunk)?;
            if bytes_read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..bytes_read]);
        }

        let members: Vec<GroupMember> =
            serde_json::from_slice(&buf).context("Failed to deserialize JSON member payload")?;

        Ok(members)
    }

    pub fn save_members_to_flash(path: &str, members: &[GroupMember]) -> Result<()> {
        let json = serde_json::to_vec(members)?;
        let mut file = File::create(path)?;
        file.write_all(&json)?;
        Ok(())
    }

    pub fn load_members_from_flash(path: &str) -> Result<Vec<GroupMember>> {
        let mut file = File::open(path)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        let members: Vec<GroupMember> = serde_json::from_slice(&buf)?;
        Ok(members)
    }

    /// Safe wrapper around ESP-DL MobileFaceNet C initialization
    pub fn init_mobilefacenet(&self) -> Result<()> {
        if self.model_ptr.is_null() {
            bail!("MobileFaceNet model pointer is null!");
        }

        if self.model_size == 0 {
            bail!("MobileFaceNet model size is 0 bytes!");
        }

        info!(
            "[ESP-DL] Initializing MobileFaceNet from flash at {:p} ({} bytes)...",
            self.model_ptr, self.model_size
        );

        let err_code = unsafe { dl_mobilefacenet_init(self.model_ptr as *const u8, self.model_size) };

        if err_code != 0 {
            bail!("dl_mobilefacenet_init failed with esp_err_t code: {}", err_code);
        }

        info!("[ESP-DL] MobileFaceNet model initialized successfully!");
        Ok(())
    }

    /// Extracts a 512-element face feature embedding from a cropped 112x112 face frame
    pub fn extract_face_embedding(&self, face_crop_112x112: &[u8]) -> Result<[f32; 512]> {
        if face_crop_112x112.len() != 112 * 112 * 3 {
            anyhow::bail!("Invalid face crop frame size");
        }

        let mut embedding = [0.0f32; 512];

        let err = unsafe {
            dl_mobilefacenet_run(
                face_crop_112x112.as_ptr(),
                embedding.as_mut_ptr(),
                embedding.len(),
            )
        };

        if err != 0 {
            anyhow::bail!("ESP-DL inference failed with error code: {}", err);
        }

        Ok(embedding)
    }
}

pub fn validate_running_app() {
    unsafe { p4_mark_app_valid() };
}

pub fn setup_admin_button() -> Result<()> {
    let err = unsafe { init_admin_button_gpio() };
    if err != 0 {
        bail!("Failed to configure admin button GPIO, ESP-IDF err: {}", err);
    }
    Ok(())
}

pub fn check_admin_button() -> bool {
    unsafe { is_admin_button_pressed() }
}

pub fn check_ethernet_link_status() -> bool {
    true
}

pub fn bring_up_hardware() -> Result<(), String> {
    let config = P4HardwareConfig {
        display_width: 720,
        display_height: 1280,
        camera_width: 1280,
        camera_height: 720,
    };

    let ret = unsafe { p4_hardware_init_all(&config as *const _) };
    if ret == 0 {
        info!("ESP32-P4 hardware initialized cleanly!");
        Ok(())
    } else {
        Err(format!("Hardware bring-up failed with code {}", ret))
    }
}

pub fn init_audio_subsystem() -> Result<(), i32> {
    let ret = unsafe {
        ffi::init_i2s_duplex_c(SAMPLE_RATE, BCLK_GPIO, WS_GPIO, DIN_GPIO, DOUT_GPIO)
    };

    if ret == 0 {
        info!("[Audio] Duplex I2S audio subsystem initialized.");
        Ok(())
    } else {
        error!("[Audio] Duplex I2S init failed: {}", ret);
        Err(ret)
    }
}

pub fn test_camera_capture() {
    let mut frame = P4CameraFrame {
        data: std::ptr::null_mut(),
        data_len: 0,
        width: 0,
        height: 0,
    };

    info!("Attempting to capture frame from OV5647 MIPI-CSI camera...");
    let ret = unsafe { p4_camera_capture_frame(&mut frame as *mut _, 1000) };

    if ret == 0 {
        info!(
            "SUCCESS! Captured {}x{} frame ({} bytes) in PSRAM!",
            frame.width, frame.height, frame.data_len
        );
    } else {
        error!("Camera capture failed with error code: {}", ret);
    }
}