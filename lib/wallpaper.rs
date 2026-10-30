// Random wallpaper from ~/Wallpapers (lib/wallpaper.tsx). The decoded image lives in exactly one shm
// buffer: every monitor's background surface shows it, and BackgroundSection headers and the
// lockscreen draw from the same memory.

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
        self.path == o.path && Rc::ptr_eq(&self.shm, &o.shm)
    }
}

thread_local! {
    static CURRENT: State<Option<Wallpaper>> = State::new(None);
}

pub fn current() -> State<Option<Wallpaper>> {
    CURRENT.with(Clone::clone)
}

fn list_files(dir: &str, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = format!("{dir}/{}", e.file_name().to_string_lossy());
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            list_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn random(n: usize) -> usize {
    let mut x = 0u64;
    unsafe { libc::getrandom((&raw mut x).cast(), 8, 0) };
    (x % n as u64) as usize
}

fn pick() -> Option<String> {
    let mut files = vec![];
    list_files(&format!("{}/Wallpapers", crate::lib::xdg::home_dir()), &mut files);
    let cur = current().with(|c| c.as_ref().map(|c| c.path.clone()));
    files.retain(|f| Some(f) != cur.as_ref());
    (!files.is_empty()).then(|| files.swap_remove(random(files.len())))
}

/// Reads and decodes straight into shared memory; runs on a worker thread.
fn decode(path: &str) -> Option<ShmPixels> {
    image::decode_into(&std::fs::read(path).ok()?, |w, h, fill| {
        let mut p = ShmPixels::new(w, h)?;
        fill(p.data()).then_some(p)
    })
}

/// Swaps in another random wallpaper (decoded on demand: a preloaded one would cost a second
/// full-size image in memory all the time). The decode never blocks the loop (lock, unlock, startup).
pub fn random_wallpaper() {
    let Some(path) = pick() else { return };
    runtime::spawn(async move {
        let p = path.clone();
        if let Some(Some(pixels)) = runtime::blocking(move || decode(&p)).await {
            current().set(Some(Wallpaper { path, shm: ShmImage::from_pixels(pixels) }));
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
