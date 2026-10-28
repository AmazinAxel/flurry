use std::rc::Rc;

use crate::lib::apps::{App, fuzzy_query};
use crate::lib::searchDialog::{SearchSpec, search_dialog};
use crate::lib::style::*;
use crate::lib::sway;
use crate::ui::node::{Align, Node};
use crate::ui::window;
use crate::widgets::focus::{focus, start_break};

const FOCUS_BLOCKED: [&str; 1] = ["discord"];

fn blockable(a: &App) -> bool {
    FOCUS_BLOCKED.iter().any(|b| a.name.to_lowercase().contains(b))
}

/// Exec without field codes (` %U`, `%f`, ...)
pub fn app_cmd(a: &App) -> String {
    let mut out = String::new();
    let mut chars = a.executable.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' && chars.peek().is_some_and(char::is_ascii_alphabetic) {
            chars.next();
            if out.ends_with(' ') {
                out.pop();
            }
            continue;
        }
        out.push(c);
    }
    out.trim().to_string()
}

fn launch_app(a: &App) {
    if focus() && blockable(a) {
        return;
    }
    window::toggle("launcher");
    sway::command(&format!("exec {}", app_cmd(a)));
}

fn break_launch(a: &App) {
    if !focus() || !blockable(a) {
        return launch_app(a);
    }
    window::toggle("launcher");
    start_break(&app_cmd(a));
}

pub fn launcher() {
    search_dialog("launcher", SearchSpec {
        placeholder: "Search",
        search: Box::new(|text| if text.is_empty() { vec![] } else { fuzzy_query(text).into_iter().take(5).collect() }),
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
        on_show: None,
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_field_codes() {
        let a = crate::lib::apps::App { executable: "foot %U --x %f".into(), ..Default::default() };
        assert_eq!(super::app_cmd(&a), "foot --x");
    }
}
