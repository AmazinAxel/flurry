// Named windows (app.toggle_window / get_window), per-monitor instances, focus chains with GTK-like
// arrow navigation, and redraw-on-state-change wiring.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use tiny_skia::Rect;

use super::node::{Kind, Node};
use super::wayland::{self, Anchor, KeyEvent, Keymode, Layer, LayerSpec, PointerEvent, Surface};
use crate::lib::state::State;

#[derive(Clone)]
pub struct Spec {
    pub name: &'static str,
    pub layer: Layer,
    pub anchor: Anchor,
    pub keymode: Keymode,
    pub exclusive: i32,
    /// rgba dim behind content (`.backgroundDim`)
    pub dim: Option<[f32; 4]>,
    pub input: bool,
    pub per_monitor: bool,
    pub width: u32,
    pub height: u32,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            name: "",
            layer: Layer::Overlay,
            anchor: Anchor::empty(),
            keymode: Keymode::None,
            exclusive: 0,
            dim: None,
            input: true,
            per_monitor: false,
            width: 0,
            height: 0,
        }
    }
}

/// What a window's build function sees.
pub struct View {
    pub focus: Option<String>,
}

impl View {
    pub fn focused(&self, id: &str) -> bool {
        self.focus.as_deref() == Some(id)
    }
}

pub type Build = Rc<dyn Fn(&View) -> Node>;
pub type KeyHandler = Rc<dyn Fn(&KeyEvent) -> bool>;

pub struct Win {
    pub spec: RefCell<Spec>,
    build: Build,
    surfaces: RefCell<Vec<(String, Surface)>>,
    visible: Cell<bool>,
    pub focus: RefCell<Option<String>>,
    focusables: RefCell<Vec<(String, Rect)>>,
    rects: RefCell<Vec<(String, Rect)>>,
    key: RefCell<Option<KeyHandler>>,
    pointer: RefCell<Option<Rc<dyn Fn(PointerEvent, &str)>>>,
    on_show: RefCell<Vec<Rc<dyn Fn()>>>,
    pub visibility: State<bool>,
    image: RefCell<Option<Rc<wayland::ShmImage>>>,
}

thread_local! {
    static WINDOWS: RefCell<HashMap<&'static str, Rc<Win>>> = RefCell::default();
    static TRIM: RefCell<Option<super::runtime::Timer>> = const { RefCell::new(None) };
}

pub fn get(name: &str) -> Option<Rc<Win>> {
    WINDOWS.with(|w| w.borrow().get(name).cloned())
}

pub fn toggle(name: &str) {
    if let Some(w) = get(name) {
        w.set_visible(!w.visible());
    }
}

/// Redraws every shown window (state they all read, like the wallpaper, changed).
pub fn redraw_all() {
    let all: Vec<Rc<Win>> = WINDOWS.with(|w| w.borrow().values().filter(|w| w.visible()).cloned().collect());
    for w in all {
        w.redraw();
    }
}

pub fn set_visible(name: &str, v: bool) {
    if let Some(w) = get(name) {
        w.set_visible(v);
    }
}


impl Win {
    pub fn new(spec: Spec, build: impl Fn(&View) -> Node + 'static) -> Rc<Win> {
        let w = Rc::new(Win {
            spec: RefCell::new(spec.clone()),
            build: Rc::new(build),
            surfaces: RefCell::default(),
            visible: Cell::new(false),
            focus: RefCell::default(),
            focusables: RefCell::default(),
            rects: RefCell::default(),
            key: RefCell::default(),
            pointer: RefCell::default(),
            on_show: RefCell::default(),
            visibility: State::new(false),
            image: RefCell::default(),
        });
        if spec.per_monitor {
            let weak = Rc::downgrade(&w);
            wayland::on_outputs(move || {
                if let Some(w) = weak.upgrade()
                    && w.visible()
                {
                    w.sync_outputs();
                }
            });
        }
        WINDOWS.with(|m| m.borrow_mut().insert(spec.name, w.clone()));
        w
    }

    pub fn visible(&self) -> bool {
        self.visible.get()
    }

    pub fn set_visible(self: &Rc<Self>, v: bool) {
        if self.visible.replace(v) == v {
            return;
        }
        if v {
            for f in self.on_show.borrow().clone() {
                f();
            }
            self.sync_outputs();
        } else {
            self.surfaces.borrow_mut().clear();
            // hand freed buffers back to the kernel once things settle, never in the hide path
            let t = super::runtime::timeout(1000, || unsafe {
                libc::malloc_trim(0);
            });
            if let Some(old) = TRIM.with(|x| x.borrow_mut().replace(t)) {
                old.cancel();
            }
        }
        self.visibility.set(v);
    }

