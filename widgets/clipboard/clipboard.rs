// Clipboard history dialog (widgets/clipboard/clipboard.tsx + clipboardItem.tsx).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::rc::Rc;

use tiny_skia::Pixmap;
use xkbcommon::xkb::Keysym;

use crate::lib::backgroundSection::background_section;
use crate::lib::apps::launch;
use crate::lib::clipboard::{Entry, copy_entry, entry_file};
use crate::lib::inputControl::{EntryBinding, Opts, input_control};
use crate::lib::searchDialog::search_entry;
use crate::lib::streamingMode::streaming_mode;
use crate::lib::style::*;
use crate::ui::entry;
use crate::ui::node::{Align, Background, Kind, Node, Sides, Style, rgba};
use crate::ui::{image, runtime, window};
use crate::widgets::clipboard::compress::open_compress;

const MAX_W: f32 = 380.0;
const MAX_H: f32 = 150.0;

/// Thumbnails decoded per show (newest media first), and kept at most.
const THUMB_ROWS: usize = 12;
const THUMB_CAP: usize = 24;

#[derive(Clone)]
enum Thumb {
    Loading,
    Ready(Rc<crate::ui::node::Pic>, f32), // device pixels per logical pixel
    Failed,
}

#[derive(Clone)]
struct Item {
    id: u64,
    content: String,
    path: Option<String>,
    image: Option<String>, // WxH
    video: bool,
    mime: String,
    search: String,
}

