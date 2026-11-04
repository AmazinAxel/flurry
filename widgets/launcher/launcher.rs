use std::rc::Rc;

use crate::lib::apps::{App, exec_argv, fuzzy_query, launch, reload_if_changed};
use crate::lib::searchDialog::{SearchSpec, search_dialog};
use crate::lib::style::*;
use crate::settings::FOCUS_BLOCKED;
use crate::ui::node::{Align, Node};
use crate::ui::window;
use crate::widgets::focus::{focus, start_break};

fn blockable(a: &App) -> bool {
    let name = a.name.to_lowercase();
    FOCUS_BLOCKED.iter().any(|b| name.contains(&b.to_lowercase()))
}

fn launch_app(a: &App) {
    window::toggle("launcher");
    launch(&exec_argv(&a.executable));
}

fn break_launch(a: &App) {
    if !focus() || !blockable(a) {
        return launch_app(a);
    }
    window::toggle("launcher");
    start_break(exec_argv(&a.executable));
}

pub fn launcher() {
    search_dialog("launcher", SearchSpec {
        search: Box::new(|text| if text.is_empty() { vec![] } else { fuzzy_query(text, 5) }),
        activate: Rc::new(launch_app),
        shift_activate: Some(Rc::new(break_launch)),
        row: Box::new(|a, s| {
            let mut l = s.inherit();
            l.color = light3();
            l.font.weight = 700;
            // app icons are not drawn; their 32px slot keeps the row height
            let mut n = Node::label(l, a.name.clone()).xalign(0.0).align(Align::Fill, Align::Center);
            n.style.min_height = 32.0;
            n
        }),
        sensitive: Some(Box::new(|a| !(focus() && blockable(a)))),
        on_show: Some(Rc::new(reload_if_changed)),
    });
}
