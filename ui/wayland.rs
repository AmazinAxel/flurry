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

use tiny_skia::{Color, PixmapMut, Transform};
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
    _data_device: Option<wl_data_device::WlDataDevice>,
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
    Motion { x: f64 },
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
        _data_device: None,
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
        CTX.with(|c| *c.borrow_mut() = Some(Ctx { _data_device: Some(dd), ..c0 }));
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
        && let Err(e) = g.read()
        && !matches!(&e, wayland_client::backend::WaylandError::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    {
        // compositor gone or a protocol error (the connection is dead either way; staying would spin
        // on the hung-up fd): exit so the session (systemd) can restart us
        eprintln!("flurry: lost the Wayland connection: {e}");
        // _exit: skip thread-local destructors (surfaces would try to talk to the dead connection)
        unsafe { libc::_exit(1) };
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
                // sway re-sends unchanged configures to every layer when one maps: nothing to redraw
                let shown = s.configured.replace(true) && !s.buffers.borrow().is_empty();
                let unchanged = w == 0 || h == 0 || s.size.get() == (w, h);
                if w > 0 && h > 0 {
                    s.size.set((w, h));
                }
                if shown && unchanged {
                    return;
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
    /// -1 ignores other zones (IGNORE), 0 respects them (NORMAL), >0 reserves (EXCLUSIVE, set from size)
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
    /// number of the frame it holds (0: undefined contents)
    frame: Cell<u64>,
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
    /// mapped again after `unmap`: the next frame must attach a buffer even if nothing changed
    remapped: Cell<bool>,
    /// unmaps whose sync hasn't come back: configures before then are for the old mapping
    stale: Cell<u32>,
    build: RefCell<Option<BuildFn>>,
    on_key: RefCell<Option<Rc<dyn Fn(&KeyEvent)>>>,
    on_pointer: RefCell<Option<Rc<dyn Fn(PointerEvent)>>>,
    after_render: RefCell<Option<Rc<dyn Fn(&Node)>>>,
    /// run once the next frame is committed and flushed
    committed: RefCell<Vec<Box<dyn FnOnce()>>>,
    /// run right after the next frame's commit, so what they commit goes out in the same flush
    on_commit: RefCell<Vec<Box<dyn FnOnce()>>>,
    /// configured but not drawing until released (a backdrop waiting for what goes over it)
    held: Cell<bool>,
    /// called each frame; the surface re-renders while it returns true
    animation: RefCell<Option<Rc<dyn Fn() -> bool>>>,
    image: RefCell<Option<Rc<ShmImage>>>,
    img_viewport: RefCell<Option<wp_viewport::WpViewport>>,
    dim_buffer: RefCell<Option<wl_buffer::WlBuffer>>,
    /// what the main surface shows under the content: (image ptr or 0 for dim, w, h)
    backdrop_key: Cell<Option<(usize, u32, u32)>>,
    gradient: Option<Gradient>,
    damage: RefCell<Damage>,
}

/// Buffer pixel rect: x, y, w, h.
type PxRect = (u32, u32, u32, u32);

/// What the last frames painted and where they differed, so a frame repaints only what changed.
#[derive(Default)]
struct Damage {
    items: Vec<(tiny_skia::Rect, u64)>,
    size: (u32, u32),
    frame: u64,
    /// recent frames' damage (None: everything), newest last
    history: std::collections::VecDeque<(u64, Option<Vec<PxRect>>)>,
}

impl Damage {
    /// Where `items` paints differently from the last frame (None: everything).
    fn changed(&self, items: &[(tiny_skia::Rect, u64)], size: (u32, u32), scale: i32) -> Option<Vec<PxRect>> {
        if self.frame == 0 || self.size != size {
            return None;
        }
        let key = |(r, h): &(tiny_skia::Rect, u64)| (r.left().to_bits(), r.top().to_bits(), r.right().to_bits(), r.bottom().to_bits(), *h);
        let old: std::collections::HashSet<_> = self.items.iter().map(key).collect();
        let new: std::collections::HashSet<_> = items.iter().map(key).collect();
        // antialiasing and glyph overhang reach slightly past the boxes
        let pad = 2.0 * scale as f32;
        let mut rects: Vec<PxRect> = vec![];
        let moved = items.iter().filter(|i| !old.contains(&key(i))).chain(self.items.iter().filter(|i| !new.contains(&key(i))));
        for (r, _) in moved {
            let x0 = (r.left() - pad).floor().max(0.0) as u32;
            let y0 = (r.top() - pad).floor().max(0.0) as u32;
            let x1 = ((r.right() + pad).ceil().max(0.0) as u32).min(size.0);
            let y1 = ((r.bottom() + pad).ceil().max(0.0) as u32).min(size.1);
            if x1 > x0 && y1 > y0 {
                rects.push((x0, y0, x1 - x0, y1 - y0));
            }
        }
        Some(merge(rects, size))
    }

    /// Damage of every frame after `held` (None: unknown, repaint everything).
    fn since(&self, held: u64) -> Option<Vec<PxRect>> {
        if held == 0 {
            return None;
        }
        let mut out = vec![];
        let mut next = held + 1;
        for (n, d) in &self.history {
            if *n < next {
                continue;
            }
            if *n != next {
                return None;
            }
            out.extend(d.as_ref()?);
            next += 1;
        }
        (next == self.frame + 1).then_some(out)
    }

    fn commit(&mut self, number: u64, items: Vec<(tiny_skia::Rect, u64)>, size: (u32, u32), changed: Option<Vec<PxRect>>) {
        self.frame = number;
        self.items = items;
        self.size = size;
        self.history.push_back((number, changed));
        while self.history.len() > 4 {
            self.history.pop_front();
        }
    }
}

/// Overlapping rects merged; past a handful, one bounding rect (each is a full tree walk).
fn merge(mut rects: Vec<PxRect>, size: (u32, u32)) -> Vec<PxRect> {
    let union = |a: PxRect, b: PxRect| {
        let (x0, y0) = (a.0.min(b.0), a.1.min(b.1));
        let (x1, y1) = ((a.0 + a.2).max(b.0 + b.2), (a.1 + a.3).max(b.1 + b.3));
        (x0, y0, x1 - x0, y1 - y0)
    };
    let overlap = |a: PxRect, b: PxRect| a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3;
    let mut i = 0;
    while i < rects.len() {
        let mut j = i + 1;
        let mut grew = false;
        while j < rects.len() {
            if overlap(rects[i], rects[j]) {
                rects[i] = union(rects[i], rects.swap_remove(j));
                grew = true;
            } else {
                j += 1;
            }
        }
        if !grew {
            i += 1;
        }
    }
    if rects.len() > 4 {
        let all = rects.iter().copied().reduce(union).unwrap_or((0, 0, size.0, size.1));
        return vec![all];
    }
    rects
}

/// A full-size vertical gradient as a 1px-wide buffer stretched by the viewport (lockscreen cover).
struct Gradient {
    wl: wl_surface::WlSurface,
    sub: wl_subsurface::WlSubsurface,
    viewport: wp_viewport::WpViewport,
    colors: (Color, Color),
    buffer: RefCell<Option<Buffer>>,
}

impl Gradient {
    fn update(&self, w: u32, h: u32, scale: i32) {
        let bh = h * scale as u32;
        if self.buffer.borrow().as_ref().map(|b| b.size) != Some((1, bh)) {
            let Some(b) = new_buffer(1, bh) else { return };
            let dst = unsafe { std::slice::from_raw_parts_mut(b.ptr, b.len) };
            let Some(mut pm) = PixmapMut::from_bytes(dst, 1, bh) else { return };
            let stops = vec![tiny_skia::GradientStop::new(0.0, self.colors.0), tiny_skia::GradientStop::new(1.0, self.colors.1)];
            if let Some(shader) = tiny_skia::LinearGradient::new((0.0, 0.0).into(), (0.0, bh as f32).into(), stops, tiny_skia::SpreadMode::Pad, Transform::identity())
                && let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, 1.0, bh as f32)
            {
                pm.fill_rect(r, &tiny_skia::Paint { shader, anti_alias: false, ..Default::default() }, Transform::identity(), None);
            }
            self.wl.attach(Some(&b.wl), 0, 0);
            self.wl.damage_buffer(0, 0, 1, bh as i32);
            *self.buffer.borrow_mut() = Some(b);
        }
        self.viewport.set_destination(w as i32, h as i32);
        self.wl.commit();
    }

    fn destroy(&self) {
        self.viewport.destroy();
        self.sub.destroy();
        self.wl.destroy();
        self.buffer.borrow_mut().take();
    }
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
                remapped: Cell::new(false),
                stale: Cell::new(0),
                build: RefCell::new(Some(build)),
                on_key: RefCell::default(),
                on_pointer: RefCell::default(),
                after_render: RefCell::default(),
                committed: RefCell::default(),
                on_commit: RefCell::default(),
                held: Cell::new(false),
                animation: RefCell::default(),
                image: RefCell::default(),
                img_viewport: RefCell::default(),
                dim_buffer: RefCell::default(),
                backdrop_key: Cell::new(None),
                gradient: None,
                damage: RefCell::default(),
            }
        });
        let s = Rc::new(s);
        SURFACES.with(|m| m.borrow_mut().insert(s.wl.id(), Rc::downgrade(&s)));
        // initial commit: size from content first, so the configure already has the right size
        s.apply_content_size(None);
        s.wl.commit();
        Surface(s)
    }

    pub fn on_key(&self, f: impl Fn(&KeyEvent) + 'static) {
        *self.0.on_key.borrow_mut() = Some(Rc::new(f));
    }
    pub fn on_pointer(&self, f: impl Fn(PointerEvent) + 'static) {
        *self.0.on_pointer.borrow_mut() = Some(Rc::new(f));
    }
    /// Re-renders on every frame callback while `f` returns true (animations at display rate).
    pub fn animate(&self, f: Rc<dyn Fn() -> bool>) {
        *self.0.animation.borrow_mut() = Some(f);
        self.redraw();
    }
    /// Called with the laid-out tree after every render (focus/scroll bookkeeping).
    pub fn after_render(&self, f: impl Fn(&Node) + 'static) {
        *self.0.after_render.borrow_mut() = Some(Rc::new(f));
    }
    /// Hides the surface but keeps it, its newest buffer and that frame's damage state, so showing
    /// it again repaints only what changed meanwhile (usually nothing).
    pub fn unmap(&self) {
        let s = &self.0;
        if !matches!(*s.role.borrow(), Role::Layer(_)) {
            return;
        }
        s.wl.attach(None, 0, 0);
        s.wl.commit();
        s.stale.set(s.stale.get() + 1);
        ctx(|c| c.conn.display().sync(&c.qh, Unmapped(s.wl.id())));
        s.configured.set(false);
        s.held.set(false);
        s.frame_pending.set(false);
        s.redraw_queued.set(false);
        s.animation.borrow_mut().take();
        s.backdrop_key.set(None);
        let newest = s.damage.borrow().frame;
        s.buffers.borrow_mut().retain(|b| b.frame.get() == newest || b.busy.load(Relaxed));
    }

    /// Maps an `unmap`ped surface again: initial commit, then the configure renders.
    pub fn remap(&self) {
        let s = &self.0;
        s.remapped.set(true);
        s.apply_content_size(None);
        s.wl.commit();
    }

    pub fn alive(&self) -> bool {
        self.0.alive()
    }

    /// Runs `f` once this surface's next frame has been committed and sent.
    pub fn after_commit(&self, f: impl FnOnce() + 'static) {
        self.0.committed.borrow_mut().push(Box::new(f));
    }
    /// Runs `f` right after this surface's next frame is committed, ahead of the flush.
    pub fn on_commit(&self, f: impl FnOnce() + 'static) {
        self.0.on_commit.borrow_mut().push(Box::new(f));
    }
    /// Keeps the surface from drawing until `release`.
    pub fn hold(&self) {
        self.0.held.set(true);
    }
    pub fn release(&self) {
        if self.0.held.replace(false) {
            self.0.render();
        }
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
    /// `redraw` that doesn't wait out a frame in flight: what's on screen is already wrong.
    pub fn redraw_now(&self) {
        self.0.frame_pending.set(false);
        self.redraw();
    }
    pub fn set_keymode(&self, k: Keymode) {
        if let Role::Layer(l) = &*self.0.role.borrow() {
            l.set_keyboard_interactivity(k);
            // unmapped: a commit here would be an initial commit and map it; the remap's commit applies it
            if self.0.configured.get() {
                self.0.wl.commit();
            }
        }
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
        // never strand work waiting on a frame this surface won't commit now
        for f in self.committed.take() {
            runtime::idle(f);
        }
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
        if let Some(g) = &self.gradient {
            g.destroy();
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
    fn apply_content_size(&self, built: Option<&Node>) {
        let Role::Layer(layer) = &*self.role.borrow() else { return };
        let a = self.spec_anchor;
        let stretch_w = a.contains(Anchor::Left) && a.contains(Anchor::Right);
        let stretch_h = a.contains(Anchor::Top) && a.contains(Anchor::Bottom);
        if (stretch_w || self.fixed.0 > 0) && (stretch_h || self.fixed.1 > 0) {
            return;
        }
        let (cw, ch) = self.size.get();
        let owned;
        let node = match built {
            Some(n) => n,
            None => {
                let Some(build) = self.build.borrow().clone() else { return };
                owned = build(cw as f32, ch as f32);
                &owned
            }
        };
        let (_, nw) = node.measure_w();
        let nw = nw + node.style.margin.h();
        let w = if stretch_w { 0 } else if self.fixed.0 > 0 { self.fixed.0 } else { nw.ceil().max(1.0) as u32 };
        let width_for_h = if stretch_w { cw as f32 } else { w as f32 };
        let (_, nh) = node.measure_h(width_for_h - node.style.margin.h());
        let nh = nh + node.style.margin.v();
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
        if !self.alive() || !self.configured.get() || self.held.get() {
            return;
        }
        if self.frame_pending.get() {
            // a frame is in flight; the frame callback re-renders
            self.redraw_queued.set(true);
            return;
        }
        if self.content.is_none()
            && let Some(img) = self.image.borrow().clone()
        {
            self.attach_image(&img);
            return;
        }
        let Some(build) = self.build.borrow().clone() else { return };
        let before = self.size.get();
        // one build per render: the same tree sizes the layer and gets painted
        let mut node = build(before.0 as f32, before.1 as f32);
        self.apply_content_size(Some(&node));
        if self.size.get() != before && matches!(*self.role.borrow(), Role::Layer(_)) {
            // wait for the configure with the new size (which then always renders)
            self.configured.set(false);
            self.wl.commit();
            return;
        }
        let (w, h) = self.size.get();
        let scale = self.scale.get().max(1);
        node.allocate(0.0, 0.0, w as f32, h as f32);

        let (target, ox, oy, bw, bh) = if let Some((sub_wl, sub)) = &self.content {
            // backdrop on the main surface, content in a subsurface clipped to what it paints
            self.attach_backdrop(w, h);
            if let Some(g) = &self.gradient {
                g.update(w, h, scale);
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

        let (pw, ph) = (bw * scale as u32, bh * scale as u32);
        let ts = Transform::from_scale(scale as f32, scale as f32).pre_translate(-ox, -oy);
        let mut items = vec![];
        node.paint_items(ts, 0, None, &mut items);
        let changed = self.damage.borrow().changed(&items, (pw, ph), scale);
        if let Some(cb) = self.after_render.borrow().clone() {
            cb(&node);
        }
        let remapped = self.remapped.replace(false);
        let changed = match changed {
            // shown again unchanged: the kept frame goes straight back up, nothing repainted
            Some(r) if r.is_empty() && remapped => Some(r),
            Some(r) if r.is_empty() && self.animation.borrow().is_none() => return, // nothing to show
            // an animation step that moved nothing: a 1px commit keeps its frame callbacks coming
            Some(r) if r.is_empty() => Some(vec![(0, 0, 1, 1)]),
            c => c,
        };
        let mut damage = self.damage.borrow_mut();
        let number = damage.frame + 1;
        let Some(buf) = self.take_buffer(pw, ph, number) else { return };
        // ABGR8888 is tiny-skia's premultiplied RGBA byte order: paint straight into the shm buffer
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.0, buf.1) };
        // the buffer holds frame buf.3: bring it up to date with everything damaged since
        let repaint = changed.as_ref().and_then(|c| damage.since(buf.3).map(|mut old| {
            old.extend(c);
            old
        }));
        match &repaint {
            None => {
                let Some(mut pm) = PixmapMut::from_bytes(dst, pw, ph) else { return };
                pm.fill(Color::TRANSPARENT);
                node.paint(&mut pm, ts);
            }
            Some(rects) => {
                for &(x, y, w, h) in rects {
                    let Some(mut layer) = tiny_skia::Pixmap::new(w, h) else { continue };
                    node.paint(&mut layer.as_mut(), ts.post_translate(-(x as f32), -(y as f32)));
                    let row = (w * 4) as usize;
                    for (i, src) in layer.data().chunks_exact(row).enumerate() {
                        let at = ((y as usize + i) * pw as usize + x as usize) * 4;
                        dst[at..at + row].copy_from_slice(src);
                    }
                }
            }
        }
        damage.commit(number, items, (pw, ph), changed.clone());
        drop(damage);
        target.set_buffer_scale(scale);
        target.attach(Some(&buf.2), 0, 0);
        match &changed {
            Some(rects) if !remapped => rects.iter().for_each(|&(x, y, w, h)| target.damage_buffer(x as i32, y as i32, w as i32, h as i32)),
            _ => target.damage_buffer(0, 0, pw as i32, ph as i32),
        }
        self.request_frame(&target);
        target.commit();
        if target != self.wl {
            self.wl.commit();
        }
        for f in self.on_commit.take() {
            f();
        }
        for f in self.committed.take() {
            runtime::after_flush(f);
        }
    }

    /// Main surface under the content subsurface: the image cover-cropped if set, else the dim color,
    /// both stretched by the viewport (no full-size buffer of our own).
    fn attach_backdrop(&self, w: u32, h: u32) {
        let img = self.image.borrow().clone();
        let key = (img.as_ref().map_or(0, |i| Rc::as_ptr(i) as usize), w, h);
        if self.backdrop_key.replace(Some(key)) == Some(key) {
            return;
        }
        let Some(v) = &self.viewport else { return };
        v.set_destination(w as i32, h as i32);
        match img {
            Some(img) => {
                let (sx, sy, sw, sh) = super::image::cover(img.width as f32, img.height as f32, w as f32, h as f32);
                v.set_source(sx as f64, sy as f64, sw as f64, sh as f64);
                self.wl.attach(Some(&img.buffer), 0, 0);
            }
            None => {
                let [r, g, b, a] = self.dim.unwrap_or([0.0, 0.0, 0.0, 1.0]);
                let conv = |v: f32| (v * a * u32::MAX as f32) as u32;
                // created once and kept: a buffer destroyed before the commit would unmap the dim
                let mut dim = self.dim_buffer.borrow_mut();
                if dim.is_none() {
                    *dim = Some(ctx(|c| c.single_pixel.create_u32_rgba_buffer(conv(r), conv(g), conv(b), (a * u32::MAX as f32) as u32, &c.qh, ())));
                }
                v.set_source(-1.0, -1.0, -1.0, -1.0);
                self.wl.attach(dim.as_ref(), 0, 0);
            }
        }
        self.wl.damage(0, 0, i32::MAX, i32::MAX);
    }

    fn request_frame(self: &Rc<Self>, target: &wl_surface::WlSurface) {
        self.frame_pending.set(true);
        ctx(|c| target.frame(&c.qh, self.wl.id()));
    }

    /// A free buffer for frame `number`, and the frame it held (0: none). The newest is reused first.
    fn take_buffer(&self, w: u32, h: u32, number: u64) -> Option<(*mut u8, usize, wl_buffer::WlBuffer, u64)> {
        let mut bufs = self.buffers.borrow_mut();
        bufs.retain(|b| b.size == (w, h) || b.busy.load(Relaxed));
        if let Some(b) = bufs.iter().filter(|b| !b.busy.load(Relaxed) && b.size == (w, h)).max_by_key(|b| b.frame.get()) {
            b.busy.store(true, Relaxed);
            return Some((b.ptr, b.len, b.wl.clone(), b.frame.replace(number)));
        }
        let b = new_buffer(w, h)?;
        b.busy.store(true, Relaxed);
        b.frame.set(number);
        let out = (b.ptr, b.len, b.wl.clone(), 0);
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

// ABGR8888 everywhere: the same byte order as tiny-skia, so pixmaps live in the buffers
fn new_buffer(w: u32, h: u32) -> Option<Buffer> {
    let len = (w * h * 4) as usize;
    let fd = memfd(len)?;
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
    if ptr == libc::MAP_FAILED {
        return None;
    }
    let busy = Arc::new(AtomicBool::new(false));
    let wl = ctx(|c| {
        let pool = c.shm.create_pool(fd.as_fd(), len as i32, &c.qh, ());
        let b = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Abgr8888, &c.qh, busy.clone());
        pool.destroy();
        b
    });
    Some(Buffer { wl, ptr: ptr.cast(), len, size: (w, h), busy, frame: Cell::new(0) })
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
        let len = (w as usize) * (h as usize) * 4;
        let fd = memfd(len)?;
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        Some(ShmPixels { fd: Some(fd), ptr: ptr as usize, len, width: w, height: h })
    }

    pub fn data(&mut self) -> &mut [u8] {
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
        let buffer = ctx(|c| {
            let pool = c.shm.create_pool(fd.as_fd(), p.len as i32, &c.qh, ());
            let b = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Abgr8888, &c.qh, Arc::new(AtomicBool::new(false)));
            pool.destroy();
            b
        });
        Rc::new(ShmImage { buffer, width: w, height: h, ptr: p.ptr as *mut u8, len: p.len })
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
/// A lock surface showing `image` (black without one) under a vertical `gradient`, with the built
/// content in a subsurface clipped to what it paints: no full-screen repaint per keypress.
pub fn lock_surface(output: &wl_output::WlOutput, build: BuildFn, image: Option<Rc<ShmImage>>, gradient: (Color, Color)) -> Option<Surface> {
    let inner = ctx(|c| {
        let lock = c.lock.as_ref()?;
        let wl = c.compositor.create_surface(&c.qh, ());
        let ls = lock.get_lock_surface(&wl, output, &c.qh, ());
        let scale = c.outputs.iter().find(|o| o.wl == *output).map(|o| o.scale).unwrap_or(1);
        // subsurfaces stack in creation order: gradient, then content on top
        let sub = |c: &Ctx| {
            let sub_wl = c.compositor.create_surface(&c.qh, ());
            let sub = c.subcompositor.get_subsurface(&sub_wl, &wl, &c.qh, ());
            sub.set_desync();
            let region = c.compositor.create_region(&c.qh, ());
            sub_wl.set_input_region(Some(&region));
            region.destroy();
            (sub_wl, sub)
        };
        let (gwl, gsub) = sub(c);
        let gradient = Gradient { viewport: c.viewporter.get_viewport(&gwl, &c.qh, ()), wl: gwl, sub: gsub, colors: gradient, buffer: RefCell::default() };
        let content = sub(c);
        Some(Inner {
            viewport: Some(c.viewporter.get_viewport(&wl, &c.qh, ())),
            wl,
            role: RefCell::new(Role::Lock(ls)),
            spec_anchor: Anchor::all(),
            content: Some(content),
            dim: None,
            size: Cell::new((1, 1)),
            fixed: (0, 0),
            exclusive: 0,
            scale: Cell::new(scale),
            configured: Cell::new(false),
            buffers: RefCell::default(),
            frame_pending: Cell::new(false),
            redraw_queued: Cell::new(false),
            remapped: Cell::new(false),
            stale: Cell::new(0),
            build: RefCell::new(Some(build)),
            on_key: RefCell::default(),
            on_pointer: RefCell::default(),
            after_render: RefCell::default(),
            committed: RefCell::default(),
            on_commit: RefCell::default(),
            held: Cell::new(false),
            animation: RefCell::default(),
            image: RefCell::new(image),
            img_viewport: RefCell::default(),
            dim_buffer: RefCell::default(),
            backdrop_key: Cell::new(None),
            gradient: Some(gradient),
            damage: RefCell::default(),
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
                push(Ev::Pointer(s.id(), PointerEvent::Motion { x: surface_x }));
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
                    push(Ev::Pointer(id, PointerEvent::Motion { x: surface_x }));
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
        let found = SURFACES.with(|m| {
            m.borrow().iter().find_map(|(id, s)| s.upgrade().filter(|s| matches!(&*s.role.borrow(), Role::Layer(x) if x == l)).map(|s| (id.clone(), s)))
        });
        let Some((id, s)) = found else { return };
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                if s.stale.get() > 0 {
                    return; // sent before an unmap: its serial is void now, acking it is a protocol error
                }
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

/// The surface an unmap's sync belongs to.
pub struct Unmapped(ObjectId);

impl Dispatch<wl_callback::WlCallback, Unmapped> for Wl {
    fn event(_: &mut Self, _: &wl_callback::WlCallback, ev: wl_callback::Event, u: &Unmapped, _: &Connection, _: &QueueHandle<Self>) {
        if let (wl_callback::Event::Done { .. }, Some(s)) = (ev, surface(&u.0)) {
            s.stale.set(s.stale.get().saturating_sub(1));
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ObjectId> for Wl {
    fn event(_: &mut Self, _: &wl_callback::WlCallback, ev: wl_callback::Event, id: &ObjectId, _: &Connection, _: &QueueHandle<Self>) {
        if let (wl_callback::Event::Done { .. }, Some(s)) = (ev, surface(id)) {
            s.frame_pending.set(false);
            let queued = s.redraw_queued.replace(false);
            let anim = s.animation.borrow().clone();
            if queued || anim.is_some() {
                runtime::idle(move || {
                    // the step may hide the window, so it runs before the render and liveness is rechecked
                    let more = anim.is_some_and(|a| a());
                    if !more {
                        s.animation.borrow_mut().take();
                    }
                    if queued || more {
                        s.render();
                    }
                });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::node::{Background, Node, Sides, Style, shadow};

    fn tree(sel: usize, center: Option<usize>, text: &str) -> Node {
        let rows = (0..12)
            .map(|i| {
                let mut s = Style { padding: Sides::all(8.0), margin: Sides::trbl(0.0, 0.0, 8.0, 0.0), ..Default::default() };
                s.background = Background::Color(Color::from_rgba8(40, 40, 60, 255));
                s.box_shadow = vec![shadow(0.0, 2.0, 6.0, Color::from_rgba8(0, 0, 0, 120))];
                if i == sel {
                    s.translate_y = -2.0;
                    s.outline = Some((2.0, 1.0, Color::from_rgba8(100, 150, 255, 255)));
                }
                Node::vbox(s, 0.0, vec![Node::label(Style::default(), format!("row {i} {text}"))])
            })
            .collect();
        let list = Node::vbox(Style::default(), 0.0, rows);
        let sc = Node::scroll(Style::default(), 300.0, center, list);
        Node::vbox(Style { padding: Sides::all(16.0), background: Background::Color(Color::from_rgba8(20, 20, 30, 230)), ..Default::default() }, 0.0, vec![
            Node::label(Style::default(), "header"),
            sc,
        ])
    }

    fn full(n: &mut Node, ts: Transform, w: u32, h: u32) -> tiny_skia::Pixmap {
        n.allocate(0.0, 0.0, w as f32 / 2.0, h as f32 / 2.0);
        let mut p = tiny_skia::Pixmap::new(w, h).unwrap();
        n.paint(&mut p.as_mut(), ts);
        p
    }

    #[test]
    fn partial_repaint_matches_full() {
        let (w, h) = (500, 800);
        let ts = Transform::from_scale(2.0, 2.0);
        let frames = [(0, None, "a"), (1, None, "a"), (1, None, "b"), (5, Some(5), "b"), (5, Some(5), "b"), (2, Some(5), "c")];
        let mut damage = Damage::default();
        let mut buf: Option<tiny_skia::Pixmap> = None;
        for (n, &(sel, center, text)) in frames.iter().enumerate() {
            let mut node = tree(sel, center, text);
            let want = full(&mut node, ts, w, h);
            let mut items = vec![];
            node.paint_items(ts, 0, None, &mut items);
            let changed = damage.changed(&items, (w, h), 2);
            let mut got = buf.take().unwrap_or_else(|| tiny_skia::Pixmap::new(w, h).unwrap());
            match &changed {
                None => got = want.clone(),
                Some(rects) => {
                    for &(x, y, rw, rh) in rects {
                        let mut layer = tiny_skia::Pixmap::new(rw, rh).unwrap();
                        node.paint(&mut layer.as_mut(), ts.post_translate(-(x as f32), -(y as f32)));
                        let row = (rw * 4) as usize;
                        for (i, src) in layer.data().chunks_exact(row).enumerate() {
                            let at = ((y as usize + i) * w as usize + x as usize) * 4;
                            got.data_mut()[at..at + row].copy_from_slice(src);
                        }
                    }
                }
            }
            if n == 4 {
                assert_eq!(changed.as_deref(), Some(&[][..]), "unchanged frame has no damage");
            }
            if let Some(i) = got.data().iter().zip(want.data()).position(|(a, b)| a != b) {
                panic!("frame {n} differs at px {} ({}, {}) got {:?} want {:?} (damage {changed:?})", i / 4, (i / 4) % w as usize, (i / 4) / w as usize, &got.data()[i / 4 * 4..i / 4 * 4 + 4], &want.data()[i / 4 * 4..i / 4 * 4 + 4]);
            }
            damage.commit(n as u64 + 1, items, (w, h), changed);
            buf = Some(got);
        }
    }

    #[test]
    fn scroll_centers_row_within_content() {
        let row_top = |center: usize, row: usize| {
            let rows = (0..10).map(|_| Node::vbox(Style { min_height: 50.0, ..Default::default() }, 0.0, vec![])).collect();
            let mut n = Node::scroll(Style::default(), 200.0, Some(center), Node::vbox(Style::default(), 0.0, rows));
            n.allocate(0.0, 0.0, 100.0, 200.0);
            n.children[0].children[row].rect.top()
        };
        assert_eq!(row_top(0, 0), 0.0, "first row: pinned to the top");
        assert_eq!(row_top(5, 5), 75.0, "middle row: centered");
        assert_eq!(row_top(9, 9), 150.0, "last row: pinned to the bottom, fully visible");
    }
}
