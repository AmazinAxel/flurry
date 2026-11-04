// org.freedesktop.Notifications server (ids, replace, actions,
// resident, expire timeouts, close reasons).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use ::dbus::arg::{ArgType, Iter};

use crate::lib::dbus::{self, Message};
use crate::ui::runtime;

const PATH: &str = "/org/freedesktop/Notifications";
const IFACE: &str = "org.freedesktop.Notifications";
/// kept notifications; the oldest expires past this
const MAX: usize = 100;
/// summary/body byte cap
const MAX_TEXT: usize = 4096;
/// action pairs kept (the rest are ignored), and their key/label byte caps
const MAX_ACTIONS: usize = 3;
const MAX_ACTION_KEY: usize = 64;
const MAX_ACTION_LABEL: usize = 256;

#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>,
    pub resident: bool,
    pub desktop_entry: String,
    /// sent by flurry itself (send_local)
    pub local: bool,
    /// unique bus name of the D-Bus client that sent it ("" when local): only it may close it
    sender: String,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Reason {
    Expired = 1,
    Dismissed = 2,
    Closed = 3,
}

type Notified = Rc<dyn Fn(u32)>;
type Resolved = Rc<dyn Fn(u32, Reason)>;

#[derive(Default)]
struct Store {
    map: HashMap<u32, Notification>,
    /// ids, oldest first
    order: VecDeque<u32>,
}

thread_local! {
    static NOTIFS: RefCell<Store> = RefCell::default();
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };
    static ON_NOTIFIED: RefCell<Vec<Notified>> = const { RefCell::new(Vec::new()) };
    static ON_RESOLVED: RefCell<Vec<Resolved>> = const { RefCell::new(Vec::new()) };
    /// action handlers for notifications flurry sent itself (notifySend)
    static LOCAL_ACTIONS: RefCell<HashMap<u32, Rc<dyn Fn(&str)>>> = RefCell::default();
    /// expire timers, replaced along with their notification
    static EXPIRY: RefCell<HashMap<u32, runtime::Timer>> = RefCell::default();
}

pub fn on_notified(f: impl Fn(u32) + 'static) {
    ON_NOTIFIED.with(|l| l.borrow_mut().push(Rc::new(f)));
}
pub fn on_resolved(f: impl Fn(u32, Reason) + 'static) {
    ON_RESOLVED.with(|l| l.borrow_mut().push(Rc::new(f)));
}

pub fn get(id: u32) -> Option<Notification> {
    NOTIFS.with(|n| n.borrow().map.get(&id).cloned())
}

/// Up to `max` kept notifications passing `keep`, newest first.
pub fn newest(keep: impl Fn(&Notification) -> bool, max: usize) -> Vec<Notification> {
    NOTIFS.with(|n| {
        let n = n.borrow();
        n.order.iter().rev().filter_map(|id| n.map.get(id)).filter(|x| keep(x)).take(max).cloned().collect()
    })
}

fn exists(id: u32) -> bool {
    NOTIFS.with(|n| n.borrow().map.contains_key(&id))
}

pub fn resolve(id: u32, reason: Reason) {
    let removed = NOTIFS.with(|n| {
        let mut n = n.borrow_mut();
        let removed = n.map.remove(&id).is_some();
        if removed {
            n.order.retain(|x| *x != id);
        }
        removed
    });
    if !removed {
        return;
    }
    LOCAL_ACTIONS.with(|a| a.borrow_mut().remove(&id));
    if let Some(t) = EXPIRY.with(|e| e.borrow_mut().remove(&id)) {
        t.cancel();
    }
    if let Some(bus) = dbus::session() {
        bus.emit(PATH, IFACE, "NotificationClosed", |m| m.append2(id, reason as u32));
    }
    for f in ON_RESOLVED.with(|l| l.borrow().clone()) {
        f(id, reason);
    }
}

pub fn dismiss(id: u32) {
    resolve(id, Reason::Dismissed);
}

pub fn invoke(id: u32, action: &str) {
    let Some(n) = get(id) else { return };
    if let Some(bus) = dbus::session() {
        bus.emit(PATH, IFACE, "ActionInvoked", |m| m.append2(id, action));
    }
    if let Some(f) = LOCAL_ACTIONS.with(|a| a.borrow().get(&id).cloned()) {
        f(action);
    }
    if !n.resident {
        resolve(id, Reason::Closed);
    }
}

/// `s` cut to MAX_TEXT bytes on a char boundary.
fn cap(s: &str) -> String {
    cap_at(s, MAX_TEXT)
}

