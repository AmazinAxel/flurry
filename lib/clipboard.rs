// wl-copy / cliphist helpers (lib/clipboard.ts).

use std::cell::RefCell;
use std::collections::HashSet;

use crate::lib::process;

pub async fn pipe(argv: &[&str], input: &str) -> bool {
    process::run_input(argv, input.as_bytes()).await
}

pub fn copy_typed(text: &str, mime: &str) {
    let (text, mime) = (text.to_string(), mime.to_string());
    crate::ui::runtime::spawn(async move {
        pipe(&["wl-copy", "-n", "-t", &mime], &text).await;
    });
}

pub fn copy(text: &str) {
    copy_typed(text, "text/plain");
}

pub fn uri(path: &str) -> String {
    let mut s = String::from("file://");
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*'
            | b'+' | b',' | b';' | b'=' | b':' | b'@' => s.push(b as char),
            _ => s += &format!("%{b:02X}"),
        }
    }
    s
}

pub fn copy_uri(path: &str) {
    copy_typed(&(uri(path) + "\r\n"), "text/uri-list");
}

pub async fn copy_login(user: &str, password: &str) {
    pipe(&["cliphist", "store"], user).await;
    copy_sensitive(password).await;
}

thread_local! {
    /// History ids shown as [ Sensitive ], loaded on first use.
    static SENSITIVE: RefCell<Option<HashSet<u64>>> = const { RefCell::new(None) };
}

/// Beside cliphist's db, so the marks live and die with the entries they hide.
fn sensitive_file() -> String {
    format!("{}/cliphist/.flurry-sensitive", crate::lib::xdg::cache_dir())
}

fn with_sensitive<R>(f: impl FnOnce(&mut HashSet<u64>) -> R) -> R {
    SENSITIVE.with(|s| {
        let mut s = s.borrow_mut();
        f(s.get_or_insert_with(|| std::fs::read_to_string(sensitive_file()).unwrap_or_default().lines().filter_map(|l| l.parse().ok()).collect()))
    })
}

fn save_sensitive(ids: &HashSet<u64>) {
    let text: String = ids.iter().map(|i| format!("{i}\n")).collect();
    let _ = std::fs::write(sensitive_file(), text);
}

pub fn is_sensitive(id: u64) -> bool {
    with_sensitive(|s| s.contains(&id))
}

pub fn set_sensitive(id: u64, on: bool) {
    with_sensitive(|s| {
        if if on { s.insert(id) } else { s.remove(&id) } {
            save_sensitive(s);
        }
    });
}

/// Forgets marks of entries no longer in the history.
pub fn keep_sensitive(ids: &[u64]) {
    with_sensitive(|s| {
        let before = s.len();
        s.retain(|i| ids.contains(i));
        if s.len() != before {
            save_sensitive(s);
        }
    });
}

/// Stored in the history masked, then copied with the sensitive hint (cliphist's watcher skips those).
pub async fn copy_sensitive(text: &str) {
    if pipe(&["cliphist", "store"], text).await
        && let Ok(list) = process::exec(&["cliphist", "list"]).await
        && let Some(id) = list.lines().next().and_then(|l| l.split('\t').next()?.parse().ok())
    {
        set_sensitive(id, true);
    }
    pipe(&["wl-copy", "-n", "--sensitive", "-t", "text/plain"], text).await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn uri_escapes_like_glib() {
        assert_eq!(super::uri("/tmp/a b/ü.mp4"), "file:///tmp/a%20b/%C3%BC.mp4");
    }
}
