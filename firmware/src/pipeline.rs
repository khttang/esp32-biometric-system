//! Real-time vision pipeline, both threads pinned to Core 1 (Core 0 runs LVGL + the state machine).
//!
//! ```text
//! camera thread (prio 6)                        inference thread (prio 3)
//!   next_frame() ── PPA scale ─▶ preview back buffer ─▶ p4_ui_present_camera (swap)
//!               └─ if a request is pending:              ┌───────────────────────────────┐
//!                  PPA scale ─▶ detector image ─ frame ─▶ │ detect (boxes + landmarks)    │
//!                                               ◀ request │ → overlay → embed → match     │
//!   Frame dropped → buffer back to the ISP                └───────────────────────────────┘
//! ```
//!
//! While an enrollment is running (started by [`Command::StartEnroll`] from the main thread),
//! the inference thread collects embeddings of the one face in view instead of matching, and
//! reports the finished template with [`InferenceEvent::EnrollCaptured`].
//!
//! The inference thread asks for a frame by handing its (single) detector buffer back, and
//! the camera fills it from the very next frame. Inference therefore always works on a frame
//! at most one sensor period old, the camera never waits for inference, and the PPA only does
//! the detector downscale when someone will consume it.

use std::ffi::CStr;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure, Context, Result};
use arc_swap::ArcSwap;
use biometric_core::contract::{
    DETECTOR_FORMAT, DETECTOR_HEIGHT, DETECTOR_WIDTH, EMBEDDING_DIM, FEATURE_MODEL, MNP_MODEL,
    MSR_MODEL,
};
use biometric_core::enrollment::Enrollment;
use biometric_core::geometry::{fit_rect, map_rect};
use biometric_core::matching::{closest, GroupMember, Modality, MATCH_THRESHOLD};
use biometric_core::stats::{LatencyStats, ScoreStats};
use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::{error, info, warn};

use crate::camera::Camera;
use crate::ffi;
use crate::models::ModelStore;
use crate::ppa::{image_len, DmaBuf, ImageRef, PixelFormat, Ppa, Rect, Target};

// Sensor stream; must match VideoConfig::SENSOR_* in biometrics_wrapper.cpp
const SENSOR_W: u32 = 1280;
const SENSOR_H: u32 = 960;
// Camera canvas (the image area only, centred by the UI); must match VideoConfig::VIEWPORT_*
const VIEW_W: u32 = 640;
const VIEW_H: u32 = 480;

/// Max time the camera thread waits for LVGL before skipping one preview update.
const PRESENT_LOCK_TIMEOUT_MS: u32 = 5;
/// Max time the inference thread waits for LVGL before skipping one overlay update.
const OVERLAY_LOCK_TIMEOUT_MS: u32 = 10;
const EVENT_QUEUE_DEPTH: usize = 8;
const COMMAND_QUEUE_DEPTH: usize = 2;
/// Upper bound on inference rate. Face ID doesn't need camera rate, and each request costs a
/// full-frame PPA downscale on the camera thread.
const MIN_INFERENCE_INTERVAL: Duration = Duration::from_millis(100);
/// Faces handled per frame; matches the number of overlay boxes the UI provides.
const MAX_FACES: usize = ffi::P4_UI_MAX_FACE_BOXES as usize;
/// Interval between inference performance log lines.
const STATS_LOG_INTERVAL: Duration = Duration::from_secs(10);
const INFERENCE_STACK_SIZE: usize = 32 * 1024;

/// Enrolled members, shared with the main thread: it publishes a new list after every change,
/// and the inference thread reads the current one without locking.
pub type Roster = ArcSwap<Vec<Arc<GroupMember>>>;

