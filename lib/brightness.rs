// Backlight via sysfs (no udev): written directly when the file is writable (a udev rule), else
// through logind.

use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

use crate::ui::runtime;
use crate::ui::state::global_state;

const DIR: &str = "/sys/class/backlight";

thread_local! {
    /// firmware > platform > raw, as the kernel's sysfs-class-backlight ABI doc tells userspace; then by name
    static SCREEN: String = {
        let rank = |n: &str| match std::fs::read_to_string(format!("{DIR}/{n}/type")).as_deref().map(str::trim) {
            Ok("firmware") => 0,
            Ok("platform") => 1,
            Ok("raw") => 2,
            _ => 3,
        };
        let names = std::fs::read_dir(DIR).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>()).unwrap_or_default();
        names.into_iter().min_by_key(|n| (rank(n), n.clone())).unwrap_or_default()
    };
    static MAX: u32 = read(&format!("{DIR}/{}/max_brightness", screen()));
}

fn read(p: &str) -> u32 {
    std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

pub fn screen() -> String {
    SCREEN.with(Clone::clone)
}
fn max() -> u32 {
    MAX.with(|m| *m)
}
fn path() -> String {
    format!("{DIR}/{}/brightness", screen())
}

pub fn available() -> bool {
    !screen().is_empty()
}

global_state! {
    pub fn brightness() -> f64 = read(&path()) as f64 / max().max(1) as f64;
}

/// Direct sysfs write: synchronous, no bus round trip.
fn write(v: u32) -> bool {
    std::fs::write(path(), v.to_string()).is_ok()
}

pub fn set_brightness(percent: f64) {
    let max = max();
    if max == 0 {
        return;
    }
    let steps = (percent * max as f64).floor().clamp(0.0, max as f64) as u32;
    brightness().set(steps as f64 / max as f64);
    if !write(steps) {
        crate::lib::logind::set_brightness(&screen(), steps);
    }
}

fn actual() -> u32 {
    read(&format!("{DIR}/{}/actual_brightness", screen()))
}

/// Brightness keys: steps the requested level 15% until the panel's actual level moves (amdgpu maps
/// a range of low requests to one floor level) or the end is reached; the OSD shows either way.
pub fn step(up: bool) {
    thread_local! {
        /// level last sent while a step is in flight, so fast repeats build on it instead of the stale sysfs value
        static PENDING: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    }
    fn show() {
        PENDING.set(None);
        // the requested level: amdgpu's floor (actual) sits a few % above a request of 0
        let v = read(&path()) as f64 / max().max(1) as f64;
        if brightness().peek() == v {
            brightness().notify();
        } else {
            brightness().set(v);
        }
    }
    fn go(req: u32, start: u32, up: bool) {
        let max = max();
        let delta = (max as f64 * 0.15).round().max(1.0) as u32;
        let next = if up { req.saturating_add(delta).min(max) } else { req.saturating_sub(delta) };
        if next == req {
            return show();
        }
        if write(next) {
            return if actual() == start { go(next, start, up) } else { show() };
        }
        PENDING.set(Some(next));
        // a failed write (or no bus) stops here rather than stepping on
        let sent = crate::lib::logind::set_brightness_then(&screen(), next, move |ok| if ok && actual() == start { go(next, start, up) } else { show() });
        if !sent {
            show();
        }
    }
    if max() == 0 {
        return;
    }
    go(PENDING.get().unwrap_or_else(|| read(&path())), actual(), up);
}

/// The backlight core sysfs_notify()s actual_brightness on every change (keys, logind, other tools),
/// so this wakes only for brightness changes, not for every kernel uevent; shown is the requested
/// level, as the keys and the slider set it.
pub fn monitor() {
    let Ok(file) = std::fs::File::open(format!("{DIR}/{}/actual_brightness", screen())) else { return };
    // reading re-arms the notification; None once it fails (device gone)
    let fd = file.as_raw_fd();
    let rearm = move || {
        let mut b = [0u8; 16];
        file.read_at(&mut b, 0).ok()
    };
    if rearm().is_none() {
        return;
    }
    runtime::watch_pri(fd, move || {
        if rearm().is_none() {
            // an unread sysfs notification would wake poll() forever
            runtime::unwatch(fd);
            return;
        }
        brightness().set(read(&path()) as f64 / max().max(1) as f64);
    });
}
