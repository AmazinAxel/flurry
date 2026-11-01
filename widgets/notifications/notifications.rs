// Notification popups (widgets/notifications/*.tsx).

use std::cell::RefCell;

use crate::lib::notifd::{self, Notification};
use crate::lib::state::State;
use crate::lib::streamingMode::streaming_mode;
use crate::lib::style::*;
use crate::lib::sway;
use crate::ui::node::{Align, Node, Sides, Style};
use crate::ui::wayland::{Anchor, Layer};
use crate::ui::window::{Spec, Win};
use crate::widgets::focus::{focus, focus_text};

/// shown at once, newest first
const MAX_SHOWN: usize = 10;

thread_local! {
    /// shown notifications, oldest first (Map insertion order)
    static SHOWN: RefCell<Vec<Notification>> = const { RefCell::new(Vec::new()) };
    static LIST: State<Vec<Notification>> = State::new(vec![]);
}

/// Dropped for good (settings filters).
fn filtered(n: &Notification) -> bool {
    crate::settings::NOTIFICATION_FILTERS.iter().any(|f| n.body.starts_with(f))
}

/// Hidden while streaming or focusing; flurry's own notifications still show.
fn hidden(n: &Notification) -> bool {
    !n.local && (streaming_mode().peek() || focus())
}

fn refresh() {
    let list: Vec<Notification> = SHOWN.with(|s| s.borrow().iter().rev().filter(|n| !hidden(n)).take(MAX_SHOWN).cloned().collect());
    LIST.with(|l| l.set(list));
}

fn item(base: &Style, n: &Notification) -> Node {
    let mut card = status_element(base);
    card.margin = Sides::trbl(SWAY_GAP, SWAY_GAP, 0.0, SPACE_L);
    // not an active widget: gray
    card.border_color = [dark4(); 4];
    card.min_width = 280.0;
    card.max_width = Some(280.0);
    let mut summary = base.inherit();
    summary.font.size = 18.0;
    summary.font.family = SORA;
    summary.font.weight = 700;
    let mut kids = vec![];
    if !n.summary.trim().is_empty() {
        kids.push(Node::label(summary, n.summary.clone()).wrap(30.0).xalign(0.0).align(Align::Start, Align::Fill));
    }
    if !n.body.trim().is_empty() {
        kids.push(Node::label(base.inherit(), n.body.clone()).wrap(30.0).xalign(0.0));
    }
    if !n.actions.is_empty() {
        let s = Style { padding: Sides::trbl(SPACE_M, 0.0, 0.0, 0.0), ..base.inherit() };
        let buttons = n
            .actions
            .iter()
            .map(|(_, label)| {
                let b = button(base);
                Node::hbox(b.clone(), 0.0, vec![Node::label(b.inherit(), label.replace("Activate", "Open")).ellipsize_natural().expand(true, false)]).expand(true, false)
            })
            .collect();
        kids.push(Node::hbox(s, SPACE_S, buttons));
    }
    // less on top: the line box already adds space over the text
    Node::vbox(card, 0.0, vec![Node::hbox(Style { padding: Sides::trbl(SPACE_L - SPACE_S, SPACE_L, SPACE_L, SPACE_L), ..base.inherit() }, 0.0, vec![Node::vbox(base.inherit(), SPACE_S, kids)])])
}

pub fn notifications() {
    let w = Win::new(Spec { name: "notifications", anchor: Anchor::Top | Anchor::Right, layer: Layer::Top, per_monitor: true, ..Default::default() }, |_| {
        let base = window();
        let list = LIST.with(|l| l.peek());
        Node::vbox(Style { padding: Sides::trbl(0.0, 0.0, SPACE_L, 0.0), ..base.inherit() }, 0.0, list.iter().map(|n| item(&base, n)).collect()).size(200.0, 0.0)
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
        if filtered(&n) || hidden(&n) {
            return notifd::dismiss(id);
        }
        SHOWN.with(|s| {
            let mut s = s.borrow_mut();
            match s.iter_mut().find(|x| x.id == id) {
                Some(slot) => *slot = n,
                None => s.push(n),
            }
        });
        refresh();
    });
    notifd::on_resolved(|id, _| {
        let removed = SHOWN.with(|s| {
            let mut s = s.borrow_mut();
            let before = s.len();
            s.retain(|x| x.id != id);
            before != s.len()
        });
        if removed {
            refresh();
        }
    });
    streaming_mode().subscribe(refresh);
    focus_text().subscribe(refresh);
}

fn oldest() -> Option<Notification> {
    SHOWN.with(|s| s.borrow().iter().find(|n| !hidden(n)).cloned())
}

pub fn clear_oldest_notification() {
    if let Some(n) = oldest() {
        notifd::dismiss(n.id);
    }
}

/// Window app ids are plain names; anything else could inject sway criteria.
fn safe_app_id(app: &str) -> bool {
    !app.is_empty() && app.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn invoke_oldest_notification(i: usize) {
    let Some(n) = oldest() else { return };
    let Some((action, _)) = n.actions.get(i).cloned() else { return };
    notifd::invoke(n.id, &action);
    let app = if n.desktop_entry.is_empty() { &n.app_name } else { &n.desktop_entry };
    if action == "default" && safe_app_id(app) {
        sway::command(&format!("[app_id=\"{app}\"] focus"));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn app_id_charset() {
        assert!(super::safe_app_id("org.mozilla.firefox"));
        assert!(super::safe_app_id("foot_2-x"));
        assert!(!super::safe_app_id(""));
        assert!(!super::safe_app_id("x\"] exec rm"));
        assert!(!super::safe_app_id("a b"));
    }
}
