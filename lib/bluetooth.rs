// BlueZ object model (the subset flurry uses), kept live from ObjectManager signals.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

use crate::lib::dbus::{self, ManagedObjects, Message, PropMap, SignalFilter, method, prop_bool, prop_i64, prop_str, set_prop};
use crate::lib::state::State;

const BLUEZ: &str = "org.bluez";

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Device {
    pub path: String,
    pub adapter: String,
    pub address: String,
    pub alias: String,
    pub icon: String,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    pub connecting: bool,
    /// Battery1.Percentage / 100, -1 when absent
    pub battery: f64,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Adapter {
    pub path: String,
    pub powered: bool,
    pub discovering: bool,
}

/// A pairing code shown in the device's row; `confirm` waits for Enter (accept) or rejection.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Pairing {
    pub device: String,
    pub code: String,
    pub confirm: bool,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Bt {
    pub available: bool,
    pub pairing: Option<Pairing>,
    pub adapters: BTreeMap<String, Adapter>,
    /// insertion order
    pub devices: Vec<Device>,
}

impl Bt {
    pub fn adapter(&self) -> Option<&Adapter> {
        self.adapters.values().next()
    }
    pub fn powered(&self) -> bool {
        self.adapter().is_some_and(|a| a.powered)
    }
    pub fn discovering(&self) -> bool {
        self.adapter().is_some_and(|a| a.discovering)
    }
}

thread_local! {
    static BT: State<Bt> = State::new(Bt::default());
    /// (accept, reject) replies to the agent call awaiting the user's answer
    static PENDING: RefCell<Option<(Message, Message)>> = const { RefCell::new(None) };
}

pub fn bt() -> State<Bt> {
    BT.with(Clone::clone)
}

const DEVICE_KEYS: [&str; 8] = ["Address", "Alias", "Icon", "Adapter", "Paired", "Trusted", "Connected", "Percentage"];

fn set_device(d: &mut Device, p: &PropMap) {
    if let Some(v) = prop_str(p, "Address") {
        d.address = v;
    }
    if let Some(v) = prop_str(p, "Alias") {
        d.alias = v;
    }
    if let Some(v) = prop_str(p, "Icon") {
        d.icon = v;
    }
    if let Some(v) = p.get("Adapter").and_then(|v| v.0.as_str()) {
        d.adapter = v.into();
    }
    if let Some(v) = prop_bool(p, "Paired") {
        d.paired = v;
    }
    if let Some(v) = prop_bool(p, "Trusted") {
        d.trusted = v;
    }
    if let Some(v) = prop_bool(p, "Connected") {
        d.connected = v;
    }
    if let Some(v) = prop_i64(p, "Percentage") {
        d.battery = v as f64 / 100.0;
    }
}

/// Applies the keys flurry uses; the state only changes (and notifies) when one of them did
/// (discovery floods RSSI updates).
fn apply(path: &str, iface: &str, p: &PropMap) {
    match iface {
        "org.bluez.Adapter1" => {
            let (powered, discovering) = (prop_bool(p, "Powered"), prop_bool(p, "Discovering"));
            let cur = bt().with(|b| b.adapters.get(path).cloned());
            if cur.is_some() && powered.is_none() && discovering.is_none() {
                return;
            }
            let mut a = cur.clone().unwrap_or_else(|| Adapter { path: path.into(), ..Default::default() });
            a.powered = powered.unwrap_or(a.powered);
            a.discovering = discovering.unwrap_or(a.discovering);
            if cur.as_ref() != Some(&a) {
                bt().update(|b| {
                    b.adapters.insert(path.into(), a);
                });
            }
        }
        "org.bluez.Device1" | "org.bluez.Battery1" => {
            let cur = bt().with(|b| b.devices.iter().find(|d| d.path == path).cloned());
            if cur.is_some() && !p.keys().any(|k| DEVICE_KEYS.contains(&k.as_str())) {
                return;
            }
            let mut d = match &cur {
                Some(d) => d.clone(),
                None if iface == "org.bluez.Device1" => Device { path: path.into(), battery: -1.0, ..Default::default() },
                None => return,
            };
            set_device(&mut d, p);
            if cur.as_ref() == Some(&d) {
                return;
            }
            bt().update(|b| match b.devices.iter_mut().find(|x| x.path == path) {
                Some(slot) => *slot = d,
                None => b.devices.push(d),
            });
        }
        _ => {}
    }
}

/// Device1 before Battery1 so the battery lands on an existing device.
fn apply_all(path: &str, ifaces: &HashMap<String, PropMap>) {
    for want in ["org.bluez.Adapter1", "org.bluez.Device1", "org.bluez.Battery1"] {
        if let Some(p) = ifaces.get(want) {
            apply(path, want, p);
        }
    }
}

fn removed(path: &str, ifaces: &[String]) {
    bt().update(|b| {
        if ifaces.iter().any(|i| i == "org.bluez.Adapter1") {
            b.adapters.remove(path);
        }
        if ifaces.iter().any(|i| i == "org.bluez.Device1") {
            b.devices.retain(|d| d.path != path);
        }
        if ifaces.iter().any(|i| i == "org.bluez.Battery1")
            && let Some(d) = b.devices.iter_mut().find(|d| d.path == path)
        {
            d.battery = -1.0;
        }
    });
}


/// Watches org.bluez; the menu only exists once bluez appears (lib/bluetooth.tsx).
pub fn start() {
    let Some(bus) = dbus::system() else { return };
    bus.subscribe(
        SignalFilter { sender: Some(BLUEZ.into()), interface: Some("org.freedesktop.DBus.ObjectManager".into()), member: Some("InterfacesAdded".into()), ..Default::default() },
        |m| {
            if let Ok((path, ifaces)) = m.read2::<dbus::Path<'static>, HashMap<String, PropMap>>() {
                apply_all(&path, &ifaces);
            }
        },
    );
    bus.subscribe(
        SignalFilter { sender: Some(BLUEZ.into()), interface: Some("org.freedesktop.DBus.ObjectManager".into()), member: Some("InterfacesRemoved".into()), ..Default::default() },
        |m| {
            if let Ok((path, ifaces)) = m.read2::<dbus::Path<'static>, Vec<String>>() {
                removed(&path, &ifaces);
            }
        },
    );
    bus.subscribe(
        SignalFilter { sender: Some(BLUEZ.into()), interface: Some("org.freedesktop.DBus.Properties".into()), member: Some("PropertiesChanged".into()), ..Default::default() },
        |m| {
            if let (Some(path), Ok((iface, props))) = (m.path(), m.read2::<&str, PropMap>()) {
                apply(&path, iface, &props);
            }
        },
    );
    export_agent(&bus);
    let b2 = bus.clone();
    bus.watch_name(
        BLUEZ,
        move || {
            register_agent(&b2);
            let msg = method(BLUEZ, "/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects");
            b2.call_cb(msg, |r| {
                let Some(objs) = r.ok().and_then(|m| m.read1::<ManagedObjects>().ok()) else { return };
                let mut paths: Vec<_> = objs.into_iter().collect();
                paths.sort_by(|a, b| a.0.cmp(&b.0));
                for (path, ifaces) in paths {
                    apply_all(&path, &ifaces);
                }
                bt().update(|b| b.available = true);
            });
        },
        || {
            PENDING.with(|p| p.borrow_mut().take());
            bt().set(Bt::default())
        },
    );
}

