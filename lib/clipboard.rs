// Copying and the clipboard history, in-process over wlr-data-control (lib/clipboard.ts + cliphist).
// History: one file per entry (raw bytes, named by content hash) and an index of
// `id\tmime\tlen\tpreview` lines, newest first, in $XDG_CACHE_HOME/flurry/clipboard. Hashing and
// disk writes run on worker threads, one job at a time; HISTORY itself only changes on the loop.

use std::cell::{Cell, RefCell};
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
/// All entry files together; the oldest go first past it.
const BUDGET: u64 = 256 << 20;

fn offer(mimes: &[&str], data: &[u8]) -> Vec<(String, Arc<[u8]>)> {
    let data: Arc<[u8]> = data.into();
    mimes.iter().map(|m| (m.to_string(), data.clone())).collect()
}

/// Selections set here list `wayland::OWN_MIME`: the history records them directly instead of
/// reading them back through a pipe.
fn set(offers: Vec<(String, Arc<[u8]>)>) -> Option<u64> {
    wayland::set_selection(Arc::new(offers))
}

pub fn copy(text: &str) {
    set(offer(&TEXT, text.as_bytes()));
    record("text/plain", text.as_bytes().to_vec());
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
    let data = uri(path) + "\r\n";
    set(offer(&["text/uri-list"], data.as_bytes()));
    record("text/uri-list", data.into_bytes());
}

pub fn copy_login(user: &str, password: &str) {
    record("text/plain", user.as_bytes().to_vec());
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
    if mime == "text/uri-list" {
        mimes.insert(0, mime); // paste the ACTUAL file
    }
    set(offer(&mimes, &data));
    promote(id);
}

/// Moves the entry to the top under its own id (older entries' ids aren't the content hash).
fn promote(id: u64) {
    crate::ui::runtime::spawn(async move {
        let disk = DISK.with(Clone::clone);
        let _turn = disk.lock().await;
        let index = with_history(|h| {
            let i = h.iter().position(|e| e.id == id).filter(|&i| i > 0)?;
            h[..=i].rotate_right(1);
            Some(format_index(h))
        });
        let Some(index) = index else { return };
        changed();
        crate::ui::runtime::blocking(move || write_atomic(&format!("{}/index", dir()), index.as_bytes())).await;
    });
}

// ---------------------------------------------------------------- history

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: u64,
    pub mime: String,
    /// the entry file's size
    pub len: u64,
    /// text: whitespace collapsed, cut at 100 chars; images: WxH
    pub preview: String,
}

thread_local! {
    /// newest first, read on first use
    static HISTORY: RefCell<Option<Vec<Entry>>> = const { RefCell::new(None) };
    static ON_CHANGE: RefCell<Option<Rc<dyn Fn()>>> = RefCell::default();
    /// disk jobs (records, wipes, cleanup), one at a time and in order
    static DISK: Rc<crate::ui::runtime::Queue> = Rc::default();
    /// bumped by wipe: a record started before it is dropped
    static WIPES: Cell<u64> = const { Cell::new(0) };
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
            // previews never hold tabs: 3 fields is the older index without sizes
            let p: Vec<&str> = l.splitn(4, '\t').collect();
            let (id, mime) = (p.first()?.parse().ok()?, p.get(1)?.to_string());
            let (len, preview) = match p[..] {
                [_, _, len, preview] => (len.parse().ok()?, preview),
                [_, _, preview] => (std::fs::metadata(entry_file(id, &mime)).map(|m| m.len()).unwrap_or(0), preview),
                _ => return None,
            };
            Some(Entry { id, mime, len, preview: preview.into() })
        })
        .collect()
}

fn format_index(entries: &[Entry]) -> String {
    entries.iter().map(|e| format!("{}\t{}\t{}\t{}\n", e.id, e.mime, e.len, e.preview)).collect()
}

fn with_history<R>(f: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
    HISTORY.with(|h| f(h.borrow_mut().get_or_insert_with(|| parse_index(&std::fs::read_to_string(format!("{}/index", dir())).unwrap_or_default()))))
}

/// FNV-1a 64: stable across builds, so ids (file names) stay valid.
fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

pub fn history() -> Vec<Entry> {
    with_history(|h| h.clone())
}

fn write_private(path: &str, data: &[u8]) -> bool {
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path).and_then(|mut f| f.write_all(data)).is_ok()
}

/// tmp + rename: a crash mid-write never leaves a torn file under the real name.
fn write_atomic(path: &str, data: &[u8]) -> bool {
    let tmp = format!("{path}.tmp");
    write_private(&tmp, data) && std::fs::rename(&tmp, path).is_ok()
}

fn changed() {
    if let Some(f) = ON_CHANGE.with(|c| c.borrow().clone()) {
        f();
    }
}

fn preview(mime: &str, data: &[u8]) -> Option<String> {
    if mime.starts_with("image/") {
        return crate::ui::image::dims(data).map(|(w, h)| format!("{w}x{h}"));
    }
    let text = String::from_utf8_lossy(&data[..data.len().min(4096)]).split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(100) {
        Some((i, _)) => Some(format!("{}…", &text[..i])),
        None => (!text.is_empty()).then_some(text),
    }
}

