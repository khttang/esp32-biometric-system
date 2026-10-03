use anyhow::Result;
use log::{error, info, warn};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::thread;

use crate::ffi;

pub const AUDIO_FRAME_SAMPLES: usize = 512;
/// ~0.5s of 16 kHz audio buffered before new frames are dropped
pub const AUDIO_QUEUE_DEPTH: usize = 16;

/// Fixed-size PCM frame moved through the channel by value (no per-frame heap allocation)
#[allow(dead_code)] // TODO: consumed by the voice pipeline
pub struct AudioFrame {
    pub samples: [i16; AUDIO_FRAME_SAMPLES],
    pub len: usize,
}

pub struct AudioWorker {
    port: i32,
}

impl AudioWorker {
    pub fn new(port: i32) -> Self {
        Self { port }
    }

    pub fn capture_frame(&self, buffer: &mut [i16]) -> Result<usize, i32> {
        let mut bytes_read: u32 = 0;
        let ret = unsafe {
            ffi::read_i2s_mic_c(
                self.port,
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
}

pub fn spawn_audio_capture_thread(
    i2s_port: i32,
    audio_tx: SyncSender<AudioFrame>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let worker = AudioWorker::new(i2s_port);

        info!("[Audio] I2S MEMS Microphone capture thread running...");
        let mut pcm_buffer = [0i16; AUDIO_FRAME_SAMPLES];

        loop {
            // Blocking read from DMA in C—thread yields until buffer is filled
            match worker.capture_frame(&mut pcm_buffer) {
                Ok(samples_read) => {
                    if samples_read > 0 {
                        let frame = AudioFrame { samples: pcm_buffer, len: samples_read };
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
    })
}