thread_local! {
    static ITEMS: RefCell<Vec<Item>> = const { RefCell::new(Vec::new()) };
    static THUMBS: RefCell<HashMap<u64, Thumb>> = RefCell::default();
    /// THUMBS ids, least recently shown first
    static THUMB_LRU: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    static QUERY: RefCell<String> = const { RefCell::new(String::new()) };
    static SELECTED: Cell<Option<u64>> = const { Cell::new(None) };
    static PRODUCE: Rc<runtime::Queue> = Rc::default();
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn entry_path(e: &Entry) -> Option<String> {
    if e.mime.starts_with("image/") {
        return Some(entry_file(e.id, &e.mime));
    }
    if e.preview.trim().contains(' ') {
        return None; // not a path
    }
    // the preview is cut at 100 chars: the path itself comes from the entry file
    let file = if e.len <= 4096 { std::fs::read_to_string(entry_file(e.id, &e.mime)).ok() } else { None };
    let single = file.as_deref().unwrap_or(&e.preview).trim();
    if single.contains(char::is_whitespace) {
        return None;
    }
    let path = match single.strip_prefix("file:") {
        Some(rest) => percent_decode(&format!("/{}", rest.trim_start_matches('/'))),
        None => single.to_string(),
    };
    path.starts_with('/').then_some(path)
}

pub fn is_video(p: &str) -> bool {
    let l = p.to_lowercase();
    [".mp4", ".mkv", ".webm", ".mov", ".m4v", ".avi"].iter().any(|e| l.ends_with(e))
}

/// Thumbnails are rendered at the largest output scale so they map 1:1 onto device pixels.
fn device_scale() -> f32 {
    crate::ui::wayland::outputs().iter().map(|o| o.scale).max().unwrap_or(1).max(1) as f32
}

/// A child's whole stdout, binary-safe; killed past `ms` (blocking: worker threads only).
fn read_output(argv: &[&str], ms: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let (prog, args) = argv.split_first()?;
    let mut child = Command::new(prog).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut out = child.stdout.take()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    let (mut buf, mut chunk) = (Vec::new(), vec![0u8; 1 << 16]);
    // bounded by the deadline: a poll with no time left returns 0
    let finished = loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as i32;
        let mut pfd = libc::pollfd { fd: out.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        match unsafe { libc::poll(&mut pfd, 1, left) } {
            0 => break false,
            r if r < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => continue,
            r if r < 0 => break false,
            _ => {}
        }
        match out.read(&mut chunk) {
            Ok(0) => break true,
            Ok(n) if buf.len() + n <= MAX_THUMB_SOURCE => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            _ => break false,
        }
    };
    if !finished {
        let _ = child.kill();
    }
    let ok = child.wait().is_ok_and(|s| s.success());
    (finished && ok).then_some(buf)
}

const MAX_THUMB_SOURCE: usize = 64 << 20;

/// A thumbnail's size in device pixels: fits MAX_W x MAX_H logical pixels, never upscaled.
fn thumb_size(w: f32, h: f32, k: f32) -> (u32, u32, f32) {
    let s = (MAX_W * k / w).min(MAX_H * k / h).min(k);
    ((w * s).round().max(1.0) as u32, (h * s).round().max(1.0) as u32, s)
}

fn load_scaled(bytes: &[u8], k: f32) -> Option<Pixmap> {
    let src = image::decode(bytes)?;
    let (w, h, s) = thumb_size(src.width() as f32, src.height() as f32, k);
    let mut out = Pixmap::new(w, h)?;
    let paint = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
    out.draw_pixmap(0, 0, src.as_ref(), &paint, tiny_skia::Transform::from_scale(s, s), None);
    Some(out)
}

/// Marks `id` most recently used, evicting the least recently used finished thumbnails past the cap.
fn touch_thumb(id: u64) {
    THUMB_LRU.with(|l| {
        let mut l = l.borrow_mut();
        l.retain(|i| *i != id);
        l.push(id);
        THUMBS.with(|t| {
            let mut t = t.borrow_mut();
            while l.len() > THUMB_CAP {
                let old = l.remove(0);
                t.remove(&old);
            }
        });
    });
}

/// Thumbnails are produced one at a time (produceQueue), in memory: nothing lands in the runtime dir.
fn request_thumb(item: &Item) {
    if THUMBS.with(|t| t.borrow().contains_key(&item.id)) {
        return;
    }
    THUMBS.with(|t| t.borrow_mut().insert(item.id, Thumb::Loading));
    let item = item.clone();
    runtime::spawn(async move {
        let produce = PRODUCE.with(Clone::clone);
        let _turn = produce.lock().await;
        let k = device_scale();
        let (id, video, src) = (item.id, item.video, item.path.clone().unwrap_or_default());
        // decoding a screenshot takes tens of ms: off the loop
        let pix = runtime::blocking(move || {
            // regular files only: no fifos or devices to hang on, no symlinks out of the history
            if !std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_file()) {
                return None;
            }
            let bytes = if video {
                let scale = format!("scale=w={}:h={}:force_original_aspect_ratio=decrease", MAX_W * k, MAX_H * k);
                let argv = ["ffmpeg", "-v", "error", "-skip_frame", "nokey", "-i", &src, "-vf", &scale, "-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"];
                read_output(&argv, 5000)
            } else {
                std::fs::read(&src).ok()
            };
            load_scaled(&bytes?, k)
        })
        .await
        .flatten();
        let thumb = pix.map(|p| Thumb::Ready(crate::ui::node::Pic::new(p), k)).unwrap_or(Thumb::Failed);
        // dropped meanwhile (wipe, stale entry): don't resurrect it
        if THUMBS.with(|t| t.borrow_mut().get_mut(&id).map(|t| *t = thumb).is_some()) {
            touch_thumb(id);
        }
        if let Some(w) = window::get("clipboard") {
            w.redraw();
        }
    });
}

/// Lazily, while the dialog is up: the newest media rows get thumbnails; older ones show their
/// description. Image rows hold their thumbnail's size while loading; video rows (size unknown)
/// stay hidden (requesting from the row builder never fired: hidden rows are never built).
fn request_thumbs() {
    if streaming_mode().peek() || !window::get("clipboard").is_some_and(|w| w.visible()) {
        return;
    }
    let media: Vec<Item> = ITEMS.with(|i| i.borrow().iter().filter(|x| x.image.is_some() || x.video).cloned().collect());
    for item in media.iter().take(THUMB_ROWS) {
        request_thumb(item);
    }
}

