use anyhow::{anyhow, Result};
use std::f32::consts::PI;

use crate::ffi;

const AUDIO_SAMPLE_RATE: u32 = 16_000;

pub struct HdmiAudioPlayer {
    i2s_port: i32,
}

impl HdmiAudioPlayer {
pub fn new(i2s_port: i32) -> Self {
        Self { i2s_port }
    }

    pub fn play_success_chime(&mut self) -> Result<()> {
        let mut chime_pcm = Vec::new();
        Self::generate_sine_wave(880.0, 0.15, &mut chime_pcm);  // A5
        Self::generate_sine_wave(1760.0, 0.25, &mut chime_pcm); // A6

        let ret = unsafe {
            ffi::write_i2s_tx_c(
                self.i2s_port,
                chime_pcm.as_ptr(),
                chime_pcm.len() as u32,
                1000,
            )
        };

        if ret == 0 {
            Ok(())
        } else {
            Err(anyhow!("I2S write error: code {}", ret))
        }
    }

    fn generate_sine_wave(freq_hz: f32, duration_sec: f32, output: &mut Vec<i16>) {
        let total_samples = (AUDIO_SAMPLE_RATE as f32 * duration_sec) as usize;
        let amplitude = 12000.0;

        for i in 0..total_samples {
            let t = i as f32 / AUDIO_SAMPLE_RATE as f32;
            let envelope = 1.0 - (i as f32 / total_samples as f32); // Exponential decay
            let val = (amplitude * envelope * (2.0 * PI * freq_hz * t).sin()) as i16;
            output.push(val); // Mono PCM sample
        }
    }
}