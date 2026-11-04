// Output selector with multi-sink combining.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use crate::lib::audio::{self, Sink};
use crate::ui::runtime;
use crate::ui::state::global_state;

/// An external null sink (set up outside flurry) whose monitor is linked to every selected output.
const COMBINED: &str = "combined-bt-sink";

thread_local! {
    static LINKED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static WATCHED: RefCell<HashSet<String>> = RefCell::default();
    /// the first wire after PipeWire is ready only records the sinks already there
    static FIRST_WIRE: Cell<bool> = const { Cell::new(true) };
    /// a just-connected bluetooth device's address (with `_`), until its sink shows up
    static PENDING: RefCell<Option<String>> = const { RefCell::new(None) };
}

global_state! {
    pub fn selected() -> Vec<String> = vec![];
}

fn is_combined(s: &Sink) -> bool {
    s.name == COMBINED
}
fn is_bluetooth(s: &Sink) -> bool {
    s.name.starts_with("bluez_")
}
fn speakers() -> Vec<Sink> {
    audio::speakers().peek()
}

/// The output's shown name: settings' soundNames, else its own.
pub fn name_substitute(name: &str) -> String {
    crate::settings::SOUND_NAMES.iter().find(|(k, _)| *k == name).map_or(name, |(_, v)| v).to_string()
}

fn link(target: &str, connect: bool) {
    audio::link(COMBINED, target, connect);
}

pub fn apply_selection(names: &[String]) {
    let all = speakers();
    let sinks: Vec<Sink> = names.iter().filter_map(|n| all.iter().find(|s| s.name == *n).cloned()).collect();
    if sinks.is_empty() {
        return reconcile_now();
    }
    let combined = all.into_iter().find(is_combined);
    let want: Vec<String> = if sinks.len() > 1 && combined.is_some() { sinks.iter().map(|s| s.name.clone()).collect() } else { vec![] };
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
    selected().set(if want.is_empty() { vec![sinks[0].name.clone()] } else { want });
}

fn reconcile_now() {
    let linked = LINKED.with(|l| l.borrow().clone());
    if linked.len() > 1 {
        return selected().set(linked);
    }
    match audio::default_speaker() {
        Some(def) if !is_combined(&def) => selected().set(vec![def.name]),
        // the combined sink is default with nothing linked to it: back to a real output
        Some(_) => {
            let all = speakers();
            let real = all.iter().find(|s| !is_combined(s) && !is_bluetooth(s)).or_else(|| all.iter().find(|s| !is_combined(s)));
            match real {
                Some(r) => audio::set_default(r),
                None => selected().set(vec![]),
            }
        }
        // the default is not known yet (or names no sink): WirePlumber's choice stands
        None => selected().set(vec![]),
    }
}

fn reconcile() {
    runtime::idle_once(reconcile_now);
}

fn wire() {
    if !audio::ready().peek() {
        FIRST_WIRE.with(|f| f.set(true));
        WATCHED.with(|w| w.borrow_mut().clear());
        return;
    }
    let was_first = FIRST_WIRE.with(|f| f.replace(false));
    let all = speakers();
    let present: Vec<String> = all.iter().map(|s| s.name.clone()).collect();
    let mut fresh: Option<Sink> = None;
    WATCHED.with(|w| {
        let mut w = w.borrow_mut();
        w.retain(|n| present.contains(n));
        for s in &all {
            if w.insert(s.name.clone()) && is_bluetooth(s) {
                fresh = Some(s.clone());
            }
        }
    });
    let linked = LINKED.with(|l| l.borrow().clone());
    let survivors: Vec<String> = linked.iter().filter(|n| present.contains(n)).cloned().collect();
    if survivors.len() != linked.len() {
        // apply_selection relinks only what changes
        LINKED.with(|l| *l.borrow_mut() = survivors.clone());
        if !survivors.is_empty() {
            return apply_selection(&survivors);
        }
    }
    let pending = PENDING.with(|p| p.borrow().clone());
    if let Some(s) = pending.and_then(|a| all.iter().find(|s| s.name.contains(&a))) {
        PENDING.with(|p| p.borrow_mut().take());
        return apply_selection(std::slice::from_ref(&s.name));
    }
    if let Some(f) = fresh
        && !was_first
        && LINKED.with(|l| l.borrow().len()) < 2
    {
        apply_selection(&[f.name]);
    }
    reconcile();
}

pub fn toggle_sink(s: &Sink) {
    let current = selected().peek();
    let next: Vec<String> = if current.contains(&s.name) { current.into_iter().filter(|n| *n != s.name).collect() } else { current.into_iter().chain([s.name.clone()]).collect() };
    apply_selection(&next);
}

pub fn start() {
    // isDefault changes on any sink and default-speaker changes both reconcile
    audio::default_name().subscribe(reconcile);
    let names = audio::speakers().map(|l| l.iter().map(|s| s.name.clone()).collect::<Vec<_>>());
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
    PENDING.with(|p| *p.borrow_mut() = Some(address.replace(':', "_")));
    wire();
}

/// Stops waiting for a bluetooth sink (new connect attempt or the device disconnected).
pub fn stop_pending() {
    PENDING.with(|p| p.borrow_mut().take());
}
