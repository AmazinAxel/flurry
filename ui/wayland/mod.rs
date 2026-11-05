// Wayland client: outputs, seat (xkb keyboard with repeat and compose, pointer) and the event queue.
// Surfaces, shm buffers, the session lock and the clipboard live in the submodules.
// Protocol callbacks only queue events; they're handled after dispatch so handlers can freely
// create/destroy surfaces.

mod clipboard;
mod lock;
mod screencopy;
mod shm;
mod surface;

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::rc::{Rc, Weak};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, backend::ObjectId, delegate_noop};
use wayland_protocols::ext::session_lock::v1::client::{ext_session_lock_manager_v1, ext_session_lock_v1};
use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1;
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use xkbcommon::xkb;
use xkbcommon::xkb::compose;

use super::runtime;
use surface::Inner;

pub use clipboard::{OWN_MIME, clear_selection, on_selection, paste, read_selection, set_selection};
pub use lock::{is_locking, lock, lock_surface, unlock};
pub use screencopy::{Capture, capture};
pub use shm::{ShmImage, ShmPixels};
pub use surface::{BuildFn, Surface};
pub use zwlr_layer_shell_v1::Layer;
pub use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity as Keymode};

pub struct Wl;

#[derive(Clone)]
pub struct Output {
    pub wl: wl_output::WlOutput,
    pub name: String,
    pub scale: i32,
    pub done: bool,
}

impl Output {
    fn new(wl: wl_output::WlOutput) -> Output {
        Output { wl, name: String::new(), scale: 1, done: false }
    }
}

struct Ctx {
    conn: Connection,
    qh: QueueHandle<Wl>,
    compositor: wl_compositor::WlCompositor,
    subcompositor: wl_subcompositor::WlSubcompositor,
    shm: wl_shm::WlShm,
    layer_shell: zwlr_layer_shell_v1::ZwlrLayerShellV1,
    lock_mgr: Option<ext_session_lock_manager_v1::ExtSessionLockManagerV1>,
    viewporter: wp_viewporter::WpViewporter,
    single_pixel: wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
    cursor_shape: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    screencopy: Option<wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_device: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    xkb_ctx: xkb::Context,
    xkb: Option<xkb::State>,
    /// dead keys and the Compose key (None: no compose table for the locale)
    compose: Option<compose::State>,
    repeat: (i32, i32), // rate per second, delay ms
    outputs: Vec<Output>,
    lock: Option<ext_session_lock_v1::ExtSessionLockV1>,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
    static QUEUE: RefCell<Option<EventQueue<Wl>>> = const { RefCell::new(None) };
    static EVENTS: RefCell<Vec<Ev>> = const { RefCell::new(Vec::new()) };
    static SURFACES: RefCell<HashMap<ObjectId, Weak<Inner>>> = RefCell::default();
    static FOCUS: RefCell<Option<ObjectId>> = const { RefCell::new(None) };
    static HOVER: RefCell<Option<(ObjectId, f64, f64)>> = const { RefCell::new(None) };
    /// the repeating key and its timer
    static REPEAT: RefCell<Option<(xkb::Keycode, runtime::Timer)>> = const { RefCell::new(None) };
    static OUTPUT_LISTENERS: RefCell<Vec<Rc<dyn Fn()>>> = RefCell::default();
    static LOCK_LISTENER: RefCell<Option<Rc<dyn Fn(LockEvent)>>> = RefCell::default();
}

fn ctx<R>(f: impl FnOnce(&mut Ctx) -> R) -> R {
    CTX.with(|c| f(c.borrow_mut().as_mut().expect("wayland not connected")))
}

