// Sideview controller. The web view itself lives in the `flurry-sideview` process (Servo),
// spawned by the first show and stopped by closeSideview; this side keeps the state machine, the
// dim window and the color scheme sync.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};

use crate::config::sideview_url;
use crate::lib::dbus::{self, SignalFilter, method, set_prop};
use crate::lib::inputControl::DIM;
use crate::ui::node::{Node, Style};
use crate::ui::runtime;
use crate::ui::wayland::{Anchor, Layer};
use crate::ui::window::{self, Spec, Win};

thread_local! {
    static HELPER: RefCell<Option<(Child, ChildStdin)>> = const { RefCell::new(None) };
    static VISIBLE: Cell<bool> = const { Cell::new(false) };
    static FOCUSED: Cell<bool> = const { Cell::new(false) };
    static PUSHED_ASIDE: Cell<bool> = const { Cell::new(false) };
    static CURRENT: RefCell<Option<String>> = const { RefCell::new(None) };
    static WIDTH: Cell<u32> = const { Cell::new(400) };
    static DARK: Cell<bool> = const { Cell::new(true) };
}

/// Whether the sideview is shown; a helper that died takes its panel (and the dim) with it.
fn visible() -> bool {
    let died = HELPER.with(|h| {
        let mut h = h.borrow_mut();
        let died = h.as_mut().is_some_and(|(child, _)| child.try_wait().ok().flatten().is_some());
        if died {
            *h = None;
        }
        died
    });
    if died && VISIBLE.with(|v| v.replace(false)) {
        PUSHED_ASIDE.with(|p| p.set(false));
        FOCUSED.with(|f| f.set(false));
        window::set_visible("sideviewDim", false);
    }
    VISIBLE.with(Cell::get)
}

fn focus_cmd() -> String {
    let focused = FOCUSED.with(Cell::get);
    let ignore = focused && !PUSHED_ASIDE.with(Cell::get);
    format!("focus {focused} {}", if ignore { "ignore" } else { "exclusive" })
}

/// Only `show` starts the helper; other commands without one are moot (a new helper gets the
/// current state first).
fn send(cmd: &str) {
    visible();
    HELPER.with(|h| {
        let mut h = h.borrow_mut();
        if h.is_none() && cmd.starts_with("show ") {
            let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("flurry-sideview")));
            let exe = exe.filter(|p| p.exists()).map(|p| p.into_os_string()).unwrap_or_else(|| "flurry-sideview".into());
            if let Ok(mut child) = Command::new(exe).stdin(Stdio::piped()).spawn()
                && let Some(mut s) = child.stdin.take()
            {
                let _ = writeln!(s, "dark {}", DARK.with(Cell::get));
                let _ = writeln!(s, "width {}", WIDTH.with(Cell::get));
                let _ = writeln!(s, "{}", focus_cmd());
                *h = Some((child, s));
            }
        }
        if let Some((_, s)) = h.as_mut() {
            let _ = writeln!(s, "{cmd}");
            let _ = s.flush();
        }
    });
}

fn set_focused(focused: bool) {
    FOCUSED.with(|f| f.set(focused));
    send(&focus_cmd());
    window::set_visible("sideviewDim", focused);
}

pub fn show_page(name: &str) {
    let Some(url) = sideview_url(name) else { return };
    let visible = visible();
    if visible && CURRENT.with(|c| c.borrow().as_deref() == Some(name)) {
        return hide_sideview();
    }
    if !visible {
        // focus first: the show then maps the panel once, already with the keyboard
        set_focused(true);
    }
    send(&format!("show {name} {url}"));
    CURRENT.with(|c| *c.borrow_mut() = Some(name.to_string()));
    VISIBLE.with(|v| v.set(true));
}

pub fn toggle_sideview_focus() {
    if !visible() {
        return;
    }
    PUSHED_ASIDE.with(|p| p.set(true)); // first focus makes it take screen space only
    set_focused(!FOCUSED.with(Cell::get));
}

pub fn hide_sideview() {
    PUSHED_ASIDE.with(|p| p.set(false));
    VISIBLE.with(|v| v.set(false));
    send("hide");
    set_focused(false);
}

/// Hides the sideview, or brings back the last shown page.
pub fn toggle_sideview() {
    if visible() {
        return hide_sideview();
    }
    if let Some(name) = CURRENT.with(|c| c.borrow().clone()) {
        show_page(&name);
    }
}

/// Destroys all web views (the whole helper process).
pub fn close_sideview() {
    hide_sideview();
    if let Some((mut child, stdin)) = HELPER.with(|h| h.borrow_mut().take()) {
        drop(stdin); // EOF: the helper exits by itself (stopping Claude Code)
        runtime::timeout(1000, move || {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            std::thread::spawn(move || child.wait()); // reaped off the loop
        });
    }
    CURRENT.with(|c| c.borrow_mut().take());
}

pub fn toggle_sideview_size() {
    let next = if WIDTH.with(Cell::get) == 400 { 700 } else { 400 };
    WIDTH.with(|w| w.set(next));
    send(&format!("width {next}"));
}

/// Mirrors dark/light mode into the night light temperature (and the web views).
fn apply_color_scheme(dark: bool) {
    DARK.with(|d| d.set(dark));
    if let Some(bus) = dbus::session() {
        let mut m = set_prop("rs.wl-gammarelay", "/", "rs.wl.gammarelay", "Temperature", if dark { 3500u16 } else { 6500u16 });
        m.set_auto_start(false);
        bus.send(m);
    }
    send(&format!("dark {dark}"));
}

fn read_scheme() {
    let Some(bus) = dbus::session() else { return };
    let m = method("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Settings", "ReadOne")
        .append2("org.freedesktop.appearance", "color-scheme");
    bus.call_cb(m, |r| {
        // 1 = prefer dark
        let v = r.ok().and_then(|m| m.read1::<dbus::Variant<Box<dyn dbus::RefArg>>>().ok()).and_then(|v| v.0.as_u64());
        apply_color_scheme(v == Some(1));
    });
}

pub fn sideview() {
    Win::new(Spec { name: "sideviewDim", layer: Layer::Top, anchor: Anchor::all(), dim: Some(DIM), ..Default::default() }, |_| Node::hbox(Style::default(), 0.0, vec![]));
    let Some(bus) = dbus::session() else { return };
    bus.subscribe(
        SignalFilter { interface: Some("org.freedesktop.portal.Settings".into()), member: Some("SettingChanged".into()), ..Default::default() },
        |m| {
            if let Ok((ns, key, v)) = m.read3::<&str, &str, dbus::Variant<Box<dyn dbus::RefArg>>>()
                && ns == "org.freedesktop.appearance"
                && key == "color-scheme"
            {
                apply_color_scheme(v.0.as_u64() == Some(1));
            }
        },
    );
    read_scheme();
    bus.watch_name("rs.wl-gammarelay", || apply_color_scheme(DARK.with(Cell::get)), || {});
}
