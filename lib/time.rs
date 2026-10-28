// Minute clock (lib/time.ts): ticks on the minute boundary, re-syncs after sleep.

use std::cell::RefCell;

use crate::lib::state::State;
use crate::ui::runtime::{self, Timer};

thread_local! {
    pub static NOW: State<i64> = State::new(now_secs());
    static TICK: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn now() -> State<i64> {
    NOW.with(Clone::clone)
}

pub fn update() {
    TICK.with(|t| {
        if let Some(t) = t.borrow_mut().take() {
            t.cancel();
        }
    });
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    now().set(d.as_secs() as i64);
    let ms = (60 - d.as_secs() % 60) * 1000 - d.subsec_millis() as u64;
    let t = runtime::timeout(ms, update);
    TICK.with(|x| *x.borrow_mut() = Some(t));
}

/// strftime in local time (GLib.DateTime.format equivalents used: %H %M %m %d %a %Y-%m-%d_%H-%M-%S).
pub fn format(t: i64, fmt: &str) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let tt = t as libc::time_t;
    unsafe { libc::localtime_r(&tt, &mut tm) };
    let cfmt = std::ffi::CString::new(fmt).unwrap_or_default();
    let mut buf = [0u8; 128];
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), cfmt.as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

pub fn format_now(fmt: &str) -> String {
    format(now().peek(), fmt)
}

pub fn start() {
    unsafe extern "C" {
        fn tzset();
    }
    unsafe { tzset() };
    update();
    let sleeping = crate::lib::logind::sleeping();
    let s = sleeping.clone();
    sleeping.subscribe(move || {
        if !s.peek() {
            update();
        }
    });
}
