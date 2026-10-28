// libdbus channels on the shared poll loop: async method calls, signal subscriptions, name watching,
// and exported methods (the notification daemon).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
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
    signals: RefCell<Vec<(u64, SignalFilter, Handler)>>,
    methods: RefCell<HashMap<(String, String), MethodHandler>>,
    seq: Cell<u64>,
}

#[derive(Default, Clone)]
pub struct SignalFilter {
    pub sender: Option<String>,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
}

impl SignalFilter {
    fn rule(&self) -> String {
        let mut r = String::from("type='signal'");
        for (k, v) in [("sender", &self.sender), ("path", &self.path), ("interface", &self.interface), ("member", &self.member)] {
            if let Some(v) = v {
                r += &format!(",{k}='{v}'");
            }
        }
        r
    }
    fn matches(&self, m: &Message) -> bool {
        let eq = |want: &Option<String>, got: Option<String>| want.as_ref().is_none_or(|w| got.as_deref() == Some(w));
        // sender is matched by the bus (unique name), not here
        eq(&self.path, m.path().map(|p| p.to_string()))
            && eq(&self.interface, m.interface().map(|p| p.to_string()))
            && eq(&self.member, m.member().map(|p| p.to_string()))
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
        seq: Cell::new(0),
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
        let _ = self.ch.read_write(Some(Duration::ZERO));
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
                    let handlers: Vec<Handler> =
                        self.signals.borrow().iter().filter(|(_, f, _)| f.matches(&msg)).map(|(_, _, h)| h.clone()).collect();
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
                            &std::ffi::CString::new("unknown method").unwrap(),
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

    pub fn subscribe(&self, filter: SignalFilter, h: impl Fn(&Message) + 'static) -> u64 {
        let rule = filter.rule();
        self.send(method("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "AddMatch").append1(rule));
        let id = self.seq.get() + 1;
        self.seq.set(id);
        self.signals.borrow_mut().push((id, filter, Rc::new(h)));
        id
    }

    /// Calls `appeared`/`vanished` as `name` gets/loses an owner (Gio.bus_watch_name).
    pub fn watch_name(self: &Rc<Self>, name: &str, appeared: impl Fn() + 'static, vanished: impl Fn() + 'static) {
        let (appeared, vanished) = (Rc::new(appeared), Rc::new(vanished));
        let want = name.to_string();
        let (a, v) = (appeared.clone(), vanished.clone());
        self.subscribe(
            SignalFilter {
                sender: Some("org.freedesktop.DBus".into()),
                interface: Some("org.freedesktop.DBus".into()),
                member: Some("NameOwnerChanged".into()),
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
