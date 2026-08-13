use anyhow::{Context, Result};
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::http::Method;
use serde::{Deserialize, Serialize};

use log::{error, info, warn};
use std::time::{Duration, Instant};

use crate::system::SystemResources;
use crate::video::FaceBox;

const INACTIVITY_TIMEOUT_SECS: u64 = 180; // 3 minutes idle -> Deep Sleep
const FACE_EMBEDDING_DIM: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Role {
    ADMIN,
    USER,
    GUEST,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMember {
    pub id: String,      // First_Last
    pub name: String,
    pub role: Role,
    pub face_embedding: Vec<f32>,
}

#[derive(Debug)]
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
    last_activity_time: Instant,
    action_display_timer: Option<Instant>,
}

impl BiometricSystem {
    pub fn new() -> Self {
        Self {
            state: SystemState::Initialize,
            last_activity_time: Instant::now(),
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

        match &self.state {
            // -----------------------------------------------------------------
            // 1. INITIALIZE: System setup verification
            // -----------------------------------------------------------------
            SystemState::Initialize => {
                info!("Hardware & pipeline ready. Transitioning to DetectionValidation...");
                self.last_activity_time = now;
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

                // B. Capture frame and detect faces using SystemResources
                if resources.capture_camera_frame() {
                    if let Some(frame_slice) = resources.camera_frame() {
                        let detected_faces = resources.detect_faces(frame_slice);

                        // Render 640x360 camera preview on the left half of the display
                        let _ = resources.video_pipeline.render_camera_half(frame_slice, &detected_faces);

                        if !detected_faces.is_empty() {
                            self.last_activity_time = now;

                            // 1. Crop face box to 112x112 RGB888 buffer
                            if let Some(crop_112x112) = resources.crop_face_112x112(frame_slice, &detected_faces[0]) {
                                if let Ok(live_embedding) = resources.extract_face_embedding(&crop_112x112) {
                                    let members_guard = resources.group_members.load();
                                    if let Some(matched_member) = self.try_match_biometrics(&live_embedding, &members_guard) {
                                        info!("Biometric match confirmed for: {}", matched_member.name);
                                        self.action_display_timer = Some(now + Duration::from_secs(3));
                                        self.state = SystemState::ActionExecuted { member: matched_member };
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }


                // D. 180s Inactivity Timeout -> Deep Sleep
                if now.duration_since(self.last_activity_time) >= Duration::from_secs(INACTIVITY_TIMEOUT_SECS) {
                    info!("No activity detected for {}s. Entering Deep Sleep...", INACTIVITY_TIMEOUT_SECS);
                    crate::power::enter_deep_sleep(None); // Shuts off backlight, arms wake pins, calls esp_deep_sleep_start
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
            SystemState::ActionExecuted { member } => {
                // Keep rendering live preview behind UI banner
                //if let Some(frame_slice) = resources.camera_frame() {
                //    let _ = resources.video_pipeline.render_camera_half(frame_slice, &[]);
                //}

                if let Some(timer) = self.action_display_timer {
                    if now >= timer {
                        info!("Action feedback complete. Returning to DetectionValidation.");
                        self.last_activity_time = now;
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

pub fn try_match_biometrics(
        &self,
        live_embedding: &[f32; 512],
        enrolled_templates: &[GroupMember],
    ) -> Option<GroupMember> {
        let mut best_match: Option<(&GroupMember, f32)> = None;
        const MATCH_THRESHOLD: f32 = 0.75; // Cosine similarity threshold

        for member in enrolled_templates {
            let similarity = self.cosine_similarity(live_embedding, &member.face_embedding);

            if similarity >= MATCH_THRESHOLD {
                match best_match {
                    Some((_, highest_sim)) if similarity > highest_sim => {
                        best_match = Some((member, similarity));
                    }
                    None => {
                        best_match = Some((member, similarity));
                    }
                    _ => {}
                }
            }
        }

        // Return a clone of the matched enrolled member
        best_match.map(|(member, _)| member.clone())
    }

    /// Computes dot product of normalized L2 embeddings (Cosine Similarity)
    fn cosine_similarity(&self, a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }

    pub fn state(&self) -> &SystemState {
        &self.state
    }

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
}