#[derive(Debug)]
pub enum InferenceEvent {
    /// At least one face was detected (keeps the device awake).
    FaceSeen,
    /// A detected face matched an enrolled member with cosine similarity `score`.
    Match {
        member: Arc<GroupMember>,
        score: f32,
    },
    /// An enrollment accepted another sample.
    EnrollProgress { collected: u8 },
    /// An enrollment finished: the template embedding and the feature model release that
    /// produced it.
    EnrollCaptured {
        embedding: Vec<f32>,
        model_version: Arc<str>,
    },
    /// An enrollment could not start.
    EnrollFailed(&'static str),
}

/// Requests from the main thread to the inference thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Build a template from the single face in view (replaces a running enrollment).
    StartEnroll,
    /// Abandon the running enrollment, if any.
    CancelEnroll,
}

/// The main thread's ends of the pipeline's channels.
pub struct Vision {
    pub events: Receiver<InferenceEvent>,
    pub commands: SyncSender<Command>,
}

/// Starts the camera and inference threads.
pub fn spawn(members: Arc<Roster>, nvs: EspDefaultNvsPartition) -> Result<Vision> {
    let camera = Camera::take().context("camera handle already taken")?;

    // inference → camera: an empty detector buffer means "fill me from the next frame"
    let (request_tx, request_rx) = mpsc::sync_channel::<DmaBuf>(1);
    // camera → inference: the filled detector image
    let (frame_tx, frame_rx) = mpsc::sync_channel::<DmaBuf>(1);
    let (event_tx, event_rx) = mpsc::sync_channel::<InferenceEvent>(EVENT_QUEUE_DEPTH);
    let (command_tx, command_rx) = mpsc::sync_channel::<Command>(COMMAND_QUEUE_DEPTH);

    let detector_buf = DmaBuf::new(image_len(DETECTOR_WIDTH, DETECTOR_HEIGHT, DETECTOR_FORMAT))
        .context("failed to allocate detector buffer")?;
    request_tx
        .send(detector_buf)
        .map_err(|_| anyhow!("request channel closed"))?; // first request

    spawn_on_core1(c"cam_pipeline", 8 * 1024, 6, move || {
        camera_loop(camera, request_rx, frame_tx)
    })?;
    spawn_on_core1(c"inference", INFERENCE_STACK_SIZE, 3, move || {
        inference_loop(frame_rx, request_tx, event_tx, command_rx, members, nvs)
    })?;
    Ok(Vision {
        events: event_rx,
        commands: command_tx,
    })
}

