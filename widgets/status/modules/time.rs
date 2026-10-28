use crate::lib::style::*;
use crate::lib::time::format_now;
use crate::ui::node::{Align, Node, Sides, Style};

pub fn time(base: &Style) -> Node {
    let mut wrap = base.inherit();
    wrap.font.family = WQY;
    wrap.padding = Sides::vh(6.0, 0.0);
    let date = |text: String, bottom: bool, day: bool| {
        let mut s = wrap.inherit();
        s.color = blue2();
        s.text_shadow = vec![shadow_(), glow(8.0, blue3())];
        s.font.size = if day { 18.0 } else { 22.0 };
        s.line_height = Some(26.0);
        s.margin = Sides::trbl(if day { 1.0 } else { 0.0 }, 0.0, if bottom { 6.0 } else { 0.0 }, 0.0);
        Node::label(s, text)
    };
    let mut t = wrap.inherit();
    t.font.size = 24.0;
    t.font.weight = 700;
    t.font.letter_spacing = 2.0;
    t.color = blue2();
    t.text_shadow = vec![shadow_(), glow(5.0, blue3())];
    t.padding = Sides::vh(0.0, 5.0);
    Node::vbox(wrap.clone(), 0.0, vec![
        date(format_now("%m"), false, false),
        date(format_now("%d"), true, false),
        Node::label(t, format_now("%H\n%M")),
        date(format_now("%a"), false, true),
    ])
    .expand(true, false)
    .align(Align::Fill, Align::Fill)
}
