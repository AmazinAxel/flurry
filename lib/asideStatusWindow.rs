// Dimmed overlay with the open aside menu at the bottom-left (lib/asideStatusWindow.tsx).

use std::cell::Cell;
use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::asideState::{close_aside_window, current};
use crate::lib::inputControl::{DIM, wrap_focus};
use crate::lib::style::*;
use crate::ui::node::{Align, Background, Node, Sides, Style};
use crate::ui::wayland::{Anchor, Keymode, PointerEvent};
use crate::ui::window::{Spec, Win};
use crate::widgets::status::quicksettings::quicksettings::{self as qs, quick_settings, set_slider, slider_value};
use crate::widgets::status::status::status_margin;
use crate::widgets::status::{bluetooth, calendar, network};

thread_local! {
    static DRAG: Cell<Option<&'static str>> = const { Cell::new(None) };
}

fn content(base: &Style, name: &str, focus: Option<&str>) -> Node {
    match name {
        "quickSettings" => quick_settings(base, focus),
        "bluetooth" => {
            if crate::lib::bluetooth::bt().with(|b| b.available) {
                bluetooth::menu(base, focus, 0.0)
            } else {
                Node::hbox(base.inherit(), 0.0, vec![])
            }
        }
        "wifi" => network::menu(base, focus, 0.0),
        _ => calendar::calendar(base),
    }
}

pub fn aside_status_window() -> Rc<Win> {
    let spec = Spec { name: "asideStatus", anchor: Anchor::all(), keymode: Keymode::Exclusive, dim: Some(DIM), ..Default::default() };
    let w = Win::new(spec, |v| {
        let base = window();
        let Some(name) = current().peek() else { return Node::hbox(base, 0.0, vec![]) };
        let mut s = base.inherit();
        s = s.border(Sides::all(3.0), blue3());
        s.background = Background::Color(dark2());
        s.margin = Sides::all(5.0);
        s.padding = Sides::all(13.0);
        let menu = Node::hbox(base.inherit(), 0.0, vec![content(&base, name, v.focus.as_deref())]).size(300.0, 0.0);
        let card = Node::hbox(s, 0.0, vec![menu]).align(Align::Start, Align::End);
        let mut outer = Node::hbox(base.inherit(), 0.0, vec![card]);
        outer.style.padding.left = status_margin().peek();
        outer
    });

    let ww = Rc::downgrade(&w);
    current().subscribe(move || {
        let Some(w) = ww.upgrade() else { return };
        let open = current().peek();
        if open == Some("calendar") {
            calendar::reset();
        }
        *w.focus.borrow_mut() = match open {
            Some("calendar") => Some("calendar".into()),
            Some("quickSettings") => qs::first_sink(),
            _ => None,
        };
        w.set_visible(open.is_some());
        w.redraw();
    });
    w.watch(&status_margin());
    w.watch(&crate::lib::audio::speakers());
    w.watch(&crate::lib::audio::default_name());
    w.watch(&crate::lib::brightness::brightness());
    w.watch(&crate::widgets::status::quicksettings::sound::selected());
    w.watch(&crate::lib::bluetooth::bt());
    w.watch(&bluetooth::order());
    w.watch(&bluetooth::sticky());
    w.watch(&crate::lib::time::now());
    network::watch(&w);

    let ww = Rc::downgrade(&w);
    w.on_key(move |ev| {
        let Some(w) = ww.upgrade() else { return false };
        let focus = w.focused().unwrap_or_default();
        let done = |w: &Win| {
            w.redraw();
            true
        };
        if network::key(&focus, ev) {
            return done(&w);
        }
        if ev.sym == Keysym::Escape {
            close_aside_window();
            return true;
        }
        let name = current().peek().unwrap_or("");
        if name == "calendar" && calendar::key(ev) {
            return done(&w);
        }
        if name == "bluetooth" && bluetooth::key(&focus, ev) {
            return done(&w);
        }
        if matches!(ev.sym, Keysym::Return | Keysym::KP_Enter | Keysym::space) {
            match name {
                "quickSettings" if matches!(ev.sym, Keysym::Return | Keysym::KP_Enter) => {
                    qs::activate(&focus, ev.shift);
                }
                "bluetooth" => {
                    bluetooth::activate(&focus);
                }
                "wifi" => {
                    if let Some(Some(next)) = network::activate(&focus) {
                        w.grab_focus(&next);
                    }
                }
                _ => {}
            }
            return done(&w);
        }
        let tab = matches!(ev.sym, Keysym::Tab | Keysym::ISO_Left_Tab) && !ev.ctrl && !ev.alt;
        if wrap_focus(&w, ev, false) || (tab && w.focus_step(!ev.shift && ev.sym == Keysym::Tab)) {
            if let Some(f) = w.focused() {
                bluetooth::on_focus(&f);
                network::on_focus(&f);
            }
            return done(&w);
        }
        false
    });

    let ww = Rc::downgrade(&w);
    w.on_pointer(move |ev, _| {
        let Some(w) = ww.upgrade() else { return };
        let hit = |x: f64, y: f64| {
            ["volume", "brightness"].into_iter().find_map(|id| w.rect_of(id).filter(|r| (x as f32) >= r.left() && (x as f32) < r.right() && (y as f32) >= r.top() && (y as f32) < r.bottom()).map(|r| (id, r)))
        };
        match ev {
            PointerEvent::Button { x, y, pressed: true } => {
                if let Some((id, r)) = hit(x, y) {
                    DRAG.with(|d| d.set(Some(id)));
                    set_slider(id, slider_value(r, x as f32));
                }
            }
            PointerEvent::Button { pressed: false, .. } | PointerEvent::Leave => DRAG.with(|d| d.set(None)),
            PointerEvent::Motion { x, .. } => {
                if let Some(id) = DRAG.with(Cell::get)
                    && let Some(r) = w.rect_of(id)
                {
                    set_slider(id, slider_value(r, x as f32));
                }
            }
            _ => {}
        }
    });
    w
}

