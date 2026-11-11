// Quickstatus menu: clock, status icons and date, the media block, then the quicksettings sliders and outputs.

use crate::lib::mediaPlayer::{is_playing, playlist, song, volume as mpd_volume};
use crate::lib::menuWindow::GAP;
use crate::lib::style::*;
use crate::ui::node::{Align, Node, Sides, Style, TRANSPARENT, levelbar};
use crate::widgets::status::modules::{mpris::{self, track}, statusIcons::status, time::{self, DAY_INK, DIGIT_BEARING}};
use crate::widgets::status::quicksettings::quicksettings::quick_settings;

/// Ink height of the clock and date; the group is centered in the wallpaper's width.
const TOP_HEIGHT: f32 = 220.0;
/// Text size of the controls under the wallpaper.
const CONTROL_FONT: f32 = 19.0;

/// The playlist's style color name from settings (anything unknown is blue2).
fn playlist_color() -> tiny_skia::Color {
    match playlist().map(|(_, c)| c) {
        Some("red") => red(),
        Some("yellow") => yellow(),
        Some("purple") => purple(),
        Some("blue1") => blue1(),
        Some("blue3") => blue3(),
        Some("blue4") => blue4(),
        _ => blue2(),
    }
}

/// What the media block shows, with its (title, artist); its volume is the one scrolled and drawn.
pub enum MediaSource {
    /// mpd plays (its song, if it has a title), or no MPRIS player has a track
    Mpd(Option<(String, String)>),
    /// mpd is not playing and an MPRIS player has a track
    Mpris((String, String)),
}

pub fn media_source() -> MediaSource {
    if is_playing().peek() {
        return MediaSource::Mpd(Some(song().peek()).filter(|(t, _)| !t.is_empty()));
    }
    track().peek().map_or(MediaSource::Mpd(None), MediaSource::Mpris)
}

/// Play/pause on the left with the mpd song, or the MPRIS track while mpd is paused;
/// just a centered button when there is neither. Always as tall as title plus artist.
fn media(base: &Style, focus: Option<&str>) -> Node {
    let info = |title: String, artist: String| {
        let mut t = base.inherit();
        t.font.size = 22.0;
        t.font.weight = 700;
        let a = Style { font: font(SORA, 18.0), ..base.inherit() };
        let show_artist = !artist.is_empty();
        Node::vbox(Style { padding: Sides::vh(SPACE_M, SPACE_L), ..base.inherit() }, SPACE_S, vec![
            Node::label(t, title).xalign(0.0).ellipsize(),
            Node::label(a, artist).xalign(0.0).ellipsize().show(show_artist),
        ])
        .expand(true, false)
        .align(Align::Fill, Align::Center)
    };
    let playlist = playlist_color();
    let playing = is_playing().peek();
    let bar = Style { min_height: info(" ".into(), " ".into()).measure_h(f32::MAX).1, ..base.inherit() };
    // the volume bar overlays the card's bottom edge, above its border, so the card keeps its size
    let card = Style { background: Some(playlist), margin: Sides::vh(SPACE_M, 0.0), ..base.inherit() }.border(Sides::all(BORDER), lighter(playlist));
    let icon = if playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" };
    let mut btn_style = Style { padding: Sides::vh(SPACE_M, SPACE_L), ..base.inherit() };
    if focus == Some("mediaBtn") {
        btn_style = focus_outline(btn_style);
    }
    let btn = Node::hbox(btn_style, 0.0, vec![Node::icon(base.inherit(), icon, 32.0).align(Align::Center, Align::Center)]).focusable("mediaBtn");
    let (text, volume) = match media_source() {
        MediaSource::Mpd(text) => (text, mpd_volume().peek()),
        MediaSource::Mpris(text) => (Some(text), mpris::volume().peek()),
    };
    let children = match text {
        Some((title, artist)) => vec![btn, info(title, artist)],
        None => vec![btn.expand(true, false).align(Align::Center, Align::Fill)],
    };
    Node::bottom_bar(card, Node::hbox(bar, 0.0, children), levelbar(base, volume.unwrap_or(0.0), lighter(playlist), TRANSPARENT, BORDER)).id("media")
}

/// Wallpaper part (clock | icons | date on one GAP grid, as tall as each other) and the controls under it.
pub fn quickstatus(base: &Style, focus: Option<&str>) -> (Node, Option<Node>) {
    let icons = status(base);
    // weekday ink as wide as the date digits (two digits: 2 advances less the outer bearings)
    let day = (1.0 - 2.0 * DIGIT_BEARING) / DAY_INK;
    // both columns TOP_HEIGHT tall: 2 rows + gap for the clock, 3 rows + 2 gaps for the date
    let c = (TOP_HEIGHT - GAP) / 1.5;
    let d = (TOP_HEIGHT - 2.0 * GAP) / (1.5 + 0.75 * day);
    // clock and date padded to the same width so the icons sit on the center line
    let (mut clock, mut date) = (time::clock(base, c, GAP), time::date(base, d, day * d, GAP));
    let (cw, dw) = (clock.measure_w().1, date.measure_w().1);
    clock.style.margin.left += (dw - cw).max(0.0);
    date.style.margin.right += (cw - dw).max(0.0);
    let top = Node::hbox(base.inherit(), GAP, vec![clock, icons, date]).expand(true, false).align(Align::Center, Align::Center);
    let mut big = base.clone();
    big.font.size = CONTROL_FONT;
    (top, Some(Node::vbox(big.inherit(), 0.0, vec![media(&big, focus), quick_settings(&big, focus)]).expand(true, false)))
}
