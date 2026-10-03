use log::{error, info, warn};
use std::time::{Duration, Instant};

use crate::pipeline::InferenceEvent;
use crate::system::SystemResources;


pub use biometric_core::matching::{best_match, GroupMember};

#[derive(Debug)]
#[allow(dead_code)] // TODO: UpdatingRuntimeData/Error become reachable once network triggers are wired
pub enum SystemState {
    Initialize,
    RetrieveRuntimeData,
    DetectionValidation,
    UpdatingRuntimeData { force_full_resync: bool },
    ActionExecuted { member: GroupMember },
    Error(String),
}

pub struct BiometricSystem {
    state: SystemState,
    action_display_timer: Option<Instant>,
}

impl BiometricSystem {
    pub fn new() -> Self {
        Self {
            state: SystemState::Initialize,
            action_display_timer: None,
        }
    }

    /// Primary execution cycle called continuously from the main loop
    pub fn tick(
        &mut self,
        resources: &mut SystemResources,
        admin_button_pressed: bool,
        has_update: bool,
    ) {
        let now = Instant::now();

        // Any user input keeps the device awake (deep sleep is owned by power::spawn_inactivity_watchdog)
        if admin_button_pressed || resources.is_touch_pressed() {
            resources.inactivity_timer.reset();
        }

        match &self.state {
            // -----------------------------------------------------------------
            // 1. INITIALIZE: System setup verification
            // -----------------------------------------------------------------
            SystemState::Initialize => {
                info!("Hardware & pipeline ready. Transitioning to DetectionValidation...");
                resources.inactivity_timer.reset();
                self.state = SystemState::DetectionValidation;
            }

            // -----------------------------------------------------------------
            // 2. DETECTION & VALIDATION: Main Active Loop
            // -----------------------------------------------------------------
            SystemState::DetectionValidation => {
                // A. Check for Admin / Network Update Trigger
                if admin_button_pressed && has_update {
                    info!("Admin update triggered. Transitioning to RetrieveRuntimeData...");
                    self.state = SystemState::RetrieveRuntimeData;
                    return;
                }

                // B. Consume results from the Core 1 vision pipeline
                while let Ok(event) = resources.vision_events.try_recv() {
                    match event {
                        InferenceEvent::FaceSeen => resources.inactivity_timer.reset(),
                        InferenceEvent::Match(member) => {
                            info!("Biometric match confirmed for: {}", member.name);
                            resources.inactivity_timer.reset();
                            self.action_display_timer = Some(now + Duration::from_secs(3));
                            self.state = SystemState::ActionExecuted { member };
                            return;
                        }
                    }
                }
            }

            // -----------------------------------------------------------------
            // 3. RETRIEVE RUNTIME DATA: Admin update check
            // -----------------------------------------------------------------
            SystemState::RetrieveRuntimeData => {
                resources.inactivity_timer.reset();
                info!("State: RetrieveRuntimeData - Fetching user biometric profiles...");

                let members_guard = resources.group_members.load();
                if members_guard.is_empty() && resources.check_ethernet_link_status() {
                    if let Err(e) = resources.fetch_runtime_templates() {
                        warn!("Failed to load runtime templates: {:?}", e);
                    }
                }
                self.state = SystemState::DetectionValidation;
            }

            // -----------------------------------------------------------------
            // 4. UPDATING RUNTIME DATA: Flash sync
            // -----------------------------------------------------------------
            SystemState::UpdatingRuntimeData { force_full_resync } => {
                resources.inactivity_timer.reset();
                info!("State: UpdatingRuntimeData (Force Full Resync: {})", force_full_resync);

                if !resources.check_ethernet_link_status() {
                    error!("Cannot sync: Ethernet cable is disconnected!");
                } else {
                    info!("Ethernet link verified. Starting outbound HTTP sync...");
                    if *force_full_resync {
                        info!("Fetching biometric templates over Ethernet...");
                        if let Err(e) = resources.fetch_runtime_templates() {
                            warn!("Failed to load runtime templates: {:?}", e);
                        }
                    }
                }
                self.state = SystemState::DetectionValidation;
            }

            // -----------------------------------------------------------------
            // 5. ACTION EXECUTED: Unlock / Success UI feedback
            // -----------------------------------------------------------------
            SystemState::ActionExecuted { .. } => {
                if let Some(timer) = self.action_display_timer {
                    if now >= timer {
                        info!("Action feedback complete. Returning to DetectionValidation.");
                        resources.inactivity_timer.reset();
                        self.action_display_timer = None;
                        self.state = SystemState::DetectionValidation;
                    }
                }
            }

            // -----------------------------------------------------------------
            // 6. ERROR: Recovery / Fault State
            // -----------------------------------------------------------------
            SystemState::Error(err_msg) => {
                error!("Catastrophic error encountered: {}", err_msg);
            }
        }
    }
}
