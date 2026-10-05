use log::{info, warn};
use std::time::{Duration, Instant};

use biometric_core::enrollment::{ENROLL_SAMPLES, MAX_MEMBERS};
use biometric_core::matching::best_match;

use crate::pipeline::{Command, InferenceEvent};
use crate::system::SystemResources;
use crate::ui::{self, UiEvent};

/// How long a match stays on screen.
const MATCH_DISPLAY_TIME: Duration = Duration::from_secs(3);
/// The admin view closes by itself after this long without a touch.
const ADMIN_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// An enrollment is abandoned if it has not collected its samples by then.
const ENROLL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub enum SystemState {
    Initialize,
    DetectionValidation,
    /// Admin view open: members can be enrolled and deleted.
    Admin {
        idle_deadline: Instant,
    },
    /// The vision pipeline is collecting samples of the face in view for a new member.
    Enrolling {
        name: String,
        deadline: Instant,
    },
    /// A match is on screen until `until`.
    ActionExecuted {
        until: Instant,
    },
}

pub struct BiometricSystem {
    state: SystemState,
    admin_was_pressed: bool,
}

impl BiometricSystem {
    pub fn new() -> Self {
        Self {
            state: SystemState::Initialize,
            admin_was_pressed: false,
        }
    }

    /// Primary execution cycle called continuously from the main loop
    pub fn tick(&mut self, resources: &mut SystemResources, admin_button_pressed: bool) {
        let now = Instant::now();

        // Any user input keeps the device awake (deep sleep is owned by power::spawn_inactivity_watchdog)
        if admin_button_pressed || resources.is_touch_pressed() {
            resources.inactivity_timer.reset();
        }
        // The admin button acts once per press, not once per tick while held.
        let admin_clicked = admin_button_pressed && !self.admin_was_pressed;
        self.admin_was_pressed = admin_button_pressed;
        // Polled in every state so touches made while the panel is not listening don't queue up.
        let ui_event = ui::poll_event();

        match &self.state {
            // -----------------------------------------------------------------
            // 1. INITIALIZE: System setup verification
            // -----------------------------------------------------------------
            SystemState::Initialize => {
                info!("Hardware & pipeline ready. Transitioning to DetectionValidation...");
                resources.inactivity_timer.reset();
                show_idle_status(resources);
                self.state = SystemState::DetectionValidation;
            }

            // -----------------------------------------------------------------
            // 2. DETECTION & VALIDATION: Main Active Loop
            // -----------------------------------------------------------------
            SystemState::DetectionValidation => {
                // A. Check for the Admin trigger
                if admin_clicked || ui_event == Some(UiEvent::Admin) {
                    info!("Entering admin mode.");
                    show_member_list(resources);
                    ui::set_admin_mode(true);
                    ui::set_status("Admin: type a name and Enroll, or pick a member and Delete.");
                    self.state = SystemState::Admin {
                        idle_deadline: now + ADMIN_IDLE_TIMEOUT,
                    };
                    return;
                }

                // B. Consume results from the Core 1 vision pipeline
                while let Ok(event) = resources.vision_events.try_recv() {
                    match event {
                        InferenceEvent::FaceSeen => resources.inactivity_timer.reset(),
                        InferenceEvent::Match { member, score } => {
                            info!(
                                "Biometric match confirmed for: {} (similarity {score:.2})",
                                member.name
                            );
                            resources.inactivity_timer.reset();
                            ui::set_status(&format!("Welcome, {}", member.name));
                            if let Err(e) = resources.speaker.play_success_chime() {
                                warn!("Match chime failed: {e:#}");
                            }
                            self.state = SystemState::ActionExecuted {
                                until: now + MATCH_DISPLAY_TIME,
                            };
                            return;
                        }
                        // Leftovers of an enrollment that was cancelled meanwhile.
                        InferenceEvent::EnrollProgress { .. }
                        | InferenceEvent::EnrollCaptured { .. }
                        | InferenceEvent::EnrollFailed(_) => {}
                    }
                }
            }

            // -----------------------------------------------------------------
            // 3. ADMIN: enroll / delete members from the touch panel
            // -----------------------------------------------------------------
            SystemState::Admin { idle_deadline } => {
                drain_vision_events(resources);
                let mut idle_deadline = *idle_deadline;
                if ui_event.is_some() {
                    idle_deadline = now + ADMIN_IDLE_TIMEOUT;
                }
                self.state = SystemState::Admin { idle_deadline };

                if admin_clicked || ui_event == Some(UiEvent::Exit) || now >= idle_deadline {
                    info!("Leaving admin mode.");
                    ui::set_admin_mode(false);
                    show_idle_status(resources);
                    self.state = SystemState::DetectionValidation;
                    return;
                }
                match ui_event {
                    // Whether there is room is decided when the template is stored: enrolling
                    // an existing member again needs no free slot.
                    Some(UiEvent::Enroll { name }) => {
                        if resources
                            .vision_commands
                            .try_send(Command::StartEnroll)
                            .is_err()
                        {
                            ui::set_status("Face recognition is not running.");
                        } else {
                            ui::set_status("Look at the camera, alone in view...");
                            self.state = SystemState::Enrolling {
                                name,
                                deadline: now + ENROLL_TIMEOUT,
                            };
                        }
                    }
                    Some(UiEvent::Delete { index }) => match resources.delete_member(index) {
                        Ok(member) => {
                            show_member_list(resources);
                            ui::set_status(&format!("Deleted {}.", member.name));
                        }
                        Err(e) => {
                            warn!("Delete failed: {e:#}");
                            ui::set_status("Delete failed (see log).");
                        }
                    },
                    Some(UiEvent::Admin | UiEvent::Exit) | None => {}
                }
            }

            // -----------------------------------------------------------------
            // 4. ENROLLING: waiting for the pipeline to capture a template
            // -----------------------------------------------------------------
            SystemState::Enrolling { name, deadline } => {
                resources.inactivity_timer.reset();
                let mut outcome: Option<String> = None;
                while let Ok(event) = resources.vision_events.try_recv() {
                    match event {
                        InferenceEvent::EnrollProgress { collected } => {
                            ui::set_status(&format!("Hold still... {collected}/{ENROLL_SAMPLES}"));
                        }
                        InferenceEvent::EnrollCaptured {
                            embedding,
                            model_version,
                        } => {
                            outcome = Some(finish_enrollment(
                                resources,
                                name,
                                embedding,
                                &model_version,
                            ));
                            break;
                        }
                        InferenceEvent::EnrollFailed(reason) => {
                            outcome = Some(format!("{reason}."));
                            break;
                        }
                        InferenceEvent::FaceSeen | InferenceEvent::Match { .. } => {}
                    }
                }
                if outcome.is_none() {
                    let cancelled = admin_clicked || ui_event == Some(UiEvent::Exit);
                    if cancelled || now >= *deadline {
                        // If the queue is full the pipeline is about to report anyway, and
                        // the leftover event is dropped in the Admin state.
                        let _ = resources.vision_commands.try_send(Command::CancelEnroll);
                        outcome = Some(if cancelled {
                            "Enrollment cancelled.".to_owned()
                        } else {
                            "Enrollment timed out: keep one face steady in view.".to_owned()
                        });
                    }
                }
                if let Some(message) = outcome {
                    info!("Enrollment finished: {message}");
                    ui::set_status(&message);
                    self.state = SystemState::Admin {
                        idle_deadline: now + ADMIN_IDLE_TIMEOUT,
                    };
                }
            }

            // -----------------------------------------------------------------
            // 5. ACTION EXECUTED: Unlock / Success UI feedback
            // -----------------------------------------------------------------
            SystemState::ActionExecuted { until } => {
                let until = *until;
                drain_vision_events(resources);
                if now >= until {
                    info!("Action feedback complete. Returning to DetectionValidation.");
                    resources.inactivity_timer.reset();
                    show_idle_status(resources);
                    self.state = SystemState::DetectionValidation;
                }
            }
        }
    }
}

