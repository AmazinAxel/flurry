// Fullscreen dimmed dialog windows with key actions (lib/inputControl.tsx).

use std::cell::RefCell;
use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::ui::entry::{Entry, EntryRef};
use crate::ui::node::{Node, Style};
use crate::ui::wayland::{Anchor, KeyEvent, Keymode, Layer};
use crate::ui::runtime;
use crate::ui::window::{self, Dir, Spec, View, Win};

pub type Action = Rc<dyn Fn()>;
pub type Actions = Vec<(Keysym, Action)>;

pub struct EntryBinding {
    pub id: &'static str,
    pub entry: EntryRef,
    pub on_activate: Option<Action>,
    /// Extra keys on this entry (passSave's Tab), before editing
    pub on_key: Option<Rc<dyn Fn(&KeyEvent) -> bool>>,
}

#[derive(Default)]
pub struct Opts {
    pub on_show: Option<Action>,
    pub actions: Actions,
    pub ctrl_actions: Actions,
    pub on_key: Option<Rc<dyn Fn(&KeyEvent) -> bool>>,
    pub keymode: Option<Keymode>,
    pub layer: Option<Layer>,
    /// entry focused (and cleared) on show
    pub focus: Option<&'static str>,
    pub entries: Vec<EntryBinding>,
}

pub const DIM: [f32; 4] = [0.0, 0.0, 0.0, 0.3];

thread_local! {
    static MODALS: RefCell<Vec<&'static str>> = RefCell::default();
    // one dim for every modal, created before any of them (so stacked under) and kept while switching
    static BACKDROP: Rc<Win> = Win::new(
        Spec { name: "modalDim", anchor: Anchor::all(), dim: Some(DIM), input: false, ..Default::default() },
        |_| Node::hbox(Style::default(), 0.0, vec![]),
    );
}

/// Makes `win` modal: showing it hides the open one, all sharing one dim surface.
pub fn modal(win: &Rc<Win>) {
    let name = win.spec.borrow().name;
    // own backdrop transparent: still fullscreen for input, content stays in its subsurface
    win.spec.borrow_mut().dim = Some([0.0; 4]);
    MODALS.with(|m| m.borrow_mut().push(name));
    let weak = Rc::downgrade(win);
    win.on_show(move || {
        // the dim waits for this window's first frame: both go out in one flush, never the dim alone
        let fresh = BACKDROP.with(|b| {
            let fresh = !b.visible();
            b.show();
            if fresh {
                b.hold();
            }
            fresh
        });
        // the open one goes once this one's first frame is up, so switching never shows a gap;
        // registered after show() has (re)mapped the surfaces, which then wait for a configure
        let weak = weak.clone();
        runtime::idle(move || {
            let Some(w) = weak.upgrade() else { return };
            if fresh {
                w.on_commit(|| BACKDROP.with(|b| b.release()));
            }
            w.after_commit(move || {
                if !window::get(name).is_some_and(|w| w.visible()) {
                    return;
                }
                for other in MODALS.with(|m| m.borrow().clone()) {
                    if other != name {
                        window::set_visible(other, false);
                    }
                }
            });
        });
    });
    win.visibility.subscribe(|| {
        if !MODALS.with(|m| m.borrow().iter().any(|n| window::get(n).is_some_and(|w| w.visible()))) {
            BACKDROP.with(|b| b.hide());
        }
    });
}

/// lib/wrapFocus.ts: arrows move focus (with wrap) unless a modifier is held or the focused widget
/// consumes them (entries own Left/Right and typed letters). hjkl work like arrows outside entries.
pub fn wrap_focus(win: &Win, ev: &KeyEvent, entry_focused: bool) -> bool {
    let vim = matches!(ev.sym, Keysym::h | Keysym::j | Keysym::k | Keysym::l);
    let dir = match ev.sym {
        Keysym::Up | Keysym::k => Dir::Up,
        Keysym::Down | Keysym::j => Dir::Down,
        Keysym::Left | Keysym::h => Dir::Left,
        Keysym::Right | Keysym::l => Dir::Right,
        _ => return false,
    };
    if win.focused().is_none() || ev.ctrl || ev.alt || ev.shift || (entry_focused && (vim || matches!(dir, Dir::Left | Dir::Right))) {
        return false;
    }
    win.wrap_focus(dir);
    true
}

pub fn input_control(name: &'static str, build: impl Fn(&View) -> Node + 'static, opts: Opts) -> Rc<Win> {
    let spec = Spec {
        name,
        layer: opts.layer.unwrap_or(Layer::Overlay),
        anchor: Anchor::all(),
        keymode: opts.keymode.unwrap_or(Keymode::Exclusive),
        ..Default::default()
    };
    let win = Win::new(spec, build);
    modal(&win);
    let opts = Rc::new(opts);

    let o = opts.clone();
    let weak = Rc::downgrade(&win);
    win.on_show(move || {
        if let Some(f) = &o.on_show {
            f();
        }
        let Some(w) = weak.upgrade() else { return };
        *w.focus.borrow_mut() = None;
        if let Some(id) = o.focus
            && let Some(b) = o.entries.iter().find(|b| b.id == id)
        {
            b.entry.borrow_mut().set_text("");
            *w.focus.borrow_mut() = Some(id.to_string());
        }
    });

    let o = opts.clone();
    let weak = Rc::downgrade(&win);
    win.on_key(move |ev| {
        let Some(w) = weak.upgrade() else { return false };
        if ev.sym == Keysym::Escape {
            window::toggle(name);
            return true;
        }
        let lower = ev.lower();
        let action = o.actions.iter().find(|(k, _)| *k == lower).map(|(_, a)| a.clone()).or_else(|| {
            ev.ctrl.then(|| o.ctrl_actions.iter().find(|(k, _)| *k == lower).map(|(_, a)| a.clone())).flatten()
        });
        if let Some(a) = action {
            a();
            w.redraw();
            return true;
        }
        if let Some(f) = &o.on_key
            && f(ev)
        {
            w.redraw();
            return true;
        }
        let focused = w.focused();
        let entry = o.entries.iter().find(|b| Some(b.id) == focused.as_deref());
        if wrap_focus(&w, ev, entry.is_some()) {
            return true;
        }
        if let Some(b) = entry {
            if let Some(f) = &b.on_key
                && f(ev)
            {
                w.redraw();
                return true;
            }
            if matches!(ev.sym, Keysym::Return | Keysym::KP_Enter) {
                if let Some(a) = &b.on_activate {
                    a();
                }
                w.redraw();
                return true;
            }
            if Entry::key(&b.entry, ev) {
                w.redraw();
                return true;
            }
        }
        // GTK default focus chain: Tab / Shift+Tab
        if matches!(ev.sym, Keysym::Tab | Keysym::ISO_Left_Tab) && !ev.ctrl && !ev.alt {
            return w.focus_step(!ev.shift && ev.sym == Keysym::Tab);
        }
        false
    });
    win
}
