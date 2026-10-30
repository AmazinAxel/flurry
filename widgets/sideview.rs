// Sideview controller (widgets/sideview.tsx). The web view itself lives in the `flurry-sideview`
// process (Servo), spawned on first use and killed by closeSideview; this side keeps the state
// machine, the dim window and the color scheme sync.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};

use crate::config::sideview_url;
use crate::lib::dbus::{self, SignalFilter, method, set_prop};
use crate::lib::inputControl::{Opts, input_control};
use crate::ui::node::{Node, Style};
use crate::ui::wayland::{Keymode, Layer};
use crate::ui::window;

thread_local! {
    static HELPER: RefCell<Option<(Child, ChildStdin)>> = const { RefCell::new(None) };
    static VISIBLE: Cell<bool> = const { Cell::new(false) };
    static FOCUSED: Cell<bool> = const { Cell::new(false) };
    static PUSHED_ASIDE: Cell<bool> = const { Cell::new(false) };
    static CURRENT: RefCell<Option<String>> = const { RefCell::new(None) };
    static WIDTH: Cell<u32> = const { Cell::new(400) };
    static DARK: Cell<bool> = const { Cell::new(true) };
}

fn send(cmd: &str) {
    HELPER.with(|h| {
        let mut h = h.borrow_mut();
        if let Some((child, _)) = h.as_mut()
            && child.try_wait().ok().flatten().is_some()
        {
            *h = None; // helper died; respawn below
        }
        if h.is_none() {
            let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("flurry-sideview")));
            let exe = exe.filter(|p| p.exists()).map(|p| p.into_os_string()).unwrap_or_else(|| "flurry-sideview".into());
            if let Ok(mut child) = Command::new(exe).stdin(Stdio::piped()).spawn()
                && let Some(stdin) = child.stdin.take()
            {
                *h = Some((child, stdin));
                let dark = DARK.with(Cell::get);
                let width = WIDTH.with(Cell::get);
                if let Some((_, s)) = h.as_mut() {
                    let _ = writeln!(s, "dark {dark}");
                    let _ = writeln!(s, "width {width}");
                }
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
    let ignore = focused && !PUSHED_ASIDE.with(Cell::get);
    send(&format!("focus {focused} {}", if ignore { "ignore" } else { "exclusive" }));
    window::set_visible("sideviewDim", focused);
}

pub fn show_page(name: &str) {
    let Some(url) = sideview_url(name) else { return };
    let visible = VISIBLE.with(Cell::get);
    if visible && CURRENT.with(|c| c.borrow().as_deref() == Some(name)) {
        return hide_sideview();
    }
    let fresh = !visible;
    send(&format!("show {name} {url}"));
    CURRENT.with(|c| *c.borrow_mut() = Some(name.to_string()));
    VISIBLE.with(|v| v.set(true));
    if fresh {
        set_focused(true);
    }
}

pub fn toggle_sideview_focus() {
    if !VISIBLE.with(Cell::get) {
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
    if VISIBLE.with(Cell::get) {
        return hide_sideview();
    }
    if let Some(name) = CURRENT.with(|c| c.borrow().clone()) {
        show_page(&name);
    }
}

/// Destroys all web views (the whole helper process).
pub fn close_sideview() {
    hide_sideview();
    HELPER.with(|h| {
        if let Some((mut child, _)) = h.borrow_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    });
    CURRENT.with(|c| c.borrow_mut().take());
}

pub fn toggle_sideview_size() {
    let next = if WIDTH.with(Cell::get) == 400 { 700 } else { 400 };
    WIDTH.with(|w| w.set(next));
    if HELPER.with(|h| h.borrow().is_some()) {
        send(&format!("width {next}"));
    }
}

/// Mirrors dark/light mode into the night light temperature (and the web views).
fn apply_color_scheme(dark: bool) {
    DARK.with(|d| d.set(dark));
    if let Some(bus) = dbus::session() {
        let mut m = set_prop("rs.wl-gammarelay", "/", "rs.wl.gammarelay", "Temperature", if dark { 3500u16 } else { 6500u16 });
        m.set_auto_start(false);
        bus.send(m);
    }
    if HELPER.with(|h| h.borrow().is_some()) {
        send(&format!("dark {dark}"));
    }
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
    input_control("sideviewDim", |_| Node::hbox(Style::default(), 0.0, vec![]), Opts { keymode: Some(Keymode::None), layer: Some(Layer::Top), ..Default::default() });
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
