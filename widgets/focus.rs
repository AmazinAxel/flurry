// Focus task: entry dialog, top label with break progress (widgets/focus.tsx).

use std::cell::RefCell;
use std::rc::Rc;

use crate::lib::backgroundSection::background_section;
use crate::lib::inputControl::{EntryBinding, Opts, input_control};
use crate::lib::process::exec;
use crate::lib::searchDialog::search_entry;
use crate::lib::state::State;
use crate::lib::style::*;
use crate::ui::entry;
use crate::ui::node::{Background, Kind, Node, Sides, Style, fill_rect, rgba};
use crate::ui::runtime::{self, Timer};
use crate::ui::wayland::{Anchor, Layer};
use crate::ui::window::{self, Spec, Win};

thread_local! {
    static FOCUS_TEXT: State<String> = State::new(String::new());
    static BREAK_WAIT: State<f64> = State::new(-1.0);
    static BREAK_TIMERS: RefCell<Vec<Timer>> = const { RefCell::new(Vec::new()) };
    static BREAK_START: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
    static CLOSING_NOTIF: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub fn focus_text() -> State<String> {
    FOCUS_TEXT.with(Clone::clone)
}
pub fn focus() -> bool {
    !focus_text().peek().is_empty()
}
fn break_wait() -> State<f64> {
    BREAK_WAIT.with(Clone::clone)
}

fn end_break() {
    BREAK_TIMERS.with(|t| t.borrow_mut().drain(..).for_each(Timer::cancel));
    BREAK_START.with(|s| s.set(None));
    break_wait().set(-1.0);
}

pub fn cancel_break() {
    if break_wait().peek() >= 0.0 {
        end_break();
    }
}

fn active() -> bool {
    BREAK_TIMERS.with(|t| !t.borrow().is_empty())
}

/// 10s wait shown as a levelbar, then the blocked app runs for at most 2 minutes.
pub fn start_break(cmd: &str) {
    if active() {
        return;
    }
    BREAK_START.with(|s| s.set(Some(std::time::Instant::now())));
    break_wait().set(0.0);
    let cmd = cmd.to_string();
    let t = runtime::timeout(10_000, move || {
        end_break();
        let cmd = cmd.clone();
        runtime::spawn(async move {
            let _ = exec(&["systemd-run", "--user", "--scope", "--collect", "-p", "RuntimeMaxSec=120", "sh", "-c", &cmd]).await;
            end_break();
            let id = CLOSING_NOTIF.with(|n| n.replace(0));
            if id != 0 {
                crate::lib::notifd::dismiss(id);
            }
        });
        let t = runtime::timeout(105_000, || {
            let id = crate::lib::notifySend::notify_send("Focus", "App closing in 15s", "", vec![]);
            CLOSING_NOTIF.with(|n| n.set(id));
        });
        BREAK_TIMERS.with(|x| x.borrow_mut().push(t));
    });
    BREAK_TIMERS.with(|x| x.borrow_mut().push(t));
    if let Some(w) = window::get("focusTask") {
        w.redraw();
    }
}

pub fn toggle_focus() {
    if active() {
        return;
    }
    if focus() {
        focus_text().set(String::new());
    } else {
        window::toggle("focusMenu");
    }
}

pub fn focus_menu() {
    let e = entry::new("");
    e.borrow_mut().max_len = Some(60);
    let start = {
        let e = e.clone();
        Rc::new(move || {
            let text = e.borrow().text.trim().to_string();
            if text.is_empty() {
                return;
            }
            focus_text().set(text);
            window::set_visible("focusMenu", false);
        })
    };
    let ec = e.clone();
    input_control(
        "focusMenu",
        move |v| {
            let base = window();
            let header = search_entry(&base, &ec, "emoji-flags-symbolic", v.focused("entry"));
            background_section(&base, header, None, 0.0, 400.0)
        },
        Opts { focus: Some("entry"), entries: vec![EntryBinding { id: "entry", entry: e, on_activate: Some(start), on_key: None }], ..Default::default() },
    );
}

/// GTK levelbar progress (continuous mode): trough with the filled block over it.
pub fn levelbar(base: &Style, value: f64, filled: tiny_skia::Color, empty: tiny_skia::Color, height: f32) -> Node {
    let paint = Rc::new(move |pm: &mut tiny_skia::PixmapMut, r: tiny_skia::Rect, ts: tiny_skia::Transform| {
        fill_rect(pm, r, empty, ts);
        if let Some(f) = tiny_skia::Rect::from_xywh(r.x(), r.y(), r.width() * value.clamp(0.0, 1.0) as f32, r.height()) {
            fill_rect(pm, f, filled, ts);
        }
    });
    let key = Some(crate::ui::node::hash(&(value.to_bits(), [filled, empty].map(|c| [c.red(), c.green(), c.blue(), c.alpha()].map(f32::to_bits)))));
    Node::new(Kind::Custom { width: 0.0, height, key, paint }, base.inherit())
}

pub fn focus_task() -> Rc<Win> {
    let spec = Spec { name: "focusTask", anchor: Anchor::Top, layer: Layer::Top, per_monitor: true, ..Default::default() };
    let w = Win::new(spec, |_| {
        let base = window();
        let mut l = base.inherit();
        l.margin = Sides::trbl(5.0, 0.0, 0.0, 0.0);
        l = l.border(Sides::trbl(0.0, 3.0, 3.0, 3.0), blue3());
        l.font.size = 32.0;
        l.font.weight = 700;
        l.padding = Sides::vh(3.0, 6.0);
        l.background = Background::Color(dark1());
        let wait = break_wait().peek();
        let progress = BREAK_START.with(|s| s.get()).map(|s| s.elapsed().as_secs_f64() / 10.0).unwrap_or(wait.max(0.0));
        Node::vbox(base.inherit(), 0.0, vec![
            Node::label(l, focus_text().peek()),
            levelbar(&base, progress, blue3(), rgba(255, 255, 255, 0.12), 6.0).show(wait >= 0.0),
        ])
    });
    let fw = Rc::downgrade(&w);
    focus_text().subscribe(move || {
        if let Some(w) = fw.upgrade() {
            w.set_visible(focus());
            w.redraw();
        }
    });
    w.watch(&break_wait());
    // animate the 10s break bar on frame callbacks only while it runs
    let fw = Rc::downgrade(&w);
    break_wait().subscribe(move || {
        if let Some(w) = fw.upgrade() {
            w.animate(|| active() && BREAK_START.with(|s| s.get()).is_some());
        }
    });
    w
}

