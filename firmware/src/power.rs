// src/power.rs

use esp_idf_sys::{
    esp_deep_sleep_enable_gpio_wakeup, esp_sleep_enable_timer_wakeup,
    esp_deep_sleep_start, esp_deepsleep_gpio_wake_up_mode_t_ESP_GPIO_WAKEUP_GPIO_LOW,
    esp_restart, fflush, gpio_config, gpio_config_t, gpio_int_type_t_GPIO_INTR_DISABLE,
    gpio_mode_t_GPIO_MODE_INPUT, gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
    gpio_pullup_t_GPIO_PULLUP_ENABLE,
};
use log::{error, info, warn};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

// Retained in RTC RAM across software resets (cleared on power cycle or explicit reset)
#[no_mangle]
#[link_section = ".rtc.data"]
static mut RTC_BOOT_CRASH_COUNT: u32 = 0;

// Default LP GPIO pins (matched to hardware schematic)
pub const DEFAULT_GT911_INT_LP_GPIO: i32 = 0;
pub const DEFAULT_ADMIN_BUTTON_LP_GPIO: i32 = 1;

// =============================================================================
// 1. Inactivity Tracking
// =============================================================================

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
        self.last_activity
            .store(elapsed_secs.try_into().unwrap_or(u32::MAX), Ordering::Relaxed);
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
        info!("[Power] Inactivity watchdog active (Timeout: {}s)", timeout_secs);

        loop {
            thread::sleep(Duration::from_secs(1));

            let idle = timer.idle_duration_secs();
            if idle >= timeout_secs {
                warn!(
                    "[Power] Inactivity timeout reached ({}s idle). Entering Deep Sleep...",
                    idle
                );
                enter_deep_sleep(None);
            }
        }
    });
}

// =============================================================================
// 2. Boot Crash Counter Management
// =============================================================================

/// Resets the RTC crash counter to 0 upon successful initialization
pub fn reset_boot_crash_counter() {
    unsafe {
        RTC_BOOT_CRASH_COUNT = 0;
    }
}

/// Hybrid fatal error handler: Panics in Dev mode; retries/sleeps in Prod mode
pub fn handle_fatal_init_error(err: anyhow::Error) -> ! {
    // DEVELOPMENT MODE (`cargo build` / `debug_assertions` active)
    if cfg!(debug_assertions) {
        error!("==================================================");
        error!(" [DEV MODE] Fatal Hardware Initialization Error!");
        error!(" Details: {:?}", err);
        error!("==================================================");

        panic!("Development Hardware Init Failure: {:?}", err);
    }

    // PRODUCTION MODE (`cargo build --release`)
    unsafe {
        RTC_BOOT_CRASH_COUNT += 1;
        let crash_count = RTC_BOOT_CRASH_COUNT;

        error!("==================================================");
        error!(" [PROD MODE] Hardware Init Failed (Attempt {}/3)", crash_count);
        error!(" Error: {}", err);
        error!("==================================================");

        if crash_count < 3 {
            // Give power rails 2 seconds to settle before retrying
            thread::sleep(Duration::from_secs(2));
            esp_restart();
            loop {}
        } else {
            error!("CRITICAL: Max boot retries reached. Sleeping 1hr to preserve battery.");
            
            // Sleep for 1 hour OR until GPIO wake event (button/touch)
            enter_deep_sleep(Some(Duration::from_secs(3600)));
        }
    }
}

// =============================================================================
// 3. Unified Deep Sleep Entry
// =============================================================================

/// Configures LP GPIO pull-ups, arms wake triggers, flushes UART, and enters Deep Sleep (`-> !`)
pub fn enter_deep_sleep(timer_wakeup: Option<Duration>) -> ! {
    info!("[Power] Preparing device for Deep Sleep transition...");

    unsafe {
        // 1. Configure GT911 INT pin (Pull-Up, Input Mode)
        let cfg_touch = gpio_config_t {
            pin_bit_mask: 1u64 << DEFAULT_GT911_INT_LP_GPIO,
            mode: gpio_mode_t_GPIO_MODE_INPUT,
            pull_up_en: gpio_pullup_t_GPIO_PULLUP_ENABLE,
            pull_down_en: gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
            intr_type: gpio_int_type_t_GPIO_INTR_DISABLE,
            ..Default::default()
        };
        if gpio_config(&cfg_touch) != 0 {
            warn!("[Power] Failed to configure GT911 LP GPIO pin pull-up");
        }

        // 2. Configure Admin Button pin (Pull-Up, Input Mode)
        let cfg_button = gpio_config_t {
            pin_bit_mask: 1u64 << DEFAULT_ADMIN_BUTTON_LP_GPIO,
            mode: gpio_mode_t_GPIO_MODE_INPUT,
            pull_up_en: gpio_pullup_t_GPIO_PULLUP_ENABLE,
            pull_down_en: gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
            intr_type: gpio_int_type_t_GPIO_INTR_DISABLE,
            ..Default::default()
        };
        if gpio_config(&cfg_button) != 0 {
            warn!("[Power] Failed to configure Admin Button LP GPIO pin pull-up");
        }

        // 3. Enable GPIO Wakeup on active-LOW
        let pin_mask = (1u64 << DEFAULT_GT911_INT_LP_GPIO) | (1u64 << DEFAULT_ADMIN_BUTTON_LP_GPIO);
        esp_deep_sleep_enable_gpio_wakeup(
            pin_mask,
            esp_deepsleep_gpio_wake_up_mode_t_ESP_GPIO_WAKEUP_GPIO_LOW,
        );

        // 4. (Optional) Enable periodic timer wake-up
        if let Some(dur) = timer_wakeup {
            let sleep_us = dur.as_micros() as u64;
            esp_sleep_enable_timer_wakeup(sleep_us);
            info!("[Power] Timer wake-up set for {}s", dur.as_secs());
        }

        info!("[Power] Power domains shutting down NOW.");
        fflush(std::ptr::null_mut());

        // 5. Trigger Deep Sleep (Does not return)
        esp_deep_sleep_start();
    }

    // Unreachable loop for compiler divergence (`-> !`)
    loop {}
}