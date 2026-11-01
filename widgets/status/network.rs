// Wi-Fi menu over iwd (widgets/status/network.tsx). The password popover is an inline entry
// under the row.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use xkbcommon::xkb::Keysym;

use crate::lib::menuState::{current, focus_menu};
use crate::lib::dbus::{Path, prop_str};
use crate::lib::deviceMenu::{DeviceMenu, FocusMemory, Trailing, aside_row, device_menu, device_row};
use crate::lib::iwd::{self, DEVICE, NETWORK, STATION};
use crate::lib::searchDialog::plain_entry;
use crate::lib::state::State;
use crate::lib::style::SPACE_S;
use crate::lib::streamingMode::streaming_mode;
use crate::ui::entry::{self, Entry, EntryRef};
use crate::ui::node::{Align, Node, Sides, Style};
use crate::ui::runtime;
use crate::ui::wayland::KeyEvent;

const LEVEL_ICONS: [&str; 5] = [
    "network-wireless-signal-none-symbolic",
    "network-wireless-signal-weak-symbolic",
    "network-wireless-signal-ok-symbolic",
    "network-wireless-signal-good-symbolic",
    "network-wireless-signal-excellent-symbolic",
];

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
    static PENDING: RefCell<String> = const { RefCell::new(String::new()) };
    static SHOW_ALL: Cell<bool> = const { Cell::new(false) };
    static GEN: Cell<u32> = const { Cell::new(0) };
    static FOCUSED: FocusMemory = const { FocusMemory::new("wifi:") };
    /// network path with the open password entry
    static PASSWORD: RefCell<Option<(String, EntryRef)>> = const { RefCell::new(None) };
}

pub fn watch(w: &std::rc::Rc<crate::ui::window::Win>) {
    w.watch(&paths());
    w.watch(&nets());
    w.watch(&connected());
    w.watch(&connecting());
    w.watch(&iwd::scanning());
    w.watch(&iwd::powered());
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

/// Signal is in 100*dBm units from iwd
pub fn sig_level(mbm: i64) -> usize {
    [-5000, -6500, -7500, -8500].iter().position(|t| mbm >= *t).map_or(0, |i| 4 - i)
}

fn row_id(path: &str) -> String {
    FOCUSED.with(|f| f.row_id(path))
}

pub fn focus_target() -> Option<String> {
    if !iwd::powered().peek() {
        return Some("power".into());
    }
    let list = paths().peek();
    for p in [FOCUSED.with(FocusMemory::last), Some(connected().peek())].into_iter().flatten() {
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
    let Some(objects) = iwd::refresh().await else { return };
    if generation != GEN.with(Cell::get) {
        return;
    }
    let station = iwd::station();
    if !iwd::powered().peek() || station.is_empty() {
        return clear();
    }
    let Ok(reply) = iwd::call(&station, STATION, "GetOrderedNetworks", |m| m).await else { return };
    let Ok(ordered) = reply.read1::<Vec<(Path<'static>, i16)>>() else { return };
    if generation != GEN.with(Cell::get) || PASSWORD.with(|p| p.borrow().is_some()) {
        return;
    }
    let st = objects.get(&Path::from(station)).and_then(|i| i.get(STATION));
    let current_net = st.and_then(|p| p.get("ConnectedNetwork")).and_then(|v| v.0.as_str()).unwrap_or("").to_string();
    let joining = st.and_then(|p| prop_str(p, "State")).as_deref() == Some("connecting");
    let pending = PENDING.with(|p| p.borrow().clone());
    connected().set(if joining { String::new() } else { current_net.clone() });
    connecting().set(if joining { current_net.clone() } else { pending.clone() });

    let show_all = SHOW_ALL.with(Cell::get);
    let mut next = BTreeMap::new();
    let mut order = vec![];
    for (path, mbm) in ordered {
        let Some(p) = objects.get(&path).and_then(|i| i.get(NETWORK)) else { continue };
        let Some(name) = prop_str(p, "Name") else { continue };
        let path = path.to_string();
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
    let Some(device) = iwd::device().await else { return };
    let next = !iwd::powered().peek();
    let Some(bus) = crate::lib::dbus::system() else { return };
    if bus.call(crate::lib::dbus::set_prop(iwd::IWD, &device, DEVICE, "Powered", next)).await.is_err() {
        return;
    }
    iwd::powered().set(next);
    if !next {
        clear();
    } else {
        refresh().await;
    }
}

async fn scan() {
    let scanning = iwd::scanning();
    let station = iwd::station();
    if scanning.peek() || station.is_empty() {
        return;
    }
    scanning.set(true);
    SHOW_ALL.with(|s| s.set(true));
    queue_refresh();
    if let Err(e) = iwd::call(&station, STATION, "Scan", |m| m).await
        && !e.contains("Operation already in progress")
    {
        scanning.set(false);
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
    iwd::start();
    iwd::on_change(queue_refresh);
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

pub fn menu(base: &Style, focus: Option<&str>) -> Node {
    if streaming_mode().peek() {
        return Node::label(base.inherit(), "Streaming mode enabled").align(Align::Center, Align::Fill).expand(true, false);
    }
    let on = iwd::powered().peek();
    let list = paths().peek();
    let map = nets().peek();
    let (conn, joining) = (connected().peek(), connecting().peek());
    let pw = PASSWORD.with(|p| p.borrow().clone());
    let mut rows = vec![];
    for (path, net) in list.iter().filter_map(|p| Some((p, map.get(p)?))) {
        let id = row_id(path);
        let s = aside_row(base, *path == conn && *path != joining, *path == joining, focus == Some(id.as_str()));
        let lock = if net.security != "open" { Trailing::Icon("network-wireless-encrypted-symbolic") } else { Trailing::None };
        rows.push(device_row(s, id, LEVEL_ICONS[net.level.min(4)], net.ssid.clone(), lock));
        if let Some((p, e)) = &pw
            && p == path
        {
            let mut entry = plain_entry(base, e, "wifi-password");
            entry.style.margin = Sides::all(SPACE_S);
            rows.push(entry);
        }
    }
    let icon = if on { "network-wireless-symbolic" } else { "network-wireless-offline-symbolic" };
    device_menu(DeviceMenu { base, powered: on, power_icon: icon, scanning: iwd::scanning().peek(), empty: list.is_empty(), focus, rows })
}

pub fn on_focus(id: &str) {
    FOCUSED.with(|f| f.on_focus(id));
}

fn close_password() {
    PASSWORD.with(|p| p.borrow_mut().take());
    queue_refresh();
}

fn submit(path: &str) {
    let Some((_, e)) = PASSWORD.with(|p| p.borrow().clone()) else { return };
    if e.borrow().text.is_empty() {
        return;
    }
    // moved out, not copied: iwd::connect wipes it once the agent has handed it over
    let pw = {
        let mut e = e.borrow_mut();
        (e.cursor, e.anchor) = (0, 0);
        std::mem::take(&mut e.text)
    };
    close_password();
    let p = path.to_string();
    join(path, async move { iwd::connect(&p, Some(pw)).await });
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
                let station = iwd::station();
                if station.is_empty() {
                    return Some(None);
                }
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
                join(&path, async move { iwd::connect(&p, None).await });
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

#[cfg(test)]
mod tests {
    #[test]
    fn signal_levels() {
        assert_eq!(super::sig_level(-4000), 4);
        assert_eq!(super::sig_level(-5000), 4);
        assert_eq!(super::sig_level(-6000), 3);
        assert_eq!(super::sig_level(-7500), 2);
        assert_eq!(super::sig_level(-8000), 1);
        assert_eq!(super::sig_level(-9000), 0);
    }
}
