//! Real-time vision pipeline, both threads pinned to Core 1 (Core 0 runs LVGL + the state machine).
//!
//! ```text
//! camera thread (prio 6)                         inference thread (prio 3)
//!   next_frame() ── PPA crop/scale ─▶ preview back buffer ─▶ p4_ui_present_camera (swap)
//!               └─ if a request is pending:                  ┌──────────────────────────┐
//!                  PPA scale ─▶ detector image ── frame ───▶ │ detect → PPA crop 112×112 │
//!                                                ◀─ request ─│ → embed → match → event   │
//!   Frame dropped → buffer back to the ISP                   └──────────────────────────┘
//! ```
//!
//! The inference thread asks for a frame by handing its (single) detector buffer back, and
//! the camera fills it from the very next frame. Inference therefore always works on a frame
//! at most one sensor period old, the camera never waits for inference, and the PPA only does
//! the detector downscale when someone will consume it.

use std::ffi::CStr;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, ensure, Context, Result};
use arc_swap::ArcSwap;
use esp_idf_svc::hal::cpu::Core;
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use log::{error, info, warn};

use crate::biometrics::{best_match, GroupMember};
use crate::camera::Camera;
use crate::ffi;
use biometric_core::geometry::fit_rect;

use crate::ppa::{image_len, DmaBuf, ImageRef, PixelFormat, Ppa, Rect, Target};

// Sensor stream; must match VideoConfig::SENSOR_* in biometrics_wrapper.cpp
const SENSOR_W: u32 = 1280;
const SENSOR_H: u32 = 960;
// Preview canvas on the left half of the screen; must match VideoConfig::VIEWPORT_*
const VIEW_W: u32 = 640;
const VIEW_H: u32 = 720;
// Detector input: full field of view at half resolution
const DET_W: u32 = 640;
const DET_H: u32 = 480;
// MobileFaceNet input
const FACE_SIZE: u32 = 112;
pub const EMBEDDING_DIM: usize = 512;

/// Max time the camera thread waits for LVGL before skipping one preview update.
const PRESENT_LOCK_TIMEOUT_MS: u32 = 5;
const EVENT_QUEUE_DEPTH: usize = 8;
/// Upper bound on inference rate. Face ID doesn't need camera rate, and each request costs a
/// full-frame PPA downscale on the camera thread.
const MIN_INFERENCE_INTERVAL: Duration = Duration::from_millis(100);

/// Face location in detector-image coordinates.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // TODO: filled by human_face_detect
pub struct FaceBox {
    pub rect: Rect,
    pub confidence: f32,
}

#[derive(Debug)]
pub enum InferenceEvent {
    /// At least one face was detected (keeps the device awake).
    FaceSeen,
    /// A detected face matched an enrolled member.
    Match(GroupMember),
}

