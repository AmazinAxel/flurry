// Volume slider and output selector with multi-sink combining (widgets/status/quicksettings/sound.tsx).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crate::lib::audio::{self, Sink};
use crate::lib::state::State;
use crate::ui::runtime;

const COMBINED: &str = "combined-bt-sink";

thread_local! {
    static SELECTED: State<Vec<String>> = State::new(vec![]);
    static LINKED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static WATCHED: RefCell<HashMap<String, bool>> = RefCell::default();
    static FIRST_WIRE: Cell<bool> = const { Cell::new(true) };
}

pub fn selected() -> State<Vec<String>> {
    SELECTED.with(Clone::clone)
}

pub fn node_name(s: &Sink) -> String {
    if s.name.is_empty() { s.serial.to_string() } else { s.name.clone() }
}
fn is_combined(s: &Sink) -> bool {
    node_name(s) == COMBINED
}
fn is_bluetooth(s: &Sink) -> bool {
    node_name(s).starts_with("bluez_")
}
fn speakers() -> Vec<Sink> {
    audio::speakers().peek()
}

pub fn name_substitute(name: &str) -> String {
    if name.contains("HD Audio Controller") {
        return name.rsplit(' ').next().unwrap_or("").to_string(); // 'Speaker' or 'Headphones'
    }
    if name.contains("HDMI") {
        return "Monitor".into(); // monitor has a speaker
    }
    name.to_string()
}

fn link(target: &str, connect: bool) {
    audio::link(COMBINED, target, connect);
}

fn resolve(names: &[String]) -> Vec<Sink> {
    let all = speakers();
    let mut seen = vec![];
    for n in names {
        if !seen.contains(n) {
            seen.push(n.clone());
        }
    }
    seen.iter().filter_map(|n| all.iter().find(|s| node_name(s) == *n).cloned()).collect()
}

pub fn apply_selection(names: &[String]) {
    let sinks = resolve(names);
    if sinks.is_empty() {
        return reconcile_now();
    }
    let combined = speakers().into_iter().find(is_combined);
    let want: Vec<String> = if sinks.len() > 1 && combined.is_some() { sinks.iter().map(node_name).collect() } else { vec![] };
    let linked = LINKED.with(|l| l.borrow().clone());
    for t in &linked {
        if !want.contains(t) {
            link(t, false);
        }
    }
    for n in &want {
        if !linked.contains(n) {
            link(n, true);
        }
    }
    LINKED.with(|l| *l.borrow_mut() = want.clone());
    let target = if want.is_empty() { sinks[0].clone() } else { combined.unwrap() };
    audio::set_default(&target);
    selected().set(if want.is_empty() { vec![node_name(&sinks[0])] } else { want });
}

fn reconcile_now() {
    let linked = LINKED.with(|l| l.borrow().clone());
    if linked.len() > 1 {
        return selected().set(linked);
    }
    if let Some(def) = audio::default_speaker()
        && !is_combined(&def)
    {
        return selected().set(vec![node_name(&def)]);
    }
    let all = speakers();
    let real = all.iter().find(|s| !is_combined(s) && !is_bluetooth(s)).or_else(|| all.iter().find(|s| !is_combined(s)));
    match real {
        Some(r) => audio::set_default(r),
        None => selected().set(vec![]),
    }
}

fn reconcile() {
    thread_local! {
        static QUEUED: Cell<bool> = const { Cell::new(false) };
    }
    if QUEUED.with(|q| q.replace(true)) {
        return;
    }
    runtime::idle(|| {
        QUEUED.with(|q| q.set(false));
        reconcile_now();
    });
}

fn wire() {
    let was_first = FIRST_WIRE.with(|f| f.replace(false));
    let all = speakers();
    let present: Vec<String> = all.iter().map(node_name).collect();
    let mut fresh: Option<Sink> = None;
    WATCHED.with(|w| {
        let mut w = w.borrow_mut();
        w.retain(|n, _| present.contains(n));
        for s in &all {
            let name = node_name(s);
            if w.contains_key(&name) {
                continue;
            }
            w.insert(name, true);
            if is_bluetooth(s) {
                fresh = Some(s.clone());
            }
        }
    });
    let linked = LINKED.with(|l| l.borrow().clone());
    let survivors: Vec<String> = linked.iter().filter(|n| present.contains(n)).cloned().collect();
    let collapsed = survivors.len() != linked.len();
    if collapsed {
        LINKED.with(|l| l.borrow_mut().clear());
        for n in &survivors {
            link(n, false);
        }
    }
    if collapsed && !survivors.is_empty() {
        return apply_selection(&survivors);
    }
    if let Some(f) = fresh
        && !was_first
        && LINKED.with(|l| l.borrow().len()) < 2
    {
        apply_selection(&[node_name(&f)]);
    }
    reconcile();
}

pub fn toggle_sink(s: &Sink) {
    let current = selected().peek();
    let name = node_name(s);
    let next: Vec<String> = if current.contains(&name) { current.into_iter().filter(|n| *n != name).collect() } else { current.into_iter().chain([name]).collect() };
    apply_selection(&next);
}

pub fn start() {
    // isDefault changes on any sink and default-speaker changes both reconcile
    audio::default_name().subscribe(reconcile);
    let names = audio::speakers().map(|l| l.iter().map(node_name).collect::<Vec<_>>());
    names.subscribe(wire);
    audio::ready().subscribe(wire);
    wire();
}

/// Sinks listed in the selector (the combined sink is internal).
pub fn listed() -> Vec<Sink> {
    speakers().into_iter().filter(|s| !is_combined(s)).collect()
}

/// Bluetooth menu: make a freshly connected device the output once its sink shows up.
pub fn switch_to_bluetooth_sink(address: &str) {
    stop_pending();
    let wanted = address.replace(':', "_");
    let take = move || -> bool {
        match speakers().into_iter().find(|s| s.name.contains(&wanted)) {
            Some(s) => {
                audio::set_default(&s);
                true
            }
            None => false,
        }
    };
    if take() {
        return;
    }
    let id = audio::speakers().subscribe(move || {
        if take() {
            runtime::idle(stop_pending);
        }
    });
    PENDING.with(|p| p.set(Some(id)));
}

thread_local! {
    static PENDING: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Stops waiting for a bluetooth sink (new connect attempt or the device disconnected).
pub fn stop_pending() {
    if let Some(id) = PENDING.with(|p| p.take()) {
        audio::speakers().unsubscribe(id);
    }
}
