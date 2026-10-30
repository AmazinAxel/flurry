// Which centered status menu is open (lib/asideState.ts).

use crate::lib::state::State;
use crate::ui::{runtime, window};

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

/// Focuses `target()` in the menu window after layout, if `name` is still the open menu.
pub fn focus_menu(name: &'static str, target: impl Fn() -> Option<String> + 'static) {
    runtime::idle(move || {
        if current().peek() != Some(name) {
            return;
        }
        if let (Some(id), Some(w)) = (target(), window::get("menu"))
        {
            w.grab_focus(&id);
        }
    });
}