fn describe(item: &Item) -> String {
    if let Some(dims) = &item.image {
        format!("Image ({dims})")
    } else if item.video {
        format!("Video ({})", item.path.as_deref().unwrap_or("").rsplit('/').next().unwrap_or(""))
    } else {
        format!("Text ({} chars)", item.content.encode_utf16().count())
    }
}

/// An empty box of the image thumbnail's size, from the stored WxH.
fn placeholder(base: &Style, item: &Item) -> Option<Node> {
    let (w, h) = item.image.as_deref()?.split_once('x')?;
    let (w, h): (f32, f32) = (w.parse().ok()?, h.parse().ok()?);
    // past THUMB_PIXELS the decode is refused: no thumbnail to hold room for
    if w < 1.0 || h < 1.0 || (w as u64).saturating_mul(h as u64) > image::THUMB_PIXELS {
        return None;
    }
    let k = device_scale();
    let (tw, th, _) = thumb_size(w, h, k);
    Some(Node::vbox(base.inherit(), 0.0, vec![]).size(tw as f32 / k, th as f32 / k).align(Align::Center, Align::Center))
}

fn item_node(base: &Style, item: &Item, row: &Style) -> Option<Node> {
    if streaming_mode().peek() {
        return Some(Node::label(row.inherit(), describe(item)).xalign(0.0));
    }
    if item.image.is_some() || item.video {
        return match THUMBS.with(|t| t.borrow().get(&item.id).cloned()) {
            Some(Thumb::Ready(p, k)) => {
                touch_thumb(item.id);
                let (w, h) = (p.pix.width() as f32 / k, p.pix.height() as f32 / k);
                let s = base.inherit();
                let pic = Node::new(Kind::Picture { pixmap: crate::ui::node::Img::Pix(p), cover: false, blur: 0.0, density: k }, base.inherit());
                let mut kids = vec![pic];
                if item.video {
                    let ps = Style { background: Background::Color(rgba(0, 0, 0, 0.5)), padding: Sides::all(SPACE_M), color: tiny_skia::Color::WHITE, ..base.inherit() };
                    kids.push(Node::icon(ps, "media-playback-start-symbolic", 42.0).align(Align::Center, Align::Center));
                }
                Some(Node::overlay(s, kids).size(w, h).align(Align::Center, Align::Center))
            }
            // images: an empty box of the thumbnail's size, so nothing moves when it lands
            Some(Thumb::Loading) => Some(placeholder(base, item).unwrap_or_else(|| Node::label(row.inherit(), describe(item)).xalign(0.0))),
            Some(Thumb::Failed) | None => Some(Node::label(row.inherit(), describe(item)).xalign(0.0)),
        };
    }
    let mut n = Node::label(row.inherit(), item.content.clone()).xalign(0.0);
    if let Kind::Label(l) = &mut n.kind {
        l.wrap = true;
        l.max_width_chars = Some(1.0);
    }
    Some(n)
}

fn visible_rows() -> Vec<Item> {
    let q = QUERY.with(|q| q.borrow().clone());
    let streaming = streaming_mode().peek();
    // filter first, clone only what is shown
    THUMBS.with(|t| {
        let t = t.borrow();
        ITEMS.with(|i| {
            i.borrow()
                .iter()
                .filter(|i| streaming || !i.video || !matches!(t.get(&i.id), Some(Thumb::Loading)))
                .filter(|i| q.is_empty() || i.search.contains(&q))
                .cloned()
                .collect()
        })
    })
}

fn select_top(keep: bool) {
    let rows = visible_rows();
    let cur = SELECTED.with(Cell::get);
    if !keep || cur.is_none() || !rows.iter().any(|r| Some(r.id) == cur) {
        SELECTED.with(|s| s.set(rows.first().map(|r| r.id)));
    }
}

