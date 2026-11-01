// libdbus channels on the shared poll loop: async method calls, signal subscriptions, name watching,
// and exported methods (the notification daemon).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

pub use dbus::{Message, Path};
pub use dbus::arg::{PropMap, RefArg, Variant};
use dbus::channel::{BusType, Channel};
use dbus::message::MessageType;

use crate::ui::runtime;

type Handler = Rc<dyn Fn(&Message)>;
type MethodHandler = Rc<dyn Fn(&Message) -> Option<Message>>;

pub struct Bus {
    ch: Channel,
    replies: RefCell<HashMap<u32, Box<dyn FnOnce(Result<Message, String>)>>>,
    signals: RefCell<Vec<(SignalFilter, Handler)>>,
    methods: RefCell<HashMap<(String, String), MethodHandler>>,
    /// well-known name -> unique owner ("" while unowned), for names used as a filter sender
    owners: Rc<RefCell<HashMap<String, String>>>,
    rules: RefCell<HashSet<String>>,
}

const DBUS: &str = "org.freedesktop.DBus";

#[derive(Default, Clone)]
pub struct SignalFilter {
    pub sender: Option<String>,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub arg0: Option<String>,
    pub arg0namespace: Option<String>,
}

impl SignalFilter {
    fn rule(&self) -> String {
        let mut r = String::from("type='signal'");
        for (k, v) in [
            ("sender", &self.sender),
            ("path", &self.path),
            ("interface", &self.interface),
            ("member", &self.member),
            ("arg0", &self.arg0),
            ("arg0namespace", &self.arg0namespace),
        ] {
            if let Some(v) = v {
                r += &format!(",{k}='{v}'");
            }
        }
        r
    }
    /// Signals carry the sender's unique name, so a well-known `sender` matches only its tracked owner.
    fn matches(&self, m: &Message, owners: &HashMap<String, String>) -> bool {
        let eq = |want: &Option<String>, got: Option<&str>| want.as_deref().is_none_or(|w| got == Some(w));
        let sender = self.sender.as_deref().is_none_or(|s| {
            let got = m.sender();
            let want = if s.starts_with(':') || s == DBUS { Some(s) } else { owners.get(s).map(String::as_str).filter(|o| !o.is_empty()) };
            want.is_some() && got.as_deref() == want
        });
        let arg0 = if self.arg0.is_some() || self.arg0namespace.is_some() { m.get1::<&str>() } else { None };
        let ns = self.arg0namespace.as_deref().is_none_or(|ns| {
            arg0.is_some_and(|a| a.strip_prefix(ns).is_some_and(|rest| rest.is_empty() || rest.starts_with('.')))
        });
        sender
            && ns
            && eq(&self.arg0, arg0)
            && eq(&self.path, m.path().as_deref())
            && eq(&self.interface, m.interface().as_deref())
            && eq(&self.member, m.member().as_deref())
    }
}

thread_local! {
    static SYSTEM: RefCell<Option<Rc<Bus>>> = const { RefCell::new(None) };
    static SESSION: RefCell<Option<Rc<Bus>>> = const { RefCell::new(None) };
}

pub fn system() -> Option<Rc<Bus>> {
    get(&SYSTEM, BusType::System)
}

pub fn session() -> Option<Rc<Bus>> {
    get(&SESSION, BusType::Session)
}

fn get(slot: &'static std::thread::LocalKey<RefCell<Option<Rc<Bus>>>>, kind: BusType) -> Option<Rc<Bus>> {
    if let Some(b) = slot.with(|s| s.borrow().clone()) {
        return Some(b);
    }
    let mut ch = Channel::get_private(kind).ok()?;
    ch.set_watch_enabled(true);
    let bus = Rc::new(Bus {
        ch,
        replies: RefCell::default(),
        signals: RefCell::default(),
        methods: RefCell::default(),
        owners: Rc::default(),
        rules: RefCell::default(),
    });
    let fd = bus.ch.watch().fd;
    let weak = Rc::downgrade(&bus);
    runtime::watch(fd, move || {
        if let Some(b) = weak.upgrade() {
            b.pump();
        }
    });
    let weak = Rc::downgrade(&bus);
    runtime::before_poll(move || {
        if let Some(b) = weak.upgrade() {
            b.ch.flush();
            // libdbus may have buffered messages already read off the socket
            b.dispatch();
        }
    });
    slot.with(|s| *s.borrow_mut() = Some(bus.clone()));
    Some(bus)
}

