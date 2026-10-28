use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::backgroundSection::{background_section, header_style};
use crate::lib::inputControl::{Opts, input_control};
use crate::lib::style::*;
use crate::ui::node::{Align, Node, Sides, Style};
use crate::ui::window;
use crate::widgets::record::service::{is_rec, rec_mic, rec_quality, save_clip, start_rec};

fn hide() {
    window::set_visible("recordMenu", false);
}

pub fn record_menu() {
    let w = input_control(
        "recordMenu",
        |_| {
            let base = window();
            let hs = header_style(&base);
            let mut img = hs.inherit();
            img.icon_shadow = vec![shadow_(), glow(10.0, light3())];
            let mut label = hs.inherit();
            label.font.size = 35.0;
            let mic = if rec_mic().peek() { "audio-input-microphone-symbolic" } else { "microphone-disabled-symbolic" };
            let header = Node::hbox(hs.inherit(), 8.0, vec![Node::icon(img, mic, 32.0), Node::label(label, rec_quality().peek())])
                .expand(true, true)
                .align(Align::Center, Align::Center);
            background_section(&base, header, None, 100.0, 350.0)
        },
        Opts {
            actions: vec![
                (Keysym::space, Rc::new(|| {
                    start_rec();
                    hide();
                })),
                (Keysym::c, Rc::new(|| {
                    save_clip();
                    hide();
                })),
                (Keysym::r, Rc::new(|| rec_mic().set(!rec_mic().peek()))),
                (Keysym::q, Rc::new(|| rec_quality().set(if rec_quality().peek() == "Medium" { "Ultra" } else { "Medium" }))),
            ],
            ..Default::default()
        },
    );
    w.watch(&rec_mic());
    w.watch(&rec_quality());
}

pub fn recording_indicator(base: &Style) -> Node {
    let mut s = base.inherit();
    s.color = red();
    s.padding = Sides::vh(8.0, 0.0);
    s.icon_shadow = vec![shadow_(), glow(10.0, red())];
    Node::icon(s, "media-record-symbolic", 23.0).align(Align::Center, Align::Fill).show(is_rec().peek())
}
