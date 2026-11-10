// Session lock: ext-session-lock surfaces per output, PAM password + fingerprint, display power,
// sleep inhibitor.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use xkbcommon::xkb::Keysym;

use crate::lib::battery::battery;
use crate::lib::fprintd;
use crate::lib::logind::{self, inhibit_sleep, release_sleep, sleeping};
use crate::lib::pam;
use crate::lib::style::*;
use crate::lib::sway;
use crate::lib::time::{format_now, now};
use crate::lib::wallpaper;
use crate::ui::entry::{self, Entry, EntryRef};
use crate::ui::node::{Align, Kind, LabelSpec, Node, Sides, Style, alpha, rgba};
use crate::ui::runtime::{self, Timer};
use crate::ui::state::global_state;
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
    static ENTRY: EntryRef = {
        let e = entry::new("");
        e.borrow_mut().password = true;
        e.borrow_mut().invisible_char = None;
        e.borrow_mut().text.reserve(256); // typing never reallocates, leaving copies behind
        // typing clears a red/orange clock
        e.borrow_mut().on_change = Some(Rc::new(|t: &str| {
            if !t.is_empty() {
                auth_failed().set(Failed::None);
            }
        }));
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
    /// past MAX_FAILURES (the reboot was refused or is slow): no guess before this
    static BACKOFF_UNTIL: Cell<Option<Instant>> = const { Cell::new(None) };
    static DISPLAY_OFF: Cell<bool> = const { Cell::new(false) };
    static DISPLAY_OFF_ON_LOCK: Cell<bool> = const { Cell::new(false) };
    static AFTER_LOCK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    static FPRINT: RefCell<Option<Rc<Fprint>>> = const { RefCell::new(None) };
}

global_state! {
    pub fn locked() -> bool = false;
    fn auth_failed() -> Failed = Failed::None;
    /// the reader is (or is about to be again) listening: false turns the clock white
    fn fprint_ready() -> bool = true;
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

/// The marker names the session it was written in: a new login doesn't inherit it. An unknown
/// session on either side relocks: never fail open.
pub fn was_locked() -> bool {
    let Some(marker) = lock_marker().and_then(|m| std::fs::read_to_string(m).ok()) else { return false };
    marker_matches(&marker, &logind::session_id())
}

/// A marker left by this session (or one that can't tell: either id unknown) still holds.
fn marker_matches(marker: &str, session: &str) -> bool {
    marker == session || marker.is_empty() || session.is_empty()
}

fn set_marker(on: bool) {
    let Some(m) = lock_marker() else { return };
    let res = if on {
        if let Some(dir) = std::path::Path::new(&m).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        crate::lib::xdg::write_atomic(&m, logind::session_id().as_bytes(), 0o600)
    } else {
        std::fs::remove_file(&m).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) })
    };
    if let Err(e) = res {
        eprintln!("flurry: lock marker {m}: {e}");
    }
}

fn check_login() {
    // an empty submit (or Return held down after one) is no attempt
    if entry().borrow().text.is_empty() || BUSY.with(Cell::get) {
        return;
    }
    // too many wrong passwords: the reboot is requested, and guesses wait out a growing delay
    if BACKOFF_UNTIL.with(Cell::get).is_some_and(|t| Instant::now() < t) {
        entry().borrow_mut().set_text("");
        auth_failed().set(Failed::Password);
        return;
    }
    BUSY.with(|b| b.set(true));
    let attempt = ATTEMPT.with(|a| {
        a.set(a.get() + 1);
        a.get()
    });
    // replaced or taken (and cancelled) before another attempt can start
    let t = runtime::timeout(30_000, || BUSY.with(|b| b.set(false)));
    if let Some(old) = BUSY_TIMER.with(|b| b.borrow_mut().replace(t)) {
        old.cancel();
    }
    // wiped on drop once pam is done
    let password = entry().borrow_mut().take_secret();
    let generation = LOCK_GEN.with(Cell::get);
    pam::authenticate("flurry-auth", password, move |ev| {
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
            if n == MAX_FAILURES {
                logind::reboot();
            }
            if let Some(wait) = failure_backoff(n) {
                BACKOFF_UNTIL.with(|b| b.set(Some(Instant::now() + wait)));
            }
        }
        auth_failed().set(Failed::Password);
    });
}

const MAX_FAILURES: u32 = 5;

/// The wait before another guess after `n` wrong passwords, from MAX_FAILURES on (the reboot
/// refused (polkit...) or still coming): 30s, doubling per further miss.
fn failure_backoff(n: u32) -> Option<Duration> {
    (n >= MAX_FAILURES).then(|| Duration::from_secs(30) * (1 << (n - MAX_FAILURES).min(7)))
}

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
    Entry::key(&entry(), ev);
}

