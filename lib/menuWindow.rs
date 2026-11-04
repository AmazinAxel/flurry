// Centered modal hosting the open status menu: quickstatus, bluetooth, wifi or calendar.

use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::menuState::{close_menu, current};
use crate::lib::inputControl::modal;
use crate::lib::wrapFocus::wrap_focus;
use crate::lib::backgroundSection::background_section;
use crate::lib::style::*;
use crate::ui::node::{Node, Sides, Style};
use crate::ui::wayland::{Anchor, Keymode, PointerEvent};
use crate::ui::window::{Spec, Win};
use crate::lib::{audio, mediaPlayer};
use crate::lib::scrollStep::scroll_step;
use crate::ui::state::State;
use crate::widgets::status::quicksettings::quicksettings as qs;
use crate::widgets::status::status::quickstatus;
use crate::widgets::status::{bluetooth, calendar, network};

/// Spacing unit inside the cards; also the wallpaper's inner padding.
pub const GAP: f32 = SPACE_L;

/// Wallpaper-backed part, and what goes under it.
fn content(base: &Style, name: &str, focus: Option<&str>) -> (Node, Option<Node>) {
    let menu = match name {
        "status" => return quickstatus(base, focus),
        "bluetooth" => {
            if crate::lib::bluetooth::bt().with(|b| b.available) {
                bluetooth::menu(base, focus)
            } else {
                Node::hbox(base.inherit(), 0.0, vec![])
            }
        }
        "wifi" => network::menu(base, focus),
        _ => calendar::calendar(base),
    };
    (menu, None)
}

/// Redraws when `state` changes while one of `menus` is open.
pub fn watch_in<T: Clone + PartialEq + 'static>(w: &Rc<Win>, state: &State<T>, menus: &'static [&'static str]) {
    let weak = Rc::downgrade(w);
    state.subscribe(move || {
        if let Some(w) = weak.upgrade()
            && w.visible()
            && current().peek().is_some_and(|m| menus.contains(&m))
        {
            w.redraw();
        }
    });
}

pub fn menu_window() -> Rc<Win> {
    let spec = Spec { name: "menu", anchor: Anchor::all(), keymode: Keymode::Exclusive, ..Default::default() };
    let w = Win::new(spec, |focus| {
        let base = window();
        let Some(name) = current().peek() else { return Node::hbox(base, 0.0, vec![]) };
        let (top, rest) = content(&base, name, focus);
        let top = Node::hbox(Style { padding: Sides::all(GAP), ..base.inherit() }, 0.0, vec![top]).expand(true, false).id("card");
        // same box as the launcher/clipboard/pass cards, so the same top edge
        background_section(&base, top, rest.map(|r| r.id("cardRest")), CARD_HEIGHT, CARD_WIDTH)
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
            Some("wifi") => network::focus_target(),
            Some("bluetooth") => bluetooth::focus_target(),
            _ => None,
        };
        w.set_visible(open.is_some());
        w.redraw();
    });
    // each state redraws only the menus showing it
    const STATUS: &[&str] = &["status"];
    watch_in(&w, &crate::lib::audio::speakers(), STATUS);
    watch_in(&w, &crate::lib::audio::default_name(), STATUS);
    watch_in(&w, &crate::lib::brightness::brightness(), STATUS);
    watch_in(&w, &crate::widgets::status::quicksettings::sound::selected(), STATUS);
    watch_in(&w, &crate::lib::bluetooth::bt(), &["status", "bluetooth"]);
    watch_in(&w, &bluetooth::order(), &["bluetooth"]);
    watch_in(&w, &bluetooth::sticky(), &["bluetooth"]);
    watch_in(&w, &crate::lib::time::now(), &["status", "calendar"]);
    watch_in(&w, &mediaPlayer::is_playing(), STATUS);
    watch_in(&w, &mediaPlayer::song(), STATUS);
    watch_in(&w, &mediaPlayer::playlist_index(), STATUS);
    watch_in(&w, &crate::lib::battery::battery(), STATUS);
    watch_in(&w, &crate::lib::streamingMode::streaming_mode(), &["status", "wifi"]);
    watch_in(&w, &crate::lib::iwd::state(), STATUS);
    watch_in(&w, &crate::widgets::status::modules::mpris::track(), STATUS);
    watch_in(&w, &crate::widgets::record::service::is_rec(), STATUS);
    network::watch(&w);

    let ww = Rc::downgrade(&w);
    w.on_key(move |ev| {
        let Some(w) = ww.upgrade() else { return };
        let focus = w.focused().unwrap_or_default();
        if network::key(&focus, ev) {
            return w.redraw();
        }
        // hiding forgets the open menu, and also closes one opened with no menu (`flurry toggle menu`)
        if ev.sym == Keysym::Escape {
            return w.set_visible(false);
        }
        let name = current().peek().unwrap_or("");
        if name == "calendar" && calendar::key(ev) {
            return w.redraw();
        }
        if name == "bluetooth" && bluetooth::key(&focus, ev) {
            return w.redraw();
        }
        if name == "status" && !ev.shift && !ev.ctrl && !ev.alt && qs::key(&focus, ev) {
            return w.redraw();
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
            return w.redraw();
        }
        let tab = matches!(ev.sym, Keysym::Tab | Keysym::ISO_Left_Tab) && !ev.ctrl && !ev.alt;
        if wrap_focus(&w, ev, false) || (tab && w.focus_step(!ev.shift && ev.sym == Keysym::Tab)) {
            if let Some(f) = w.focused() {
                bluetooth::on_focus(&f);
                network::on_focus(&f);
            }
            w.redraw();
        }
    });

    let ww = Rc::downgrade(&w);
    // quickstatus: the media block scrolls mpd's volume, the rest of the card the speaker's
    w.on_pointer(move |ev| {
        let Some(w) = ww.upgrade() else { return };
        let PointerEvent::Scroll { x, y, dy, wheel } = ev;
        if current().peek() != Some("status") {
            return;
        }
        let (x, y) = (x as f32, y as f32);
        let inside = |id: &str| w.rect_of(id).is_some_and(|r| x >= r.left() && x < r.right() && y >= r.top() && y < r.bottom());
        if inside("media") {
            mediaPlayer::media_scroll(dy, wheel);
        } else if (inside("card") || inside("cardRest"))
            && let Some(s) = audio::default_speaker()
        {
            audio::set_volume(&s, s.volume - scroll_step(dy, wheel, 0.05, 0.006));
        }
    });
    w
}

