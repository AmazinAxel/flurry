// MPRIS players (anything but mpd, which has its own client): the track to show while mpd is paused.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::lib::dbus::{self, PropMap, RefArg, SignalFilter, get_all, method, prop_str};
use crate::lib::state::State;

const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// Longest title/artist kept (a player can send anything).
const MAX_CHARS: usize = 256;

#[derive(Default, Clone, PartialEq)]
struct Player {
    /// unique bus name: PropertiesChanged carries only this
    owner: String,
    /// bumped by every change; a GetAll reply older than one is dropped
    seq: u32,
    playing: bool,
    title: String,
    artist: String,
}

thread_local! {
    static TRACK: State<Option<(String, String)>> = State::new(None);
    static PLAYERS: RefCell<HashMap<String, Player>> = RefCell::default();
}

/// (title, artist) of the playing player, else of any player with a track loaded.
pub fn track() -> State<Option<(String, String)>> {
    TRACK.with(Clone::clone)
}

fn publish() {
    let t = PLAYERS.with(|p| {
        let p = p.borrow();
        let mut v: Vec<_> = p.iter().filter(|(_, pl)| !pl.title.is_empty()).collect();
        v.sort_by(|a, b| b.1.playing.cmp(&a.1.playing).then(a.0.cmp(b.0)));
        v.first().map(|(_, pl)| (pl.title.clone(), pl.artist.clone()))
    });
    track().set(t);
}

/// First string inside a (possibly variant- or array-wrapped) value, truncated.
fn first_str(v: &dyn RefArg) -> Option<String> {
    v.as_str().map(|s| s.chars().take(MAX_CHARS).collect()).or_else(|| v.as_iter()?.find_map(first_str))
}

/// Applies PlaybackStatus/Metadata present in `props`.
fn apply(pl: &mut Player, props: &PropMap) {
    if let Some(s) = prop_str(props, "PlaybackStatus") {
        pl.playing = s == "Playing";
    }
    if let Some(mut it) = props.get("Metadata").and_then(|m| m.0.as_iter()) {
        (pl.title, pl.artist) = (String::new(), String::new());
        while let (Some(k), Some(v)) = (it.next(), it.next()) {
            match k.as_str() {
                Some("xesam:title") => pl.title = first_str(v).unwrap_or_default(),
                Some("xesam:artist") => pl.artist = first_str(v).unwrap_or_default(),
                _ => {}
            }
        }
    }
}

/// GetAll for `name`; a reply outdated by a change meanwhile is asked again once (`retry`).
fn refresh(name: String, retry: bool) {
    let Some(bus) = dbus::session() else { return };
    let Some(seq) = PLAYERS.with(|p| p.borrow_mut().get_mut(&name).map(|pl| {
        pl.seq += 1;
        pl.seq
    })) else { return };
    bus.call_cb(get_all(&name, PATH, PLAYER), move |r| {
        let Ok(m) = r else { return };
        let Ok(props) = m.read1::<PropMap>() else { return };
        // gone, or changed since the call went out
        let stale = PLAYERS.with(|p| match p.borrow_mut().get_mut(&name) {
            Some(pl) if pl.seq == seq => {
                *pl = Player { owner: std::mem::take(&mut pl.owner), seq, ..Default::default() };
                apply(pl, &props);
                false
            }
            Some(_) => true,
            None => false,
        });
        if stale && retry {
            return refresh(name, false);
        }
        publish();
    });
}

fn set(name: &str, owner: &str) {
    if !name.starts_with("org.mpris.MediaPlayer2.") || name.contains("mpd") {
        return;
    }
    PLAYERS.with(|p| {
        let mut p = p.borrow_mut();
        if owner.is_empty() {
            p.remove(name);
        } else {
            let seq = p.get(name).map_or(0, |pl| pl.seq + 1);
            p.insert(name.into(), Player { owner: owner.into(), seq, ..Default::default() });
        }
    });
    if !owner.is_empty() {
        refresh(name.into(), true);
    }
    publish();
}

pub fn start() {
    let Some(bus) = dbus::session() else { return };
    const DBUS: &str = "org.freedesktop.DBus";
    bus.subscribe(
        SignalFilter {
            sender: Some(DBUS.into()),
            interface: Some(DBUS.into()),
            member: Some("NameOwnerChanged".into()),
            arg0namespace: Some("org.mpris.MediaPlayer2".into()),
            ..Default::default()
        },
        |m| {
            if let Ok((name, _, new)) = m.read3::<&str, &str, &str>() {
                set(name, new);
            }
        },
    );
    // applied to the players owned by the sender's unique name
    bus.subscribe(
        SignalFilter { path: Some(PATH.into()), interface: Some("org.freedesktop.DBus.Properties".into()), member: Some("PropertiesChanged".into()), ..Default::default() },
        |m| {
            let Ok((iface, changed, invalidated)) = m.read3::<&str, PropMap, Vec<String>>() else { return };
            let Some(sender) = m.sender() else { return };
            if iface != PLAYER {
                return;
            }
            // one connection can own several player names
            let stale: Vec<String> = PLAYERS.with(|p| {
                let mut p = p.borrow_mut();
                p.iter_mut()
                    .filter(|(_, pl)| pl.owner == *sender)
                    .filter_map(|(name, pl)| {
                        pl.seq += 1;
                        apply(pl, &changed);
                        invalidated.iter().any(|k| k == "PlaybackStatus" || k == "Metadata").then(|| name.clone())
                    })
                    .collect()
            });
            for name in stale {
                refresh(name, true);
            }
            publish();
        },
    );
    bus.call_cb(method(DBUS, "/org/freedesktop/DBus", DBUS, "ListNames"), |r| {
        let Some(names) = r.ok().and_then(|m| m.read1::<Vec<String>>().ok()) else { return };
        for n in names.into_iter().filter(|n| n.starts_with("org.mpris.MediaPlayer2.")) {
            let Some(bus) = dbus::session() else { return };
            bus.call_cb(method(DBUS, "/org/freedesktop/DBus", DBUS, "GetNameOwner").append1(n.as_str()), move |r| {
                if let Some(owner) = r.ok().and_then(|m| m.read1::<String>().ok()) {
                    set(&n, &owner);
                }
            });
        }
    });
}