    pub fn show(self: &Rc<Self>) {
        self.set_visible(true);
    }
    pub fn hide(self: &Rc<Self>) {
        self.set_visible(false);
    }

    /// Hide + show: recreates the surfaces (lockscreen's prepareUnlock).
    pub fn remap(self: &Rc<Self>) {
        if self.visible() {
            self.surfaces.borrow_mut().clear();
            self.sync_outputs();
        }
    }

    pub fn on_show(&self, f: impl Fn() + 'static) {
        self.on_show.borrow_mut().push(Rc::new(f));
    }
    pub fn on_key(&self, f: impl Fn(&KeyEvent) -> bool + 'static) {
        *self.key.borrow_mut() = Some(Rc::new(f));
    }
    pub fn on_pointer(&self, f: impl Fn(PointerEvent, &str) + 'static) {
        *self.pointer.borrow_mut() = Some(Rc::new(f));
    }

    fn sync_outputs(self: &Rc<Self>) {
        let spec = self.spec.borrow().clone();
        let outputs = wayland::outputs();
        let wanted: Vec<(String, Option<_>)> = if spec.per_monitor {
            outputs.iter().map(|o| (o.name.clone(), Some(o.wl.clone()))).collect()
        } else {
            vec![(String::new(), None)]
        };
        self.surfaces.borrow_mut().retain(|(n, _)| wanted.iter().any(|(w, _)| w == n));
        for (name, output) in wanted {
            if self.surfaces.borrow().iter().any(|(n, _)| *n == name) {
                continue;
            }
            let surface = self.create(&spec, name.clone(), output);
            self.surfaces.borrow_mut().push((name, surface));
        }
    }

    fn create(self: &Rc<Self>, spec: &Spec, output_name: String, output: Option<wayland_client::protocol::wl_output::WlOutput>) -> Surface {
        let weak = Rc::downgrade(self);
        let build: wayland::BuildFn = Rc::new(move |_, _| {
            let Some(win) = weak.upgrade() else { return Node::label(Default::default(), "") };
            let view = View { focus: win.focus.borrow().clone() };
            (win.build)(&view)
        });
        let layer = LayerSpec {
            namespace: spec.name,
            layer: spec.layer,
            anchor: spec.anchor,
            keymode: spec.keymode,
            exclusive: spec.exclusive,
            output,
            dim: spec.dim,
            input: spec.input,
            width: spec.width,
            height: spec.height,
        };
        let s = Surface::layer(layer, build);
        if let Some(img) = self.image.borrow().clone() {
            s.set_image(img);
        }
        let weak = Rc::downgrade(self);
        s.on_key(move |ev| {
            if let Some(w) = weak.upgrade() {
                let h = w.key.borrow().clone();
                if let Some(h) = h {
                    h(ev);
                }
            }
        });
        let weak = Rc::downgrade(self);
        let name = output_name;
        s.on_pointer(move |ev| {
            if let Some(w) = weak.upgrade() {
                let h = w.pointer.borrow().clone();
                if let Some(h) = h {
                    h(ev, &name);
                }
            }
        });
        let weak = Rc::downgrade(self);
        s.after_render(move |node| {
            if let Some(w) = weak.upgrade() {
                let mut list = vec![];
                collect_focusables(node, &mut list);
                *w.focusables.borrow_mut() = list;
                let mut all = vec![];
                collect_ids(node, &mut all);
                *w.rects.borrow_mut() = all;
            }
        });
        s
    }

    /// Steps `f` on every frame of each surface, re-rendering while it returns true.
    pub fn animate(&self, f: impl Fn() -> bool + 'static) {
        let f: Rc<dyn Fn() -> bool> = Rc::new(f);
        for (_, s) in self.surfaces.borrow().iter() {
            s.animate(f.clone());
        }
    }

    pub fn redraw(&self) {
        for (_, s) in self.surfaces.borrow().iter() {
            s.redraw();
        }
    }