fn move_selection(delta: i64) {
    let rows = visible_rows();
    if rows.is_empty() {
        return;
    }
    let i = rows.iter().position(|r| Some(r.id) == SELECTED.with(Cell::get)).map(|i| i as i64).unwrap_or(-1);
    let j = crate::lib::wrapFocus::wrap_index(i, delta, rows.len() as i64) as usize;
    SELECTED.with(|s| s.set(Some(rows[j].id)));
}

fn selected_item() -> Option<Item> {
    let rows = visible_rows();
    let sel = SELECTED.with(Cell::get);
    rows.iter().find(|r| Some(r.id) == sel).cloned().or_else(|| rows.first().cloned())
}

fn hide() {
    window::set_visible("clipboard", false);
}

fn activate(item: &Item) {
    hide();
    copy_entry(item.id, &item.mime);
}

fn new_item(e: Entry) -> Item {
    let image = e.mime.starts_with("image/").then(|| e.preview.clone());
    let path = entry_path(&e);
    let video = path.as_deref().is_some_and(|p| is_video(p) && std::path::Path::new(p).exists());
    let search = format!("{}{}", if image.is_some() { "image " } else { "" }, e.preview.to_lowercase());
    Item { id: e.id, content: e.preview, path, image, video, mime: e.mime, search }
}

/// Rebuilt from the history, keeping rows (and thumbnails) of entries still in it.
fn refresh_items() {
    let ids: Vec<u64> = ITEMS.with(|i| {
        let mut old: HashMap<u64, Item> = i.take().into_iter().map(|x| (x.id, x)).collect();
        let items: Vec<Item> = crate::lib::clipboard::history().into_iter().map(|e| old.remove(&e.id).unwrap_or_else(|| new_item(e))).collect();
        let ids = items.iter().map(|x| x.id).collect();
        *i.borrow_mut() = items;
        ids
    });
    THUMBS.with(|t| t.borrow_mut().retain(|id, _| ids.contains(id)));
    THUMB_LRU.with(|l| l.borrow_mut().retain(|id| ids.contains(id)));
    select_top(true);
    request_thumbs();
    if let Some(w) = window::get("clipboard") {
        w.redraw();
    }
}

fn show_in_file_manager(file: &str) {
    if let Some(bus) = crate::lib::dbus::session() {
        let msg = crate::lib::dbus::method("org.freedesktop.FileManager1", "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1", "ShowItems")
            .append2(vec![crate::lib::clipboard::uri(file)], "");
        bus.call_cb(msg, |_| {});
    }
}

fn with_file(want: Option<fn(&str) -> bool>, action: impl FnOnce(String, u64)) {
    let Some(item) = selected_item() else { return };
    let Some(path) = item.path else { return };
    if want.is_some_and(|w| !w(&path)) || !std::path::Path::new(&path).exists() {
        return;
    }
    hide();
    action(path, item.id);
}

fn with_prefix(prefix: &[&str], file: String) -> Vec<String> {
    prefix.iter().map(|s| s.to_string()).chain([file]).collect()
}

fn wipe() {
    hide();
    crate::lib::clipboard::wipe();
}

