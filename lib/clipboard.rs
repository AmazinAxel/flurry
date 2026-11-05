// Copying and the clipboard history, in-process over wlr-data-control.
// History: one file per entry (raw bytes, named by content hash) and an index of
// `id\tmime\tlen\tpreview` lines, newest first, in $XDG_CACHE_HOME/flurry/clipboard. Hashing and
// disk writes run on worker threads, one job at a time; HISTORY itself only changes on the loop.

use std::cell::{Cell, RefCell};
use std::os::unix::fs::DirBuilderExt;
use std::rc::Rc;
use std::sync::Arc;

use crate::lib::xdg::write_atomic;
use crate::ui::entry::Secret;
use crate::ui::runtime::{blocking, spawn};
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

/// One copy of `data`, wiped once the last offer goes.
fn offer(mimes: &[&str], data: &[u8]) -> Vec<(String, Arc<Secret>)> {
    let data = Arc::new(Secret::new(data.to_vec()));
    mimes.iter().map(|m| (m.to_string(), data.clone())).collect()
}

/// Selections set here list `wayland::OWN_MIME`: the history records them directly instead of
/// reading them back through a pipe.
fn set(offers: Vec<(String, Arc<Secret>)>) -> Option<u64> {
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
    offers.push((SECRET_HINT.into(), Arc::new(Secret::new(b"secret".to_vec()))));
    if let Some(token) = set(offers) {
        let _ = crate::ui::runtime::timeout(45_000, move || wayland::clear_selection(token));
    }
}

pub fn copy_png(png: Vec<u8>) {
    set(offer(&["image/png"], &png));
    record("image/png", png);
}

/// Copies a history entry back as what it was copied as.
pub fn copy_entry(id: u64, mime: &str) {
    let (path, mime) = (entry_file(id, mime), mime.to_string());
    spawn(async move {
        let Some(data) = blocking(move || std::fs::read(path).ok()).await.flatten() else { return };
        set(stored_offer(&mime, &data));
        promote(id);
    });
}

/// What an entry stored as `mime` is offered as.
fn stored_offer(mime: &str, data: &[u8]) -> Vec<(String, Arc<Secret>)> {
    let mut mimes = if mime.starts_with("image/") { vec![mime] } else { TEXT.to_vec() };
    if mime == "text/uri-list" {
        mimes.insert(0, mime); // paste the ACTUAL file
    }
    offer(&mimes, data)
}

/// Moves the entry to the top under its own id (older entries' ids aren't the content hash).
fn promote(id: u64) {
    spawn(async move {
        let disk = DISK.with(Clone::clone);
        let _turn = disk.lock().await;
        let index = with_history(|h| {
            let i = h.iter().position(|e| e.id == id).filter(|&i| i > 0)?;
            h[..=i].rotate_right(1);
            Some(format_index(h))
        });
        let Some(index) = index else { return };
        changed();
        write_index(index).await;
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
    /// the current selection from another app, as read for the history: re-offered when that app
    /// exits (what wl-clip-persist did), only in the one recorded format
    static LAST: Cell<Option<Vec<(String, Arc<Secret>)>>> = const { Cell::new(None) };
    /// newest first, read on first use. By design there is no expiry and no per-source exclusion:
    /// entries go only past CAP/BUDGET or on wipe, and only SECRET_HINT selections are skipped.
    static HISTORY: RefCell<Option<Vec<Entry>>> = const { RefCell::new(None) };
    /// the index exists but couldn't be read: neither overwritten nor cleaned up after, this run
    static LOAD_FAILED: Cell<bool> = const { Cell::new(false) };
    static ON_CHANGE: RefCell<Option<Rc<dyn Fn()>>> = RefCell::default();
    /// disk jobs (records, wipes, cleanup), one at a time and in order
    static DISK: Rc<crate::ui::runtime::Queue> = Rc::default();
    /// bumped by wipe: a record started before it is dropped
    static WIPES: Cell<u64> = const { Cell::new(0) };
    /// the selection waiting to be recorded: only the latest, however fast they come
    static PENDING: RefCell<Option<(String, Vec<u8>)>> = const { RefCell::new(None) };
    /// a record task is waiting for its disk turn (it takes PENDING then)
    static QUEUED: Cell<bool> = const { Cell::new(false) };
}

fn dir() -> String {
    format!("{}/flurry/clipboard", crate::lib::xdg::cache_dir())
}

fn index_path() -> String {
    format!("{}/index", dir())
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
            // previews never hold tabs
            let mut p = l.splitn(4, '\t');
            let (id, mime, len, preview) = (p.next()?.parse().ok()?, p.next()?.to_string(), p.next()?.parse().ok()?, p.next()?);
            Some(Entry { id, mime, len, preview: preview.into() })
        })
        .collect()
}

fn format_index(entries: &[Entry]) -> String {
    entries.iter().map(|e| format!("{}\t{}\t{}\t{}\n", e.id, e.mime, e.len, e.preview)).collect()
}