/// Copy, with the typed character inline: no heap copy of each key of a password.
#[derive(Clone, Copy, Debug)]
pub struct KeyEvent {
    pub sym: xkb::Keysym,
    /// The character the key types (without ctrl translation), None for non-printing keys
    pub ch: Option<char>,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

impl KeyEvent {
    pub fn lower(&self) -> xkb::Keysym {
        let raw = self.sym.raw();
        xkb::Keysym::new(if (0x41..=0x5a).contains(&raw) { raw + 0x20 } else { raw })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum PointerEvent {
    /// dy in GDK scroll units; `wheel` true for discrete mouse wheel steps
    Scroll { x: f64, y: f64, dy: f64, wheel: bool },
    /// only delivered to surfaces that `track_pointer`
    Motion { x: f64, y: f64 },
    /// left button (anything else: `left` false)
    Button { x: f64, y: f64, left: bool, pressed: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LockEvent {
    Locked,
    Finished,
}

enum Ev {
    Configure(ObjectId, u32, u32),
    Closed(ObjectId),
    Key(xkb::Keycode, bool),
    KeyboardEnter(ObjectId),
    KeyboardLeave,
    Pointer(ObjectId, PointerEvent),
    PreferredScale(ObjectId, i32),
    Outputs,
    OutputGone(wl_output::WlOutput),
    Lock(LockEvent),
    Captured(ObjectId, bool),
}

fn push(ev: Ev) {
    EVENTS.with(|e| e.borrow_mut().push(ev));
}

pub fn connect() -> bool {
    let Ok(conn) = Connection::connect_to_env() else { return false };
    let Ok((globals, mut queue)) = registry_queue_init::<Wl>(&conn) else { return false };
    let qh = queue.handle();
    // a compositor missing a required global: not one flurry can run on
    let (Ok(compositor), Ok(subcompositor), Ok(shm), Ok(layer_shell), Ok(viewporter), Ok(single_pixel), Ok(seat)) = (
        globals.bind(&qh, 4..=6, ()),
        globals.bind(&qh, 1..=1, ()),
        globals.bind(&qh, 1..=1, ()),
        globals.bind(&qh, 4..=4, ()),
        globals.bind(&qh, 1..=1, ()),
        globals.bind(&qh, 1..=1, ()),
        globals.bind::<wl_seat::WlSeat, _, _>(&qh, 5..=9, ()),
    ) else {
        return false;
    };
    let mut c0 = Ctx {
        compositor,
        subcompositor,
        shm,
        layer_shell,
        lock_mgr: globals.bind(&qh, 1..=1, ()).ok(),
        viewporter,
        single_pixel,
        cursor_shape: globals.bind(&qh, 1..=1, ()).ok(),
        screencopy: globals.bind(&qh, 1..=3, ()).ok(),
        keyboard: None,
        pointer: None,
        cursor_device: None,
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb: None,
        compose: None,
        repeat: (25, 600),
        outputs: vec![],
        lock: None,
        conn: conn.clone(),
        qh: qh.clone(),
    };
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"].iter().filter_map(std::env::var_os).find(|v| !v.is_empty()).unwrap_or_else(|| "C".into());
    c0.compose = compose::Table::new_from_locale(&c0.xkb_ctx, &locale, compose::COMPILE_NO_FLAGS)
        .ok()
        .map(|t| compose::State::new(&t, compose::STATE_NO_FLAGS));
    globals.contents().with_list(|list| {
        for g in list.iter().filter(|g| g.interface == "wl_output") {
            let wl: wl_output::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, g.name);
            c0.outputs.push(Output::new(wl));
        }
    });
    clipboard::bind(&globals, &qh, &seat);
    CTX.with(|c| *c.borrow_mut() = Some(c0));
    let _ = queue.roundtrip(&mut Wl);
    let _ = queue.roundtrip(&mut Wl);
    QUEUE.with(|q| *q.borrow_mut() = Some(queue));

    let fd = conn.as_fd().as_raw_fd();
    runtime::watch(fd, read_events);
    // events only enter the queue through read_events, which dispatches them: nothing to do here
    // but send what handlers queued
    runtime::before_poll(move || {
        // socket full: what's left stays queued; wake when it drains so the next pass sends it
        if let Err(wayland_client::backend::WaylandError::Io(e)) = ctx(|c| c.conn.flush())
            && e.kind() == std::io::ErrorKind::WouldBlock
        {
            runtime::poll_out(fd);
        }
    });
    handle_events();
    true
}

fn read_events() {
    let guard = ctx(|c| c.conn.prepare_read());
    if let Some(g) = guard
        && let Err(e) = g.read()
        && !matches!(&e, wayland_client::backend::WaylandError::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    {
        // compositor gone or a protocol error (the connection is dead either way; staying would spin
        // on the hung-up fd): exit so the session (systemd) can restart us
        eprintln!("flurry: lost the Wayland connection: {e}");
        // _exit: skip thread-local destructors (surfaces would try to talk to the dead connection)
        unsafe { libc::_exit(1) };
    }
    QUEUE.with(|q| {
        if let Some(q) = q.borrow_mut().as_mut() {
            let _ = q.dispatch_pending(&mut Wl);
        }
    });
    handle_events();
}

fn handle_events() {
    loop {
        let events = EVENTS.with(|e| std::mem::take(&mut *e.borrow_mut()));
        if events.is_empty() {
            return;
        }
        // one at a time: a panicking handler must not drop the rest of the batch (a lock Configure)
        for ev in events {
            runtime::guard(|| handle(ev));
        }
    }
}

fn surface(id: &ObjectId) -> Option<Rc<Inner>> {
    SURFACES.with(|s| s.borrow().get(id).and_then(Weak::upgrade))
}

fn handle(ev: Ev) {
    match ev {
        Ev::Configure(id, w, h) => {
            if let Some(s) = surface(&id) {
                s.configure(w, h);
            }
        }
        Ev::Closed(id) => {
            if let Some(s) = surface(&id) {
                s.destroy_role();
                if let Some(f) = s.on_closed.take() {
                    f();
                }
            }
        }
        Ev::PreferredScale(id, scale) => {
            if let Some(s) = surface(&id)
                && s.scale.replace(scale) != scale
            {
                s.render();
            }
        }
        Ev::KeyboardEnter(id) => FOCUS.with(|f| *f.borrow_mut() = Some(id)),
        Ev::KeyboardLeave => {
            FOCUS.with(|f| *f.borrow_mut() = None);
            stop_repeat();
            ctx(|c| c.compose.as_mut().map(|s| s.reset()));
        }
        Ev::Key(code, pressed) => {
            if !pressed {
                // only the repeating key's release stops it (not a Shift let go meanwhile)
                if REPEAT.with(|r| r.borrow().as_ref().is_some_and(|(k, _)| *k == code)) {
                    stop_repeat();
                }
                return;
            }
            let Some((ev, composed)) = key_event(code) else { return };
            deliver_key(&ev);
            let repeats = !composed && ctx(|c| c.xkb.as_ref().is_some_and(|x| x.get_keymap().key_repeats(code)));
            if repeats {
                start_repeat(code, ev);
            }
        }
        Ev::Pointer(id, pe) => {
            if let Some(s) = surface(&id) {
                let handler = s.on_pointer.borrow().clone();
                if let Some(h) = handler {
                    h(pe);
                }
            }
        }
        Ev::Outputs => {
            let listeners = OUTPUT_LISTENERS.with(|l| l.borrow().clone());
            for l in listeners {
                l();
            }
        }
        Ev::OutputGone(o) => {
            // its lock surfaces die with it (alive() false), so a replug gets fresh ones
            let gone: Vec<Rc<Inner>> =
                SURFACES.with(|m| m.borrow().values().filter_map(Weak::upgrade).filter(|s| s.output.as_ref() == Some(&o)).collect());
            for s in gone {
                s.destroy_role();
            }
            if o.version() >= 3 {
                o.release();
            }
        }
        Ev::Captured(id, ok) => screencopy::finish(&id, ok),
        Ev::Lock(e) => {
            // unlock() destroys the lock first, so Finished only ever comes from the compositor
            if e == LockEvent::Finished {
                eprintln!("flurry: the compositor refused or ended the session lock");
                ctx(|c| {
                    if let Some(l) = c.lock.take() {
                        l.destroy();
                    }
                });
            }
            let l = LOCK_LISTENER.with(|l| l.borrow().clone());
            if let Some(l) = l {
                l(e);
            }
        }
    }
}

/// The event a key press gives, and whether compose produced it; None while a compose sequence runs.
fn key_event(code: xkb::Keycode) -> Option<(KeyEvent, bool)> {
    ctx(|c| {
        let x = c.xkb.as_ref()?;
        let mut sym = x.key_get_one_sym(code);
        let mut ch = sym.key_char();
        let mut composed = false;
        if let Some(cs) = &mut c.compose
            && cs.feed(sym) == compose::FeedResult::Accepted
        {
            match cs.status() {
                compose::Status::Composing => return None,
                compose::Status::Cancelled => {
                    cs.reset();
                    return None;
                }
                compose::Status::Composed => {
                    sym = cs.keysym().unwrap_or(sym);
                    ch = sym.key_char().or_else(|| {
                        // no single keysym for it: the result as text, kept only if it's one char
                        let mut s = cs.utf8().unwrap_or_default();
                        let mut it = s.chars();
                        let one = it.next().filter(|_| it.next().is_none());
                        crate::ui::entry::wipe(&mut s);
                        one
                    });
                    cs.reset();
                    composed = true;
                }
                compose::Status::Nothing => {}
            }
        }
        let active = |m| x.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
        let ch = ch.filter(|ch| !ch.is_control());
        Some((KeyEvent { sym, ch, ctrl: active(xkb::MOD_NAME_CTRL), shift: active(xkb::MOD_NAME_SHIFT), alt: active(xkb::MOD_NAME_ALT) }, composed))
    })
}

fn deliver_key(ev: &KeyEvent) {
    let Some(id) = FOCUS.with(|f| f.borrow().clone()) else { return };
    let Some(s) = surface(&id) else { return };
    let handler = s.on_key.borrow().clone();
    if let Some(h) = handler {
        h(ev);
    }
}

fn stop_repeat() {
    REPEAT.with(|r| {
        if let Some((_, t)) = r.borrow_mut().take() {
            t.cancel();
        }
    });
}

fn start_repeat(code: xkb::Keycode, ev: KeyEvent) {
    stop_repeat();
    let (rate, delay) = ctx(|c| c.repeat);
    if rate <= 0 {
        return;
    }
    let interval = (1000 / rate).max(1) as u64;
    fn tick(code: xkb::Keycode, ev: KeyEvent, interval: u64) {
        deliver_key(&ev);
        let t = runtime::timeout(interval, move || tick(code, ev, interval));
        REPEAT.with(|r| *r.borrow_mut() = Some((code, t)));
    }
    let t = runtime::timeout(delay as u64, move || tick(code, ev, interval));
    REPEAT.with(|r| *r.borrow_mut() = Some((code, t)));
}

/// Redraws the surface with keyboard focus (input that lands asynchronously, like a paste).
pub fn redraw_focused() {
    if let Some(s) = FOCUS.with(|f| f.borrow().clone()).and_then(|id| surface(&id)) {
        s.queue_redraw();
    }
}

pub fn outputs() -> Vec<Output> {
    ctx(|c| c.outputs.iter().filter(|o| o.done).cloned().collect())
}

/// Calls `f` whenever outputs are added or removed.
pub fn on_outputs(f: impl Fn() + 'static) {
    OUTPUT_LISTENERS.with(|l| l.borrow_mut().push(Rc::new(f)));
}

// ---------------------------------------------------------------- protocol dispatch

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Wl {
    fn event(_: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &GlobalListContents, _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_registry::Event::Global { name, interface, version } if interface == "wl_output" => {
                let wl: wl_output::WlOutput = reg.bind(name, version.min(4), qh, name);
                ctx(|c| c.outputs.push(Output::new(wl)));
            }
            wl_registry::Event::GlobalRemove { name } => {
                let removed = ctx(|c| {
                    let i = c.outputs.iter().position(|o| o.wl.data::<u32>() == Some(&name))?;
                    Some(c.outputs.remove(i).wl)
                });
                if let Some(o) = removed {
                    push(Ev::OutputGone(o));
                    push(Ev::Outputs);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for Wl {
    fn event(_: &mut Self, o: &wl_output::WlOutput, ev: wl_output::Event, _: &u32, _: &Connection, _: &QueueHandle<Self>) {
        ctx(|c| {
            // listed when bound; events still in flight after its GlobalRemove must not bring it back
            let Some(out) = c.outputs.iter_mut().find(|x| x.wl == *o) else { return };
            match ev {
                wl_output::Event::Name { name } => out.name = name,
                wl_output::Event::Scale { factor } => out.scale = factor,
                wl_output::Event::Done => {
                    out.done = true;
                    push(Ev::Outputs);
                }
                _ => {}
            }
        });
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Wl {
    fn event(_: &mut Self, seat: &wl_seat::WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = ev {
            ctx(|c| {
                // capability dropped (last keyboard/pointer device removed): the old object is dead
                if !caps.contains(wl_seat::Capability::Keyboard)
                    && let Some(k) = c.keyboard.take()
                {
                    k.release();
                    // no Leave comes for a dead keyboard: drop focus and stop a running key repeat
                    push(Ev::KeyboardLeave);
                }
                if !caps.contains(wl_seat::Capability::Pointer) {
                    if let Some(d) = c.cursor_device.take() {
                        d.destroy();
                    }
                    if let Some(p) = c.pointer.take() {
                        p.release();
                    }
                }
                if caps.contains(wl_seat::Capability::Keyboard) && c.keyboard.is_none() {
                    c.keyboard = Some(seat.get_keyboard(qh, ()));
                }
                if caps.contains(wl_seat::Capability::Pointer) && c.pointer.is_none() {
                    let p = seat.get_pointer(qh, ());
                    c.cursor_device = c.cursor_shape.as_ref().map(|m| m.get_pointer(&p, qh, ()));
                    c.pointer = Some(p);
                }
            });
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Wl {
    fn event(_: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wl_keyboard::Event::Keymap { fd, size, .. } => ctx(|c| {
                let km = unsafe { xkb::Keymap::new_from_fd(&c.xkb_ctx, fd, size as usize, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS) };
                if let Ok(Some(km)) = km {
                    c.xkb = Some(xkb::State::new(&km));
                }
            }),
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => ctx(|c| {
                if let Some(x) = &mut c.xkb {
                    x.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }),
            wl_keyboard::Event::RepeatInfo { rate, delay } => ctx(|c| c.repeat = (rate, delay)),
            wl_keyboard::Event::Enter { surface, .. } => push(Ev::KeyboardEnter(surface.id())),
            wl_keyboard::Event::Leave { .. } => push(Ev::KeyboardLeave),
            wl_keyboard::Event::Key { key, state, .. } => {
                push(Ev::Key(xkb::Keycode::new(key + 8), state == WEnum::Value(wl_keyboard::KeyState::Pressed)))
            }
            _ => {}
        }
    }
}

thread_local! {
    static AXIS: RefCell<(f64, Option<i32>)> = const { RefCell::new((0.0, None)) };
}

impl Dispatch<wl_pointer::WlPointer, ()> for Wl {
    fn event(_: &mut Self, _: &wl_pointer::WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wl_pointer::Event::Enter { serial, surface: s, surface_x, surface_y } => {
                let inner = surface(&s.id());
                let hidden = inner.as_ref().is_some_and(|i| i.is_lock());
                let tracked = inner.as_ref().is_some_and(|i| i.track_pointer.get());
                if tracked {
                    push(Ev::Pointer(s.id(), PointerEvent::Motion { x: surface_x, y: surface_y }));
                }
                ctx(|c| {
                    if hidden {
                        // lockscreen hides the cursor
                        if let Some(p) = &c.pointer {
                            p.set_cursor(serial, None, 0, 0);
                        }
                    } else if let Some(d) = &c.cursor_device {
                        d.set_shape(serial, if tracked { wp_cursor_shape_device_v1::Shape::Crosshair } else { wp_cursor_shape_device_v1::Shape::Default });
                    }
                });
                HOVER.with(|h| *h.borrow_mut() = Some((s.id(), surface_x, surface_y)));
            }
            wl_pointer::Event::Leave { .. } => HOVER.with(|h| *h.borrow_mut() = None),
            // only tracked for where a scroll lands
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => HOVER.with(|h| {
                if let Some((id, x, y)) = h.borrow_mut().as_mut() {
                    *x = surface_x;
                    *y = surface_y;
                    if surface(id).is_some_and(|i| i.track_pointer.get()) {
                        push(Ev::Pointer(id.clone(), PointerEvent::Motion { x: surface_x, y: surface_y }));
                    }
                }
            }),
            wl_pointer::Event::Button { button, state, .. } => {
                if let Some((id, x, y)) = HOVER.with(|h| h.borrow().clone()) {
                    let pressed = state == WEnum::Value(wl_pointer::ButtonState::Pressed);
                    push(Ev::Pointer(id, PointerEvent::Button { x, y, left: button == 0x110, pressed }));
                }
            }
            wl_pointer::Event::Axis { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), value, .. } => {
                AXIS.with(|a| a.borrow_mut().0 += value);
            }
            wl_pointer::Event::AxisValue120 { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), value120 } => {
                AXIS.with(|a| {
                    let mut a = a.borrow_mut();
                    a.1 = Some(a.1.unwrap_or(0) + value120);
                });
            }
            wl_pointer::Event::AxisDiscrete { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), discrete } => {
                AXIS.with(|a| {
                    let mut a = a.borrow_mut();
                    a.1 = Some(a.1.unwrap_or(0) + discrete * 120);
                });
            }
            wl_pointer::Event::Frame => {
                let (value, steps) = AXIS.with(|a| std::mem::take(&mut *a.borrow_mut()));
                if value == 0.0 && steps.is_none() {
                    return;
                }
                if let Some((id, x, y)) = HOVER.with(|h| h.borrow().clone()) {
                    // GDK: wheel steps are discrete units, smooth deltas are px / 10
                    let (dy, wheel) = match steps {
                        Some(s) => (s as f64 / 120.0, true),
                        None => (value / 10.0, false),
                    };
                    push(Ev::Pointer(id, PointerEvent::Scroll { x, y, dy, wheel }));
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(Wl: ignore wl_compositor::WlCompositor);
delegate_noop!(Wl: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(Wl: ignore wl_subsurface::WlSubsurface);
delegate_noop!(Wl: ignore wl_shm::WlShm);
delegate_noop!(Wl: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Wl: ignore wl_region::WlRegion);
delegate_noop!(Wl: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(Wl: ignore ext_session_lock_manager_v1::ExtSessionLockManagerV1);
delegate_noop!(Wl: ignore wp_viewporter::WpViewporter);
delegate_noop!(Wl: ignore wp_viewport::WpViewport);
delegate_noop!(Wl: ignore wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1);
delegate_noop!(Wl: ignore wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(Wl: ignore wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);
