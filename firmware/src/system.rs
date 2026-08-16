use anyhow::{Context, Result, bail};
use arc_swap::ArcSwap;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{Gpio0, Input, PinDriver, Pull};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection, Method};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::timer::EspTaskTimerService;
use log::{error, info, warn};
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread::sleep;
use std::time::Duration;

use crate::hdmi_audio::HdmiAudioPlayer;
use crate::power::InactivityTimer;
use crate::video::{FaceBox, VideoPipeline};
use crate::biometrics::{GroupMember, SystemState};

// Hardware LP GPIO Configuration & Sleep Parameters
const GT911_INT_LP_GPIO: i32 = 0;
const ADMIN_BUTTON_LP_GPIO: i32 = 1;
const INACTIVITY_TIMEOUT_SECS: u64 = 180;  // 3 mins
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
    pub buffer_index: u32,
}
impl Default for P4CameraFrame {
    fn default() -> Self {
        Self {
            data: std::ptr::null_mut(),
            data_len: 0,
            width: 0,
            height: 0,
            buffer_index: 0,
        }
    }
}
impl P4CameraFrame {
    pub fn as_slice(&self) -> Option<&[u16]> {
        if self.data.is_null() || self.data_len == 0 {
            return None;
        }
        let pixel_count = (self.width as usize) * (self.height as usize);
        unsafe {
            Some(std::slice::from_raw_parts(
                self.data as *const u16,
                pixel_count,
            ))
        }
    }
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

pub mod ffi {
    use esp_idf_svc::sys::esp_err_t;
    use super::{ P4CameraFrame, P4HardwareConfig, P4TouchData };

    extern "C" {
        pub fn p4_mark_app_valid();
        pub fn p4_hardware_init_all(config: *const P4HardwareConfig) -> i32;
        pub fn p4_camera_capture_frame(frame: *mut P4CameraFrame, timeout_ms: u32) -> i32;
        pub fn p4_camera_release_frame(frame: *const P4CameraFrame) -> i32;
        pub fn p4_perform_ota_update(url: *const libc::c_char) -> esp_err_t;
        pub fn dl_mobilefacenet_init(model_buf: *const u8, model_size: usize) -> i32;
        pub fn dl_mobilefacenet_run(crop_rgb888: *const u8, out_embedding: *mut f32, embedding_len: usize) -> i32;
        pub fn init_i2s_duplex_c(sample_rate: u32, bclk_gpio: i32, ws_gpio: i32, din_gpio: i32, dout_gpio: i32) -> i32;
        pub fn update_camera_viewport(frame: *const P4CameraFrame);
    }
}

pub struct EthernetSession {
    pub is_connected: bool,
    pub ip_address: Option<String>,
}

/// Lifetime-free system resources container
pub struct SystemResources {
    pub nvs: EspDefaultNvsPartition,
    pub event_loop: EspSystemEventLoop,
    pub timer_service: EspTaskTimerService,
    pub model_ptr: *mut u8,
    pub model_size: usize,
    pub model_weights: Option<Vec<u8>>,
    pub video_pipeline: VideoPipeline,
    pub admin_button: PinDriver<'static, Input>,

    // Inactivity watchdog timer handle
    pub inactivity_timer: InactivityTimer,

    // Shared between Core 0 (Network) and Core 1 (Matcher)
    pub group_members: Arc<ArcSwap<Vec<GroupMember>>>,

    // Peripheral Handles & Session State
    pub hdmi_player: HdmiAudioPlayer,
    pub net_session: EthernetSession,
    pub update_trigger_received: bool,
    pub pending_template_download: bool,

    // Zero-copy camera metadata struct passed to C
    pub raw_frame: P4CameraFrame,
    frame_valid: bool,

    pub fail_count: u32,
}

pub struct SystemResourcesBuilder {
    peripherals: Peripherals,
}

impl SystemResourcesBuilder {
    pub fn new() -> Result<Self> {
        let peripherals = Peripherals::take()
            .context("SystemResources Failed to take ESP32-P4 peripherals")?;
        Ok(Self { peripherals })
    }

