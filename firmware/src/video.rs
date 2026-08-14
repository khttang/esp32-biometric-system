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
    /*     Region       X Bounds         Y Bounds    Width x Height
     *     Left Half    0 - 640          180 - 540   640 x 360
     *    Right Half    640 - 1280       0 - 720     640 x 720
     */
    pub fn new(dst_width: usize, dst_height: usize) -> Self {
        Self {
            dst_width,
            dst_height,
            scaled_buffer: vec![0u8; dst_width * dst_height * 2] // 640x360 RGB565
                .chunks_exact(2)
                .map(|_| 0u16)
                .collect(),
        }
    }

    /// Processes a raw 1280x720 RGB565 frame, downsamples it to 640x360,
    /// overlays bounding boxes, and blits to the left half of the display.
    pub fn render_camera_half(&mut self, src_raw: &[u16], faces: &[FaceBox]) -> Result<(), i32> {
        // 1. Rotate & scale to 640x360 landscape buffer
        self.rotate_90_cw(src_raw, 1280);

        // 2. Draw face bounding boxes
        for face in faces {
            self.draw_bounding_box(face, 0x07E0, 2);
        }

        // 3. Blit to entire left half (X: 0..640, Y: 180..540)
        let res = unsafe {
            p4_display_draw_bitmap(
                0,    // x_start (left edge of screen)
                180,  // y_start (centered vertically)
                640,  // x_end   (midpoint of 1280 screen)
                540,  // y_end   (180 + 360)
                self.scaled_buffer.as_ptr(),
            )
        };

        if res == 0 { Ok(()) } else { Err(res) }
    }

    /// Rotates 1280x960 camera frame 90 deg CW into a 640x360 landscape buffer
    pub fn rotate_90_cw(&mut self, src_raw: &[u16], src_stride: usize) {
        let dst_w = 640; // Horizontal Width
        let dst_h = 360; // Vertical Height

        // Center crop 540 pixels out of 1280 camera width: (1280 - 540) / 2 = 370
        let x_center_offset = 370;

        for dy in 0..dst_h {
            // Destination Y (0..360) maps to Camera X (370..910)
            let sx = x_center_offset + ((dy * 3) >> 1);
            let dst_row = dy * dst_w;

            for dx in 0..dst_w {
                // Destination X (0..640) maps to Camera Y (959..0)
                let sy = 959 - ((dx * 3) >> 1);

                self.scaled_buffer[dst_row + dx] = src_raw[(sy * src_stride) + sx];
            }
        }
    }

    /// Downsamples 1280x960 camera frame to 640x360 for Left Half Display
    pub fn downsample_2x(&mut self, src_raw: &[u16], src_stride: usize) {
        let dst_w = 640;
        let dst_h = 360;

        // Center crop vertically: start reading at row 120 to extract a 1280x720 region from 1280x960
        let y_start_offset = 120;

        for dy in 0..dst_h {
            let sy = y_start_offset + (dy * 2);
            let src_row = sy * src_stride;
            let dst_row = dy * dst_w;

            for dx in 0..dst_w {
                let sx = dx * 2;
                
                // Sample pixel directly (1280 -> 640)
                let pixel = src_raw[src_row + sx];

                // Remove .swap_bytes() first to test native endianness.
                // If static disappears but colors are inverted (blue skin), change to: pixel.swap_bytes()
                self.scaled_buffer[dst_row + dx] = pixel;
            }
        }
    }

    /// Downsamples and rotates raw 1280x960 camera frame 90 degrees CW into a 640x360 landscape viewport
    pub fn downsample_and_rotate_90_cw(&mut self, src_raw: &[u16], src_stride: usize) {
        let dst_w = 640;
        let dst_h = 360;

        // Center crop raw camera width from 1280 down to 720 (280..1000)
        let x_center_offset = 280; 

        for dy in 0..dst_h {
            // Map destination Y (0..360) to raw camera X (280..1000)
            let sx = x_center_offset + (dy * 2);
            let dst_row_offset = dy * dst_w;

            for dx in 0..dst_w {
                // Map destination X (0..640) to raw camera Y (0..960) with 1.5x scale
                let sy = (dx * 3) >> 1; // Integer equivalent of (dx * 1.5)
                
                let src_idx = (sy * src_stride) + sx;
                let pixel = src_raw[src_idx];

                // If colors look inverted (e.g. blue skin), swap endianness: pixel.swap_bytes()
                self.scaled_buffer[dst_row_offset + dx] = pixel;
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