pub fn clipboard() {
    let e = entry::new("");
    e.borrow_mut().on_change = Some(Rc::new(|text: &str| {
        QUERY.with(|q| *q.borrow_mut() = text.to_lowercase());
        select_top(false);
        if let Some(w) = window::get("clipboard") {
            w.redraw();
        }
    }));
    let ec = e.clone();
    let build = move |_: &window::View| {
        let base = window();
        let rows = visible_rows();
        let sel = SELECTED.with(Cell::get);
        let mut nodes = vec![];
        let mut center = None;
        let count = rows.len();
        for (i, item) in rows.iter().enumerate() {
            let selected = Some(item.id) == sel;
            let mut s = base.inherit();
            s.padding = Sides::all(SPACE_M);
            s = s.border(Sides::all(BORDER), dark4());
            s.background = Background::Color(dark2());
            s.margin.bottom = if i + 1 == count { 0.0 } else { BORDER };
            s.color = light1();
            if selected {
                s = focus_outline(s);
            }
            let Some(inner) = item_node(&base, item, &s) else { continue };
            if selected {
                center = Some(nodes.len());
            }
            nodes.push(Node::vbox(s, 0.0, vec![inner]));
        }
        // room for the first and last rows' outline inside the clip; the last row's hangs into the
        // card's padding, so the rows end as far from the border as they do at the sides
        let list = Node::vbox(Style { padding: Sides::trbl(SPACE_M, 0.0, BORDER, 0.0), ..base.inherit() }, 0.0, nodes);
        let scroll = Node::scroll(Style { margin: Sides::trbl(0.0, 0.0, -BORDER, 0.0), ..base.inherit() }, 460.0, center, list).show(count > 0);
        let header = search_entry(&base, &ec);
        background_section(&base, header, Some(scroll), 700.0, 500.0)
    };
    let ctrl = |k: Keysym, f: fn()| -> (Keysym, crate::lib::inputControl::Action) { (k, Rc::new(f)) };
    let w = input_control("clipboard", build, Opts {
        on_show: Some(Rc::new(|| {
            refresh_items();
            select_top(false);
        })),
        focus: Some("entry"),
        actions: vec![
            ctrl(Keysym::Return, || {
                if let Some(i) = selected_item() {
                    activate(&i);
                }
            }),
            ctrl(Keysym::Up, || move_selection(-1)),
            ctrl(Keysym::Down, || move_selection(1)),
        ],
        ctrl_actions: vec![
            ctrl(Keysym::k, || move_selection(-1)),
            ctrl(Keysym::j, || move_selection(1)),
            ctrl(Keysym::c, || {
                if let Some(i) = visible_rows().get(1) {
                    activate(i); // copy 2nd recent entry
                }
            }),
            ctrl(Keysym::e, || with_file(Some(|f| !is_video(f)), |f, _| launch(&with_prefix(&["swappy", "-f"], f)))),
            ctrl(Keysym::g, || with_file(None, |f, _| launch(&with_prefix(crate::settings::IMAGE_VIEWER, f)))),
            ctrl(Keysym::m, || with_file(Some(is_video), |f, id| open_compress(&f, id))),
            ctrl(Keysym::n, || with_file(None, |f, _| show_in_file_manager(&f))),
            ctrl(Keysym::w, wipe),
        ],
        entries: vec![EntryBinding { id: "entry", entry: e, on_activate: None, on_key: None }],
        ..Default::default()
    });
    w.watch(&streaming_mode());
    streaming_mode().subscribe(|| {
        request_thumbs(); // rows themselves just redraw (watched above): no refetch gap
    });
    // rows are only built while the dialog is up: hidden, a copy just lands in the history
    crate::lib::clipboard::watch(|| {
        if window::get("clipboard").is_some_and(|w| w.visible()) {
            refresh_items();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_entry_paths() {
        let e = |mime: &str, preview: &str| entry_path(&Entry { id: 7, mime: mime.into(), len: 1 << 20, preview: preview.into() });
        assert_eq!(e("image/jpeg", "1x1"), Some(entry_file(7, "image/jpeg")));
        assert_eq!(e("text/uri-list", "file:///home/a/x%20y.mp4").as_deref(), Some("/home/a/x y.mp4"));
        assert_eq!(e("text/plain", "/tmp/a b"), None);
        assert_eq!(e("text/plain", "hello"), None);
        assert!(is_video("/a/B.MKV"));
        // multibyte after a broken escape: kept, never a panic
        assert_eq!(e("text/plain", "file:%aé").as_deref(), Some("/%aé"));
    }
}
