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

/// --shadow: 0 1px 3px alpha(@dark1, 0.8)
pub fn shadow_() -> Shadow {
    shadow(0.0, 1.0, 3.0, alpha(dark1(), 0.8))
}

pub fn glow(blur: f32, c: Color) -> Shadow {
    shadow(0.0, 0.0, blur, c)
}

pub fn font(family: &'static str, size: f32) -> Font {
    Font { family, weight: 400, size, letter_spacing: 0.0 }
}

pub const SORA: &str = "Sora";
pub const HAMMERSMITH: &str = "Hammersmith One";
pub const IOSEVKA: &str = "Iosevka Nerd Font Mono";

/// `window`: Sora 16px, @light1, text/icon shadow.
pub fn window() -> Style {
    Style {
        color: light1(),
        font: font(SORA, 16.0),
        text_shadow: vec![shadow_()],
        icon_shadow: vec![shadow_()],
        ..Style::default()
    }
}

/// `.statusElement`
pub fn status_element(base: &Style) -> Style {
    Style { background: Background::Color(dark1()), margin: Sides::trbl(0.0, 0.0, 5.0, 5.0), ..base.inherit() }
        .border(Sides::all(3.0), dark4())
}

/// `.widgetBackground`
pub fn widget_background(base: &Style) -> Style {
    Style { background: Background::Color(dark1()), padding: Sides::all(16.0), ..base.inherit() }.border(Sides::all(3.0), blue3())
}

/// Graphite `button` (border from style.css, square corners from `* { border-radius: 0 }`)
pub fn button(base: &Style) -> Style {
    let mut s = Style {
        background: Background::Color(rgba(255, 255, 255, 0.04)),
        padding: Sides::vh(6.0, 10.0),
        min_height: 24.0,
        min_width: 16.0,
        ..base.inherit()
    }
    .border(Sides::all(1.0), dark4());
    s.font.weight = 500;
    s
}

/// Frosted look for anything over the tinted wallpaper (menu buttons, calendar): white text on
/// translucent black, no outline; blue marks the active/selected item.
pub fn frosted(mut s: Style) -> Style {
    s.background = Background::Color(alpha(dark1(), 0.6));
    s.border = Sides::all(0.0);
    s.color = light1();
    s.text_shadow = vec![shadow(0.0, 2.0, 6.0, alpha(dark1(), 0.6))];
    s.icon_shadow = s.text_shadow.clone();
    s
}

/// `button.active, .button.selected, list row:selected, ...`
pub fn button_active(mut s: Style) -> Style {
    s.background = Background::Color(blue2());
    s.box_shadow = vec![Shadow { spread: 3.0, ..glow(5.0, blue3()) }];
    s.border_color = [blue1(); 4];
    s
}

/// `button:focus` outline
pub fn focus_outline(mut s: Style) -> Style {
    s.outline = Some((2.0, 1.0, blue2()));
    s
}

