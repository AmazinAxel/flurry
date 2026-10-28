// iwd over DBus (lib/iwd.ts).

use crate::lib::dbus::{self, ManagedObjects, Message, PropMap, SignalFilter, method};

pub const IWD: &str = "net.connman.iwd";
pub const DEVICE: &str = "net.connman.iwd.Device";
pub const STATION: &str = "net.connman.iwd.Station";
pub const NETWORK: &str = "net.connman.iwd.Network";

pub async fn call(path: &str, iface: &str, member: &str, build: impl FnOnce(Message) -> Message) -> Result<Message, String> {
    let bus = dbus::system().ok_or("no system bus")?;
    bus.call(build(method(IWD, path, iface, member))).await
}

pub async fn objects() -> Result<ManagedObjects, String> {
    call("/", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", |m| m).await?.read1().map_err(|e| e.to_string())
}

pub fn on_properties_changed(cb: impl Fn(&str, &str, &PropMap) + 'static) {
    let Some(bus) = dbus::system() else { return };
    bus.subscribe(
        SignalFilter { sender: Some(IWD.into()), interface: Some("org.freedesktop.DBus.Properties".into()), member: Some("PropertiesChanged".into()), ..Default::default() },
        move |m| {
            if let (Some(path), Ok((iface, changed))) = (m.path(), m.read2::<&str, PropMap>()) {
                cb(&path, iface, &changed);
            }
        },
    );
}

pub fn on_objects_changed(cb: impl Fn() + 'static) {
    let Some(bus) = dbus::system() else { return };
    bus.subscribe(SignalFilter { sender: Some(IWD.into()), interface: Some("org.freedesktop.DBus.ObjectManager".into()), ..Default::default() }, move |_| cb());
}
