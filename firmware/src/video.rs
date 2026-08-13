use log::{info, warn};

// Direct FFI imports from C++ shim
extern "C" {
    fn p4_display_draw_bitmap(
        x_start: u16,
        y_start: u16,
        x_end: u16,
        y_end: u16,
        data: *const u16,
    ) -> i32;
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct FaceBox {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub confidence: f32,
}

pub struct VideoPipeline {
    // PSRAM-backed pixel buffer for the 640x360 scaled camera feed
    scaled_buffer: Vec<u16>,
    dst_width: usize,
    dst_height: usize,
}

impl VideoPipeline {
    pub fn new(dst_width: usize, dst_height: usize) -> Self {
        let buffer_size = dst_width * dst_height;
        info!("Allocating {} KB preview buffer in Rust...", (buffer_size * 2) / 1024);

        Self {
            scaled_buffer: vec![0u16; buffer_size],
            dst_width,
            dst_height,
        }
    }

    /// Processes a raw 1280x720 RGB565 frame, downsamples it to 640x360,
    /// overlays bounding boxes, and blits to the left half of the display.
    pub fn render_camera_half(&mut self, src_raw: &[u16], faces: &[FaceBox]) -> Result<(), i32> {
        // 1. Fast 2x Nearest-Neighbor Downsample in Rust
        self.downsample_2x(src_raw, 1280);

        // 2. Draw Green Bounding Boxes directly in the Rust buffer
        for face in faces {
            self.draw_bounding_box(face, 0x07E0, 2); // Bright Green in RGB565
        }

        // 3. Send final frame to display via thin C++ FFI
        // Blit to Left Half centered vertically: X: 0..640, Y: 180..540
        let res = unsafe {
            p4_display_draw_bitmap(
                0,
                180,
                self.dst_width as u16,
                (180 + self.dst_height) as u16,
                self.scaled_buffer.as_ptr(),
            )
        };

        if res == 0 {
            Ok(())
        } else {
            Err(res)
        }
    }

    /// Fast 2x Downscaler (1280x720 -> 640x360) using slice iteration
    fn downsample_2x(&mut self, src: &[u16], src_stride: usize) {
        for y in 0..self.dst_height {
            let src_y = y * 2;
            let src_row = &src[src_y * src_stride..(src_y + 1) * src_stride];
            let dst_row = &mut self.scaled_buffer[y * self.dst_width..(y + 1) * self.dst_width];

            for x in 0..self.dst_width {
                dst_row[x] = src_row[x * 2];
            }
        }
    }

    /// Draws a hollow rectangle onto the RGB565 buffer in Rust
    fn draw_bounding_box(&mut self, face: &FaceBox, color: u16, thickness: usize) {
        // Scale 1280x720 bounding box coordinates down to 640x360
        let rx = (face.x / 2) as usize;
        let ry = (face.y / 2) as usize;
        let rw = (face.width / 2) as usize;
        let rh = (face.height / 2) as usize;

        let buf_w = self.dst_width;
        let buf_h = self.dst_height;

        for t in 0..thickness {
            let x0 = rx.saturating_sub(t);
            let y0 = ry.saturating_sub(t);
            let x1 = (rx + rw + t).min(buf_w - 1);
            let y1 = (ry + rh + t).min(buf_h - 1);

            // Horizontal borders
            for x in x0..=x1 {
                if y0 < buf_h { self.scaled_buffer[y0 * buf_w + x] = color; }
                if y1 < buf_h { self.scaled_buffer[y1 * buf_w + x] = color; }
            }

            // Vertical borders
            for y in y0..=y1 {
                if x0 < buf_w { self.scaled_buffer[y * buf_w + x0] = color; }
                if x1 < buf_w { self.scaled_buffer[y * buf_w + x1] = color; }
            }
        }
    }
}