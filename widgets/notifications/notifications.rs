// Notification popups (widgets/notifications/*.tsx).

use std::cell::RefCell;

use crate::lib::notifd::{self, Notification};
use crate::lib::state::State;
use crate::lib::streamingMode::streaming_mode;
use crate::lib::style::*;
use crate::lib::sway;
use crate::lib::time::format;
use crate::ui::node::{Align, Background, Node, Sides, Style};
use crate::ui::wayland::{Anchor, Layer};
use crate::ui::window::{Spec, Win};
use crate::widgets::focus::focus;

const AGS_APPS: [&str; 6] = ["Low battery", "Focus", "Pass", "Compress", "Sync", "Cleanup"];

thread_local! {
    /// shown notifications, oldest first (Map insertion order)
    static SHOWN: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static LIST: State<Vec<Notification>> = State::new(vec![]);
}

fn hidden(n: &Notification) -> bool {
    ((streaming_mode().peek() || focus()) && !AGS_APPS.contains(&n.app_name.as_str())) || n.body.starts_with("Failed to connect to server")
}

fn refresh() {
    let list: Vec<Notification> = SHOWN.with(|s| s.borrow().iter().rev().filter_map(|id| notifd::get(*id)).collect());
    LIST.with(|l| l.set(list));
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

fn item(base: &Style, n: &Notification) -> Node {
    let mut card = status_element(base);
    card.margin = Sides::trbl(5.0, 5.0, 0.0, 0.0);
    card.min_width = 280.0;
    card.max_width = Some(280.0);
    let mut header = base.inherit();
    header.background = Background::Color(dark2());
    header.padding = Sides::all(8.0);
    header = header.border(Sides::trbl(0.0, 0.0, 3.0, 0.0), blue3());
    let mut app = header.inherit();
    app.font.family = HAMMERSMITH;
    let mut summary = base.inherit();
    summary.font.size = 18.0;
    summary.font.family = HAMMERSMITH;
    let mut kids = vec![];
    if !n.summary.trim().is_empty() {
        kids.push(Node::label(summary, n.summary.clone()).wrap(30.0).xalign(0.0).align(Align::Start, Align::Fill));
    }
    if !n.body.trim().is_empty() {
        kids.push(Node::label(base.inherit(), n.body.clone()).wrap(30.0).xalign(0.0));
    }
    if !n.actions.is_empty() {
        let s = Style { padding: Sides::trbl(11.0, 0.0, 0.0, 0.0), ..base.inherit() };
        let buttons = n
            .actions
            .iter()
            .map(|(_, label)| {
                let b = button(base);
                Node::hbox(b.clone(), 0.0, vec![Node::label(b.inherit(), label.replace("Activate", "Open")).ellipsize_natural().expand(true, false)]).expand(true, false)
            })
            .collect();
        kids.push(Node::hbox(s, 5.0, buttons));
    }
    Node::vbox(card, 0.0, vec![
        Node::hbox(header, 0.0, vec![
            Node::label(app, capitalize(if n.app_name.is_empty() { "Unknown" } else { &n.app_name })).ellipsize_natural().align(Align::Start, Align::Fill),
            Node::label(base.inherit(), format(n.time, "%H:%M")).expand(true, false).align(Align::End, Align::Fill),
        ]),
        Node::hbox(Style { padding: Sides::all(11.0), ..base.inherit() }, 0.0, vec![Node::vbox(base.inherit(), 0.0, kids)]),
    ])
}

pub fn notifications() {
    let w = Win::new(Spec { name: "notifications", anchor: Anchor::Top | Anchor::Right, layer: Layer::Top, per_monitor: true, ..Default::default() }, |_| {
        let base = window();
        let list = LIST.with(|l| l.peek());
        Node::vbox(base.inherit(), 0.0, list.iter().map(|n| item(&base, n)).collect()).size(200.0, 0.0)
    });
    let ww = w.clone();
    LIST.with(|l| {
        let l2 = l.clone();
        l.subscribe(move || {
            ww.set_visible(!l2.with(Vec::is_empty));
            ww.redraw();
        })
    });
    notifd::on_notified(|id, _| {
        let Some(n) = notifd::get(id) else { return };
        if hidden(&n) {
            return notifd::dismiss(id);
        }
        SHOWN.with(|s| {
            let mut s = s.borrow_mut();
            if !s.contains(&id) {
                s.push(id);
            }
        });
        refresh();
    });
    notifd::on_resolved(|id, _| {
        let removed = SHOWN.with(|s| {
            let mut s = s.borrow_mut();
            let before = s.len();
            s.retain(|x| *x != id);
            before != s.len()
        });
        if removed {
            refresh();
        }
    });
}

fn oldest() -> Option<Notification> {
    SHOWN.with(|s| s.borrow().first().copied()).and_then(notifd::get)
}

pub fn clear_oldest_notification() {
    if let Some(n) = oldest() {
        notifd::dismiss(n.id);
    }
}

pub fn invoke_oldest_notification(i: usize) {
    let Some(n) = oldest() else { return };
    let Some((action, _)) = n.actions.get(i).cloned() else { return };
    notifd::invoke(n.id, &action);
    let app = if n.desktop_entry.is_empty() { &n.app_name } else { &n.desktop_entry };
    if action == "default" && !app.is_empty() {
        sway::command(&format!("[app_id=\"{app}\"] focus"));
    }
    if !n.resident {
        notifd::dismiss(n.id);
    }
}
