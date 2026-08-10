use log::{info, warn, error};
use std::thread::sleep;
use std::time::Duration;

// Assuming bindings module is `crate::ffi`
use crate::ffi;

/// GT911 Registers
const GT911_REG_PRODUCT_ID: u16 = 0x8140;
const GT911_REG_COORD_ADDR: u16 = 0x814E;

pub fn run_gt911_bringup_test() {
    info!("[GT911 Test] Initializing I2C bus & GT911 hardware...");

    // 1. Initialize I2C and GT911 hardware
    unsafe {
        if ffi::init_i2c_bus() != 0 {
            error!("[GT911 Test] Failed to initialize I2C bus!");
            return;
        }

        if ffi::init_gt911_device() != 0 {
            error!("[GT911 Test] Failed to initialize GT911 device!");
            return;
        }
    }

    // 2. Read Product ID (4 bytes ASCII e.g., "911", "9110", etc.)
    let mut pid_buf = [0u8; 4];
    let read_res = unsafe {
        ffi::gt911_i2c_read(GT911_REG_PRODUCT_ID, pid_buf.as_mut_ptr(), pid_buf.len())
    };

    if read_res == 0 {
        let product_id = String::from_utf8_lossy(&pid_buf);
        info!("[GT911 Test] GT911 Product ID: {}", product_id);
    } else {
        error!("[GT911 Test] Failed to read GT911 Product ID!");
        return;
    }

    info!("[GT911 Test] Touch the screen! Polling coordinates for 30 seconds...");

    // 3. Poll touch status and point 1 coordinates
    for _ in 0..1500 { // Poll for ~30 seconds (1500 * 20ms)
        let mut status: u8 = 0;

        // Read touch buffer status (0x814E)
        let res = unsafe { ffi::gt911_i2c_read(GT911_REG_COORD_ADDR, &mut status, 1) };

        if res == 0 {
            let buffer_ready = (status & 0x80) != 0;
            let point_count = status & 0x0F;

            if buffer_ready && point_count > 0 {
                // Point 1 data starts at 0x814F (8 bytes: Track ID, X_Low, X_High, Y_Low, Y_High, Size_Low, Size_High, Reserved)
                let mut point_buf = [0u8; 8];
                let pt_res = unsafe {
                    ffi::gt911_i2c_read(GT911_REG_COORD_ADDR + 1, point_buf.as_mut_ptr(), point_buf.len())
                };

                if pt_res == 0 {
                    let track_id = point_buf[0];
                    let x = u16::from_le_bytes([point_buf[1], point_buf[2]]);
                    let y = u16::from_le_bytes([point_buf[3], point_buf[4]]);
                    let size = u16::from_le_bytes([point_buf[5], point_buf[6]]);

                    info!("[Touch Event] ID: {}, X: {}, Y: {}, Size: {}", track_id, x, y, size);
                }

                // CRITICAL FOR GT911: Must write 0x00 back to 0x814E to clear buffer status bit
                let clear_byte: u8 = 0;
                unsafe {
                    ffi::gt911_i2c_write(GT911_REG_COORD_ADDR, clear_byte);
                }
            } else if buffer_ready {
                // Buffer was ready but no touch points active (release event); clear buffer
                let clear_byte: u8 = 0;
                unsafe {
                    ffi::gt911_i2c_write(GT911_REG_COORD_ADDR, clear_byte);
                }
            }
        }

        sleep(Duration::from_millis(20)); // GT911 refreshes at ~50–100Hz
    }

    info!("[GT911 Test] Test complete.");
}