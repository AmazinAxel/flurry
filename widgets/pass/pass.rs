use std::cell::RefCell;
use std::rc::Rc;

use crate::lib::clipboard::copy_login;
use crate::lib::process::exec;
use crate::lib::searchDialog::{SearchSpec, search_dialog};
use crate::lib::style::*;
use crate::ui::node::{Align, Node};
use crate::ui::{runtime, window};

thread_local! {
    static ENTRIES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn store() -> String {
    format!("{}/.password-store", crate::lib::xdg::home_dir())
}

fn walk(dir: &str, base: usize, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = format!("{dir}/{}", e.file_name().to_string_lossy());
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, base, out),
            Ok(t) if t.is_file() && p.ends_with(".gpg") => out.push(p[base + 1..p.len() - 4].to_string()),
            _ => {}
        }
    }
}

/// `find STORE -type f -name '*.gpg'`, relative and without the extension, sorted
fn load_entries() {
    let s = store();
    let mut out = vec![];
    walk(&s, s.len(), &mut out);
    out.sort();
    ENTRIES.with(|e| *e.borrow_mut() = out);
}

pub fn search(q: &str) -> Vec<String> {
    if q.chars().count() < 2 {
        return vec![];
    }
    let ql = q.to_lowercase();
    let mut hits: Vec<(String, usize)> = ENTRIES.with(|e| e.borrow().iter().filter_map(|n| n.to_lowercase().find(&ql).map(|i| (n.clone(), i))).collect());
    hits.sort_by_key(|h| h.1);
    hits.into_iter().take(5).map(|h| h.0).collect()
}

fn copy_pair(name: &str) {
    window::set_visible("pass", false);
    let name = name.to_string();
    runtime::spawn(async move {
        let out = exec(&["pass", "show", &name]).await.unwrap_or_default();
        let mut lines = out.split('\n');
        let password = lines.next().unwrap_or("").to_string();
        let login = lines.next().unwrap_or("");
        if password.is_empty() {
            return;
        }
        let stripped = strip_login(login);
        let user = if stripped.is_empty() { name.rsplit('/').next().unwrap_or("").to_string() } else { stripped };
        copy_login(&user, &password).await;
    });
}

/// `/^\s*(login|username|user|email):\s*/i` removed, trimmed
pub fn strip_login(line: &str) -> String {
    let t = line.trim_start();
    let lower = t.to_lowercase();
    for key in ["login", "username", "user", "email"] {
        if let Some(rest) = lower.strip_prefix(key)
            && rest.starts_with(':')
        {
            return t[key.len() + 1..].trim().to_string();
        }
    }
    line.trim().to_string()
}

pub fn pass() {
    load_entries();
    search_dialog("pass", SearchSpec {
        placeholder: "Passwords",
        search: Box::new(search),
        activate: Rc::new(|n: &String| copy_pair(n)),
        shift_activate: None,
        row: Box::new(|n, s| {
            let mut l = s.inherit();
            l.color = light3();
            l.font.weight = 700;
            Node::label(l, n.clone()).xalign(0.0).align(Align::Fill, Align::Center)
        }),
        sensitive: None,
        on_show: Some(Rc::new(load_entries)),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_prefix() {
        assert_eq!(strip_login("  Login: me@x "), "me@x");
        assert_eq!(strip_login("user:bob"), "bob");
        assert_eq!(strip_login("plain"), "plain");
    }

    #[test]
    fn search_orders_by_match_position() {
        ENTRIES.with(|e| *e.borrow_mut() = vec!["web/example.net".into(), "example.org".into(), "zzz".into()]);
        assert_eq!(search("ex"), vec!["example.org".to_string(), "web/example.net".to_string()]);
        assert!(search("e").is_empty());
    }
}
