// Wayland client: outputs, seat (xkb keyboard with repeat, pointer), layer-shell and session-lock
// surfaces, shm double buffers, single-pixel dim backgrounds, clipboard paste.
// Protocol callbacks only queue events; they're handled after dispatch so handlers can freely
// create/destroy surfaces.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use tiny_skia::{Pixmap, Transform};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_keyboard, wl_output,
    wl_pointer, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, backend::ObjectId, delegate_noop};
use wayland_protocols::ext::session_lock::v1::client::{ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1};
use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1;
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use xkbcommon::xkb;

use super::node::Node;
use super::runtime;

pub use zwlr_layer_shell_v1::Layer;
pub use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity as Keymode};

pub struct Wl;

#[derive(Clone)]
pub struct Output {
    pub wl: wl_output::WlOutput,
    pub name: String,
    pub scale: i32,
    pub size: (i32, i32),
    pub done: bool,
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
    seat: wl_seat::WlSeat,
    ddm: Option<wl_data_device_manager::WlDataDeviceManager>,
    data_device: Option<wl_data_device::WlDataDevice>,
    selection: Option<(wl_data_offer::WlDataOffer, Vec<String>)>,
    offers: HashMap<ObjectId, Vec<String>>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    cursor_device: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    xkb_ctx: xkb::Context,
    xkb: Option<xkb::State>,
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
    static REPEAT: RefCell<Option<runtime::Timer>> = const { RefCell::new(None) };
    static OUTPUT_LISTENERS: RefCell<Vec<Rc<dyn Fn()>>> = RefCell::default();
    static LOCK_LISTENER: RefCell<Option<Rc<dyn Fn(LockEvent)>>> = RefCell::default();
}

fn ctx<R>(f: impl FnOnce(&mut Ctx) -> R) -> R {
    CTX.with(|c| f(c.borrow_mut().as_mut().expect("wayland not connected")))
}

#[derive(Clone, Debug)]
pub struct KeyEvent {
    pub sym: xkb::Keysym,
    /// Text the key types (without ctrl translation), empty for non-printing keys
    pub text: String,
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
    Motion { x: f64, y: f64 },
    Button { x: f64, y: f64, pressed: bool },
    /// dy in GDK scroll units; `wheel` true for discrete mouse wheel steps
    Scroll { x: f64, y: f64, dy: f64, wheel: bool },
    Leave,
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
    Lock(LockEvent),
}

fn push(ev: Ev) {
    EVENTS.with(|e| e.borrow_mut().push(ev));
}

pub fn connect() -> bool {
    let Ok(conn) = Connection::connect_to_env() else { return false };
    let Ok((globals, mut queue)) = registry_queue_init::<Wl>(&conn) else { return false };
    let qh = queue.handle();
    let bind = |v: std::ops::RangeInclusive<u32>| v;
    let c0 = Ctx {
        compositor: globals.bind(&qh, bind(4..=6), ()).expect("wl_compositor"),
        subcompositor: globals.bind(&qh, bind(1..=1), ()).expect("wl_subcompositor"),
        shm: globals.bind(&qh, bind(1..=1), ()).expect("wl_shm"),
        layer_shell: globals.bind(&qh, bind(4..=4), ()).expect("zwlr_layer_shell_v1"),
        lock_mgr: globals.bind(&qh, bind(1..=1), ()).ok(),
        viewporter: globals.bind(&qh, bind(1..=1), ()).expect("wp_viewporter"),
        single_pixel: globals.bind(&qh, bind(1..=1), ()).expect("wp_single_pixel_buffer_manager_v1"),
        cursor_shape: globals.bind(&qh, bind(1..=1), ()).ok(),
        seat: globals.bind(&qh, bind(5..=9), ()).expect("wl_seat"),
        ddm: globals.bind(&qh, bind(3..=3), ()).ok(),
        data_device: None,
        selection: None,
        offers: HashMap::new(),
        keyboard: None,
        pointer: None,
        cursor_device: None,
        xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb: None,
        repeat: (25, 600),
        outputs: vec![],
        lock: None,
        conn: conn.clone(),
        qh: qh.clone(),
    };
    globals.contents().with_list(|list| {
        for g in list.iter().filter(|g| g.interface == "wl_output") {
            let _: wl_output::WlOutput = globals.registry().bind(g.name, g.version.min(4), &qh, g.name);
        }
    });
    if let Some(ddm) = &c0.ddm {
        let dd = ddm.get_data_device(&c0.seat, &qh, ());
        CTX.with(|c| *c.borrow_mut() = Some(Ctx { data_device: Some(dd), ..c0 }));
    } else {
        CTX.with(|c| *c.borrow_mut() = Some(c0));
    }
    let _ = queue.roundtrip(&mut Wl);
    let _ = queue.roundtrip(&mut Wl);
    QUEUE.with(|q| *q.borrow_mut() = Some(queue));

    let fd = conn.as_fd().as_raw_fd();
    runtime::watch(fd, read_events);
    runtime::before_poll(|| {
        dispatch_pending();
        let _ = ctx(|c| c.conn.flush());
    });
    handle_events();
    true
}

