use anyhow::Result;
use biometric_core::stats::AudioLevel;
use log::{error, info, warn};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::thread;

use crate::ffi;

pub const AUDIO_FRAME_SAMPLES: usize = 512;
const SAMPLE_RATE_HZ: u64 = 16_000;
const CAPTURE_STACK_SIZE: usize = 8 * 1024;
/// The microphone level is logged once per this many samples (10 s).
const LEVEL_LOG_SAMPLES: u64 = 10 * SAMPLE_RATE_HZ;
/// ~0.5s of 16 kHz audio buffered before new frames are dropped
pub const AUDIO_QUEUE_DEPTH: usize = 16;

/// Fixed-size PCM frame moved through the channel by value (no per-frame heap allocation)
#[allow(dead_code)] // TODO: consumed by the voice pipeline
pub struct AudioFrame {
    pub samples: [i16; AUDIO_FRAME_SAMPLES],
    pub len: usize,
}

/// Reads one frame from the microphone; returns the number of samples read. Blocks until
/// `buffer` is full.
fn capture_frame(i2s_port: i32, buffer: &mut [i16]) -> Result<usize, i32> {
    let mut bytes_read: u32 = 0;
    // Safety: `buffer` is valid for `buffer.len()` samples and `bytes_read` is a valid
    // out-pointer, both for the duration of the call.
    let ret = unsafe {
        ffi::read_i2s_mic_c(
            i2s_port,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            &mut bytes_read,
            1000,
        )
    };

    if ret == 0 {
        Ok((bytes_read as usize) / std::mem::size_of::<i16>())
    } else {
        Err(ret)
    }
}

pub fn spawn_audio_capture_thread(i2s_port: i32, audio_tx: SyncSender<AudioFrame>) -> Result<()> {
    // The codec read path, a frame on the stack and logging need more than the 4 KB default.
    let thread = thread::Builder::new().stack_size(CAPTURE_STACK_SIZE);
    thread.spawn(move || {
        info!("[Audio] microphone capture thread running");
        let mut pcm_buffer = [0i16; AUDIO_FRAME_SAMPLES];
        let mut level = AudioLevel::default();

        loop {
            // Blocking read from DMA in C—thread yields until buffer is filled
            match capture_frame(i2s_port, &mut pcm_buffer) {
                Ok(samples_read) => {
                    level.record(&pcm_buffer[..samples_read]);
                    if level.samples() >= LEVEL_LOG_SAMPLES {
                        info!(
                            "[Audio] microphone over {} s: peak {} of 32768, rms {:.0}",
                            level.samples() / SAMPLE_RATE_HZ,
                            level.peak(),
                            level.rms().unwrap_or(0.0)
                        );
                        level = AudioLevel::default();
                    }
                    if samples_read > 0 {
                        let frame = AudioFrame {
                            samples: pcm_buffer,
                            len: samples_read,
                        };
                        match audio_tx.try_send(frame) {
                            Ok(()) | Err(TrySendError::Full(_)) => {} // drop newest frame when no consumer keeps up
                            Err(TrySendError::Disconnected(_)) => {
                                warn!("[Audio] Receiver dropped. Exiting audio worker loop.");
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    error!("[Audio] I2S mic read error code: {}", err);
                    thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    })?;
    Ok(())
}
