//! Image layout and crop geometry used by the PPA pipeline.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb565,
    Rgb888,
}

impl PixelFormat {
    pub const fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgb565 => 2,
            Self::Rgb888 => 3,
        }
    }
}

/// Bytes needed for a packed `w`×`h` image.
pub const fn image_len(w: u32, h: u32, format: PixelFormat) -> usize {
    w as usize * h as usize * format.bytes_per_pixel()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn full(w: u32, h: u32) -> Self {
        Self { x: 0, y: 0, w, h }
    }

    /// Intersection with a `width`×`height` image; `None` if nothing is left.
    pub fn clamp_to(self, width: u32, height: u32) -> Option<Self> {
        let x = self.x.min(width);
        let y = self.y.min(height);
        let w = self.w.min(width - x);
        let h = self.h.min(height - y);
        (w > 0 && h > 0).then_some(Self { x, y, w, h })
    }
}

/// Largest centered crop of a `src_w`×`src_h` image with the aspect ratio of `dst_w`×`dst_h`.
///
/// Used to fill the preview without stretching: e.g. 1280×960 → 640×720 keeps the full
/// height and crops the width to 853.
pub fn centered_aspect_crop(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Rect {
    // Compare aspect ratios without floats: src_w/src_h vs dst_w/dst_h.
    if u64::from(src_w) * u64::from(dst_h) >= u64::from(dst_w) * u64::from(src_h) {
        // Source is wider: keep full height, crop width.
        let w = (u64::from(src_h) * u64::from(dst_w) / u64::from(dst_h)) as u32;
        Rect { x: (src_w - w) / 2, y: 0, w, h: src_h }
    } else {
        // Source is taller: keep full width, crop height.
        let h = (u64::from(src_w) * u64::from(dst_h) / u64::from(dst_w)) as u32;
        Rect { x: 0, y: (src_h - h) / 2, w: src_w, h }
    }
}

/// Largest `src_w`×`src_h`-shaped rectangle centered inside a `dst_w`×`dst_h` area
/// (letterbox / pillarbox placement: the whole source stays visible, unstretched).
///
/// E.g. a 1280×960 frame in the 640×720 preview → 640×480 at y = 120.
pub fn fit_rect(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Rect {
    if u64::from(src_w) * u64::from(dst_h) >= u64::from(dst_w) * u64::from(src_h) {
        // Source is wider: full width, bars above and below.
        let h = (u64::from(dst_w) * u64::from(src_h) / u64::from(src_w)) as u32;
        Rect { x: 0, y: (dst_h - h) / 2, w: dst_w, h }
    } else {
        // Source is taller: full height, bars left and right.
        let w = (u64::from(dst_h) * u64::from(src_w) / u64::from(src_h)) as u32;
        Rect { x: (dst_w - w) / 2, y: 0, w, h: dst_h }
    }
}

/// Borrowed, read-only view of a packed image.
#[derive(Clone, Copy, Debug)]
pub struct ImageRef<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

impl ImageRef<'_> {
    /// True if `r` is non-empty and lies entirely inside the image.
    pub fn contains(&self, r: Rect) -> bool {
        r.w > 0
            && r.h > 0
            && u64::from(r.x) + u64::from(r.w) <= u64::from(self.width)
            && u64::from(r.y) + u64::from(r.h) <= u64::from(self.height)
    }

    /// True if `data` is large enough for `width`×`height` pixels of `format`.
    pub fn is_well_formed(&self) -> bool {
        self.data.len() >= image_len(self.width, self.height, self.format)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_len_matches_pipeline_buffers() {
        assert_eq!(image_len(640, 720, PixelFormat::Rgb565), 921_600);
        assert_eq!(image_len(640, 480, PixelFormat::Rgb888), 921_600);
        assert_eq!(image_len(112, 112, PixelFormat::Rgb888), 37_632);
    }

    #[test]
    fn preview_crop_for_ov5647_into_left_half() {
        // The firmware's actual configuration: 1280×960 sensor → 640×720 viewport.
        let crop = centered_aspect_crop(1280, 960, 640, 720);
        assert_eq!(crop, Rect { x: 213, y: 0, w: 853, h: 960 });
    }

    #[test]
    fn centered_crop_on_taller_source_crops_height() {
        let crop = centered_aspect_crop(720, 1280, 640, 480);
        assert_eq!(crop, Rect { x: 0, y: 370, w: 720, h: 540 });
    }

    #[test]
    fn centered_crop_with_same_aspect_is_identity() {
        assert_eq!(centered_aspect_crop(1280, 960, 640, 480), Rect::full(1280, 960));
    }

    #[test]
    fn preview_fit_for_ov5647_into_left_half() {
        // The firmware's actual configuration: whole 1280×960 frame letterboxed in 640×720.
        assert_eq!(fit_rect(1280, 960, 640, 720), Rect { x: 0, y: 120, w: 640, h: 480 });
    }

    #[test]
    fn fit_taller_source_pillarboxes() {
        assert_eq!(fit_rect(480, 640, 640, 480), Rect { x: 140, y: 0, w: 360, h: 480 });
    }

    #[test]
    fn fit_same_aspect_fills_area() {
        assert_eq!(fit_rect(1280, 960, 640, 480), Rect::full(640, 480));
    }

    #[test]
    fn clamp_trims_box_hanging_off_the_edge() {
        let r = Rect { x: 600, y: 450, w: 100, h: 100 };
        assert_eq!(r.clamp_to(640, 480), Some(Rect { x: 600, y: 450, w: 40, h: 30 }));
    }

    #[test]
    fn clamp_rejects_box_fully_outside() {
        assert_eq!(Rect { x: 700, y: 10, w: 20, h: 20 }.clamp_to(640, 480), None);
        assert_eq!(Rect { x: 10, y: 10, w: 0, h: 20 }.clamp_to(640, 480), None);
    }

    #[test]
    fn contains_checks_bounds_without_overflow() {
        let data = [0u8; 16];
        let img = ImageRef { data: &data, width: 4, height: 2, format: PixelFormat::Rgb565 };
        assert!(img.contains(Rect::full(4, 2)));
        assert!(img.contains(Rect { x: 3, y: 1, w: 1, h: 1 }));
        assert!(!img.contains(Rect { x: 3, y: 0, w: 2, h: 1 }));
        assert!(!img.contains(Rect { x: 0, y: 0, w: 0, h: 1 }));
        assert!(!img.contains(Rect { x: u32::MAX, y: 0, w: 2, h: 1 }));
    }

    #[test]
    fn well_formed_requires_enough_bytes() {
        let data = [0u8; 15];
        let img = ImageRef { data: &data, width: 4, height: 2, format: PixelFormat::Rgb565 };
        assert!(!img.is_well_formed());
        let img = ImageRef { data: &data[..12], width: 2, height: 2, format: PixelFormat::Rgb888 };
        assert!(img.is_well_formed());
    }
}
