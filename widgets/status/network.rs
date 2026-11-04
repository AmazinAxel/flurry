// Wi-Fi menu over iwd. The password popover is an inline entry under the row.

use std::cell::RefCell;
use std::cmp::Reverse;

use xkbcommon::xkb::Keysym;

use crate::lib::menuState::{current, focus_menu};
use crate::lib::menuWindow::watch_in;
use crate::lib::dbus::{Path, prop_str};
use crate::lib::deviceMenu::{DeviceMenu, FocusMemory, Trailing, aside_row, device_menu, device_row};
use crate::lib::iwd::{self, DEVICE, NETWORK, STATION};
use crate::lib::searchDialog::plain_entry;
use crate::lib::style::SPACE_S;
use crate::lib::streamingMode::streaming_mode;
use crate::lib::notifySend::notify_send;
use crate::ui::entry::{self, Entry, EntryRef, Secret};
use crate::ui::node::{Align, Node, Sides, Style};
use crate::ui::runtime;
use crate::ui::state::global_state;
use crate::ui::wayland::KeyEvent;

const LEVEL_ICONS: [&str; 5] = [
    "network-wireless-signal-none-symbolic",
    "network-wireless-signal-weak-symbolic",
    "network-wireless-signal-ok-symbolic",
    "network-wireless-signal-good-symbolic",
    "network-wireless-signal-excellent-symbolic",
];

#[derive(Clone, PartialEq)]
struct WifiNet {
    ssid: String,
    security: String,
    level: usize,
    known: bool,
}

thread_local! {
    static PENDING: RefCell<String> = const { RefCell::new(String::new()) };
    static FOCUSED: FocusMemory = const { FocusMemory::new("wifi:") };
    /// network path with the open password entry
    static PASSWORD: RefCell<Option<(String, EntryRef)>> = const { RefCell::new(None) };
}

/// Redraws the menu window on Wi-Fi changes while the Wi-Fi menu is open.
pub fn watch(w: &std::rc::Rc<crate::ui::window::Win>) {
    const WIFI: &[&str] = &["wifi"];
    watch_in(w, &nets(), WIFI);
    watch_in(w, &connected(), WIFI);
    watch_in(w, &connecting(), WIFI);
    watch_in(w, &show_all(), WIFI);
    watch_in(w, &iwd::scanning(), WIFI);
    watch_in(w, &iwd::powered(), WIFI);
}

global_state! {
    /// (network path, network) in menu order
    fn nets() -> Vec<(String, WifiNet)> = vec![];
    fn connected() -> String = String::new();
    fn connecting() -> String = String::new();
    /// unknown networks listed too (after a scan); the list itself always holds them all
    fn show_all() -> bool = false;
}

/// The listed networks, in order: known, connected or joining, or any after a scan.
fn shown() -> Vec<(String, WifiNet)> {
    let (all, conn, joining) = (show_all().peek(), connected().peek(), connecting().peek());
    nets().peek().into_iter().filter(|(p, n)| n.known || all || *p == conn || *p == joining).collect()
}

/// Signal is in 100*dBm units from iwd
fn sig_level(mbm: i64) -> usize {
    [-5000, -6500, -7500, -8500].iter().position(|t| mbm >= *t).map_or(0, |i| 4 - i)
}

fn row_id(path: &str) -> String {
    FOCUSED.with(|f| f.row_id(path))
}

pub fn focus_target() -> Option<String> {
    if streaming_mode().peek() {
        return None;
    }
    if !iwd::powered().peek() {
        return Some("power".into());
    }
    let list: Vec<String> = shown().into_iter().map(|(p, _)| p).collect();
    for p in [FOCUSED.with(FocusMemory::last), Some(connected().peek())].into_iter().flatten() {
        if list.contains(&p) {
            return Some(row_id(&p));
        }
    }
    Some(list.first().map(|p| row_id(p)).unwrap_or_else(|| "scan".into()))
}

/// Rebuilds the list from iwd's caches on the next idle pass.
fn queue_refresh() {
    // kept live while the menu is closed, so it opens on the current list
    runtime::idle_once(refresh);
}

