use std::rc::Rc;

use crate::lib::audio;
use crate::lib::mediaPlayer::{PLAYLISTS, is_playing, media_scroll, playlist_index};
use crate::lib::scrollStep::scroll_step;
use crate::lib::state::State;
use crate::lib::style::*;
use crate::ui::node::{Align, Background, Node, Sides, Style};
use crate::ui::wayland::{Anchor, PointerEvent};
use crate::ui::window::{self, Spec, Win};
use crate::widgets::record::record::recording_indicator;
use crate::widgets::status::modules::{mpris::mpris, statusIcons::status, time::time};

thread_local! {
    static STATUS_MARGIN: State<f32> = State::new(0.0);
}

pub fn status_margin() -> State<f32> {
    STATUS_MARGIN.with(Clone::clone)
}

pub fn volume_icon() -> &'static str {
    audio::default_speaker().map(|s| s.volume_icon()).unwrap_or("audio-volume-muted-symbolic")
}

fn playlist_color() -> tiny_skia::Color {
    match PLAYLISTS[playlist_index().peek()] {
        "Study" => red(),
        "Focus" => blue3(),
        "Synthwave" => purple(),
        "Liked" => blue4(),
        "SynthAmbient" => yellow(),
        _ => blue2(),
    }
}

fn build() -> Node {
    let base = window();
    let playlist = playlist_color();
    let media_style = status_element(&base).border(Sides::all(3.0), shade(playlist, 1.1));
    let btn = Style { padding: Sides::all(8.0), background: Background::Color(playlist), ..base.inherit() };
    let play = if is_playing().peek() { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" };
    let media = Node::vbox(media_style, 0.0, vec![
        Node::hbox(btn, 0.0, vec![Node::icon(base.inherit(), play, 21.0).expand(true, false)]),
        mpris(&base),
    ])
    .align(Align::Fill, Align::Fill)
    .id("media");

    let info = Node::vbox(status_element(&base), 0.0, vec![recording_indicator(&base), time(&base), status(&base)]).id("info");
    Node::vbox(base.inherit(), 0.0, vec![media, info]).align(Align::Fill, Align::End)
}

pub fn status_window() -> Rc<Win> {
    let w = Win::new(Spec { name: "status", anchor: Anchor::Bottom | Anchor::Left, ..Default::default() }, |_| build());
    w.watch(&crate::lib::time::now());
    w.watch(&is_playing());
    w.watch(&playlist_index());
    w.watch(&audio::speakers());
    w.watch(&audio::default_name());
    w.watch(&crate::lib::battery::battery());
    w.watch(&crate::lib::bluetooth::bt());
    w.watch(&crate::lib::streamingMode::streaming_mode());
    w.watch(&crate::widgets::status::modules::statusIcons::network_icon());
    w.watch(&crate::widgets::status::modules::mpris::players());
    w.watch(&crate::widgets::record::service::is_rec());
    w.on_pointer(|ev, _| {
        let PointerEvent::Scroll { x, y, dy, wheel } = ev else { return };
        let Some(win) = window::get("status") else { return };
        // each block has its own scroll controller; the gaps between them scroll nothing
        let (x, y) = (x as f32, y as f32);
        let inside = |id: &str| win.rect_of(id).is_some_and(|r| x >= r.left() && x < r.right() && y >= r.top() && y < r.bottom());
        if inside("media") {
            media_scroll(dy, wheel);
        } else if inside("info")
            && let Some(s) = audio::default_speaker()
        {
            audio::set_volume(&s, s.volume - scroll_step(dy, wheel, 0.05, 0.006));
        }
    });
    w
}

/// toggleInfoArea
pub fn toggle_info_area() {
    let Some(w) = window::get("status") else { return };
    w.set_visible(!w.visible());
    let width = if w.visible() {
        let n = build();
        n.measure_w().1 + n.style.margin.h()
    } else {
        0.0
    };
    status_margin().set(width);
}
