// Random wallpaper from ~/Wallpapers (lib/wallpaper.tsx). The decoded image, shrunk to cover the
// largest output, lives in exactly one shm buffer: every monitor's background surface shows it, and
// BackgroundSection headers and the lockscreen draw from the same memory.

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

/// A random wallpaper other than `cur`, read and decoded into shared memory, shrunk to cover the
/// largest output; a few candidates are tried in case one is broken. Runs on a worker thread
/// (listing included).
fn load(cur: Option<String>) -> Option<(String, ShmPixels)> {
    let mut files = vec![];
    list_files(&format!("{}/Wallpapers", crate::lib::xdg::home_dir()), &mut files);
    files.retain(|f| Some(f) != cur.as_ref());
    let outputs = output_sizes();
    for _ in 0..3 {
        if files.is_empty() {
            break;
        }
        let Some(i) = crate::lib::random::below(files.len() as u64).map(|i| i as usize) else { break };
        let path = files.swap_remove(i);
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let fit = |w, h| cover_size(w, h, &outputs);
        if let Some(p) = image::decode_into(&bytes, image::WALLPAPER_PIXELS, fit, ShmPixels::new, ShmPixels::data) {
            return Some((path, p));
        }
    }
    None
}

/// Swaps in another random wallpaper at once: the next one is always decoded ahead of time, which
/// costs a second (output-sized) image in memory. Decodes never block the loop (lock, unlock, startup).
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
