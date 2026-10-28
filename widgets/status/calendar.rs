// Month calendar (GtkCalendar look: header labels, weekday row, 6-week grid with adjacent days).

use std::cell::Cell;

use xkbcommon::xkb::Keysym;

use crate::lib::style::*;
use crate::lib::time::format;
use crate::ui::node::{Align, Background, Node, Shadow, Sides, Style, alpha, rgba};
use crate::ui::wayland::KeyEvent;

thread_local! {
    /// selected day as days since the unix epoch (local)
    static SELECTED: Cell<i64> = const { Cell::new(0) };
}

/// Civil date <-> day number (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// 0 = Sunday
fn weekday(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

fn today() -> i64 {
    let now = crate::lib::time::now().peek();
    let (y, m, d) = (format(now, "%Y").parse().unwrap_or(1970), format(now, "%m").parse().unwrap_or(1), format(now, "%d").parse().unwrap_or(1));
    days_from_civil(y, m, d)
}

/// set_date(now) when the calendar menu opens
pub fn reset() {
    SELECTED.with(|s| s.set(today()));
}

/// Arrow keys move the selected day (GtkCalendar keynav).
pub fn key(ev: &KeyEvent) -> bool {
    let delta = match ev.sym {
        Keysym::Left => -1,
        Keysym::Right => 1,
        Keysym::Up => -7,
        Keysym::Down => 7,
        _ => return false,
    };
    SELECTED.with(|s| s.set(s.get() + delta));
    true
}

pub fn calendar(base: &Style) -> Node {
    let sel = SELECTED.with(Cell::get);
    let today = today();
    let (y, m, _) = civil_from_days(sel);
    let first = days_from_civil(y, m, 1);
    let start = first - weekday(first);
    let line = rgba(255, 255, 255, 0.12);

    let mut cal = base.inherit();
    cal.color = light1();
    cal = cal.border(Sides::all(1.0), line);
    cal.min_width = 0.0;

    let mut header = cal.inherit();
    header.padding = Sides::all(3.0);
    header = header.border(Sides::trbl(0.0, 0.0, 1.0, 0.0), line);
    header.min_height = 24.0 + 12.0;
    let month = format(sel * 86400 + 43200, "%B");
    let head = Node::hbox(header.clone(), 0.0, vec![
        Node::label(header.inherit(), month).expand(true, false).align(Align::Fill, Align::Center),
        Node::label(Style { padding: Sides::vh(0.0, 10.0), ..header.inherit() }, y.to_string()).align(Align::Fill, Align::Center),
    ]);

    let mut names = cal.inherit();
    names.font.size = 14.0;
    // GtkCalendar's grid has homogeneous rows: the weekday row is as tall as a day row
    let sample = Node::label(Style { padding: Sides::trbl(9.0, 7.0, 9.0, 7.0), ..cal.inherit() }, "30");
    let row_h = sample.measure_h(sample.measure_w().1).1;
    names.min_height = row_h;
    let mut grid_rows = vec![];
    let mut names_row = Node::hbox(cal.inherit(), 0.0, (0..7).map(|i| Node::label(names.clone(), format((start + i) * 86400 + 43200, "%a")).expand(true, false)).collect());
    names_row.homogeneous = true;
    grid_rows.push(names_row);
    for week in 0..6 {
        let cells = (0..7)
            .map(|i| {
                let day = start + week * 7 + i;
                let (_, dm, dd) = civil_from_days(day);
                let mut s = cal.inherit();
                s.padding = Sides::trbl(9.0, 7.0, 9.0, 7.0);
                if dm != m {
                    s.color = alpha(s.color, 0.3);
                }
                if day == sel {
                    s.background = Background::Color(alpha(s.color, 0.06));
                }
                if day == today {
                    s.background = Background::Color(blue2());
                    s.box_shadow = vec![Shadow { inset: true, spread: 1.0, ..glow(0.0, blue1()) }, Shadow { spread: 3.0, ..glow(5.0, blue3()) }];
                    s.color = light1();
                    s.opacity = 0.8;
                }
                Node::label(s, dd.to_string()).expand(true, false)
            })
            .collect();
        let mut row = Node::hbox(cal.inherit(), 0.0, cells);
        row.homogeneous = true;
        grid_rows.push(row);
    }
    let grid = Node::vbox(Style { margin: Sides::all(3.0), ..cal.inherit() }, 0.0, grid_rows);
    Node::vbox(cal, 0.0, vec![head, grid]).expand(true, false).focusable("calendar")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip_and_weekday() {
        let d = days_from_civil(2026, 10, 27);
        assert_eq!(civil_from_days(d), (2026, 10, 27));
        assert_eq!(weekday(d), 2); // Tuesday
        assert_eq!(weekday(0), 4); // 1970-01-01 was a Thursday
    }
}