/// Starts the camera and inference threads; returns the stream of inference events.
pub fn spawn(members: Arc<ArcSwap<Vec<GroupMember>>>) -> Result<Receiver<InferenceEvent>> {
    let camera = Camera::take().context("camera handle already taken")?;

    // inference → camera: an empty detector buffer means "fill me from the next frame"
    let (request_tx, request_rx) = mpsc::sync_channel::<DmaBuf>(1);
    // camera → inference: the filled detector image
    let (frame_tx, frame_rx) = mpsc::sync_channel::<DmaBuf>(1);
    let (event_tx, event_rx) = mpsc::sync_channel::<InferenceEvent>(EVENT_QUEUE_DEPTH);

    let detector_buf = DmaBuf::new(image_len(DET_W, DET_H, PixelFormat::Rgb888))
        .context("failed to allocate detector buffer")?;
    request_tx
        .send(detector_buf)
        .map_err(|_| anyhow!("request channel closed"))?; // first request

    spawn_on_core1(c"cam_pipeline", 8 * 1024, 6, move || {
        camera_loop(camera, request_rx, frame_tx)
    })?;
    spawn_on_core1(c"inference", 32 * 1024, 3, move || {
        inference_loop(frame_rx, request_tx, event_tx, members)
    })?;
    Ok(event_rx)
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

    // Whole sensor frame, unstretched, letterboxed in the viewport: 1280×960 → 640×480 at y=120.
    // The bars stay black because preview buffers start zeroed and the PPA never writes there.
    let full_frame = Rect::full(SENSOR_W, SENSOR_H);
    let view_rect = fit_rect(SENSOR_W, SENSOR_H, VIEW_W, VIEW_H);
    let mut pending_request: Option<DmaBuf> = None;

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
        let image = frame.image();

        let preview = Target {
            buf: &mut previews[back],
            width: VIEW_W,
            height: VIEW_H,
            rect: view_rect,
            format: PixelFormat::Rgb565,
        };
        match ppa.scale_crop(image, full_frame, preview) {
            Ok(()) => {
                // Safety: the buffer stays alive (and unwritten) until the next swap.
                let presented = unsafe {
                    ffi::p4_ui_present_camera(
                        previews[back].as_ptr().cast(),
                        PRESENT_LOCK_TIMEOUT_MS,
                    )
                };
                if presented {
                    back ^= 1;
                }
            }
            Err(e) => warn!("[Pipeline] preview PPA failed: {e}"),
        }

        if pending_request.is_none() {
            pending_request = requests.try_recv().ok();
        }
        if let Some(mut detector_buf) = pending_request.take() {
            let detector = Target::full(&mut detector_buf, DET_W, DET_H, PixelFormat::Rgb888);
            match ppa.scale_crop(image, full_frame, detector) {
                Ok(()) => {
                    if frames.send(detector_buf).is_err() {
                        return warn!("[Pipeline] inference thread gone; camera thread exiting");
                    }
                }
                Err(e) => {
                    warn!("[Pipeline] detector PPA failed: {e}");
                    pending_request = Some(detector_buf); // retry on the next frame
                }
            }
        }
        // `frame` drops here and its buffer goes back to the ISP.
    }
}

fn inference_loop(
    frames: Receiver<DmaBuf>,
    requests: SyncSender<DmaBuf>,
    events: SyncSender<InferenceEvent>,
    members: Arc<ArcSwap<Vec<GroupMember>>>,
) {
    let mut ppa = match Ppa::new() {
        Ok(ppa) => ppa,
        Err(e) => return error!("[Pipeline] PPA client registration failed: {e}"),
    };
    let Some(mut face_buf) = DmaBuf::new(image_len(FACE_SIZE, FACE_SIZE, PixelFormat::Rgb888))
    else {
        return error!("[Pipeline] failed to allocate face buffer");
    };
    let mut embedding = [0.0f32; EMBEDDING_DIM];

    info!("[Pipeline] inference thread running on core 1");
    while let Ok(detector_buf) = frames.recv() {
        let started = std::time::Instant::now();
        let image = ImageRef {
            data: detector_buf.as_slice(),
            width: DET_W,
            height: DET_H,
            format: PixelFormat::Rgb888,
        };

        let faces = detect_faces(image);
        if !faces.is_empty() {
            let _ = events.try_send(InferenceEvent::FaceSeen);
        }
        for face in &faces {
            let Some(rect) = face.rect.clamp_to(DET_W, DET_H) else {
                continue;
            };
            let face_target =
                Target::full(&mut face_buf, FACE_SIZE, FACE_SIZE, PixelFormat::Rgb888);
            if let Err(e) = ppa.scale_crop(image, rect, face_target) {
                warn!("[Pipeline] face crop failed: {e}");
                continue;
            }
            if let Err(e) = embed(&face_buf, &mut embedding) {
                warn!("[Pipeline] {e}");
                continue;
            }
            if let Some(member) = best_match(&embedding, &members.load()) {
                let _ = events.try_send(InferenceEvent::Match(member));
                break;
            }
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

/// TODO: replace with espressif/human_face_detect.
fn detect_faces(_image: ImageRef<'_>) -> Vec<FaceBox> {
    Vec::new()
}

/// Runs MobileFaceNet on a 112×112 RGB888 crop; writes an L2-normalised embedding.
fn embed(face: &DmaBuf, out: &mut [f32; EMBEDDING_DIM]) -> Result<()> {
    // Safety: `face` holds at least 112*112*3 bytes and `out` has EMBEDDING_DIM floats.
    let ret = unsafe { ffi::dl_mobilefacenet_run(face.as_ptr(), out.as_mut_ptr(), out.len()) };
    ensure!(ret == 0, "dl_mobilefacenet_run failed: {ret}");
    Ok(())
}
