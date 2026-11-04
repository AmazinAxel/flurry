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
