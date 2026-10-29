// MPRIS player presence (any player running).

use std::cell::RefCell;
use std::collections::HashSet;

use crate::lib::dbus::{self, SignalFilter, method};
use crate::lib::state::State;
use crate::ui::node::{Node, Sides, Style};

thread_local! {
    static PLAYERS: State<usize> = State::new(0);
    static NAMES: RefCell<HashSet<String>> = RefCell::default();
}

pub fn players() -> State<usize> {
    PLAYERS.with(Clone::clone)
}

fn set(name: &str, present: bool) {
    if !name.starts_with("org.mpris.MediaPlayer2.") {
        return;
    }
    let n = NAMES.with(|s| {
        let mut s = s.borrow_mut();
        if present { s.insert(name.into()) } else { s.remove(name) };
        s.len()
    });
    players().set(n);
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
    bus.call_cb(method("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "ListNames"), |r| {
        if let Some(names) = r.ok().and_then(|m| m.read1::<Vec<String>>().ok()) {
            for n in names {
                set(&n, true);
            }
        }
    });
}

pub fn mpris(base: &Style) -> Node {
    let s = Style { padding: Sides::vh(13.0, 0.0), ..base.inherit() };
    Node::hbox(s, 0.0, vec![Node::icon(base.inherit(), "folder-music-symbolic", 21.0).expand(true, false)]).show(players().peek() > 0)
}
