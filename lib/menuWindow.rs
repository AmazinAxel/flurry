// Centered modal hosting the open status menu: quickstatus, bluetooth, wifi or calendar (lib/asideStatusWindow.tsx).

use std::cell::Cell;
use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::menuState::{close_menu, current};
use crate::lib::inputControl::{modal, wrap_focus};
use crate::lib::backgroundSection::background_section;
use crate::lib::style::*;
use crate::ui::node::{Node, Sides, Style};
use crate::ui::wayland::{Anchor, Keymode, PointerEvent};
use crate::ui::window::{Spec, Win};
use crate::lib::{audio, mediaPlayer};
use crate::lib::scrollStep::scroll_step;
use crate::widgets::status::quicksettings::quicksettings::{self as qs, set_slider, slider_value};
use crate::widgets::status::status::quickstatus;
use crate::widgets::status::{bluetooth, calendar, network};

/// Card width, the same as the launcher/clipboard cards.
pub const MENU_WIDTH: f32 = 500.0;
/// Spacing unit inside the cards; also the wallpaper's inner padding.
pub const GAP: f32 = 20.0;
/// Width inside the wallpaper: minus background_section's margin, border and padding, and GAP padding.
pub const INNER_WIDTH: f32 = MENU_WIDTH - 2.0 * (16.0 + 3.0 + 16.0 + GAP);

thread_local! {
    static DRAG: Cell<Option<&'static str>> = const { Cell::new(None) };
}

/// Wallpaper-backed part, and what goes under it.
fn content(base: &Style, name: &str, focus: Option<&str>) -> (Node, Option<Node>) {
    let menu = match name {
        "status" => return quickstatus(base, focus),
        "bluetooth" => {
            if crate::lib::bluetooth::bt().with(|b| b.available) {
                bluetooth::menu(base, focus, 0.0)
            } else {
                Node::hbox(base.inherit(), 0.0, vec![])
            }
        }
        "wifi" => network::menu(base, focus, 0.0),
        _ => calendar::calendar(base),
    };
    (menu, None)
}

