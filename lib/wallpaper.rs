// Random wallpaper from ~/Wallpapers (lib/wallpaper.tsx). The decoded image lives in exactly one shm
// buffer: every monitor's background surface shows it, and BackgroundSection headers and the
// lockscreen draw from the same memory.

use std::rc::Rc;

use crate::lib::state::State;
use crate::ui::image;
use crate::ui::node::{Img, Kind, Node, Style};
use crate::ui::wayland::{Anchor, Layer, ShmImage};
use crate::ui::window::{Spec, Win};

#[derive(Clone)]
pub struct Wallpaper {
    pub path: String,
    pub shm: Rc<ShmImage>,
    pub light: bool,
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

pub fn light() -> bool {
    current().with(|w| w.as_ref().is_some_and(|w| w.light))
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

fn load(path: String) -> Option<Wallpaper> {
    let pix = image::decode(&std::fs::read(&path).ok()?)?;
    let light = image::is_light(&pix);
    let shm = ShmImage::new(pix.width(), pix.height(), |dst| dst.copy_from_slice(pix.data()))?;
    Some(Wallpaper { path, shm, light })
}

/// Swaps in another random wallpaper (decoded on demand: a preloaded one would cost a second
/// full-size image in memory all the time).
pub fn random_wallpaper() {
    if let Some(w) = pick().and_then(load) {
        current().set(Some(w));
    }
}

/// The wallpaper as a cover-fit picture node (BackgroundSection header, lockscreen).
pub fn picture(style: Style, blur: f32) -> Node {
    match current().peek() {
        Some(w) => Node::new(Kind::Picture { pixmap: Img::Shm(w.shm), cover: true, blur }, style),
        None => Node::new(Kind::Custom { width: 0.0, height: 0.0, paint: Rc::new(|_, _, _| {}) }, style),
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
    });
    apply(&w);
    w
}
