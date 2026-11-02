// Session lock (widgets/lockscreen/lockscreen.tsx): ext-session-lock surfaces per output, PAM
// password + fingerprint, display power, sleep inhibitor.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::battery::battery;
use crate::lib::logind::{self, inhibit_sleep, release_sleep, sleeping};
use crate::lib::pam;
use crate::lib::state::State;
use crate::lib::style::*;
use crate::lib::sway;
use crate::lib::time::{format_now, now};
use crate::lib::wallpaper;
use crate::ui::entry::{self, Entry, EntryRef};
use crate::ui::node::{Align, Background, Kind, LabelSpec, Node, Sides, Style, alpha, hash, rgba};
use crate::ui::runtime::{self, Timer};
use crate::ui::wayland::{self, KeyEvent, Keymode, LockEvent, Surface};
use wayland_client::protocol::wl_output::WlOutput;
use crate::ui::window;
use crate::widgets::status::modules::statusIcons::battery_percent;

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
        e.borrow_mut().text.reserve(256); // typing never reallocates, leaving copies behind
        e
    };
    static SURFACES: RefCell<Vec<(WlOutput, Surface)>> = const { RefCell::new(Vec::new()) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
    /// a hung PAM stack must not block password entry forever
    static BUSY_TIMER: RefCell<Option<Timer>> = const { RefCell::new(None) };
    static ATTEMPT: Cell<u64> = const { Cell::new(0) };
    /// Bumped on every Locked: a password check only counts for the lock it started in.
    static LOCK_GEN: Cell<u64> = const { Cell::new(0) };
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
    if on {
        fprint_kick();
    }
}

fn fprint_kick() {
    if let Some(f) = FPRINT.with(|f| f.borrow().clone()) {
        f.reset_backoff();
    }
}

fn is_locked() -> bool {
    wayland::is_locking() && locked().peek()
}

/// Marks a held lock in the runtime dir, so a restarted shell locks again.
fn lock_marker() -> Option<String> {
    crate::lib::xdg::runtime_dir().map(|d| format!("{d}/flurry/locked"))
}

/// The marker names the session it was written in: a new login doesn't inherit it.
fn session_id() -> String {
    std::env::var("XDG_SESSION_ID").unwrap_or_default()
}

pub fn was_locked() -> bool {
    lock_marker().and_then(|m| std::fs::read_to_string(m).ok()).is_some_and(|s| s == session_id())
}

fn set_marker(on: bool) {
    let Some(m) = lock_marker() else { return };
    if on {
        if let Some(dir) = std::path::Path::new(&m).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&m, session_id());
    } else {
        let _ = std::fs::remove_file(&m);
    }
}

fn check_login() {
    // an empty submit (or Return held down after one) is no attempt
    if entry().borrow().text.is_empty() || BUSY.with(Cell::get) {
        return;
    }
    // too many wrong passwords: the reboot is requested, and no more guesses for this lock
    if FAILURES.with(Cell::get) >= MAX_FAILURES {
        entry().borrow_mut().set_text("");
        auth_failed().set(Failed::Password);
        return;
    }
    BUSY.with(|b| b.set(true));
    let attempt = ATTEMPT.with(|a| {
        a.set(a.get() + 1);
        a.get()
    });
    let t = runtime::timeout(30_000, move || {
        if ATTEMPT.with(Cell::get) == attempt {
            BUSY.with(|b| b.set(false));
        }
    });
    if let Some(old) = BUSY_TIMER.with(|b| b.borrow_mut().replace(t)) {
        old.cancel();
    }
    // moved, not copied: pam wipes it when done
    let password = std::mem::replace(&mut entry().borrow_mut().text, String::with_capacity(256));
    entry().borrow_mut().set_text("");
    let generation = LOCK_GEN.with(Cell::get);
    pam::authenticate("flurry-auth", Some(password), move |ev| {
        let pam::Event::Done(code) = ev else { return };
        if ATTEMPT.with(Cell::get) == attempt {
            BUSY.with(|b| b.set(false));
            if let Some(t) = BUSY_TIMER.with(|b| b.borrow_mut().take()) {
                t.cancel();
            }
        }
        if generation != LOCK_GEN.with(Cell::get) || !is_locked() {
            return;
        }
        if code == 0 {
            unlock_screen();
            return;
        }
        // only a wrong password counts, not a broken PAM stack
        if matches!(code, pam::PAM_AUTH_ERR | pam::PAM_MAXTRIES) {
            let n = FAILURES.with(|f| {
                f.set(f.get() + 1);
                f.get()
            });
            if n >= MAX_FAILURES {
                // refused (polkit...): no more guesses either way, the clock stays red
                logind::reboot_cb(|ok| {
                    if !ok && is_locked() {
                        auth_failed().set(Failed::Password);
                    }
                });
            }
        }
        auth_failed().set(Failed::Password);
    });
}

