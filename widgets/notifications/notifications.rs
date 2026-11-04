// Notification popups.

use std::cell::Cell;

use crate::lib::notifd::{self, Notification};
use crate::lib::streamingMode::streaming_mode;
use crate::lib::style::*;
use crate::lib::sway;
use crate::ui::node::{Align, Node, Sides, Style};
use crate::ui::state::global_state;
use crate::ui::wayland::{Anchor, Layer};
use crate::ui::window::{Spec, Win};
use crate::widgets::focus::{focus, focus_text};

/// shown at once, newest first
const MAX_SHOWN: usize = 10;
/// wrapped lines shown of the summary and the body
const SUMMARY_LINES: usize = 2;
const BODY_LINES: usize = 6;

global_state! {
    fn shown() -> Vec<Notification> = vec![];
}

thread_local! {
    /// the shortest usable output height: the cards scroll past it instead of outgrowing the output
    static MAX_HEIGHT: Cell<f32> = const { Cell::new(f32::MAX) };
    /// workspaces or outputs changed since MAX_HEIGHT was read
    static HEIGHT_STALE: Cell<bool> = const { Cell::new(true) };
    /// a foot notification was activated; foot marks its window urgent in response
    static AWAIT_URGENT: Cell<bool> = const { Cell::new(false) };
}

/// Dropped for good (settings filters).
fn filtered(n: &Notification) -> bool {
    crate::settings::NOTIFICATION_FILTERS.iter().any(|f| n.body.starts_with(f))
}

/// Hidden while streaming or focusing; flurry's own notifications still show.
fn hidden(n: &Notification) -> bool {
    !n.local && (streaming_mode().peek() || focus())
}

/// One arriving while hidden (streaming, focusing) is kept by notifd and shows once that ends.
fn refresh() {
    let list = notifd::newest(|n| !hidden(n), MAX_SHOWN);
    if !list.is_empty() && HEIGHT_STALE.with(|s| s.replace(false)) {
        // usable area (bars excluded) of the visible workspaces
        let h = sway::workspaces().and_then(|ws| {
            ws.arr().iter().filter(|w| w.get("visible").bool() == Some(true)).filter_map(|w| w.get("rect").get("height").int()).min()
        });
        MAX_HEIGHT.with(|m| m.set(h.map_or(f32::MAX, |h| (h as f32 - SPACE_L).max(0.0))));
    }
    shown().set(list);
}

/// `text` with runs of line breaks (and blank lines) collapsed into one.
fn collapse_lines(text: &str) -> String {
    text.split('\n').map(|l| l.trim_end_matches('\r')).filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join("\n")
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
        kids.push(Node::label(summary, collapse_lines(&n.summary)).wrap(30.0).max_lines(SUMMARY_LINES).xalign(0.0).align(Align::Start, Align::Fill));
    }
    if !n.body.trim().is_empty() {
        kids.push(Node::label(base.inherit(), collapse_lines(&n.body)).wrap(30.0).max_lines(BODY_LINES).xalign(0.0));
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
        let list = shown().peek();
        let cards = Node::vbox(base.inherit(), 0.0, list.iter().map(|n| item(&base, n)).collect());
        let max = MAX_HEIGHT.with(Cell::get);
        Node::vbox(Style { padding: Sides::trbl(0.0, 0.0, SPACE_L, 0.0), ..base.inherit() }, 0.0, vec![Node::scroll(base.inherit(), max, None, cards)]).size(200.0, 0.0)
    });
    let ww = w;
    shown().subscribe(move || {
        ww.set_visible(!shown().with(Vec::is_empty));
        ww.redraw();
    });
    sway::subscribe(&["window"], |ev| match ev.get("change").str() {
        Some("urgent") if ev.get("container").get("urgent").bool() == Some(true) && AWAIT_URGENT.with(|a| a.take()) => {
            if let Some(id) = ev.get("container").get("id").int() {
                sway::command(&format!("[con_id={id}] focus"));
            }
        }
        // foot's window was already focused, so no urgent event is coming
        Some("focus") => AWAIT_URGENT.with(|a| a.set(false)),
        _ => {}
    });
    sway::subscribe(&["workspace", "output"], |_| HEIGHT_STALE.with(|s| s.set(true)));
    notifd::on_notified(|id| {
        let Some(n) = notifd::get(id) else { return };
        // dropped unseen: expired, not dismissed by the user
        if filtered(&n) {
            return notifd::resolve(id, notifd::Reason::Expired);
        }
        refresh();
    });
    notifd::on_resolved(|id, _| {
        if shown().with(|l| l.iter().any(|n| n.id == id)) {
            refresh();
        }
    });
    streaming_mode().subscribe(refresh);
    focus_text().subscribe(refresh);
}

/// The oldest card on screen (the list is newest first).
fn oldest() -> Option<Notification> {
    shown().with(|l| l.last().cloned())
}

pub fn clear_oldest_notification() {
    if let Some(n) = oldest() {
        notifd::dismiss(n.id);
    }
}

pub fn invoke_oldest_notification(i: usize) {
    let Some(n) = oldest() else { return };
    let Some((action, _)) = n.actions.get(i).cloned() else { return };
    let app = if n.desktop_entry.is_empty() { &n.app_name } else { &n.desktop_entry };
    let foot = action == "default" && app.starts_with("foot");
    AWAIT_URGENT.with(|a| a.set(foot));
    notifd::invoke(n.id, &action);
    // window app ids are plain names; anything else could inject sway criteria
    if action == "default" && !foot && notifd::safe_app_id(app) {
        sway::command(&format!("[app_id=\"{app}\"] focus"));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn line_collapse() {
        assert_eq!(super::collapse_lines("a\n\n\r\n  \nb\r\nc"), "a\nb\nc");
    }
}
