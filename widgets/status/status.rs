// Quickstatus menu: clock, status icons and date, the media block, then the quicksettings sliders and outputs.

use crate::lib::audio;
use crate::lib::mediaPlayer::{PLAYLISTS, is_playing, playlist_index, song};
use crate::lib::style::*;
use crate::ui::node::{Align, Background, Node, Sides, Style};
use crate::widgets::record::record::recording_indicator;
use crate::widgets::status::modules::{mpris::track, statusIcons::status, time};
use crate::widgets::status::quicksettings::quicksettings::quick_settings;

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

/// Play/pause on the left with the mpd song, or the MPRIS track while mpd is paused;
/// just a centered button when there is neither.
fn media(base: &Style) -> Node {
    let playlist = playlist_color();
    let playing = is_playing().peek();
    let mut bar = Style { background: Background::Color(playlist), margin: Sides::trbl(18.0, 0.0, 6.0, 0.0), ..base.inherit() };
    bar = bar.border(Sides::all(3.0), shade(playlist, 1.1));
    let mut btn = Style { padding: Sides::vh(12.0, 18.0), background: Background::Color(shade(playlist, 0.9)), ..base.inherit() };
    let icon = if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" };
    let text = if playing { Some(song().peek()).filter(|(t, _)| !t.is_empty()) } else { track().peek() };
    let Some((title, artist)) = text else {
        btn.background = Background::Color(playlist);
        return Node::hbox(bar, 0.0, vec![Node::hbox(btn, 0.0, vec![Node::icon(base.inherit(), icon, 26.0)]).expand(true, false).align(Align::Center, Align::Fill).id("mediaBtn")])
            .id("media");
    };
    let mut t = base.inherit();
    t.font.size = 19.0;
    t.font.weight = 700;
    let a = Style { font: font(SORA, 16.0), ..base.inherit() };
    let info = Node::vbox(Style { padding: Sides::vh(8.0, 14.0), ..base.inherit() }, 2.0, vec![
        Node::label(t, title).xalign(0.0).ellipsize(),
        Node::label(a, artist.clone()).xalign(0.0).ellipsize().show(!artist.is_empty()),
    ])
    .expand(true, false)
    .align(Align::Fill, Align::Center);
    Node::hbox(bar, 0.0, vec![Node::hbox(btn, 0.0, vec![Node::icon(base.inherit(), icon, 26.0).align(Align::Center, Align::Center)]).id("mediaBtn"), info]).id("media")
}

pub fn quickstatus(base: &Style, focus: Option<&str>) -> Node {
    let mut icons = Node::vbox(base.inherit(), 0.0, vec![recording_indicator(base), status(base)]).expand(true, false);
    icons = icons.align(Align::Center, Align::Center);
    let top = Node::hbox(base.inherit(), 0.0, vec![time::clock(base), icons, time::date(base)]);
    Node::vbox(base.inherit(), 0.0, vec![top, media(base), quick_settings(base, focus)]).expand(true, false)
}