fn read_events() {
    let guard = ctx(|c| c.conn.prepare_read());
    if let Some(g) = guard
        && let Err(wayland_client::backend::WaylandError::Io(e)) = g.read()
        && e.kind() != std::io::ErrorKind::WouldBlock
    {
        // compositor gone: exit so the session (systemd) can restart us with the next one
        eprintln!("flurry: lost the Wayland connection: {e}");
        std::process::exit(1);
    }
    dispatch_pending();
}

fn dispatch_pending() {
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
        for ev in events {
            handle(ev);
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
                s.configured.set(true);
                if w > 0 && h > 0 {
                    s.size.set((w, h));
                }
                s.render();
            }
        }
        Ev::Closed(id) => {
            if let Some(s) = surface(&id) {
                s.destroy_role();
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
        }
        Ev::Key(code, pressed) => {
            if !pressed {
                stop_repeat();
                return;
            }
            let Some(ev) = key_event(code) else { return };
            deliver_key(&ev);
            let repeats = ctx(|c| c.xkb.as_ref().map(|x| x.get_keymap().key_repeats(code)).unwrap_or(false));
            if repeats {
                start_repeat(ev);
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
        Ev::Lock(e) => {
            if e == LockEvent::Finished {
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

fn key_event(code: xkb::Keycode) -> Option<KeyEvent> {
    ctx(|c| {
        let x = c.xkb.as_ref()?;
        let sym = x.key_get_one_sym(code);
        let active = |m| x.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
        let text = sym.key_char().filter(|ch| !ch.is_control()).map(String::from).unwrap_or_default();
        Some(KeyEvent { sym, text, ctrl: active(xkb::MOD_NAME_CTRL), shift: active(xkb::MOD_NAME_SHIFT), alt: active(xkb::MOD_NAME_ALT) })
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
        if let Some(t) = r.borrow_mut().take() {
            t.cancel();
        }
    });
}

fn start_repeat(ev: KeyEvent) {
    stop_repeat();
    let (rate, delay) = ctx(|c| c.repeat);
    if rate <= 0 {
        return;
    }
    let interval = (1000 / rate).max(1) as u64;
    fn tick(ev: KeyEvent, interval: u64) {
        deliver_key(&ev);
        let t = runtime::timeout(interval, move || tick(ev.clone(), interval));
        REPEAT.with(|r| *r.borrow_mut() = Some(t));
    }
    let t = runtime::timeout(delay as u64, move || tick(ev.clone(), interval));
    REPEAT.with(|r| *r.borrow_mut() = Some(t));
}

pub fn outputs() -> Vec<Output> {
    ctx(|c| c.outputs.iter().filter(|o| o.done).cloned().collect())
}

/// Calls `f` whenever outputs are added or removed.
pub fn on_outputs(f: impl Fn() + 'static) {
    OUTPUT_LISTENERS.with(|l| l.borrow_mut().push(Rc::new(f)));
}

// ---------------------------------------------------------------- surfaces

pub struct LayerSpec {
    pub namespace: &'static str,
    pub layer: Layer,
    pub anchor: Anchor,
    pub keymode: Keymode,
    /// -1 ignores other zones (Astal IGNORE), 0 respects them (NORMAL), >0 reserves (EXCLUSIVE, set from size)
    pub exclusive: i32,
    pub output: Option<wl_output::WlOutput>,
    /// fullscreen dim color (r, g, b, a in 0-1); content then lives in a subsurface sized to what it paints
    pub dim: Option<[f32; 4]>,
    pub input: bool,
    /// Fixed layer size; 0 = from content (or stretched when anchored on both sides)
    pub width: u32,
    pub height: u32,
}

impl Default for LayerSpec {
    fn default() -> Self {
        Self {
            namespace: "flurry",
            layer: Layer::Overlay,
            anchor: Anchor::empty(),
            keymode: Keymode::None,
            exclusive: 0,
            output: None,
            dim: None,
            input: true,
            width: 0,
            height: 0,
        }
    }
}

enum Role {
    Layer(zwlr_layer_surface_v1::ZwlrLayerSurfaceV1),
    Lock(ext_session_lock_surface_v1::ExtSessionLockSurfaceV1),
    None,
}

pub type BuildFn = Rc<dyn Fn(f32, f32) -> Node>;

struct Buffer {
    wl: wl_buffer::WlBuffer,
    ptr: *mut u8,
    len: usize,
    size: (u32, u32),
    busy: Arc<AtomicBool>,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        self.wl.destroy();
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

pub struct Inner {
    wl: wl_surface::WlSurface,
    role: RefCell<Role>,
    spec_anchor: Anchor,
    content: Option<(wl_surface::WlSurface, wl_subsurface::WlSubsurface)>,
    viewport: Option<wp_viewport::WpViewport>,
    dim: Option<[f32; 4]>,
    size: Cell<(u32, u32)>,
    fixed: (u32, u32),
    exclusive: i32,
    scale: Cell<i32>,
    configured: Cell<bool>,
    buffers: RefCell<Vec<Buffer>>,
    frame_pending: Cell<bool>,
    redraw_queued: Cell<bool>,
    build: RefCell<Option<BuildFn>>,
    on_key: RefCell<Option<Rc<dyn Fn(&KeyEvent)>>>,
    on_pointer: RefCell<Option<Rc<dyn Fn(PointerEvent)>>>,
    after_render: RefCell<Option<Rc<dyn Fn(&Node)>>>,
    /// Offset of the content subsurface, to map pointer coordinates
    content_origin: Cell<(f32, f32)>,
    image: RefCell<Option<Rc<ShmImage>>>,
    img_viewport: RefCell<Option<wp_viewport::WpViewport>>,
    dim_buffer: RefCell<Option<wl_buffer::WlBuffer>>,
}

pub struct Surface(Rc<Inner>);

impl Surface {
    pub fn layer(spec: LayerSpec, build: BuildFn) -> Surface {
        let s = ctx(|c| {
            let wl = c.compositor.create_surface(&c.qh, ());
            let layer = c.layer_shell.get_layer_surface(&wl, spec.output.as_ref(), spec.layer, spec.namespace.into(), &c.qh, ());
            layer.set_anchor(spec.anchor);
            layer.set_keyboard_interactivity(spec.keymode);
            layer.set_exclusive_zone(spec.exclusive.min(0));
            layer.set_size(spec.width, spec.height);
            let (content, viewport) = if spec.dim.is_some() {
                let sub_wl = c.compositor.create_surface(&c.qh, ());
                let sub = c.subcompositor.get_subsurface(&sub_wl, &wl, &c.qh, ());
                sub.set_desync();
                let region = c.compositor.create_region(&c.qh, ());
                sub_wl.set_input_region(Some(&region));
                region.destroy();
                (Some((sub_wl, sub)), Some(c.viewporter.get_viewport(&wl, &c.qh, ())))
            } else {
                (None, None)
            };
            if !spec.input {
                let region = c.compositor.create_region(&c.qh, ());
                wl.set_input_region(Some(&region));
                region.destroy();
            }
            let scale = spec
                .output
                .as_ref()
                .and_then(|o| c.outputs.iter().find(|x| x.wl == *o))
                .or(c.outputs.first())
                .map(|o| o.scale)
                .unwrap_or(1);
            Inner {
                wl,
                role: RefCell::new(Role::Layer(layer)),
                spec_anchor: spec.anchor,
                content,
                viewport,
                dim: spec.dim,
                size: Cell::new((spec.width.max(1), spec.height.max(1))),
                fixed: (spec.width, spec.height),
                exclusive: spec.exclusive,
                scale: Cell::new(scale),
                configured: Cell::new(false),
                buffers: RefCell::default(),
                frame_pending: Cell::new(false),
                redraw_queued: Cell::new(false),
                build: RefCell::new(Some(build)),
                on_key: RefCell::default(),
                on_pointer: RefCell::default(),
                after_render: RefCell::default(),
                content_origin: Cell::new((0.0, 0.0)),
                image: RefCell::default(),
                img_viewport: RefCell::default(),
                dim_buffer: RefCell::default(),
            }
        });
        let s = Rc::new(s);
        SURFACES.with(|m| m.borrow_mut().insert(s.wl.id(), Rc::downgrade(&s)));
        // initial commit: size from content first, so the configure already has the right size
        s.apply_content_size();
        s.wl.commit();
        Surface(s)
    }

    pub fn on_key(&self, f: impl Fn(&KeyEvent) + 'static) {
        *self.0.on_key.borrow_mut() = Some(Rc::new(f));
    }
    pub fn on_pointer(&self, f: impl Fn(PointerEvent) + 'static) {
        *self.0.on_pointer.borrow_mut() = Some(Rc::new(f));
    }
    /// Called with the laid-out tree after every render (focus/scroll bookkeeping).
    pub fn after_render(&self, f: impl Fn(&Node) + 'static) {
        *self.0.after_render.borrow_mut() = Some(Rc::new(f));
    }
    /// Rebuilds and repaints on the next idle pass (coalesced).
    pub fn redraw(&self) {
        let s = self.0.clone();
        if s.redraw_queued.replace(true) {
            return;
        }
        runtime::idle(move || {
            s.redraw_queued.set(false);
            s.render();
        });
    }
    pub fn set_keymode(&self, k: Keymode) {
        if let Role::Layer(l) = &*self.0.role.borrow() {
            l.set_keyboard_interactivity(k);
            self.0.wl.commit();
        }
    }
    pub fn set_exclusive(&self, zone: i32) {
        if let Role::Layer(l) = &*self.0.role.borrow() {
            l.set_exclusive_zone(zone);
            self.0.wl.commit();
        }
    }
    pub fn size(&self) -> (u32, u32) {
        self.0.size.get()
    }
    pub fn scale(&self) -> i32 {
        self.0.scale.get()
    }
    pub fn has_keyboard(&self) -> bool {
        FOCUS.with(|f| f.borrow().as_ref() == Some(&self.0.wl.id()))
    }
    /// Shows a shared image (wallpaper) cover-cropped to the surface, without copying it.
    pub fn set_image(&self, img: Rc<ShmImage>) {
        *self.0.image.borrow_mut() = Some(img);
        self.0.render();
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // the owning handle: unmap now even if a handler still holds the inner state
        self.0.destroy_role();
    }
}

impl Inner {
    fn destroy_role(&self) {
        let role = std::mem::replace(&mut *self.role.borrow_mut(), Role::None);
        match role {
            Role::Layer(l) => l.destroy(),
            Role::Lock(l) => l.destroy(),
            Role::None => return,
        }
        if let Some((s, sub)) = &self.content {
            sub.destroy();
            s.destroy();
        }
        if let Some(b) = self.dim_buffer.borrow_mut().take() {
            b.destroy();
        }
        if let Some(v) = self.img_viewport.borrow_mut().take() {
            v.destroy();
        }
        if let Some(v) = &self.viewport {
            v.destroy();
        }
        self.buffers.borrow_mut().clear();
        self.wl.destroy();
        SURFACES.with(|m| m.borrow_mut().remove(&self.wl.id()));
    }

    fn alive(&self) -> bool {
        !matches!(*self.role.borrow(), Role::None)
    }

    /// Sizes non-stretched axes from the content's natural size (GTK layer windows do the same).
    fn apply_content_size(&self) {
        let Role::Layer(layer) = &*self.role.borrow() else { return };
        let a = self.spec_anchor;
        let stretch_w = a.contains(Anchor::Left) && a.contains(Anchor::Right);
        let stretch_h = a.contains(Anchor::Top) && a.contains(Anchor::Bottom);
        if (stretch_w || self.fixed.0 > 0) && (stretch_h || self.fixed.1 > 0) {
            return;
        }
        let Some(build) = self.build.borrow().clone() else { return };
        let (cw, ch) = self.size.get();
        let node = build(cw as f32, ch as f32);
        let (_, nw) = node.measure_w();
        let nw = nw + node.style.margin_h();
        let w = if stretch_w { 0 } else if self.fixed.0 > 0 { self.fixed.0 } else { nw.ceil().max(1.0) as u32 };
        let width_for_h = if stretch_w { cw as f32 } else { w as f32 };
        let (_, nh) = node.measure_h(width_for_h - node.style.margin_h());
        let nh = nh + node.style.margin_v();
        let h = if stretch_h { 0 } else if self.fixed.1 > 0 { self.fixed.1 } else { nh.ceil().max(1.0) as u32 };
        layer.set_size(w, h);
        if self.exclusive > 0 {
            let zone = if a.contains(Anchor::Left) ^ a.contains(Anchor::Right) { w } else { h };
            layer.set_exclusive_zone(zone as i32);
        }
        let (old_w, old_h) = self.size.get();
        let new = (if w == 0 { old_w } else { w }, if h == 0 { old_h } else { h });
        if new != (old_w, old_h) {
            self.size.set(new);
        }
    }

    fn render(self: &Rc<Self>) {
        if !self.alive() || !self.configured.get() {
            return;
        }
        if self.frame_pending.get() {
            // a frame is in flight; the frame callback re-renders
            self.redraw_queued.set(true);
            return;
        }
        if let Some(img) = self.image.borrow().clone() {
            self.attach_image(&img);
            return;
        }
        let Some(build) = self.build.borrow().clone() else { return };
        let before = self.size.get();
        self.apply_content_size();
        if self.size.get() != before && matches!(*self.role.borrow(), Role::Layer(_)) {
            // wait for the configure with the new size
            self.wl.commit();
            return;
        }
        let (w, h) = self.size.get();
        let scale = self.scale.get().max(1);
        let mut node = build(w as f32, h as f32);
        node.allocate(0.0, 0.0, w as f32, h as f32);

        let (target, ox, oy, bw, bh) = if let Some((sub_wl, sub)) = &self.content {
            // dim on the main surface, content in a subsurface clipped to what it paints
            let [r, g, b, a] = self.dim.unwrap();
            let conv = |v: f32| (v * a * u32::MAX as f32) as u32;
            // created once and kept: a buffer destroyed before the commit would unmap the dim
            let mut dim = self.dim_buffer.borrow_mut();
            if dim.is_none() {
                *dim = Some(ctx(|c| c.single_pixel.create_u32_rgba_buffer(conv(r), conv(g), conv(b), (a * u32::MAX as f32) as u32, &c.qh, ())));
                self.wl.attach(dim.as_ref(), 0, 0);
                self.wl.damage(0, 0, i32::MAX, i32::MAX);
            }
            drop(dim);
            if let Some(v) = &self.viewport {
                v.set_destination(w as i32, h as i32);
            }
            let bounds = node.paint_bounds();
            let x0 = bounds.left().floor().max(0.0);
            let y0 = bounds.top().floor().max(0.0);
            let x1 = bounds.right().ceil().min(w as f32);
            let y1 = bounds.bottom().ceil().min(h as f32);
            sub.set_position(x0 as i32, y0 as i32);
            (sub_wl.clone(), x0, y0, (x1 - x0).max(1.0) as u32, (y1 - y0).max(1.0) as u32)
        } else {
            (self.wl.clone(), 0.0, 0.0, w, h)
        };
        self.content_origin.set((ox, oy));

        let (pw, ph) = (bw * scale as u32, bh * scale as u32);
        let Some(mut pixmap) = Pixmap::new(pw, ph) else { return };
        let ts = Transform::from_scale(scale as f32, scale as f32).pre_translate(-ox, -oy);
        node.paint(&mut pixmap.as_mut(), ts);
        if let Some(cb) = self.after_render.borrow().clone() {
            cb(&node);
        }

        let Some(buf) = self.take_buffer(pw, ph) else { return };
        // tiny-skia is premultiplied RGBA, wl_shm ARGB8888 is BGRA in memory
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.0, buf.1) };
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(pixmap.data().chunks_exact(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }
        target.set_buffer_scale(scale);
        target.attach(Some(&buf.2), 0, 0);
        target.damage_buffer(0, 0, pw as i32, ph as i32);
        self.request_frame(&target);
        target.commit();
        if target != self.wl {
            self.wl.commit();
        }
    }

    fn request_frame(self: &Rc<Self>, target: &wl_surface::WlSurface) {
        self.frame_pending.set(true);
        ctx(|c| target.frame(&c.qh, self.wl.id()));
    }

    fn take_buffer(&self, w: u32, h: u32) -> Option<(*mut u8, usize, wl_buffer::WlBuffer)> {
        let mut bufs = self.buffers.borrow_mut();
        bufs.retain(|b| b.size == (w, h) || b.busy.load(Relaxed));
        if let Some(b) = bufs.iter().find(|b| !b.busy.load(Relaxed) && b.size == (w, h)) {
            b.busy.store(true, Relaxed);
            return Some((b.ptr, b.len, b.wl.clone()));
        }
        let b = new_buffer(w, h)?;
        b.busy.store(true, Relaxed);
        let out = (b.ptr, b.len, b.wl.clone());
        bufs.push(b);
        Some(out)
    }

    fn attach_image(self: &Rc<Self>, img: &ShmImage) {
        let (w, h) = self.size.get();
        let v = self.img_viewport.borrow_mut().get_or_insert_with(|| ctx(|c| c.viewporter.get_viewport(&self.wl, &c.qh, ()))).clone();
        let (sx, sy, sw, sh) = super::image::cover(img.width as f32, img.height as f32, w as f32, h as f32);
        v.set_source(sx as f64, sy as f64, sw as f64, sh as f64);
        v.set_destination(w as i32, h as i32);
        self.wl.attach(Some(&img.buffer), 0, 0);
        self.wl.damage_buffer(0, 0, img.width as i32, img.height as i32);
        self.wl.commit();
    }
}

fn memfd(len: usize) -> Option<OwnedFd> {
    unsafe {
        let fd = libc::memfd_create(c"flurry".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING);
        if fd < 0 || libc::ftruncate(fd, len as libc::off_t) < 0 {
            return None;
        }
        Some(OwnedFd::from_raw_fd(fd))
    }
}

fn new_buffer(w: u32, h: u32) -> Option<Buffer> {
    new_buffer_fmt(w, h, wl_shm::Format::Argb8888)
}

fn new_buffer_fmt(w: u32, h: u32, format: wl_shm::Format) -> Option<Buffer> {
    let len = (w * h * 4) as usize;
    let fd = memfd(len)?;
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if ptr == libc::MAP_FAILED {
        return None;
    }
    let busy = Arc::new(AtomicBool::new(false));
    let wl = ctx(|c| {
        let pool = c.shm.create_pool(fd.as_fd(), len as i32, &c.qh, ());
        let b = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, format, &c.qh, busy.clone());
        pool.destroy();
        b
    });
    Some(Buffer { wl, ptr: ptr.cast(), len, size: (w, h), busy })
}

/// An immutable full-size image living in one shm buffer (ABGR8888 = tiny-skia's RGBA byte order),
/// attached to any number of surfaces and drawn from directly: the only copy of the wallpaper.
pub struct ShmImage {
    pub buffer: wl_buffer::WlBuffer,
    pub width: u32,
    pub height: u32,
    ptr: *mut u8,
    len: usize,
}

impl ShmImage {
    /// `fill` writes premultiplied RGBA pixels into the buffer.
    pub fn new(w: u32, h: u32, fill: impl FnOnce(&mut [u8])) -> Option<Rc<ShmImage>> {
        let b = new_buffer_fmt(w, h, wl_shm::Format::Abgr8888)?;
        fill(unsafe { std::slice::from_raw_parts_mut(b.ptr, b.len) });
        let img = ShmImage { buffer: b.wl.clone(), width: w, height: h, ptr: b.ptr, len: b.len };
        std::mem::forget(b); // ownership moves to ShmImage
        Some(Rc::new(img))
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

// ---------------------------------------------------------------- session lock

/// Locks the session; `on_event` gets Locked / Finished (compositor refused or lock ended).
pub fn lock(on_event: impl Fn(LockEvent) + 'static) -> bool {
    LOCK_LISTENER.with(|l| *l.borrow_mut() = Some(Rc::new(on_event)));
    ctx(|c| {
        let Some(m) = &c.lock_mgr else { return false };
        if c.lock.is_some() {
            return false;
        }
        c.lock = Some(m.lock(&c.qh, ()));
        true
    })
}

pub fn is_locking() -> bool {
    ctx(|c| c.lock.is_some())
}

/// Creates the lock surface for `output`.
pub fn lock_surface(output: &wl_output::WlOutput, build: BuildFn) -> Option<Surface> {
    let inner = ctx(|c| {
        let lock = c.lock.as_ref()?;
        let wl = c.compositor.create_surface(&c.qh, ());
        let ls = lock.get_lock_surface(&wl, output, &c.qh, ());
        let scale = c.outputs.iter().find(|o| o.wl == *output).map(|o| o.scale).unwrap_or(1);
        Some(Inner {
            wl,
            role: RefCell::new(Role::Lock(ls)),
            spec_anchor: Anchor::all(),
            content: None,
            viewport: None,
            dim: None,
            size: Cell::new((1, 1)),
            fixed: (0, 0),
            exclusive: 0,
            scale: Cell::new(scale),
            configured: Cell::new(false),
            buffers: RefCell::default(),
            frame_pending: Cell::new(false),
            redraw_queued: Cell::new(false),
            build: RefCell::new(Some(build)),
            on_key: RefCell::default(),
            on_pointer: RefCell::default(),
            after_render: RefCell::default(),
            content_origin: Cell::new((0.0, 0.0)),
            image: RefCell::default(),
            img_viewport: RefCell::default(),
            dim_buffer: RefCell::default(),
        })
    })?;
    let s = Rc::new(inner);
    SURFACES.with(|m| m.borrow_mut().insert(s.wl.id(), Rc::downgrade(&s)));
    Some(Surface(s))
}

pub fn unlock() {
    ctx(|c| {
        if let Some(l) = c.lock.take() {
            l.unlock_and_destroy();
        }
        let _ = c.conn.flush();
    });
}

// ---------------------------------------------------------------- clipboard

/// Reads the clipboard as text (Ctrl+V into entries).
pub fn paste(cb: impl FnOnce(String) + 'static) {
    let Some((offer, mimes)) = ctx(|c| c.selection.clone()) else { return };
    let Some(mime) = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"].into_iter().find(|m| mimes.iter().any(|x| x == m)) else {
        return;
    };
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return;
    }
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    offer.receive(mime.into(), wr.as_fd());
    drop(wr);
    let _ = ctx(|c| c.conn.flush());
    let raw = rd.as_raw_fd();
    let data = Rc::new(RefCell::new(Vec::new()));
    let cb = RefCell::new(Some(cb));
    runtime::watch(raw, move || {
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(raw, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            data.borrow_mut().extend_from_slice(&buf[..n as usize]);
            return;
        }
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
            return;
        }
        runtime::unwatch(raw);
        let _keep = &rd;
        if let Some(cb) = cb.borrow_mut().take() {
            cb(String::from_utf8_lossy(&data.borrow()).into_owned());
        }
    });
}

// ---------------------------------------------------------------- protocol dispatch

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Wl {
    fn event(_: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &GlobalListContents, _: &Connection, qh: &QueueHandle<Self>) {
        match ev {
            wl_registry::Event::Global { name, interface, version } if interface == "wl_output" => {
                let _: wl_output::WlOutput = reg.bind(name, version.min(4), qh, name);
            }
            wl_registry::Event::GlobalRemove { name } => {
                let removed = ctx(|c| {
                    let before = c.outputs.len();
                    c.outputs.retain(|o| o.wl.data::<u32>() != Some(&name));
                    before != c.outputs.len()
                });
                if removed {
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
            if !c.outputs.iter().any(|x| x.wl == *o) {
                c.outputs.push(Output { wl: o.clone(), name: String::new(), scale: 1, size: (0, 0), done: false });
            }
            let out = c.outputs.iter_mut().find(|x| x.wl == *o).unwrap();
            match ev {
                wl_output::Event::Name { name } => out.name = name,
                wl_output::Event::Scale { factor } => out.scale = factor,
                wl_output::Event::Mode { width, height, flags: WEnum::Value(f), .. } if f.contains(wl_output::Mode::Current) => {
                    out.size = (width, height)
                }
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
        let at = |id: &ObjectId, x: f64, y: f64| {
            let (ox, oy) = surface(id).map(|s| s.content_origin.get()).unwrap_or((0.0, 0.0));
            let _ = (ox, oy);
            (x, y)
        };
        match ev {
            wl_pointer::Event::Enter { serial, surface: s, surface_x, surface_y } => {
                let hidden = surface(&s.id()).map(|i| matches!(*i.role.borrow(), Role::Lock(_))).unwrap_or(false);
                ctx(|c| {
                    if hidden {
                        // lockscreen hides the cursor
                        if let Some(p) = &c.pointer {
                            p.set_cursor(serial, None, 0, 0);
                        }
                    } else if let Some(d) = &c.cursor_device {
                        d.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default);
                    }
                });
                HOVER.with(|h| *h.borrow_mut() = Some((s.id(), surface_x, surface_y)));
                let (x, y) = at(&s.id(), surface_x, surface_y);
                push(Ev::Pointer(s.id(), PointerEvent::Motion { x, y }));
            }
            wl_pointer::Event::Leave { surface: s, .. } => {
                HOVER.with(|h| *h.borrow_mut() = None);
                push(Ev::Pointer(s.id(), PointerEvent::Leave));
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                if let Some(id) = HOVER.with(|h| {
                    let mut h = h.borrow_mut();
                    h.as_mut().map(|(id, x, y)| {
                        *x = surface_x;
                        *y = surface_y;
                        id.clone()
                    })
                }) {
                    push(Ev::Pointer(id, PointerEvent::Motion { x: surface_x, y: surface_y }));
                }
            }
            wl_pointer::Event::Button { button: 0x110, state, .. } => {
                if let Some((id, x, y)) = HOVER.with(|h| h.borrow().clone()) {
                    let pressed = state == WEnum::Value(wl_pointer::ButtonState::Pressed);
                    push(Ev::Pointer(id, PointerEvent::Button { x, y, pressed }));
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

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Wl {
    fn event(_: &mut Self, l: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ev: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let id = SURFACES.with(|m| {
            m.borrow().iter().find_map(|(id, s)| s.upgrade().filter(|s| matches!(&*s.role.borrow(), Role::Layer(x) if x == l)).map(|_| id.clone()))
        });
        let Some(id) = id else { return };
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                l.ack_configure(serial);
                push(Ev::Configure(id, width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => push(Ev::Closed(id)),
            _ => {}
        }
    }
}

impl Dispatch<ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, ()> for Wl {
    fn event(
        _: &mut Self, l: &ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, ev: ext_session_lock_surface_v1::Event, _: &(), _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_session_lock_surface_v1::Event::Configure { serial, width, height } = ev {
            l.ack_configure(serial);
            let id = SURFACES.with(|m| {
                m.borrow().iter().find_map(|(id, s)| s.upgrade().filter(|s| matches!(&*s.role.borrow(), Role::Lock(x) if x == l)).map(|_| id.clone()))
            });
            if let Some(id) = id {
                push(Ev::Configure(id, width, height));
            }
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

impl Dispatch<wl_surface::WlSurface, ()> for Wl {
    fn event(_: &mut Self, s: &wl_surface::WlSurface, ev: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_surface::Event::PreferredBufferScale { factor } = ev {
            push(Ev::PreferredScale(s.id(), factor));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ObjectId> for Wl {
    fn event(_: &mut Self, _: &wl_callback::WlCallback, ev: wl_callback::Event, id: &ObjectId, _: &Connection, _: &QueueHandle<Self>) {
        if let (wl_callback::Event::Done { .. }, Some(s)) = (ev, surface(id)) {
            s.frame_pending.set(false);
            if s.redraw_queued.replace(false) {
                runtime::idle(move || s.render());
            }
        }
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

impl Dispatch<wl_data_device::WlDataDevice, ()> for Wl {
    fn event(_: &mut Self, _: &wl_data_device::WlDataDevice, ev: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wl_data_device::Event::DataOffer { id } => ctx(|c| {
                c.offers.insert(id.id(), vec![]);
            }),
            wl_data_device::Event::Selection { id } => ctx(|c| {
                if let Some((old, _)) = c.selection.take() {
                    old.destroy();
                }
                c.selection = id.map(|o| {
                    let mimes = c.offers.remove(&o.id()).unwrap_or_default();
                    (o, mimes)
                });
            }),
            _ => {}
        }
    }

    wayland_client::event_created_child!(Wl, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for Wl {
    fn event(_: &mut Self, o: &wl_data_offer::WlDataOffer, ev: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_offer::Event::Offer { mime_type } = ev {
            ctx(|c| {
                if let Some(m) = c.offers.get_mut(&o.id()) {
                    m.push(mime_type);
                } else if let Some((sel, m)) = &mut c.selection
                    && sel == o
                {
                    m.push(mime_type);
                }
            });
        }
    }
}

delegate_noop!(Wl: ignore wl_compositor::WlCompositor);
delegate_noop!(Wl: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(Wl: ignore wl_subsurface::WlSubsurface);
delegate_noop!(Wl: ignore wl_shm::WlShm);
delegate_noop!(Wl: ignore wl_shm_pool::WlShmPool);
delegate_noop!(Wl: ignore wl_region::WlRegion);
delegate_noop!(Wl: ignore wl_data_device_manager::WlDataDeviceManager);
delegate_noop!(Wl: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(Wl: ignore ext_session_lock_manager_v1::ExtSessionLockManagerV1);
delegate_noop!(Wl: ignore wp_viewporter::WpViewporter);
delegate_noop!(Wl: ignore wp_viewport::WpViewport);
delegate_noop!(Wl: ignore wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1);
delegate_noop!(Wl: ignore wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(Wl: ignore wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);
