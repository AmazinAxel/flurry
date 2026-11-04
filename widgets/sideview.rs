// Sideview controller. The web view itself lives in the `flurry-sideview` process (Servo),
// started by the first show and stopped by closeSideview; this side keeps the state machine, the
// dim window, passes the color scheme on and opens the links the pages ask for.

use std::cell::{Cell, RefCell};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::lib::colorScheme::{self, dark};
use crate::lib::inputControl::DIM;
use crate::lib::style;
use crate::settings;
use crate::ui::node::{Node, Style};
use crate::ui::runtime;
use crate::ui::wayland::{Anchor, Layer};
use crate::ui::window::{self, Spec, Win};

/// Built-in sideview pages; settings.sideview overrides and extends them.
const BUILT_IN: &[(&str, &str)] = &[("assistant", "flurry:claude")];

fn sideview_url(page: &str) -> Option<&'static str> {
    settings::SIDEVIEW.iter().chain(BUILT_IN).find(|(p, _)| *p == page).map(|(_, u)| *u)
}

struct Helper {
    pid: u32,
    pidfd: Rc<OwnedFd>,
    stdin: ChildStdin,
}

thread_local! {
    static HELPER: RefCell<Option<Helper>> = const { RefCell::new(None) };
    static VISIBLE: Cell<bool> = const { Cell::new(false) };
    static FOCUSED: Cell<bool> = const { Cell::new(false) };
    /// the helper reported the panel on screen since the last show
    static MAPPED: Cell<bool> = const { Cell::new(false) };
    static PUSHED_ASIDE: Cell<bool> = const { Cell::new(false) };
    static CURRENT: RefCell<Option<String>> = const { RefCell::new(None) };
    static WIDTH: Cell<u32> = const { Cell::new(style::SIDEVIEW_WIDTH) };
    static OPENED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
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

fn visible() -> bool {
    VISIBLE.with(Cell::get)
}

/// A pidfd can't hit a reused pid, even after the reap.
fn kill(pidfd: &OwnedFd) {
    unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd.as_raw_fd(), libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0) };
}

/// Reaps the helper on exit; the current one's exit also takes its panel down.
fn watch_exit(child: Child, pidfd: Rc<OwnedFd>) {
    let pid = child.id();
    let raw = pidfd.as_raw_fd();
    let child = RefCell::new(child);
    runtime::watch(raw, move || {
        runtime::unwatch(raw);
        let _keep = &pidfd;
        let _ = child.borrow_mut().try_wait();
        // a closed helper's exit (close_sideview) leaves the current one alone
        if HELPER.with(|h| h.borrow().as_ref().is_some_and(|h| h.pid == pid)) {
            helper_gone();
        }
    });
}

/// A page's link, opened outside the sandbox: http(s) only, one a second at most.
fn open(url: &str) {
    let ok = (url.starts_with("https://") || url.starts_with("http://")) && !url.chars().any(|c| c.is_whitespace() || c.is_control());
    let now = Instant::now();
    if !ok || !visible() || OPENED_AT.with(Cell::get).is_some_and(|at| now - at < Duration::from_secs(1)) {
        return;
    }
    OPENED_AT.with(|o| o.set(Some(now)));
    crate::lib::apps::launch(&["xdg-open".into(), url.into()]);
}

