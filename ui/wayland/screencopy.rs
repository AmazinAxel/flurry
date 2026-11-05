// wlr-screencopy: one output's current frame into an shm image (the screenshot freeze).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_output, wl_shm};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1 as frame, zwlr_screencopy_manager_v1 as manager};

use super::shm::ShmImage;
use super::{Ev, Wl, ctx, push};

/// A captured frame: 32-bit pixels, blue first in memory (`bgr`) or red first.
pub struct Capture {
    pub image: Rc<ShmImage>,
    pub bgr: bool,
    pub y_invert: bool,
}

struct Pending {
    proxy: frame::ZwlrScreencopyFrameV1,
    /// (format, width, height) of the shm buffer offered
    offer: Option<(wl_shm::Format, u32, u32)>,
    image: Option<Rc<ShmImage>>,
    y_invert: bool,
    done: Box<dyn FnOnce(Option<Capture>)>,
}

thread_local! {
    static PENDING: RefCell<HashMap<ObjectId, Pending>> = RefCell::default();
}

/// Captures `output` without the cursor; `done` gets None when the compositor can't.
pub fn capture(output: &wl_output::WlOutput, done: impl FnOnce(Option<Capture>) + 'static) {
    let Some(proxy) = ctx(|c| c.screencopy.as_ref().map(|m| m.capture_output(0, output, &c.qh, ()))) else { return done(None) };
    PENDING.with(|p| p.borrow_mut().insert(proxy.id(), Pending { proxy, offer: None, image: None, y_invert: false, done: Box::new(done) }));
}

/// The 8-bit-per-channel formats, and whether blue comes first in memory.
fn layout(f: wl_shm::Format) -> Option<bool> {
    use wl_shm::Format::*;
    match f {
        Argb8888 | Xrgb8888 => Some(true),
        Abgr8888 | Xbgr8888 => Some(false),
        _ => None,
    }
}

/// The offered buffer is complete (v3: buffer_done; earlier: the buffer event): copy into it.
fn copy(id: &ObjectId) {
    let ok = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let Some(f) = p.get_mut(id) else { return true };
        let Some((format, w, h)) = f.offer else { return false };
        let Some(img) = ShmImage::blank(w, h, format) else { return false };
        f.proxy.copy(&img.buffer);
        f.image = Some(Rc::new(img));
        true
    });
    if !ok {
        push(Ev::Captured(id.clone(), false));
    }
}

pub(super) fn finish(id: &ObjectId, ok: bool) {
    let Some(f) = PENDING.with(|p| p.borrow_mut().remove(id)) else { return };
    f.proxy.destroy();
    let bgr = f.offer.and_then(|(fmt, ..)| layout(fmt));
    let cap = match (ok, f.image, bgr) {
        (true, Some(image), Some(bgr)) => Some(Capture { image, bgr, y_invert: f.y_invert }),
        _ => None,
    };
    (f.done)(cap);
}

impl Dispatch<frame::ZwlrScreencopyFrameV1, ()> for Wl {
    fn event(_: &mut Self, fr: &frame::ZwlrScreencopyFrameV1, ev: frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let id = fr.id();
        match ev {
            frame::Event::Buffer { format: WEnum::Value(format), width, height, stride } => {
                if layout(format).is_some() && stride == width * 4 {
                    PENDING.with(|p| p.borrow_mut().get_mut(&id).map(|f| f.offer = Some((format, width, height))));
                }
                if fr.version() < 3 {
                    copy(&id);
                }
            }
            frame::Event::BufferDone => copy(&id),
            frame::Event::Flags { flags: WEnum::Value(fl) } => {
                PENDING.with(|p| p.borrow_mut().get_mut(&id).map(|f| f.y_invert = fl.contains(frame::Flags::YInvert)));
            }
            frame::Event::Ready { .. } => push(Ev::Captured(id, true)),
            frame::Event::Failed => push(Ev::Captured(id, false)),
            _ => {}
        }
    }
}

delegate_noop!(Wl: ignore manager::ZwlrScreencopyManagerV1);