fn clear() {
    nets().set(vec![]);
    connected().set(String::new());
    connecting().set(String::new());
}

/// The list from iwd's cached tree and ordered networks (unknown ones too: the menu filters).
fn refresh() {
    // the open password entry keeps its row where it is
    if PASSWORD.with(|p| p.borrow().is_some()) {
        return;
    }
    let station = iwd::station();
    if !iwd::powered().peek() || station.is_empty() {
        return clear();
    }
    let ordered = iwd::ordered();
    let (current_net, joining, mut next) = iwd::with_objects(|objects| {
        let st = objects.get(&Path::from(station.as_str())).and_then(|i| i.get(STATION));
        let current_net = st.and_then(|p| p.get("ConnectedNetwork")).and_then(|v| v.0.as_str()).unwrap_or("").to_string();
        let joining = st.and_then(|p| prop_str(p, "State")).as_deref() == Some("connecting");
        let mut next = vec![];
        for (path, mbm) in ordered {
            let Some(p) = objects.get(&Path::from(path.as_str())).and_then(|i| i.get(NETWORK)) else { continue };
            let Some(name) = prop_str(p, "Name") else { continue };
            let kn = p.get("KnownNetwork").and_then(|v| v.0.as_str()).unwrap_or("");
            let known = !kn.is_empty() && kn != "/";
            next.push((path.clone(), WifiNet { ssid: name, security: prop_str(p, "Type").unwrap_or_default(), level: sig_level(mbm as i64), known }));
        }
        (current_net, joining, next)
    });
    let pending = PENDING.with(|p| p.borrow().clone());
    connected().set(if joining { String::new() } else { current_net.clone() });
    connecting().set(if joining { current_net.clone() } else { pending });
    // current first, then strongest, then by name (case-insensitive first)
    next.sort_by_cached_key(|(p, n)| (*p != current_net, Reverse(n.level), n.ssid.to_lowercase(), n.ssid.clone()));
    let reordered = nets().with(|old| old.len() != next.len() || old.iter().zip(&next).any(|(a, b)| a.0 != b.0));
    nets().set(next);
    if reordered {
        let in_list = crate::ui::window::get("menu").and_then(|w| w.focused()).is_some_and(|f| f.starts_with("wifi:"));
        if !in_list {
            focus_menu("wifi", focus_target);
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
        iwd::refresh().await;
    }
}

async fn scan() {
    let scanning = iwd::scanning();
    let station = iwd::station();
    if scanning.peek() || station.is_empty() {
        return;
    }
    scanning.set(true);
    show_all().set(true);
    queue_refresh();
    if let Err(e) = iwd::call(&station, STATION, "Scan", |m| m).await
        && !e.contains("Operation already in progress")
    {
        scanning.set(false);
    }
}

/// Connects to `path`; a failure (e.g. a wrong password) shows as a notification, the menu having
/// no inline error.
fn join(path: &str, passphrase: Option<Secret>) {
    PENDING.with(|p| *p.borrow_mut() = path.to_string());
    connecting().set(path.to_string());
    let path = path.to_string();
    runtime::spawn(async move {
        if let Err(Some(e)) = iwd::connect(&path, passphrase).await {
            let ssid = nets().with(|l| l.iter().find(|(p, _)| *p == path).map(|(_, n)| n.ssid.clone())).unwrap_or_default();
            notify_send("Wi-Fi", &format!("Could not connect to {ssid}"), &e, vec![]);
        }
        if PENDING.with(|p| *p.borrow() == path) {
            PENDING.with(|p| p.borrow_mut().clear());
        }
        iwd::refresh().await;
    });
}

pub fn start() {
    iwd::on_change(queue_refresh);
    current().subscribe(|| {
        iwd::show_networks(current().peek() == Some("wifi"));
        if current().peek() != Some("wifi") {
            show_all().set(false);
            if PASSWORD.with(|p| p.borrow_mut().take()).is_some() {
                // refreshes skipped while it was open
                queue_refresh();
            }
        } else {
            // the list shows from the cache now; signal levels change without a signal, so
            // iwd's are re-read too and the rows update in place once they land
            queue_refresh();
            runtime::spawn(async {
                iwd::refresh().await;
            });
        }
    });
}

pub fn menu(base: &Style, focus: Option<&str>) -> Node {
    if streaming_mode().peek() {
        return Node::label(base.inherit(), "Streaming mode enabled").align(Align::Center, Align::Fill).expand(true, false);
    }
    let on = iwd::powered().peek();
    let (conn, joining) = (connected().peek(), connecting().peek());
    let pw = PASSWORD.with(|p| p.borrow().clone());
    let mut rows = vec![];
    for (path, net) in shown() {
        let id = row_id(&path);
        let s = aside_row(base, path == conn && path != joining, path == joining, focus == Some(id.as_str()));
        let lock = if net.security != "open" { Trailing::Icon("network-wireless-encrypted-symbolic") } else { Trailing::None };
        rows.push(device_row(s, id, LEVEL_ICONS[net.level], net.ssid, lock));
        if let Some((p, e)) = &pw
            && *p == path
        {
            let mut entry = plain_entry(base, e, "wifi-password");
            entry.style.margin = Sides::all(SPACE_S);
            rows.push(entry);
        }
    }
    let icon = if on { "network-wireless-symbolic" } else { "network-wireless-offline-symbolic" };
    device_menu(DeviceMenu { base, powered: on, power_icon: icon, scanning: iwd::scanning().peek(), focus, rows })
}

pub fn on_focus(id: &str) {
    FOCUSED.with(|f| f.on_focus(id));
}

fn close_password() {
    PASSWORD.with(|p| p.borrow_mut().take());
    queue_refresh();
}

/// Joins with the open entry's password; false (entry left open) if it is empty.
fn submit() -> bool {
    let Some((path, e)) = PASSWORD.with(|p| p.borrow().clone()) else { return false };
    if e.borrow().text.is_empty() {
        return false;
    }
    // wiped once the agent has handed it over
    let pw = e.borrow_mut().take_secret();
    close_password();
    join(&path, Some(pw));
    true
}

/// Enter on a focused item; returns the id to focus next, if any.
pub fn activate(id: &str) -> Option<Option<String>> {
    if streaming_mode().peek() {
        return None;
    }
    match id {
        "power" => runtime::spawn(toggle_wifi()),
        "scan" => runtime::spawn(scan()),
        "wifi-password" => {
            let path = PASSWORD.with(|p| p.borrow().as_ref().map(|(p, _)| p.clone()))?;
            return Some(submit().then(|| row_id(&path)));
        }
        _ => {
            let path = id.strip_prefix("wifi:")?.to_string();
            if connecting().peek() == path {
                return Some(None);
            }
            let net = nets().with(|l| l.iter().find(|(p, _)| *p == path).map(|(_, n)| n.clone()))?;
            if connected().peek() == path {
                let station = iwd::station();
                if station.is_empty() {
                    return Some(None);
                }
                runtime::spawn(async move {
                    let _ = iwd::call(&station, STATION, "Disconnect", |m| m).await;
                    iwd::refresh().await;
                });
            } else if net.security != "open" && net.security != "8021x" && !net.known {
                // (an unknown 802.1X network needs a provisioning file, not a passphrase)
                if PASSWORD.with(|p| p.borrow().as_ref().is_some_and(|(p, _)| *p == path)) {
                    close_password();
                    return Some(None);
                }
                let e = entry::new("Password");
                e.borrow_mut().password = true;
                PASSWORD.with(|p| *p.borrow_mut() = Some((path, e)));
                return Some(Some("wifi-password".into()));
            } else {
                join(&path, None);
            }
        }
    }
    Some(None)
}

/// Keys for the inline password entry (Escape closes it, others edit).
pub fn key(id: &str, ev: &KeyEvent) -> bool {
    if id != "wifi-password" || streaming_mode().peek() {
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
