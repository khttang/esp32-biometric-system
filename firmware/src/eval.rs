//! On-board evaluation harness (cargo feature `eval`).
//!
//! Replaces the camera pipeline with a server on the console UART: the host
//! (`crates/face-eval`) sends images, and for each one the board runs the real detector and
//! every loaded feature model and returns the embeddings. Accuracy is then computed on the
//! host, from embeddings produced by the same quantised models, on the same chip, as in the
//! field. The wire format is in `biometric_core::eval_protocol`.
//!
//! Feature models: the active one from the regular A/B slots, plus an optional candidate in
//! the `eval_feat` partition of `partitions-eval.csv`, so two models can be compared on
//! identical detections.

use core::ffi::CStr;
use core::time::Duration;
use std::thread;

use anyhow::{anyhow, ensure, Context, Result};
use biometric_core::contract::{
    DETECTOR_FORMAT, DETECTOR_HEIGHT, DETECTOR_WIDTH, FEATURE_MODEL, MNP_MODEL, MSR_MODEL,
};
use biometric_core::eval_protocol::{
    self, crc32, Request, MODEL_PREFIX, READY_PREFIX, REQUEST_HEADER_LEN, REQUEST_MAGIC,
    TRANSFER_BAUD,
};
use biometric_core::geometry::image_len;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use log::warn;

use crate::ffi;
use crate::models::{self, ModelStore};

const UART: ffi::uart_port_t = 0;
/// Driver receive buffer; the reader drains it faster than the UART fills it.
const RX_BUFFER_LEN: i32 = 64 * 1024;
const CANDIDATE_PARTITION: &CStr = c"eval_feat";
/// ESP-DL inference needs far more stack than the main task has.
const STACK_SIZE: usize = 32 * 1024;
const MAX_FACES: usize = ffi::P4_UI_MAX_FACE_BOXES as usize;
/// Longest wait for the rest of an image once its header has arrived.
const PIXEL_TIMEOUT: Duration = Duration::from_secs(5);

type EmbedFn =
    unsafe extern "C" fn(*const u8, u16, u16, *const ffi::p4_face_t, *mut f32, usize) -> i32;

struct Embedder {
    run: EmbedFn,
    output: Vec<f32>,
}

/// Serves evaluation requests; returns only if setup fails.
pub fn run() -> Result<()> {
    thread::Builder::new()
        .stack_size(STACK_SIZE)
        .spawn(serve)?
        .join()
        .map_err(|_| anyhow!("evaluation thread panicked"))?
}

fn serve() -> Result<()> {
    let store = ModelStore::open(EspDefaultNvsPartition::take()?)?;
    let (msr, mnp) = store
        .select(&MSR_MODEL)
        .zip(store.select(&MNP_MODEL))
        .context("detector models unavailable")?;
    // Safety: both partitions were verified; the labels are valid C strings. This thread is
    // the only user of the p4_face_* API.
    let ret = unsafe { ffi::p4_face_init_detector(msr.partition.as_ptr(), mnp.partition.as_ptr()) };
    ensure!(ret == 0, "detector init failed: {ret}");

    let mut names = Vec::new();
    let mut embedders = Vec::new();

    let active = store
        .select(&FEATURE_MODEL)
        .context("feature model unavailable")?;
    // Safety: as above.
    let ret = unsafe { ffi::p4_face_init_embedder(active.partition.as_ptr()) };
    ensure!(ret == 0, "embedder init failed: {ret}");
    names.push(active.manifest.model);
    embedders.push(Embedder {
        run: ffi::p4_face_embed,
        // Safety: plain query.
        output: vec![0.0; unsafe { ffi::p4_face_embedding_len() }],
    });

    match models::verify_any(CANDIDATE_PARTITION) {
        Ok(manifest) => {
            // Safety: the partition was verified.
            let ret = unsafe { ffi::p4_face_init_candidate_embedder(CANDIDATE_PARTITION.as_ptr()) };
            ensure!(ret == 0, "candidate embedder init failed: {ret}");
            names.push(manifest.model);
            embedders.push(Embedder {
                run: ffi::p4_face_embed_candidate,
                // Safety: plain query.
                output: vec![0.0; unsafe { ffi::p4_face_candidate_embedding_len() }],
            });
        }
        Err(e) => warn!("[Eval] no candidate feature model: {e:#}"),
    }
    ensure!(
        embedders.iter().all(|e| !e.output.is_empty()),
        "a feature model reports an empty embedding"
    );

    // Safety: installs the driver on the console UART; logging keeps working alongside it.
    let ret =
        unsafe { ffi::uart_driver_install(UART, RX_BUFFER_LEN, 0, 0, core::ptr::null_mut(), 0) };
    ensure!(ret == 0, "UART driver install failed: {ret}");

    for (index, name) in names.iter().enumerate() {
        write_line(&format!("{MODEL_PREFIX} {index} {name}"));
    }
    write_line(&format!(
        "{READY_PREFIX} baud={TRANSFER_BAUD} models={}",
        names.len()
    ));
    // Only warnings and errors from here on, so log lines rarely interleave with responses.
    // Safety: plain calls; the ready line is fully sent before the baud rate changes.
    unsafe {
        ffi::esp_log_level_set(c"*".as_ptr(), ffi::esp_log_level_t_ESP_LOG_WARN);
        ffi::uart_wait_tx_done(UART, ticks(Duration::from_secs(1)));
        ffi::uart_set_baudrate(UART, TRANSFER_BAUD);
    }

    let mut pixels = vec![0u8; image_len(DETECTOR_WIDTH, DETECTOR_HEIGHT, DETECTOR_FORMAT)];
    let mut line = String::new();
    loop {
        line.clear();
        match receive(&mut pixels) {
            Ok(request) => {
                let len = request.pixel_bytes().unwrap_or(0);
                respond(&mut line, &request, &pixels[..len], &mut embedders);
            }
            Err(reason) => {
                // Drop whatever is left of a broken request so the next one starts clean.
                // Safety: plain call.
                unsafe { ffi::uart_flush_input(UART) };
                eval_protocol::encode_error(&mut line, reason);
            }
        }
        write_line(&line);
    }
}

