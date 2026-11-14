// Bluetooth device menu.

use std::cell::Cell;

use xkbcommon::xkb::Keysym;

use crate::lib::menuState::{current, focus_menu};
use crate::lib::bluetooth::{self as bt, Device, bt as bt_state};
use crate::lib::deviceMenu::{DeviceMenu, FocusMemory, Trailing, aside_row, device_menu, device_row};
use crate::lib::json;
use crate::ui::node::{Node, Style};
use crate::ui::runtime::{self, Timer};
use crate::ui::state::global_state;
use crate::ui::wayland::KeyEvent;
use crate::widgets::status::quicksettings::sound::{stop_pending, switch_to_bluetooth_sink};

/// A scan started from the menu stops after this long.
const SCAN_MS: u64 = 60_000;

thread_local! {
    static SCAN_TIMER: Cell<Option<Timer>> = const { Cell::new(None) };
    static FOCUSED: FocusMemory = const { FocusMemory::new("bt:") };
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
    let prev = order().peek();
    let next: Vec<String> = addresses.iter().cloned().chain(prev.into_iter().filter(|a| !addresses.contains(a))).collect();
    let text = format!("[{}]", next.iter().map(|a| json::quote(a)).collect::<Vec<_>>().join(","));
    let path = order_path();
    if let Some(dir) = std::path::Path::new(&path).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = crate::lib::xdg::write_atomic(&path, text.as_bytes(), 0o600);
    order().set(next);
}

global_state! {
    pub fn order() -> Vec<String> = load_order();
    pub fn sticky() -> Vec<String> = vec![];
}

fn name_substitute(n: &str) -> String {
    crate::settings::BLUETOOTH_NAMES.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string()).unwrap_or_else(|| n.to_string())
}

fn is_known(d: &Device) -> bool {
    d.paired || d.trusted
}

fn has_name(d: &Device) -> bool {
    !d.alias.is_empty() && d.alias.replace('-', ":") != d.address
}

/// Named devices that are known, connected, found by a scan or just used; and any device
/// asking to pair, so its request can be answered.
fn listed(d: &Device) -> bool {
    let (discovering, asking) = bt_state().with(|b| (b.discovering(), b.pairing.as_ref().is_some_and(|p| p.device == d.path)));
    asking || has_name(d) && (is_known(d) || d.connected || discovering || sticky().with(|s| s.contains(&d.address)))
}

/// Known devices in the saved order first, then the rest in discovery order.
fn visible_devices() -> Vec<Device> {
    if current().peek() != Some("bluetooth") {
        return vec![];
    }
    let ord = order().peek();
    let rank = |d: &Device| if is_known(d) { ord.iter().position(|a| *a == d.address).unwrap_or(ord.len()) } else { usize::MAX };
    let mut list = bt_state().with(|b| b.devices.clone());
    list.sort_by_key(rank);
    list
}

fn first_listed() -> Option<Device> {
    visible_devices().into_iter().find(listed)
}

fn focus_device() -> Option<Device> {
    let f = FOCUSED.with(FocusMemory::last);
    f.and_then(|p| bt_state().with(|b| b.devices.iter().find(|d| d.path == p).cloned())).filter(listed).or_else(first_listed)
}

fn row_id(d: &Device) -> String {
    FOCUSED.with(|f| f.row_id(&d.path))
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


/// A confirmation nobody can answer (menu closed) is rejected.
fn reject_unseen() {
    if current().peek() != Some("bluetooth") && bt_state().with(|b| b.pairing.as_ref().is_some_and(|p| p.confirm)) {
        bt::answer(false);
    }
}

pub fn start() {
    current().subscribe(|| {
        if current().peek() == Some("bluetooth") {
            return;
        }
        reject_unseen();
        if bt_state().with(|b| b.discovering()) {
            bt::set_discovery(false);
        }
        if !sticky().with(Vec::is_empty) {
            sticky().set(vec![]);
        }
    });
    bt_state().subscribe(reject_unseen);
    // power and connected/connecting changes move focus back onto the focused device
    let links = bt_state().map(|b| (b.powered(), b.devices.iter().map(|d| (d.path.clone(), d.connected, d.connecting)).collect::<Vec<_>>()));
    // powering on starts at the first row
    let was_on = Cell::new(bt_state().with(|b| b.powered()));
    links.subscribe(move || {
        let on = bt_state().with(|b| b.powered());
        if !was_on.replace(on) && on {
            FOCUSED.with(FocusMemory::forget);
        }
        focus_menu("bluetooth", focus_target)
    });
}

fn icon(d: &Device) -> &'static str {
    crate::ui::icons::find(&format!("{}-symbolic", d.icon)).unwrap_or("bluetooth-symbolic")
}

