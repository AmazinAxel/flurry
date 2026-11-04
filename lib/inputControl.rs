// Fullscreen dimmed dialog windows with key actions.

use std::cell::RefCell;
use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::wrapFocus::wrap_focus;
use crate::ui::entry::{Entry, EntryRef};
use crate::ui::node::{Node, Style};
use crate::ui::wayland::{Anchor, KeyEvent, Keymode, Layer};
use crate::ui::runtime;
use crate::ui::window::{self, Spec, Win};

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
    /// entry focused on show
    pub focus: Option<&'static str>,
    /// all cleared on show and hide
    pub entries: Vec<EntryBinding>,
}

fn find(list: &Actions, k: Keysym) -> Option<&Action> {
    list.iter().find(|(s, _)| *s == k).map(|(_, a)| a)
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

pub fn input_control(name: &'static str, build: impl Fn(Option<&str>) -> Node + 'static, opts: Opts) -> Rc<Win> {
    let spec = Spec { name, layer: Layer::Overlay, anchor: Anchor::all(), keymode: Keymode::Exclusive, ..Default::default() };
    let win = Win::new(spec, build);
    modal(&win);
    let opts = Rc::new(opts);
    let clear = |o: &Opts| {
        for b in &o.entries {
            b.entry.borrow_mut().set_text("");
        }
    };

    let o = opts.clone();
    let weak = Rc::downgrade(&win);
    win.on_show(move || {
        clear(&o);
        if let Some(f) = &o.on_show {
            f();
        }
        let Some(w) = weak.upgrade() else { return };
        *w.focus.borrow_mut() = o.focus.filter(|id| o.entries.iter().any(|b| b.id == *id)).map(String::from);
    });

    // typed text (a password, say) doesn't outlive the dialog
    let o = opts.clone();
    let weak = Rc::downgrade(&win);
    win.visibility.subscribe(move || {
        if weak.upgrade().is_some_and(|w| !w.visible()) {
            clear(&o);
        }
    });

    let o = opts;
    let weak = Rc::downgrade(&win);
    win.on_key(move |ev| {
        let Some(w) = weak.upgrade() else { return };
        if ev.sym == Keysym::Escape {
            return window::toggle(name);
        }
        let action = if ev.alt {
            None
        } else if ev.ctrl {
            find(&o.ctrl_actions, ev.lower())
        } else {
            find(&o.actions, ev.lower())
        };
        if let Some(a) = action {
            a();
            return w.redraw();
        }
        if let Some(f) = &o.on_key
            && f(ev)
        {
            return w.redraw();
        }
        let focused = w.focused();
        let entry = o.entries.iter().find(|b| Some(b.id) == focused.as_deref());
        if wrap_focus(&w, ev, entry.is_some()) {
            return;
        }
        if let Some(b) = entry {
            if let Some(f) = &b.on_key
                && f(ev)
            {
                return w.redraw();
            }
            if matches!(ev.sym, Keysym::Return | Keysym::KP_Enter) {
                if let Some(a) = &b.on_activate {
                    a();
                }
                return w.redraw();
            }
            if Entry::key(&b.entry, ev) {
                return w.redraw();
            }
        }
        // GTK default focus chain: Tab / Shift+Tab
        if matches!(ev.sym, Keysym::Tab | Keysym::ISO_Left_Tab) && !ev.ctrl && !ev.alt {
            w.focus_step(!ev.shift && ev.sym == Keysym::Tab);
        }
    });
    win
}
