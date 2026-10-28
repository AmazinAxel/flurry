use crate::lib::battery::battery;
use crate::lib::bluetooth::bt;
use crate::lib::iwd;
use crate::lib::state::State;
use crate::lib::streamingMode::streaming_mode;
use crate::lib::style::*;
use crate::ui::node::{Align, Node, Sides, Style};
use crate::widgets::status::status::volume_icon;

thread_local! {
    static NETWORK_ICON: State<&'static str> = State::new("network-wireless-offline-symbolic");
}

pub fn network_icon() -> State<&'static str> {
    NETWORK_ICON.with(Clone::clone)
}

fn apply_state(state: &str) {
    network_icon().set(match state {
        "connected" | "roaming" => "network-wireless-symbolic",
        "connecting" | "disconnecting" => "network-wireless-acquiring-symbolic",
        _ => "network-wireless-offline-symbolic",
    });
}

pub fn start() {
    iwd::on_properties_changed(|_, iface, changed| {
        if iface == iwd::STATION
            && let Some(s) = crate::lib::dbus::prop_str(changed, "State")
        {
            apply_state(&s);
        }
    });
    crate::ui::runtime::spawn(async {
        if let Ok(objs) = iwd::objects().await {
            let state = objs.values().find_map(|i| i.get(iwd::STATION)).and_then(|p| crate::lib::dbus::prop_str(p, "State")).unwrap_or_default();
            apply_state(&state);
        }
    });
}

thread_local! {
    /// overlay visible={Boolean(battery.percentage)} is evaluated once when the bar is built
    static HAS_BATTERY: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

pub fn status(base: &Style) -> Node {
    let img = |name: &'static str| {
        let mut s = base.inherit();
        s.color = light3();
        s.icon_shadow = vec![shadow_(), glow(12.0, light3())];
        Node::icon(s, name, 14.0)
    };
    let b = battery().peek();
    let has = HAS_BATTERY.with(|h| match h.get() {
        Some(v) => v,
        None => {
            h.set(Some(b.percentage != 0.0));
            b.percentage != 0.0
        }
    });
    let pct = (b.percentage * 100.0).round() as i64;
    let low = pct < 99;
    let mut bat_icon = img(if b.icon.is_empty() { "battery-missing-symbolic" } else { leak(&b.icon) });
    if low {
        bat_icon.style.opacity = 0.4;
    }
    let mut pct_style = base.inherit();
    pct_style.font.size = 13.0;
    pct_style.font.weight = 700;
    pct_style.color = if b.charging() { green() } else { blue2() };
    let battery_node = Node::overlay(base.inherit(), vec![bat_icon, Node::label(pct_style, pct.to_string()).show(low)]).show(has);

    let s = Style { padding: Sides::trbl(8.0, 0.0, 14.0, 0.0), ..base.inherit() };
    Node::vbox(s, 7.0, vec![
        img(volume_icon()),
        battery_node,
        img(network_icon().peek()),
        img("bluetooth-active-symbolic").show(bt().with(|b| b.powered())),
        img("notifications-disabled-symbolic").show(streaming_mode().peek()),
    ])
    .align(Align::Fill, Align::Fill)
}

/// Icon names from UPower are dynamic; intern them for the 'static icon API.
pub fn leak(s: &str) -> &'static str {
    thread_local! {
        static INTERN: std::cell::RefCell<std::collections::HashSet<&'static str>> = Default::default();
    }
    INTERN.with(|i| {
        let mut i = i.borrow_mut();
        if let Some(x) = i.get(s) {
            return *x;
        }
        let x: &'static str = Box::leak(s.to_string().into_boxed_str());
        i.insert(x);
        x
    })
}
