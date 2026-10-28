// Symbolic icons baked by build.rs, filled with the current color like GTK recolors symbolic SVGs.

use tiny_skia::{Color, FillRule, Paint, PathBuilder, PixmapMut, Transform};

pub struct Shape {
    pub opacity: f32,
    pub even_odd: bool,
    pub segs: &'static [Seg],
}

pub enum Seg {
    M(f32, f32),
    L(f32, f32),
    #[allow(dead_code)] // build.rs emits it for quadratic curves; no vendored icon has one yet
    Q(f32, f32, f32, f32),
    C(f32, f32, f32, f32, f32, f32),
    Z,
}
use Seg::*;

include!(concat!(env!("OUT_DIR"), "/icons.rs"));

#[cfg(test)]
fn exists(name: &str) -> bool {
    ICONS.binary_search_by(|(n, _, _)| n.cmp(&name)).is_ok()
}
thread_local! {
    /// Paths are built once per icon on first draw.
    static PATHS: std::cell::RefCell<std::collections::HashMap<usize, std::rc::Rc<Vec<Option<tiny_skia::Path>>>>> = Default::default();
}

fn paths(i: usize) -> std::rc::Rc<Vec<Option<tiny_skia::Path>>> {
    if let Some(p) = PATHS.with(|c| c.borrow().get(&i).cloned()) {
        return p;
    }
    let built: Vec<Option<tiny_skia::Path>> = ICONS[i]
        .2
        .iter()
        .map(|shape| {
            let mut pb = PathBuilder::new();
            for s in shape.segs {
                match *s {
                    M(x, y) => pb.move_to(x, y),
                    L(x, y) => pb.line_to(x, y),
                    Q(a, b, x, y) => pb.quad_to(a, b, x, y),
                    C(a, b, c, d, x, y) => pb.cubic_to(a, b, c, d, x, y),
                    Z => pb.close(),
                }
            }
            pb.finish()
        })
        .collect();
    let built = std::rc::Rc::new(built);
    PATHS.with(|c| c.borrow_mut().insert(i, built.clone()));
    built
}

/// Draws icon `name` scaled to `size` px at (x, y). Unknown names draw nothing (GTK's missing icon is
/// never shown in flurry since every name is baked).
pub fn draw(pm: &mut PixmapMut, name: &str, size: f32, x: f32, y: f32, color: Color, ts: Transform) {
    let Ok(i) = ICONS.binary_search_by(|(n, _, _)| n.cmp(&name)) else { return };
    let (_, view, shapes) = &ICONS[i];
    let t = ts.pre_translate(x, y).pre_scale(size / view, size / view);
    for (shape, path) in shapes.iter().zip(paths(i).iter()) {
        let Some(path) = path else { continue };
        let mut paint = Paint::default();
        let mut c = color;
        c.apply_opacity(shape.opacity);
        paint.set_color(c);
        paint.anti_alias = true;
        let rule = if shape.even_odd { FillRule::EvenOdd } else { FillRule::Winding };
        pm.fill_path(path, &paint, rule, t, None);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn table_is_sorted_for_binary_search() {
        assert!(super::ICONS.windows(2).all(|w| w[0].0 < w[1].0));
        for n in ["network-wireless-symbolic", "audio-volume-muted-symbolic", "bluetooth-active-symbolic"] {
            assert!(super::exists(n), "{n}");
        }
    }
}
