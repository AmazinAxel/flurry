// Focus task: entry dialog, top label with break progress.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use crate::lib::backgroundSection::background_section;
use crate::lib::inputControl::{EntryBinding, Opts, input_control};
use crate::lib::process;
use crate::lib::searchDialog::search_entry;
use crate::lib::style::*;
use crate::ui::entry;
use crate::ui::node::{Node, Sides, Style, TRANSPARENT, levelbar};
use crate::ui::runtime::{self, Timer};
use crate::ui::state::global_state;
use crate::ui::wayland::{Anchor, Layer};
use crate::ui::window::{self, Spec, Win};

/// The wait before a blocked app runs.
const WAIT_MS: u64 = 10_000;
/// How long a blocked app may run, and the warning before it closes.
const RUN_SECS: u64 = 300;
const WARN_SECS: u64 = 15;

thread_local! {
    static BREAK_TIMERS: RefCell<Vec<Timer>> = const { RefCell::new(Vec::new()) };
    static CLOSING_NOTIF: Cell<u32> = const { Cell::new(0) };
    /// the break bar's repaint timer is armed
    static TICKING: Cell<bool> = const { Cell::new(false) };
}

global_state! {
    pub fn focus_text() -> String = String::new();
    /// start of the wait before a blocked app runs
    fn break_start() -> Option<Instant> = None;
}
pub fn focus() -> bool {
    !focus_text().peek().is_empty()
}

fn end_break() {
    BREAK_TIMERS.with(|t| t.borrow_mut().drain(..).for_each(Timer::cancel));
    break_start().set(None);
}

pub fn cancel_break() {
    if break_start().peek().is_some() {
        end_break();
    }
}

fn active() -> bool {
    BREAK_TIMERS.with(|t| !t.borrow().is_empty())
}

/// The wait shown as a levelbar, then the blocked app runs for at most RUN_SECS.
pub fn start_break(argv: Vec<String>) {
    if active() {
        return;
    }
    break_start().set(Some(Instant::now()));
    let t = runtime::timeout(WAIT_MS, move || {
        end_break();
        runtime::spawn(async move {
            let max = format!("RuntimeMaxSec={RUN_SECS}");
            let mut cmd = vec!["systemd-run", "--user", "--scope", "--collect", "-p", &max, "--"];
            cmd.extend(argv.iter().map(String::as_str));
            let _ = process::status(&cmd).await;
            end_break();
            let id = CLOSING_NOTIF.with(|n| n.replace(0));
            if id != 0 {
                crate::lib::notifd::dismiss(id);
            }
        });
        let t = runtime::timeout((RUN_SECS - WARN_SECS) * 1000, || {
            let id = crate::lib::notifySend::notify_send("Focus", &format!("App closing in {WARN_SECS}s"), "", vec![]);
            CLOSING_NOTIF.with(|n| n.set(id));
        });
        BREAK_TIMERS.with(|x| x.borrow_mut().push(t));
    });
    BREAK_TIMERS.with(|x| x.borrow_mut().push(t));
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
        move |_| {
            let base = window();
            let header = search_entry(&base, &ec);
            background_section(&base, header, None, 0.0, 400.0)
        },
        Opts { focus: Some("entry"), entries: vec![EntryBinding { id: "entry", entry: e, on_activate: Some(start), on_key: None }], ..Default::default() },
    );
}

pub fn focus_task() -> Rc<Win> {
    let spec = Spec { name: "focusTask", anchor: Anchor::Top, layer: Layer::Top, per_monitor: true, ..Default::default() };
    let w = Win::new(spec, |_| {
        let base = window();
        let mut l = base.inherit();
        l.margin = Sides::trbl(SWAY_GAP, SPACE_L, SPACE_L, SPACE_L);
        l = l.border(Sides::trbl(0.0, BORDER, BORDER, BORDER), blue3());
        l.font.size = 32.0;
        l.font.weight = 700;
        l.background = Some(dark1());
        let text = Style { padding: Sides::vh(SPACE_S, SPACE_M), ..l.inherit() };
        let start = break_start().peek();
        let progress = start.map_or(0.0, |s| s.elapsed().as_secs_f64() * 1000.0 / WAIT_MS as f64);
        // the bar overlays the card's bottom edge, so the card keeps its size
        Node::vbox(base.inherit(), 0.0, vec![Node::bottom_bar(
            l,
            Node::label(text, focus_text().peek()),
            levelbar(&base, progress, blue3(), TRANSPARENT, 6.0).show(start.is_some()),
        )])
    });
    let fw = Rc::downgrade(&w);
    focus_text().subscribe(move || {
        if let Some(w) = fw.upgrade() {
            w.show_redraw(focus());
        }
    });
    w.watch(&break_start());
    // the break bar steps at ~15Hz while it runs, not on every frame
    let fw = Rc::downgrade(&w);
    break_start().subscribe(move || {
        if break_start().peek().is_some() && !TICKING.with(|t| t.replace(true)) {
            tick_bar(fw.clone());
        }
    });
    w
}

fn tick_bar(w: std::rc::Weak<Win>) {
    let Some(win) = w.upgrade().filter(|_| break_start().peek().is_some()) else {
        return TICKING.with(|t| t.set(false));
    };
    win.redraw();
    runtime::timeout(66, move || tick_bar(w));
}
