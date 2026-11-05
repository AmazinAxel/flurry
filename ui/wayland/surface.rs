// Layer and lock surfaces: sizing from content, damage-tracked repaints into shm double buffers,
// single-pixel dim and shared-image backdrops.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::Ordering::Relaxed;

use tiny_skia::{Color, PixmapMut, Transform};
use wayland_client::protocol::{wl_buffer, wl_callback, wl_output, wl_subsurface, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, backend::ObjectId};
use wayland_protocols::ext::session_lock::v1::client::ext_session_lock_surface_v1;
use wayland_protocols::wp::viewporter::client::wp_viewport;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1;

use super::shm::{Buffer, ShmImage, new_buffer};
use super::{Anchor, Ctx, Ev, KeyEvent, Keymode, PointerEvent, SURFACES, Wl, ctx, push, runtime, surface};
use crate::ui::node::Node;
use crate::ui::window::Spec;

thread_local! {
    /// pixels of one damage rect, reused by every surface's partial repaints
    static SCRATCH: Cell<Vec<u8>> = const { Cell::new(Vec::new()) };
}

pub(super) enum Role {
    Layer(zwlr_layer_surface_v1::ZwlrLayerSurfaceV1),
    Lock(ext_session_lock_surface_v1::ExtSessionLockSurfaceV1),
    None,
}

pub type BuildFn = Rc<dyn Fn() -> Node>;

/// A subsurface with an empty input region (input goes to the parent), stacked above earlier ones.
pub(super) fn subsurface(c: &Ctx, parent: &wl_surface::WlSurface) -> (wl_surface::WlSurface, wl_subsurface::WlSubsurface) {
    let wl = c.compositor.create_surface(&c.qh, ());
    let sub = c.subcompositor.get_subsurface(&wl, parent, &c.qh, ());
    sub.set_desync();
    no_input(c, &wl);
    (wl, sub)
}

/// A solid rectangle over its surface: a single-pixel buffer the viewport stretches, so moving or
/// resizing it paints nothing (the screenshot selection).
struct Solid {
    wl: wl_surface::WlSurface,
    sub: wl_subsurface::WlSubsurface,
    viewport: wp_viewport::WpViewport,
    buffer: wl_buffer::WlBuffer,
    /// x, y, w, h shown (None: hidden)
    at: Cell<Option<(i32, i32, i32, i32)>>,
}

impl Solid {
    fn new(c: &Ctx, parent: &wl_surface::WlSurface, [r, g, b, a]: [f32; 4]) -> Solid {
        let (wl, sub) = subsurface(c, parent);
        // synced: every rect moves in the parent's commit, all in one frame
        sub.set_sync();
        let premul = |v: f32| (v * a * u32::MAX as f32) as u32;
        let buffer = c.single_pixel.create_u32_rgba_buffer(premul(r), premul(g), premul(b), (a * u32::MAX as f32) as u32, &c.qh, ());
        Solid { viewport: c.viewporter.get_viewport(&wl, &c.qh, ()), wl, sub, buffer, at: Cell::new(None) }
    }

    /// True when the parent needs a commit.
    fn show(&self, r: Option<(i32, i32, i32, i32)>) -> bool {
        let r = r.filter(|r| r.2 > 0 && r.3 > 0);
        let old = self.at.replace(r);
        if old == r {
            return false;
        }
        match r {
            None => self.wl.attach(None, 0, 0),
            Some((x, y, w, h)) => {
                if old.is_none_or(|o| (o.0, o.1) != (x, y)) {
                    self.sub.set_position(x, y);
                }
                if old.is_some_and(|o| (o.2, o.3) == (w, h)) {
                    return true; // moved only: parent state
                }
                self.viewport.set_destination(w, h);
                if old.is_none() {
                    self.wl.attach(Some(&self.buffer), 0, 0);
                    self.wl.damage_buffer(0, 0, 1, 1);
                }
            }
        }
        self.wl.commit();
        true
    }

    fn destroy(&self) {
        self.viewport.destroy();
        self.sub.destroy();
        self.wl.destroy();
        self.buffer.destroy();
    }
}

fn no_input(c: &Ctx, wl: &wl_surface::WlSurface) {
    let region = c.compositor.create_region(&c.qh, ());
    wl.set_input_region(Some(&region));
    region.destroy();
}

