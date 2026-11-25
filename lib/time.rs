// Minute clock: ticks on the minute boundary, re-syncs after sleep.

use std::cell::RefCell;

use crate::ui::runtime::{self, Timer};
use crate::ui::state::global_state;

thread_local! {
    static TICK: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

global_state! {
    pub fn now() -> i64 = now_secs() as i64;
}

/// Wall clock, unix seconds.
pub fn now_secs() -> u64 {
    since_epoch().as_secs()
}

fn since_epoch() -> std::time::Duration {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
}

unsafe extern "C" {
    fn tzset();
}

pub fn update() {
    TICK.with(|t| {
        if let Some(t) = t.borrow_mut().take() {
            t.cancel();
        }
    });
    // pick up timezone changes (travel, DST rules) on every tick and wake-up
    unsafe { tzset() };
    let d = since_epoch();
    // minute-aligned, so a re-sync within the same minute changes nothing
    now().set((d.as_secs() / 60 * 60) as i64);
    // rounded up: a timer a fraction of a millisecond early would tick twice for one minute
    let into_minute = (d.as_secs() % 60) as u128 * 1_000_000_000 + d.subsec_nanos() as u128;
    let ms = (60_000_000_000 - into_minute).div_ceil(1_000_000) as u64;
    let t = runtime::timeout(ms, update);
    TICK.with(|x| *x.borrow_mut() = Some(t));
}

/// strftime in local time.
pub fn format(t: i64, fmt: &str) -> String {
    strftime(t, fmt, false)
}

/// strftime on unix seconds, in UTC or local time.
pub fn strftime(t: i64, fmt: &str, utc: bool) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    unsafe {
        if utc {
            libc::gmtime_r(&tt, &mut tm);
        } else {
            libc::localtime_r(&tt, &mut tm);
        }
    }
    let cfmt = std::ffi::CString::new(fmt).unwrap_or_default();
    let mut buf = [0u8; 128];
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), cfmt.as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

pub fn format_now(fmt: &str) -> String {
    format(now().peek(), fmt)
}

pub fn start() {
    update();
    let sleeping = crate::lib::logind::sleeping();
    let s = sleeping.clone();
    sleeping.subscribe(move || {
        if !s.peek() {
            update();
        }
    });
}

/// A one-shot timer on the wall clock (CLOCK_REALTIME timerfd): counts time asleep, unlike the
/// loop's monotonic timers. Dropped: cancelled.
pub struct WallTimer(std::os::fd::OwnedFd);

impl Drop for WallTimer {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        runtime::unwatch(self.0.as_raw_fd());
    }
}

/// Runs `f` once the wall clock reaches `secs` (unix), at once if it already has. A clock change
/// (set by hand, NTP stepping it) runs it early too: the caller re-checks its deadline then.
pub fn at_wall(secs: u64, f: impl FnOnce() + 'static) -> Option<WallTimer> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let fd = unsafe { libc::timerfd_create(libc::CLOCK_REALTIME, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // 0 would disarm it: a deadline in the past is due now all the same
    let at = libc::timespec { tv_sec: secs.clamp(1, i64::MAX as u64) as libc::time_t, tv_nsec: 0 };
    let spec = libc::itimerspec { it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 }, it_value: at };
    let flags = libc::TFD_TIMER_ABSTIME | libc::TFD_TIMER_CANCEL_ON_SET;
    if unsafe { libc::timerfd_settime(fd.as_raw_fd(), flags, &spec, std::ptr::null_mut()) } < 0 {
        return None;
    }
    let raw = fd.as_raw_fd();
    let f = std::cell::Cell::new(Some(f));
    runtime::watch(raw, move || {
        runtime::unwatch(raw);
        if let Some(f) = f.take() {
            f();
        }
    });
    Some(WallTimer(fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn wall_timer_fires_when_due_and_not_once_dropped() {
        let (due, later) = (Rc::new(Cell::new(false)), Rc::new(Cell::new(false)));
        let (d, l) = (due.clone(), later.clone());
        // already past: due at once
        let _t = at_wall(now_secs() - 5, move || d.set(true)).expect("timerfd");
        let dropped = at_wall(now_secs() - 5, move || l.set(true)).expect("timerfd");
        drop(dropped);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !due.get() && std::time::Instant::now() < end {
            runtime::iterate(false);
        }
        assert!(due.get());
        runtime::iterate(false);
        assert!(!later.get());
    }
}