fn call(path: &str, iface: &str, member: &str, done: impl FnOnce(bool) + 'static) {
    let Some(bus) = dbus::system() else { return };
    bus.call_cb(method(BLUEZ, path, iface, member), move |r| done(r.is_ok()));
}

fn set_connecting(path: &str, v: bool) {
    let path = path.to_string();
    bt().update(|b| {
        if let Some(d) = b.devices.iter_mut().find(|d| d.path == path) {
            d.connecting = v;
        }
    });
}

pub fn connect(path: &str, done: impl FnOnce(bool) + 'static) {
    set_connecting(path, true);
    let p = path.to_string();
    call(path, "org.bluez.Device1", "Connect", move |ok| {
        set_connecting(&p, false);
        done(ok);
    });
}

pub fn disconnect(path: &str) {
    call(path, "org.bluez.Device1", "Disconnect", |_| {});
}

/// `done` gets whether pairing succeeded; the code shown for it goes either way.
pub fn pair(path: &str, done: impl FnOnce(bool) + 'static) {
    let p = path.to_string();
    call(path, "org.bluez.Device1", "Pair", move |ok| {
        if bt().with(|b| b.pairing.as_ref().is_some_and(|x| x.device == p)) {
            answer(false);
            bt().update(|b| b.pairing = None);
        }
        done(ok);
    });
}

pub fn trust(path: &str) {
    if let Some(bus) = dbus::system() {
        bus.call_cb(set_prop(BLUEZ, path, "org.bluez.Device1", "Trusted", true), |_| {});
    }
}

pub fn remove(adapter: &str, device: &str) {
    if let Some(bus) = dbus::system() {
        let msg = method(BLUEZ, adapter, "org.bluez.Adapter1", "RemoveDevice").append1(dbus::Path::from(device.to_string()));
        bus.call_cb(msg, |_| {});
    }
}

pub fn set_discovery(on: bool) {
    let Some(a) = bt().with(|b| b.adapter().map(|a| a.path.clone())) else { return };
    call(&a, "org.bluez.Adapter1", if on { "StartDiscovery" } else { "StopDiscovery" }, |_| {});
}

