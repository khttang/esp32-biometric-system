//! ESP32-P4 Pixel-Processing Accelerator (hardware crop / scale / colour convert)
//! and the cache-line aligned PSRAM buffers it reads from and writes into.
//!
//! The PPA driver performs the required cache write-back / invalidate on its input
//! and output windows itself (`esp_driver_ppa/src/ppa_srm.c`), so callers only need
//! correctly aligned buffers.

use core::ffi::c_void;
use core::ptr::NonNull;

use esp_idf_svc::sys::{self, esp, EspError};

pub use biometric_core::geometry::{image_len, ImageRef, PixelFormat, Rect};

/// L2 cache line size (CONFIG_CACHE_L2_CACHE_LINE_128B). PPA output buffers must be
/// aligned to it and sized in multiples of it.
const CACHE_LINE: usize = 128;

fn srm_color_mode(format: PixelFormat) -> sys::ppa_srm_color_mode_t {
    match format {
        PixelFormat::Rgb565 => sys::ppa_srm_color_mode_t_PPA_SRM_COLOR_MODE_RGB565,
        PixelFormat::Rgb888 => sys::ppa_srm_color_mode_t_PPA_SRM_COLOR_MODE_RGB888,
    }
}

/// Owned, zero-initialised, cache-line aligned PSRAM buffer usable as a DMA/PPA target.
pub struct DmaBuf {
    ptr: NonNull<u8>,
    len: usize,
}

// Safety: DmaBuf uniquely owns its heap allocation and holds no thread-affine state.
unsafe impl Send for DmaBuf {}

impl DmaBuf {
    pub fn new(len: usize) -> Option<Self> {
        let len = len.next_multiple_of(CACHE_LINE);
        // Safety: plain allocator call; a null return is handled below.
        let raw = unsafe {
            sys::heap_caps_aligned_alloc(
                CACHE_LINE,
                len,
                sys::MALLOC_CAP_SPIRAM | sys::MALLOC_CAP_8BIT,
            )
        };
        let ptr = NonNull::new(raw.cast::<u8>())?;
        // Safety: freshly allocated `len` bytes; zeroing makes every later `&[u8]` view initialised.
        unsafe { ptr.as_ptr().write_bytes(0, len) };
        Some(Self { ptr, len })
    }

    pub fn as_slice(&self) -> &[u8] {
        // Safety: `ptr` is valid for `len` initialised bytes for as long as `self` lives.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for DmaBuf {
    fn drop(&mut self) {
        // Safety: allocated by heap_caps_aligned_alloc and freed exactly once.
        unsafe { sys::heap_caps_free(self.ptr.as_ptr().cast::<c_void>()) };
    }
}

/// Destination of a PPA operation: a `width`×`height` picture of `format` stored in `buf`,
/// of which `rect` is written.
pub struct Target<'a> {
    pub buf: &'a mut DmaBuf,
    pub width: u32,
    pub height: u32,
    pub rect: Rect,
    pub format: PixelFormat,
}

impl<'a> Target<'a> {
    /// Fill the whole `width`×`height` picture.
    pub fn full(buf: &'a mut DmaBuf, width: u32, height: u32, format: PixelFormat) -> Self {
        Self {
            buf,
            width,
            height,
            rect: Rect::full(width, height),
            format,
        }
    }
}

/// One PPA scale-rotate-mirror client. Each thread that issues PPA work owns its own.
pub struct Ppa {
    client: sys::ppa_client_handle_t,
}

// Safety: the client handle is only used through `&mut self`, i.e. by one thread at a time.
unsafe impl Send for Ppa {}

impl Ppa {
    pub fn new() -> Result<Self, EspError> {
        let config = sys::ppa_client_config_t {
            oper_type: sys::ppa_operation_t_PPA_OPERATION_SRM,
            ..Default::default()
        };
        let mut client: sys::ppa_client_handle_t = core::ptr::null_mut();
        esp!(unsafe { sys::ppa_register_client(&config, &mut client) })?;
        Ok(Self { client })
    }

    /// Crops `crop` out of `src`, scales it to fill `dst.rect`, converting to `dst.format`.
    /// Pixels of `dst.buf` outside `dst.rect` are left untouched. Blocks until the hardware is done.
    ///
    /// The PPA quantises scale factors to 1/16 steps, so the written area can be a pixel
    /// smaller than requested on non-integer ratios.
    pub fn scale_crop(
        &mut self,
        src: ImageRef<'_>,
        crop: Rect,
        dst: Target<'_>,
    ) -> Result<(), EspError> {
        let dst_area = ImageRef {
            data: dst.buf.as_slice(),
            width: dst.width,
            height: dst.height,
            format: dst.format,
        };
        if !src.contains(crop)
            || !src.is_well_formed()
            || !dst_area.contains(dst.rect)
            || !dst_area.is_well_formed()
        {
            return Err(EspError::from_infallible::<
                { sys::ESP_ERR_INVALID_ARG as sys::esp_err_t },
            >());
        }

        let mut op = sys::ppa_srm_oper_config_t::default();
        op.in_.buffer = src.data.as_ptr().cast::<c_void>();
        op.in_.pic_w = src.width;
        op.in_.pic_h = src.height;
        op.in_.block_w = crop.w;
        op.in_.block_h = crop.h;
        op.in_.block_offset_x = crop.x;
        op.in_.block_offset_y = crop.y;
        op.in_.__bindgen_anon_1.srm_cm = srm_color_mode(src.format);

        op.out.buffer = dst.buf.ptr.as_ptr().cast::<c_void>();
        op.out.buffer_size = dst.buf.len as u32;
        op.out.pic_w = dst.width;
        op.out.pic_h = dst.height;
        op.out.block_offset_x = dst.rect.x;
        op.out.block_offset_y = dst.rect.y;
        op.out.__bindgen_anon_1.srm_cm = srm_color_mode(dst.format);

        op.scale_x = dst.rect.w as f32 / crop.w as f32;
        op.scale_y = dst.rect.h as f32 / crop.h as f32;
        op.rotation_angle = sys::ppa_srm_rotation_angle_t_PPA_SRM_ROTATION_ANGLE_0;
        op.mode = sys::ppa_trans_mode_t_PPA_TRANS_MODE_BLOCKING;

        // Safety: buffers were bounds-checked above and outlive this blocking call.
        esp!(unsafe { sys::ppa_do_scale_rotate_mirror(self.client, &op) })
    }
}

impl Drop for Ppa {
    fn drop(&mut self) {
        // Safety: registered in `new`, unregistered exactly once.
        unsafe { sys::ppa_unregister_client(self.client) };
    }
}