pub fn menu(base: &Style, focus: Option<&str>) -> Node {
    let b = bt_state().peek();
    let rows: Vec<Node> = visible_devices()
        .iter()
        .filter(|d| listed(d))
        .map(|d| {
            let id = row_id(d);
            let s = aside_row(base, d.connected && !d.connecting, d.connecting, focus == Some(id.as_str()));
            // a pairing code takes the battery's place (an authorization has no code: a key icon);
            // gone once the device is paired, unless it still waits for an answer
            let pairing = b.pairing.as_ref().filter(|p| p.device == d.path && (!d.paired || p.confirm));
            let trailing = match pairing {
                Some(p) if p.code.is_empty() => Trailing::Icon("dialog-password-symbolic"),
                Some(p) => Trailing::Label(p.code.clone()),
                _ if d.connected && d.battery >= 0.0 => Trailing::Label(format!("{}%", (d.battery * 100.0).round() as i64)),
                _ => Trailing::None,
            };
            device_row(s, id, icon(d), name_substitute(&d.alias), trailing)
        })
        .collect();
    device_menu(DeviceMenu {
        base,
        powered: b.powered(),
        power_icon: "bluetooth-active-symbolic",
        scanning: b.discovering(),
        focus,
        rows,
    })
}

fn device_by_id(id: &str) -> Option<Device> {
    let path = FOCUSED.with(|f| f.key(id).map(String::from))?;
    bt_state().with(|b| b.devices.iter().find(|d| d.path == path).cloned())
}

pub fn on_focus(id: &str) {
    FOCUSED.with(|f| f.on_focus(id));
}

/// The device whose pairing waits for Enter.
fn confirming(d: &Device) -> bool {
    bt_state().with(|b| b.pairing.as_ref().is_some_and(|p| p.confirm && p.device == d.path))
}

fn connect_and_switch(d: Device) {
    let addr = d.address.clone();
    // only audio devices get a sink to wait for
    let audio = d.icon.starts_with("audio-");
    bt::connect(&d.path, move |ok| {
        if ok && audio {
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
            let timer = runtime::timeout(SCAN_MS, || {
                SCAN_TIMER.with(|t| t.set(None));
                if bt_state().with(|b| b.discovering()) {
                    bt::set_discovery(false);
                }
            });
            if let Some(old) = SCAN_TIMER.with(|t| t.replace(Some(timer))) {
                old.cancel();
            }
        }
        _ => {
            let Some(d) = device_by_id(id) else { return false };
            if confirming(&d) {
                bt::answer(true);
                return true;
            }
            if d.pairing {
                return true;
            }
            if d.connected || d.connecting {
                bt::disconnect(&d.path);
                stop_pending();
                return true;
            }
            sticky().update(|s| {
                let new = !s.contains(&d.address);
                if new {
                    s.push(d.address.clone());
                }
                new
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
            bt::pair(&path, move |ok| {
                if !ok {
                    return sticky().update(|s| {
                        let n = s.len();
                        s.retain(|a| *a != d.address);
                        s.len() != n
                    });
                }
                bt::trust(&d.path);
                save_order(&known_devices().iter().map(|d| d.address.clone()).collect::<Vec<_>>());
                connect_and_switch(d);
            });
        }
    }
    true
}

/// Backspace forgets a known device; Shift+Up/Down (k/j) reorders known devices.
pub fn key(id: &str, ev: &KeyEvent) -> bool {
    let Some(d) = device_by_id(id) else { return false };
    if ev.sym == Keysym::BackSpace && confirming(&d) {
        bt::answer(false);
        return true;
    }
    if ev.sym == Keysym::BackSpace {
        if is_known(&d) && !bt_state().with(|b| b.discovering()) {
            bt::remove(&d.adapter, &d.path);
        }
        return true;
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

