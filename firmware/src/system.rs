use anyhow::{bail, Context, Result};
use biometric_core::matching::GroupMember;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::{Input, PinDriver, Pull};
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::{info, warn};
use std::sync::Arc;

use crate::audio_worker::{AudioFrame, AUDIO_QUEUE_DEPTH};
use crate::ffi;
use crate::pipeline::{Command, InferenceEvent, Roster};
use crate::power::InactivityTimer;
use crate::speaker::Speaker;
use crate::templates::TemplateStore;

// Sleep Parameters (wake pins live in power.rs)
const INACTIVITY_TIMEOUT_SECS: u64 = 180; // 3 mins

/// Lifetime-free system resources container
pub struct SystemResources {
    // Service handles held for the program lifetime (taking them initializes the underlying IDF services)
    _nvs: EspDefaultNvsPartition,
    _event_loop: EspSystemEventLoop,
    pub admin_button: PinDriver<'static, Input>,

    // Inactivity watchdog timer handle
    pub inactivity_timer: InactivityTimer,

    // Bounded mic frame queue; frames are dropped when nobody drains it
    #[allow(dead_code)] // TODO: drained by the voice pipeline
    pub audio_rx: std::sync::mpsc::Receiver<AudioFrame>,

    // Enrolled members as seen by the matcher on Core 1; republished after every change
    pub group_members: Arc<Roster>,
    // Their persistent copy; None if the `templates` partition is unusable (enrollment disabled)
    templates: Option<TemplateStore>,

    // Peripheral Handles
    pub speaker: Speaker,

    // Detection / recognition results from the Core 1 vision pipeline
    pub vision_events: std::sync::mpsc::Receiver<InferenceEvent>,
    // Enrollment requests to the Core 1 inference thread
    pub vision_commands: std::sync::mpsc::SyncSender<Command>,
}

impl SystemResources {
    /// Brings up the board and starts the worker threads.
    pub fn new() -> Result<Self> {
        let peripherals =
            Peripherals::take().context("SystemResources Failed to take ESP32-P4 peripherals")?;

        // 1. Base Service Handlers
        let nvs = EspDefaultNvsPartition::take()
            .context("[SystemResources] Failed to take default NVS partition")?;
        let event_loop = EspSystemEventLoop::take()
            .context("[SystemResources] Failed to take system event loop")?;

        // 2. Unified BSP Board Hardware (Display, Camera, I2C, Power)
        // Safety: plain call; called once, from the main thread, before any other wrapper call.
        let init_ret = unsafe { ffi::p4_hardware_init_all() };
        if init_ret != 0 {
            bail!(
                "[SystemResources] p4_hardware_init_all failed: {}",
                init_ret
            );
        }

        // 3. Audio Worker
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioFrame>(AUDIO_QUEUE_DEPTH);
        crate::audio_worker::spawn_audio_capture_thread(0, audio_tx)
            .context("[SystemResources] Failed to start audio capture")?;
        let mut speaker = Speaker::new(0);
        // Audible sign that the codec, amplifier and speaker work.
        if let Err(e) = speaker.play_success_chime() {
            warn!("[SystemResources] boot chime failed: {e:#}");
        }

        // 4. Admin Button (Pure Rust PinDriver on GPIO0)
        let admin_button = PinDriver::input(peripherals.pins.gpio0, Pull::Up)
            .context("[SystemResources] Failed to configure GPIO0 as admin button input")?;

        // 5. Inactivity Watchdog
        let inactivity_timer = InactivityTimer::new();
        crate::power::spawn_inactivity_watchdog(inactivity_timer.clone(), INACTIVITY_TIMEOUT_SECS);
        info!(
            "[SystemResources] Power Inactivity watchdog active (Timeout: {}s)",
            INACTIVITY_TIMEOUT_SECS
        );

        // 6. Enrolled templates. Recognition of nobody is better than no device, so a broken
        //    store only disables enrollment.
        let templates = TemplateStore::open()
            .inspect_err(|e| warn!("[SystemResources] enrollment disabled: {e:#}"))
            .ok();
        let members = templates.as_ref().map(TemplateStore::members);
        let group_members = Arc::new(Roster::from_pointee(members.unwrap_or_default()));

        // 7. Vision pipeline (camera + inference threads on Core 1; loads the face models)
        let vision = crate::pipeline::spawn(group_members.clone(), nvs.clone())
            .context("[SystemResources] Failed to start vision pipeline")?;

        info!("[SystemResources] All hardware subsystems and LVGL 9 split-screen ready!");
        Ok(SystemResources {
            _nvs: nvs,
            _event_loop: event_loop,
            speaker,
            inactivity_timer,
            audio_rx,
            admin_button,
            group_members,
            templates,
            vision_events: vision.events,
            vision_commands: vision.commands,
        })
    }

    pub fn is_admin_pressed(&self) -> bool {
        self.admin_button.is_low()
    }

    pub fn is_touch_pressed(&self) -> bool {
        // Safety: plain call; reads a flag the touch task keeps up to date.
        unsafe { ffi::p4_touch_is_pressed() }
    }

    /// Persists a new member and publishes the updated list to the matcher.
    pub fn enroll_member(
        &mut self,
        name: &str,
        embedding: Vec<f32>,
        model_version: &str,
    ) -> Result<Arc<GroupMember>> {
        let store = self.template_store()?;
        let member = store.enroll(name, embedding, model_version)?;
        let members = store.members();
        self.group_members.store(Arc::new(members));
        Ok(member)
    }

    /// Erases the member at `index` of the published list and publishes the updated list.
    pub fn delete_member(&mut self, index: usize) -> Result<Arc<GroupMember>> {
        let store = self.template_store()?;
        let member = store.delete(index)?;
        let members = store.members();
        self.group_members.store(Arc::new(members));
        Ok(member)
    }

    fn template_store(&mut self) -> Result<&mut TemplateStore> {
        self.templates
            .as_mut()
            .context("template storage is unavailable (see the boot log)")
    }
}

pub fn validate_running_app() {
    // Safety: plain call; confirms the running image to the bootloader.
    unsafe { ffi::p4_mark_app_valid() };
}
