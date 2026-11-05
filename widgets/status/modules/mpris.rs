// MPRIS players (anything but mpd, which has its own client): the track to show while mpd is paused.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::lib::dbus::{self, PropMap, RefArg, SignalFilter, get_all, method, prop_str};
use crate::ui::state::global_state;

const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

/// Longest title/artist kept (a player can send anything).
const MAX_CHARS: usize = 256;

#[derive(Default)]
struct Player {
    /// unique bus name: PropertiesChanged carries only this
    owner: String,
    playing: bool,
    title: String,
    artist: String,
}

thread_local! {
    static PLAYERS: RefCell<HashMap<String, Player>> = RefCell::default();
}

global_state! {
    /// (title, artist) of the playing player, else of any player with a track loaded.
    pub fn track() -> Option<(String, String)> = None;
}

fn publish() {
    let t = PLAYERS.with(|p| {
        let p = p.borrow();
        let best = p.iter().filter(|(_, pl)| !pl.title.is_empty()).min_by(|a, b| b.1.playing.cmp(&a.1.playing).then(a.0.cmp(b.0)));
        best.map(|(_, pl)| (pl.title.clone(), pl.artist.clone()))
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

/// GetAll for `name`. The bus keeps one sender's messages in order, so the reply is newer than
/// every signal before it and older than every signal after it.
fn refresh(name: String) {
    let Some(bus) = dbus::session() else { return };
    bus.call_cb(get_all(&name, PATH, PLAYER), move |r| {
        let Ok(m) = r else { return };
        let Ok(props) = m.read1::<PropMap>() else { return };
        PLAYERS.with(|p| {
            if let Some(pl) = p.borrow_mut().get_mut(&name) {
                *pl = Player { owner: std::mem::take(&mut pl.owner), ..Default::default() };
                apply(pl, &props);
            }
        });
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
            p.insert(name.into(), Player { owner: owner.into(), ..Default::default() });
        }
    });
    if !owner.is_empty() {
        refresh(name.into());
    }
    publish();
}

/// Keybinds: calls `member` (PlayPause, Next, Previous) on spotify, else the playing player, else any
/// (playerctl's `--player=spotify,%any`), from the tracked players: no lookup per press.
pub fn control(member: &'static str) {
    let dest = PLAYERS.with(|p| {
        let p = p.borrow();
        let key = |(n, pl): (&String, &Player)| (!n.ends_with(".spotify"), !pl.playing);
        p.iter().min_by(|a, b| key(*a).cmp(&key(*b)).then(a.0.cmp(b.0))).map(|(n, _)| n.clone())
    });
    if let (Some(dest), Some(bus)) = (dest, dbus::session()) {
        bus.send(method(&dest, PATH, PLAYER, member));
    }
}

pub fn start() {
    let Some(bus) = dbus::session() else { return };
    const DBUS: &str = "org.freedesktop.DBus";
    bus.subscribe(
        SignalFilter { arg0namespace: Some("org.mpris.MediaPlayer2".into()), ..SignalFilter::name_owner_changed() },
        |m| {
            if let Ok((name, _, new)) = m.read3::<&str, &str, &str>() {
                set(name, new);
            }
        },
    );
    // applied to the players owned by the sender's unique name
    bus.subscribe(
        SignalFilter { path: Some(PATH.into()), ..SignalFilter::properties_changed(None) },
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
                        apply(pl, &changed);
                        invalidated.iter().any(|k| k == "PlaybackStatus" || k == "Metadata").then(|| name.clone())
                    })
                    .collect()
            });
            for name in stale {
                refresh(name);
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
