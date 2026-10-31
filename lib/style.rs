// style.css + the Graphite-Dark-nord rules that reach flurry's widgets, resolved to values.

use tiny_skia::Color;

use crate::ui::node::{Background, Shadow, Sides, Style, alpha, hex, rgba, shadow};
use crate::ui::text::Font;

pub fn dark1() -> Color { hex(0x2e3440) }
pub fn dark2() -> Color { hex(0x3b4252) }
pub fn dark4() -> Color { hex(0x4c566a) }
pub fn light1() -> Color { hex(0xeceff4) }
pub fn light3() -> Color { hex(0xd8dee9) }
pub fn red() -> Color { hex(0xbf616a) }
pub fn orange() -> Color { hex(0xd08770) }
pub fn yellow() -> Color { hex(0xebcb8b) }
pub fn green() -> Color { hex(0xa3be8c) }
pub fn purple() -> Color { hex(0xb48ead) }
pub fn blue1() -> Color { hex(0xb5d2d9) }
pub fn blue2() -> Color { hex(0x81a1c1) }
pub fn blue3() -> Color { hex(0x5e81ac) }
pub fn blue4() -> Color { hex(0x8fbcbb) }

/// Every border's width.
pub const BORDER: f32 = 3.0;
/// Spacing scale: between items, inside controls, inside cards and around them.
pub const SPACE_S: f32 = 4.0;
pub const SPACE_M: f32 = 8.0;
pub const SPACE_L: f32 = 16.0;
/// Gap of the windows sway tiles: edge-anchored widgets line up with them.
pub const SWAY_GAP: f32 = 5.0;
/// Blur of every bloom.

pub const BLOOM: f32 = 8.0;

/// `c` a sixth of the way to white: a border that reads against its own fill.
pub fn lighter(c: Color) -> Color {
    let up = |v: f32| v + (1.0 - v) / 6.0;
    Color::from_rgba(up(c.red()), up(c.green()), up(c.blue()), c.alpha()).unwrap_or(c)
}

/// A glyph's glow, in whatever color it is drawn.
pub fn own_bloom() -> Shadow {
    Shadow { own_color: true, ..glow(BLOOM, alpha(light1(), 0.6)) }
}

pub fn glow(blur: f32, c: Color) -> Shadow {
    shadow(0.0, 0.0, blur, c)
}

pub fn font(family: &'static str, size: f32) -> Font {
    Font { family, weight: 400, size, letter_spacing: 0.0 }
}

pub const SORA: &str = "Sora";
pub const IOSEVKA: &str = "Iosevka Nerd Font Mono";

/// `window`: Sora 16px, @light1, text/icon bloom.
pub fn window() -> Style {
    Style {
        color: light1(),
        font: font(SORA, 16.0),
        text_shadow: vec![own_bloom()],
        icon_shadow: vec![own_bloom()],
        ..Style::default()
    }
}

/// `.statusElement`
pub fn status_element(base: &Style) -> Style {
    Style { background: Background::Color(dark1()), margin: Sides::all(SPACE_L), ..base.inherit() }
        .border(Sides::all(BORDER), blue3())
}

/// `.widgetBackground`
pub fn widget_background(base: &Style) -> Style {
    Style { background: Background::Color(dark1()), padding: Sides::all(SPACE_L), ..base.inherit() }.border(Sides::all(BORDER), blue3())
}

/// Graphite `button` (border from style.css, square corners from `* { border-radius: 0 }`)
pub fn button(base: &Style) -> Style {
    let mut s = Style {
        background: Background::Color(rgba(255, 255, 255, 0.04)),
        padding: Sides::all(SPACE_M),
        min_height: 16.0, // a 16px icon's button comes out square
        min_width: 16.0,
        ..base.inherit()
    }
    .border(Sides::all(BORDER), dark4());
    s.font.weight = 500;
    s
}

/// Frosted look for anything over the tinted wallpaper (menu buttons, calendar): white text on
/// translucent black; blue marks the active/selected item.
pub fn frosted(mut s: Style) -> Style {
    s.background = Background::Color(alpha(dark1(), 0.6));
    s.color = light1();
    s
}

/// `button.active, .button.selected, list row:selected, ...`
pub fn button_active(mut s: Style) -> Style {
    s.background = Background::Color(blue3());
    s.border_color = [blue3(); 4];
    s
}

/// `button:focus` outline
pub fn focus_outline(mut s: Style) -> Style {
    s.outline = Some((BORDER, 0.0, blue2()));
    s
}