/// Identical content moves to the top instead of repeating.
fn record(mime: &str, data: Vec<u8>) {
    if data.len() > MAX_BYTES {
        return;
    }
    let mime = mime.to_string();
    let wipes = WIPES.with(Cell::get);
    crate::ui::runtime::spawn(async move {
        let disk = DISK.with(Clone::clone);
        let _turn = disk.lock().await;
        // files the history already has needn't be written again
        let known: Vec<(u64, u64)> = with_history(|h| h.iter().map(|e| (e.id, e.len)).collect());
        let m = mime.clone();
        let entry = crate::ui::runtime::blocking(move || {
            let preview = preview(&m, &data)?;
            let (id, len) = (fnv1a(&data), data.len() as u64);
            if !known.contains(&(id, len)) {
                let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir());
                if !write_atomic(&entry_file(id, &m), &data) {
                    return None;
                }
            }
            Some(Entry { id, mime: m, len, preview })
        })
        .await
        .flatten();
        let Some(entry) = entry else { return };
        if WIPES.with(Cell::get) != wipes {
            return;
        }
        let update = with_history(|h| {
            if h.first().is_some_and(|e| e.id == entry.id && e.len == entry.len) {
                return None;
            }
            // same id, other length: a hash collision whose file was just replaced
            h.retain(|e| e.id != entry.id);
            h.insert(0, entry);
            let mut keep = h.len().min(CAP);
            let mut total = 0;
            for (i, e) in h.iter().enumerate().take(keep) {
                total += e.len;
                if total > BUDGET && i > 0 {
                    keep = i;
                    break;
                }
            }
            let dropped = h.split_off(keep);
            Some((format_index(h), dropped))
        });
        let Some((index, dropped)) = update else { return };
        changed();
        crate::ui::runtime::blocking(move || {
            // files go only once the index no longer names them
            if write_atomic(&format!("{}/index", dir()), index.as_bytes()) {
                for e in dropped {
                    let _ = std::fs::remove_file(entry_file(e.id, &e.mime));
                }
            }
        })
        .await;
    });
}

pub fn wipe() {
    WIPES.with(|w| w.set(w.get() + 1));
    with_history(Vec::clear);
    changed();
    crate::ui::runtime::spawn(async {
        let disk = DISK.with(Clone::clone);
        let _turn = disk.lock().await;
        crate::ui::runtime::blocking(|| std::fs::remove_dir_all(dir())).await;
    });
}

/// Files the index doesn't name (a crash between writes, older builds): deleted, off the loop.
fn cleanup() {
    crate::ui::runtime::spawn(async {
        let disk = DISK.with(Clone::clone);
        let _turn = disk.lock().await;
        let keep: Vec<String> = with_history(|h| h.iter().map(|e| entry_file(e.id, &e.mime)).collect());
        crate::ui::runtime::blocking(move || {
            let Ok(rd) = std::fs::read_dir(dir()) else { return };
            let index = format!("{}/index", dir());
            for e in rd.flatten() {
                let p = e.path().to_string_lossy().into_owned();
                if p != index && !keep.contains(&p) {
                    let _ = std::fs::remove_file(&p);
                }
            }
        })
        .await;
    });
}

/// Records every new selection from here on (text preferred, then file lists, then images);
/// `on_change` runs whenever the history changes.
pub fn watch(on_change: impl Fn() + 'static) {
    ON_CHANGE.with(|c| *c.borrow_mut() = Some(Rc::new(on_change)));
    cleanup();
    wayland::on_selection(|mimes| {
        let has = |m: &str| mimes.iter().any(|x| x == m);
        // secrets aren't kept; flurry's own copies were recorded already
        if has(SECRET_HINT) || has(wayland::OWN_MIME) {
            return;
        }
        let Some(mime) = TEXT.into_iter().chain(["text/uri-list", "image/png", "image/jpeg"]).find(|m| has(m)) else { return };
        let stored = if TEXT.contains(&mime) { "text/plain" } else { mime };
        wayland::read_selection(mime, MAX_BYTES, 5000, move |data| {
            if let Some(d) = data {
                record(stored, d);
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
            Entry { id: 7, mime: "text/plain".into(), len: 4, preview: "a\u{a0}b".into() },
            Entry { id: 1, mime: "image/png".into(), len: 100, preview: "1x1".into() },
        ];
        assert_eq!(parse_index(&format_index(&e)), e);
        // a torn or foreign line is skipped, never a panic
        assert_eq!(parse_index("x\ty\tz\n3\ttext/plain\n4\ttext/plain\tx\ty\n").len(), 0);
        // the older index without sizes still loads
        assert_eq!(parse_index("3\ttext/plain\tdeleted entry\n")[0].preview, "deleted entry");
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(preview("text/plain", b"  second line\n\tthird line ").as_deref(), Some("second line third line"));
        assert_eq!(preview("text/plain", " \n".as_bytes()), None);
        assert_eq!(preview("text/plain", "é".repeat(101).as_bytes()), Some(format!("{}…", "é".repeat(100))));
        let png = [&[0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13, b'I', b'H', b'D', b'R'][..], &1920u32.to_be_bytes(), &1080u32.to_be_bytes()].concat();
        assert_eq!(preview("image/png", &png).as_deref(), Some("1920x1080"));
        assert_eq!(preview("image/png", b"not an image"), None);
        assert_eq!(entry_file(5, "image/jpeg"), format!("{}/5.jpg", dir()));
    }
}
