//! Image layout and crop geometry used by the PPA pipeline.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb565,
    /// ESP-IDF RGB888 as produced by the PPA: 3 bytes per pixel stored **B, G, R**
    /// (`color_pixel_rgb888_data_t`). ESP-DL calls this layout BGR888.
    Rgb888,
}

impl PixelFormat {
    const fn bytes_per_pixel(self) -> usize {
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
    /// Rectangle from inclusive corner coordinates as reported by detectors, clipped to a
    /// `width`×`height` image. `None` if nothing of it lies inside the image.
    pub fn from_corners(
        x0: i32,
        y0: i32,
        x1: i32,
        y1: i32,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        let clip = |v: i32, max: u32| i64::from(v).clamp(0, i64::from(max));
        // Inclusive corners -> half-open [x0, x1 + 1), then clip both ends to the image.
        let (left, right) = (clip(x0, width), clip(x1.saturating_add(1), width));
        let (top, bottom) = (clip(y0, height), clip(y1.saturating_add(1), height));
        (right > left && bottom > top).then(|| Self {
            x: left as u32,
            y: top as u32,
            w: (right - left) as u32,
            h: (bottom - top) as u32,
        })
    }

    pub const fn full(w: u32, h: u32) -> Self {
        Self { x: 0, y: 0, w, h }
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
        Rect {
            x: 0,
            y: (dst_h - h) / 2,
            w: dst_w,
            h,
        }
    } else {
        // Source is taller: full height, bars left and right.
        let w = (u64::from(dst_h) * u64::from(src_w) / u64::from(src_h)) as u32;
        Rect {
            x: (dst_w - w) / 2,
            y: 0,
            w,
            h: dst_h,
        }
    }
}

/// Maps `r`, given in a `from_w`×`from_h` image, into the `to` rectangle of another picture
/// (e.g. detector-image coordinates → the letterboxed preview area of the canvas).
pub fn map_rect(r: Rect, from_w: u32, from_h: u32, to: Rect) -> Rect {
    let sx = |v: u32| (u64::from(v) * u64::from(to.w) / u64::from(from_w)) as u32;
    let sy = |v: u32| (u64::from(v) * u64::from(to.h) / u64::from(from_h)) as u32;
    let (x0, y0) = (sx(r.x), sy(r.y));
    let (x1, y1) = (sx(r.x + r.w), sy(r.y + r.h));
    Rect {
        x: to.x + x0,
        y: to.y + y0,
        w: x1 - x0,
        h: y1 - y0,
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
    fn fit_4_3_frame_into_taller_area_letterboxes() {
        // A whole 1280×960 frame placed in a 640×720 area: bars above and below.
        assert_eq!(
            fit_rect(1280, 960, 640, 720),
            Rect {
                x: 0,
                y: 120,
                w: 640,
                h: 480
            }
        );
    }

    #[test]
    fn firmware_canvas_is_the_full_sensor_view() {
        // Firmware configuration: 1280×960 sensor → 640×480 canvas (the UI centres the canvas),
        // and the 640×480 detector image maps 1:1 onto it for the face overlay.
        let canvas = fit_rect(1280, 960, 640, 480);
        assert_eq!(canvas, Rect::full(640, 480));
        let face = Rect {
            x: 100,
            y: 50,
            w: 80,
            h: 90,
        };
        assert_eq!(map_rect(face, 640, 480, canvas), face);
    }

    #[test]
    fn fit_taller_source_pillarboxes() {
        assert_eq!(
            fit_rect(480, 640, 640, 480),
            Rect {
                x: 140,
                y: 0,
                w: 360,
                h: 480
            }
        );
    }

    #[test]
    fn fit_same_aspect_fills_area() {
        assert_eq!(fit_rect(1280, 960, 640, 480), Rect::full(640, 480));
    }

    #[test]
    fn from_corners_converts_inclusive_corners() {
        assert_eq!(
            Rect::from_corners(10, 20, 109, 219, 640, 480),
            Some(Rect {
                x: 10,
                y: 20,
                w: 100,
                h: 200
            })
        );
    }

    #[test]
    fn from_corners_clips_to_image_and_handles_negatives() {
        assert_eq!(
            Rect::from_corners(-5, -5, 9, 9, 640, 480),
            Some(Rect {
                x: 0,
                y: 0,
                w: 10,
                h: 10
            })
        );
        assert_eq!(
            Rect::from_corners(630, 470, 700, 500, 640, 480),
            Some(Rect {
                x: 630,
                y: 470,
                w: 10,
                h: 10
            })
        );
        assert_eq!(
            Rect::from_corners(i32::MIN, 0, i32::MAX, 0, 640, 480),
            Some(Rect {
                x: 0,
                y: 0,
                w: 640,
                h: 1
            })
        );
    }

    #[test]
    fn from_corners_rejects_empty_or_outside_boxes() {
        assert_eq!(Rect::from_corners(50, 50, 40, 60, 640, 480), None); // x1 < x0
        assert_eq!(Rect::from_corners(700, 10, 720, 20, 640, 480), None); // right of image
        assert_eq!(Rect::from_corners(-30, 10, -10, 20, 640, 480), None); // left of image
    }

    #[test]
    fn map_box_into_letterboxed_area() {
        // A 640×480 image shown at (0,120) inside a 640×720 area.
        let preview = fit_rect(1280, 960, 640, 720);
        let face = Rect {
            x: 100,
            y: 50,
            w: 80,
            h: 90,
        };
        assert_eq!(
            map_rect(face, 640, 480, preview),
            Rect {
                x: 100,
                y: 170,
                w: 80,
                h: 90
            }
        );
    }

    #[test]
    fn map_rect_scales_both_axes() {
        let to = Rect {
            x: 10,
            y: 20,
            w: 320,
            h: 120,
        };
        let r = Rect {
            x: 64,
            y: 48,
            w: 128,
            h: 96,
        };
        assert_eq!(
            map_rect(r, 640, 480, to),
            Rect {
                x: 42,
                y: 32,
                w: 64,
                h: 24
            }
        );
        assert_eq!(map_rect(Rect::full(640, 480), 640, 480, to), to);
    }

    #[test]
    fn contains_checks_bounds_without_overflow() {
        let data = [0u8; 16];
        let img = ImageRef {
            data: &data,
            width: 4,
            height: 2,
            format: PixelFormat::Rgb565,
        };
        assert!(img.contains(Rect::full(4, 2)));
        assert!(img.contains(Rect {
            x: 3,
            y: 1,
            w: 1,
            h: 1
        }));
        assert!(!img.contains(Rect {
            x: 3,
            y: 0,
            w: 2,
            h: 1
        }));
        assert!(!img.contains(Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 1
        }));
        assert!(!img.contains(Rect {
            x: u32::MAX,
            y: 0,
            w: 2,
            h: 1
        }));
    }

    #[test]
    fn well_formed_requires_enough_bytes() {
        let data = [0u8; 15];
        let img = ImageRef {
            data: &data,
            width: 4,
            height: 2,
            format: PixelFormat::Rgb565,
        };
        assert!(!img.is_well_formed());
        let img = ImageRef {
            data: &data[..12],
            width: 2,
            height: 2,
            format: PixelFormat::Rgb888,
        };
        assert!(img.is_well_formed());
    }
}
