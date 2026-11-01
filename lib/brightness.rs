// Backlight via sysfs (no udev), set through logind.

use std::os::fd::AsRawFd;

use crate::lib::state::State;
use crate::ui::runtime;

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
    static BRIGHTNESS: State<f64> = State::new(read(&path()) as f64 / max().max(1) as f64);
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

pub fn brightness() -> State<f64> {
    BRIGHTNESS.with(Clone::clone)
}

pub fn set_brightness(percent: f64) {
    let max = max();
    if max == 0 {
        return;
    }
    let steps = (percent * max as f64).floor().clamp(0.0, max as f64) as u32;
    brightness().set(steps as f64 / max as f64);
    crate::lib::logind::set_brightness(&screen(), steps);
}

/// The backlight core sysfs_notify()s actual_brightness on every change (keys, logind, other tools),
/// so this wakes only for brightness changes, not for every kernel uevent.
pub fn monitor() {
    let Ok(file) = std::fs::File::open(format!("{DIR}/{}/actual_brightness", screen())) else { return };
    // reading re-arms the notification; false once it fails (device gone)
    let fd = file.as_raw_fd();
    let rearm = move || {
        let mut b = [0u8; 16];
        unsafe { libc::pread(file.as_raw_fd(), b.as_mut_ptr().cast(), b.len(), 0) >= 0 }
    };
    if !rearm() {
        return;
    }
    runtime::watch_pri(fd, move || {
        if !rearm() {
            // an unread sysfs notification would wake poll() forever
            runtime::unwatch(fd);
            return;
        }
        brightness().set(read(&path()) as f64 / max().max(1) as f64);
    });
}
