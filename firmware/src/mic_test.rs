use log::{info, error};
use std::thread::sleep;
use std::time::Duration;

use crate::ffi;

/// Default I2S Mic Configuration
/// (Adjust GPIO pins according to your board's schematic if needed)
const I2S_PORT: i32 = 0;
const SAMPLE_RATE: u32 = 16000; // 16 kHz mono
const BCLK_GPIO: i32 = 12;      // SCK / BCLK
const WS_GPIO: i32 = 13;        // WS / LRCK
const DIN_GPIO: i32 = 11;       // SD / DIN

pub fn run_inmp441_mic_bringup_test() {
    info!("[INMP441 Test] Initializing I2S MEMS Microphone...");

    let init_res = unsafe {
        ffi::init_i2s_mic_c(I2S_PORT, SAMPLE_RATE, BCLK_GPIO, WS_GPIO, DIN_GPIO)
    };

    if init_res != 0 {
        error!("[INMP441 Test] Failed to initialize I2S RX (Error code: {})", init_res);
        return;
    }

    info!("[INMP441 Test] I2S RX initialized! Listening to audio input for 10 seconds...");
    info!("[INMP441 Test] Speak, tap, or clap near the microphone to observe levels.");

    const SAMPLES_TO_READ: u32 = 512;
    let mut pcm_buffer = [0i16; 512];
    let mut bytes_read: u32 = 0;

    // Poll for ~10 seconds (50 iterations * 200ms)
    for _ in 0..50 {
        let read_res = unsafe {
            ffi::read_i2s_mic_c(
                I2S_PORT,
                pcm_buffer.as_mut_ptr(),
                SAMPLES_TO_READ,
                &mut bytes_read,
                100, // 100ms read timeout
            )
        };

        if read_res == 0 && bytes_read > 0 {
            let samples_retrieved = (bytes_read as usize) / std::mem::size_of::<i16>();
            
            // Calculate Peak Amplitude & RMS (Root Mean Square) Energy
            let mut sum_squares: f64 = 0.0;
            let mut peak_val: i16 = 0;

            for &sample in &pcm_buffer[..samples_retrieved] {
                let abs_s = sample.saturating_abs();
                if abs_s > peak_val {
                    peak_val = abs_s;
                }
                sum_squares += (sample as f64) * (sample as f64);
            }

            let rms = (sum_squares / samples_retrieved as f64).sqrt();
            
            // Render terminal visual meter (20-character width)
            let meter_len = ((peak_val as usize) * 20) / 32768;
            let meter_bar = "#".repeat(meter_len);

            info!(
                "[Mic Level] Peak: {:5} | RMS: {:6.1} | VU: [{:<20}]",
                peak_val, rms, meter_bar
            );
        } else {
            error!("[INMP441 Test] I2S read timeout or error code: {}", read_res);
        }

        sleep(Duration::from_millis(200));
    }

    info!("[INMP441 Test] Microphone bring-up test complete.");
}