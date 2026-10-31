// Bluetooth device menu (widgets/status/bluetooth.tsx).

use std::cell::{Cell, RefCell};

use xkbcommon::xkb::Keysym;

use crate::lib::menuState::{current, focus_menu};
use crate::lib::bluetooth::{self as bt, Device, bt as bt_state};
use crate::lib::deviceMenu::{DeviceMenu, aside_row, device_menu};
use crate::lib::json;
use crate::lib::state::State;
use crate::lib::style::SPACE_M;
use crate::ui::node::{Align, Node, Style};
use crate::ui::runtime;
use crate::ui::wayland::KeyEvent;
use crate::widgets::status::modules::statusIcons::leak;
use crate::widgets::status::quicksettings::sound::{stop_pending, switch_to_bluetooth_sink};

thread_local! {
    static STICKY: State<Vec<String>> = State::new(vec![]);
    static ORDER: State<Vec<String>> = State::new(load_order());
    static SCAN_TOKEN: Cell<u32> = const { Cell::new(0) };
    static FOCUSED_DEVICE: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn order_path() -> String {
    format!("{}/flurry/bluetooth-order.json", crate::lib::xdg::state_dir())
}

fn load_order() -> Vec<String> {
    std::fs::read_to_string(order_path())
        .ok()
        .and_then(|t| json::parse(&t))
        .map(|j| j.arr().iter().filter_map(|v| v.str().map(String::from)).collect())
        .unwrap_or_default()
}

fn save_order(addresses: &[String]) {
    let prev = ORDER.with(|o| o.peek());
    let next: Vec<String> = addresses.iter().cloned().chain(prev.into_iter().filter(|a| !addresses.contains(a))).collect();
    let text = format!("[{}]", next.iter().map(|a| json::quote(a)).collect::<Vec<_>>().join(","));
    let path = order_path();
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, text);
    ORDER.with(|o| o.set(next));
}

pub fn order() -> State<Vec<String>> {
    ORDER.with(Clone::clone)
}
pub fn sticky() -> State<Vec<String>> {
    STICKY.with(Clone::clone)
}

const NAMES: [(&str, &str); 4] = [("S80A", "Touchscreen Earbuds"), ("MINI_KEYBOARD", "2-key Presenter"), ("K38", "Karaoke Speaker"), ("MOU-302", "Ergo Mouse")];

fn name_substitute(n: &str) -> String {
    NAMES.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string()).unwrap_or_else(|| n.to_string())
}

fn is_known(d: &Device) -> bool {
    d.paired || d.trusted
}

fn has_name(d: &Device) -> bool {
    !d.alias.is_empty() && d.alias.replace('-', ":") != d.address
}

fn listed(d: &Device) -> bool {
    let discovering = bt_state().with(|b| b.discovering());
    has_name(d) && (d.paired || d.trusted || d.connected || discovering || sticky().with(|s| s.contains(&d.address)))
}

/// Known devices in the saved order first, then the rest in discovery order.
fn visible_devices() -> Vec<Device> {
    if current().peek() != Some("bluetooth") {
        return vec![];
    }
    let ord = order().peek();
    let rank = |d: &Device| if is_known(d) { ord.iter().position(|a| *a == d.address).map(|i| i + 1).unwrap_or(ord.len() + 1) } else { usize::MAX };
    let mut list = bt_state().with(|b| b.devices.clone());
    list.sort_by_key(rank);
    list
}

fn first_listed() -> Option<Device> {
    let devices = bt_state().with(|b| b.devices.clone());
    devices.iter().find(|d| d.connected && listed(d)).or_else(|| devices.iter().find(|d| listed(d))).cloned()
}

fn focus_device() -> Option<Device> {
    let f = FOCUSED_DEVICE.with(|f| f.borrow().clone());
    f.and_then(|p| bt_state().with(|b| b.devices.iter().find(|d| d.path == p).cloned())).filter(listed).or_else(first_listed)
}

fn row_id(d: &Device) -> String {
    format!("bt:{}", d.path)
}

fn known_devices() -> Vec<Device> {
    visible_devices().into_iter().filter(|d| is_known(d) && listed(d)).collect()
}

fn move_device(d: &Device, delta: i64) -> bool {
    let mut list = known_devices();
    let Some(i) = list.iter().position(|x| x.path == d.path) else { return false };
    let j = i as i64 + delta;
    if j < 0 || j >= list.len() as i64 {
        return false;
    }
    list.swap(i, j as usize);
    save_order(&list.iter().map(|d| d.address.clone()).collect::<Vec<_>>());
    true
}

pub fn focus_target() -> Option<String> {
    if !bt_state().with(|b| b.powered()) {
        return Some("power".into());
    }
    Some(focus_device().map(|d| row_id(&d)).unwrap_or_else(|| "scan".into()))
}

pub fn refocus() {
    focus_menu("bluetooth", focus_target);
}