/// The content subsurface over a backdrop on the main surface, which the viewport stretches.
pub(super) struct Content {
    pub(super) wl: wl_surface::WlSurface,
    pub(super) sub: wl_subsurface::WlSubsurface,
    pub(super) viewport: wp_viewport::WpViewport,
    /// subsurface position last set
    pos: Cell<(i32, i32)>,
}

impl Content {
    pub(super) fn new(c: &Ctx, parent: &wl_surface::WlSurface) -> Content {
        let (wl, sub) = subsurface(c, parent);
        Content { wl, sub, viewport: c.viewporter.get_viewport(parent, &c.qh, ()), pos: Cell::new((0, 0)) }
    }
}

pub struct Inner {
    pub(super) wl: wl_surface::WlSurface,
    role: RefCell<Role>,
    spec_anchor: Anchor,
    content: Option<Content>,
    dim: Option<[f32; 4]>,
    size: Cell<(u32, u32)>,
    fixed: (u32, u32),
    exclusive: i32,
    pub(super) scale: Cell<i32>,
    configured: Cell<bool>,
    buffers: RefCell<Vec<Buffer>>,
    frame_pending: Cell<bool>,
    redraw_queued: Cell<bool>,
    /// mapped again after `unmap`: the next frame must attach a buffer even if nothing changed
    remapped: Cell<bool>,
    /// unmaps whose sync hasn't come back: configures before then are for the old mapping
    stale: Cell<u32>,
    build: BuildFn,
    pub(super) on_key: RefCell<Option<Rc<dyn Fn(&KeyEvent)>>>,
    pub(super) on_pointer: RefCell<Option<Rc<dyn Fn(PointerEvent)>>>,
    /// the compositor closed it (its output went away)
    pub(super) on_closed: RefCell<Option<Box<dyn FnOnce()>>>,
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
    /// what the main surface shows under the content: (image id or 0 for dim, w, h)
    backdrop_key: Cell<Option<(u64, u32, u32)>>,
    gradient: Option<Gradient>,
    damage: RefCell<Damage>,
    /// the output a lock surface is on: it dies with it
    pub(super) output: Option<wl_output::WlOutput>,
    /// the tree the initial commit was sized from: the configure's render paints it
    prebuilt: RefCell<Option<Node>>,
    /// the layer size last asked for (0: stretched/fixed axis); asked again only when it changes
    requested: Cell<Option<(u32, u32)>>,
    /// gets every motion (crosshair cursor), not only scrolls
    pub(super) track_pointer: Cell<bool>,
    solids: RefCell<Vec<Solid>>,
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
        Some(merge(rects))
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
fn merge(mut rects: Vec<PxRect>) -> Vec<PxRect> {
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
        return vec![rects[1..].iter().fold(rects[0], |a, &b| union(a, b))];
    }
    rects
}

/// A full-size vertical gradient as a 1px-wide buffer stretched by the viewport (lockscreen cover).
pub(super) struct Gradient {
    wl: wl_surface::WlSurface,
    sub: wl_subsurface::WlSubsurface,
    viewport: wp_viewport::WpViewport,
    colors: (Color, Color),
    buffer: RefCell<Option<Buffer>>,
    /// (w, h, scale) last committed: unchanged needs no commit
    shown: Cell<Option<(u32, u32, i32)>>,
}

impl Gradient {
    pub(super) fn new(c: &Ctx, parent: &wl_surface::WlSurface, colors: (Color, Color)) -> Gradient {
        let (wl, sub) = subsurface(c, parent);
        Gradient { viewport: c.viewporter.get_viewport(&wl, &c.qh, ()), wl, sub, colors, buffer: RefCell::default(), shown: Cell::new(None) }
    }

