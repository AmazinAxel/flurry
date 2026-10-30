// Month calendar (GtkCalendar look: header labels, weekday row, 6-week grid with adjacent days).

use std::cell::Cell;

use xkbcommon::xkb::Keysym;

use crate::lib::style::*;
use crate::lib::time::format;
use crate::ui::node::{Align, Background, Node, Sides, Style, rgba};
use crate::ui::wayland::KeyEvent;

thread_local! {
    /// shown month as year * 12 + month - 1
    static SHOWN: Cell<i64> = const { Cell::new(0) };
}

/// Civil date <-> day number (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
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

/// Today as a day number (local).
fn today() -> i64 {
    let ymd = format(crate::lib::time::now().peek(), "%Y %m %d");
    let mut it = ymd.split(' ').map(|v| v.parse().unwrap_or(1));
    let (y, m, d) = (it.next().unwrap_or(1970), it.next().unwrap_or(1), it.next().unwrap_or(1));
    days_from_civil(y, m, d)
}

/// Back to the current month when the calendar menu opens.
pub fn reset() {
    let (y, m, _) = civil_from_days(today());
    SHOWN.with(|s| s.set(y * 12 + m - 1));
}

/// Left/Right (h/l) switch months, with Ctrl years.
pub fn key(ev: &KeyEvent) -> bool {
    let dir = match ev.sym {
        Keysym::Left | Keysym::h => -1,
        Keysym::Right | Keysym::l => 1,
        _ => return false,
    };
    SHOWN.with(|s| s.set(s.get() + dir * if ev.ctrl { 12 } else { 1 }));
    true
}

/// strftime on a day number (noon, so DST never shifts the date).
fn day_format(day: i64, fmt: &str) -> String {
    format(day * 86400 + 43200, fmt)
}

/// Seven equal-width cells.
fn week_row(style: &Style, cells: Vec<Node>) -> Node {
    let mut row = Node::hbox(style.clone(), 0.0, cells);
    row.homogeneous = true;
    row
}

pub fn calendar(base: &Style) -> Node {
    let shown = SHOWN.with(Cell::get);
    let (y, m) = (shown.div_euclid(12), shown.rem_euclid(12) + 1);
    let first = days_from_civil(y, m, 1);
    let start = first - weekday(first); // grid starts on the Sunday on or before the 1st
    let today = today();
    let mut cal = frosted(base.inherit());
    cal.min_width = 0.0;
    cal.background = Background::None;
    let cell = Style { padding: Sides::trbl(9.0, 7.0, 9.0, 7.0), ..cal.inherit() };

    // "October 2026", left-aligned
    let mut header = cal.inherit();
    header.padding = Sides::vh(3.0, 10.0);
    header.min_height = 36.0;
    let title = Node::label(header.inherit(), day_format(first, "%B %Y")).xalign(0.0).expand(true, false).align(Align::Fill, Align::Center);
    let head = Node::hbox(header, 0.0, vec![title]);

    // weekday names, as tall as a day row (GtkCalendar's grid rows are homogeneous)
    let sample = Node::label(cell.clone(), "30");
    let mut names = Style { min_height: sample.measure_h(sample.measure_w().1).1, ..cal.inherit() };
    names.font.size = 14.0;
    let mut rows = vec![week_row(&cal.inherit(), (0..7).map(|i| Node::label(names.clone(), day_format(start + i, "%a")).expand(true, false)).collect())];

    for week in 0..6 {
        let cells = (0..7)
            .map(|i| {
                let day = start + week * 7 + i;
                let (_, dm, dd) = civil_from_days(day);
                let mut s = cell.clone();
                if dm != m {
                    s.color = rgba(255, 255, 255, 0.5); // adjacent months greyed
                }
                if day == today {
                    s.background = Background::Color(blue3());
                    s.color = light1();
                }
                Node::label(s, dd.to_string()).expand(true, false)
            })
            .collect();
        rows.push(week_row(&cal.inherit(), cells));
    }
    let grid = Node::vbox(Style { margin: Sides::all(3.0), ..cal.inherit() }, 0.0, rows);
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
