// Wi-Fi menu over iwd (widgets/status/network.tsx). The password popover is an inline entry
// under the row.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use xkbcommon::xkb::Keysym;

use crate::lib::menuState::{current, focus_menu};
use crate::lib::dbus::{prop_bool, prop_str};
use crate::lib::deviceMenu::{DeviceMenu, aside_button, device_menu};
use crate::lib::iwd::{self, DEVICE, NETWORK, STATION};
use crate::lib::process::exec;
use crate::lib::searchDialog::plain_entry;
use crate::lib::state::State;
use crate::lib::streamingMode::streaming_mode;
use crate::ui::entry::{self, Entry, EntryRef};
use crate::ui::node::{Align, Node, Sides, Style};
use crate::ui::runtime;
use crate::ui::wayland::KeyEvent;

const STATION_NAME: &str = "wlan0";
const LEVELS: [&str; 5] = ["none", "weak", "ok", "good", "excellent"];

#[derive(Clone, PartialEq, Debug)]
pub struct WifiNet {
    pub ssid: String,
    pub security: String,
    pub level: usize,
    pub known: bool,
}

thread_local! {
    static PATHS: State<Vec<String>> = State::new(vec![]);
    static NETS: State<BTreeMap<String, WifiNet>> = State::new(BTreeMap::new());
    static CONNECTED: State<String> = State::new(String::new());
    static CONNECTING: State<String> = State::new(String::new());
    static SCANNING: State<bool> = State::new(false);
    static WIFI_ON: State<bool> = State::new(true);
    static STATION_PATH: RefCell<String> = const { RefCell::new(String::new()) };
    static DEVICE_PATH: RefCell<String> = const { RefCell::new(String::new()) };
    static PENDING: RefCell<String> = const { RefCell::new(String::new()) };
    static SHOW_ALL: Cell<bool> = const { Cell::new(false) };
    static GEN: Cell<u32> = const { Cell::new(0) };
    static FOCUSED_PATH: RefCell<Option<String>> = const { RefCell::new(None) };
    /// network path with the open password entry
    static PASSWORD: RefCell<Option<(String, EntryRef)>> = const { RefCell::new(None) };
}

pub fn watch(w: &std::rc::Rc<crate::ui::window::Win>) {
    w.watch(&paths());
    w.watch(&nets());
    w.watch(&connected());
    w.watch(&connecting());
    w.watch(&scanning());
    w.watch(&wifi_on());
    w.watch(&streaming_mode());
}

fn paths() -> State<Vec<String>> {
    PATHS.with(Clone::clone)
}
fn nets() -> State<BTreeMap<String, WifiNet>> {
    NETS.with(Clone::clone)
}
fn connected() -> State<String> {
    CONNECTED.with(Clone::clone)
}
fn connecting() -> State<String> {
    CONNECTING.with(Clone::clone)
}
fn scanning() -> State<bool> {
    SCANNING.with(Clone::clone)
}
fn wifi_on() -> State<bool> {
    WIFI_ON.with(Clone::clone)
}

/// Signal is in 100*dBm units from iwd
pub fn sig_level(mbm: i64) -> usize {
    let dbm = mbm as f64 / 100.0;
    if dbm >= -50.0 {
        4
    } else if dbm >= -65.0 {
        3
    } else if dbm >= -75.0 {
        2
    } else if dbm >= -85.0 {
        1
    } else {
        0
    }
}

fn row_id(path: &str) -> String {
    format!("wifi:{path}")
}

fn focus_target() -> Option<String> {
    if !wifi_on().peek() {
        return Some("power".into());
    }
    let list = paths().peek();
    for p in [FOCUSED_PATH.with(|f| f.borrow().clone()), Some(connected().peek())].into_iter().flatten() {
        if list.contains(&p) {
            return Some(row_id(&p));
        }
    }
    Some(list.first().map(|p| row_id(p)).unwrap_or_else(|| "scan".into()))
}

fn refocus() {
    focus_menu("wifi", focus_target);
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
        if current().peek() == Some("wifi") {
            runtime::spawn(refresh());
        }
    });
}

fn clear() {
    paths().set(vec![]);
    nets().set(BTreeMap::new());
    connected().set(String::new());
    connecting().set(String::new());
}