fn clock_style(failed: Failed, fprint_ready: bool) -> Style {
    let mut s = window();
    s.font = font(IOSEVKA, 192.0);
    if fprint_ready {
        s.color = alpha(blue1(), 0.9);
        s.text_shadow = Some(glow(32.0, alpha(blue2(), 0.8)));
    } else {
        s.color = alpha(light1(), 0.9);
        s.text_shadow = Some(glow(32.0, alpha(light3(), 0.6)));
    }
    match failed {
        Failed::Password => {
            s.color = red();
            s.text_shadow = Some(glow(16.0, red()));
        }
        Failed::Fprint => {
            s.color = orange();
            s.text_shadow = Some(glow(16.0, orange()));
        }
        Failed::None => {}
    }
    s = s.border(Sides::all(BORDER), blue3());
    s.padding = Sides::trbl(30.0, 31.0, 20.0, 26.0);
    s.background = Some(alpha(dark1(), 0.45));
    s
}

fn build() -> Node {
    // wallpaper and the dark gradient (the hidden password entry) are the surface's backdrop
    let base = window();
    let mut clock = Node::label(clock_style(auth_failed().peek(), fprint_ready().peek()), format_now("%H\n%M")).align(Align::Center, Align::Center);
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
            logind::set_locked_hint(true);
            // whatever was typed before the lock came up is no attempt
            entry().borrow_mut().set_text("");
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
                if f.mismatches.replace(0) >= MAX_FAILURES {
                    fprint_ready().set(true);
                }
                f.backoff.set(FPRINT_BACKOFF_MIN);
                f.sync();
            }
        }
        // refused (another locker) or ended by the compositor: never leave a half-locked state behind
        LockEvent::Finished => end_lock(),
    }
}

/// Everything a lock held, undone: shared by unlocking and a lock the compositor ended.
fn end_lock() {
    SURFACES.with(|l| l.borrow_mut().clear());
    set_marker(false);
    locked().set(false);
    FAILURES.with(|f| f.set(0));
    BACKOFF_UNTIL.with(|b| b.set(None));
    BUSY.with(|b| b.set(false));
    if let Some(t) = BUSY_TIMER.with(|b| b.borrow_mut().take()) {
        t.cancel();
    }
    entry().borrow_mut().set_text("");
    // unconditional: a shell restarted while the display was off doesn't know it is
    DISPLAY_OFF.with(|d| d.set(false));
    sway::command("output * power on");
    if let Some(f) = FPRINT.with(|f| f.borrow().clone()) {
        f.sync();
    }
    auth_failed().set(Failed::None);
    if let Some(w) = window::get("focusMenu") {
        w.set_keymode(Keymode::Exclusive);
    }
    DISPLAY_OFF_ON_LOCK.with(|d| d.set(false));
    AFTER_LOCK.with(|a| a.borrow_mut().take());
    if sleeping().peek() {
        release_sleep();
    }
    logind::set_locked_hint(false);
}

