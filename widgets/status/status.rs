// Quickstatus menu: clock, status icons and date, the media block, then the quicksettings sliders and outputs.

use crate::lib::audio;
use crate::lib::menuWindow::GAP;

/// Ink height of the clock and date; the group is centered in the wallpaper's width.
const TOP_HEIGHT: f32 = 220.0;
/// Text size of the controls under the wallpaper.
const CONTROL_FONT: f32 = 19.0;
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
    let bar = Style {
        background: Background::Color(playlist),
        margin: Sides::vh(SPACE_M, 0.0),
        ..base.inherit()
    }
    .border(Sides::all(BORDER), lighter(playlist));
    let btn = Style { padding: Sides::vh(SPACE_M, SPACE_L), ..base.inherit() };
    let icon = if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" };
    let text = if playing { Some(song().peek()).filter(|(t, _)| !t.is_empty()) } else { track().peek() };
    let Some((title, artist)) = text else {
        return Node::hbox(bar, 0.0, vec![Node::hbox(btn, 0.0, vec![Node::icon(base.inherit(), icon, 32.0)]).expand(true, false).align(Align::Center, Align::Fill).id("mediaBtn")])
            .id("media");
    };
    let mut t = base.inherit();
    t.font.size = 22.0;
    t.font.weight = 700;
    let a = Style { font: font(SORA, 18.0), ..base.inherit() };
    let info = Node::vbox(Style { padding: Sides::vh(SPACE_M, SPACE_L), ..base.inherit() }, SPACE_S, vec![
        Node::label(t, title).xalign(0.0).ellipsize(),
        Node::label(a, artist.clone()).xalign(0.0).ellipsize().show(!artist.is_empty()),
    ])
    .expand(true, false)
    .align(Align::Fill, Align::Center);
    Node::hbox(bar, 0.0, vec![Node::hbox(btn, 0.0, vec![Node::icon(base.inherit(), icon, 32.0).align(Align::Center, Align::Center)]).id("mediaBtn"), info]).id("media")
}

/// Wallpaper part (clock | icons | date on one GAP grid, as tall as each other) and the controls under it.
pub fn quickstatus(base: &Style, focus: Option<&str>) -> (Node, Option<Node>) {
    let icons = status(base);
    // weekday ink as wide as the date digits (two digits: 2 advances less the outer bearings)
    let day = (1.0 - 2.0 * DIGIT_BEARING) / DAY_INK;
    // both columns TOP_HEIGHT tall: 2 rows + gap for the clock, 3 rows + 2 gaps for the date
    let c = (TOP_HEIGHT - GAP) / 1.5;
    let d = (TOP_HEIGHT - 2.0 * GAP) / (1.5 + 0.75 * day);
    debug_assert!((time::clock_height(c, GAP) - time::date_height(d, day * d, GAP)).abs() < 0.5);
    // clock and date padded to the same width so the icons sit on the center line
    let (mut clock, mut date) = (time::clock(base, c, GAP), time::date(base, d, day * d, GAP));
    let (cw, dw) = (clock.measure_w().1, date.measure_w().1);
    clock.style.margin.left += (dw - cw).max(0.0);
    date.style.margin.right += (cw - dw).max(0.0);
    let top = Node::hbox(base.inherit(), GAP, vec![clock, icons, date]).expand(true, false).align(Align::Center, Align::Center);
    let mut big = base.clone();
    big.font.size = CONTROL_FONT;
    (top, Some(Node::vbox(big.inherit(), 0.0, vec![media(&big), quick_settings(&big, focus)]).expand(true, false)))
}
