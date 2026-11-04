// ext-session-lock: the lock object and its per-output lock surfaces.

use std::rc::Rc;

use tiny_skia::Color;
use wayland_client::protocol::wl_output;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, backend::ObjectId};
use wayland_protocols::ext::session_lock::v1::client::{ext_session_lock_surface_v1, ext_session_lock_v1};

use super::surface::{BuildFn, Content, Gradient, Inner, Role, Surface};
use super::{Ev, LOCK_LISTENER, LockEvent, ShmImage, Wl, ctx, push};

/// Locks the session; `on_event` gets Locked / Finished (compositor refused or lock ended).
pub fn lock(on_event: impl Fn(LockEvent) + 'static) -> bool {
    let started = ctx(|c| {
        let Some(m) = &c.lock_mgr else {
            eprintln!("flurry: the compositor has no ext-session-lock: can't lock");
            return false;
        };
        if c.lock.is_some() {
            return false;
        }
        c.lock = Some(m.lock(&c.qh, ()));
        true
    });
    // a refused call must not take over the running lock's events
    if started {
        LOCK_LISTENER.with(|l| *l.borrow_mut() = Some(Rc::new(on_event)));
    }
    started
}

pub fn is_locking() -> bool {
    ctx(|c| c.lock.is_some())
}

/// A lock surface showing `image` (black without one) under a vertical `gradient`, with the built
/// content in a subsurface clipped to what it paints: no full-screen repaint per keypress.
pub fn lock_surface(output: &wl_output::WlOutput, build: BuildFn, image: Option<Rc<ShmImage>>, gradient: (Color, Color)) -> Option<Surface> {
    let inner = ctx(|c| {
        let lock = c.lock.as_ref()?;
        let wl = c.compositor.create_surface(&c.qh, ());
        let ls = lock.get_lock_surface(&wl, output, &c.qh, wl.id());
        let scale = c.outputs.iter().find(|o| o.wl == *output).map(|o| o.scale).unwrap_or(1);
        // subsurfaces stack in creation order: gradient, then content on top
        let gradient = Gradient::new(c, &wl, gradient);
        let content = Content::new(c, &wl);
        Some(Inner::new(wl, Role::Lock(ls), scale, build).with_lock(content, gradient, image, output.clone()))
    })?;
    Some(Surface::register(inner))
}

/// Only called once Locked arrived (before it, unlock_and_destroy is a protocol error).
pub fn unlock() {
    ctx(|c| {
        if let Some(l) = c.lock.take() {
            l.unlock_and_destroy();
        }
        let _ = c.conn.flush();
    });
}

impl Dispatch<ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, ObjectId> for Wl {
    fn event(
        _: &mut Self, l: &ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, ev: ext_session_lock_surface_v1::Event, id: &ObjectId,
        _: &Connection, _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure { serial, width, height } = ev {
            l.ack_configure(serial);
            push(Ev::Configure(id.clone(), width, height));
        }
    }
}

impl Dispatch<ext_session_lock_v1::ExtSessionLockV1, ()> for Wl {
    fn event(_: &mut Self, _: &ext_session_lock_v1::ExtSessionLockV1, ev: ext_session_lock_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            ext_session_lock_v1::Event::Locked => push(Ev::Lock(LockEvent::Locked)),
            ext_session_lock_v1::Event::Finished => push(Ev::Lock(LockEvent::Finished)),
            _ => {}
        }
    }
}
