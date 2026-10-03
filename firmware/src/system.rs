use anyhow::{Context, Result, bail};
use arc_swap::ArcSwap;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{Input, PinDriver, Pull};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection, Method};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::timer::EspTaskTimerService;
use log::{info, warn};
use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;

use crate::audio_worker::{AudioFrame, AUDIO_QUEUE_DEPTH};
use crate::ffi;
use crate::speaker::Speaker;
use crate::power::InactivityTimer;
use crate::pipeline::InferenceEvent;
use crate::biometrics::GroupMember;

// Sleep Parameters (wake pins live in power.rs)
const INACTIVITY_TIMEOUT_SECS: u64 = 180;  // 3 mins
const TEMPLATE_ENDPOINT: &str = "http://192.168.1.100:8080/api/v1/members";

// Force 16-byte alignment required by ESP32-P4 ESP-DL hardware acceleration
#[repr(C, align(16))]
struct AlignedModel<const N: usize>([u8; N]);

const RAW_MODEL_BYTES: &[u8; include_bytes!("../assets/mobilefacenet_quantized.espdl").len()] =
    include_bytes!("../assets/mobilefacenet_quantized.espdl");

static MODEL_WEIGHTS: AlignedModel<{ RAW_MODEL_BYTES.len() }> = AlignedModel(*RAW_MODEL_BYTES);

pub type P4HardwareConfig = ffi::p4_hardware_config_t;

#[allow(dead_code)] // TODO: populate from ETHERNET_EVENT_* / IP_EVENT_ETH_GOT_IP
pub struct EthernetSession {
    pub is_connected: bool,
    pub ip_address: Option<String>,
}

/// Lifetime-free system resources container
pub struct SystemResources {
    // Service handles held for the program lifetime (taking them initializes the underlying IDF services)
    _nvs: EspDefaultNvsPartition,
    _event_loop: EspSystemEventLoop,
    _timer_service: EspTaskTimerService,
    pub admin_button: PinDriver<'static, Input>,

    // Inactivity watchdog timer handle
    pub inactivity_timer: InactivityTimer,

    // Bounded mic frame queue; frames are dropped when nobody drains it
    #[allow(dead_code)] // TODO: drained by the voice pipeline
    pub audio_rx: std::sync::mpsc::Receiver<AudioFrame>,

    // Shared between Core 0 (Network) and Core 1 (Matcher)
    pub group_members: Arc<ArcSwap<Vec<GroupMember>>>,

    // Peripheral Handles & Session State
    #[allow(dead_code)] // TODO: success chime on match
    pub speaker: Speaker,
    #[allow(dead_code)] // TODO: populated from Ethernet events
    pub net_session: EthernetSession,

    // Detection / recognition results from the Core 1 vision pipeline
    pub vision_events: std::sync::mpsc::Receiver<InferenceEvent>,
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

        // 2. Unified BSP Board Hardware (Display, Camera, I2C, Power)
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

        // 3. Audio Subsystem & Worker
        //init_audio_subsystem()
        //    .context("[SystemResources] initializes audio system")?;
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioFrame>(AUDIO_QUEUE_DEPTH);
        crate::audio_worker::spawn_audio_capture_thread(0, audio_tx);
        let speaker = Speaker::new(0);

        // 4. Admin Button (Pure Rust PinDriver on GPIO0)
        let admin_button = PinDriver::input(self.peripherals.pins.gpio0, Pull::Up)
            .context("[SystemResources] Failed to configure GPIO0 as admin button input")?;

        // 5. Inactivity Watchdog
        let inactivity_timer = InactivityTimer::new();
        crate::power::spawn_inactivity_watchdog(inactivity_timer.clone(), INACTIVITY_TIMEOUT_SECS);
        info!("[SystemResources] Power Inactivity watchdog active (Timeout: {}s)", INACTIVITY_TIMEOUT_SECS);

        // 6. Neural Model Setup
        let model: &'static [u8] = &MODEL_WEIGHTS.0;
        info!("[ESP-DL] MobileFaceNet model mapped at flash addr {:p} (Size: {} bytes)", model.as_ptr(), model.len());

        let dl_err = unsafe { ffi::dl_mobilefacenet_init(model.as_ptr(), model.len()) };
        if dl_err != 0 {
            bail!("[SystemResources] MobileFaceNet Init Failed with code: {}", dl_err);
        }

        // 7. Vision pipeline (camera + inference threads on Core 1)
        let group_members = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let vision_events = crate::pipeline::spawn(group_members.clone())
            .context("[SystemResources] Failed to start vision pipeline")?;

        info!("[SystemResources] All hardware subsystems and LVGL 9 split-screen ready!");
        Ok(SystemResources {
            _nvs: nvs,
            _event_loop: event_loop,
            _timer_service: timer_service,
            speaker,
            inactivity_timer,
            audio_rx,
            admin_button,
            group_members,
            net_session: EthernetSession {
                is_connected: false,
                ip_address: None,
            },
            vision_events,
        })
    }
}

impl SystemResources {
    pub fn builder() -> Result<SystemResourcesBuilder> {
        SystemResourcesBuilder::new()
    }

    pub fn is_admin_pressed(&self) -> bool {
        self.admin_button.is_low()
    }

    pub fn is_touch_pressed(&self) -> bool {
        unsafe { ffi::p4_touch_is_pressed() }
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

    pub fn check_ethernet_link_status(&self) -> bool {
        //TODO: KTANG unsafe { ffi::p4_eth_is_link_up() }
        true
    }
}

pub fn validate_running_app() {
    unsafe { ffi::p4_mark_app_valid() };
}

