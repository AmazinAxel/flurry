// Wallpaper-backed header card (lib/backgroundSection.tsx).

use crate::lib::style::*;
use crate::lib::wallpaper;
use crate::ui::node::{Align, Background, Node, Sides, Style, alpha, rgba};

/// `.backgroundSection .header:first-child` text/icon look
pub fn header_style(base: &Style) -> Style {
    let mut s = base.inherit();
    s.font.family = SORA;
    s.font.weight = 700;
    s.font.size = 32.0;
    s.color = light3();
    s.text_shadow = vec![shadow_(), glow(10.0, light3())];
    s.icon_shadow = s.text_shadow.clone();
    s.line_height = Some(19.0);
    s
}

pub fn background_section(base: &Style, header: Node, content: Option<Node>, height: f32, width: f32) -> Node {
    let mut section = widget_background(base);
    section.margin = Sides::all(16.0);
    section.line_height = Some(19.0);
    let hs = header_style(&section);
    let mut overlay_style = hs.clone();
    overlay_style.outline = Some((2.0, -2.0, rgba(255, 255, 255, 0x1c as f32 / 255.0)));
    let filler = Node::hbox(Style { padding: Sides::trbl(48.0, 48.0, 48.0, 0.0), min_height: 32.0, ..hs.inherit() }, 0.0, vec![]);
    // the lockscreen's dark tint keeps light text readable on any wallpaper
    let tint = Node::hbox(Style { background: Background::Color(alpha(dark1(), 0.45)), ..hs.inherit() }, 0.0, vec![]);
    let header = Node::overlay(overlay_style, vec![filler, wallpaper::picture(hs.inherit(), 3.0), tint, header]);
    let mut kids = vec![header];
    kids.extend(content);
    let card = Node::vbox(section, 5.0, kids).size(width, 0.0).align(Align::Fill, Align::Start);
    Node::hbox(base.inherit(), 0.0, vec![card]).size(0.0, height).align(Align::Center, Align::Center)
}
