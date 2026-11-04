// Volume + brightness sliders and the output selector.

use std::rc::Rc;

use tiny_skia::{PathBuilder, Rect, Transform};
use xkbcommon::xkb::Keysym;

use crate::lib::audio;
use crate::lib::brightness::{self, brightness, set_brightness};
use crate::lib::deviceMenu::{aside_row, separator};
use crate::lib::style::*;
use crate::ui::node::{Align, Kind, Node, Sides, Style, alpha, blurred, fill_rect, hash, rgba};
use crate::ui::wayland::KeyEvent;
use crate::widgets::status::quicksettings::sound::{self, apply_selection, name_substitute, selected};

/// Slider horizontal padding and the trough's end caps.
const PAD_X: f32 = SPACE_M;
const CAP: f32 = 10.0;
/// Left/Right on a focused slider.
const STEP: f64 = 0.05;

fn volume_icon() -> &'static str {
    audio::default_speaker().map(|s| s.volume_icon()).unwrap_or("audio-volume-muted-symbolic")
}

/// `window scale` look: trough @light1 5px with the 10px end caps as shadows, @blue2 highlight,
/// a 19px diamond knob (9px + 5px @blue2 border, rotated 45deg).
pub fn slider(base: &Style, id: &str, value: f64, focused: bool) -> Node {
    let paint = Rc::new(move |pm: &mut tiny_skia::PixmapMut, r: Rect, ts: Transform| {
        // r is the content box (padding already removed); trough margin 0 CAP
        let Some(trough) = Rect::from_xywh(r.x() + CAP, r.y() + (r.height() - 5.0) / 2.0, (r.width() - 2.0 * CAP).max(1.0), 5.0) else { return };
        if let Some(l) = Rect::from_xywh(trough.x() - CAP, trough.y(), CAP, 5.0) {
            fill_rect(pm, l, blue2(), ts);
        }
        if let Some(rr) = Rect::from_xywh(trough.right(), trough.y(), CAP, 5.0) {
            fill_rect(pm, rr, light1(), ts);
        }
        fill_rect(pm, trough, light1(), ts);
        let v = value.clamp(0.0, 1.0) as f32;
        let cx = trough.x() + v * (trough.width() - 1.0) + 0.5;
        if let Some(h) = Rect::from_xywh(trough.x(), trough.y(), cx - trough.x(), 5.0) {
            fill_rect(pm, h, blue2(), ts);
        }
        // knob: 19x19 square centered on the slider position, rotated 45deg around its center
        let cy = trough.y() + 2.5;
        let rot = ts.pre_translate(cx, cy).pre_rotate(45.0);
        let knob = |size: f32| Rect::from_xywh(-size / 2.0, -size / 2.0, size, size);
        if let Some(k) = knob(19.0) {
            blurred(pm, k, 4.0, alpha(dark1(), 0.8), rot, hash(&"knob"), |p, t| fill_rect(p, k, tiny_skia::Color::BLACK, t));
            let mut paint = tiny_skia::Paint::default();
            paint.set_color(blue2());
            pm.fill_path(&PathBuilder::from_rect(k), &paint, tiny_skia::FillRule::Winding, rot, None);
            if let Some(inner) = knob(9.0) {
                paint.set_color(light1());
                pm.fill_path(&PathBuilder::from_rect(inner), &paint, tiny_skia::FillRule::Winding, rot, None);
            }
        }
    });
    let mut s = Style { padding: Sides::vh(SPACE_L, PAD_X), min_height: 5.0, ..base.inherit() };
    if focused {
        s = focus_outline(s);
    }
    Node::new(Kind::Custom { width: 0.0, height: 5.0, key: None, paint }, s).expand(true, false).focusable(id)
}

pub fn quick_settings(base: &Style, focus: Option<&str>) -> Node {
    let vol = audio::default_speaker().map(|s| s.volume).unwrap_or(0.0);
    let mut rows = vec![Node::hbox(base.inherit(), 0.0, vec![
        Node::icon(base.inherit(), volume_icon(), 22.0).align(Align::Fill, Align::Center),
        slider(base, "volume", vol, focus == Some("volume")),
    ])];
    if brightness::available() {
        rows.push(Node::hbox(base.inherit(), 0.0, vec![
            Node::icon(base.inherit(), "display-brightness-symbolic", 22.0).align(Align::Fill, Align::Center),
            slider(base, "brightness", brightness().peek(), focus == Some("brightness")),
        ]));
    }
    let sel = selected().peek();
    let sinks: Vec<Node> = sound::listed()
        .iter()
        .map(|sp| {
            let is_sel = sel.contains(&sp.name);
            let id = format!("sink:{}", sp.name);
            let mut s = aside_row(base, is_sel, false, focus == Some(id.as_str()));
            if !is_sel {
                s.background = Some(rgba(255, 255, 255, 0.08));
            }
            let mut check = s.inherit();
            check.margin = Sides::trbl(0.0, SPACE_M, 0.0, 0.0);
            Node::hbox(s.clone(), 0.0, vec![
                Node::icon(check, "emblem-default-symbolic", 19.0).show(is_sel),
                Node::label(s.inherit(), name_substitute(&sp.description)).xalign(0.0).expand(true, false).ellipsize(),
            ])
            .focusable(id)
        })
        .collect();
    // same gaps around the separator as the device menus; the knob's tip stops 5px into the slider's padding
    let selector = Node::vbox(Style { padding: Sides::trbl(SPACE_S + BORDER, 0.0, 0.0, 0.0), ..base.inherit() }, 0.0, sinks);
    let mut sep = separator(base);
    sep.style.margin.top = SPACE_M + SPACE_S - 5.0;
    Node::vbox(base.inherit(), 0.0, vec![Node::vbox(base.inherit(), 0.0, rows), sep, selector]).expand(true, false)
}

/// Enter selects only this sink, Shift+Enter toggles it into a combined output.
pub fn activate(id: &str, shift: bool) -> bool {
    if id == "mediaBtn" {
        crate::lib::mediaPlayer::play_pause();
        return true;
    }
    let Some(name) = id.strip_prefix("sink:") else { return false };
    let Some(sp) = sound::listed().into_iter().find(|s| s.name == name) else { return false };
    if shift { sound::toggle_sink(&sp) } else { apply_selection(&[name.to_string()]) }
    true
}

/// Left/Right (h/l) on a focused slider step its value.
pub fn key(id: &str, ev: &KeyEvent) -> bool {
    let delta = match ev.sym {
        Keysym::Left | Keysym::h => -STEP,
        Keysym::Right | Keysym::l => STEP,
        _ => return false,
    };
    match id {
        "volume" => {
            let Some(s) = audio::default_speaker() else { return true };
            audio::set_volume(&s, (s.volume + delta).clamp(0.0, 1.0));
            if s.mute {
                audio::set_mute(&s, false);
            }
        }
        "brightness" => set_brightness((brightness().peek() + delta).clamp(0.0, 1.0)),
        _ => return false,
    }
    true
}

pub fn first_sink() -> Option<String> {
    sound::listed().first().map(|s| format!("sink:{}", s.name))
}