const MAX_FAILURES: u32 = 5;

fn handle_keys(ev: &KeyEvent) {
    fprint_kick();
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
    let before = hash(&entry().borrow().text);
    Entry::key(&entry(), ev);
    if hash(&entry().borrow().text) != before {
        auth_failed().set(Failed::None);
    }
}

fn clock_style(failed: Failed) -> Style {
    let mut s = window();
    s.font = font(IOSEVKA, 192.0);
    s.color = alpha(blue1(), 0.9);
    s.text_shadow = vec![glow(32.0, alpha(blue2(), 0.8))];
    match failed {
        Failed::Password => {
            s.color = red();
            s.text_shadow = vec![glow(16.0, red())];
        }
        Failed::Fprint => {
            s.color = orange();
            s.text_shadow = vec![glow(16.0, orange())];
        }
        Failed::None => {}
    }
    s = s.border(Sides::all(BORDER), blue3());
    s.padding = Sides::trbl(30.0, 31.0, 20.0, 26.0);
    s.background = Background::Color(alpha(dark1(), 0.45));
    s
}

fn build(_w: f32, _h: f32) -> Node {
    // wallpaper and the dark gradient (the hidden password entry) are the surface's backdrop
    let base = window();
    let mut clock = Node::label(clock_style(auth_failed().peek()), format_now("%H\n%M")).align(Align::Center, Align::Center);
    if let Kind::Label(LabelSpec { line_height, .. }) = &mut clock.kind {
        *line_height = Some(0.75);
    }
    let mut bat = battery_percent(&base).align(Align::Start, Align::End);
    bat.style.margin = Sides::all(SPACE_L);
    Node::overlay(base.inherit(), vec![clock, bat])
}

fn assign_lock_window(output: &wayland::Output) {
    let image = wallpaper::current().peek().map(|w| w.shm);
    let gradient = (rgba(0, 0, 0, 0.3), rgba(0, 0, 0, 0.5));
    let Some(s) = wayland::lock_surface(&output.wl, Rc::new(build), image, gradient) else { return };
    s.on_key(handle_keys);
    SURFACES.with(|l| l.borrow_mut().push((output.wl.clone(), s)));
}

fn redraw() {
    SURFACES.with(|l| l.borrow().iter().for_each(|(_, s)| s.redraw()));
}

