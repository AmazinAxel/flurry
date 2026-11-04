// UPower DisplayDevice: percentage, state, presence.

use crate::lib::dbus::{self, PropMap, SignalFilter, get_all, prop_bool, prop_f64, prop_i64};
use crate::ui::state::global_state;

const DEST: &str = "org.freedesktop.UPower";
const PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const IFACE: &str = "org.freedesktop.UPower.Device";

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Battery {
    pub percentage: f64,
    /// UPower state: 1 charging, 2 discharging, 3 empty, 4 fully charged, 5 pending charge, 6 pending discharge
    pub state: i64,
    pub present: bool,
}

impl Battery {
    pub fn charging(&self) -> bool {
        self.state == 1
    }
    /// running on battery: not charging, full or about to charge
    pub fn on_battery(&self) -> bool {
        self.present && ![1, 4, 5].contains(&self.state)
    }
}

global_state! {
    pub fn battery() -> Battery = Battery::default();
}

fn apply(p: &PropMap) {
    battery().update(|b| {
        let old = b.clone();
        if let Some(v) = prop_f64(p, "Percentage") {
            b.percentage = v / 100.0;
        }
        if let Some(v) = prop_i64(p, "State") {
            b.state = v;
        }
        if let Some(v) = prop_bool(p, "IsPresent") {
            b.present = v;
        }
        *b != old
    });
}

pub fn start() {
    let Some(bus) = dbus::system() else { return };
    bus.subscribe(
        SignalFilter { path: Some(PATH.into()), ..SignalFilter::properties_changed(Some(DEST)) },
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