pub async fn refresh() {
    let generation = GEN.with(|g| {
        g.set(g.get() + 1);
        g.get()
    });
    let Ok(objects) = iwd::objects().await else { return };
    for (path, ifaces) in &objects {
        if ifaces.get(DEVICE).and_then(|p| prop_str(p, "Name")).as_deref() == Some(STATION_NAME) {
            DEVICE_PATH.with(|d| *d.borrow_mut() = path.to_string());
            if ifaces.contains_key(STATION) {
                STATION_PATH.with(|s| *s.borrow_mut() = path.to_string());
            }
        }
    }
    if generation != GEN.with(Cell::get) {
        return;
    }
    let device = DEVICE_PATH.with(|d| d.borrow().clone());
    let station = STATION_PATH.with(|s| s.borrow().clone());
    let powered = objects.iter().find(|(p, _)| p.to_string() == device).and_then(|(_, i)| i.get(DEVICE)).and_then(|p| prop_bool(p, "Powered")) != Some(false);
    wifi_on().set(powered);
    if !powered || station.is_empty() {
        return clear();
    }
    let Ok(reply) = iwd::call(&station, STATION, "GetOrderedNetworks", |m| m).await else { return };
    let Ok(ordered) = reply.read1::<Vec<(crate::lib::dbus::Path<'static>, i16)>>() else { return };
    if generation != GEN.with(Cell::get) || PASSWORD.with(|p| p.borrow().is_some()) {
        return;
    }
    let st = objects.iter().find(|(p, _)| p.to_string() == station).and_then(|(_, i)| i.get(STATION));
    let current_net = st.and_then(|p| p.get("ConnectedNetwork")).and_then(|v| v.0.as_str()).unwrap_or("").to_string();
    let joining = st.and_then(|p| prop_str(p, "State")).as_deref() == Some("connecting");
    let pending = PENDING.with(|p| p.borrow().clone());
    connected().set(if joining { String::new() } else { current_net.clone() });
    connecting().set(if joining { current_net.clone() } else { pending.clone() });

    let show_all = SHOW_ALL.with(Cell::get);
    let mut next = BTreeMap::new();
    let mut order = vec![];
    for (path, mbm) in ordered {
        let path = path.to_string();
        let Some(p) = objects.iter().find(|(x, _)| x.to_string() == path).and_then(|(_, i)| i.get(NETWORK)) else { continue };
        let Some(name) = prop_str(p, "Name") else { continue };
        let kn = p.get("KnownNetwork").and_then(|v| v.0.as_str()).unwrap_or("");
        let known = !kn.is_empty() && kn != "/";
        if !known && !show_all && path != current_net && path != pending {
            continue;
        }
        next.insert(path.clone(), WifiNet { ssid: name, security: prop_str(p, "Type").unwrap_or_default(), level: sig_level(mbm as i64), known });
        order.push(path);
    }
    order.sort_by(|a, b| {
        ((b == &current_net) as i32)
            .cmp(&((a == &current_net) as i32))
            .then(next[b].level.cmp(&next[a].level))
            // localeCompare: case-insensitive first
            .then(next[a].ssid.to_lowercase().cmp(&next[b].ssid.to_lowercase()).then(next[a].ssid.cmp(&next[b].ssid)))
    });
    nets().set(next);
    if order != paths().peek() {
        paths().set(order);
        let in_list = crate::ui::window::get("menu").and_then(|w| w.focused()).is_some_and(|f| f.starts_with("wifi:"));
        if !in_list {
            refocus();
        }
    }
}

async fn toggle_wifi() {
    if DEVICE_PATH.with(|d| d.borrow().is_empty()) {
        refresh().await;
    }
    let next = !wifi_on().peek();
    let device = DEVICE_PATH.with(|d| d.borrow().clone());
    let Some(bus) = crate::lib::dbus::system() else { return };
    if bus.call(crate::lib::dbus::set_prop(iwd::IWD, &device, DEVICE, "Powered", next)).await.is_err() {
        return;
    }
    wifi_on().set(next);
    if !next {
        clear();
    } else {
        refresh().await;
    }
}

async fn scan() {
    if scanning().peek() {
        return;
    }
    scanning().set(true);
    SHOW_ALL.with(|s| s.set(true));
    queue_refresh();
    if STATION_PATH.with(|s| s.borrow().is_empty()) {
        refresh().await;
    }
    let station = STATION_PATH.with(|s| s.borrow().clone());
    if let Err(e) = iwd::call(&station, STATION, "Scan", |m| m).await
        && !e.contains("Operation already in progress")
    {
        scanning().set(false);
    }
}

fn join(path: &str, attempt: impl std::future::Future<Output = ()> + 'static) {
    PENDING.with(|p| *p.borrow_mut() = path.to_string());
    connecting().set(path.to_string());
    let path = path.to_string();
    runtime::spawn(async move {
        attempt.await;
        if PENDING.with(|p| *p.borrow() == path) {
            PENDING.with(|p| p.borrow_mut().clear());
        }
        refresh().await;
    });
}

pub fn start() {
    iwd::on_properties_changed(|path, iface, changed| {
        if iface == STATION && STATION_PATH.with(|s| *s.borrow() == path) && prop_bool(changed, "Scanning") == Some(false) {
            scanning().set(false);
        }
        queue_refresh();
    });
    iwd::on_objects_changed(queue_refresh);
    current().subscribe(|| {
        if current().peek() != Some("wifi") {
            SHOW_ALL.with(|s| s.set(false));
            PASSWORD.with(|p| p.borrow_mut().take());
        } else {
            refocus();
        }
        queue_refresh();
    });
}

pub fn menu(base: &Style, focus: Option<&str>, scroll: f32) -> Node {
    if streaming_mode().peek() {
        return Node::label(base.inherit(), "Streaming mode enabled").align(Align::Center, Align::Fill).expand(true, false);
    }
    let on = wifi_on().peek();
    let list = paths().peek();
    let map = nets().peek();
    let (conn, joining) = (connected().peek(), connecting().peek());
    let pw = PASSWORD.with(|p| p.borrow().clone());
    let mut rows = vec![];
    for path in &list {
        let net = map.get(path).cloned().unwrap_or(WifiNet { ssid: String::new(), security: "open".into(), level: 0, known: false });
        let id = row_id(path);
        let s = aside_button(base, *path == conn && *path != joining, *path == joining, focus == Some(id.as_str()), true);
        let icon = crate::widgets::status::modules::statusIcons::leak(&format!("network-wireless-signal-{}-symbolic", LEVELS[net.level]));
        rows.push(
            Node::hbox(s.clone(), 0.0, vec![Node::hbox(s.inherit(), 10.0, vec![
                Node::icon(s.inherit(), icon, 16.0),
                Node::label(s.inherit(), net.ssid.clone()).xalign(0.0).expand(true, false).ellipsize(),
                Node::icon(s.inherit(), "network-wireless-encrypted-symbolic", 16.0).show(net.security != "open"),
            ])
            .expand(true, false)])
            .focusable(id),
        );
        if let Some((p, e)) = &pw
            && p == path
        {
            let mut entry = plain_entry(base, e, "wifi-password");
            entry.style.margin = Sides::all(4.0);
            rows.push(entry);
        }
    }
    let icon = if on { "network-wireless-symbolic" } else { "network-wireless-offline-symbolic" };
    let scanning = scanning().peek();
    let mut menu = device_menu(DeviceMenu { base, powered: on, power_icon: icon, scanning, scan_sensitive: !scanning, empty: list.is_empty(), focus, scroll, rows });
    menu.hexpand = true;
    menu
}

pub fn on_focus(id: &str) {
    if let Some(p) = id.strip_prefix("wifi:") {
        FOCUSED_PATH.with(|f| *f.borrow_mut() = Some(p.to_string()));
    }
}

fn close_password() {
    PASSWORD.with(|p| p.borrow_mut().take());
    queue_refresh();
}

fn submit(path: &str) {
    let Some((_, e)) = PASSWORD.with(|p| p.borrow().clone()) else { return };
    let pw = e.borrow().text.clone();
    let ssid = nets().with(|m| m.get(path).map(|n| n.ssid.clone())).unwrap_or_default();
    close_password();
    join(path, async move {
        let _ = if pw.is_empty() {
            exec(&["iwctl", "station", STATION_NAME, "connect", &ssid]).await
        } else {
            exec(&["iwctl", "--passphrase", &pw, "station", STATION_NAME, "connect", &ssid]).await
        };
    });
}

/// Enter on a focused item; returns the id to focus next, if any.
pub fn activate(id: &str) -> Option<Option<String>> {
    match id {
        "power" => runtime::spawn(toggle_wifi()),
        "scan" => runtime::spawn(scan()),
        "wifi-password" => {
            let path = PASSWORD.with(|p| p.borrow().as_ref().map(|(p, _)| p.clone()))?;
            submit(&path);
            return Some(Some(row_id(&path)));
        }
        _ => {
            let path = id.strip_prefix("wifi:")?.to_string();
            if connecting().peek() == path {
                return Some(None);
            }
            let net = nets().with(|m| m.get(&path).cloned())?;
            if connected().peek() == path {
                let station = STATION_PATH.with(|s| s.borrow().clone());
                runtime::spawn(async move {
                    let _ = iwd::call(&station, STATION, "Disconnect", |m| m).await;
                    refresh().await;
                });
            } else if net.security != "open" && !net.known {
                if PASSWORD.with(|p| p.borrow().as_ref().is_some_and(|(p, _)| *p == path)) {
                    close_password();
                    return Some(None);
                }
                let e = entry::new("Password");
                e.borrow_mut().password = true;
                PASSWORD.with(|p| *p.borrow_mut() = Some((path, e)));
                return Some(Some("wifi-password".into()));
            } else {
                let p = path.clone();
                join(&path, async move {
                    let _ = iwd::call(&p, NETWORK, "Connect", |m| m).await;
                });
            }
        }
    }
    Some(None)
}

/// Keys for the inline password entry (Escape closes it, others edit).
pub fn key(id: &str, ev: &KeyEvent) -> bool {
    if id != "wifi-password" {
        return false;
    }
    if ev.sym == Keysym::Escape {
        let path = PASSWORD.with(|p| p.borrow().as_ref().map(|(p, _)| p.clone()));
        close_password();
        if let (Some(p), Some(w)) = (path, crate::ui::window::get("menu")) {
            w.grab_focus(&row_id(&p));
        }
        return true;
    }
    let Some((_, e)) = PASSWORD.with(|p| p.borrow().clone()) else { return false };
    Entry::key(&e, ev)
}


