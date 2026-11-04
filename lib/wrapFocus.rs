use xkbcommon::xkb::Keysym;

use crate::ui::wayland::KeyEvent;
use crate::ui::window::{Dir, Win};

/// Arrows move focus (with wrap) unless a modifier is held or the focused widget consumes them
/// (entries own Left/Right and typed letters). hjkl work like arrows outside entries.
pub fn wrap_focus(win: &Win, ev: &KeyEvent, entry_focused: bool) -> bool {
    let vim = matches!(ev.sym, Keysym::h | Keysym::j | Keysym::k | Keysym::l);
    let dir = match ev.sym {
        Keysym::Up | Keysym::k => Dir::Up,
        Keysym::Down | Keysym::j => Dir::Down,
        Keysym::Left | Keysym::h => Dir::Left,
        Keysym::Right | Keysym::l => Dir::Right,
        _ => return false,
    };
    if win.focused().is_none() || ev.ctrl || ev.alt || ev.shift || (entry_focused && (vim || matches!(dir, Dir::Left | Dir::Right))) {
        return false;
    }
    win.wrap_focus(dir);
    true
}

/// Index after moving by `delta` with wrap-around; -1 (nothing selected) moving up lands on the last.
pub fn wrap_index(i: i64, delta: i64, len: i64) -> i64 {
    ((if i < 0 && delta < 0 { 0 } else { i }) + delta).rem_euclid(len.max(1))
}

#[cfg(test)]
mod tests {
    use super::wrap_index;

    #[test]
    fn wraps_both_ways() {
        assert_eq!(wrap_index(0, -1, 3), 2);
        assert_eq!(wrap_index(2, 1, 3), 0);
        assert_eq!(wrap_index(-1, 1, 3), 0);
        assert_eq!(wrap_index(-1, -1, 3), 2);
        assert_eq!(wrap_index(0, -7, 3), 2);
        assert_eq!(wrap_index(1, 7, 3), 2);
    }
}