pub fn start() {
    current().subscribe(|| {
        if current().peek() == Some("bluetooth") {
            return refocus();
        }
        if bt_state().with(|b| b.discovering()) {
            bt::set_discovery(false);
        }
        if !sticky().with(Vec::is_empty) {
            sticky().set(vec![]);
        }
    });
    // power and connected/connecting changes move focus back onto the focused device
    let last = RefCell::new(String::new());
    bt_state().subscribe(move || {
        let sig: String = bt_state().with(|b| {
            std::iter::once(b.powered().to_string()).chain(b.devices.iter().map(|d| format!("{}{}{}", d.path, d.connected, d.connecting))).collect()
        });
        if *last.borrow() != sig {
            *last.borrow_mut() = sig;
            if current().peek() == Some("bluetooth") {
                refocus();
            }
        }
    });
}

pub fn menu(base: &Style, focus: Option<&str>) -> Node {
    let b = bt_state().peek();
    let devices = visible_devices();
    let none_listed = !devices.iter().any(listed);
    let rows = devices
        .iter()
        .filter(|d| listed(d))
        .map(|d| {
            let id = row_id(d);
            let s = aside_row(base, d.connected && !d.connecting, d.connecting, focus == Some(id.as_str()));
            let battery = d.connected && d.battery >= 0.0;
            let icon = leak(&format!("{}-symbolic", d.icon));
            Node::hbox(s.clone(), 0.0, vec![
                Node::hbox(s.inherit(), SPACE_M, vec![
                    Node::icon(s.inherit(), icon, 16.0),
                    Node::label(s.inherit(), name_substitute(&d.alias)).xalign(0.0).expand(true, false).ellipsize(),
                    Node::label(s.inherit(), format!("{}%", (d.battery * 100.0).round() as i64)).align(Align::End, Align::Fill).show(battery),
                ])
                .expand(true, false)
                .align(Align::Fill, Align::Center),
            ])
            .expand(true, false)
            .focusable(id)
        })
        .collect();
    device_menu(DeviceMenu {
        base,
        powered: b.powered(),
        power_icon: "bluetooth-active-symbolic",
        scanning: b.discovering(),
        empty: none_listed,
        focus,
        rows,
    })
}

fn device_by_id(id: &str) -> Option<Device> {
    let path = id.strip_prefix("bt:")?;
    bt_state().with(|b| b.devices.iter().find(|d| d.path == path).cloned())
}

pub fn on_focus(id: &str) {
    if id.starts_with("bt:") {
        FOCUSED_DEVICE.with(|f| *f.borrow_mut() = id.strip_prefix("bt:").map(String::from));
    }
}

fn connect_and_switch(d: Device) {
    let addr = d.address.clone();
    bt::connect(&d.path, move |ok| {
        if ok {
            switch_to_bluetooth_sink(&addr);
        }
    });
}

/// Enter on a focused button.
pub fn activate(id: &str) -> bool {
    match id {
        "power" => bt::toggle_power(),
        "scan" => {
            let b = bt_state().peek();
            if b.adapter().is_none() {
                return true;
            }
            if b.discovering() {
                bt::set_discovery(false);
                return true;
            }
            bt::set_discovery(true);
            let token = SCAN_TOKEN.with(|t| {
                t.set(t.get() + 1);
                t.get()
            });
            runtime::timeout(60_000, move || {
                if SCAN_TOKEN.with(Cell::get) == token && bt_state().with(|b| b.discovering()) {
                    bt::set_discovery(false);
                }
            });
        }
        _ => {
            let Some(d) = device_by_id(id) else { return false };
            if d.connecting {
                return true;
            }
            if d.connected {
                bt::disconnect(&d.path);
                stop_pending();
                return true;
            }
            sticky().update(|s| {
                if !s.contains(&d.address) {
                    s.push(d.address.clone());
                }
            });
            if bt_state().with(|b| b.discovering()) {
                bt::set_discovery(false);
            }
            if d.paired {
                bt::trust(&d.path);
                connect_and_switch(d);
                return true;
            }
            let path = d.path.clone();
            bt::pair(&path, move || {
                bt::trust(&d.path);
                save_order(&known_devices().iter().map(|d| d.address.clone()).collect::<Vec<_>>());
                connect_and_switch(d.clone());
            });
        }
    }
    true
}

/// Backspace forgets a known device; Shift+Up/Down (k/j) reorders known devices.
pub fn key(id: &str, ev: &KeyEvent) -> bool {
    let Some(d) = device_by_id(id) else { return false };
    if ev.sym == Keysym::BackSpace && is_known(&d) && !bt_state().with(|b| b.discovering()) {
        bt::remove(&d.adapter, &d.path);
    }
    let delta = match ev.lower() {
        Keysym::Up | Keysym::k => -1,
        Keysym::Down | Keysym::j => 1,
        _ => return false,
    };
    if !ev.shift {
        return false;
    }
    if move_device(&d, delta)
        && let Some(w) = crate::ui::window::get("menu")
    {
        w.grab_focus(id);
    }
    true
}

