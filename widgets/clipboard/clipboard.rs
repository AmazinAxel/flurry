// cliphist history dialog (widgets/clipboard/clipboard.tsx + clipboardItem.tsx).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use tiny_skia::Pixmap;
use xkbcommon::xkb::Keysym;

use crate::lib::backgroundSection::background_section;
use crate::lib::clipboard::{is_sensitive, keep_sensitive, set_sensitive};
use crate::lib::inputControl::{EntryBinding, Opts, input_control};
use crate::lib::process::exec;
use crate::lib::searchDialog::search_entry;
use crate::lib::streamingMode::streaming_mode;
use crate::lib::style::*;
use crate::ui::entry;
use crate::ui::node::{Align, Background, Kind, Node, Sides, Style, rgba};
use crate::ui::{image, runtime, window};
use crate::widgets::clipboard::compress::open_compress;

const MAX_W: f32 = 380.0;
const MAX_H: f32 = 150.0;

/// Decoded clipboard files and compress output: the runtime dir is private to this user.
pub fn tmp_dir() -> String {
    format!("{}/flurry", crate::lib::xdg::runtime_dir())
}

pub fn cache_dir() -> String {
    format!("{}/cliphist", tmp_dir())
}

#[derive(Clone)]
enum Thumb {
    Loading,
    Ready(Rc<Pixmap>, f32), // device pixels per logical pixel
    Failed,
}

#[derive(Clone)]
struct Item {
    id: u64,
    content: String,
    path: Option<String>,
    image: Option<(String, String, String)>, // ext, width, height
    video: bool,
    mime: String,
    search: String,
}

thread_local! {
    static ITEMS: RefCell<Vec<Item>> = const { RefCell::new(Vec::new()) };
    static THUMBS: RefCell<HashMap<u64, Thumb>> = RefCell::default();
    static QUERY: RefCell<String> = const { RefCell::new(String::new()) };
    static SELECTED: Cell<Option<u64>> = const { Cell::new(None) };
    static SCROLL: Cell<f32> = const { Cell::new(0.0) };
    static PRODUCE: Rc<runtime::Queue> = Rc::default();
    static REFRESH: Rc<runtime::Queue> = Rc::default();
}

/// `[[ binary data 12 KiB png 1920x1080 ]]` -> (ext, w, h)
pub fn binary_data(content: &str) -> Option<(String, String, String)> {
    let start = content.find("[[ binary data ")?;
    let rest = &content[start + 15..];
    let end = rest.find(" ]]")?;
    let parts: Vec<&str> = rest[..end].split(' ').collect();
    if parts.len() != 4 || !["B", "KiB", "MiB", "GiB"].contains(&parts[1]) || !parts[0].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // copied text can look like this too, and the extension ends up in a path and a shell command
    if parts[2].is_empty() || !parts[2].chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let (w, h) = parts[3].split_once('x')?;
    if !w.chars().all(|c| c.is_ascii_digit()) || !h.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((parts[2].to_string(), w.to_string(), h.to_string()))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn entry_path(id: u64, content: &str) -> Option<String> {
    if let Some((ext, _, _)) = binary_data(content) {
        return Some(format!("{}/{id}.{ext}", cache_dir()));
    }
    let single = content.trim();
    if single.contains('\n') || single.contains(' ') {
        return None; // not a path
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


async fn decode_bytes(id: u64, path: &str) -> bool {
    crate::lib::process::to_file(&["cliphist", "decode", &id.to_string()], path).await
}

/// Thumbnails are rendered at the largest output scale so they map 1:1 onto device pixels.
fn device_scale() -> f32 {
    crate::ui::wayland::outputs().iter().map(|o| o.scale).max().unwrap_or(1).max(1) as f32
}

fn load_scaled(file: &str, k: f32) -> Option<Pixmap> {
    let src = image::decode(&std::fs::read(file).ok()?)?;
    let s = (MAX_W * k / src.width() as f32).min(MAX_H * k / src.height() as f32).min(k);
    let (w, h) = ((src.width() as f32 * s).round().max(1.0) as u32, (src.height() as f32 * s).round().max(1.0) as u32);
    let mut out = Pixmap::new(w, h)?;
    let paint = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
    out.draw_pixmap(0, 0, src.as_ref(), &paint, tiny_skia::Transform::from_scale(s, s), None);
    Some(out)
}

/// Thumbnails are produced one at a time (produceQueue).
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
        let file = if item.video { format!("{}/{}.{k}x.jpg", cache_dir(), item.id) } else { item.path.clone().unwrap_or_default() };
        let ready = std::path::Path::new(&file).exists()
            || if item.video {
                let scale = format!("scale=w={}:h={}:force_original_aspect_ratio=decrease", MAX_W * k, MAX_H * k);
                let src = item.path.clone().unwrap_or_default();
                exec(&["ffmpeg", "-v", "error", "-skip_frame", "nokey", "-i", &src, "-vf", &scale, "-frames:v", "1", "-y", &file]).await.is_ok()
            } else {
                decode_bytes(item.id, &file).await
            };
        // decoding a screenshot takes tens of ms: off the loop
        let pix = if ready { runtime::blocking(move || load_scaled(&file, k)).await.flatten() } else { None };
        let thumb = pix.map(|p| Thumb::Ready(Rc::new(p), k)).unwrap_or(Thumb::Failed);
        THUMBS.with(|t| t.borrow_mut().insert(item.id, thumb));
        if let Some(w) = window::get("clipboard") {
            w.redraw();
        }
    });
}

