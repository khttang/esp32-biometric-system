use anyhow::{Result};
use esp_idf_sys::{
    esp_deep_sleep_enable_gpio_wakeup, esp_deep_sleep_start, gpio_config, gpio_config_t,
    gpio_int_type_t_GPIO_INTR_DISABLE, gpio_mode_t_GPIO_MODE_INPUT,
    gpio_pulldown_t_GPIO_PULLDOWN_DISABLE, gpio_pullup_t_GPIO_PULLUP_ENABLE,
    esp_deepsleep_gpio_wake_up_mode_t_ESP_GPIO_WAKEUP_GPIO_LOW,
};
use log::{info, warn};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Thread-safe tracker for system activity timestamps
#[derive(Clone)]
pub struct InactivityTimer {
    last_activity: Arc<AtomicU32>,
    start_time: Instant,
}

impl InactivityTimer {
    pub fn new() -> Self {
        Self {
            last_activity: Arc::new(AtomicU32::new(0)),
            start_time: Instant::now(),
        }
    }

    /// Reset inactivity watchdog (call on touch or button events)
    pub fn reset(&self) {
        let elapsed_secs = self.start_time.elapsed().as_secs();
        self.last_activity.store(elapsed_secs.try_into().unwrap(), Ordering::Relaxed);
    }

    /// Returns seconds elapsed since last registered user action
    pub fn idle_duration_secs(&self) -> u64 {
        let current = self.start_time.elapsed().as_secs();
        let last = self.last_activity.load(Ordering::Relaxed);
        current.saturating_sub(last.into())
    }
}

/// Spawns a background thread that monitors idle time and triggers sleep transition
pub fn spawn_inactivity_watchdog(
    timer: InactivityTimer,
    timeout_secs: u64,
    gt911_lp_pin: i32,
    button_lp_pin: i32,
) {
    thread::spawn(move || {
        info!(
            "[Power] Inactivity watchdog started (Timeout: {}s)",
            timeout_secs
        );

        loop {
            thread::sleep(Duration::from_secs(1));

            let idle = timer.idle_duration_secs();
            if idle >= timeout_secs {
                warn!(
                    "[Power] Inactivity timeout reached ({}s idle). Transitioning to Deep Sleep...",
                    idle
                );
                
                if let Err(e) = arm_lp_gpios_and_sleep(gt911_lp_pin, button_lp_pin) {
                    log::error!("[Power] Failed to enter deep sleep: {:?}", e);
                }
            }
        }
    });
}

/// Configures GT911 INT & Admin Button pins as LP GPIO sources and enters ESP32-P4 Deep Sleep
pub fn arm_lp_gpios_and_sleep(gt911_pin: i32, button_pin: i32) -> Result<()> {
    info!("[Power] Configuring LP GPIOs for Deep Sleep wakeup...");

    unsafe {
        // 1. Configure GT911 INT pin (Pull-Up, Input Mode)
        let cfg_touch = gpio_config_t {
            pin_bit_mask: 1u64 << gt911_pin,
            mode: gpio_mode_t_GPIO_MODE_INPUT,
            pull_up_en: gpio_pullup_t_GPIO_PULLUP_ENABLE,
            pull_down_en: gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
            intr_type: gpio_int_type_t_GPIO_INTR_DISABLE,
            ..Default::default()
        };
        if gpio_config(&cfg_touch) != 0 {
            anyhow::bail!("Failed to configure GT911 LP GPIO pin");
        }

        // 2. Configure Admin Button pin (Pull-Up, Input Mode)
        let cfg_button = gpio_config_t {
            pin_bit_mask: 1u64 << button_pin,
            mode: gpio_mode_t_GPIO_MODE_INPUT,
            pull_up_en: gpio_pullup_t_GPIO_PULLUP_ENABLE,
            pull_down_en: gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
            intr_type: gpio_int_type_t_GPIO_INTR_DISABLE,
            ..Default::default()
        };
        if gpio_config(&cfg_button) != 0 {
            anyhow::bail!("Failed to configure Admin Button LP GPIO pin");
        }

        // 3. Set Wakeup Mask on LOW (0) level trigger
        let pin_mask = (1u64 << gt911_pin) | (1u64 << button_pin);
        esp_deep_sleep_enable_gpio_wakeup(pin_mask, esp_deepsleep_gpio_wake_up_mode_t_ESP_GPIO_WAKEUP_GPIO_LOW);

        info!("[Power] Deep sleep source armed. Flashing UART buffer before sleep...");
        thread::sleep(Duration::from_millis(50));

        // 4. Shut down high-performance cores
        esp_deep_sleep_start();
    }

    Ok(())
}