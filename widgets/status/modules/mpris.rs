// MPRIS players (anything but mpd, which has its own client): the track to show while mpd is paused.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::lib::dbus::{self, PropMap, RefArg, SignalFilter, get_all, method, prop_f64, prop_str};
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
    /// as the player reports it (1 is full, some go above); None when it has no Volume property
    volume: Option<f64>,
}

/// Scroll steps on one player's volume until the player has read it back.
struct Steps {
    name: String,
    /// where the steps add up to: the base for further steps
    target: f64,
    /// a Set is on the bus; the target goes out when it returns
    busy: bool,
}

thread_local! {
    static PLAYERS: RefCell<HashMap<String, Player>> = RefCell::default();
    /// bus name of the player `track` shows
    static SHOWN: RefCell<Option<String>> = const { RefCell::new(None) };
    static STEPS: RefCell<Option<Steps>> = const { RefCell::new(None) };
}

global_state! {
    /// (title, artist) of the playing player, else of any player with a track loaded.
    pub fn track() -> Option<(String, String)> = None;
    /// Volume of that player.
    pub fn volume() -> Option<f64> = None;
}

fn publish() {
    let best = PLAYERS.with(|p| {
        let p = p.borrow();
        let best = p.iter().filter(|(_, pl)| !pl.title.is_empty()).min_by(|a, b| b.1.playing.cmp(&a.1.playing).then(a.0.cmp(b.0)));
        best.map(|(n, pl)| (n.clone(), (pl.title.clone(), pl.artist.clone()), pl.volume))
    });
    volume().set(best.as_ref().and_then(|b| b.2));
    track().set(best.as_ref().map(|b| b.1.clone()));
    SHOWN.with(|s| *s.borrow_mut() = best.map(|b| b.0));
}

/// Steps the shown player's volume by `delta`. Steps add up on a local target, so fast scrolls are
/// not lost; one Set at a time is on the bus, and `volume` shows only what the player reads back.
pub fn step_volume(delta: f64) {
    let Some(name) = SHOWN.with(|s| s.borrow().clone()) else { return };
    let ours = STEPS.with(|s| s.borrow().as_ref().filter(|s| s.name == name).map(|s| (s.target, s.busy)));
    let Some(base) = ours.map(|o| o.0).or_else(|| PLAYERS.with(|p| p.borrow().get(&name)?.volume)) else { return };
    // never pulls a player that goes above full down to it
    let target = (base + delta).clamp(0.0, base.max(1.0));
    let busy = ours.is_some_and(|o| o.1);
    STEPS.with(|s| *s.borrow_mut() = Some(Steps { name: name.clone(), target, busy: true }));
    if !busy {
        send_volume(name, target);
    }
}

/// Sets `v`, then the newer target if the steps moved on meanwhile, else reads the volume back
/// (accepted or not).
fn send_volume(name: String, v: f64) {
    let Some(bus) = dbus::session() else {
        STEPS.with(|s| s.borrow_mut().take());
        return;
    };
    bus.call_cb(dbus::set_prop(&name, PATH, PLAYER, "Volume", v), move |_| {
        let next = STEPS.with(|s| {
            let mut s = s.borrow_mut();
            let st = s.as_mut().filter(|st| st.name == name)?;
            st.busy = st.target != v;
            st.busy.then_some(st.target)
        });
        match next {
            Some(t) => send_volume(name, t),
            None => refresh(name, Some(v)),
        }
    });
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
    if let Some(v) = prop_f64(props, "Volume") {
        pl.volume = Some(v.max(0.0));
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
/// `sent`: the Volume set just before; its steps end here (even on error) unless more followed.
fn refresh(name: String, sent: Option<f64>) {
    let Some(bus) = dbus::session() else { return };
    bus.call_cb(get_all(&name, PATH, PLAYER), move |r| {
        if let Some(v) = sent {
            STEPS.with(|s| {
                let mut s = s.borrow_mut();
                if s.as_ref().is_some_and(|st| st.name == name && !st.busy && st.target == v) {
                    *s = None;
                }
            });
        }
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
        refresh(name.into(), None);
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
                        invalidated.iter().any(|k| k == "PlaybackStatus" || k == "Metadata" || k == "Volume").then(|| name.clone())
                    })
                    .collect()
            });
            for name in stale {
                refresh(name, None);
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
