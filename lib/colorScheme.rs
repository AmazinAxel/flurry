// The portal's color scheme, mirrored into the night light temperature (wl-gammarelay).

use crate::lib::dbus::{self, SignalFilter, method, set_prop};
use crate::settings;
use crate::ui::state::global_state;

global_state! {
    pub fn dark() -> bool = true;
}

/// Mirrors dark/light mode into the night light temperature.
fn apply_color_scheme(dark_mode: bool) {
    dark().set(dark_mode);
    if let Some((d, l)) = settings::NIGHT_LIGHT
        && let Some(bus) = dbus::session()
    {
        let k = (if dark_mode { d } else { l }).min(u16::MAX as u32) as u16;
        let mut m = set_prop("rs.wl-gammarelay", "/", "rs.wl.gammarelay", "Temperature", k);
        m.set_auto_start(false);
        bus.send(m);
    }
}

fn read_scheme() {
    let Some(bus) = dbus::session() else { return };
    let m = method("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Settings", "ReadOne")
        .append2("org.freedesktop.appearance", "color-scheme");
    bus.call_cb(m, |r| {
        // 1 = prefer dark; no answer (no portal yet) changes nothing: SettingChanged follows
        if let Some(v) = r.ok().and_then(|m| m.read1::<dbus::Variant<Box<dyn dbus::RefArg>>>().ok()).and_then(|v| v.0.as_u64()) {
            apply_color_scheme(v == 1);
        }
    });
}

pub fn start() {
    let Some(bus) = dbus::session() else { return };
    bus.subscribe(
        SignalFilter {
            sender: Some("org.freedesktop.portal.Desktop".into()),
            interface: Some("org.freedesktop.portal.Settings".into()),
            member: Some("SettingChanged".into()),
            ..Default::default()
        },
        |m| {
            if let Ok((ns, key, v)) = m.read3::<&str, &str, dbus::Variant<Box<dyn dbus::RefArg>>>()
                && ns == "org.freedesktop.appearance"
                && key == "color-scheme"
            {
                apply_color_scheme(v.0.as_u64() == Some(1));
            }
        },
    );
    read_scheme();
    bus.watch_name("rs.wl-gammarelay", || apply_color_scheme(dark().peek()), || {});
}
