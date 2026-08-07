use anyhow::{Context, Result};
use esp_idf_svc::hal::gpio::{AnyIOPin, Input, InterruptType, PinDriver, Pull};
use log::{debug, info};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use crate::ffi;

const GT911_I2C_ADDR: u16 = 0x5D;
const GT911_REG_POINT_STATUS: u16 = 0x814E;
const GT911_REG_COORD_START: u16 = 0x814F;

#[derive(Debug, Clone, Copy, Default)]
pub struct TouchPoint {
    pub x: u16,
    pub y: u16,
    pub track_id: u8,
}

pub struct GT911Driver;

impl GT911Driver {
    pub fn new() -> Result<Self> {
        let ret = unsafe { ffi::init_gt911_device() };
        if ret != 0 {
            anyhow::bail!("Failed to register GT911 device on I2C bus: {}", ret);
        }
        Ok(Self)
    }

    fn read_regs(&mut self, reg: u16, buffer: &mut [u8]) -> Result<()> {
        let ret = unsafe { ffi::gt911_i2c_read(reg, buffer.as_mut_ptr(), buffer.len()) };
        if ret != 0 {
            anyhow::bail!("I2C Read error: {}", ret);
        }
        Ok(())
    }

    fn write_reg(&mut self, reg: u16, value: u8) -> Result<()> {
        let ret = unsafe { ffi::gt911_i2c_write(reg, value) };
        if ret != 0 {
            anyhow::bail!("I2C Write error: {}", ret);
        }
        Ok(())
    }

    /// Polls status register (0x814E), reads touch point if ready, and clears the interrupt.
    pub fn poll_and_clear_interrupt(&mut self) -> Result<Option<TouchPoint>> {
        let mut status = [0u8; 1];
        self.read_regs(0x814E, &mut status)?;

        let buffer_status = status[0];
        let buffer_ready = (buffer_status & 0x80) != 0;
        let touch_count = buffer_status & 0x0F;

        if !buffer_ready {
            return Ok(None);
        }

        let point = if touch_count > 0 {
            let mut data = [0u8; 7];
            // Read Track ID (0x814F), X_LOW (0x8150), X_HIGH (0x8151), Y_LOW (0x8152), Y_HIGH (0x8153)
            self.read_regs(0x814F, &mut data)?;

            let track_id = data[0];
            let x = u16::from_le_bytes([data[1], data[2]]);
            let y = u16::from_le_bytes([data[3], data[4]]);

            Some(TouchPoint { x, y, track_id })
        } else {
            None
        };

        // Clear the buffer/interrupt by writing 0x00 back to register 0x814E
        self.write_reg(0x814E, 0x00)?;

        Ok(point)
    }
}

pub struct TouchInterruptHandler<'a> {
    _int_pin: PinDriver<'a, Input>,
    touch_flag: Arc<AtomicBool>,
}

impl<'a> TouchInterruptHandler<'a> {
    /// Configure the GT911 INT pin for falling-edge interrupt
    pub fn new(pin: AnyIOPin<'a>, touch_flag: Arc<AtomicBool>) -> Result<Self> {
        let mut int_pin = PinDriver::input(pin, Pull::Up).context("Failed to configure INT pin")?;

        // GT911 pulls INT LOW on touch
        int_pin
            .set_interrupt_type(InterruptType::NegEdge)
            .context("Failed to set NegEdge interrupt")?;

        let flag_clone = Arc::clone(&touch_flag);

        unsafe {
            int_pin
                .subscribe(move || {
                    // Set flag inside ISR (lock-free)
                    flag_clone.store(true, Ordering::Relaxed);
                })
                .context("Failed to subscribe ISR to GT911 INT pin")?;
        }

        int_pin
            .enable_interrupt()
            .context("Failed to enable INT interrupt")?;

        info!("[Touch] Runtime GPIO interrupt registered on GT911 INT pin.");
        Ok(Self {
            _int_pin: int_pin,
            touch_flag,
        })
    }
}

/// Helper function to start a background worker thread that listens for interrupts
pub fn start_touch_monitoring_loop(
    mut touch_driver: GT911Driver,
    int_pin: AnyIOPin<'static>,
    inactivity_timer: crate::power::InactivityTimer,
    on_touch: impl Fn(TouchPoint) + Send + 'static,
) -> Result<()> {
    let touch_flag = Arc::new(AtomicBool::new(false));
    let _handler = TouchInterruptHandler::new(int_pin, Arc::clone(&touch_flag))?;

    thread::spawn(move || {
        info!("[Touch Loop] Active monitoring worker thread started.");
        loop {
            if touch_flag.swap(false, Ordering::Relaxed) {
                // User touched glass -> reset inactivity timer
                inactivity_timer.reset();

                if let Ok(Some(point)) = touch_driver.poll_and_clear_interrupt() {
                    on_touch(point);
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
    });

    Ok(())
}