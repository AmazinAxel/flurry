use std::cell::RefCell;
use std::rc::Rc;

use crate::lib::audio;
use crate::lib::brightness::brightness;
use crate::lib::outTransition::Flash;
use crate::lib::style::*;
use crate::ui::node::{Kind, Node, Sides, Style, fill_rect};
use crate::ui::wayland::Anchor;
use crate::ui::window::{Spec, Win};
use crate::widgets::status::modules::statusIcons::leak;

thread_local! {
    static OSD: RefCell<(&'static str, f64)> = const { RefCell::new(("", 0.0)) };
}

/// `.osd levelbar`: trough @blue2 with glow, filled block @blue2 inset 3px, empty @light3.
fn osd_bar(value: f64) -> Node {
    let paint = Rc::new(move |pm: &mut tiny_skia::PixmapMut, r: tiny_skia::Rect, ts: tiny_skia::Transform| {
        fill_rect(pm, r, blue2(), ts);
        let Some(inner) = tiny_skia::Rect::from_ltrb(r.left() + 3.0, r.top() + 3.0, r.right() - 3.0, r.bottom() - 3.0) else { return };
        fill_rect(pm, inner, light3(), ts);
        if let Some(f) = tiny_skia::Rect::from_xywh(inner.x(), inner.y(), inner.width() * value.clamp(0.0, 1.0) as f32, inner.height()) {
            fill_rect(pm, f, blue2(), ts);
        }
    });
    Node::new(Kind::Custom { width: 400.0, height: 12.0, key: Some(value.to_bits()), paint }, Style::default()).size(400.0, 0.0)
}

pub fn osd() {
    let flash = Flash::new(500, 125.0);
    let f = flash.clone();
    let w = Win::new(Spec { name: "osd", anchor: Anchor::Bottom, per_monitor: true, ..Default::default() }, move |_| {
        let base = window();
        let (icon, val) = OSD.with(|o| *o.borrow());
        let mut s = status_element(&base);
        s.padding = Sides::all(SPACE_M);
        s.margin = Sides::trbl(SPACE_L, SPACE_L, SWAY_GAP, SPACE_L);
        let mut img = base.inherit();
        img.margin = Sides::trbl(0.0, SPACE_M, 0.0, 0.0);
        let mut n = Node::hbox(s, 0.0, vec![Node::icon(img, icon, 24.0), osd_bar(val).align(crate::ui::node::Align::Fill, crate::ui::node::Align::Fill)]);
        // SLIDE_UP revealer: content slides down out of the window as it hides
        let h = n.measure_h(n.measure_w().1).1 + SPACE_L * 2.0;
        n.style.translate_y = (1.0 - f.progress()) * h;
        n
    });
    let show = {
        let (flash, w) = (flash.clone(), w.clone());
        Rc::new(move |icon: &'static str, value: f64| {
            OSD.with(|o| *o.borrow_mut() = (icon, value));
            flash.show(&w);
        })
    };

    let b = brightness();
    let s1 = show.clone();
    b.subscribe(move || s1(leak("display-brightness-symbolic"), brightness().peek()));

    // volume/mute changes and default speaker switches, after the first sync; a default that is
    // briefly unknown (new sink not bound yet) keeps the last one so the switch still shows
    let last: Rc<RefCell<Option<(u32, String)>>> = Rc::default();
    let s2 = show.clone();
    let l = last.clone();
    let check = move || {
        if !audio::ready().peek() {
            return;
        }
        // not before its volume arrived: 0 -> actual would flash the OSD at startup
        let Some(s) = audio::default_speaker().filter(audio::Sink::known) else { return };
        let cur = (s.id, format!("{}{}", s.volume, s.mute));
        if l.replace(Some(cur.clone())).is_some_and(|prev| prev != cur) {
            s2(s.volume_icon(), s.volume);
        }
    };
    let c = Rc::new(check);
    let c1 = c.clone();
    audio::speakers().subscribe(move || c1());
    let c2 = c.clone();
    audio::default_name().subscribe(move || c2());
    let c3 = c.clone();
    audio::ready().subscribe(move || c3());
}
