use log::{info, warn};
use core::slice;
use crate::system::P4CameraFrame;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct FaceBox {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub confidence: f32,
}

pub struct VideoPipeline;

impl VideoPipeline {
    pub fn new() -> Self {
        // No more CPU buffer allocations! Hardware handles it all.
        Self
    }

    /// Overlays bounding boxes on the raw hardware frame and pushes it to the PPA pipeline.
    pub fn render_camera_half(&mut self, frame: &P4CameraFrame, faces: &[FaceBox]) {
        // Draw face bounding boxes directly onto the raw 1280x960 hardware buffer.
        // We use thickness 4 here because the hardware PPA will scale the image 
        // down 50%, resulting in a final 2px border on the screen.
        for face in faces {
            self.draw_bounding_box(frame, face, 0x07E0, 4); 
        }

        // Feed the raw frame struct to the hardware PPA via C++ FFI
        unsafe {
            crate::system::ffi::update_camera_viewport(frame as *const P4CameraFrame);
        }
    }

    /// Draws a hollow rectangle onto the raw RGB565 V4L2 hardware buffer
    fn draw_bounding_box(&mut self, frame: &P4CameraFrame, face: &FaceBox, color: u16, thickness: usize) {
        if frame.data.is_null() || frame.height == 0 {
            return;
        }

        // Calculate exact hardware stride to prevent diagonal shearing when drawing
        let bytes_per_line = frame.data_len / (frame.height as usize);
        let pixels_per_line = bytes_per_line / 2;

        let buf_w = pixels_per_line;
        let buf_h = frame.height as usize;

        // Use native 1x coordinates since we are drawing before the PPA downscales
        let rx = face.x as usize;
        let ry = face.y as usize;
        let rw = face.width as usize;
        let rh = face.height as usize;

        // Safely map the raw C pointer to a mutable Rust slice
        let pixels = unsafe { 
            slice::from_raw_parts_mut(frame.data as *mut u16, frame.data_len / 2)
        };

        for t in 0..thickness {
            let x0 = rx.saturating_sub(t);
            let y0 = ry.saturating_sub(t);
            let x1 = (rx + rw + t).min(buf_w - 1);
            let y1 = (ry + rh + t).min(buf_h - 1);

            // Horizontal borders
            for x in x0..=x1 {
                if y0 < buf_h && x < buf_w { pixels[y0 * buf_w + x] = color; }
                if y1 < buf_h && x < buf_w { pixels[y1 * buf_w + x] = color; }
            }

            // Vertical borders
            for y in y0..=y1 {
                if x0 < buf_w && y < buf_h { pixels[y * buf_w + x0] = color; }
                if x1 < buf_w && y < buf_h { pixels[y * buf_w + x1] = color; }
            }
        }
    }
}