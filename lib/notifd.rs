// org.freedesktop.Notifications server, mirroring AstalNotifd's daemon (ids, replace, actions,
// resident, expire timeouts, close reasons).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::lib::dbus::{self, Message, PropMap, RefArg};
use crate::ui::runtime;

const PATH: &str = "/org/freedesktop/Notifications";
const IFACE: &str = "org.freedesktop.Notifications";

#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    pub id: u32,
    pub app_name: String,
    pub summary: String,
    pub body: String,
    pub actions: Vec<(String, String)>,
    pub resident: bool,
    pub desktop_entry: String,
    pub time: i64,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Reason {
    Expired = 1,
    Dismissed = 2,
    Closed = 3,
}

type Notified = Rc<dyn Fn(u32, bool)>;
type Resolved = Rc<dyn Fn(u32, Reason)>;

thread_local! {
    static NOTIFS: RefCell<Vec<Notification>> = const { RefCell::new(Vec::new()) };
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };
    static ON_NOTIFIED: RefCell<Vec<Notified>> = const { RefCell::new(Vec::new()) };
    static ON_RESOLVED: RefCell<Vec<Resolved>> = const { RefCell::new(Vec::new()) };
    /// action handlers for notifications flurry sent itself (notifySend)
    static LOCAL_ACTIONS: RefCell<HashMap<u32, Rc<dyn Fn(&str)>>> = RefCell::default();
    /// expire timers, replaced along with their notification
    static EXPIRY: RefCell<HashMap<u32, runtime::Timer>> = RefCell::default();
}

pub fn on_notified(f: impl Fn(u32, bool) + 'static) {
    ON_NOTIFIED.with(|l| l.borrow_mut().push(Rc::new(f)));
}
pub fn on_resolved(f: impl Fn(u32, Reason) + 'static) {
    ON_RESOLVED.with(|l| l.borrow_mut().push(Rc::new(f)));
}

pub fn get(id: u32) -> Option<Notification> {
    NOTIFS.with(|n| n.borrow().iter().find(|x| x.id == id).cloned())
}

pub fn resolve(id: u32, reason: Reason) {
    let removed = NOTIFS.with(|n| {
        let mut n = n.borrow_mut();
        let before = n.len();
        n.retain(|x| x.id != id);
        before != n.len()
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

fn hint_str(h: &PropMap, k: &str) -> String {
    h.get(k).and_then(|v| v.0.as_str()).unwrap_or("").to_string()
}

/// Adds a notification (the Notify method body); returns its id.
pub fn add(app_name: &str, replaces_id: u32, summary: &str, body: &str, actions: &[String], hints: &PropMap, expire_timeout: i32) -> u32 {
    let replaced = get(replaces_id).is_some() && replaces_id != 0;
    let id = if replaced {
        replaces_id
    } else {
        NEXT_ID.with(|n| {
            let id = n.get();
            n.set(id + 1);
            id
        })
    };
    let n = Notification {
        id,
        app_name: app_name.into(),
        summary: summary.into(),
        body: body.into(),
        actions: actions.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0].clone(), c[1].clone())).collect(),
        resident: hints.get("resident").and_then(|v| v.0.as_i64()).is_some_and(|v| v != 0),
        desktop_entry: hint_str(hints, "desktop-entry"),
        time: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
    };
    NOTIFS.with(|list| {
        let mut list = list.borrow_mut();
        match list.iter_mut().find(|x| x.id == id) {
            Some(slot) => *slot = n,
            None => list.push(n),
        }
    });
    for f in ON_NOTIFIED.with(|l| l.borrow().clone()) {
        f(id, replaced);
    }
    // default-timeout -1, ignore-timeout false; a replacement restarts (or clears) the timer
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
    id
}

/// notify-send replacement for flurry's own notifications; `on_action` gets the invoked action id.
pub fn send_local(app_name: &str, summary: &str, body: &str, actions: &[(String, String)], on_action: impl Fn(&str) + 'static) -> u32 {
    let flat: Vec<String> = actions.iter().flat_map(|(a, b)| [a.clone(), b.clone()]).collect();
    // registered before add() so a handler sees it if the notification is hidden right away
    let id = NEXT_ID.with(Cell::get);
    LOCAL_ACTIONS.with(|a| a.borrow_mut().insert(id, Rc::new(on_action)));
    let got = add(app_name, 0, summary, body, &flat, &PropMap::new(), -1);
    debug_assert_eq!(got, id);
    got
}

pub fn start() {
    let Some(bus) = dbus::session() else { return };
    bus.export(IFACE, "Notify", |m: &Message| {
        let mut it = m.iter_init();
        let app: String = it.read().ok()?;
        let replaces: u32 = it.read().ok()?;
        let _icon: String = it.read().ok()?;
        let summary: String = it.read().ok()?;
        let body: String = it.read().ok()?;
        let actions: Vec<String> = it.read().ok()?;
        let hints: PropMap = it.read().ok()?;
        let timeout: i32 = it.read().ok()?;
        let id = add(&app, replaces, &summary, &body, &actions, &hints, timeout);
        Some(m.method_return().append1(id))
    });
    bus.export(IFACE, "CloseNotification", |m| {
        let id: u32 = m.read1().ok()?;
        resolve(id, Reason::Closed);
        Some(m.method_return())
    });
    bus.export(IFACE, "GetCapabilities", |m| {
        let caps: Vec<&str> = vec!["action-icons", "actions", "body", "body-hyperlinks", "body-images", "body-markup", "icon-static", "persistence", "sound"];
        Some(m.method_return().append1(caps))
    });
    bus.export(IFACE, "GetServerInformation", |m| Some(m.method_return().append2("notifd", "astal").append2("0.1", "1.2")));
    bus.request_name(IFACE);
}