fn load() -> Vec<Entry> {
    match std::fs::read(index_path()) {
        Ok(b) => {
            let text = String::from_utf8_lossy(&b);
            let entries = parse_index(&text);
            if entries.len() != text.lines().count() {
                // an unknown line names a file cleanup would delete: leave the history on disk alone
                eprintln!("flurry: clipboard index: unreadable lines: history kept on disk as is");
                LOAD_FAILED.with(|f| f.set(true));
            }
            entries
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => {
            eprintln!("flurry: clipboard index: {e}: history kept on disk as is");
            LOAD_FAILED.with(|f| f.set(true));
            vec![]
        }
    }
}

fn with_history<R>(f: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
    HISTORY.with(|h| f(h.borrow_mut().get_or_insert_with(load)))
}

/// Writes the index off the loop; false if it wasn't written.
async fn write_index(index: String) -> bool {
    if LOAD_FAILED.with(Cell::get) {
        return false;
    }
    blocking(move || write_atomic(&index_path(), index.as_bytes(), 0o600).is_ok()).await.unwrap_or(false)
}

/// FNV-1a 64: stable across builds, so ids (file names) stay valid.
fn fnv1a(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

pub fn history() -> Vec<Entry> {
    with_history(|h| h.clone())
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
    PENDING.with(|p| *p.borrow_mut() = Some((mime.to_string(), data)));
    if QUEUED.with(|q| q.replace(true)) {
        return;
    }
    spawn(async move {
        let disk = DISK.with(Clone::clone);
        let _turn = disk.lock().await;
        QUEUED.with(|q| q.set(false));
        // a wipe clears PENDING; one during the work below drops it
        let Some((mime, data)) = PENDING.with(|p| p.borrow_mut().take()) else { return };
        let wipes = WIPES.with(Cell::get);
        // files the history already has needn't be written again
        let known: Vec<(u64, u64)> = with_history(|h| h.iter().map(|e| (e.id, e.len)).collect());
        let entry = blocking(move || {
            let preview = preview(&mime, &data)?;
            let (id, len) = (fnv1a(&data), data.len() as u64);
            let path = entry_file(id, &mime);
            // same id and length: still a collision unless the stored bytes match
            if !known.contains(&(id, len)) || std::fs::read(&path).ok().as_deref() != Some(&data[..]) {
                let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir());
                if let Err(e) = write_atomic(&path, &data, 0o600) {
                    eprintln!("flurry: clipboard entry {path}: {e}");
                    return None;
                }
            }
            Some(Entry { id, mime, len, preview })
        })
        .await
        .flatten();
        let Some(entry) = entry else { return };
        if WIPES.with(Cell::get) != wipes {
            return;
        }
        let update = with_history(|h| {
            if h.first().is_some_and(|e| *e == entry) {
                return None;
            }
            // same id: a repeat, or a collision whose file was just replaced
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
        // files go only once the index no longer names them
        if write_index(index).await {
            blocking(move || {
                for e in dropped {
                    let _ = std::fs::remove_file(entry_file(e.id, &e.mime));
                }
            })
            .await;
        }
    });
}

pub fn wipe() {
    LAST.set(None);
    WIPES.with(|w| w.set(w.get() + 1));
    PENDING.with(|p| p.borrow_mut().take());
    with_history(Vec::clear);
    // the files are gone below: the next index may be written again
    LOAD_FAILED.with(|f| f.set(false));
    changed();
    spawn(async {
        let disk = DISK.with(Clone::clone);
        let _turn = disk.lock().await;
        blocking(|| std::fs::remove_dir_all(dir())).await;
    });
}

/// Files the index doesn't name (a crash between writes): deleted, off the loop.
fn cleanup() {
    spawn(async {
        let disk = DISK.with(Clone::clone);
        let _turn = disk.lock().await;
        let keep: Vec<String> = with_history(|h| h.iter().map(|e| entry_file(e.id, &e.mime)).collect());
        // an unreadable index names nothing: every file would go
        if LOAD_FAILED.with(Cell::get) {
            return;
        }
        blocking(move || {
            let Ok(rd) = std::fs::read_dir(dir()) else { return };
            let index = index_path();
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

/// Records every new selection from here on (file lists preferred, then text, then images);
/// `on_change` runs whenever the history changes.
pub fn watch(on_change: impl Fn() + 'static) {
    ON_CHANGE.with(|c| *c.borrow_mut() = Some(Rc::new(on_change)));
    cleanup();
    wayland::on_selection(|mimes| {
        // the app that copied it exited (or cleared it): flurry serves its last copy instead
        let Some(mimes) = mimes else {
            if let Some(o) = LAST.take() {
                set(o);
            }
            return;
        };
        LAST.set(None);
        let has = |m: &str| mimes.iter().any(|x| x == m);
        // secrets aren't kept; flurry's own copies were recorded already
        if has(SECRET_HINT) || has(wayland::OWN_MIME) {
            return;
        }
        // file managers offer the paths as text too: keep them as files
        let Some(mime) = ["text/uri-list"].into_iter().chain(TEXT).chain(["image/png", "image/jpeg"]).find(|m| has(m)) else { return };
        let stored = if TEXT.contains(&mime) { "text/plain" } else { mime };
        wayland::read_selection(mime, MAX_BYTES, 5000, move |data| {
            if let Some(d) = data {
                LAST.set(Some(stored_offer(stored, &d)));
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
        // the older 3-field index (no sizes) is no longer read
        assert!(parse_index("3\ttext/plain\tdeleted entry\n").is_empty());
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