/// Discards pipeline results the current state has no use for, so the pipeline's bounded
/// queue never fills up; a seen face still keeps the device awake.
fn drain_vision_events(resources: &mut SystemResources) {
    while let Ok(event) = resources.vision_events.try_recv() {
        if matches!(
            event,
            InferenceEvent::FaceSeen | InferenceEvent::Match { .. }
        ) {
            resources.inactivity_timer.reset();
        }
    }
}

/// Stores a captured template as a new member; returns the message to show.
fn finish_enrollment(
    resources: &mut SystemResources,
    name: &str,
    embedding: Vec<f32>,
    model_version: &str,
) -> String {
    // One template per person: a second one would only split that person's matches.
    let roster = resources.group_members.load_full();
    if let Some((index, score)) = best_match(&embedding, model_version, &roster) {
        return format!(
            "Already enrolled as {} (similarity {score:.2}).",
            roster[index].name
        );
    }
    match resources.enroll_member(name, embedding, model_version) {
        Ok(member) => {
            show_member_list(resources);
            format!("Enrolled {}.", member.name)
        }
        Err(e) => {
            warn!("Enrollment failed: {e:#}");
            format!("Enrollment failed: {e}.")
        }
    }
}

fn show_member_list(resources: &SystemResources) {
    let roster = resources.group_members.load();
    ui::set_members(&roster);
}

fn show_idle_status(resources: &SystemResources) {
    let enrolled = resources.group_members.load().len();
    ui::set_status(&format!(
        "Ready. {enrolled} of {MAX_MEMBERS} members enrolled."
    ));
}
