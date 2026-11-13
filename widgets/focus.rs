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
    grayscale(false);
    focus_text().subscribe(|| grayscale(focus()));
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

/// Grays every output through a sway ICC profile (Vulkan renderer only); off restores sway's
/// default gamma22. Reset at start so a crash mid-focus never leaves the screen gray.
fn grayscale(on: bool) {
    let path = crate::lib::xdg::runtime_dir().map(|d| format!("{d}/flurry-gray.icc"));
    match path {
        Some(p) if on && std::fs::write(&p, gray_icc()).is_ok() => {
            crate::lib::sway::command(&format!("output * color_profile icc {p}"))
        }
        _ => crate::lib::sway::command("output * color_profile gamma22"),
    }
}

/// A v2 display profile whose only output table (B2A0, Lab PCS) maps L* to a neutral gray:
/// 2-point CLUT copying L to all channels, then output curves L -> Y -> gamma 2.2.
/// A matrix/shaper profile can't do this: lcms inverts the matrix, and a gray one is singular.
fn gray_icc() -> Vec<u8> {
    const N: usize = 4096;
    let be32 = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_be_bytes());
    let be16 = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_be_bytes());
    let d50 = [0xF6D6, 0x1_0000, 0xD32D];
    let mut wtpt = b"XYZ \0\0\0\0".to_vec();
    d50.iter().for_each(|&x| be32(&mut wtpt, x));
    let mut lut = b"mft2\0\0\0\0\x03\x03\x02\0".to_vec();
    (0..9).for_each(|i| be32(&mut lut, if i % 4 == 0 { 0x1_0000 } else { 0 }));
    be16(&mut lut, 2);
    be16(&mut lut, N as u16);
    (0..3).for_each(|_| [0, 0xFFFF].iter().for_each(|&x| be16(&mut lut, x)));
    // grid order: L varies slowest, so the first 4 points are L=0
    (0..8).for_each(|i| (0..3).for_each(|_| be16(&mut lut, if i < 4 { 0 } else { 0xFFFF })));
    for _ in 0..3 {
        for t in 0..N {
            // v2 Lab: L 100 is 0xFF00
            let l = t as f64 / (N - 1) as f64 * 65535.0 / 65280.0 * 100.0;
            let y = if l > 8.0 { ((l + 16.0) / 116.0).powi(3) } else { l / 903.3 };
            be16(&mut lut, (y.min(1.0).powf(1.0 / 2.2) * 65535.0).round() as u16);
        }
    }
    let tags = [(b"wtpt", wtpt), (b"B2A0", lut)];
    let mut off = 128 + 4 + 12 * tags.len();
    let mut p = vec![0u8; 128];
    be32(&mut p, tags.len() as u32);
    for (sig, data) in &tags {
        p.extend_from_slice(*sig);
        be32(&mut p, off as u32);
        be32(&mut p, data.len() as u32);
        off += data.len().next_multiple_of(4);
    }
    for (_, data) in &tags {
        p.extend_from_slice(data);
        p.resize(p.len().next_multiple_of(4), 0);
    }
    let len = p.len() as u32;
    p[0..4].copy_from_slice(&len.to_be_bytes());
    p[8..12].copy_from_slice(&0x0210_0000u32.to_be_bytes());
    p[12..24].copy_from_slice(b"mntrRGB Lab ");
    p[36..40].copy_from_slice(b"acsp");
    for (i, x) in d50.iter().enumerate() {
        p[68 + 4 * i..72 + 4 * i].copy_from_slice(&x.to_be_bytes());
    }
    p
}

fn tick_bar(w: std::rc::Weak<Win>) {
    let Some(win) = w.upgrade().filter(|_| break_start().peek().is_some()) else {
        return TICKING.with(|t| t.set(false));
    };
    win.redraw();
    runtime::timeout(66, move || tick_bar(w));
}
