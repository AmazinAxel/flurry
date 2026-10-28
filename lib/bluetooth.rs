// BlueZ object model (the AstalBluetooth subset flurry uses), kept live from ObjectManager signals.

use std::collections::BTreeMap;

use crate::lib::dbus::{self, ManagedObjects, PropMap, SignalFilter, method, prop_bool, prop_i64, prop_str, set_prop};
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

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Bt {
    pub available: bool,
    pub adapters: BTreeMap<String, Adapter>,
    /// insertion order, like AstalBluetooth's device list
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
}

pub fn bt() -> State<Bt> {
    BT.with(Clone::clone)
}

fn apply(path: &str, iface: &str, p: &PropMap) {
    bt().update(|b| match iface {
        "org.bluez.Adapter1" => {
            let a = b.adapters.entry(path.into()).or_insert_with(|| Adapter { path: path.into(), ..Default::default() });
            if let Some(v) = prop_bool(p, "Powered") {
                a.powered = v;
            }
            if let Some(v) = prop_bool(p, "Discovering") {
                a.discovering = v;
            }
        }
        "org.bluez.Device1" | "org.bluez.Battery1" => {
            let i = match b.devices.iter().position(|d| d.path == path) {
                Some(i) => i,
                None if iface == "org.bluez.Device1" => {
                    b.devices.push(Device { path: path.into(), battery: -1.0, ..Default::default() });
                    b.devices.len() - 1
                }
                None => return,
            };
            let d = &mut b.devices[i];
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
        _ => {}
    });
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
            if let Ok((path, ifaces)) = m.read2::<dbus::Path<'static>, std::collections::HashMap<String, PropMap>>() {
                for (iface, props) in &ifaces {
                    apply(&path, iface, props);
                }
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
    let b2 = bus.clone();
    bus.watch_name(
        BLUEZ,
        move || {
            let msg = method(BLUEZ, "/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects");
            b2.call_cb(msg, |r| {
                let Some(objs) = r.ok().and_then(|m| m.read1::<ManagedObjects>().ok()) else { return };
                let mut paths: Vec<_> = objs.into_iter().collect();
                paths.sort_by(|a, b| a.0.cmp(&b.0));
                for (path, ifaces) in paths {
                    // Device1 before Battery1 so the battery lands on an existing device
                    for want in ["org.bluez.Adapter1", "org.bluez.Device1", "org.bluez.Battery1"] {
                        if let Some(p) = ifaces.get(want) {
                            apply(&path, want, p);
                        }
                    }
                }
                bt().update(|b| b.available = true);
            });
        },
        || bt().set(Bt::default()),
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

pub fn pair(path: &str, done: impl FnOnce() + 'static) {
    call(path, "org.bluez.Device1", "Pair", move |ok| {
        if ok {
            done();
        }
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