pub fn menu_window() -> Rc<Win> {
    let spec = Spec { name: "menu", anchor: Anchor::all(), keymode: Keymode::Exclusive, ..Default::default() };
    let w = Win::new(spec, |v| {
        let base = window();
        let Some(name) = current().peek() else { return Node::hbox(base, 0.0, vec![]) };
        let (top, rest) = content(&base, name, v.focus.as_deref());
        let top = Node::hbox(Style { padding: Sides::all(GAP), ..base.inherit() }, 0.0, vec![top]).expand(true, false).id("card");
        // same box as the launcher/clipboard/pass cards, so the same top edge
        background_section(&base, top, rest.map(|r| r.id("cardRest")), 700.0, MENU_WIDTH)
    });

    modal(&w);
    // hidden by another modal: forget the open menu
    let ww = Rc::downgrade(&w);
    w.visibility.subscribe(move || {
        if ww.upgrade().is_some_and(|w| !w.visible()) {
            close_menu();
        }
    });
    let ww = Rc::downgrade(&w);
    current().subscribe(move || {
        let Some(w) = ww.upgrade() else { return };
        let open = current().peek();
        if open == Some("calendar") {
            calendar::reset();
        }
        *w.focus.borrow_mut() = match open {
            Some("calendar") => Some("calendar".into()),
            Some("status") => qs::first_sink(),
            _ => None,
        };
        w.set_visible(open.is_some());
        w.redraw();
    });
    w.watch(&crate::lib::audio::speakers());
    w.watch(&crate::lib::audio::default_name());
    w.watch(&crate::lib::brightness::brightness());
    w.watch(&crate::widgets::status::quicksettings::sound::selected());
    w.watch(&crate::lib::bluetooth::bt());
    w.watch(&bluetooth::order());
    w.watch(&bluetooth::sticky());
    w.watch(&crate::lib::time::now());
    w.watch(&mediaPlayer::is_playing());
    w.watch(&mediaPlayer::song());
    w.watch(&mediaPlayer::playlist_index());
    w.watch(&crate::lib::battery::battery());
    w.watch(&crate::lib::streamingMode::streaming_mode());
    w.watch(&crate::widgets::status::modules::statusIcons::network_icon());
    w.watch(&crate::widgets::status::modules::mpris::track());
    w.watch(&crate::widgets::record::service::is_rec());
    network::watch(&w);

    let ww = Rc::downgrade(&w);
    w.on_key(move |ev| {
        let Some(w) = ww.upgrade() else { return false };
        let focus = w.focused().unwrap_or_default();
        let done = |w: &Win| {
            w.redraw();
            true
        };
        if network::key(&focus, ev) {
            return done(&w);
        }
        if ev.sym == Keysym::Escape {
            close_menu();
            return true;
        }
        let name = current().peek().unwrap_or("");
        if name == "calendar" && calendar::key(ev) {
            return done(&w);
        }
        if name == "bluetooth" && bluetooth::key(&focus, ev) {
            return done(&w);
        }
        if matches!(ev.sym, Keysym::Return | Keysym::KP_Enter | Keysym::space) {
            match name {
                "status" if matches!(ev.sym, Keysym::Return | Keysym::KP_Enter) => {
                    qs::activate(&focus, ev.shift);
                }
                "bluetooth" => {
                    bluetooth::activate(&focus);
                }
                "wifi" => {
                    if let Some(Some(next)) = network::activate(&focus) {
                        w.grab_focus(&next);
                    }
                }
                _ => {}
            }
            return done(&w);
        }
        let tab = matches!(ev.sym, Keysym::Tab | Keysym::ISO_Left_Tab) && !ev.ctrl && !ev.alt;
        if wrap_focus(&w, ev, false) || (tab && w.focus_step(!ev.shift && ev.sym == Keysym::Tab)) {
            if let Some(f) = w.focused() {
                bluetooth::on_focus(&f);
                network::on_focus(&f);
            }
            return done(&w);
        }
        false
    });

    let ww = Rc::downgrade(&w);
    w.on_pointer(move |ev, _| {
        let Some(w) = ww.upgrade() else { return };
        let inside = |id: &str, x: f64, y: f64| w.rect_of(id).is_some_and(|r| (x as f32) >= r.left() && (x as f32) < r.right() && (y as f32) >= r.top() && (y as f32) < r.bottom());
        let hit = |x: f64, y: f64| {
            ["volume", "brightness"].into_iter().find_map(|id| w.rect_of(id).filter(|r| (x as f32) >= r.left() && (x as f32) < r.right() && (y as f32) >= r.top() && (y as f32) < r.bottom()).map(|r| (id, r)))
        };
        match ev {
            PointerEvent::Button { x, y, pressed: true } => {
                if inside("mediaBtn", x, y) {
                    mediaPlayer::play_pause();
                } else if let Some((id, r)) = hit(x, y) {
                    DRAG.with(|d| d.set(Some(id)));
                    set_slider(id, slider_value(r, x as f32));
                }
            }
            PointerEvent::Button { pressed: false, .. } | PointerEvent::Leave => DRAG.with(|d| d.set(None)),
            PointerEvent::Motion { x, .. } => {
                if let Some(id) = DRAG.with(Cell::get)
                    && let Some(r) = w.rect_of(id)
                {
                    set_slider(id, slider_value(r, x as f32));
                }
            }
            // quickstatus: the media block scrolls mpd's volume, the rest of the card the speaker's
            PointerEvent::Scroll { x, y, dy, wheel } if current().peek() == Some("status") => {
                if inside("media", x, y) {
                    mediaPlayer::media_scroll(dy, wheel);
                } else if (inside("card", x, y) || inside("cardRest", x, y))
                    && let Some(s) = audio::default_speaker()
                {
                    audio::set_volume(&s, s.volume - scroll_step(dy, wheel, 0.05, 0.006));
                }
            }
            _ => {}
        }
    });
    w
}