fn cap_at(s: &str, max: usize) -> String {
    let mut n = s.len().min(max);
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].into()
}

/// A plain window app id or desktop entry name, safe inside sway criteria: [A-Za-z0-9._-], 1..=255 bytes.
pub fn safe_app_id(s: &str) -> bool {
    (1..=255).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// desktop-entry is used in sway criteria: a safe_app_id, else empty.
fn sanitize_entry(s: &str) -> String {
    if safe_app_id(s) { s.into() } else { String::new() }
}

#[derive(Default)]
struct Hints {
    resident: bool,
    desktop_entry: String,
}

/// Reads only `resident` and `desktop-entry` from an a{sv} hints argument; None if it isn't one.
fn read_hints(it: &mut Iter) -> Option<Hints> {
    if &*it.signature() != "a{sv}" {
        return None;
    }
    let mut h = Hints::default();
    let mut a = it.recurse(ArgType::Array)?;
    while a.arg_type() == ArgType::DictEntry {
        let mut e = a.recurse(ArgType::DictEntry)?;
        let k: &str = e.read().ok()?;
        if k == "resident" || k == "desktop-entry" {
            let mut v = e.recurse(ArgType::Variant)?;
            match (k, v.arg_type()) {
                ("desktop-entry", ArgType::String) => h.desktop_entry = sanitize_entry(v.get::<&str>()?),
                ("resident", ArgType::Boolean) => h.resident = v.get::<bool>()?,
                ("resident", _) => h.resident = v.get_refarg().and_then(|r| r.as_i64()).is_some_and(|v| v != 0),
                _ => {}
            }
        }
        a.next();
    }
    it.next();
    Some(h)
}

/// Reads at most MAX_ACTIONS (key, label) pairs off an `as` argument, labels capped; a pair with an
/// overlong key is skipped (cut, it would invoke a key the client never sent). None if it isn't one.
fn read_actions(it: &mut Iter) -> Option<Vec<(String, String)>> {
    if &*it.signature() != "as" {
        return None;
    }
    let mut a = it.recurse(ArgType::Array)?;
    let mut actions = vec![];
    while actions.len() < MAX_ACTIONS {
        let Some(key) = a.get::<&str>() else { break };
        a.next();
        let Some(label) = a.get::<&str>() else { break };
        a.next();
        if key.len() <= MAX_ACTION_KEY {
            actions.push((key.into(), cap_at(label, MAX_ACTION_LABEL)));
        }
    }
    it.next();
    Some(actions)
}

/// The next id not in use, without taking it (ids wrap past u32::MAX, skipping 0); at most MAX are
/// in use, so this ends.
fn free_id() -> u32 {
    NEXT_ID.with(|n| {
        while exists(n.get()) {
            n.set(n.get().checked_add(1).unwrap_or(1));
        }
        n.get()
    })
}

fn next_id() -> u32 {
    let id = free_id();
    NEXT_ID.with(|n| n.set(id.checked_add(1).unwrap_or(1)));
    id
}

/// Adds a notification (the Notify method body); returns its id. `sender` is "" for local ones.
#[allow(clippy::too_many_arguments)]
fn add(app_name: &str, replaces_id: u32, summary: &str, body: &str, actions: Vec<(String, String)>, hints: Hints, expire_timeout: i32, sender: &str) -> u32 {
    // a D-Bus client can't take over a local one (its actions would run under the client's label);
    // any client may replace another's: `notify-send -r` connects anew (a new unique name) each time
    let replaced = replaces_id != 0 && NOTIFS.with(|n| n.borrow().map.get(&replaces_id).is_some_and(|x| !x.local || sender.is_empty()));
    let id = if replaced { replaces_id } else { next_id() };
    if !replaced {
        while let Some(oldest) = NOTIFS.with(|n| {
            let n = n.borrow();
            (n.map.len() >= MAX).then(|| n.order.front().copied()).flatten()
        }) {
            resolve(oldest, Reason::Expired);
        }
    }
    let n = Notification {
        id,
        app_name: cap(app_name),
        summary: cap(summary),
        body: cap(body),
        actions,
        resident: hints.resident,
        desktop_entry: hints.desktop_entry,
        local: sender.is_empty(),
        sender: sender.into(),
    };
    NOTIFS.with(|s| {
        let mut s = s.borrow_mut();
        if s.map.insert(id, n).is_none() {
            s.order.push_back(id);
        }
    });
    // default-timeout -1, ignore-timeout false; a replacement restarts (or clears) the timer.
    // Set before the listeners, which may resolve it right away.
    let timer = (expire_timeout > 0).then(|| runtime::timeout(expire_timeout as u64, move || resolve(id, Reason::Expired)));
    if let Some(old) = EXPIRY.with(|e| {
        let mut e = e.borrow_mut();
        match timer {
            Some(t) => e.insert(id, t),
            None => e.remove(&id),
        }
    }) {
        old.cancel();
    }
    for f in ON_NOTIFIED.with(|l| l.borrow().clone()) {
        f(id);
    }
    id
}

/// notify-send replacement for flurry's own notifications; `on_action` gets the invoked action id.
pub fn send_local(app_name: &str, summary: &str, body: &str, actions: &[(String, String)], on_action: impl Fn(&str) + 'static) -> u32 {
    // registered before add() so a handler sees it if the notification is hidden right away
    let id = free_id();
    LOCAL_ACTIONS.with(|a| a.borrow_mut().insert(id, Rc::new(on_action)));
    let got = add(app_name, 0, summary, body, actions.to_vec(), Hints::default(), -1, "");
    debug_assert_eq!(got, id);
    got
}

/// Method handlers return None on bad arguments: dbus.rs answers InvalidArgs.
pub fn start() {
    let Some(bus) = dbus::session() else { return };
    bus.export(IFACE, "Notify", |m: &Message| {
        let mut it = m.iter_init();
        let sender = m.sender()?;
        let app: &str = it.read().ok()?;
        let replaces: u32 = it.read().ok()?;
        let _icon: &str = it.read().ok()?;
        let summary: &str = it.read().ok()?;
        let body: &str = it.read().ok()?;
        let actions = read_actions(&mut it)?;
        let hints = read_hints(&mut it)?;
        let timeout: i32 = it.read().ok()?;
        let id = add(app, replaces, summary, body, actions, hints, timeout, &sender);
        Some(m.method_return().append1(id))
    });
    bus.export(IFACE, "CloseNotification", |m| {
        let id = m.read1::<u32>().ok()?;
        // another client's (or a local) notification reads as missing
        let sender = m.sender();
        if !NOTIFS.with(|n| n.borrow().map.get(&id).is_some_and(|x| sender.as_deref() == Some(x.sender.as_str()))) {
            return Some(Message::error(m, &"org.freedesktop.Notifications.Error.NotFound".into(), c"no such notification"));
        }
        resolve(id, Reason::Closed);
        Some(m.method_return())
    });
    bus.export(IFACE, "GetCapabilities", |m| Some(m.method_return().append1(vec!["actions", "body", "persistence"])));
    bus.export(IFACE, "GetServerInformation", |m| Some(m.method_return().append2("notifd", "flurry").append2("0.1", "1.2")));
    bus.request_name(IFACE, |ok| {
        if !ok {
            eprintln!("flurry: another notification daemon owns {IFACE}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_on_char_boundary() {
        let s = "é".repeat(3000);
        let c = cap(&s);
        assert!(c.len() <= MAX_TEXT && c.len().is_multiple_of(2));
        assert_eq!(cap("hi"), "hi");
    }

    #[test]
    fn ids_skip_used_and_wrap() {
        NEXT_ID.with(|n| n.set(u32::MAX));
        let a = add("t", 0, "a", "", vec![], Hints::default(), -1, "");
        assert_eq!(a, u32::MAX);
        assert_eq!(free_id(), 1);
        NEXT_ID.with(|n| n.set(a));
        assert_eq!(next_id(), 1, "in-use id skipped");
        // a D-Bus client can't replace a local one
        assert_ne!(add("t", a, "b", "", vec![], Hints::default(), -1, ":1.5"), a);
        assert_eq!(add("t", a, "c", "", vec![], Hints::default(), -1, ""), a);
        // but may replace another client's (notify-send -r reconnects)
        let d = add("t", 0, "d", "", vec![], Hints::default(), -1, ":1.6");
        assert_eq!(add("t", d, "e", "", vec![], Hints::default(), -1, ":1.7"), d);
        assert_eq!(newest(|_| true, 1)[0].summary, "e");
    }

    #[test]
    fn entry_sanitized() {
        assert_eq!(sanitize_entry("org.gnome.Nautilus"), "org.gnome.Nautilus");
        assert_eq!(sanitize_entry("x\"] kill"), "");
        assert!(safe_app_id("foot_2-x"));
        assert!(!safe_app_id(""));
        assert!(!safe_app_id("a b"));
        assert!(!safe_app_id(&"a".repeat(256)));
    }
}
