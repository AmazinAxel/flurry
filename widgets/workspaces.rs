// Workspace flash on workspace change (widgets/workspaces.tsx).

use std::cell::Cell;
use std::collections::BTreeSet;
use std::rc::Rc;

use crate::lib::outTransition::Flash;
use crate::lib::state::State;
use crate::lib::style::*;
use crate::lib::sway;
use crate::ui::node::{Align, Background, Node, Sides, Style};
use crate::ui::wayland::Anchor;
use crate::ui::window::{Spec, Win};

thread_local! {
    static FOCUSED: State<i64> = State::new(0);
    static OCCUPIED: State<BTreeSet<i64>> = State::new(BTreeSet::new());
    static SCRATCHPAD: State<usize> = State::new(0);
    /// window size for a scratchpad count: the largest column (focus on one, all occupied)
    static SIZE: Cell<Option<(usize, (f32, f32))>> = const { Cell::new(None) };
}

/// Occupied/focused from GET_WORKSPACES (sway drops empty unfocused workspaces).
fn update() {
    let Some(list) = sway::workspaces() else { return };
    OCCUPIED.with(|o| o.set(list.arr().iter().filter_map(|w| w.get("num").int()).filter(|n| (1..=9).contains(n)).collect()));
    let f = list.arr().iter().find(|w| w.get("focused").bool() == Some(true)).and_then(|w| w.get("num").int()).unwrap_or(0);
    FOCUSED.with(|s| s.set(f));
}

/// The scratchpad count needs the tree; it only shows with the flash on a focus change.
fn update_scratchpad() {
    let Some(tree) = sway::tree() else { return };
    let scratch = tree
        .get("nodes")
        .arr()
        .iter()
        .flat_map(|o| o.get("nodes").arr())
        .find(|w| w.get("name").str() == Some("__i3_scratch"))
        .map(|w| w.get("floating_nodes").arr().len())
        .unwrap_or(0);
    SCRATCHPAD.with(|s| s.set(scratch));
}

fn column(base: &Style, focused: i64, occupied: &BTreeSet<i64>) -> Node {
    let mut col = status_element(base);
    col.padding = Sides::vh(SPACE_S, 0.0);
    col.margin = Sides::trbl(SWAY_GAP, SPACE_L, SPACE_L, SWAY_GAP);
    let mut kids: Vec<Node> = (1..=9)
        .map(|id| {
            let mut s = Style { min_width: 21.0, min_height: 26.0, margin: Sides::vh(SPACE_S, SPACE_M), background: Background::Color(dark2()), ..base.inherit() };
            if focused == id {
                s.background = Background::Color(blue2());
                s.min_height = 34.0;
            } else if occupied.contains(&id) {
                s.background = Background::Color(dark4());
                s.min_height = 30.0;
            }
            Node::hbox(s, 0.0, vec![])
        })
        .collect();
    let count = SCRATCHPAD.with(|s| s.peek());
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
        let focused = FOCUSED.with(|s| s.peek());
        let n = column(&base, focused, &OCCUPIED.with(|s| s.peek()));
        // the surface keeps its largest size (focus on one, all occupied) and the slide only moves
        // content inside it: resizing the layer per frame or per switch took compositor round trips
        let count = SCRATCHPAD.with(|s| s.peek());
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
    let w2: Rc<Win> = w.clone();
    sway::subscribe(&["workspace"], move |ev| {
        update();
        if ev.get("change").str() == Some("focus") {
            update_scratchpad();
            flash.show(&w2);
        }
    });
    update();
    update_scratchpad();
}