    fn update(&self, w: u32, h: u32, scale: i32) {
        if self.shown.get() == Some((w, h, scale)) {
            return;
        }
        let Some(bh) = h.checked_mul(scale as u32) else { return };
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
        self.shown.set(Some((w, h, scale)));
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
    /// A layer surface for `spec` (`per_monitor` is the window's business) on `output`.
    pub fn layer(spec: &Spec, output: Option<&wl_output::WlOutput>, build: BuildFn) -> Surface {
        let s = ctx(|c| {
            let wl = c.compositor.create_surface(&c.qh, ());
            let layer = c.layer_shell.get_layer_surface(&wl, output, spec.layer, spec.name.into(), &c.qh, wl.id());
            layer.set_anchor(spec.anchor);
            layer.set_keyboard_interactivity(spec.keymode);
            layer.set_exclusive_zone(spec.exclusive.min(0));
            layer.set_size(spec.width, spec.height);
            let content = spec.dim.map(|_| Content::new(c, &wl));
            if !spec.input {
                no_input(c, &wl);
            }
            let scale = output.and_then(|o| c.outputs.iter().find(|x| x.wl == *o)).or(c.outputs.first()).map_or(1, |o| o.scale);
            let mut s = Inner::new(wl, Role::Layer(layer), scale, build);
            s.spec_anchor = spec.anchor;
            s.content = content;
            s.dim = spec.dim;
            s.size = Cell::new((spec.width.max(1), spec.height.max(1)));
            s.fixed = (spec.width, spec.height);
            s.exclusive = spec.exclusive;
            s
        });
        let s = Surface::register(s);
        // initial commit: size from content first, so the configure already has the right size
        s.0.apply_content_size();
        s.0.wl.commit();
        s
    }

    pub(super) fn register(s: Inner) -> Surface {
        let s = Rc::new(s);
        SURFACES.with(|m| m.borrow_mut().insert(s.wl.id(), Rc::downgrade(&s)));
        Surface(s)
    }

    pub fn on_key(&self, f: impl Fn(&KeyEvent) + 'static) {
        *self.0.on_key.borrow_mut() = Some(Rc::new(f));
    }
    pub fn on_pointer(&self, f: impl Fn(PointerEvent) + 'static) {
        *self.0.on_pointer.borrow_mut() = Some(Rc::new(f));
    }
    /// Motion events too, under a crosshair cursor (region selection).
    pub fn track_pointer(&self) {
        self.0.track_pointer.set(true);
    }
    /// Solid rects (color, logical x/y/w/h or None: hidden) over the surface, stacked in order; a
    /// slot keeps the color it was first given. Applied in one commit, nothing repainted.
    pub fn set_rects(&self, rects: &[([f32; 4], Option<(i32, i32, i32, i32)>)]) {
        let s = &self.0;
        if !s.alive() {
            return;
        }
        let mut solids = s.solids.borrow_mut();
        ctx(|c| {
            for (color, _) in &rects[solids.len().min(rects.len())..] {
                solids.push(Solid::new(c, &s.wl, *color));
            }
        });
        let mut changed = false;
        for (sol, (_, r)) in solids.iter().zip(rects) {
            changed |= sol.show(*r);
        }
        // before the first configure the image's commit carries them
        if changed && s.configured.get() {
            s.wl.commit();
        }
    }
    /// Size in surface (logical) pixels, as last configured.
    pub fn size(&self) -> (u32, u32) {
        self.0.size.get()
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
        s.apply_content_size();
        s.wl.commit();
    }

    /// Runs `f` once if the compositor closes the surface (its output went away).
    pub fn on_closed(&self, f: impl FnOnce() + 'static) {
        *self.0.on_closed.borrow_mut() = Some(Box::new(f));
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
        self.0.queue_redraw();
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
    /// An unconfigured surface with `role` and everything else at its default.
    pub(super) fn new(wl: wl_surface::WlSurface, role: Role, scale: i32, build: BuildFn) -> Inner {
        Inner {
            wl,
            role: RefCell::new(role),
            spec_anchor: Anchor::all(),
            content: None,
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
            build,
            on_key: RefCell::default(),
            on_pointer: RefCell::default(),
            on_closed: RefCell::default(),
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
            output: None,
            prebuilt: RefCell::default(),
            requested: Cell::new(None),
            track_pointer: Cell::new(false),
            solids: RefCell::default(),
        }
    }

    /// Lock-surface parts `Inner::new` leaves out.
    pub(super) fn with_lock(mut self, content: Content, gradient: Gradient, image: Option<Rc<ShmImage>>, output: wl_output::WlOutput) -> Inner {
        self.content = Some(content);
        self.gradient = Some(gradient);
        self.image = RefCell::new(image);
        self.output = Some(output);
        self
    }

    pub(super) fn is_lock(&self) -> bool {
        matches!(*self.role.borrow(), Role::Lock(_))
    }

    pub(super) fn configure(self: &Rc<Self>, w: u32, h: u32) {
        // queued in the batch an unmap ran in: for the old mapping, and rendering it would commit a
        // buffer to an unmapped layer surface (a protocol error)
        if self.stale.get() > 0 {
            return;
        }
        // sway re-sends unchanged configures to every layer when one maps: nothing to redraw
        let shown = self.configured.replace(true) && (!self.buffers.borrow().is_empty() || self.shows_image());
        let unchanged = w == 0 || h == 0 || self.size.get() == (w, h);
        if w > 0 && h > 0 {
            self.size.set((w, h));
        }
        if shown && unchanged {
            return;
        }
        self.render();
    }

    pub(super) fn destroy_role(&self) {
        // never strand work waiting on a frame this surface won't commit now
        for f in self.committed.take().into_iter().chain(self.on_commit.take()) {
            runtime::idle(f);
        }
        let role = std::mem::replace(&mut *self.role.borrow_mut(), Role::None);
        match role {
            Role::Layer(l) => l.destroy(),
            Role::Lock(l) => l.destroy(),
            Role::None => return,
        }
        if let Some(c) = &self.content {
            c.viewport.destroy();
            c.sub.destroy();
            c.wl.destroy();
        }
        if let Some(g) = &self.gradient {
            g.destroy();
        }
        for s in self.solids.take() {
            s.destroy();
        }
        if let Some(b) = self.dim_buffer.borrow_mut().take() {
            b.destroy();
        }
        if let Some(v) = self.img_viewport.borrow_mut().take() {
            v.destroy();
        }
        self.buffers.borrow_mut().clear();
        // a handler may keep the inner state alive: the image goes now
        self.image.take();
        self.wl.destroy();
        SURFACES.with(|m| m.borrow_mut().remove(&self.wl.id()));
    }

    fn alive(&self) -> bool {
        !matches!(*self.role.borrow(), Role::None)
    }

    pub(super) fn queue_redraw(self: &Rc<Self>) {
        // state changed: a tree built before is stale
        self.prebuilt.take();
        if self.redraw_queued.replace(true) {
            return;
        }
        let s = self.clone();
        runtime::idle(move || {
            s.redraw_queued.set(false);
            s.render();
        });
    }

    /// An image surface (wallpaper): shown without buffers of its own.
    fn shows_image(&self) -> bool {
        self.content.is_none() && self.image.borrow().is_some()
    }

    /// Asks for the layer size the content's natural size gives its non-stretched axes (GTK layer
    /// windows do the same), from a fresh build kept for the render.
    fn apply_content_size(&self) {
        let node = (self.build)();
        if let Some(want) = self.content_size(&node) {
            self.request_size(want);
        }
        // a re-show's configure paints this tree instead of building another
        *self.prebuilt.borrow_mut() = Some(node);
    }

    /// The layer size `node` wants (0 on stretched axes); None when nothing is from content.
    fn content_size(&self, node: &Node) -> Option<(u32, u32)> {
        if !matches!(*self.role.borrow(), Role::Layer(_)) {
            return None;
        }
        let a = self.spec_anchor;
        let stretch_w = a.contains(Anchor::Left) && a.contains(Anchor::Right);
        let stretch_h = a.contains(Anchor::Top) && a.contains(Anchor::Bottom);
        if (stretch_w || self.fixed.0 > 0) && (stretch_h || self.fixed.1 > 0) {
            return None;
        }
        let (_, nw) = node.measure_w();
        let nw = nw + node.style.margin.h();
        let w = if stretch_w { 0 } else if self.fixed.0 > 0 { self.fixed.0 } else { nw.ceil().max(1.0) as u32 };
        let width_for_h = if stretch_w { self.size.get().0 as f32 } else { w as f32 };
        let (_, nh) = node.measure_h(width_for_h - node.style.margin.h());
        let nh = nh + node.style.margin.v();
        let h = if stretch_h { 0 } else if self.fixed.1 > 0 { self.fixed.1 } else { nh.ceil().max(1.0) as u32 };
        Some((w, h))
    }

    fn request_size(&self, (w, h): (u32, u32)) {
        let Role::Layer(layer) = &*self.role.borrow() else { return };
        layer.set_size(w, h);
        if self.exclusive > 0 {
            let a = self.spec_anchor;
            let zone = if a.contains(Anchor::Left) ^ a.contains(Anchor::Right) { w } else { h };
            layer.set_exclusive_zone(zone as i32);
        }
        let (old_w, old_h) = self.size.get();
        self.size.set((if w == 0 { old_w } else { w }, if h == 0 { old_h } else { h }));
        self.requested.set(Some((w, h)));
    }

    pub(super) fn render(self: &Rc<Self>) {
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
        let before = self.size.get();
        // one build per render: the same tree sizes the layer and gets painted
        let mut node = self.prebuilt.take().unwrap_or_else(|| (self.build)());
        // asked once per change: a compositor that configures another size gets painted at that
        // size rather than asked again forever
        if let Some(want) = self.content_size(&node)
            && self.requested.get() != Some(want)
        {
            self.request_size(want);
            if self.size.get() != before {
                // wait for the configure with the new size (which then always renders)
                self.configured.set(false);
                self.wl.commit();
                return;
            }
        }
        let (w, h) = self.size.get();
        let scale = self.scale.get().max(1);
        node.allocate(0.0, 0.0, w as f32, h as f32);

        // main-surface state (backdrop, subsurface position) that needs a commit even with no new frame
        let mut parent_changed = false;
        let (target, ox, oy, bw, bh) = if let Some(c) = &self.content {
            // backdrop on the main surface, content in a subsurface clipped to what it paints
            parent_changed = self.attach_backdrop(c, w, h);
            if let Some(g) = &self.gradient {
                g.update(w, h, scale);
            }
            let bounds = node.paint_bounds();
            let x0 = bounds.left().floor().max(0.0);
            let y0 = bounds.top().floor().max(0.0);
            let x1 = bounds.right().ceil().min(w as f32);
            let y1 = bounds.bottom().ceil().min(h as f32);
            let pos = (x0 as i32, y0 as i32);
            if c.pos.replace(pos) != pos {
                c.sub.set_position(pos.0, pos.1);
                parent_changed = true;
            }
            (c.wl.clone(), x0, y0, (x1 - x0).max(1.0) as u32, (y1 - y0).max(1.0) as u32)
        } else {
            (self.wl.clone(), 0.0, 0.0, w, h)
        };

        let (Some(pw), Some(ph)) = (bw.checked_mul(scale as u32), bh.checked_mul(scale as u32)) else { return };
        let ts = Transform::from_scale(scale as f32, scale as f32).pre_translate(-ox, -oy);
        let mut items = vec![];
        node.paint_items(ts, 0, None, &mut items);
        let changed = self.damage.borrow().changed(&items, (pw, ph), scale);
        if let Some(cb) = self.after_render.borrow().clone() {
            cb(&node);
        }
        // cleared only once a frame goes up: a failed render must still re-attach next time
        let remapped = self.remapped.get();
        let changed = match changed {
            // shown again unchanged: the kept frame goes straight back up, nothing repainted
            Some(r) if r.is_empty() && remapped => Some(r),
            // nothing to show
            Some(r) if r.is_empty() && self.animation.borrow().is_none() => {
                if parent_changed {
                    self.wl.commit();
                }
                return;
            }
            // an animation step that moved nothing: a 1px commit keeps its frame callbacks coming
            Some(r) if r.is_empty() => Some(vec![(0, 0, 1, 1)]),
            c => c,
        };
        let mut damage = self.damage.borrow_mut();
        let number = damage.frame + 1;
        let Some(buf) = self.take_buffer(pw, ph, number) else { return };
        self.remapped.set(false);
        // ABGR8888 is tiny-skia's premultiplied RGBA byte order: paint straight into the shm buffer
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.0, buf.1) };
        // the buffer holds frame buf.3: bring it up to date with everything damaged since
        let repaint = changed.as_ref().and_then(|c| {
            damage.since(buf.3).map(|mut old| {
                old.extend(c);
                merge(old)
            })
        });
        // repainting most of it piecewise costs more (and a scratch that big) than painting it all
        let area = |r: &Vec<PxRect>| r.iter().map(|&(_, _, w, h)| w as u64 * h as u64).sum::<u64>();
        let repaint = repaint.filter(|r| area(r) * 2 <= pw as u64 * ph as u64);
        match &repaint {
            None => {
                let Some(mut pm) = PixmapMut::from_bytes(dst, pw, ph) else {
                    // never attached: drop it rather than leave it busy with a frame it doesn't hold
                    self.buffers.borrow_mut().retain(|b| b.wl != buf.2);
                    self.remapped.set(remapped);
                    return;
                };
                pm.fill(Color::TRANSPARENT);
                node.paint(&mut pm, ts);
            }
            Some(rects) => {
                let mut scratch = SCRATCH.take();
                for &(x, y, w, h) in rects {
                    let row = (w * 4) as usize;
                    scratch.clear();
                    scratch.resize(row * h as usize, 0);
                    let Some(mut layer) = PixmapMut::from_bytes(&mut scratch, w, h) else { continue };
                    node.paint(&mut layer, ts.post_translate(-(x as f32), -(y as f32)));
                    for (i, src) in scratch.chunks_exact(row).enumerate() {
                        let at = ((y as usize + i) * pw as usize + x as usize) * 4;
                        dst[at..at + row].copy_from_slice(src);
                    }
                }
                if scratch.capacity() > 1 << 20 {
                    scratch = Vec::new();
                }
                SCRATCH.set(scratch);
            }
        }
        target.set_buffer_scale(scale);
        target.attach(Some(&buf.2), 0, 0);
        match &changed {
            Some(rects) if !remapped => rects.iter().for_each(|&(x, y, w, h)| target.damage_buffer(x as i32, y as i32, w as i32, h as i32)),
            _ => target.damage_buffer(0, 0, pw as i32, ph as i32),
        }
        damage.commit(number, items, (pw, ph), changed);
        drop(damage);
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
    /// True when it changed anything (the main surface then needs a commit).
    fn attach_backdrop(&self, c: &Content, w: u32, h: u32) -> bool {
        let img = self.image.borrow().clone();
        let key = (img.as_ref().map_or(0, |i| i.id), w, h);
        if self.backdrop_key.replace(Some(key)) == Some(key) {
            return false;
        }
        let v = &c.viewport;
        v.set_destination(w as i32, h as i32);
        match img {
            Some(img) => {
                let (sx, sy, sw, sh) = crate::ui::image::cover(img.width as f32, img.height as f32, w as f32, h as f32);
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
        true
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
        let (sx, sy, sw, sh) = crate::ui::image::cover(img.width as f32, img.height as f32, w as f32, h as f32);
        v.set_source(sx as f64, sy as f64, sw as f64, sh as f64);
        v.set_destination(w as i32, h as i32);
        self.wl.attach(Some(&img.buffer), 0, 0);
        self.wl.damage_buffer(0, 0, img.width as i32, img.height as i32);
        self.wl.commit();
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ObjectId> for Wl {
    fn event(
        _: &mut Self, l: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ev: zwlr_layer_surface_v1::Event, id: &ObjectId, _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(s) = surface(id) else { return };
        let id = id.clone();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::node::{Node, Sides, Style, shadow};

    fn tree(sel: usize, center: Option<usize>, text: &str) -> Node {
        let rows = (0..12)
            .map(|i| {
                let mut s = Style { padding: Sides::all(8.0), margin: Sides::trbl(0.0, 0.0, 8.0, 0.0), ..Default::default() };
                s.background = Some(Color::from_rgba8(40, 40, 60, 255));
                s.box_shadow = Some(shadow(0.0, 2.0, 6.0, Color::from_rgba8(0, 0, 0, 120)));
                if i == sel {
                    s.translate_y = -2.0;
                    s.outline = Some((2.0, 1.0, Color::from_rgba8(100, 150, 255, 255)));
                }
                Node::vbox(s, 0.0, vec![Node::label(Style::default(), format!("row {i} {text}"))])
            })
            .collect();
        let list = Node::vbox(Style::default(), 0.0, rows);
        let sc = Node::scroll(Style::default(), 300.0, center, list);
        Node::vbox(Style { padding: Sides::all(16.0), background: Some(Color::from_rgba8(20, 20, 30, 230)), ..Default::default() }, 0.0, vec![
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
