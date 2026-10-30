use crate::lib::style::*;
use crate::lib::time::format_now;
use crate::ui::node::{Align, Node, Sides, Style};

// Iosevka metrics (em): 0.5 advance, ~0.05 side bearings on digits, ink 0.75 tall sitting 0.028
// above the middle of its 1.25 line box. Rows get a 0.75em line box shifted onto the ink and the
// outer bearings are trimmed, so every node edge is an ink edge and spacing is exactly the gaps.
const INK: f32 = 0.75;
const RAISE: f32 = 0.028;
pub const DIGIT_BEARING: f32 = 0.05;
/// "Fri" etc. at the weekday size: ink width per em, and its bearings
pub const DAY_INK: f32 = 1.33;
const DAY_BEARING: (f32, f32) = (0.096, 0.075);

fn row(base: &Style, s: String, size: f32, weight: u16, bearing: (f32, f32)) -> Node {
    let mut t = base.inherit();
    t.font.family = IOSEVKA;
    t.font.size = size;
    t.font.weight = weight;
    t.line_height = Some(INK * size);
    t.margin = Sides::trbl(RAISE * size, -bearing.1 * size, -RAISE * size, -bearing.0 * size);
    t.color = blue2();
    t.text_shadow = vec![shadow_(), glow(8.0, blue3())];
    Node::label(t, s).align(Align::Center, Align::Start)
}

/// Hours over minutes, rows `gap` apart.
pub fn clock(base: &Style, size: f32, gap: f32) -> Node {
    let b = (DIGIT_BEARING, DIGIT_BEARING);
    Node::vbox(base.inherit(), gap, vec![row(base, format_now("%H"), size, 700, b), row(base, format_now("%M"), size, 700, b)])
}

/// Month over day over weekday, rows `gap` apart.
pub fn date(base: &Style, size: f32, day_size: f32, gap: f32) -> Node {
    let b = (DIGIT_BEARING, DIGIT_BEARING);
    Node::vbox(base.inherit(), gap, vec![
        row(base, format_now("%m"), size, 400, b),
        row(base, format_now("%d"), size, 400, b),
        row(base, format_now("%a"), day_size, 400, DAY_BEARING),
    ])
}

/// Ink height of the clock and date columns.
pub fn clock_height(size: f32, gap: f32) -> f32 {
    2.0 * INK * size + gap
}
pub fn date_height(size: f32, day_size: f32, gap: f32) -> f32 {
    2.0 * INK * size + INK * day_size + 2.0 * gap
}
