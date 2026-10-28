// Fullscreen dimmed dialog windows with key actions (lib/inputControl.tsx).

use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::ui::entry::{Entry, EntryRef};
use crate::ui::node::Node;
use crate::ui::wayland::{Anchor, KeyEvent, Keymode, Layer};
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
    pub searchable: bool,
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

/// lib/wrapFocus.ts: arrows move focus (with wrap) unless a modifier is held or the focused widget
/// consumes them (entries own Left/Right).
pub fn wrap_focus(win: &Win, ev: &KeyEvent, owns_horizontal: bool) -> bool {
    let dir = match ev.sym {
        Keysym::Up => Dir::Up,
        Keysym::Down => Dir::Down,
        Keysym::Left => Dir::Left,
        Keysym::Right => Dir::Right,
        _ => return false,
    };
    if win.focused().is_none() || ev.ctrl || ev.alt || ev.shift || (owns_horizontal && matches!(dir, Dir::Left | Dir::Right)) {
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
        dim: Some(DIM),
        ..Default::default()
    };
    let win = Win::new(spec, build);
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
