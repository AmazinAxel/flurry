use std::cell::RefCell;
use std::rc::Rc;

use crate::lib::audio;
use crate::lib::brightness::brightness;
use crate::lib::outTransition::Flash;
use crate::lib::style::*;
use crate::ui::node::{Node, Sides, Style, levelbar};
use crate::ui::wayland::Anchor;
use crate::ui::window::{Spec, Win};

thread_local! {
    static OSD: RefCell<(&'static str, f64)> = const { RefCell::new(("", 0.0)) };
    static SHOW: RefCell<Option<Rc<dyn Fn(&'static str, f64)>>> = RefCell::new(None);
}

/// Volume keys: shows the default speaker even when the press changed nothing (at 0% or 100%).
pub fn show_volume() {
    if let Some(s) = audio::default_speaker().filter(audio::Sink::known)
        && let Some(show) = SHOW.with(|f| f.borrow().clone())
    {
        show(s.volume_icon(), s.volume);
    }
}

/// `.osd levelbar`: trough @blue2, filled block @blue2 inset 3px, empty @light3.
fn osd_bar(value: f64) -> Node {
    let mut n = levelbar(&Style::default(), value, blue2(), light3(), 6.0);
    n.style.padding = Sides::all(3.0);
    n.style.background = Some(blue2());
    n.size(400.0, 0.0)
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
        let (flash, w) = (flash, w);
        Rc::new(move |icon: &'static str, value: f64| {
            OSD.with(|o| *o.borrow_mut() = (icon, value));
            flash.show(&w);
        })
    };

    SHOW.with(|f| *f.borrow_mut() = Some(show.clone()));
    let b = brightness();
    let s1 = show.clone();
    b.subscribe(move || s1("display-brightness-symbolic", brightness().peek()));

    // volume/mute changes and default speaker switches, after the first sync; a default that is
    // briefly unknown (new sink not bound yet) keeps the last one so the switch still shows
    let last: Rc<RefCell<Option<(u32, u64, bool)>>> = Rc::default();
    let s2 = show.clone();
    let l = last;
    let check = move || {
        if !audio::ready().peek() {
            return;
        }
        // not before its volume arrived: 0 -> actual would flash the OSD at startup
        let Some(s) = audio::default_speaker().filter(audio::Sink::known) else { return };
        let cur = (s.id, s.volume.to_bits(), s.mute);
        // unsettled: WirePlumber still restoring, only the baseline moves
        if l.replace(Some(cur)).is_some_and(|prev| prev != cur) && audio::settled().peek() {
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
