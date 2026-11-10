// fprintd over D-Bus: claim the default reader and verify any enrolled finger. Not pam_fprintd:
// it reports a suspend's cancelled verify and a failed VerifyStart as mismatches, and restarts
// the reader on every timeout.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::lib::dbus::{self, Bus, Message, SignalFilter, method};

const NAME: &str = "net.reactivated.Fprint";
const MANAGER_PATH: &str = "/net/reactivated/Fprint/Manager";
const MANAGER: &str = "net.reactivated.Fprint.Manager";
const DEVICE: &str = "net.reactivated.Fprint.Device";

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Event {
    /// verifying: a finger on the reader now gets matched
    Listening,
    /// a bad scan (too short, off-centre...): the verify goes on
    BadScan,
    /// the verify ended on a finger that isn't enrolled: `again` verifies once more
    NoMatch,
    Match,
    /// no reader, no enrolled finger, fprintd erroring or gone: the reader is released
    Failed,
}

/// `cb(true)` when fprintd is installed (bus-activatable).
pub fn available(cb: impl FnOnce(bool) + 'static) {
    let Some(bus) = dbus::system() else { return cb(false) };
    let msg = method("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListActivatableNames");
    bus.call_cb(msg, move |r| cb(r.ok().and_then(|m| m.read1::<Vec<String>>().ok()).is_some_and(|n| n.iter().any(|n| n == NAME))));
}

pub struct Verifier {
    bus: Rc<Bus>,
    on_event: Box<dyn Fn(Event)>,
    /// bumped by every stop: replies and signals of an older run count for nothing
    epoch: Cell<u64>,
    /// started and not stopped
    active: Cell<bool>,
    /// Claims and Releases in flight: a Claim right behind a Release can find the device still
    /// held (Release iterates fprintd's main loop), so the next run waits for them
    busy: Cell<u32>,
    /// the claimed device
    device: RefCell<Option<String>>,
    /// VerifyStart answered and no final status since
    verifying: Cell<bool>,
}

impl Verifier {
    pub fn new(on_event: impl Fn(Event) + 'static) -> Option<Rc<Self>> {
        let bus = dbus::system()?;
        let v = Rc::new(Verifier {
            bus: bus.clone(),
            on_event: Box::new(on_event),
            epoch: Cell::new(0),
            active: Cell::new(false),
            busy: Cell::new(0),
            device: RefCell::default(),
            verifying: Cell::new(false),
        });
        let w = Rc::downgrade(&v);
        // the sender is checked against fprintd's owner: no other client can fake a match
        let filter = SignalFilter { sender: Some(NAME.into()), interface: Some(DEVICE.into()), member: Some("VerifyStatus".into()), ..Default::default() };
        bus.subscribe(filter, move |m| {
            if let Some(v) = w.upgrade() {
                v.status(m);
            }
        });
        let w = Rc::downgrade(&v);
        bus.watch_name(NAME, || {}, move || {
            // a restarted fprintd holds no claim of ours
            let Some(v) = w.upgrade() else { return };
            let claimed = v.device.borrow().is_some();
            if claimed {
                v.fail();
            }
        });
        Some(v)
    }

    fn status(self: &Rc<Self>, m: &Message) {
        let path = m.path().map(|p| p.to_string());
        if !self.verifying.get() || path.is_none() || *self.device.borrow() != path {
            return;
        }
        let Ok((result, done)) = m.read2::<&str, bool>() else { return };
        if done {
            self.verifying.set(false);
        }
        match result {
            "verify-match" => (self.on_event)(Event::Match),
            "verify-no-match" => (self.on_event)(Event::NoMatch),
            _ if !done => (self.on_event)(Event::BadScan),
            _ => self.fail(),
        }
    }

    fn fail(self: &Rc<Self>) {
        self.stop();
        (self.on_event)(Event::Failed);
    }

    /// Claims the default reader and verifies: Listening, or Failed. A no-op while running.
    pub fn start(self: &Rc<Self>) {
        if !self.active.replace(true) {
            self.resume();
        }
    }

    fn resume(self: &Rc<Self>) {
        if !self.active.get() || self.busy.get() > 0 || self.device.borrow().is_some() {
            return;
        }
        self.busy.set(1);
        let epoch = self.epoch.get();
        let me = self.clone();
        self.bus.call_cb(method(NAME, MANAGER_PATH, MANAGER, "GetDefaultDevice"), move |r| {
            let path = r.ok().and_then(|m| m.read1::<dbus::Path>().ok()).map(|p| p.to_string());
            let (Some(path), true) = (path, me.epoch.get() == epoch) else {
                me.busy.set(me.busy.get() - 1);
                return if me.epoch.get() == epoch { me.fail() } else { me.resume() };
            };
            let v = me.clone();
            me.bus.call_cb(method(NAME, &path, DEVICE, "Claim").append1(""), move |r| {
                v.busy.set(v.busy.get() - 1);
                if v.epoch.get() != epoch {
                    // claimed for a run already stopped
                    return if r.is_ok() { v.release(path) } else { v.resume() };
                }
                if r.is_err() {
                    return v.fail();
                }
                *v.device.borrow_mut() = Some(path);
                v.verify(epoch);
            });
        });
    }

    /// After NoMatch: verify again on the same claim.
    pub fn again(self: &Rc<Self>) {
        let Some(path) = self.device.borrow().clone() else { return };
        let epoch = self.epoch.get();
        let me = self.clone();
        self.bus.call_cb(method(NAME, &path, DEVICE, "VerifyStop"), move |r| {
            if me.epoch.get() != epoch {
                return;
            }
            if r.is_err() {
                return me.fail();
            }
            me.verify(epoch);
        });
    }

    fn verify(self: &Rc<Self>, epoch: u64) {
        let Some(path) = self.device.borrow().clone() else { return };
        let me = self.clone();
        self.bus.call_cb(method(NAME, &path, DEVICE, "VerifyStart").append1("any"), move |r| {
            if me.epoch.get() != epoch {
                return;
            }
            if r.is_err() {
                return me.fail();
            }
            me.verifying.set(true);
            (me.on_event)(Event::Listening);
        });
    }

    fn release(self: &Rc<Self>, path: String) {
        self.busy.set(self.busy.get() + 1);
        let me = self.clone();
        self.bus.call_cb(method(NAME, &path, DEVICE, "Release"), move |_| {
            me.busy.set(me.busy.get() - 1);
            me.resume();
        });
    }

    /// Releases the reader (which stops a verify); nothing from the run follows.
    pub fn stop(self: &Rc<Self>) {
        self.epoch.set(self.epoch.get() + 1);
        self.active.set(false);
        self.verifying.set(false);
        let claimed = self.device.borrow_mut().take();
        if let Some(path) = claimed {
            self.release(path);
        }
    }
}
