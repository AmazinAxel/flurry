use std::cell::RefCell;
use std::rc::Rc;

use crate::lib::audio;
use crate::lib::brightness::brightness;
use crate::lib::outTransition::Flash;
use crate::lib::style::*;
use crate::ui::node::{Kind, Node, Sides, Style, fill_rect, shadow};
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
    let mut s = Style::default();
    s.box_shadow = vec![shadow(0.0, 0.0, 10.0, blue3())];
    Node::new(Kind::Custom { width: 400.0, height: 12.0, paint }, s).size(400.0, 0.0)
}

pub fn osd() {
    let flash = Flash::new(500, 125.0);
    let f = flash.clone();
    let w = Win::new(Spec { name: "osd", anchor: Anchor::Bottom, per_monitor: true, ..Default::default() }, move |_| {
        let base = window();
        let (icon, val) = OSD.with(|o| *o.borrow());
        let mut s = status_element(&base);
        s.padding = Sides::all(8.0);
        s.margin = Sides::trbl(0.0, 0.0, 5.0, 0.0);
        let mut img = base.inherit();
        img.margin = Sides::trbl(0.0, 8.0, 0.0, 0.0);
        img.icon_shadow = vec![shadow_(), glow(15.0, light3())];
        let mut n = Node::hbox(s, 0.0, vec![Node::icon(img, icon, 24.0), osd_bar(val).align(crate::ui::node::Align::Fill, crate::ui::node::Align::Center)]);
        // SLIDE_UP revealer: content slides down out of the window as it hides
        let h = n.measure_h(n.measure_w().1).1 + 5.0;
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

    // volume/mute changes of the same default speaker, after the first sync
    let last: Rc<RefCell<Option<(u32, String)>>> = Rc::default();
    let s2 = show.clone();
    let l = last.clone();
    let check = move || {
        if !audio::ready().peek() {
            return;
        }
        let sp = audio::default_speaker();
        let cur = sp.as_ref().map(|s| (s.id, format!("{}{}", s.volume, s.mute)));
        let prev = l.replace(cur.clone());
        if let (Some(s), Some((pid, pv)), Some((cid, cv))) = (sp, prev, cur)
            && pid == cid
            && pv != cv
        {
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
