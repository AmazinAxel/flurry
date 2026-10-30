// Quickstatus menu: clock, status icons and date, the media block, then the quicksettings sliders and outputs.

use crate::lib::audio;
use crate::lib::menuWindow::{GAP, INNER_WIDTH};
use crate::lib::mediaPlayer::{PLAYLISTS, is_playing, playlist_index, song};
use crate::lib::style::*;
use crate::ui::node::{Align, Background, Node, Sides, Style};
use crate::widgets::status::modules::{mpris::track, statusIcons::status, time::{self, DAY_INK, DIGIT_BEARING}};
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
    let mut bar = Style { background: Background::Color(playlist), margin: Sides::trbl(6.0, 0.0, 6.0, 0.0), ..base.inherit() };
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

/// Wallpaper part (clock | icons | date on one GAP grid, as tall as each other) and the controls under it.
pub fn quickstatus(base: &Style, focus: Option<&str>) -> (Node, Option<Node>) {
    let icons = status(base);
    // two digits of ink: 2 advances less the outer bearings
    let ink = 1.0 - 2.0 * DIGIT_BEARING;
    // weekday ink as wide as the date digits; clock as tall as the date: c = k * d + GAP / (2 * 0.75)
    let day = ink / DAY_INK;
    let k = (2.0 * 0.75 + 0.75 * day) / (2.0 * 0.75);
    let off = GAP / 1.5;
    let text = INNER_WIDTH - icons.measure_w().1 - 2.0 * GAP;
    let d = (text / ink - off) / (k + 1.0);
    let c = k * d + off;
    debug_assert!((time::clock_height(c, GAP) - time::date_height(d, day * d, GAP)).abs() < 0.5);
    let top = Node::hbox(base.inherit(), GAP, vec![time::clock(base, c, GAP), icons, time::date(base, d, day * d, GAP)]).align(Align::Center, Align::Center);
    (top, Some(Node::vbox(base.inherit(), 0.0, vec![media(base), quick_settings(base, focus)]).expand(true, false)))
}
