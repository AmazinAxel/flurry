// Random wallpaper from ~/Wallpapers (lib/wallpaper.tsx). The decoded image lives in exactly one shm
// buffer: every monitor's background surface shows it, and BackgroundSection headers and the
// lockscreen draw from the same memory.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::lib::state::State;
use crate::ui::image;
use crate::ui::node::{Img, Kind, Node, Style};
use crate::ui::runtime;
use crate::ui::wayland::{Anchor, Layer, ShmImage, ShmPixels};
use crate::ui::window::{Spec, Win};

#[derive(Clone)]
pub struct Wallpaper {
    pub path: String,
    pub shm: Rc<ShmImage>,
}

impl PartialEq for Wallpaper {
    fn eq(&self, o: &Self) -> bool {
        Rc::ptr_eq(&self.shm, &o.shm)
    }
}

thread_local! {
    static CURRENT: State<Option<Wallpaper>> = State::new(None);
    static NEXT: RefCell<Option<Wallpaper>> = const { RefCell::new(None) };
    static LOADING: Cell<bool> = const { Cell::new(false) };
    static SHOW_NEXT: Cell<bool> = const { Cell::new(false) };
}

pub fn current() -> State<Option<Wallpaper>> {
    CURRENT.with(Clone::clone)
}

/// Images under `dir`: only regular files (symlinks followed) named like a jpeg or png, so a
/// video or a FIFO is never read.
fn list_files(dir: &str, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = format!("{dir}/{}", e.file_name().to_string_lossy());
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            list_files(&p, out);
        } else if p.rsplit_once('.').is_some_and(|(_, x)| ["jpg", "jpeg", "png"].iter().any(|e| x.eq_ignore_ascii_case(e)))
            && std::fs::metadata(&p).is_ok_and(|m| m.is_file())
        {
            out.push(p);
        }
    }
}

fn random(n: usize) -> usize {
    let mut x = 0u64;
    unsafe { libc::getrandom((&raw mut x).cast(), 8, 0) };
    (x % n as u64) as usize
}

/// A random wallpaper other than `cur`, read and decoded straight into shared memory; a few
/// candidates are tried in case one is broken. Runs on a worker thread (listing included).
fn load(cur: Option<String>) -> Option<(String, ShmPixels)> {
    let mut files = vec![];
    list_files(&format!("{}/Wallpapers", crate::lib::xdg::home_dir()), &mut files);
    files.retain(|f| Some(f) != cur.as_ref());
    for _ in 0..3 {
        if files.is_empty() {
            break;
        }
        let path = files.swap_remove(random(files.len()));
        let Ok(bytes) = std::fs::read(&path) else { continue };
        if let Some(p) = image::decode_into(&bytes, ShmPixels::new, ShmPixels::data) {
            return Some((path, p));
        }
    }
    None
}

/// Swaps in another random wallpaper at once: the next one is always decoded ahead of time, which
/// costs a second full-size image in memory. Decodes never block the loop (lock, unlock, startup).
pub fn random_wallpaper() {
    match NEXT.with(|n| n.borrow_mut().take()) {
        Some(next) => {
            current().set(Some(next));
            preload();
        }
        None => {
            // nothing ready (startup, or switching faster than decoding): show it when it is
            SHOW_NEXT.with(|s| s.set(true));
            preload();
        }
    }
}

fn preload() {
    if LOADING.with(|l| l.replace(true)) {
        return;
    }
    let cur = current().with(|c| c.as_ref().map(|c| c.path.clone()));
    runtime::spawn(async move {
        let loaded = runtime::blocking(move || load(cur)).await.flatten();
        LOADING.with(|l| l.set(false));
        let show = SHOW_NEXT.with(|s| s.replace(false));
        let Some((path, pixels)) = loaded else { return };
        let w = Wallpaper { path, shm: ShmImage::from_pixels(pixels) };
        if show {
            current().set(Some(w));
            preload();
        } else {
            NEXT.with(|n| *n.borrow_mut() = Some(w));
        }
    });
}

/// The wallpaper as a cover-fit picture node (BackgroundSection header, lockscreen).
pub fn picture(style: Style, blur: f32) -> Node {
    match current().peek() {
        Some(w) => Node::new(Kind::Picture { pixmap: Img::Shm(w.shm), cover: true, blur, density: 1.0 }, style),
        None => Node::new(Kind::Custom { width: 0.0, height: 0.0, key: Some(0), paint: Rc::new(|_, _, _| {}) }, style),
    }
}

pub fn wallpaper_window() -> Rc<Win> {
    let spec = Spec {
        name: "wallpaper",
        layer: Layer::Background,
        anchor: Anchor::all(),
        exclusive: -1,
        input: false,
        per_monitor: true,
        ..Default::default()
    };
    let w = Win::new(spec, |_| Node::label(Style::default(), ""));
    let apply = |w: &Rc<Win>| {
        if let Some(cur) = current().peek() {
            w.show();
            w.set_image(Some(cur.shm));
        }
    };
    let win = Rc::downgrade(&w);
    current().subscribe(move || {
        if let Some(w) = win.upgrade() {
            apply(&w);
        }
        crate::ui::window::redraw_all(); // headers draw from it
    });
    apply(&w);
    w
}
