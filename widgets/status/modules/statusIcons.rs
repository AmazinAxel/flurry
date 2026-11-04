use crate::lib::battery::battery;
use crate::lib::bluetooth::bt;
use crate::lib::iwd;
use crate::lib::streamingMode::streaming_mode;
use crate::lib::style::*;
use crate::ui::node::{Align, Node, Style, alpha};

/// Icon for the iwd station state.
fn icon_for(state: &str) -> &'static str {
    match state {
        "connected" | "roaming" => "network-wireless-symbolic",
        "connecting" | "disconnecting" => "network-wireless-acquiring-symbolic",
        _ => "network-wireless-offline-symbolic",
    }
}

fn img(base: &Style, name: &'static str) -> Node {
    let mut s = base.inherit();
    s.color = light1();
    Node::icon(s, name, 36.0)
}

/// Battery percentage, or the full icon from 99%; hidden without a battery.
pub fn battery_percent(base: &Style) -> Node {
    let b = battery().peek();
    let has = b.present;
    let pct = (b.percentage * 100.0).round() as i64;
    if pct >= 99 {
        return img(base, "battery-level-100-symbolic").show(has);
    }
    let mut pct_style = base.inherit();
    pct_style.font.size = 24.0;
    pct_style.font.weight = 700;
    pct_style.color = if b.charging() { green() } else { alpha(blue1(), 0.9) };
    Node::label(pct_style, pct.to_string()).align(Align::Center, Align::Center).show(has)
}

pub fn status(base: &Style) -> Node {
    let mut rec = base.inherit();
    rec.color = red();

    // one evenly spaced column, centered on the row whatever is shown
    Node::vbox(base.inherit(), SPACE_M, vec![
        Node::icon(rec, "media-record-symbolic", 36.0).show(crate::widgets::record::service::is_rec().peek()),
        battery_percent(base),
        img(base, iwd::state().with(|s| icon_for(s))),
        img(base, "bluetooth-active-symbolic").show(bt().with(|b| b.powered())),
        img(base, "notifications-disabled-symbolic").show(streaming_mode().peek()),
    ])
    .align(Align::Center, Align::Center)
}
