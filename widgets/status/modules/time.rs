use crate::lib::style::*;
use crate::lib::time::format_now;
use crate::ui::node::{Align, Node, Style};

fn text(base: &Style, s: String, size: f32, weight: u16) -> Node {
    let mut t = base.inherit();
    t.font.family = IOSEVKA;
    t.font.size = size;
    t.font.weight = weight;
    t.line_height = Some(size);
    t.color = blue2();
    t.text_shadow = vec![shadow_(), glow(8.0, blue3())];
    Node::label(t, s)
}

/// Hours over minutes.
pub fn clock(base: &Style) -> Node {
    Node::vbox(base.inherit(), 6.0, vec![text(base, format_now("%H"), 72.0, 700), text(base, format_now("%M"), 72.0, 700)]).align(Align::Start, Align::Center)
}

/// Month over day over weekday.
pub fn date(base: &Style) -> Node {
    Node::vbox(base.inherit(), 6.0, vec![text(base, format_now("%m"), 40.0, 400), text(base, format_now("%d"), 40.0, 400), text(base, format_now("%a"), 28.0, 400)])
        .align(Align::End, Align::Center)
}
