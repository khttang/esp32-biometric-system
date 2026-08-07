use anyhow::Result;
use log::{error, info};
use std::sync::mpsc::Sender;
use std::thread;

extern "C" {
    fn init_i2s_mic_c(
        i2s_port: i32,
        sample_rate: u32,
        bclk_gpio: i32,
        ws_gpio: i32,
        din_gpio: i32,
    ) -> i32;

    fn read_i2s_mic_c(
        i2s_port: i32,
        out_buffer: *mut i16,
        samples_to_read: u32,
        bytes_read: *mut u32,
        timeout_ms: u32,
    ) -> i32;
}

pub struct AudioWorker {
    port: i32,
}

impl AudioWorker {
    pub fn new(port: i32, sample_rate: u32, bclk: i32, ws: i32, din: i32) -> Result<Self, i32> {
        let ret = unsafe { init_i2s_mic_c(port, sample_rate, bclk, ws, din) };
        if ret == 0 {
            Ok(Self { port })
        } else {
            Err(ret)
        }
    }

    pub fn capture_frame(&self, buffer: &mut [i16]) -> Result<usize, i32> {
        let mut bytes_read: u32 = 0;
        let ret = unsafe {
            read_i2s_mic_c(
                self.port,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut bytes_read,
                1000,
            )
        };

        if ret == 0 {
            Ok((bytes_read / 2) as usize) // Return sample count
        } else {
            Err(ret)
        }
    }
}

pub fn spawn_audio_capture_thread(
    i2s_port: i32,
    sample_rate: u32,
    bclk: i32,
    ws: i32,
    din: i32,
    audio_tx: Sender<Vec<i16>>
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // Initialize I2S once inside the worker thread via C wrapper
        let worker = match AudioWorker::new(i2s_port, sample_rate, bclk, ws, din) {
            Ok(w) => w,
            Err(e) => {
                println!("Failed to init C I2S driver: {}", e);
                return;
            }
        };

        info!("[Audio] I2S MEMS Microphone capture thread running...");
        let mut pcm_buffer = [0i16; 512];

        loop {
            // Blocking read from DMA in C—thread yields until buffer is filled
            match worker.capture_frame(&mut pcm_buffer) {
                Ok(samples_read) => {
                    if samples_read > 0 {
                        let active_samples = pcm_buffer[..samples_read].to_vec();
                        // Send audio chunk to pipeline (e.g. wake-word or voice match engine)
                        if audio_tx.send(active_samples).is_err() {
                            log::warn!("[Audio] Receiver dropped. Exiting audio worker loop.");
                            break;
                        }
                    }
                }
                Err(err) => {
                    log::error!("[Audio] I2S mic read error code: {}", err);
                    thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    })
}