use anyhow::{anyhow, Result};
use std::f32::consts::PI;

const SAMPLE_RATE_HZ: u32 = 44_100;
const I2S1_BCLK_PIN: i32 = 13;
const I2S1_WS_PIN: i32 = 12;
const I2S1_DOUT_PIN: i32 = 14;

extern "C" {
    fn init_i2s_tx_c(
        i2s_port: i32,
        sample_rate: u32,
        bclk_gpio: i32,
        ws_gpio: i32,
        dout_gpio: i32,
    ) -> i32;

    fn write_i2s_tx_c(
        i2s_port: i32,
        buffer: *const i16,
        sample_count: u32,
        timeout_ms: u32,
    ) -> i32;
}

pub struct HdmiAudioPlayer {
    i2s_port: i32,
}

impl HdmiAudioPlayer {
    pub fn new(i2s_port: i32) -> Result<Self> {
        let ret = unsafe {
            init_i2s_tx_c(
                i2s_port,
                SAMPLE_RATE_HZ,
                I2S1_BCLK_PIN,
                I2S1_WS_PIN,
                I2S1_DOUT_PIN,
            )
        };

        if ret == 0 {
            Ok(Self { i2s_port })
        } else {
            Err(anyhow!("Failed to initialize I2S TX channel: error code {}", ret))
        }
    }

    pub fn play_success_chime(&mut self) -> Result<()> {
        let mut chime_pcm = Vec::new();
        Self::generate_sine_wave(880.0, 0.15, &mut chime_pcm);
        Self::generate_sine_wave(1760.0, 0.25, &mut chime_pcm);

        let ret = unsafe {
            write_i2s_tx_c(
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
        let total_samples = (SAMPLE_RATE_HZ as f32 * duration_sec) as usize;
        let amplitude = 12000.0;
        for i in 0..total_samples {
            let t = i as f32 / SAMPLE_RATE_HZ as f32;
            let envelope = 1.0 - (i as f32 / total_samples as f32);
            let val = (amplitude * envelope * (2.0 * PI * freq_hz * t).sin()) as i16;
            output.push(val); // Left
            output.push(val); // Right
        }
    }
}