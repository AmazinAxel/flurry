// logind: PrepareForSleep, a delay inhibitor fd, power actions, backlight (lib/logind.ts, lib/power.ts).

use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;

use crate::lib::dbus::{self, SignalFilter, method};
use crate::lib::state::State;

const NAME: &str = "org.freedesktop.login1";
const PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";

thread_local! {
    static SLEEPING: State<bool> = State::new(false);
    static INHIBITOR: RefCell<Option<OwnedFd>> = const { RefCell::new(None) };
    static WANTED: Cell<bool> = const { Cell::new(false) };
}

pub fn sleeping() -> State<bool> {
    SLEEPING.with(Clone::clone)
}

pub fn start() {
    let Some(bus) = dbus::system() else { return };
    bus.subscribe(
        SignalFilter { interface: Some(MANAGER.into()), member: Some("PrepareForSleep".into()), path: Some(PATH.into()), ..Default::default() },
        |m| {
            if let Ok(s) = m.read1::<bool>() {
                sleeping().set(s);
            }
        },
    );
}

pub fn inhibit_sleep(why: &str) {
    if WANTED.with(|w| w.replace(true)) {
        return;
    }
    let Some(bus) = dbus::system() else { return };
    let msg = method(NAME, PATH, MANAGER, "Inhibit").append3("sleep", "ags", why).append1("delay");
    bus.call_cb(msg, |r| match r.ok().and_then(|m| m.read1::<::dbus::arg::OwnedFd>().ok()) {
        Some(fd) => {
            let fd: OwnedFd = unsafe { std::os::fd::FromRawFd::from_raw_fd(fd.into_fd()) };
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
        bus.send(
            method(NAME, "/org/freedesktop/login1/session/auto", "org.freedesktop.login1.Session", "SetBrightness")
                .append3("backlight", device, value),
        );
    }
}
