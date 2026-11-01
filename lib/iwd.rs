// iwd over DBus (lib/iwd.ts): the station's live state shared by the Wi-Fi menu and the status
// icon, and a passphrase agent so secrets never go through argv.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::lib::dbus::{self, ManagedObjects, Message, PropMap, SignalFilter, method, prop_bool, prop_str};
use crate::lib::state::State;
use crate::ui::entry::wipe;
use crate::ui::runtime;

pub const IWD: &str = "net.connman.iwd";
pub const DEVICE: &str = "net.connman.iwd.Device";
pub const STATION: &str = "net.connman.iwd.Station";
pub const NETWORK: &str = "net.connman.iwd.Network";
const AGENT: &str = "net.connman.iwd.Agent";
const AGENT_PATH: &str = "/org/flurry/IwdAgent";

thread_local! {
    static STARTED: Cell<bool> = const { Cell::new(false) };
    /// first object with a Station interface (its Device shares the path); "" when none
    static STATION_PATH: RefCell<String> = const { RefCell::new(String::new()) };
    static POWERED: State<bool> = State::new(false);
    static STATE: State<String> = State::new(String::new());
    static SCANNING: State<bool> = State::new(false);
    static GEN: Cell<u32> = const { Cell::new(0) };
    static LISTENERS: RefCell<Vec<Rc<dyn Fn()>>> = RefCell::default();
    /// (network path, passphrase) handed to the agent for the Connect in flight
    static SECRET: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
}

pub fn station() -> String {
    STATION_PATH.with(|s| s.borrow().clone())
}
/// Device.Powered of the station's device (false until iwd answers).
pub fn powered() -> State<bool> {
    POWERED.with(Clone::clone)
}
/// Station.State ("connected", "connecting", ...), "" without a station.
pub fn state() -> State<String> {
    STATE.with(Clone::clone)
}
pub fn scanning() -> State<bool> {
    SCANNING.with(Clone::clone)
}

/// `f` runs after any iwd change (signals, iwd appearing or vanishing).
pub fn on_change(f: impl Fn() + 'static) {
    LISTENERS.with(|l| l.borrow_mut().push(Rc::new(f)));
}

fn changed() {
    for f in LISTENERS.with(|l| l.borrow().clone()) {
        f();
    }
}

pub async fn call(path: &str, iface: &str, member: &str, build: impl FnOnce(Message) -> Message) -> Result<Message, String> {
    let bus = dbus::system().ok_or("no system bus")?;
    bus.call(build(method(IWD, path, iface, member))).await
}

fn clear() {
    STATION_PATH.with(|s| s.borrow_mut().clear());
    powered().set(false);
    state().set(String::new());
    scanning().set(false);
}

/// Re-reads the object tree and the station's state; None if it failed or a newer read started.
pub async fn refresh() -> Option<ManagedObjects> {
    let generation = GEN.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });
    let objects: ManagedObjects = call("/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", |m| m).await.ok()?.read1().ok()?;
    if generation != GEN.with(Cell::get) {
        return None;
    }
    let mut stations: Vec<_> = objects.iter().filter(|(_, i)| i.contains_key(STATION)).collect();
    stations.sort_by(|a, b| a.0.cmp(b.0));
    match stations.first() {
        Some((path, ifaces)) => {
            STATION_PATH.with(|s| *s.borrow_mut() = path.to_string());
            powered().set(ifaces.get(DEVICE).and_then(|p| prop_bool(p, "Powered")).unwrap_or(false));
            let st = &ifaces[STATION];
            state().set(prop_str(st, "State").unwrap_or_default());
            scanning().set(prop_bool(st, "Scanning").unwrap_or(false));
        }
        None => {
            // a powered-off device has no Station interface
            let dev = objects.iter().filter(|(_, i)| i.contains_key(DEVICE)).min_by(|a, b| a.0.cmp(b.0));
            clear();
            if let Some((_, i)) = dev {
                powered().set(prop_bool(&i[DEVICE], "Powered").unwrap_or(false));
            }
        }
    }
    Some(objects)
}

/// Device path for power toggling: the station's, else the first device.
pub async fn device() -> Option<String> {
    let s = station();
    if !s.is_empty() {
        return Some(s);
    }
    let objects = refresh().await?;
    let s = station();
    if !s.is_empty() {
        return Some(s);
    }
    objects.iter().filter(|(_, i)| i.contains_key(DEVICE)).map(|(p, _)| p.to_string()).min()
}