/// Waits for a request and reads its image into `pixels`.
fn receive(pixels: &mut [u8]) -> Result<Request, &'static str> {
    // Resynchronise on the magic: bytes before it are line noise or a stale request.
    let mut header = [0u8; REQUEST_HEADER_LEN];
    while header[..4] != REQUEST_MAGIC {
        header.copy_within(1..4, 0);
        read_exact(&mut header[3..4], None);
    }
    if !read_exact(&mut header[4..], Some(PIXEL_TIMEOUT)) {
        return Err("header timeout");
    }
    let request = Request::decode(&header).map_err(|_| "bad header")?;
    let len = request.pixel_bytes().map_err(|_| "bad header")?;
    let pixels = pixels.get_mut(..len).ok_or("image too large")?;
    if !read_exact(pixels, Some(PIXEL_TIMEOUT)) {
        return Err("image timeout");
    }
    if crc32(pixels) != request.crc {
        return Err("crc mismatch");
    }
    Ok(request)
}

/// Detects the largest face in the image and appends the response line.
fn respond(line: &mut String, request: &Request, pixels: &[u8], embedders: &mut [Embedder]) {
    let mut faces = [ffi::p4_face_t::default(); MAX_FACES];
    let mut count = 0usize;
    // Safety: `pixels` holds the whole image; `faces` has MAX_FACES slots.
    let ret = unsafe {
        ffi::p4_face_detect(
            pixels.as_ptr(),
            request.width,
            request.height,
            faces.as_mut_ptr(),
            faces.len(),
            &mut count,
        )
    };
    if ret != 0 {
        return eval_protocol::encode_error(line, "detect failed");
    }
    let area = |f: &ffi::p4_face_t| i64::from(f.x1 - f.x0) * i64::from(f.y1 - f.y0);
    let Some(face) = faces[..count.min(MAX_FACES)]
        .iter()
        .filter(|f| f.has_landmarks)
        .max_by_key(|f| area(f))
    else {
        return eval_protocol::encode_no_face(line);
    };
    for embedder in embedders.iter_mut() {
        // Safety: `pixels` holds the whole image; `output` has the model's embedding length.
        let ret = unsafe {
            (embedder.run)(
                pixels.as_ptr(),
                request.width,
                request.height,
                face,
                embedder.output.as_mut_ptr(),
                embedder.output.len(),
            )
        };
        if ret != 0 {
            return eval_protocol::encode_error(line, "embed failed");
        }
    }
    // At most two feature models.
    let mut outputs: [&[f32]; 2] = [&[]; 2];
    for (slot, embedder) in outputs.iter_mut().zip(embedders.iter()) {
        *slot = &embedder.output;
    }
    eval_protocol::encode_embeddings(line, face.score, &outputs[..embedders.len().min(2)]);
}

/// Fills `buf` from the UART; `false` if `timeout` passed first. `None` waits forever.
fn read_exact(buf: &mut [u8], timeout: Option<Duration>) -> bool {
    let wait = timeout.map_or(ffi::TickType_t::MAX, ticks);
    // Safety: `buf` is valid for `buf.len()` bytes; the driver is installed.
    let read =
        unsafe { ffi::uart_read_bytes(UART, buf.as_mut_ptr().cast(), buf.len() as u32, wait) };
    read == buf.len() as i32
}

fn write_line(line: &str) {
    // Safety: both buffers are valid for the given lengths; the driver is installed.
    unsafe {
        ffi::uart_write_bytes(UART, line.as_ptr().cast(), line.len());
        ffi::uart_write_bytes(UART, c"\n".as_ptr().cast(), 1);
    }
}

fn ticks(duration: Duration) -> ffi::TickType_t {
    (duration.as_millis() as u64 * u64::from(ffi::configTICK_RATE_HZ) / 1000) as ffi::TickType_t
}