fn spawn_on_core1<F>(name: &'static CStr, stack_size: usize, priority: u8, f: F) -> Result<()>
where
    F: FnOnce() + Send + 'static,
{
    ThreadSpawnConfiguration {
        name: Some(name),
        stack_size,
        priority,
        pin_to_core: Some(Core::Core1),
        ..Default::default()
    }
    .set()?;
    let spawned = thread::Builder::new().stack_size(stack_size).spawn(f);
    // Restore defaults so later std::thread::spawn calls aren't pinned/prioritised.
    ThreadSpawnConfiguration::default().set()?;
    spawned?;
    Ok(())
}

/// Points the preview canvas back at C's own buffer when the camera thread exits,
/// so LVGL never renders from a freed preview buffer.
struct PreviewGuard;

impl Drop for PreviewGuard {
    fn drop(&mut self) {
        // Safety: NULL restores the C-owned canvas buffer; 0 = wait for the LVGL lock.
        unsafe { ffi::p4_ui_present_camera(core::ptr::null(), 0) };
    }
}

fn camera_loop(mut camera: Camera, requests: Receiver<DmaBuf>, frames: SyncSender<DmaBuf>) {
    let mut ppa = match Ppa::new() {
        Ok(ppa) => ppa,
        Err(e) => return error!("[Pipeline] PPA client registration failed: {e}"),
    };
    let view_len = image_len(VIEW_W, VIEW_H, PixelFormat::Rgb565);
    let (Some(a), Some(b)) = (DmaBuf::new(view_len), DmaBuf::new(view_len)) else {
        return error!("[Pipeline] failed to allocate preview buffers");
    };
    // LVGL renders from the presented buffer while we fill the other one.
    let mut previews = [a, b];
    let mut back = 0;
    // Declared after `previews` so it drops first.
    let _guard = PreviewGuard;

    // Whole sensor frame, unstretched, scaled into the 4:3 canvas: 1280×960 → 640×480. The UI
    // centres the canvas in the left column; the bars are the black screen background.
    let full_frame = Rect::full(SENSOR_W, SENSOR_H);
    let view_rect = fit_rect(SENSOR_W, SENSOR_H, VIEW_W, VIEW_H);
    let mut pending_request: Option<DmaBuf> = None;
    let mut inference_running = true;
    let mut preview_ppa = LatencyStats::default();
    let mut detector_ppa = LatencyStats::default();
    let mut frames_in_window = 0u32;
    let mut window_start = Instant::now();

    info!("[Pipeline] camera thread running on core 1");
    loop {
        let frame = match camera.next_frame() {
            Ok(frame) => frame,
            Err(e) => {
                warn!("[Pipeline] {e}");
                thread::sleep(Duration::from_millis(10));
                continue;
            }
        };
        frames_in_window += 1;
        let image = frame.image();

        // 1. Preview: full sensor frame -> 640x480 RGB565 in the back buffer.
        let preview = Target {
            buf: &mut previews[back],
            width: VIEW_W,
            height: VIEW_H,
            rect: view_rect,
            format: PixelFormat::Rgb565,
        };
        let ppa_started = Instant::now();
        let preview_result = ppa.scale_crop(image, full_frame, preview);
        preview_ppa.record(ppa_started.elapsed());
        if let Err(e) = preview_result {
            warn!("[Pipeline] preview PPA failed: {e}");
            continue;
        }

        // 2. Detector image (on request), converted from the preview area rather than the sensor
        //    frame: same field of view and pixels, a quarter of the PPA input. Runs before the
        //    swap, so LVGL is not reading this buffer yet.
        if inference_running && pending_request.is_none() {
            pending_request = requests.try_recv().ok();
        }
        if let Some(mut detector_buf) = pending_request.take() {
            let preview_image = ImageRef {
                data: previews[back].as_slice(),
                width: VIEW_W,
                height: VIEW_H,
                format: PixelFormat::Rgb565,
            };
            let detector = Target::full(
                &mut detector_buf,
                DETECTOR_WIDTH,
                DETECTOR_HEIGHT,
                DETECTOR_FORMAT,
            );
            let ppa_started = Instant::now();
            let result = ppa.scale_crop(preview_image, view_rect, detector);
            detector_ppa.record(ppa_started.elapsed());
            match result {
                Ok(()) => {
                    if frames.send(detector_buf).is_err() {
                        // Keep the preview running even if inference is unavailable.
                        warn!("[Pipeline] inference thread gone; preview continues without it");
                        inference_running = false;
                    }
                }
                Err(e) => {
                    warn!("[Pipeline] detector PPA failed: {e}");
                    pending_request = Some(detector_buf); // retry on the next frame
                }
            }
        }

        // 3. Present: swap the canvas to the back buffer; the other one is filled next frame.
        // Safety: the buffer stays alive (and unwritten) until the next swap.
        let presented = unsafe {
            ffi::p4_ui_present_camera(previews[back].as_ptr().cast(), PRESENT_LOCK_TIMEOUT_MS)
        };
        if presented {
            back ^= 1;
        }
        // Return the sensor buffer only now: re-queueing it earlier lets the ISP stream the next
        // 2.4 MB frame into PSRAM while the PPA is working, which measurably slowed every stage.
        drop(frame);

        if window_start.elapsed() >= STATS_LOG_INTERVAL {
            log_camera_stats(
                frames_in_window,
                window_start.elapsed(),
                preview_ppa.take(),
                detector_ppa.take(),
            );
            frames_in_window = 0;
            window_start = Instant::now();
        }
    }
}

fn log_camera_stats(frames: u32, window: Duration, preview: LatencyStats, detector: LatencyStats) {
    let ms = |d: Option<Duration>| d.map_or(0.0, |d| d.as_secs_f32() * 1000.0);
    info!(
        "[Pipeline] camera {:.1} fps over {:.0}s | preview PPA avg {:.1} ms max {:.1} ms | \
         detector PPA n={} avg {:.1} ms max {:.1} ms",
        frames as f32 / window.as_secs_f32(),
        window.as_secs_f32(),
        ms(preview.mean()),
        ms(preview.max()),
        detector.count(),
        ms(detector.mean()),
        ms(detector.max()),
    );
}

fn inference_loop(
    frames: Receiver<DmaBuf>,
    requests: SyncSender<DmaBuf>,
    events: SyncSender<InferenceEvent>,
    commands: Receiver<Command>,
    members: Arc<Roster>,
    nvs: EspDefaultNvsPartition,
) {
    // Each model loads from the slot the store selects, and only if that slot verifies, so a
    // missing or corrupt model disables just its stage. This thread is the only user of the
    // p4_face_* API.
    let store = match ModelStore::open(nvs) {
        Ok(store) => store,
        Err(e) => return error!("[Pipeline] face models unavailable: {e:#}"),
    };
    let detector = store
        .select(&MSR_MODEL)
        .zip(store.select(&MNP_MODEL))
        .filter(|(msr, mnp)| {
            // Safety: both partitions were verified; the labels are valid C strings.
            let ret = unsafe {
                ffi::p4_face_init_detector(msr.partition.as_ptr(), mnp.partition.as_ptr())
            };
            if ret != 0 {
                error!("[Pipeline] detector init failed: {ret}");
            }
            ret == 0
        });
    if detector.is_none() {
        return error!("[Pipeline] face detection unavailable; preview continues without it");
    }
    let embedder = store.select(&FEATURE_MODEL).filter(|feature| {
        // Safety: the partition was verified; the label is a valid C string.
        let ret = unsafe { ffi::p4_face_init_embedder(feature.partition.as_ptr()) };
        if ret != 0 {
            error!("[Pipeline] embedder init failed: {ret}");
        }
        ret == 0
    });
    // Model contract check: never compare embeddings of an unexpected length. `Some` holds
    // the feature model's release, which templates are bound to.
    let model_version: Option<Arc<str>> = match embedder.as_ref().map(|e| &e.manifest) {
        Some(manifest) => {
            // Safety: plain query, embedder is loaded.
            let len = unsafe { ffi::p4_face_embedding_len() };
            if len == EMBEDDING_DIM {
                info!(
                    "[Pipeline] recognition ready: {} ({}), {len}-d",
                    manifest.model, manifest.version
                );
                Some(Arc::from(manifest.version.as_str()))
            } else {
                error!(
                    "[Pipeline] embedding length {len} != contract {EMBEDDING_DIM}; \
                     recognition disabled, detection only"
                );
                None
            }
        }
        None => {
            error!("[Pipeline] recognition disabled, detection only");
            None
        }
    };
    if let Some(version) = &model_version {
        log_stale_templates(&members.load(), version);
    }

    let view_rect = fit_rect(SENSOR_W, SENSOR_H, VIEW_W, VIEW_H);
    let mut faces = [ffi::p4_face_t::default(); MAX_FACES];
    let mut boxes = [ffi::p4_ui_rect_t::default(); MAX_FACES];
    let mut embedding = [0.0f32; EMBEDDING_DIM];
    let mut detect_stats = LatencyStats::default();
    let mut embed_stats = LatencyStats::default();
    let mut score_stats = ScoreStats::default();
    let mut enrollment: Option<Enrollment<EMBEDDING_DIM>> = None;
    let mut frames_with_faces = 0u32;
    let mut last_stats_log = Instant::now();
    let mut stack_reported = false;

    info!("[Pipeline] inference thread running on core 1");
    while let Ok(detector_buf) = frames.recv() {
        let started = Instant::now();
        let image = detector_buf.as_slice();

        while let Ok(command) = commands.try_recv() {
            enrollment = match command {
                Command::StartEnroll if model_version.is_some() => Some(Enrollment::new()),
                Command::StartEnroll => {
                    let _ = events.try_send(InferenceEvent::EnrollFailed(
                        "Face recognition model is not loaded",
                    ));
                    None
                }
                Command::CancelEnroll => None,
            };
        }

        let face_count = match detect(image, &mut faces) {
            Ok(n) => n,
            Err(e) => {
                warn!("[Pipeline] {e}");
                0
            }
        };
        detect_stats.record(started.elapsed());

        let detected = &faces[..face_count];
        show_overlay(detected, view_rect, &mut boxes);
        if !detected.is_empty() {
            frames_with_faces += 1;
            let _ = events.try_send(InferenceEvent::FaceSeen);
        }

        if let Some(version) = &model_version {
            // Enrolling needs exactly one face: with several in view there is no telling
            // which one the admin means.
            let candidates = match (&enrollment, detected) {
                (Some(_), [single]) => core::slice::from_ref(single),
                (Some(_), _) => &[],
                (None, all) => all,
            };
            for face in candidates.iter().filter(|f| f.has_landmarks) {
                let embed_started = Instant::now();
                let result = embed(image, face, &mut embedding);
                embed_stats.record(embed_started.elapsed());
                if let Err(e) = result {
                    warn!("[Pipeline] {e}");
                    continue;
                }
                if !stack_reported {
                    log_stack_headroom();
                    stack_reported = true;
                }
                if let Some(session) = &mut enrollment {
                    if let Some(event) = enroll_sample(session, &embedding, version) {
                        let finished = matches!(event, InferenceEvent::EnrollCaptured { .. });
                        // The main thread is waiting for these, so the queue has room.
                        if events.send(event).is_err() {
                            return warn!("[Pipeline] main thread gone; inference exiting");
                        }
                        if finished {
                            enrollment = None;
                        }
                    }
                    continue;
                }
                let roster = members.load();
                if let Some((index, score)) = closest(&embedding, version, &roster) {
                    score_stats.record(score);
                    if score >= MATCH_THRESHOLD {
                        let _ = events.try_send(InferenceEvent::Match {
                            member: roster[index].clone(),
                            score,
                        });
                        break;
                    }
                }
            }
        }

        if last_stats_log.elapsed() >= STATS_LOG_INTERVAL {
            log_stats(
                detect_stats.take(),
                embed_stats.take(),
                score_stats.take(),
                frames_with_faces,
                last_stats_log.elapsed(),
            );
            frames_with_faces = 0;
            last_stats_log = Instant::now();
        }

        // Hand the buffer back: this is the request for the next (fresh) frame.
        if let Some(remaining) = MIN_INFERENCE_INTERVAL.checked_sub(started.elapsed()) {
            thread::sleep(remaining);
        }
        if requests.send(detector_buf).is_err() {
            break;
        }
    }
    warn!("[Pipeline] camera thread gone; inference thread exiting");
}

/// Feeds one live embedding to a running enrollment; returns the event to report, if any.
fn enroll_sample(
    session: &mut Enrollment<EMBEDDING_DIM>,
    embedding: &[f32; EMBEDDING_DIM],
    model_version: &Arc<str>,
) -> Option<InferenceEvent> {
    match session.add(embedding) {
        Ok(collected) => Some(match session.template() {
            Some(embedding) => InferenceEvent::EnrollCaptured {
                embedding,
                model_version: model_version.clone(),
            },
            None => InferenceEvent::EnrollProgress { collected },
        }),
        Err(e) => {
            warn!("[Pipeline] enrollment sample rejected: {e}");
            None
        }
    }
}

/// Warns about members the loaded feature model cannot recognise.
fn log_stale_templates(members: &[Arc<GroupMember>], model_version: &str) {
    let unusable = members
        .iter()
        .filter(|m| m.template(Modality::Face, model_version).is_none())
        .count();
    if unusable > 0 {
        warn!(
            "[Pipeline] {unusable} of {} members have no face template for this model release \
             and will not match; enroll them again under their name",
            members.len()
        );
    }
}

/// Runs the face detector on the RGB888 detector image; returns the number of faces written.
fn detect(image: &[u8], faces: &mut [ffi::p4_face_t; MAX_FACES]) -> Result<usize> {
    debug_assert!(image.len() >= image_len(DETECTOR_WIDTH, DETECTOR_HEIGHT, DETECTOR_FORMAT));
    let mut count = 0usize;
    // Safety: `image` holds a full detector frame; `faces` has MAX_FACES slots.
    let ret = unsafe {
        ffi::p4_face_detect(
            image.as_ptr(),
            DETECTOR_WIDTH as u16,
            DETECTOR_HEIGHT as u16,
            faces.as_mut_ptr(),
            faces.len(),
            &mut count,
        )
    };
    ensure!(ret == 0, "p4_face_detect failed: {ret}");
    Ok(count.min(faces.len()))
}

/// Aligns `face` and writes its L2-normalised embedding.
fn embed(image: &[u8], face: &ffi::p4_face_t, out: &mut [f32; EMBEDDING_DIM]) -> Result<()> {
    // Safety: `image` holds a full detector frame; `out` has EMBEDDING_DIM floats, which was
    // checked against the loaded model at startup.
    let ret = unsafe {
        ffi::p4_face_embed(
            image.as_ptr(),
            DETECTOR_WIDTH as u16,
            DETECTOR_HEIGHT as u16,
            face,
            out.as_mut_ptr(),
            out.len(),
        )
    };
    ensure!(ret == 0, "p4_face_embed failed: {ret}");
    Ok(())
}

/// Draws the detected boxes over the preview (or hides them when there are none).
fn show_overlay(
    faces: &[ffi::p4_face_t],
    view_rect: Rect,
    boxes: &mut [ffi::p4_ui_rect_t; MAX_FACES],
) {
    let mut n = 0;
    for face in faces {
        let Some(rect) = Rect::from_corners(
            face.x0,
            face.y0,
            face.x1,
            face.y1,
            DETECTOR_WIDTH,
            DETECTOR_HEIGHT,
        ) else {
            continue;
        };
        let r = map_rect(rect, DETECTOR_WIDTH, DETECTOR_HEIGHT, view_rect);
        // Canvas coordinates are at most VIEW_W × VIEW_H, so they fit in i16.
        boxes[n] = ffi::p4_ui_rect_t {
            x: r.x as i16,
            y: r.y as i16,
            w: r.w as i16,
            h: r.h as i16,
        };
        n += 1;
    }
    // Safety: `boxes[..n]` is initialised; the C side copies the values under the LVGL lock.
    // A skipped update (lock busy) is harmless: the next frame redraws the overlay.
    unsafe { ffi::p4_ui_show_faces(boxes.as_ptr(), n, OVERLAY_LOCK_TIMEOUT_MS) };
}

fn log_stats(
    detect: LatencyStats,
    embed: LatencyStats,
    scores: ScoreStats,
    frames_with_faces: u32,
    window: Duration,
) {
    let ms = |d: Option<Duration>| d.map_or(0.0, |d| d.as_secs_f32() * 1000.0);
    info!(
        "[Pipeline] inference {:.1}/s over {:.0}s | detect avg {:.1} ms max {:.1} ms | \
         embed n={} avg {:.1} ms max {:.1} ms | frames with faces {} | \
         closest template n={} min {:.2} avg {:.2} max {:.2} (threshold {MATCH_THRESHOLD})",
        detect.count() as f32 / window.as_secs_f32(),
        window.as_secs_f32(),
        ms(detect.mean()),
        ms(detect.max()),
        embed.count(),
        ms(embed.mean()),
        ms(embed.max()),
        frames_with_faces,
        scores.count(),
        scores.min().unwrap_or(0.0),
        scores.mean().unwrap_or(0.0),
        scores.max().unwrap_or(0.0),
    );
}

/// Logs the inference thread's minimum free stack after the deepest call path (embedding) ran.
fn log_stack_headroom() {
    // Safety: NULL queries the calling task.
    let free_words = unsafe { ffi::uxTaskGetStackHighWaterMark(core::ptr::null_mut()) };
    info!(
        "[Pipeline] inference stack: {} of {} bytes never used",
        free_words as usize * core::mem::size_of::<ffi::StackType_t>(),
        INFERENCE_STACK_SIZE
    );
}