pub fn lock_screen() {
    // marked before locking: a crash between the two still relocks on restart
    let fresh = !wayland::is_locking();
    if fresh {
        // a marker left by a crash: the display may have been off with nothing to turn it on
        if was_locked() {
            sway::command("output * power on");
        }
        set_marker(true);
    }
    if !wayland::lock(on_lock_event) {
        if fresh && !wayland::is_locking() {
            set_marker(false); // refused
            crate::lib::notifySend::notify_send("Lock", "The screen could not be locked", "", Vec::new());
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
    end_lock();
}

fn prepare_unlock() {
    // the focus menu is what unlocking uncovers, already up and redrawn for the new wallpaper
    if let Some(w) = window::get("focusMenu") {
        w.set_keymode(Keymode::None);
        w.show();
    }
    wallpaper::random_wallpaper();
}

const FPRINT_BACKOFF_MIN: u64 = 250;
const FPRINT_BACKOFF_MAX: u64 = 60_000;

fn next_fprint_backoff(ms: u64) -> u64 {
    (ms * 2).min(FPRINT_BACKOFF_MAX)
}

struct Fprint {
    reader: Rc<fprintd::Verifier>,
    /// verifies that ended on an unenrolled finger, this lock
    mismatches: Cell<u32>,
    flash: RefCell<Option<Timer>>,
    /// next delay after a failure (no reader, device busy or resuming, fprintd gone...)
    backoff: Cell<u64>,
    wait: RefCell<Option<Timer>>,
}

impl Fprint {
    /// verifying while locked and awake (a suspend cancels the verify), up to MAX_FAILURES mismatches
    fn wanted(&self) -> bool {
        is_locked() && !sleeping().peek() && self.mismatches.get() < MAX_FAILURES
    }

    fn stop_waiting(&self) {
        if let Some(t) = self.wait.borrow_mut().take() {
            t.cancel();
        }
    }

    /// verifying from now if wanted, else the reader released
    fn sync(&self) {
        self.stop_waiting();
        if self.wanted() { self.reader.start() } else { self.reader.stop() }
    }

    fn event(self: &Rc<Self>, ev: fprintd::Event) {
        if !self.wanted() {
            return;
        }
        match ev {
            fprintd::Event::Listening => {
                self.backoff.set(FPRINT_BACKOFF_MIN);
                fprint_ready().set(true);
            }
            fprintd::Event::BadScan => self.flash(),
            fprintd::Event::NoMatch => {
                self.flash();
                self.mismatches.set(self.mismatches.get() + 1);
                if self.wanted() {
                    self.reader.again();
                } else {
                    self.reader.stop();
                    fprint_ready().set(false);
                }
            }
            fprintd::Event::Match => unlock_screen(),
            fprintd::Event::Failed => self.schedule(),
        }
    }

    /// back off instead of spinning: 250ms doubling to 60s. Only a repeated failure turns the
    /// clock white: a single one (a reader still resuming) is retried before it would show.
    fn schedule(self: &Rc<Self>) {
        let ms = self.backoff.get();
        if ms > FPRINT_BACKOFF_MIN {
            fprint_ready().set(false);
        }
        self.backoff.set(next_fprint_backoff(ms));
        let me = self.clone();
        let t = runtime::timeout(ms, move || {
            me.wait.borrow_mut().take();
            me.sync();
        });
        if let Some(old) = self.wait.borrow_mut().replace(t) {
            old.cancel();
        }
    }

    /// activity (a key, the display coming on): a waiting retry comes back at the shortest delay
    fn reset_backoff(self: &Rc<Self>) {
        if self.wait.borrow().is_some() {
            self.backoff.set(FPRINT_BACKOFF_MIN);
            self.schedule();
        }
    }

    fn flash(&self) {
        // a wrong password's red stays until typing clears it
        if !is_locked() || sleeping().peek() || auth_failed().peek() == Failed::Password {
            return;
        }
        entry().borrow_mut().set_text("");
        if let Some(t) = self.flash.borrow_mut().take() {
            t.cancel();
        }
        auth_failed().set(Failed::Fprint);
        let t = runtime::timeout(400, || {
            if auth_failed().peek() == Failed::Fprint {
                auth_failed().set(Failed::None);
            }
        });
        *self.flash.borrow_mut() = Some(t);
    }
}

pub fn start() {
    // without it every password fails: say so now, not at the first unlock
    if !std::path::Path::new("/etc/pam.d/flurry-auth").exists() {
        eprintln!("flurry: /etc/pam.d/flurry-auth is missing: unlocking needs security.pam.services.flurry-auth");
        crate::lib::notifySend::notify_send("Lock", "PAM service flurry-auth is missing", "Unlocking will fail: set security.pam.services.flurry-auth", Vec::new());
    }
    logind::on_lock(lock_screen);
    fprintd::available(|yes| {
        if !yes {
            return;
        }
        let reader = fprintd::Verifier::new(|ev| {
            if let Some(f) = FPRINT.with(|f| f.borrow().clone()) {
                f.event(ev);
            }
        });
        let Some(reader) = reader else { return };
        let f = Rc::new(Fprint {
            reader,
            mismatches: Cell::new(0),
            flash: RefCell::default(),
            backoff: Cell::new(FPRINT_BACKOFF_MIN),
            wait: RefCell::default(),
        });
        FPRINT.with(|s| *s.borrow_mut() = Some(f.clone()));
        // locked before fprintd answered (a restarted shell relocking)
        f.sync();
    });
    auth_failed().subscribe(redraw);
    fprint_ready().subscribe(redraw);
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
        // released before the suspend cancels the verify, verifying again on wake
        if let Some(f) = FPRINT.with(|f| f.borrow().clone()) {
            f.sync();
        }
        if !sleeping().peek() {
            inhibit_sleep(REASON);
        } else if is_locked() {
            release_sleep();
        } else {
            lock_screen();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_holds_for_this_session_or_unknown_ids() {
        assert!(marker_matches("3", "3"));
        assert!(marker_matches("", "3"));
        assert!(marker_matches("3", ""));
        assert!(!marker_matches("2", "3"));
    }

    #[test]
    fn failure_backoff_starts_at_max_and_doubles_capped() {
        assert_eq!(failure_backoff(MAX_FAILURES - 1), None);
        assert_eq!(failure_backoff(MAX_FAILURES), Some(Duration::from_secs(30)));
        assert_eq!(failure_backoff(MAX_FAILURES + 1), Some(Duration::from_secs(60)));
        assert_eq!(failure_backoff(MAX_FAILURES + 7), Some(Duration::from_secs(30 << 7)));
        assert_eq!(failure_backoff(MAX_FAILURES + 100), Some(Duration::from_secs(30 << 7)));
    }

    #[test]
    fn fprint_backoff_doubles_to_max() {
        let mut ms = FPRINT_BACKOFF_MIN;
        let mut seen = vec![];
        for _ in 0..10 {
            seen.push(ms);
            ms = next_fprint_backoff(ms);
        }
        assert_eq!(seen, [250, 500, 1000, 2000, 4000, 8000, 16000, 32000, 60000, 60000]);
    }
}