fn queue_refresh() {
    thread_local! {
        static Q: Cell<bool> = const { Cell::new(false) };
    }
    if Q.with(|q| q.replace(true)) {
        return;
    }
    runtime::idle(|| {
        Q.with(|q| q.set(false));
        runtime::spawn(async {
            refresh().await;
            changed();
        });
    });
}

fn on_properties(path: &str, iface: &str, p: &PropMap) {
    if STATION_PATH.with(|s| *s.borrow() == path) {
        if iface == STATION {
            if let Some(v) = prop_str(p, "State") {
                state().set(v);
            }
            if let Some(v) = prop_bool(p, "Scanning") {
                scanning().set(v);
            }
        } else if iface == DEVICE
            && let Some(v) = prop_bool(p, "Powered")
        {
            powered().set(v);
        }
    } else if iface == DEVICE && prop_bool(p, "Powered").is_some() {
        // a device without a station powering on
        queue_refresh();
    }
    changed();
}

fn from_iwd(bus: &dbus::Bus, m: &Message) -> bool {
    bus.owner_of(IWD).is_some_and(|o| m.sender().as_deref() == Some(o.as_str()))
}

fn take_secret() {
    if let Some((_, mut pw)) = SECRET.with(|s| s.borrow_mut().take()) {
        wipe(&mut pw);
    }
}

fn export_agent(bus: &Rc<dbus::Bus>) {
    let canceled = |m: &Message| Some(Message::error(m, &"net.connman.iwd.Agent.Error.Canceled".into(), c"canceled"));
    let b = Rc::downgrade(bus);
    bus.export(AGENT, "RequestPassphrase", move |m| {
        let b = b.upgrade()?;
        if !from_iwd(&b, m) || m.path().as_deref() != Some(AGENT_PATH) {
            return canceled(m);
        }
        let net: dbus::Path = m.read1().ok()?;
        let secret = SECRET.with(|s| s.borrow_mut().take_if(|(p, _)| p.as_str() == &*net));
        match secret {
            Some((_, mut pw)) => {
                let reply = m.method_return().append1(pw.as_str());
                wipe(&mut pw);
                Some(reply)
            }
            None => canceled(m),
        }
    });
    for member in ["Release", "Cancel"] {
        let b = Rc::downgrade(bus);
        bus.export(AGENT, member, move |m| {
            let b = b.upgrade()?;
            if !from_iwd(&b, m) {
                return canceled(m);
            }
            take_secret();
            Some(m.method_return())
        });
    }
}

fn register_agent(bus: &dbus::Bus) {
    let msg = method(IWD, "/net/connman/iwd", "net.connman.iwd.AgentManager", "RegisterAgent").append1(dbus::Path::from(AGENT_PATH));
    bus.call_cb(msg, |_| {});
}

/// Network.Connect; a non-empty `passphrase` answers the agent's request for this network, then is wiped.
pub async fn connect(network: &str, passphrase: Option<String>) {
    take_secret();
    if let Some(pw) = passphrase {
        SECRET.with(|s| *s.borrow_mut() = Some((network.to_string(), pw)));
    }
    let _ = call(network, NETWORK, "Connect", |m| m).await;
    take_secret();
}

/// Idempotent: subscribes once, keeps the station state live across iwd restarts.
pub fn start() {
    if STARTED.with(|s| s.replace(true)) {
        return;
    }
    let Some(bus) = dbus::system() else { return };
    bus.subscribe(
        SignalFilter { sender: Some(IWD.into()), interface: Some("org.freedesktop.DBus.Properties".into()), member: Some("PropertiesChanged".into()), ..Default::default() },
        |m| {
            if let (Some(path), Ok((iface, changed))) = (m.path(), m.read2::<&str, PropMap>()) {
                on_properties(&path, iface, &changed);
            }
        },
    );
    bus.subscribe(SignalFilter { sender: Some(IWD.into()), interface: Some("org.freedesktop.DBus.ObjectManager".into()), ..Default::default() }, |_| queue_refresh());
    export_agent(&bus);
    let b = Rc::downgrade(&bus);
    bus.watch_name(
        IWD,
        move || {
            if let Some(b) = b.upgrade() {
                register_agent(&b);
            }
            queue_refresh();
        },
        || {
            GEN.with(|g| g.set(g.get() + 1));
            take_secret();
            clear();
            changed();
        },
    );
}
