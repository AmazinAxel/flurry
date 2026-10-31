// Which centered status menu is open (lib/asideState.ts).

use crate::lib::state::State;
use crate::ui::window;

thread_local! {
    static CURRENT: State<Option<&'static str>> = State::new(None);
}

pub fn current() -> State<Option<&'static str>> {
    CURRENT.with(Clone::clone)
}

pub fn toggle_menu(name: &'static str) {
    let c = current();
    c.set(if c.peek() == Some(name) { None } else { Some(name) });
}

pub fn close_menu() {
    current().set(None);
}

/// Focuses `target()` in the menu window if `name` is the open menu. Right now, never deferred: the
/// redraw is still queued, so the change and its focus land in one frame.
pub fn focus_menu(name: &'static str, target: impl Fn() -> Option<String>) {
    if current().peek() != Some(name) {
        return;
    }
    if let (Some(id), Some(w)) = (target(), window::get("menu")) {
        w.grab_focus(&id);
    }
}
