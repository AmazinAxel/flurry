// Copying and the clipboard history, in-process over wlr-data-control (lib/clipboard.ts + cliphist).
// History: one file per entry (raw bytes, named by content hash) and an index of `id\tmime\tpreview`
// lines, newest first, in $XDG_CACHE_HOME/flurry/clipboard.

use std::cell::RefCell;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::rc::Rc;
use std::sync::Arc;

use crate::ui::wayland;

const TEXT: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"];
/// Offered with secrets: clipboard managers (this history too) skip such selections.
const SECRET_HINT: &str = "x-kde-passwordManagerHint";
/// Entries kept; the oldest go first.
const CAP: usize = 500;
/// Bigger selections aren't recorded.
const MAX_BYTES: usize = 32 << 20;

fn offer(mimes: &[&str], data: &[u8]) -> Vec<(String, Arc<[u8]>)> {
    let data: Arc<[u8]> = data.into();
    mimes.iter().map(|m| (m.to_string(), data.clone())).collect()
}

fn set(offers: Vec<(String, Arc<[u8]>)>) -> Option<u64> {
    wayland::set_selection(Arc::new(offers))
}

pub fn copy(text: &str) {
    set(offer(&TEXT, text.as_bytes()));
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
    set(offer(&["text/uri-list"], (uri(path) + "\r\n").as_bytes()));
}

pub fn copy_login(user: &str, password: &str) {
    record("text/plain", user.as_bytes());
    copy_sensitive(password);
}

/// Never stored in the history; cleared after 45s if the clipboard still holds it.
pub fn copy_sensitive(text: &str) {
    let mut offers = offer(&TEXT, text.as_bytes());
    offers.push((SECRET_HINT.into(), Arc::from(&b"secret"[..])));
    if let Some(token) = set(offers) {
        let _ = crate::ui::runtime::timeout(45_000, move || wayland::clear_selection(token));
    }
}

/// Copies a history entry back as what it was copied as.
pub fn copy_entry(id: u64, mime: &str) {
    let Ok(data) = std::fs::read(entry_file(id, mime)) else { return };
    let mut mimes = if mime.starts_with("image/") { vec![mime] } else { TEXT.to_vec() };
    if data.trim_ascii_start().starts_with(b"file://") {
        mimes.insert(0, "text/uri-list"); // paste the ACTUAL file
    }
    set(offer(&mimes, &data));
}

// ---------------------------------------------------------------- history

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: u64,
    pub mime: String,
    /// text: whitespace collapsed, cut at 100 chars; images: WxH
    pub preview: String,
}

thread_local! {
    /// newest first, read on first use
    static HISTORY: RefCell<Option<Vec<Entry>>> = const { RefCell::new(None) };
    static ON_CHANGE: RefCell<Option<Rc<dyn Fn()>>> = RefCell::default();
}

fn dir() -> String {
    format!("{}/flurry/clipboard", crate::lib::xdg::cache_dir())
}

/// Images keep an extension so editors and viewers open the file as is.
pub fn entry_file(id: u64, mime: &str) -> String {
    let ext = match mime {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        _ => "",
    };
    format!("{}/{id}{ext}", dir())
}

fn parse_index(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            Some(Entry { id: p.next()?.parse().ok()?, mime: p.next()?.into(), preview: p.next()?.into() })
        })
        .collect()
}

fn format_index(entries: &[Entry]) -> String {
    entries.iter().map(|e| format!("{}\t{}\t{}\n", e.id, e.mime, e.preview)).collect()
}

fn with_history<R>(f: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
    HISTORY.with(|h| f(h.borrow_mut().get_or_insert_with(|| parse_index(&std::fs::read_to_string(format!("{}/index", dir())).unwrap_or_default()))))
}

pub fn history() -> Vec<Entry> {
    with_history(|h| h.clone())
}

fn write_private(path: &str, data: &[u8]) -> bool {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path).and_then(|mut f| f.write_all(data)).is_ok()
}

fn changed() {
    if let Some(f) = ON_CHANGE.with(|c| c.borrow().clone()) {
        f();
    }
}

