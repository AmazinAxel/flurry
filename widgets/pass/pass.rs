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
    /// bumped per show and hide: a walk finishing after either is dropped
    static SHOWS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// pass's own rule: $PASSWORD_STORE_DIR, else ~/.password-store
pub(crate) fn store() -> String {
    std::env::var("PASSWORD_STORE_DIR").ok().filter(|d| !d.is_empty()).unwrap_or_else(|| format!("{}/.password-store", crate::lib::xdg::home_dir()))
}

fn walk(dir: &str, base: usize, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // .git, .gpg-id, ...
        }
        let p = format!("{dir}/{name}");
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, base, out),
            Ok(t) if t.is_file() && p.ends_with(".gpg") => out.push(p[base + 1..p.len() - 4].to_string()),
            _ => {}
        }
    }
}

/// `find STORE -type f -name '*.gpg'`, relative and without the extension, sorted; off the loop
/// (the dialog's first frame never waits on it, and searching starts at 2 chars)
fn load_entries() {
    let show = SHOWS.with(|s| {
        s.set(s.get() + 1);
        s.get()
    });
    runtime::spawn(async move {
        let out = runtime::blocking(|| {
            let s = store();
            let mut out = vec![];
            walk(&s, s.len(), &mut out);
            out.sort();
            out
        })
        .await;
        if let Some(out) = out
            && SHOWS.with(|s| s.get()) == show
        {
            ENTRIES.with(|e| *e.borrow_mut() = out);
        }
    });
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
        let mut out = match exec(&["pass", "show", "--", &name]).await {
            Ok(o) => o,
            Err(mut e) => {
                crate::lib::notifySend::notify_send("Pass", "pass show failed", e.trim(), vec![]);
                crate::ui::entry::wipe(&mut e);
                return;
            }
        };
        let mut lines = out.split('\n');
        let password = lines.next().unwrap_or("");
        if !password.is_empty() {
            // only a labelled login line: any other second line may be a secret (an OTP URI, a note)
            let user = lines.next().and_then(strip_login).filter(|u| !u.is_empty()).unwrap_or_else(|| name.rsplit('/').next().unwrap_or("").to_string());
            copy_login(&user, password);
        }
        crate::ui::entry::wipe(&mut out);
    });
}

/// The value of a `login:`/`username:`/`user:`/`email:` line (any case), trimmed; None for any other line.
pub fn strip_login(line: &str) -> Option<String> {
    let (key, value) = line.split_once(':')?;
    let key = key.trim();
    ["login", "username", "user", "email"].iter().any(|k| key.eq_ignore_ascii_case(k)).then(|| value.trim().to_string())
}

pub fn pass() {
    let w = search_dialog("pass", SearchSpec {
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
    // the store's names don't stay in memory while hidden
    let v = w.visibility.clone();
    w.visibility.subscribe(move || {
        if !v.peek() {
            SHOWS.with(|s| s.set(s.get() + 1));
            ENTRIES.with(|e| *e.borrow_mut() = Vec::new());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_prefix() {
        assert_eq!(strip_login("  Login: me@x ").as_deref(), Some("me@x"));
        assert_eq!(strip_login("user:bob").as_deref(), Some("bob"));
        assert_eq!(strip_login("plain"), None);
        assert_eq!(strip_login("otpauth://totp/x?secret=y"), None);
    }

    #[test]
    fn search_orders_by_match_position() {
        ENTRIES.with(|e| *e.borrow_mut() = vec!["web/example.net".into(), "example.org".into(), "zzz".into()]);
        assert_eq!(search("ex"), vec!["example.org".to_string(), "web/example.net".to_string()]);
        assert!(search("e").is_empty());
    }
}