/// Image/video rows stay hidden until their thumbnail exists, so they are requested up front
/// (requesting from the row builder never fired: hidden rows are never built).
fn request_thumbs() {
    if streaming_mode().peek() {
        return;
    }
    let media: Vec<Item> = ITEMS.with(|i| i.borrow().iter().filter(|x| (x.image.is_some() || x.video) && !is_sensitive(x.id)).cloned().collect());
    for item in &media {
        request_thumb(item);
    }
}

fn describe(item: &Item) -> String {
    if let Some((_, w, h)) = &item.image {
        format!("Image ({w}x{h})")
    } else if item.video {
        format!("Video ({})", item.path.as_deref().unwrap_or("").rsplit('/').next().unwrap_or(""))
    } else {
        format!("Text ({} chars)", item.content.encode_utf16().count())
    }
}

fn item_node(base: &Style, item: &Item, row: &Style) -> Option<Node> {
    if is_sensitive(item.id) {
        return Some(Node::label(row.inherit(), "[ Sensitive ]").xalign(0.0));
    }
    if streaming_mode().peek() {
        return Some(Node::label(row.inherit(), describe(item)).xalign(0.0));
    }
    if item.image.is_some() || item.video {
        return match THUMBS.with(|t| t.borrow().get(&item.id).cloned()) {
            Some(Thumb::Ready(p, k)) => {
                let (w, h) = (p.width() as f32 / k, p.height() as f32 / k);
                let mut s = base.inherit();
                s.box_shadow = vec![shadow_()];
                let pic = Node::new(Kind::Picture { pixmap: crate::ui::node::Img::Pix(p), cover: false, blur: 0.0, density: k }, base.inherit());
                let mut kids = vec![pic];
                if item.video {
                    let ps = Style { background: Background::Color(rgba(0, 0, 0, 0.5)), padding: Sides::all(8.0), color: tiny_skia::Color::WHITE, ..base.inherit() };
                    kids.push(Node::icon(ps, "media-playback-start-symbolic", 42.0).align(Align::Center, Align::Center));
                }
                Some(Node::overlay(s, kids).size(w, h).align(Align::Center, Align::Center))
            }
            Some(Thumb::Failed) => Some(Node::label(row.inherit(), describe(item)).xalign(0.0)),
            // hidden until loaded (visible=false rows are filtered out)
            _ => None,
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
    let mut items: Vec<Item> = THUMBS.with(|t| {
        let t = t.borrow();
        ITEMS.with(|i| {
            i.borrow()
                .iter()
                .filter(|i| streaming || (i.image.is_none() && !i.video) || is_sensitive(i.id) || !matches!(t.get(&i.id), Some(Thumb::Loading) | None))
                .filter(|i| q.is_empty() || (!is_sensitive(i.id) && i.search.contains(&q)))
                .cloned()
                .collect()
        })
    });
    items.sort_by_key(|i| std::cmp::Reverse(i.id));
    items
}

fn select_top(keep: bool) {
    let rows = visible_rows();
    let cur = SELECTED.with(Cell::get);
    if !keep || cur.is_none() || !rows.iter().any(|r| Some(r.id) == cur) {
        SELECTED.with(|s| s.set(rows.first().map(|r| r.id)));
        SCROLL.with(|s| s.set(0.0));
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
    let hint = if is_sensitive(item.id) { "--sensitive " } else { "" };
    let cmd = format!("cliphist decode {} | wl-copy {hint}-t {} 2>/dev/null", item.id, item.mime);
    runtime::spawn(async move {
        let _ = exec(&["bash", "-c", &cmd]).await;
    });
}

async fn refresh_items() {
    let queue = REFRESH.with(Clone::clone);
    let _turn = queue.lock().await;
    if let Ok(out) = exec(&["cliphist", "list"]).await {
        let entries: Vec<(u64, String)> = out
            .split('\n')
            .filter_map(|l| {
                let (id, content) = l.split_once('\t')?;
                (!id.is_empty() && !content.is_empty()).then(|| Some((id.parse().ok()?, content.to_string()))).flatten()
            })
            .collect();
        keep_sensitive(&entries.iter().map(|(id, _)| *id).collect::<Vec<_>>());
        let stale: Vec<u64> = ITEMS.with(|i| {
            let mut items = i.borrow_mut();
            for (id, content) in &entries {
                if items.iter().any(|x| x.id == *id) {
                    continue;
                }
                let image = binary_data(content);
                let path = entry_path(*id, content);
                let video = path.as_deref().is_some_and(|p| is_video(p) && std::path::Path::new(p).exists());
                let mime = if let Some((ext, _, _)) = &image {
                    format!("image/{ext}")
                } else if content.trim().starts_with("file://") {
                    "text/uri-list".into() // paste the ACTUAL file
                } else {
                    "text/plain".into()
                };
                let search = format!("{}{}", if image.is_some() { "image " } else { "" }, content.to_lowercase());
                items.push(Item { id: *id, content: content.clone(), path, image, video, mime, search });
            }
            let stale: Vec<u64> = items.iter().filter(|x| !entries.iter().any(|(id, _)| *id == x.id)).map(|x| x.id).collect();
            items.retain(|x| !stale.contains(&x.id));
            stale
        });
        // their decodes and thumbnails are dead weight now
        if !stale.is_empty()
            && let Ok(rd) = std::fs::read_dir(cache_dir())
        {
            for f in rd.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if name.split('.').next().and_then(|i| i.parse::<u64>().ok()).is_some_and(|i| stale.contains(&i)) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
            THUMBS.with(|t| t.borrow_mut().retain(|id, _| !stale.contains(id)));
        }
        select_top(true);
        request_thumbs();
    }
    if let Some(w) = window::get("clipboard") {
        w.redraw();
    }
}

fn watch_db() {
    let dir = format!("{}/cliphist", crate::lib::xdg::cache_dir());
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    let Ok(c) = std::ffi::CString::new(dir) else { return };
    if fd < 0 || unsafe { libc::inotify_add_watch(fd, c.as_ptr(), libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO) } < 0 {
        return;
    }
    runtime::watch(fd, move || {
        let mut buf = [0u8; 4096];
        let mut db_changed = false;
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            // inotify_event: wd(4) mask(4) cookie(4) len(4) name
            let mut off = 0usize;
            while off + 16 <= n as usize {
                let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
                let name = String::from_utf8_lossy(&buf[off + 16..off + 16 + len]);
                if name.trim_end_matches('\0') == "db" {
                    db_changed = true;
                }
                off += 16 + len;
            }
        }
        if db_changed {
            runtime::spawn(refresh_items());
        }
    });
}

fn show_in_file_manager(file: &str) {
    if let Some(bus) = crate::lib::dbus::session() {
        let msg = crate::lib::dbus::method("org.freedesktop.FileManager1", "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1", "ShowItems")
            .append2(vec![crate::lib::clipboard::uri(file)], "");
        bus.call_cb(msg, |_| {});
    }
}

/// Streaming mode skips thumbnails, so image entries may have no decode on disk yet.
fn with_file(want: Option<fn(&str) -> bool>, action: impl FnOnce(String, u64) + 'static) {
    let Some(item) = selected_item() else { return };
    let Some(path) = item.path.clone() else { return };
    if want.is_some_and(|w| !w(&path)) {
        return;
    }
    let run = move || {
        if !std::path::Path::new(&path).exists() {
            return;
        }
        hide();
        action(path, item.id);
    };
    if std::path::Path::new(item.path.as_deref().unwrap_or("")).exists() || !item.mime.starts_with("image/") {
        return run();
    }
    let p = item.path.clone().unwrap_or_default();
    runtime::spawn(async move {
        if decode_bytes(item.id, &p).await {
            run();
        }
    });
}

fn spawn_exec(argv: Vec<String>) {
    runtime::spawn(async move {
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let _ = exec(&refs).await;
    });
}

/// Keeps the selected row inside the 460px viewport.
fn scroll_into_view(rows: &[(u64, f32, f32)], viewport: f32) {
    let sel = SELECTED.with(Cell::get);
    let Some(&(_, top, h)) = rows.iter().find(|r| Some(r.0) == sel) else { return };
    SCROLL.with(|s| {
        let mut off = s.get();
        if top < off {
            off = top;
        } else if top + h > off + viewport {
            off = top + h - viewport;
        }
        s.set(off.max(0.0));
    });
}

pub fn clipboard() {
    let _ = std::fs::create_dir_all(cache_dir());
    let e = entry::new("Clipboard");
    e.borrow_mut().on_change = Some(Rc::new(|text: &str| {
        QUERY.with(|q| *q.borrow_mut() = text.to_lowercase());
        select_top(false);
        if let Some(w) = window::get("clipboard") {
            w.redraw();
        }
    }));
    let ec = e.clone();
    let build = move |v: &window::View| {
        let base = window();
        let rows = visible_rows();
        let sel = SELECTED.with(Cell::get);
        let mut nodes = vec![];
        let mut geometry = vec![];
        let mut y = 10.0;
        let count = rows.len();
        for (i, item) in rows.iter().enumerate() {
            let selected = Some(item.id) == sel;
            let mut s = base.inherit();
            s.padding = Sides::all(8.0);
            s = s.border(Sides::all(1.0), dark4());
            s.background = Background::Color(dark2());
            s.margin.bottom = if i + 1 == count { 0.0 } else { 8.0 };
            s.color = light1();
            if selected {
                s = focus_outline(s);
                s.translate_y = -2.0;
            }
            let Some(inner) = item_node(&base, item, &s) else { continue };
            let row = Node::vbox(s, 0.0, vec![inner]);
            let h = row.measure_h(500.0 - 32.0 - 6.0).1;
            geometry.push((item.id, y, h + row.style.margin.bottom));
            y += h + row.style.margin.bottom;
            nodes.push(row);
        }
        scroll_into_view(&geometry, 460.0);
        let list = Node::vbox(Style { padding: Sides::trbl(10.0, 0.0, 0.0, 0.0), ..base.inherit() }, 0.0, nodes);
        let scroll = Node::scroll(base.inherit(), 460.0, SCROLL.with(Cell::get), list).show(count > 0);
        let header = search_entry(&base, &ec, "system-search-symbolic", v.focused("entry"));
        background_section(&base, header, Some(scroll), 700.0, 500.0)
    };
    let ctrl = |k: Keysym, f: fn()| -> (Keysym, crate::lib::inputControl::Action) { (k, Rc::new(f)) };
    let w = input_control("clipboard", build, Opts {
        on_show: Some(Rc::new(|| select_top(false))),
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
            ctrl(Keysym::e, || with_file(Some(|f| !is_video(f)), |f, _| spawn_exec(vec!["swappy".into(), "-f".into(), f]))),
            ctrl(Keysym::g, || with_file(None, |f, _| spawn_exec(vec!["gthumb".into(), f]))),
            ctrl(Keysym::m, || with_file(Some(is_video), |f, id| open_compress(&f, id))),
            ctrl(Keysym::n, || with_file(None, |f, _| show_in_file_manager(&f))),
            ctrl(Keysym::s, || {
                if let Some(i) = selected_item() {
                    set_sensitive(i.id, !is_sensitive(i.id));
                    request_thumbs();
                }
            }),
            ctrl(Keysym::w, || {
                keep_sensitive(&[]);
                let cmd = format!("cliphist wipe && rm -rf {0}/* && mkdir -p {0}", cache_dir());
                spawn_exec(vec!["bash".into(), "-c".into(), cmd]);
                hide();
            }),
        ],
        entries: vec![EntryBinding { id: "entry", entry: e, on_activate: None, on_key: None }],
        ..Default::default()
    });
    w.watch(&streaming_mode());
    streaming_mode().subscribe(|| {
        request_thumbs(); // rows themselves just redraw (watched above): no refetch gap
    });
    runtime::spawn(refresh_items());
    watch_db();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cliphist_entries() {
        assert_eq!(binary_data("[[ binary data 12 KiB png 1920x1080 ]]"), Some(("png".into(), "1920".into(), "1080".into())));
        assert_eq!(entry_path(7, "[[ binary data 1 B jpeg 1x1 ]]"), Some(format!("{}/7.jpeg", cache_dir())));
        // copied text posing as a binary entry never reaches a path or the shell
        assert_eq!(binary_data("[[ binary data 1 B png;$(id) 1x1 ]]"), None);
        assert_eq!(binary_data("[[ binary data 1 B ../x 1x1 ]]"), None);
        assert_eq!(entry_path(1, "file:///home/a/x%20y.mp4").as_deref(), Some("/home/a/x y.mp4"));
        assert_eq!(entry_path(1, "/tmp/a b"), None);
        assert_eq!(entry_path(1, "hello"), None);
        assert!(is_video("/a/B.MKV"));
    }
}
