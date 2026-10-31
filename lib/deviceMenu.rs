// Power/scan header + scrolling device list shared by bluetooth and wifi (lib/deviceMenu.tsx).

use crate::lib::style::*;
use crate::ui::node::{Align, Background, Node, Sides, Style};

pub struct DeviceMenu<'a> {
    pub base: &'a Style,
    pub powered: bool,
    pub power_icon: &'static str,
    pub scanning: bool,
    pub empty: bool,
    pub focus: Option<&'a str>,
    pub rows: Vec<Node>,
}

/// `#asideStatus button` with active / focus states; dimmed only while a connection is in progress.
pub fn aside_button(base: &Style, active: bool, connecting: bool, focused: bool) -> Style {
    let mut s = frosted(button(base));
    s.margin = Sides::all(SPACE_S);
    if active || connecting {
        s = button_active(s);
    }
    if focused {
        s = focus_outline(s);
    }
    if connecting {
        s.opacity = 0.35;
    }
    s
}

/// A list row: one outline width from the next, so a focused row's outline touches its neighbor.
pub fn aside_row(base: &Style, active: bool, connecting: bool, focused: bool) -> Style {
    let mut s = aside_button(base, active, connecting, focused);
    s.margin = Sides::trbl(0.0, SPACE_S, BORDER, SPACE_S);
    s
}

pub fn separator(base: &Style) -> Node {
    // as wide as a focused button's outline; the buttons above end SPACE_S + BORDER short of this margin too
    let s = Style { margin: Sides::trbl(SPACE_M + BORDER, SPACE_S - BORDER, SPACE_M, SPACE_S - BORDER), min_height: BORDER, background: Background::Color(light3()), ..base.inherit() };
    Node::hbox(s, 0.0, vec![])
}

pub fn device_menu(m: DeviceMenu) -> Node {
    let base = m.base;
    let show_list = m.powered && !m.empty;
    let mut power = aside_button(base, m.powered, false, m.focus == Some("power"));
    if !m.powered {
        power.padding = Sides::all(SPACE_L);
    }
    let icon_size = if m.powered { 16.0 } else { 48.0 };
    let power_btn = Node::hbox(power.clone(), 0.0, vec![Node::icon(power.inherit(), m.power_icon, icon_size)])
        .expand(true, false)
        .align(if m.powered { Align::Start } else { Align::Center }, Align::Fill)
        .focusable("power");
    let scan = aside_button(base, m.scanning, false, m.focus == Some("scan"));
    let scan_btn = Node::hbox(scan.clone(), 0.0, vec![Node::icon(scan.inherit(), "view-refresh-symbolic", 16.0)]).show(m.powered).focusable("scan");
    let center = m.rows.iter().position(|r| r.id.is_some() && r.id.as_deref() == m.focus);
    // SPACE_M + SPACE_S from the separator to a focused row's outline, which stays inside the clip
    let list = Node::vbox(Style { padding: Sides::trbl(SPACE_S + BORDER, 0.0, 0.0, 0.0), ..base.inherit() }, 0.0, m.rows);
    let scroll = Node::scroll(base.inherit(), 500.0, center, list).expand(true, true).show(show_list);
    // full width whatever the rows hold, so the scan button sits at the right edge in both menus
    Node::vbox(base.inherit(), 0.0, vec![Node::hbox(base.inherit(), 0.0, vec![power_btn, scan_btn]), separator(base).show(show_list), scroll]).expand(true, false)
}
