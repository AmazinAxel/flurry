// Shared-memory buffers: the per-surface frame buffers and the immutable images (the wallpaper).

use std::cell::Cell;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU64};

use wayland_client::protocol::{wl_buffer, wl_shm};
use wayland_client::{Connection, Dispatch, QueueHandle};

use super::{Wl, ctx};

pub(super) struct Buffer {
    pub(super) wl: wl_buffer::WlBuffer,
    pub(super) ptr: *mut u8,
    pub(super) len: usize,
    pub(super) size: (u32, u32),
    pub(super) busy: Arc<AtomicBool>,
    /// number of the frame it holds (0: undefined contents)
    pub(super) frame: Cell<u64>,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.wl.destroy();
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// A shared mapping of a w×h ABGR8888 buffer; None when empty or when it or its stride overflows
/// the protocol's i32 (the compositor would kill the connection).
fn map(w: u32, h: u32) -> Option<(OwnedFd, *mut u8, usize)> {
    let len = w.checked_mul(4)?.checked_mul(h)?;
    if len == 0 || len > i32::MAX as u32 {
        return None;
    }
    let len = len as usize;
    let fd = unsafe { libc::memfd_create(c"flurry".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // owned first: closed on every failure below
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } < 0 {
        return None;
    }
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    (ptr != libc::MAP_FAILED).then_some((fd, ptr.cast(), len))
}

/// A wl_buffer over all of `fd` (sized by `map`).
fn pool_buffer<U: Send + Sync + 'static>(fd: &OwnedFd, w: u32, h: u32, format: wl_shm::Format, data: U) -> wl_buffer::WlBuffer
where
    Wl: Dispatch<wl_buffer::WlBuffer, U>,
{
    ctx(|c| {
        let pool = c.shm.create_pool(fd.as_fd(), (w * h * 4) as i32, &c.qh, ());
        let b = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, format, &c.qh, data);
        pool.destroy();
        b
    })
}

// ABGR8888 everywhere: the same byte order as tiny-skia, so pixmaps live in the buffers
pub(super) fn new_buffer(w: u32, h: u32) -> Option<Buffer> {
    let (fd, ptr, len) = map(w, h)?;
    let busy = Arc::new(AtomicBool::new(false));
    let wl = pool_buffer(&fd, w, h, wl_shm::Format::Abgr8888, busy.clone());
    Some(Buffer { wl, ptr, len, size: (w, h), busy, frame: Cell::new(0) })
}

/// An immutable full-size image living in one shm buffer (ABGR8888 = tiny-skia's RGBA byte order),
/// attached to any number of surfaces and drawn from directly: the only copy of the wallpaper.
pub struct ShmImage {
    pub buffer: wl_buffer::WlBuffer,
    pub width: u32,
    pub height: u32,
    /// unique per image: a cache key that can't collide with a freed image's
    pub id: u64,
    ptr: *mut u8,
    len: usize,
}

static IMAGE_IDS: AtomicU64 = AtomicU64::new(1);

/// Premultiplied RGBA pixels in mapped shared memory, fillable on a worker thread (the wallpaper
/// decodes straight into it); `ShmImage::from_pixels` then hands the memory to the compositor.
pub struct ShmPixels {
    fd: Option<OwnedFd>,
    ptr: usize,
    len: usize,
    pub width: u32,
    pub height: u32,
}

impl ShmPixels {
    pub fn new(w: u32, h: u32) -> Option<ShmPixels> {
        let (fd, ptr, len) = map(w, h)?;
        Some(ShmPixels { fd: Some(fd), ptr: ptr as usize, len, width: w, height: h })
    }
}

impl AsMut<[u8]> for ShmPixels {
    fn as_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr as *mut u8, self.len) }
    }
}

impl Drop for ShmPixels {
    fn drop(&mut self) {
        if self.fd.is_some() {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        }
    }
}

impl ShmImage {
    pub fn from_pixels(mut p: ShmPixels) -> Rc<ShmImage> {
        let fd = p.fd.take().expect("fresh pixels"); // the mapping now belongs to the ShmImage
        let (w, h) = (p.width, p.height);
        // Release ignored: the contents never change, so it's attachable again any time
        let buffer = pool_buffer(&fd, w, h, wl_shm::Format::Abgr8888, ());
        Rc::new(ShmImage { buffer, width: w, height: h, id: IMAGE_IDS.fetch_add(1, Relaxed), ptr: p.ptr as *mut u8, len: p.len })
    }

    /// An unfilled image in `format` (a 32-bit one), for the compositor to copy into (screencopy).
    pub(super) fn blank(w: u32, h: u32, format: wl_shm::Format) -> Option<ShmImage> {
        let (fd, ptr, len) = map(w, h)?;
        let buffer = pool_buffer(&fd, w, h, format, ());
        Some(ShmImage { buffer, width: w, height: h, id: IMAGE_IDS.fetch_add(1, Relaxed), ptr, len })
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn pixmap(&self) -> tiny_skia::PixmapRef<'_> {
        let data = unsafe { std::slice::from_raw_parts(self.ptr, self.len) };
        tiny_skia::PixmapRef::from_bytes(data, self.width, self.height).expect("sized at creation")
    }
}

impl Drop for ShmImage {
    fn drop(&mut self) {
        self.buffer.destroy();
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for Wl {
    fn event(_: &mut Self, _: &wl_buffer::WlBuffer, ev: wl_buffer::Event, busy: &Arc<AtomicBool>, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = ev {
            busy.store(false, Relaxed);
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for Wl {
    fn event(_: &mut Self, _: &wl_buffer::WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
