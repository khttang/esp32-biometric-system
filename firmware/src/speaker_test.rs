use log::{info, error};
use std::f32::consts::PI;
use std::thread::sleep;
use std::time::Duration;

use crate::ffi;

/// I2S Speaker Pin Configuration
/// (Adjust these GPIO numbers to match your ESP32-P4 board schematic)
const I2S_PORT: i32 = 0;
const SAMPLE_RATE: u32 = 16000; // 16 kHz PCM
const BCLK_GPIO: i32 = 12;      // SCK / BCLK
const WS_GPIO: i32 = 13;        // WS / LRCK
const DOUT_GPIO: i32 = 14;      // SD / DOUT

pub fn run_speaker_bringup_test() {
    info!("[Speaker Test] Initializing I2S TX hardware...");

    // 1. Initialize I2S TX driver
    let init_res = unsafe {
        ffi::init_i2s_tx_c(I2S_PORT, SAMPLE_RATE, BCLK_GPIO, WS_GPIO, DOUT_GPIO)
    };

    if init_res != 0 {
        error!("[Speaker Test] Failed to initialize I2S TX! Error: {}", init_res);
        return;
    }

    info!("[Speaker Test] Generating 440 Hz sine wave test tone...");

    // 2. Generate 1 second of 440 Hz sine wave (16-bit signed PCM)
    const TONE_FREQ: f32 = 440.0;
    const AMPLITUDE: f32 = 8000.0; // Moderate volume (~25% of full scale)
    let sample_count = SAMPLE_RATE as usize;
    let mut pcm_buffer = Vec::with_capacity(sample_count);

    for i in 0..sample_count {
        let t = i as f32 / SAMPLE_RATE as f32;
        let sample = (AMPLITUDE * (2.0 * PI * TONE_FREQ * t).sin()) as i16;
        pcm_buffer.push(sample);
    }

    info!("[Speaker Test] Playing tone for 2 seconds...");

    // 3. Play tone twice via I2S TX
    for pass in 1..=2 {
        let write_res = unsafe {
            ffi::write_i2s_tx_c(
                I2S_PORT,
                pcm_buffer.as_ptr(),
                pcm_buffer.len() as u32,
                1000, // Timeout in ms
            )
        };

        if write_res != 0 {
            error!("[Speaker Test] Failed write on pass {}! Error: {}", pass, write_res);
            break;
        }

        sleep(Duration::from_millis(100));
    }

    info!("[Speaker Test] Playback test complete.");
}