fn on_lock_event(ev: LockEvent) {
    match ev {
        LockEvent::Locked => {
            LOCK_GEN.with(|g| g.set(g.get() + 1));
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
                f.stop_waiting();
                f.backoff.set(FPRINT_BACKOFF_MIN);
                f.retry();
            }
        }
        LockEvent::Finished => {
            // refused (another locker) or ended: never leave a half-locked state behind
            SURFACES.with(|l| l.borrow_mut().clear());
            set_marker(false);
            if locked().peek() {
                locked().set(false);
            }
            if let Some(w) = window::get("focusMenu") {
                w.set_keymode(Keymode::Exclusive);
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
    // marked before locking: a crash between the two still relocks on restart
    let fresh = !wayland::is_locking();
    if fresh {
        set_marker(true);
    }
    if !wayland::lock(on_lock_event) {
        if fresh && !wayland::is_locking() {
            set_marker(false); // refused
        }
        // refused, or already locked: a pending action/display-off must not fire on a later lock.
        // Still locking: Locked will run them.
        if !wayland::is_locking() || locked().peek() {
            DISPLAY_OFF_ON_LOCK.with(|d| d.set(false));
            AFTER_LOCK.with(|a| a.borrow_mut().take());
        }
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
    set_marker(false);
    if DISPLAY_OFF.with(Cell::get) {
        set_display(true);
    }
    if let Some(f) = FPRINT.with(|f| f.borrow().clone()) {
        f.stop_waiting();
    }
    FAILURES.with(|f| f.set(0));
    SURFACES.with(|l| l.borrow_mut().clear());
    locked().set(false);
    auth_failed().set(Failed::None);
    if let Some(w) = window::get("focusMenu") {
        w.set_keymode(Keymode::Exclusive);
    }
}

fn prepare_unlock() {
    // the focus menu is what unlocking uncovers, already up and redrawn for the new wallpaper
    if let Some(w) = window::get("focusMenu") {
        w.set_keymode(Keymode::None);
        w.show();
    }
    wallpaper::random_wallpaper();
}

const FPRINT_BACKOFF_MIN: u64 = 2000;
const FPRINT_BACKOFF_MAX: u64 = 60_000;
/// an attempt that failed sooner than this never reached the reader (none, busy): back off;
/// a longer one is pam_fprintd's idle verify timeout and retries at once
const FPRINT_QUICK_FAIL: std::time::Duration = std::time::Duration::from_secs(5);

struct Fprint {
    /// attempts that ended on a mismatch (PAM_AUTH_ERR/PAM_MAXTRIES)
    mismatches: Cell<u32>,
    running: Cell<bool>,
    /// when the running attempt started
    started: Cell<Option<std::time::Instant>>,
    flash: RefCell<Option<Timer>>,
    /// next delay after a non-mismatch failure (no reader, device busy, ...)
    backoff: Cell<u64>,
    wait: RefCell<Option<Timer>>,
    /// a backoff retry came due while the display was off: run it once it's back on
    paused: Cell<bool>,
}

impl Fprint {
    fn allowed(&self) -> bool {
        is_locked() && self.mismatches.get() < MAX_FAILURES
    }

    fn stop_waiting(&self) {
        self.paused.set(false);
        if let Some(t) = self.wait.borrow_mut().take() {
            t.cancel();
        }
    }

    /// back off instead of spinning: 2s doubling to 60s, paused while the display is off
    fn schedule(self: &Rc<Self>) {
        let ms = self.backoff.get();
        self.backoff.set((ms * 2).min(FPRINT_BACKOFF_MAX));
        let me = self.clone();
        let t = runtime::timeout(ms, move || {
            me.wait.borrow_mut().take();
            if DISPLAY_OFF.with(Cell::get) {
                me.paused.set(true);
            } else {
                me.retry();
            }
        });
        if let Some(old) = self.wait.borrow_mut().replace(t) {
            old.cancel();
        }
    }

    /// activity (a key, the display coming on): a waiting retry comes back at the shortest delay
    fn reset_backoff(self: &Rc<Self>) {
        self.backoff.set(FPRINT_BACKOFF_MIN);
        let waiting = self.wait.borrow().is_some();
        if self.paused.replace(false) || waiting {
            self.schedule();
        }
    }

    fn retry(self: &Rc<Self>) {
        if self.running.get() || !self.allowed() {
            return;
        }
        self.running.set(true);
        self.started.set(Some(std::time::Instant::now()));
        let me = self.clone();
        pam::authenticate("flurry-fprint", None, move |ev| match ev {
            pam::Event::Info => {}
            pam::Event::Error => {
                entry().borrow_mut().set_text("");
                if let Some(t) = me.flash.borrow_mut().take() {
                    t.cancel();
                }
                auth_failed().set(Failed::Fprint);
                let t = runtime::timeout(400, || {
                    if auth_failed().peek() == Failed::Fprint {
                        auth_failed().set(Failed::None);
                    }
                });
                *me.flash.borrow_mut() = Some(t);
            }
            pam::Event::Done(code) => {
                me.running.set(false);
                if code == 0 {
                    if me.allowed() {
                        unlock_screen();
                    }
                } else if matches!(code, pam::PAM_AUTH_ERR | pam::PAM_MAXTRIES) {
                    me.mismatches.set(me.mismatches.get() + 1);
                    if me.allowed() {
                        me.retry();
                    } else if is_locked() {
                        auth_failed().set(Failed::Password);
                    }
                } else if is_locked() {
                    if me.started.take().is_none_or(|t| t.elapsed() < FPRINT_QUICK_FAIL) {
                        me.schedule();
                    } else {
                        me.backoff.set(FPRINT_BACKOFF_MIN);
                        if DISPLAY_OFF.with(Cell::get) {
                            me.paused.set(true);
                        } else {
                            me.retry();
                        }
                    }
                }
            }
        });
    }
}

pub fn start() {
    if std::path::Path::new("/etc/pam.d/flurry-fprint").exists() {
        FPRINT.with(|f| *f.borrow_mut() = Some(Rc::new(Fprint {
            mismatches: Cell::new(0),
            running: Cell::new(false),
            started: Cell::new(None),
            flash: RefCell::default(),
            backoff: Cell::new(FPRINT_BACKOFF_MIN),
            wait: RefCell::default(),
            paused: Cell::new(false),
        })));
    }
    auth_failed().subscribe(redraw);
    now().subscribe(|| {
        if locked().peek() {
            redraw();
        }
    });
    battery().subscribe(|| {
        if locked().peek() {
            redraw();
        }
    });
    // new outputs while locked get a lock surface too
    wayland::on_outputs(|| {
        if !wayland::is_locking() {
            return;
        }
        // removed outputs' surfaces are dead: drop them, so a replugged output gets a fresh one
        SURFACES.with(|l| l.borrow_mut().retain(|(_, s)| s.alive()));
        for o in wayland::outputs() {
            if !SURFACES.with(|l| l.borrow().iter().any(|(w, _)| *w == o.wl)) {
                assign_lock_window(&o);
            }
        }
    });

    const REASON: &str = "Lock screen before sleep";
    inhibit_sleep(REASON);
    // every PrepareForSleep(true) notifies, even a repeated one: lock now, or let sleep go if locked
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
