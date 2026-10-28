// Backlight via sysfs + kernel uevents (no udev), set through logind (lib/brightness.tsx).

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::lib::state::State;
use crate::ui::runtime;

const DIR: &str = "/sys/class/backlight";

thread_local! {
    static SCREEN: String = {
        let mut names: Vec<String> = std::fs::read_dir(DIR).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        names.sort();
        names.into_iter().next().unwrap_or_default()
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

/// Listens for kernel backlight uevents (what GUdev did), only reacting to external changes.
pub fn monitor() {
    if !available() {
        return;
    }
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, libc::NETLINK_KOBJECT_UEVENT) };
    if fd < 0 {
        return;
    }
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as u16;
    addr.nl_groups = 1;
    if unsafe { libc::bind(fd, (&raw const addr).cast(), std::mem::size_of::<libc::sockaddr_nl>() as u32) } < 0 {
        return;
    }
    // devpath is wherever the driver hangs it (amdgpu: .../card1-eDP-1/amdgpu_bl1), so match name + subsystem
    let tail = format!("/{}", screen());
    runtime::watch(sock.as_raw_fd(), move || {
        let _keep = &sock;
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
            if n <= 0 {
                return;
            }
            let msg = String::from_utf8_lossy(&buf[..n as usize]);
            let header = msg.split('\0').next().unwrap_or("");
            if header.ends_with(&tail) && msg.split('\0').any(|f| f == "SUBSYSTEM=backlight") {
                let v = read(&path()) as f64 / max() as f64;
                brightness().set(v);
            }
        }
    });
}
