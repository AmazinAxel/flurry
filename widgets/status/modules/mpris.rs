// MPRIS players (anything but mpd, which has its own client): the track to show while mpd is paused.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::lib::dbus::{self, PropMap, RefArg, SignalFilter, get_all, method, prop_str};
use crate::lib::state::State;

const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Default, Clone, PartialEq)]
struct Player {
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
    if track().peek() != t {
        track().set(t);
    }
}

/// First string inside a (possibly variant- or array-wrapped) value.
fn first_str(v: &dyn RefArg) -> Option<String> {
    v.as_str().map(String::from).or_else(|| v.as_iter()?.find_map(first_str))
}

fn refresh(name: String) {
    let Some(bus) = dbus::session() else { return };
    bus.call_cb(get_all(&name, PATH, PLAYER), move |r| {
        let Some(props) = r.ok().and_then(|m| m.read1::<PropMap>().ok()) else { return };
        let mut pl = Player { playing: prop_str(&props, "PlaybackStatus").as_deref() == Some("Playing"), ..Default::default() };
        if let Some(mut it) = props.get("Metadata").and_then(|m| m.0.as_iter()) {
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                match k.as_str() {
                    Some("xesam:title") => pl.title = first_str(v).unwrap_or_default(),
                    Some("xesam:artist") => pl.artist = first_str(v).unwrap_or_default(),
                    _ => {}
                }
            }
        }
        // the name may have vanished while the call was in flight
        PLAYERS.with(|p| {
            if let Some(slot) = p.borrow_mut().get_mut(&name) {
                *slot = pl;
            }
        });
        publish();
    });
}

fn set(name: &str, present: bool) {
    if !name.starts_with("org.mpris.MediaPlayer2.") || name.contains("mpd") {
        return;
    }
    PLAYERS.with(|p| {
        let mut p = p.borrow_mut();
        if present { p.insert(name.into(), Player::default()) } else { p.remove(name) }
    });
    if present {
        refresh(name.into());
    }
    publish();
}

pub fn start() {
    let Some(bus) = dbus::session() else { return };
    bus.subscribe(
        SignalFilter { sender: Some("org.freedesktop.DBus".into()), member: Some("NameOwnerChanged".into()), ..Default::default() },
        |m| {
            if let Ok((name, _, new)) = m.read3::<&str, &str, &str>() {
                set(name, !new.is_empty());
            }
        },
    );
    // signals carry the unique sender name; with a handful of players, re-read them all
    bus.subscribe(
        SignalFilter { path: Some(PATH.into()), interface: Some("org.freedesktop.DBus.Properties".into()), member: Some("PropertiesChanged".into()), ..Default::default() },
        |m| {
            if m.read1::<&str>().ok() == Some(PLAYER) {
                for name in PLAYERS.with(|p| p.borrow().keys().cloned().collect::<Vec<_>>()) {
                    refresh(name);
                }
            }
        },
    );
    bus.call_cb(method("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListNames"), |r| {
        if let Some(names) = r.ok().and_then(|m| m.read1::<Vec<String>>().ok()) {
            for n in names {
                set(&n, true);
            }
        }
    });
}