/// Helper stdout lines: "mapped" once the panel is on screen after a show, "closed" when its
/// output went away, "open <url>" for a link the user followed.
fn watch_stdout(out: ChildStdout) {
    let raw = out.as_raw_fd();
    runtime::set_nonblocking(raw, true);
    let out = RefCell::new(out);
    let line = RefCell::new(Vec::<u8>::new());
    runtime::watch(raw, move || {
        let mut buf = [0u8; 4096];
        let mut lines = vec![];
        let eof = loop {
            match out.borrow_mut().read(&mut buf) {
                Ok(0) => break true,
                Ok(n) => {
                    let mut line = line.borrow_mut();
                    for &b in &buf[..n] {
                        if b != b'\n' {
                            // anything else it prints is no command of ours: never kept past a line
                            if line.len() < 4096 {
                                line.push(b);
                            }
                            continue;
                        }
                        lines.push(String::from_utf8_lossy(&line).into_owned());
                        line.clear();
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break false,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break true,
            }
        };
        for l in lines {
            match l.split_once(' ') {
                None if l == "mapped" && visible() => {
                    MAPPED.with(|m| m.set(true));
                    sync_dim();
                }
                None if l == "closed" && visible() => hide_sideview(),
                Some(("open", url)) => open(url),
                _ => {}
            }
        }
        if eof {
            // the helper is exiting (its exit is watched separately)
            runtime::unwatch(raw);
        }
    });
}

/// One command line, without blocking; false if it didn't all go in (the helper isn't reading,
/// or a torn line).
fn write_line(s: &mut ChildStdin, cmd: &str) -> bool {
    let line = format!("{cmd}\n");
    s.write(line.as_bytes()).is_ok_and(|n| n == line.len())
}

/// Starts the helper (hidden) if none runs; false if it can't be.
fn ensure_helper(h: &mut Option<Helper>) -> bool {
    if h.is_some() {
        return true;
    }
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("flurry-sideview")));
    let exe = exe.filter(|p| p.exists()).map(|p| p.into_os_string()).unwrap_or_else(|| "flurry-sideview".into());
    // its one argument: the panel background before a page paints
    let bg = style::dark1().to_color_u8();
    let bg = format!("{:02x}{:02x}{:02x}", bg.red(), bg.green(), bg.blue());
    let Ok(mut child) = Command::new(exe).arg(bg).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() else { return false };
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as libc::pid_t, 0) } as i32;
    let (Some(mut stdin), Some(out), true) = (child.stdin.take(), child.stdout.take(), fd >= 0) else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    let pidfd = Rc::new(unsafe { OwnedFd::from_raw_fd(fd) });
    runtime::set_nonblocking(stdin.as_raw_fd(), true);
    watch_stdout(out);
    let pid = child.id();
    watch_exit(child, pidfd.clone());
    MAPPED.with(|m| m.set(false));
    // an empty pipe takes these whole
    write_line(&mut stdin, &format!("dark {}", dark().peek()));
    write_line(&mut stdin, &format!("width {}", WIDTH.with(Cell::get)));
    write_line(&mut stdin, &focus_cmd());
    *h = Some(Helper { pid, pidfd, stdin });
    true
}

fn focus_cmd() -> String {
    let focused = FOCUSED.with(Cell::get);
    let ignore = focused && !PUSHED_ASIDE.with(Cell::get);
    format!("focus {focused} {}", if ignore { "ignore" } else { "exclusive" })
}

/// Only `show` starts the helper; other commands without one are moot (a new helper gets the
/// current state first). A helper that can't take a command is stopped: its state would be wrong.
/// False if there is no helper after all.
fn send(cmd: &str) -> bool {
    let sent = HELPER.with(|h| {
        let mut h = h.borrow_mut();
        if cmd.starts_with("show ") && !ensure_helper(&mut h) {
            return None;
        }
        let h = h.as_mut()?;
        if write_line(&mut h.stdin, cmd) {
            return Some(true);
        }
        kill(&h.pidfd);
        Some(false)
    });
    if sent == Some(false) {
        helper_gone();
    }
    sent == Some(true)
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
    if let Some(Helper { pidfd, stdin, .. }) = HELPER.with(|h| h.borrow_mut().take()) {
        drop(stdin); // EOF: the helper exits by itself (stopping Claude Code); its watch reaps it
        runtime::timeout(1000, move || kill(&pidfd));
    }
    CURRENT.with(|c| c.borrow_mut().take());
}

pub fn toggle_sideview_size() {
    let next = if WIDTH.with(Cell::get) == style::SIDEVIEW_WIDTH { style::SIDEVIEW_WIDE } else { style::SIDEVIEW_WIDTH };
    WIDTH.with(|w| w.set(next));
    send(&format!("width {next}"));
}

pub fn sideview() {
    Win::new(Spec { name: "sideviewDim", layer: Layer::Top, anchor: Anchor::all(), dim: Some(DIM), ..Default::default() }, |_| Node::hbox(Style::default(), 0.0, vec![]));
    dark().subscribe(|| {
        send(&format!("dark {}", dark().peek()));
    });
    colorScheme::start();
}
