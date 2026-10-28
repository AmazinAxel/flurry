// wl-copy / cliphist helpers (lib/clipboard.ts).

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
    copy(password);
}

#[cfg(test)]
mod tests {
    #[test]
    fn uri_escapes_like_glib() {
        assert_eq!(super::uri("/tmp/a b/ü.mp4"), "file:///tmp/a%20b/%C3%BC.mp4");
    }
}
