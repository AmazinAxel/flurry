// Sideview controller. The web view itself lives in the `flurry-sideview` process (Servo),
// prespawned hidden once the shell is up and stopped by closeSideview; this side keeps the state
// machine, the dim window and the color scheme sync.

use std::cell::{Cell, RefCell};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::config::sideview_url;
use crate::lib::dbus::{self, SignalFilter, method, set_prop};
use crate::lib::inputControl::DIM;
use crate::ui::node::{Node, Style};
use crate::ui::runtime;
use crate::ui::wayland::{Anchor, Layer};
use crate::ui::window::{self, Spec, Win};

/// Longest command line written: a pipe write up to PIPE_BUF is all or nothing, so a full pipe
/// drops a command instead of tearing it.
const PIPE_BUF: usize = 4096;

thread_local! {
    static HELPER: RefCell<Option<(Child, ChildStdin)>> = const { RefCell::new(None) };
    static VISIBLE: Cell<bool> = const { Cell::new(false) };
    static FOCUSED: Cell<bool> = const { Cell::new(false) };
    /// the helper reported the panel on screen since the last show
    static MAPPED: Cell<bool> = const { Cell::new(false) };
    static PUSHED_ASIDE: Cell<bool> = const { Cell::new(false) };
    static CURRENT: RefCell<Option<String>> = const { RefCell::new(None) };
    static WIDTH: Cell<u32> = const { Cell::new(400) };
    static DARK: Cell<bool> = const { Cell::new(true) };
}

/// The dim goes up only with the panel actually on screen, and only while it has the keyboard.
fn sync_dim() {
    let on = FOCUSED.with(Cell::get) && VISIBLE.with(Cell::get) && MAPPED.with(Cell::get);
    window::set_visible("sideviewDim", on);
}

/// The helper is gone: its panel (and the dim) went with it.
fn helper_gone() {
    HELPER.with(|h| h.borrow_mut().take());
    MAPPED.with(|m| m.set(false));
    if VISIBLE.with(|v| v.replace(false)) {
        PUSHED_ASIDE.with(|p| p.set(false));
        FOCUSED.with(|f| f.set(false));
    }
    sync_dim();
}

/// Whether the sideview is shown; a helper that died takes its panel (and the dim) with it.
fn visible() -> bool {
    let died = HELPER.with(|h| h.borrow_mut().as_mut().is_some_and(|(child, _)| child.try_wait().ok().flatten().is_some()));
    if died {
        helper_gone();
    }
    VISIBLE.with(Cell::get)
}

/// Exit of helper `pid` runs the cleanup right away (pidfd), not at the next command.
fn watch_exit(pid: u32) {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) } as i32;
    if fd < 0 {
        return; // old kernel: noticed by visible() at the next command
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let raw = fd.as_raw_fd();
    runtime::watch(raw, move || {
        runtime::unwatch(raw);
        let _keep = &fd;
        // a closed helper's exit (close_sideview) leaves the current one alone
        if HELPER.with(|h| h.borrow().as_ref().is_some_and(|(c, _)| c.id() == pid)) {
            visible();
        }
    });
}

/// Helper stdout lines: "mapped" once the panel is on screen after a show.
fn watch_stdout(out: ChildStdout) {
    let raw = out.as_raw_fd();
    unsafe { libc::fcntl(raw, libc::F_SETFL, libc::fcntl(raw, libc::F_GETFL) | libc::O_NONBLOCK) };
    let out = RefCell::new(out);
    let line = RefCell::new(Vec::<u8>::new());
    runtime::watch(raw, move || {
        let mut buf = [0u8; 512];
        loop {
            match out.borrow_mut().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let mut line = line.borrow_mut();
                    for &b in &buf[..n] {
                        if b != b'\n' {
                            // anything else it prints is no command of ours: never kept past a line
                            if line.len() < 64 {
                                line.push(b);
                            }
                            continue;
                        }
                        if *line == b"mapped" && VISIBLE.with(Cell::get) {
                            MAPPED.with(|m| m.set(true));
                            sync_dim();
                        }
                        line.clear();
                    }
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(_) => break,
            }
        }
        // EOF: the helper is exiting (its exit is watched separately)
        runtime::unwatch(raw);
    });
}

/// One command line, without blocking: dropped when the helper isn't reading (pipe full).
fn write_line(s: &mut ChildStdin, cmd: &str) {
    let line = format!("{cmd}\n");
    if line.len() > PIPE_BUF {
        return;
    }
    // EINTR before anything was written; a few tries at most
    for _ in 0..3 {
        match s.write(line.as_bytes()) {
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            _ => return,
        }
    }
}

/// Starts the helper (hidden) if none runs; false if it can't be.
fn ensure_helper(h: &mut Option<(Child, ChildStdin)>) -> bool {
    if h.is_some() {
        return true;
    }
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("flurry-sideview")));
    let exe = exe.filter(|p| p.exists()).map(|p| p.into_os_string()).unwrap_or_else(|| "flurry-sideview".into());
    let Ok(mut child) = Command::new(exe).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() else { return false };
    let (Some(mut s), Some(out)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        std::thread::spawn(move || child.wait());
        return false;
    };
    let fd = s.as_raw_fd();
    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
    watch_stdout(out);
    watch_exit(child.id());
    MAPPED.with(|m| m.set(false));
    write_line(&mut s, &format!("dark {}", DARK.with(Cell::get)));
    write_line(&mut s, &format!("width {}", WIDTH.with(Cell::get)));
    write_line(&mut s, &focus_cmd());
    *h = Some((child, s));
    true
}

fn focus_cmd() -> String {
    let focused = FOCUSED.with(Cell::get);
    let ignore = focused && !PUSHED_ASIDE.with(Cell::get);
    format!("focus {focused} {}", if ignore { "ignore" } else { "exclusive" })
}

/// Only `show` starts the helper (when the prespawned one is gone); other commands without one
/// are moot (a new helper gets the current state first). False if there is no helper after all.
fn send(cmd: &str) -> bool {
    visible();
    HELPER.with(|h| {
        let mut h = h.borrow_mut();
        if cmd.starts_with("show ") && !ensure_helper(&mut h) {
            return false;
        }
        match h.as_mut() {
            Some((_, s)) => {
                write_line(s, cmd);
                true
            }
            None => false,
        }
    })
}

fn set_focused(focused: bool) {
    FOCUSED.with(|f| f.set(focused));
    send(&focus_cmd());
    sync_dim();
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
    if !send(&format!("show {name} {url}")) {
        // no helper: nothing to show, nothing to dim
        return set_focused(false);
    }
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
    MAPPED.with(|m| m.set(false));
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
    // hidden and ready by the first show, which then only maps the panel
    runtime::idle(|| HELPER.with(|h| _ = ensure_helper(&mut h.borrow_mut())));
    let Some(bus) = dbus::session() else { return };
    bus.subscribe(
        SignalFilter {
            sender: Some("org.freedesktop.portal.Desktop".into()),
            interface: Some("org.freedesktop.portal.Settings".into()),
            member: Some("SettingChanged".into()),
            ..Default::default()
        },
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
