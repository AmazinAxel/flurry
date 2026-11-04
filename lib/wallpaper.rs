// Random wallpaper from settings' WALLPAPER_DIR. The decoded image, shrunk to cover the largest
// output, lives in exactly one shm buffer: every monitor's background surface shows it, and
// BackgroundSection headers and the lockscreen draw from the same memory.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::settings::WALLPAPER_DIR;
use crate::ui::image;
use crate::ui::node::{Img, Kind, Node, Style};
use crate::ui::runtime;
use crate::ui::state::global_state;
use crate::ui::wayland::{Anchor, Layer, ShmImage, ShmPixels};
use crate::ui::window::{Spec, Win};

#[derive(Clone)]
pub struct Wallpaper {
    pub path: String,
    pub shm: Rc<ShmImage>,
    /// the file's own size, before shrinking
    full: (u32, u32),
}

impl PartialEq for Wallpaper {
    fn eq(&self, o: &Self) -> bool {
        Rc::ptr_eq(&self.shm, &o.shm)
    }
}

/// What to decode: a random file other than the current one, or the current one again (larger).
enum Pick {
    Other(Option<String>),
    Same(String),
}

thread_local! {
    static LOADING: Cell<bool> = const { Cell::new(false) };
    /// asked for while a decode ran: runs once it lands
    static QUEUED: RefCell<Option<Pick>> = const { RefCell::new(None) };
}

global_state! {
    pub fn current() -> Option<Wallpaper> = None;
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

/// Device pixel sizes of the active outputs (sway's logical rects at their scale): a wallpaper
/// never needs more than covering the largest.
fn output_sizes() -> Vec<(f64, f64)> {
    let Some(outs) = crate::lib::sway::outputs() else { return vec![] };
    outs.arr()
        .iter()
        .filter(|o| o.get("active").bool() == Some(true))
        .filter_map(|o| {
            let (r, s) = (o.get("rect"), o.get("scale").num().unwrap_or(1.0));
            Some((r.get("width").num()? * s, r.get("height").num()? * s))
        })
        .collect()
}

/// The size a `w`x`h` image is shrunk to so it still covers every output.
fn cover_size(w: u32, h: u32, outputs: &[(f64, f64)]) -> (u32, u32) {
    let s = outputs.iter().map(|&(ow, oh)| (ow / w as f64).max(oh / h as f64)).fold(0.0, f64::max);
    if outputs.is_empty() || s >= 1.0 {
        return (w, h);
    }
    ((w as f64 * s).ceil() as u32, (h as f64 * s).ceil() as u32)
}

fn wallpaper_dir() -> String {
    if WALLPAPER_DIR.starts_with('/') { WALLPAPER_DIR.to_string() } else { format!("{}/{WALLPAPER_DIR}", crate::lib::xdg::home_dir()) }
}

/// `path` read and decoded into shared memory, shrunk to cover the largest output, with its full size.
fn decode(path: &str, outputs: &[(f64, f64)]) -> Option<(ShmPixels, (u32, u32))> {
    let bytes = std::fs::read(path).ok()?;
    let full = Cell::new((0, 0));
    let fit = |w, h| {
        full.set((w, h));
        cover_size(w, h, outputs)
    };
    image::decode_into(&bytes, image::WALLPAPER_PIXELS, fit, ShmPixels::new).map(|p| (p, full.get()))
}

/// The picked wallpaper decoded; for a random one a few candidates are tried in case one is
/// broken. Runs on a worker thread (listing included).
fn load(pick: Pick) -> Option<(String, ShmPixels, (u32, u32))> {
    let outputs = output_sizes();
    let cur = match pick {
        Pick::Same(path) => return decode(&path, &outputs).map(|(p, full)| (path, p, full)),
        Pick::Other(cur) => cur,
    };
    let mut files = vec![];
    list_files(&wallpaper_dir(), &mut files);
    files.retain(|f| Some(f) != cur.as_ref());
    for _ in 0..3 {
        if files.is_empty() {
            break;
        }
        let Some(i) = crate::lib::random::below(files.len() as u64).map(|i| i as usize) else { break };
        let path = files.swap_remove(i);
        if let Some((p, full)) = decode(&path, &outputs) {
            return Some((path, p, full));
        }
    }
    None
}

/// Shows another random wallpaper once it's decoded (only one image is ever held). Decodes never
/// block the loop (lock, unlock, startup).
pub fn random_wallpaper() {
    let cur = current().with(|c| c.as_ref().map(|c| c.path.clone()));
    start(Pick::Other(cur));
}

/// A newly plugged output larger than the current decode covers gets it re-decoded at its size.
fn outputs_changed() {
    let Some(w) = current().peek() else { return };
    let outputs = output_sizes();
    let (nw, nh) = cover_size(w.full.0, w.full.1, &outputs);
    if nw > w.shm.width || nh > w.shm.height {
        start(Pick::Same(w.path));
    }
}

fn start(pick: Pick) {
    if LOADING.with(|l| l.replace(true)) {
        // a random pick is decoded at the current outputs anyway: it covers a re-decode
        QUEUED.with(|q| {
            let mut q = q.borrow_mut();
            if !matches!((&pick, &*q), (Pick::Same(_), Some(Pick::Other(_)))) {
                *q = Some(pick);
            }
        });
        return;
    }
    runtime::spawn(async move {
        let loaded = runtime::blocking(move || load(pick)).await.flatten();
        LOADING.with(|l| l.set(false));
        if let Some((path, pixels, full)) = loaded {
            current().set(Some(Wallpaper { path, shm: ShmImage::from_pixels(pixels), full }));
        }
        // queued against what's showing now
        let cur = current().with(|c| c.as_ref().map(|c| c.path.clone()));
        match QUEUED.with(|q| q.borrow_mut().take()) {
            Some(Pick::Other(_)) => start(Pick::Other(cur)),
            Some(Pick::Same(_)) => outputs_changed(),
            None => {}
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
            w.set_image(cur.shm);
        }
    };
    crate::lib::sway::subscribe(&["output"], |_| outputs_changed());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_the_largest_output() {
        assert_eq!(cover_size(6000, 4000, &[(2560.0, 1440.0), (1920.0, 1080.0)]), (2560, 1707));
        assert_eq!(cover_size(1000, 1000, &[(2560.0, 1440.0)]), (1000, 1000));
        assert_eq!(cover_size(6000, 4000, &[]), (6000, 4000));
    }
}