    /// Redraws whenever `state` changes.
    pub fn watch<T: Clone + PartialEq + 'static>(self: &Rc<Self>, state: &State<T>) {
        let weak = Rc::downgrade(self);
        state.subscribe(move || {
            if let Some(w) = weak.upgrade()
                && w.visible()
            {
                w.redraw();
            }
        });
    }

    pub fn set_keymode(&self, k: Keymode) {
        self.spec.borrow_mut().keymode = k;
        for (_, s) in self.surfaces.borrow().iter() {
            s.set_keymode(k);
        }
    }

    pub fn grab_focus(&self, id: &str) {
        *self.focus.borrow_mut() = Some(id.to_string());
        self.redraw();
    }

    pub fn focused(&self) -> Option<String> {
        self.focus.borrow().clone()
    }

    pub fn focusables(&self) -> Vec<(String, Rect)> {
        self.focusables.borrow().clone()
    }

    /// GTK child_focus with a direction: nearest focusable that way; returns false at the edge.
    pub fn child_focus(&self, dir: Dir) -> bool {
        let list = self.focusables();
        let Some(cur) = self.focused() else {
            if let Some((id, _)) = list.first() {
                self.grab_focus(&id.clone());
                return true;
            }
            return false;
        };
        let Some(&(_, cr)) = list.iter().find(|(id, _)| *id == cur) else { return false };
        match pick(&list, cr, dir) {
            Some(id) => {
                self.grab_focus(&id);
                true
            }
            None => false,
        }
    }

    /// Arrow navigation with wrap-around (lib/wrapFocus.ts).
    pub fn wrap_focus(&self, dir: Dir) {
        if !self.child_focus(dir) {
            for _ in 0..1000 {
                if !self.child_focus(dir.opposite()) {
                    break;
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    pub fn opposite(self) -> Dir {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
        }
    }
}

fn collect_focusables(n: &Node, out: &mut Vec<(String, Rect)>) {
    if !n.visible {
        return;
    }
    if let (Some(id), true) = (&n.id, n.focusable) {
        out.push((id.clone(), n.rect));
    }
    if let Kind::Scroll { .. } = n.kind {
        // keep scrolled rows navigable even when clipped
    }
    for c in &n.children {
        collect_focusables(c, out);
    }
}

/// GTK's directional focus: candidates entirely past the current widget in `dir`, preferring those
/// overlapping it on the other axis, then the closest.
fn pick(list: &[(String, Rect)], cur: Rect, dir: Dir) -> Option<String> {
    let (cx, cy) = (cur.x() + cur.width() / 2.0, cur.y() + cur.height() / 2.0);
    list.iter()
        .filter(|(_, r)| match dir {
            Dir::Up => r.bottom() <= cur.top() + 0.5,
            Dir::Down => r.top() >= cur.bottom() - 0.5,
            Dir::Left => r.right() <= cur.left() + 0.5,
            Dir::Right => r.left() >= cur.right() - 0.5,
        })
        .map(|(id, r)| {
            let (x, y) = (r.x() + r.width() / 2.0, r.y() + r.height() / 2.0);
            let (main, cross, overlap) = match dir {
                Dir::Up | Dir::Down => ((y - cy).abs(), (x - cx).abs(), r.left() < cur.right() && r.right() > cur.left()),
                Dir::Left | Dir::Right => ((x - cx).abs(), (y - cy).abs(), r.top() < cur.bottom() && r.bottom() > cur.top()),
            };
            (id, (!overlap, main, cross))
        })
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
        .map(|(id, _)| id.clone())
}

impl Win {
    /// Shows a shared image on every surface (wallpaper) instead of a rendered tree.
    pub fn set_image(&self, img: Option<Rc<wayland::ShmImage>>) {
        *self.image.borrow_mut() = img.clone();
        if let Some(img) = img {
            for (_, s) in self.surfaces.borrow().iter() {
                s.set_image(img.clone());
            }
        }
    }
}

fn collect_ids(n: &Node, out: &mut Vec<(String, Rect)>) {
    if !n.visible {
        return;
    }
    if let Some(id) = &n.id {
        out.push((id.clone(), n.rect));
    }
    for c in &n.children {
        collect_ids(c, out);
    }
}

impl Win {
    /// Layout rect of node `id` from the last render.
    pub fn rect_of(&self, id: &str) -> Option<Rect> {
        self.rects.borrow().iter().find(|(i, _)| i == id).map(|(_, r)| *r)
    }
}

impl Win {
    /// Tab / Shift+Tab: next or previous focusable in tree order, wrapping around (GTK focus chain).
    pub fn focus_step(&self, forward: bool) -> bool {
        let list = self.focusables();
        if list.is_empty() {
            return false;
        }
        let cur = self.focused().and_then(|f| list.iter().position(|(id, _)| *id == f));
        let n = list.len();
        let next = match (cur, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        self.grab_focus(&list[next].0.clone());
        true
    }
}