    pub fn build(self) -> Result<SystemResources> {
        // 1. Base Service Handlers
        let nvs = EspDefaultNvsPartition::take()
            .context("[SystemResources] Failed to take default NVS partition")?;
        let event_loop = EspSystemEventLoop::take()
            .context("[SystemResources] Failed to take system event loop")?;
        let timer_service = EspTaskTimerService::new()
            .context("[SystemResources] Failed to create task timer service")?;

        // 2. Audio Subsystem & Worker
        init_audio_subsystem()
            .context("[SystemResources] initializes audio system")?;
        let (audio_tx, _audio_rx) = std::sync::mpsc::channel::<Vec<i16>>();
        crate::audio_worker::spawn_audio_capture_thread(0, audio_tx);
        let hdmi_player = HdmiAudioPlayer::new(0);

        // 3. Unified BSP Board Hardware (Display, Camera, I2C, Power)
        let config = P4HardwareConfig {
            display_width: 1280,
            display_height: 720,
            camera_width: 1280,
            camera_height: 720,
        };
        let init_ret = unsafe { ffi::p4_hardware_init_all(&config) };
        if init_ret != 0 {
            bail!("[SystemResources] p4_hardware_init_all failed: {}", init_ret);
        }

        // 4. Admin Button (Pure Rust PinDriver on GPIO0)
        let admin_button = PinDriver::input(self.peripherals.pins.gpio0, Pull::Up)
            .context("[SystemResources] Failed to configure GPIO0 as admin button input")?;

        // 5. Inactivity Watchdog
        let inactivity_timer = InactivityTimer::new();
        crate::power::spawn_inactivity_watchdog(
            inactivity_timer.clone(),
            INACTIVITY_TIMEOUT_SECS,
            GT911_INT_LP_GPIO,
            ADMIN_BUTTON_LP_GPIO,
        );
        info!("[SystemResources] Power Inactivity watchdog active (Timeout: {}s)", INACTIVITY_TIMEOUT_SECS);

        // 6. Neural Model Setup
        let model_ptr = MODEL_WEIGHTS.0.as_ptr();
        let model_size = MODEL_WEIGHTS.0.len();
        info!("[ESP-DL] MobileFaceNet model mapped at flash addr {:p} (Size: {} bytes)", model_ptr, model_size);

        let dl_err = unsafe { ffi::dl_mobilefacenet_init(model_ptr as *const u8, model_size) };
        if dl_err != 0 {
            bail!("[SystemResources] MobileFaceNet Init Failed with code: {}", dl_err);
        }

        info!("[SystemResources] All hardware subsystems and LVGL 9 split-screen ready!");
        Ok(SystemResources {
            nvs,
            event_loop,
            timer_service,
            hdmi_player,
            inactivity_timer: InactivityTimer::new(),
            admin_button,
            video_pipeline: VideoPipeline::new(),
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
            raw_frame: P4CameraFrame::default(),
            frame_valid: false,
            fail_count: 0,
        })
    }
}

impl SystemResources {
    pub fn builder() -> Result<SystemResourcesBuilder> {
        SystemResourcesBuilder::new()
    }

    /// Captures the frame into the C driver buffer.
    /// `&mut self` borrow ENDS instantly upon return.
    pub fn capture_camera_frame(&mut self) -> bool {
        let ret = unsafe { ffi::p4_camera_capture_frame(&mut self.raw_frame, 100) };
        if ret == 0 && !self.raw_frame.data.is_null() {
            self.frame_valid = true;
            true
        } else {
            self.frame_valid = false;
            false
        }
    }

    pub fn release_camera_frame(&mut self) {
        if self.frame_valid {
            unsafe {
                ffi::p4_camera_release_frame(&self.raw_frame);
            }
            self.frame_valid = false;
        }
    }

    /// Converts the C PSRAM pointer into a safe, immutable RGB565 Rust slice (&[u16]).
    pub fn camera_frame(&self) -> Option<&[u16]> {
        if !self.frame_valid || self.raw_frame.data.is_null() {
            return None;
        }

        let pixel_count = (self.raw_frame.width as usize) * (self.raw_frame.height as usize);
        
        // Safety: Pointer is guaranteed non-null and valid for `pixel_count` u16 elements by C driver
        unsafe {
            Some(std::slice::from_raw_parts(self.raw_frame.data as *const u16,pixel_count))
        }
    }

    pub fn detect_faces(&self, _frame: &[u16]) -> Vec<FaceBox> {
        // Run ESP-DL MobileFaceNet pass
        vec![]
    }

    pub fn is_admin_pressed(&self) -> bool {
        self.admin_button.is_low()
    }

