// UPower DisplayDevice (AstalBattery subset): percentage, state, icon, presence.

use crate::lib::dbus::{self, PropMap, SignalFilter, get_all, prop_bool, prop_f64, prop_i64, prop_str};
use crate::lib::state::State;

const DEST: &str = "org.freedesktop.UPower";
const PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const IFACE: &str = "org.freedesktop.UPower.Device";

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Battery {
    pub percentage: f64,
    /// UPower state: 1 charging, 2 discharging, 3 empty, 4 fully charged, 5 pending charge, 6 pending discharge
    pub state: i64,
    pub icon: String,
    pub present: bool,
}

impl Battery {
    pub fn charging(&self) -> bool {
        self.state == 1
    }
    /// record/service.ts onBattery
    pub fn on_battery(&self) -> bool {
        self.present && ![1, 4, 5].contains(&self.state)
    }
}

thread_local! {
    static BATTERY: State<Battery> = State::new(Battery::default());
}

pub fn battery() -> State<Battery> {
    BATTERY.with(Clone::clone)
}

fn apply(p: &PropMap) {
    battery().update(|b| {
        if let Some(v) = prop_f64(p, "Percentage") {
            b.percentage = v / 100.0;
        }
        if let Some(v) = prop_i64(p, "State") {
            b.state = v;
        }
        if let Some(v) = prop_str(p, "IconName") {
            b.icon = v;
        }
        if let Some(v) = prop_bool(p, "IsPresent") {
            b.present = v;
        }
    });
}

pub fn start() {
    let Some(bus) = dbus::system() else { return };
    bus.subscribe(
        SignalFilter {
            path: Some(PATH.into()),
            interface: Some("org.freedesktop.DBus.Properties".into()),
            member: Some("PropertiesChanged".into()),
            ..Default::default()
        },
        |m| {
            if let Ok((iface, p)) = m.read2::<&str, PropMap>()
                && iface == IFACE
            {
                apply(&p);
            }
        },
    );
    bus.call_cb(get_all(DEST, PATH, IFACE), |r| {
        if let Some(p) = r.ok().and_then(|m| m.read1::<PropMap>().ok()) {
            apply(&p);
        }
    });
}
