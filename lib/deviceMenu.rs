// Power/scan header + scrolling device list shared by bluetooth and wifi (lib/deviceMenu.tsx).

use crate::lib::style::*;
use crate::ui::node::{Align, Background, Kind, Node, Sides, Style};

pub struct DeviceMenu<'a> {
    pub base: &'a Style,
    pub powered: bool,
    pub power_icon: &'static str,
    pub scanning: bool,
    pub scan_sensitive: bool,
    pub empty: bool,
    pub focus: Option<&'a str>,
    pub scroll: f32,
    pub rows: Vec<Node>,
}

/// `#asideStatus button` with active / focus / disabled states.
pub fn aside_button(base: &Style, active: bool, connecting: bool, focused: bool, sensitive: bool) -> Style {
    let mut s = button(base);
    s.margin = Sides::all(4.0);
    if active {
        s = button_active(s);
    }
    if connecting {
        s.background = Background::Color(blue3());
        s.border_color = [blue2(); 4];
    }
    if focused {
        s = focus_outline(button_active(s));
    }
    if !sensitive {
        s.opacity = 0.35;
    }
    s
}

pub fn separator(base: &Style) -> Node {
    let s = Style { margin: Sides::vh(10.0, 0.0), min_height: 2.0, background: Background::Color(light1()), ..base.inherit() };
    Node::hbox(s, 0.0, vec![])
}

pub fn device_menu(m: DeviceMenu) -> Node {
    let base = m.base;
    let show_list = m.powered && !m.empty;
    let mut power = aside_button(base, m.powered, false, m.focus == Some("power"), true);
    if !m.powered {
        power.padding = Sides::all(32.0);
    }
    let icon_size = if m.powered { 16.0 } else { 48.0 };
    let power_btn = Node::hbox(power.clone(), 0.0, vec![Node::icon(power.inherit(), m.power_icon, icon_size)])
        .expand(true, false)
        .align(if m.powered { Align::Start } else { Align::Center }, Align::Fill)
        .focusable("power");
    let scan = aside_button(base, m.scanning, false, m.focus == Some("scan"), m.scan_sensitive);
    let scan_btn = Node::hbox(scan.clone(), 0.0, vec![Node::icon(scan.inherit(), "view-refresh-symbolic", 16.0)]).show(m.powered).focusable("scan");
    let list = Node::vbox(Style { padding: Sides::all(5.0), ..base.inherit() }, 0.0, m.rows);
    let scroll = Node { children: vec![list], ..Node::new(Kind::Scroll { max_height: 500.0, offset: m.scroll }, base.inherit()) }.expand(true, true).show(show_list);
    Node::vbox(base.inherit(), 0.0, vec![Node::hbox(base.inherit(), 0.0, vec![power_btn, scan_btn]), separator(base).show(show_list), scroll])
}