fn image_dims(b: &[u8]) -> Option<(u32, u32)> {
    match b {
        [0x89, b'P', b'N', b'G', ..] if b.len() >= 24 => Some((u32::from_be_bytes(b[16..20].try_into().ok()?), u32::from_be_bytes(b[20..24].try_into().ok()?))),
        [0xff, 0xd8, ..] => {
            let mut d = zune_jpeg::JpegDecoder::new(b);
            d.decode_headers().ok()?;
            d.dimensions().map(|(w, h)| (w as u32, h as u32))
        }
        _ => None,
    }
}

fn preview(mime: &str, data: &[u8]) -> Option<String> {
    if mime.starts_with("image/") {
        return image_dims(data).map(|(w, h)| format!("{w}x{h}"));
    }
    let text = String::from_utf8_lossy(&data[..data.len().min(4096)]).split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(100) {
        Some((i, _)) => Some(format!("{}…", &text[..i])),
        None => (!text.is_empty()).then_some(text),
    }
}

/// Identical content moves to the top instead of repeating.
fn record(mime: &str, data: &[u8]) {
    let Some(preview) = preview(mime, data) else { return };
    let id = crate::ui::node::hash(&data);
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir());
    let dropped = with_history(|h| {
        match h.iter().position(|e| e.id == id) {
            Some(0) => return None,
            Some(i) => {
                let e = h.remove(i);
                h.insert(0, e);
            }
            None if write_private(&entry_file(id, mime), data) => h.insert(0, Entry { id, mime: mime.into(), preview }),
            None => return None,
        }
        let dropped = h.split_off(h.len().min(CAP));
        // atomic: a crash mid-write never loses the history
        let (file, tmp) = (format!("{}/index", dir()), format!("{}/index.tmp", dir()));
        if write_private(&tmp, format_index(h).as_bytes()) {
            let _ = std::fs::rename(&tmp, &file);
        }
        Some(dropped)
    });
    let Some(dropped) = dropped else { return };
    for e in dropped {
        let _ = std::fs::remove_file(entry_file(e.id, &e.mime));
    }
    changed();
}

pub fn wipe() {
    let _ = std::fs::remove_dir_all(dir());
    with_history(Vec::clear);
    changed();
}

/// Records every new selection from here on (text preferred, then file lists, then images);
/// `on_change` runs whenever the history changes.
pub fn watch(on_change: impl Fn() + 'static) {
    ON_CHANGE.with(|c| *c.borrow_mut() = Some(Rc::new(on_change)));
    wayland::on_selection(|mimes| {
        let has = |m: &str| mimes.iter().any(|x| x == m);
        if has(SECRET_HINT) {
            return;
        }
        let Some(mime) = TEXT.into_iter().chain(["text/uri-list", "image/png", "image/jpeg"]).find(|m| has(m)) else { return };
        let stored = if TEXT.contains(&mime) { "text/plain" } else { mime };
        wayland::read_selection(mime, MAX_BYTES, 5000, move |data| {
            if let Some(d) = data {
                record(stored, &d);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_escapes_like_glib() {
        assert_eq!(uri("/tmp/a b/ü.mp4"), "file:///tmp/a%20b/%C3%BC.mp4");
    }

    #[test]
    fn index_round_trips() {
        let e = vec![
            Entry { id: 7, mime: "text/plain".into(), preview: "a\u{a0}b".into() },
            Entry { id: 1, mime: "image/png".into(), preview: "1x1".into() },
        ];
        assert_eq!(parse_index(&format_index(&e)), e);
        // a torn or foreign line is skipped, never a panic
        assert_eq!(parse_index("x\ty\tz\n3\ttext/plain\n").len(), 0);
        assert_eq!(preview("text/plain", b"  second line\n\tthird line ").as_deref(), Some("second line third line"));
        assert_eq!(preview("text/plain", " \n".as_bytes()), None);
        assert_eq!(preview("text/plain", "é".repeat(101).as_bytes()), Some(format!("{}…", "é".repeat(100))));
        let png = [&[0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R'][..], &1920u32.to_be_bytes(), &1080u32.to_be_bytes()].concat();
        assert_eq!(preview("image/png", &png).as_deref(), Some("1920x1080"));
        assert_eq!(preview("image/png", b"not an image"), None);
        assert_eq!(entry_file(5, "image/jpeg"), format!("{}/5.jpg", dir()));
    }
}