    /// Fetch group members over network with local Flash fallback
    pub fn fetch_runtime_templates(&mut self) -> Result<()> {
        info!("Attempting HTTP template fetch from: {}", TEMPLATE_ENDPOINT);

        match self.download_members_http() {
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
    pub fn download_members_http(&self) -> Result<Vec<GroupMember>> {
        let mut connection = EspHttpConnection::new(&HttpConfig {
            use_global_ca_store: false,
            buffer_size: Some(1024),
            buffer_size_tx: Some(1024),
            ..Default::default()
        })
        .context("Failed to build HTTP connection handle")?;

        connection
            .initiate_request(Method::Get, TEMPLATE_ENDPOINT, &[])
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

        let err_code = unsafe { ffi::dl_mobilefacenet_init(self.model_ptr as *const u8, self.model_size) };

        if err_code != 0 {
            bail!("dl_mobilefacenet_init failed with esp_err_t code: {}", err_code);
        }

        info!("[ESP-DL] MobileFaceNet model initialized successfully!");
        Ok(())
    }

    pub fn extract_face_embedding(&self, face_crop_112x112: &[u8]) -> anyhow::Result<[f32; 512]> {
        if face_crop_112x112.len() != 112 * 112 * 3 {
            anyhow::bail!("Invalid face crop frame size");
        }

        let mut embedding = [0.0f32; 512];

        let err = unsafe {
            ffi::dl_mobilefacenet_run(
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

    pub fn check_ethernet_link_status(&self) -> bool {
        //TODO: KTANG unsafe { ffi::p4_eth_is_link_up() }
        true
    }
}

pub fn validate_running_app() {
    unsafe { ffi::p4_mark_app_valid() };
}

pub fn init_audio_subsystem() -> Result<()> {
    let ret = unsafe { 
        ffi::init_i2s_duplex_c(SAMPLE_RATE, BCLK_GPIO, WS_GPIO, DIN_GPIO, DOUT_GPIO) 
    };
    
    if ret != 0 {
        bail!("Failed to initialize audio subsystem: {}", ret); // Instantly returns Err
    }

    info!("[Audio] Duplex I2S audio subsystem initialized.");
    Ok(())
}

pub fn test_camera_capture() {
    let mut frame = P4CameraFrame {
        data: std::ptr::null_mut(),
        data_len: 0,
        width: 0,
        height: 0,
        buffer_index: 0
    };

    info!("Attempting to capture frame from OV5647 MIPI-CSI camera...");
    let ret = unsafe { ffi::p4_camera_capture_frame(&mut frame as *mut _, 1000) };

    if ret == 0 {
        info!(
            "SUCCESS! Captured {}x{} frame ({} bytes) in PSRAM!",
            frame.width, frame.height, frame.data_len
        );
    } else {
        error!("Camera capture failed with error code: {}", ret);
    }
}

/// Crops a face region from the raw RGB565 camera frame and resizes it to 112x112 RGB888 for ESP-DL
pub fn crop_face_112x112(frame_rgb565: &[u16], frame_w: usize, frame_h: usize, face: &FaceBox) -> Option<Vec<u8>> {

    let bx = (face.x as usize).min(frame_w - 1);
    let by = (face.y as usize).min(frame_h - 1);
    let bw = (face.width as usize).min(frame_w - bx);
    let bh = (face.height as usize).min(frame_h - by);
    
    if bw == 0 || bh == 0 {
        return None;
    }

    let mut crop_rgb888 = vec![0u8; 112 * 112 * 3];

    // Nearest-neighbor crop and RGB565 -> RGB888 conversion
    for dy in 0..112 {
        let sy = by + (dy * bh) / 112;
        for dx in 0..112 {
            let sx = bx + (dx * bw) / 112;
            let pixel_565 = frame_rgb565[sy * frame_w + sx];

            // Extract RGB channels
            let r = (((pixel_565 >> 11) & 0x1F) as u8) << 3;
            let g = (((pixel_565 >> 5) & 0x3F) as u8) << 2;
            let b = ((pixel_565 & 0x1F) as u8) << 3;

            let out_idx = (dy * 112 + dx) * 3;
            crop_rgb888[out_idx] = r;
            crop_rgb888[out_idx + 1] = g;
            crop_rgb888[out_idx + 2] = b;
        }
    }

    Some(crop_rgb888)
}