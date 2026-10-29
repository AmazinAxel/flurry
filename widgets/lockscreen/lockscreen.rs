// Session lock (widgets/lockscreen/lockscreen.tsx): ext-session-lock surfaces per output, PAM
// password + fingerprint, display power, sleep inhibitor.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::logind::{self, inhibit_sleep, release_sleep, sleeping};
use crate::lib::pam;
use crate::lib::state::State;
use crate::lib::style::*;
use crate::lib::sway;
use crate::lib::time::{format_now, now};
use crate::lib::wallpaper;
use crate::ui::entry::{self, Entry, EntryRef};
use crate::ui::node::{Align, Background, Kind, LabelSpec, Node, Shadow, Sides, Style, alpha, rgba, shadow};
use crate::ui::runtime::{self, Timer};
use crate::ui::wayland::{self, KeyEvent, Keymode, LockEvent, Surface};
use crate::ui::window;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Failed {
    None,
    Password,
    Fprint,
}

thread_local! {
    static LOCKED: State<bool> = State::new(false);
    static AUTH_FAILED: State<Failed> = State::new(Failed::None);
    static ENTRY: EntryRef = {
        let e = entry::new("");
        e.borrow_mut().password = true;
        e.borrow_mut().invisible_char = None;
        e
    };
    static SURFACES: RefCell<Vec<(String, Surface)>> = const { RefCell::new(Vec::new()) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
    static FAILURES: Cell<u32> = const { Cell::new(0) };
    static DISPLAY_OFF: Cell<bool> = const { Cell::new(false) };
    static DISPLAY_OFF_ON_LOCK: Cell<bool> = const { Cell::new(false) };
    static AFTER_LOCK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    static FPRINT: RefCell<Option<Rc<Fprint>>> = const { RefCell::new(None) };
}

pub fn locked() -> State<bool> {
    LOCKED.with(Clone::clone)
}
fn auth_failed() -> State<Failed> {
    AUTH_FAILED.with(Clone::clone)
}
fn entry() -> EntryRef {
    ENTRY.with(Clone::clone)
}

fn set_display(on: bool) {
    DISPLAY_OFF.with(|d| d.set(!on));
    sway::command(&format!("output * power {}", if on { "on" } else { "off" }));
}

fn is_locked() -> bool {
    wayland::is_locking() && locked().peek()
}

fn check_login() {
    if BUSY.with(|b| b.replace(true)) {
        return;
    }
    let password = entry().borrow().text.clone();
    entry().borrow_mut().set_text("");
    pam::authenticate("flurry-auth", Some(password), |ev| {
        let pam::Event::Done(ok) = ev else { return };
        if ok {
            unlock_screen();
            FAILURES.with(|f| f.set(0));
        } else {
            let n = FAILURES.with(|f| {
                f.set(f.get() + 1);
                f.get()
            });
            if n >= 5 {
                logind::reboot();
            } else {
                auth_failed().set(Failed::Password);
            }
        }
        BUSY.with(|b| b.set(false));
    });
}

fn handle_keys(ev: &KeyEvent) {
    if DISPLAY_OFF.with(Cell::get) {
        set_display(true);
        return;
    }
    if ev.sym == Keysym::Insert {
        return;
    }
    if ev.ctrl {
        let lower = ev.lower();
        if lower == Keysym::v {
            return; // no paste from clipboard!
        }
        let action: Option<fn()> = match lower {
            Keysym::s => Some(logind::suspend),
            Keysym::h => Some(logind::hibernate),
            Keysym::q => Some(logind::poweroff),
            Keysym::d => Some(|| set_display(false)),
            Keysym::c => Some(|| {}), // clear input
            _ => None,
        };
        if let Some(a) = action {
            a();
            entry().borrow_mut().set_text(""); // wipe anything typed
            return;
        }
    }
    if matches!(ev.sym, Keysym::Return | Keysym::KP_Enter) {
        check_login();
        return;
    }
    let before = entry().borrow().text.clone();
    Entry::key(&entry(), ev);
    if entry().borrow().text != before {
        auth_failed().set(Failed::None);
    }
}

fn clock_style(failed: Failed) -> Style {
    let mut s = window();
    s.font = font(WQY, 192.0);
    s.color = alpha(blue1(), 0.9);
    s.text_shadow = vec![shadow(0.0, 2.0, 6.0, alpha(dark1(), 0.6)), glow(32.0, alpha(blue2(), 0.8))];
    match failed {
        Failed::Password => {
            s.color = red();
            s.text_shadow = vec![shadow(0.0, 2.0, 6.0, alpha(dark1(), 0.6)), glow(16.0, red())];
        }
        Failed::Fprint => {
            s.color = orange();
            s.text_shadow = vec![shadow(0.0, 2.0, 6.0, alpha(dark1(), 0.6)), glow(16.0, orange())];
        }
        Failed::None => {}
    }
    s = s.border(Sides::all(3.0), blue3());
    s.padding = Sides::trbl(30.0, 31.0, 20.0, 26.0);
    s.background = Background::Color(alpha(dark1(), 0.45));
    s.box_shadow = vec![Shadow { ..shadow(0.0, 0.0, 48.0, alpha(dark1(), 0.35)) }];
    s
}

fn build(_w: f32, _h: f32) -> Node {
    // wallpaper and the dark gradient (the hidden password entry) are the surface's backdrop
    let base = window();
    let mut clock = Node::label(clock_style(auth_failed().peek()), format_now("%H\n%M")).align(Align::Center, Align::Center);
    if let Kind::Label(LabelSpec { line_height, .. }) = &mut clock.kind {
        *line_height = Some(0.75);
    }
    Node::overlay(base.inherit(), vec![clock])
}

fn assign_lock_window(output: &wayland::Output) {
    let image = wallpaper::current().peek().map(|w| w.shm);
    let gradient = (rgba(0, 0, 0, 0.3), rgba(0, 0, 0, 0.5));
    let Some(s) = wayland::lock_surface(&output.wl, Rc::new(build), image, gradient) else { return };
    s.on_key(handle_keys);
    SURFACES.with(|l| l.borrow_mut().push((output.name.clone(), s)));
}

fn redraw() {
    SURFACES.with(|l| l.borrow().iter().for_each(|(_, s)| s.redraw()));
}

fn on_lock_event(ev: LockEvent) {
    match ev {
        LockEvent::Locked => {
            locked().set(true);
            if sleeping().peek() {
                release_sleep();
            }
            if DISPLAY_OFF_ON_LOCK.with(|d| d.replace(false)) {
                set_display(false);
            }
            window::set_visible("powermenu", false);
            if let Some(f) = AFTER_LOCK.with(|a| a.borrow_mut().take()) {
                f();
            }
            prepare_unlock();
            // move cursor up a pixel so that it updates and disappears
            sway::command("seat - cursor move 1 1, seat - cursor move -1 -1");
            if let Some(f) = FPRINT.with(|f| f.borrow().clone()) {
                f.mismatches.set(0);
                f.retry();
            }
        }
        LockEvent::Finished => {
            // refused (another locker) or ended: never leave a half-locked state behind
            SURFACES.with(|l| l.borrow_mut().clear());
            if locked().peek() {
                locked().set(false);
            }
            DISPLAY_OFF_ON_LOCK.with(|d| d.set(false));
            AFTER_LOCK.with(|a| a.borrow_mut().take());
            if sleeping().peek() {
                release_sleep();
            }
        }
    }
}

pub fn lock_screen() {
    if !wayland::lock(on_lock_event) {
        return;
    }
    for o in wayland::outputs() {
        assign_lock_window(&o);
    }
}

pub fn lock_display_off() {
    DISPLAY_OFF_ON_LOCK.with(|d| d.set(true));
    lock_screen();
}

pub fn lock_then(action: Box<dyn FnOnce()>) {
    AFTER_LOCK.with(|a| *a.borrow_mut() = Some(action));
    lock_screen();
}

fn unlock_screen() {
    wayland::unlock();
    SURFACES.with(|l| l.borrow_mut().clear());
    locked().set(false);
    auth_failed().set(Failed::None);
    if let Some(w) = window::get("focusMenu") {
        w.set_keymode(Keymode::Exclusive);
    }
}

fn prepare_unlock() {
    let win = window::get("focusMenu");
    if let Some(w) = &win {
        w.set_keymode(Keymode::None);
    }
    wallpaper::random_wallpaper();
    let Some(w) = win else { return };
    if !is_locked() {
        w.set_keymode(Keymode::Exclusive);
    }
    w.remap();
    if let Some(wp) = window::get("wallpaper") {
        wp.remap();
    }
}

struct Fprint {
    mismatches: Cell<u32>,
    running: Cell<bool>,
    flash: RefCell<Option<Timer>>,
}

impl Fprint {
    fn allowed(&self) -> bool {
        is_locked() && self.mismatches.get() < 5
    }

    fn retry(self: &Rc<Self>) {
        if self.running.get() || !self.allowed() {
            return;
        }
        self.running.set(true);
        let me = self.clone();
        pam::authenticate("flurry-fprint", None, move |ev| match ev {
            pam::Event::Info => {}
            pam::Event::Error(msg) => {
                if msg.contains("Failed to match") {
                    me.mismatches.set(me.mismatches.get() + 1);
                }
                entry().borrow_mut().set_text("");
                if let Some(t) = me.flash.borrow_mut().take() {
                    t.cancel();
                }
                if me.mismatches.get() >= 5 {
                    auth_failed().set(Failed::Password);
                } else {
                    auth_failed().set(Failed::Fprint);
                    let t = runtime::timeout(400, || {
                        if auth_failed().peek() == Failed::Fprint {
                            auth_failed().set(Failed::None);
                        }
                    });
                    *me.flash.borrow_mut() = Some(t);
                }
            }
            pam::Event::Done(ok) => {
                me.running.set(false);
                if ok {
                    if me.allowed() {
                        unlock_screen();
                    }
                } else {
                    me.retry();
                }
            }
        });
    }
}

pub fn start() {
    if std::path::Path::new("/etc/pam.d/flurry-fprint").exists() {
        FPRINT.with(|f| *f.borrow_mut() = Some(Rc::new(Fprint { mismatches: Cell::new(0), running: Cell::new(false), flash: RefCell::default() })));
    }
    auth_failed().subscribe(redraw);
    now().subscribe(|| {
        if locked().peek() {
            redraw();
        }
    });
    // new outputs while locked get a lock surface too
    wayland::on_outputs(|| {
        if !wayland::is_locking() {
            return;
        }
        for o in wayland::outputs() {
            if !SURFACES.with(|l| l.borrow().iter().any(|(n, _)| *n == o.name)) {
                assign_lock_window(&o);
            }
        }
    });

    const REASON: &str = "Lock screen before sleep";
    inhibit_sleep(REASON);
    sleeping().subscribe(|| {
        auth_failed().set(Failed::None); // never sleep or wake on a stale red/orange clock
        if !sleeping().peek() {
            inhibit_sleep(REASON);
        } else if is_locked() {
            release_sleep();
        } else {
            lock_screen();
        }
    });
}
