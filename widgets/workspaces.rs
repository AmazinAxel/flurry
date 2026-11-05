// Workspace flash on workspace change.

use std::cell::Cell;
use std::collections::BTreeSet;

use crate::lib::outTransition::Flash;
use crate::lib::style::*;
use crate::lib::sway;
use crate::ui::node::{Align, Node, Sides, Style};
use crate::ui::runtime;
use crate::ui::state::global_state;
use crate::ui::wayland::Anchor;
use crate::ui::window::{Spec, Win};

global_state! {
    pub fn focused() -> i64 = 0;
    fn occupied() -> BTreeSet<i64> = BTreeSet::new();
    fn scratchpad() -> usize = 0;
}

thread_local! {
    /// window size for a scratchpad count: the largest column (focus on one, all occupied)
    static SIZE: Cell<Option<(usize, (f32, f32))>> = const { Cell::new(None) };
    /// queued for the next idle pass: (full GET_WORKSPACES, focus flash with the scratchpad count)
    static QUEUED: Cell<Option<(bool, bool)>> = const { Cell::new(None) };
    /// a window event may have changed the scratchpad since the last GET_TREE
    static SCRATCH_DIRTY: Cell<bool> = const { Cell::new(true) };
}

/// Occupied/focused from GET_WORKSPACES (sway drops empty unfocused workspaces).
fn update() {
    let Some(list) = sway::workspaces() else { return };
    occupied().set(list.arr().iter().filter_map(|w| w.get("num").int()).filter(|n| (1..=9).contains(n)).collect());
    let f = list.arr().iter().find(|w| w.get("focused").bool() == Some(true)).and_then(|w| w.get("num").int()).unwrap_or(0);
    focused().set(f);
}

/// Applies what the event says itself (focus, a workspace created or emptied); anything else
/// re-reads the list. Returns whether the list must be re-read.
fn apply_event(ev: &crate::lib::json::Json) -> bool {
    let num = |k: &str| ev.get(k).get("num").int();
    let slot = |k: &str| num(k).filter(|n| (1..=9).contains(n));
    let change = ev.get("change").str();
    match change {
        Some("focus" | "init") => {
            if let Some(n) = slot("current") {
                occupied().update(|o| o.insert(n));
            }
            if change == Some("focus") {
                focused().set(num("current").unwrap_or(0));
            }
            false
        }
        Some("empty") => {
            if let Some(n) = slot("current") {
                occupied().update(|o| o.remove(&n));
            }
            false
        }
        Some("urgent") => false,
        _ => true,
    }
}

/// The scratchpad count needs the tree; it only shows with the flash on a focus change, and is
/// re-read only after a window moved, closed or changed floating.
fn update_scratchpad() {
    if !SCRATCH_DIRTY.with(|d| d.replace(false)) {
        return;
    }
    let Some(tree) = sway::tree() else { return };
    let scratch = tree
        .get("nodes")
        .arr()
        .iter()
        .flat_map(|o| o.get("nodes").arr())
        .find(|w| w.get("name").str() == Some("__i3_scratch"))
        .map(|w| w.get("floating_nodes").arr().len())
        .unwrap_or(0);
    scratchpad().set(scratch);
}

fn column(base: &Style, focused: i64, occupied: &BTreeSet<i64>) -> Node {
    let mut col = status_element(base);
    col.padding = Sides::vh(SPACE_S, 0.0);
    col.margin = Sides::trbl(SWAY_GAP, SPACE_L, SPACE_L, SWAY_GAP);
    let mut kids: Vec<Node> = (1..=9)
        .map(|id| {
            let mut s = Style { min_width: 21.0, min_height: 26.0, margin: Sides::vh(SPACE_S, SPACE_M), background: Some(dark2()), ..base.inherit() };
            if focused == id {
                s.background = Some(blue2());
                s.min_height = 34.0;
            } else if occupied.contains(&id) {
                s.background = Some(dark4());
                s.min_height = 30.0;
            }
            Node::hbox(s, 0.0, vec![])
        })
        .collect();
    let count = scratchpad().peek();
    let mut l = base.inherit();
    l.opacity = 0.5;
    l.margin = Sides::vh(SPACE_S, SPACE_M);
    kids.push(Node::label(l, count.to_string()).show(count > 0));
    Node::vbox(col, 0.0, kids)
}

pub fn workspaces() {
    let flash = Flash::new(400, 150.0);
    let f = flash.clone();
    let w = Win::new(Spec { name: "workspaces", anchor: Anchor::Top | Anchor::Left, per_monitor: true, ..Default::default() }, move |_| {
        let base = window();
        let focused = focused().peek();
        let n = column(&base, focused, &occupied().peek());
        // the surface keeps its largest size (focus on one, all occupied) and the slide only moves
        // content inside it: resizing the layer per frame or per switch took compositor round trips
        let count = scratchpad().peek();
        let (w, h) = match SIZE.with(Cell::get) {
            Some((c, size)) if c == count => size,
            _ => {
                let full = column(&base, 1, &(1..=9).collect());
                let size = (full.measure_w().1 + full.style.margin.h(), full.measure_h(f32::MAX).1 + full.style.margin.v());
                SIZE.with(|s| s.set(Some((count, size))));
                size
            }
        };
        let mut n = n.align(Align::Start, Align::Start);
        // SLIDE_RIGHT revealer hiding: content slides left out of the window
        n.style.margin.left -= (1.0 - f.progress()) * (w + SPACE_L);
        Node::hbox(base.inherit(), 0.0, vec![n]).size(w, h).align(Align::Start, Align::Start)
    });
    // a burst of events (a workspace switch sends several) is one read on the next idle pass
    sway::subscribe(&["workspace", "window"], move |ev| {
        // window events carry the container, workspace events don't
        if ev.get("container").get("id").int().is_some() {
            if matches!(ev.get("change").str(), Some("move" | "close" | "floating")) {
                SCRATCH_DIRTY.with(|d| d.set(true));
            }
            return;
        }
        let full = apply_event(ev);
        let focus = ev.get("change").str() == Some("focus");
        let queued = QUEUED.with(Cell::get);
        QUEUED.with(|q| q.set(Some(queued.map_or((full, focus), |(a, b)| (a || full, b || focus)))));
        if queued.is_some() {
            return;
        }
        let (flash, w) = (flash.clone(), w.clone());
        runtime::idle(move || {
            let Some((full, focus)) = QUEUED.with(Cell::take) else { return };
            if full {
                update();
            }
            if focus {
                update_scratchpad();
                flash.show(&w);
            }
        });
    });
    update();
    update_scratchpad();
}
