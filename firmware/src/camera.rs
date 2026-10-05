//! OV5647 V4L2 stream (opened and started by `p4_hardware_init_all`).
//!
//! Frames are zero-copy views of the driver's MMAP buffers. A `Frame` keeps its buffer
//! out of the capture queue; dropping it hands the buffer back to the ISP. Because
//! `next_frame` borrows the camera mutably, the borrow checker guarantees a frame can't
//! outlive its buffer or be aliased by a second dequeue.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};

use biometric_core::geometry::{ImageRef, PixelFormat};
use log::error;

use crate::ffi;

static TAKEN: AtomicBool = AtomicBool::new(false);

pub struct Camera {
    _private: (),
}

impl Camera {
    /// Returns the single handle to the camera stream, or `None` if already taken.
    pub fn take() -> Option<Self> {
        (!TAKEN.swap(true, Ordering::AcqRel)).then_some(Self { _private: () })
    }

    /// Blocks until the ISP delivers the next RGB565 frame. The error is the C return code.
    pub fn next_frame(&mut self) -> Result<Frame<'_>, i32> {
        let mut raw = ffi::p4_camera_frame_t::default();
        // Safety: `raw` is a valid out-pointer; C fills it from VIDIOC_DQBUF.
        let ret = unsafe { ffi::p4_camera_capture_frame(&mut raw) };
        if ret != 0 {
            return Err(ret);
        }
        // A buffer is dequeued from here on: dropping `frame` hands it back, also on the
        // error path below.
        let frame = Frame {
            raw,
            _camera: PhantomData,
        };
        if frame.raw.data.is_null() {
            return Err(-1);
        }
        Ok(frame)
    }
}

/// A dequeued camera buffer, re-queued to the driver on drop.
pub struct Frame<'cam> {
    raw: ffi::p4_camera_frame_t,
    _camera: PhantomData<&'cam mut Camera>,
}

impl Frame<'_> {
    pub fn image(&self) -> ImageRef<'_> {
        // Safety: the MMAP buffer is valid for `data_len` bytes and is not handed back to
        // the driver (and so not rewritten by DMA) until this Frame is dropped.
        let data = unsafe { core::slice::from_raw_parts(self.raw.data, self.raw.data_len) };
        ImageRef {
            data,
            width: u32::from(self.raw.width),
            height: u32::from(self.raw.height),
            format: PixelFormat::Rgb565,
        }
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        // Safety: `raw` came from a successful dequeue and is released exactly once.
        let ret = unsafe { ffi::p4_camera_release_frame(&self.raw) };
        if ret != 0 {
            error!(
                "[Camera] failed to re-queue buffer {}: {ret}",
                self.raw.buffer_index
            );
        }
    }
}