impl Bus {
    fn pump(&self) {
        if self.ch.read_write(Some(Duration::ZERO)).is_err() {
            // bus gone (logout): stop watching or poll() would spin on the hung-up fd
            runtime::unwatch(self.ch.watch().fd);
        }
        self.dispatch();
    }

    fn dispatch(&self) {
        while let Some(mut msg) = self.ch.pop_message() {
            match msg.msg_type() {
                MessageType::MethodReturn | MessageType::Error => {
                    let Some(serial) = msg.get_reply_serial() else { continue };
                    let Some(cb) = self.replies.borrow_mut().remove(&serial) else { continue };
                    if msg.msg_type() == MessageType::Error {
                        let err = msg.read1::<&str>().map(String::from).unwrap_or_default();
                        let name = msg.as_result().err().and_then(|e| e.name().map(String::from)).unwrap_or_default();
                        cb(Err(format!("{name}: {err}")));
                    } else {
                        cb(Ok(msg));
                    }
                }
                MessageType::Signal => {
                    // flurry subscribes to no directed signals; they'd bypass the match rules
                    if msg.destination().is_some() {
                        continue;
                    }
                    if msg.sender().as_deref() == Some(DBUS)
                        && msg.member().as_deref() == Some("NameOwnerChanged")
                        && let Ok((name, _, new)) = msg.read3::<&str, &str, &str>()
                        && let Some(o) = self.owners.borrow_mut().get_mut(name)
                    {
                        *o = new.into();
                    }
                    let owners = self.owners.borrow();
                    let handlers: Vec<Handler> =
                        self.signals.borrow().iter().filter(|(f, _)| f.matches(&msg, &owners)).map(|(_, h)| h.clone()).collect();
                    drop(owners);
                    for h in handlers {
                        h(&msg);
                    }
                }
                MessageType::MethodCall => {
                    let key = (
                        msg.interface().map(|i| i.to_string()).unwrap_or_default(),
                        msg.member().map(|m| m.to_string()).unwrap_or_default(),
                    );
                    let handler = self.methods.borrow().get(&key).cloned();
                    let reply = match handler {
                        Some(h) => h(&msg),
                        None => Some(Message::error(
                            &msg,
                            &"org.freedesktop.DBus.Error.UnknownMethod".into(),
                            c"unknown method",
                        )),
                    };
                    if let Some(r) = reply
                        && !msg.get_no_reply()
                    {
                        let _ = self.ch.send(r);
                    }
                }
            }
        }
    }

    /// Sends a method call; `cb` gets the reply or an error string.
    pub fn call_cb(&self, msg: Message, cb: impl FnOnce(Result<Message, String>) + 'static) {
        match self.ch.send(msg) {
            Ok(serial) => {
                self.replies.borrow_mut().insert(serial, Box::new(cb));
            }
            Err(()) => cb(Err("send failed".into())),
        }
    }

    pub async fn call(&self, msg: Message) -> Result<Message, String> {
        let (tx, rx) = runtime::oneshot();
        self.call_cb(msg, move |r| tx.send(r));
        rx.await.unwrap_or(Err("dropped".into()))
    }

    /// Fire-and-forget call.
    pub fn send(&self, mut msg: Message) {
        msg.set_no_reply(true);
        let _ = self.ch.send(msg);
    }

    fn add_match(&self, rule: String) {
        if self.rules.borrow_mut().insert(rule.clone()) {
            self.send(method(DBUS, "/org/freedesktop/DBus", DBUS, "AddMatch").append1(rule));
        }
    }

    /// Starts tracking the unique owner of a well-known name (rule first, so no change is missed).
    fn track(&self, name: &str) {
        if self.owners.borrow().contains_key(name) {
            return;
        }
        self.owners.borrow_mut().insert(name.into(), String::new());
        let f = SignalFilter {
            sender: Some(DBUS.into()),
            interface: Some(DBUS.into()),
            member: Some("NameOwnerChanged".into()),
            arg0: Some(name.into()),
            ..Default::default()
        };
        self.add_match(f.rule());
        let (owners, name) = (self.owners.clone(), name.to_string());
        let msg = method(DBUS, "/org/freedesktop/DBus", DBUS, "GetNameOwner").append1(name.as_str());
        self.call_cb(msg, move |r| {
            // a NameOwnerChanged seen before this reply is older than it
            let o = r.ok().and_then(|m| m.read1::<&str>().ok().map(String::from)).unwrap_or_default();
            owners.borrow_mut().insert(name, o);
        });
    }

