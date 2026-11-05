// logind: PrepareForSleep, session Lock and LockedHint, a delay inhibitor fd, power actions, backlight.

use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
use std::rc::Rc;

use crate::lib::dbus::{self, SignalFilter, method};
use crate::ui::state::global_state;

const NAME: &str = "org.freedesktop.login1";
const PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const SESSION: &str = "org.freedesktop.login1.Session";

thread_local! {
    static INHIBITOR: RefCell<Option<OwnedFd>> = const { RefCell::new(None) };
    static WANTED: Cell<bool> = const { Cell::new(false) };
    /// this session's object path, once GetSession answers
    static SESSION_PATH: RefCell<Option<String>> = const { RefCell::new(None) };
    static ON_LOCK: RefCell<Option<Rc<dyn Fn()>>> = RefCell::default();
}

global_state! {
    pub fn sleeping() -> bool = false;
}

/// The login session: XDG_SESSION_ID, else (a user service has none) the user's graphical session
/// as logind records it (what sd_uid_get_display reads); "" if neither is known.
pub fn session_id() -> String {
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.is_empty()
    {
        return id;
    }
    let state = std::fs::read_to_string(format!("/run/systemd/users/{}", unsafe { libc::getuid() })).unwrap_or_default();
    state.lines().find_map(|l| l.strip_prefix("DISPLAY=")).unwrap_or("").to_string()
}

/// `f` runs on the session's Lock signal (loginctl lock-session, IdleAction=lock). Unlock is
/// deliberately ignored: any process of the user could send it.
pub fn on_lock(f: impl Fn() + 'static) {
    ON_LOCK.with(|l| *l.borrow_mut() = Some(Rc::new(f)));
}

pub fn start() {
    let Some(bus) = dbus::system() else {
        eprintln!("flurry: no system bus: no lock before sleep");
        return;
    };
    bus.subscribe(
        SignalFilter {
            sender: Some(NAME.into()),
            interface: Some(MANAGER.into()),
            member: Some("PrepareForSleep".into()),
            path: Some(PATH.into()),
            ..Default::default()
        },
        |m| {
            if let Ok(s) = m.read1::<bool>() {
                // every PrepareForSleep(true) must reach the lock, even a repeated one
                if s && sleeping().peek() { sleeping().notify() } else { sleeping().set(s) }
            }
        },
    );
    let id = session_id();
    if id.is_empty() {
        return;
    }
    let b = bus.clone();
    bus.call_cb(method(NAME, PATH, MANAGER, "GetSession").append1(id), move |r| {
        let Some(path) = r.ok().and_then(|m| m.read1::<dbus::Path>().ok()).map(|p| p.to_string()) else { return };
        b.subscribe(
            SignalFilter { sender: Some(NAME.into()), interface: Some(SESSION.into()), member: Some("Lock".into()), path: Some(path.clone()), ..Default::default() },
            |_| {
                if let Some(f) = ON_LOCK.with(|l| l.borrow().clone()) {
                    f();
                }
            },
        );
        SESSION_PATH.with(|s| *s.borrow_mut() = Some(path));
    });
}

fn session_path() -> String {
    SESSION_PATH.with(|s| s.borrow().clone()).unwrap_or_else(|| format!("{PATH}/session/auto"))
}

/// Session.SetLockedHint: what `loginctl show-session` and idle managers see.
pub fn set_locked_hint(locked: bool) {
    if let Some(bus) = dbus::system() {
        bus.send(method(NAME, &session_path(), SESSION, "SetLockedHint").append1(locked));
    }
}

pub fn inhibit_sleep(why: &str) {
    if WANTED.with(|w| w.replace(true)) {
        return;
    }
    let Some(bus) = dbus::system() else { return };
    let msg = method(NAME, PATH, MANAGER, "Inhibit").append3("sleep", "flurry", why).append1("delay");
    bus.call_cb(msg, |r| match r.ok().and_then(|m| m.read1::<OwnedFd>().ok()) {
        Some(fd) => {
            if WANTED.with(Cell::get) {
                INHIBITOR.with(|i| *i.borrow_mut() = Some(fd));
            }
        }
        None => WANTED.with(|w| w.set(false)),
    });
}

pub fn release_sleep() {
    WANTED.with(|w| w.set(false));
    INHIBITOR.with(|i| i.borrow_mut().take());
}

fn power(action: &str) {
    if let Some(bus) = dbus::system() {
        bus.send(method(NAME, PATH, MANAGER, action).append1(false));
    }
}

pub fn suspend() {
    power("Suspend");
}
pub fn hibernate() {
    power("Hibernate");
}
pub fn poweroff() {
    power("PowerOff");
}
pub fn reboot() {
    power("Reboot");
}

pub fn set_brightness(device: &str, value: u32) {
    if let Some(bus) = dbus::system() {
        bus.send(method(NAME, &session_path(), SESSION, "SetBrightness").append3("backlight", device, value));
    }
}

/// `set_brightness`, then `done(written)` on logind's reply; false (and `done` never runs) without a bus.
pub fn set_brightness_then(device: &str, value: u32, done: impl FnOnce(bool) + 'static) -> bool {
    let Some(bus) = dbus::system() else { return false };
    bus.call_cb(method(NAME, &session_path(), SESSION, "SetBrightness").append3("backlight", device, value), move |r| done(r.is_ok()));
    true
}