pub fn toggle_power() {
    let Some((path, on)) = bt().with(|b| b.adapter().map(|a| (a.path.clone(), a.powered))) else { return };
    if let Some(bus) = dbus::system() {
        bus.call_cb(set_prop(BLUEZ, &path, "org.bluez.Adapter1", "Powered", !on), |_| {});
    }
}

// Pairing agent (KeyboardDisplay): codes show in the device's row, so both sides can be compared
// (MITM protection); confirmations wait for Enter in the menu.

const AGENT: &str = "org.bluez.Agent1";
const AGENT_PATH: &str = "/org/flurry/BluezAgent";

fn random_code() -> u32 {
    let mut x = 0u32;
    unsafe { libc::getrandom((&raw mut x).cast(), 4, 0) };
    x % 1_000_000
}

fn show(device: &str, code: String, confirm: bool) {
    let p = Pairing { device: device.into(), code, confirm };
    bt().update(|b| b.pairing = Some(p));
}

fn rejected(m: &Message) -> Message {
    Message::error(m, &"org.bluez.Error.Rejected".into(), c"rejected")
}

/// Answers the waiting confirmation, if any; the code stays until pairing ends.
pub fn answer(accept: bool) {
    let Some((yes, no)) = PENDING.with(|p| p.borrow_mut().take()) else { return };
    if let Some(bus) = dbus::system() {
        bus.send(if accept { yes } else { no });
    }
    bt().update(|b| {
        if let Some(p) = &mut b.pairing {
            p.confirm = false;
        }
    });
}

/// Rejects a waiting confirmation and hides the code.
pub fn cancel_pairing() {
    answer(false);
    bt().update(|b| b.pairing = None);
}

fn defer(m: &Message, device: &str, code: String) -> Option<Message> {
    answer(false);
    // replies built now: they need the call's serial; set before showing, which may already reject
    PENDING.with(|p| *p.borrow_mut() = Some((m.method_return(), rejected(m))));
    show(device, code, true);
    None
}

fn export_agent(bus: &Rc<dbus::Bus>) {
    // only bluez may drive the agent
    let guard = |bus: &std::rc::Weak<dbus::Bus>, m: &Message| -> bool {
        bus.upgrade().and_then(|b| b.owner_of(BLUEZ)).is_some_and(|o| m.sender().as_deref() == Some(o.as_str())) && m.path().as_deref() == Some(AGENT_PATH)
    };
    let handlers: [(&str, fn(&Message) -> Option<Message>); 9] = [
        ("Release", |m| {
            cancel_pairing();
            Some(m.method_return())
        }),
        ("Cancel", |m| {
            cancel_pairing();
            Some(m.method_return())
        }),
        ("RequestPinCode", |m| {
            let dev: dbus::Path = m.read1().ok()?;
            let code = format!("{:06}", random_code());
            show(&dev, code.clone(), false);
            Some(m.method_return().append1(code))
        }),
        ("DisplayPinCode", |m| {
            let (dev, code): (dbus::Path, &str) = m.read2().ok()?;
            show(&dev, code.into(), false);
            Some(m.method_return())
        }),
        ("RequestPasskey", |m| {
            let dev: dbus::Path = m.read1().ok()?;
            let key = random_code();
            show(&dev, format!("{key:06}"), false);
            Some(m.method_return().append1(key))
        }),
        ("DisplayPasskey", |m| {
            let (dev, key): (dbus::Path, u32) = m.read2().ok()?;
            show(&dev, format!("{key:06}"), false);
            Some(m.method_return())
        }),
        ("RequestConfirmation", |m| {
            let (dev, key): (dbus::Path, u32) = m.read2().ok()?;
            defer(m, &dev, format!("{key:06}"))
        }),
        ("RequestAuthorization", |m| {
            let dev: dbus::Path = m.read1().ok()?;
            defer(m, &dev, String::new())
        }),
        ("AuthorizeService", |m| {
            let dev: dbus::Path = m.read1().ok()?;
            let ok = bt().with(|b| b.devices.iter().any(|d| d.path == *dev && d.paired && d.trusted));
            Some(if ok { m.method_return() } else { rejected(m) })
        }),
    ];
    for (member, h) in handlers {
        let weak = Rc::downgrade(bus);
        bus.export(AGENT, member, move |m| if guard(&weak, m) { h(m) } else { Some(rejected(m)) });
    }
}

fn register_agent(bus: &dbus::Bus) {
    let path = dbus::Path::from(AGENT_PATH);
    let manager = |member| method(BLUEZ, "/org/bluez", "org.bluez.AgentManager1", member);
    bus.call_cb(manager("RegisterAgent").append2(path.clone(), "KeyboardDisplay"), |_| {});
    bus.call_cb(manager("RequestDefaultAgent").append1(path), |_| {});
}
