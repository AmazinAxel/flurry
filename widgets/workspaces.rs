// Workspace flash on workspace change (widgets/workspaces.tsx).

use std::collections::BTreeSet;
use std::rc::Rc;

use crate::lib::json::Json;
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
}

fn update() {
    let Some(tree) = sway::tree() else { return };
    let workspaces: Vec<&Json> = tree.get("nodes").arr().iter().flat_map(|o| o.get("nodes").arr()).collect();
    OCCUPIED.with(|o| o.set(workspaces.iter().filter_map(|w| w.get("num").int()).filter(|n| (1..=9).contains(n)).collect()));
    let focus0 = tree.get("focus").arr().first().and_then(Json::int);
    let output = tree.get("nodes").arr().iter().find(|o| o.get("id").int() == focus0);
    let f = output
        .and_then(|o| {
            let wf = o.get("focus").arr().first().and_then(Json::int);
            o.get("nodes").arr().iter().find(|w| w.get("id").int() == wf).and_then(|w| w.get("num").int())
        })
        .unwrap_or(0);
    FOCUSED.with(|s| s.set(f));
    let scratch = workspaces.iter().find(|w| w.get("name").str() == Some("__i3_scratch")).map(|w| w.get("floating_nodes").arr().len()).unwrap_or(0);
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
        let full = column(&base, 1, &(1..=9).collect());
        let (w, h) = (full.measure_w().1 + full.style.margin.h(), full.measure_h(f32::MAX).1 + full.style.margin.v());
        let mut n = n.align(Align::Start, Align::Start);
        // SLIDE_RIGHT revealer hiding: content slides left out of the window
        n.style.margin.left -= (1.0 - f.progress()) * (w + SPACE_L);
        Node::hbox(base.inherit(), 0.0, vec![n]).size(w, h).align(Align::Start, Align::Start)
    });
    let w2: Rc<Win> = w.clone();
    sway::subscribe(&["workspace"], move |ev| {
        let current = ev.get("current");
        if ev.get("change").str() == Some("focus")
            && let Some(num) = current.get("num").int()
        {
            let old = ev.get("old");
            OCCUPIED.with(|o| {
                let mut next = o.peek();
                if let Some(on) = old.get("num").int()
                    && old.get("nodes").arr().is_empty()
                    && old.get("floating_nodes").arr().is_empty()
                {
                    next.remove(&on);
                }
                o.set(next);
            });
            FOCUSED.with(|s| s.set(num));
        }
        update();
        flash.show(&w2);
    });
    update();
}