    /// Unique name currently owning `well_known` (tracked once some filter uses it as sender).
    pub fn owner_of(&self, well_known: &str) -> Option<String> {
        self.owners.borrow().get(well_known).filter(|o| !o.is_empty()).cloned()
    }

    pub fn subscribe(&self, filter: SignalFilter, h: impl Fn(&Message) + 'static) {
        if let Some(s) = filter.sender.as_deref()
            && !s.starts_with(':')
            && s != DBUS
        {
            self.track(s);
        }
        self.add_match(filter.rule());
        self.signals.borrow_mut().push((filter, Rc::new(h)));
    }

    /// Calls `appeared`/`vanished` as `name` gets/loses an owner (Gio.bus_watch_name).
    pub fn watch_name(self: &Rc<Self>, name: &str, appeared: impl Fn() + 'static, vanished: impl Fn() + 'static) {
        let (appeared, vanished) = (Rc::new(appeared), Rc::new(vanished));
        let want = name.to_string();
        let (a, v) = (appeared.clone(), vanished.clone());
        self.subscribe(
            SignalFilter {
                sender: Some(DBUS.into()),
                interface: Some(DBUS.into()),
                member: Some("NameOwnerChanged".into()),
                arg0: Some(name.into()),
                ..Default::default()
            },
            move |m| {
                if let Ok((n, _old, new)) = m.read3::<&str, &str, &str>()
                    && n == want
                {
                    if new.is_empty() { v() } else { a() }
                }
            },
        );
        let msg = method("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "NameHasOwner").append1(name);
        self.call_cb(msg, move |r| match r.ok().and_then(|m| m.read1::<bool>().ok()) {
            Some(true) => appeared(),
            _ => vanished(),
        });
    }

    pub fn request_name(&self, name: &str) {
        // DO_NOT_QUEUE | REPLACE_EXISTING
        self.send(method("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "RequestName").append2(name, 6u32));
    }

    pub fn export(&self, interface: &str, member: &str, h: impl Fn(&Message) -> Option<Message> + 'static) {
        self.methods.borrow_mut().insert((interface.into(), member.into()), Rc::new(h));
    }

    pub fn emit(&self, path: &str, interface: &str, member: &str, build: impl FnOnce(Message) -> Message) {
        if let Ok(m) = Message::new_signal(path, interface, member) {
            let _ = self.ch.send(build(m));
        }
    }
}

pub fn method(dest: &str, path: &str, iface: &str, member: &str) -> Message {
    Message::new_method_call(dest, path, iface, member).expect("valid dbus names")
}

/// org.freedesktop.DBus.Properties.Set with a variant value.
pub fn set_prop<T: dbus::arg::Arg + dbus::arg::Append>(dest: &str, path: &str, iface: &str, prop: &str, value: T) -> Message {
    method(dest, path, "org.freedesktop.DBus.Properties", "Set").append3(iface, prop, Variant(value))
}

pub fn get_all(dest: &str, path: &str, iface: &str) -> Message {
    method(dest, path, "org.freedesktop.DBus.Properties", "GetAll").append1(iface)
}

pub fn prop_str(p: &PropMap, k: &str) -> Option<String> {
    p.get(k).and_then(|v| v.0.as_str()).map(String::from)
}

pub fn prop_bool(p: &PropMap, k: &str) -> Option<bool> {
    p.get(k).and_then(|v| v.0.as_i64()).map(|v| v != 0).or_else(|| p.get(k).and_then(|v| v.0.as_u64()).map(|v| v != 0))
}

pub fn prop_f64(p: &PropMap, k: &str) -> Option<f64> {
    p.get(k).and_then(|v| v.0.as_f64())
}

pub fn prop_i64(p: &PropMap, k: &str) -> Option<i64> {
    p.get(k).and_then(|v| v.0.as_i64())
}

pub type ManagedObjects = HashMap<dbus::Path<'static>, HashMap<String, PropMap>>;
