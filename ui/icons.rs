// flurry's own symbolic icons, filled with the current color like GTK recolors symbolic SVGs.
// Straight edges and square corners only: every icon is rectangles, polygons and 2-unit strokes
// (square caps, mitered joins) on a 16-unit grid, drawn at a whole number of device pixels per unit
// on a whole pixel, so every unit is the same width and horizontal and vertical edges stay crisp.

use std::sync::OnceLock;

use tiny_skia::{BlendMode, Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, PathSegment, Pixmap, PixmapMut, PixmapPaint, Point, Stroke, Transform};

const GRID: f32 = 16.0;
/// Opacity of the unlit parts (signal bands, volume waves), as Adwaita dims them.
const DIM: f32 = 0.35;

/// One piece of a shape, in grid units. A shape is its fills united, minus its cuts.
#[derive(Clone, Copy)]
enum Part {
    /// Filled rectangle x, y, w, h.
    R(i8, i8, i8, i8),
    /// Rectangle cut out.
    H(i8, i8, i8, i8),
    /// Filled polygon.
    P(&'static [(i8, i8)]),
    /// Polygon cut out.
    Q(&'static [(i8, i8)]),
    /// Polyline stroked 2 units wide.
    L(&'static [(i8, i8)]),
}
use Part::*;

struct Shape {
    alpha: f32,
    parts: &'static [Part],
}

const fn lit(parts: &'static [Part]) -> Shape {
    Shape { alpha: 1.0, parts }
}

const fn dim(parts: &'static [Part]) -> Shape {
    Shape { alpha: DIM, parts }
}

// Wi-Fi: a dot and two bands fanning up from (8, 13) between 45deg rays, chamfered where arcs would round.
const DOT: Part = P(&[(8, 13), (6, 11), (7, 10), (9, 10), (10, 11)]);
const BAND1: Part = P(&[(3, 8), (5, 6), (11, 6), (13, 8), (11, 10), (10, 9), (6, 9), (5, 10)]);
const BAND2: Part = P(&[(0, 5), (3, 2), (13, 2), (16, 5), (14, 7), (12, 5), (4, 5), (2, 7)]);

// "off" icons: a slash from the top left, with a gap cut around it
const SLASH: Shape = lit(&[L(&[(2, 2), (14, 14)])]);
const SLASH_GAP: Part = Q(&[(-1, 2), (2, -1), (17, 14), (14, 17)]);

// Volume: speaker and three arcs, each chamfered back toward the speaker at 45deg, each as tall as
// the one before plus 4.
const SPEAKER: Part = P(&[(1, 5), (3, 5), (6, 2), (6, 14), (3, 11), (1, 11)]);
const WAVE1: Part = P(&[(7, 5), (8, 5), (9, 6), (9, 10), (8, 11), (7, 11)]);
const WAVE2: Part = L(&[(9, 3), (11, 5), (11, 11), (9, 13)]);
const WAVE3: Part = L(&[(12, 2), (14, 4), (14, 12), (12, 14)]);

// Bluetooth rune.
const RUNE: Part = L(&[(5, 5), (11, 11), (8, 14), (8, 2), (11, 5), (5, 11)]);

const MIC: [Part; 6] = [R(6, 1, 4, 7), R(3, 5, 2, 4), R(11, 5, 2, 4), R(3, 9, 10, 2), R(7, 11, 2, 2), R(4, 13, 8, 2)];
const BELL: [Part; 3] = [R(7, 1, 2, 2), P(&[(5, 3), (11, 3), (12, 10), (14, 12), (2, 12), (4, 10)]), R(6, 13, 4, 2)];

// Design rules, in grid units:
// - content fits 14x14 at (1, 1), centered on (8, 8); only the wide keyboard and the Wi-Fi fan use
//   the full 16 width
// - lines, frames and bars are 2 thick; parts are 1 apart, holes and slits at least 2
// - diagonals run at 45deg; corners are square, chamfered (never rounded) where a curve would be
// - unlit parts are DIM; "off" is SLASH over a SLASH_GAP cut
const ICON_COUNT: usize = 29;
/// Sorted by name: looked up with a binary search.
static ICONS: [(&str, &[Shape]); ICON_COUNT] = [
    ("battery-full", &[lit(&[R(6, 1, 4, 2), R(3, 3, 10, 12), H(5, 5, 6, 8)]), lit(&[R(6, 6, 4, 6)])]),
    ("bell-off", &[lit(&[BELL[0], BELL[1], BELL[2], SLASH_GAP]), SLASH]),
    ("bluetooth", &[lit(&[RUNE])]),
    ("bluetooth-on", &[lit(&[RUNE, R(1, 7, 2, 2), R(13, 7, 2, 2)])]),
    ("brightness", &[lit(&[
        P(&[(6, 5), (10, 5), (11, 6), (11, 10), (10, 11), (6, 11), (5, 10), (5, 6)]),
        R(7, 1, 2, 3),
        R(7, 12, 2, 3),
        R(1, 7, 3, 2),
        R(12, 7, 3, 2),
        L(&[(3, 3), (4, 4)]),
        L(&[(13, 3), (12, 4)]),
        L(&[(3, 13), (4, 12)]),
        L(&[(13, 13), (12, 12)]),
    ])]),
    ("check", &[lit(&[L(&[(2, 8), (6, 12), (14, 4)])])]),
    ("headphones", &[lit(&[L(&[(2, 9), (2, 2), (14, 2), (14, 9)]), R(1, 8, 4, 7), R(11, 8, 4, 7)])]),
    ("key", &[lit(&[R(1, 4, 8, 8), H(3, 6, 4, 4), R(9, 7, 6, 2), R(13, 9, 2, 3), R(10, 9, 2, 2)])]),
    ("keyboard", &[lit(&[
        R(0, 2, 16, 12),
        H(2, 4, 2, 2),
        H(5, 4, 2, 2),
        H(8, 4, 2, 2),
        H(11, 4, 3, 2),
        H(2, 7, 3, 2),
        H(6, 7, 2, 2),
        H(9, 7, 2, 2),
        H(12, 7, 2, 2),
        H(4, 10, 8, 2),
    ])]),
    ("lock", &[lit(&[R(4, 1, 8, 2), R(4, 1, 2, 6), R(10, 1, 2, 6), R(2, 7, 12, 8), H(7, 10, 2, 2)])]),
    ("mic", &[lit(&MIC)]),
    ("mic-off", &[lit(&[MIC[0], MIC[1], MIC[2], MIC[3], MIC[4], MIC[5], SLASH_GAP]), SLASH]),
    ("mouse", &[lit(&[R(3, 1, 10, 14), H(7, 1, 2, 5), H(3, 6, 10, 2)])]),
    ("pause", &[lit(&[R(3, 1, 4, 14), R(9, 1, 4, 14)])]),
    ("phone", &[lit(&[R(4, 1, 8, 14), H(6, 3, 4, 8)])]),
    ("play", &[lit(&[P(&[(4, 1), (14, 8), (4, 15)])])]),
    // a square loop open at the top right, its arrowhead 8 wide pointing up into the opening
    ("refresh", &[lit(&[L(&[(7, 2), (2, 2), (2, 14), (11, 14), (11, 9)]), P(&[(7, 9), (11, 5), (15, 9)])])]),
    ("volume-high", &[lit(&[SPEAKER, WAVE1, WAVE2, WAVE3])]),
    ("volume-low", &[lit(&[SPEAKER, WAVE1]), dim(&[WAVE2, WAVE3])]),
    ("volume-medium", &[lit(&[SPEAKER, WAVE1, WAVE2]), dim(&[WAVE3])]),
    ("volume-muted", &[lit(&[SPEAKER, L(&[(8, 5), (14, 11)]), L(&[(8, 11), (14, 5)])])]),
    ("volume-over", &[lit(&[
        SPEAKER,
        P(&[(7, 4), (8, 4), (9, 5), (9, 11), (8, 12), (7, 12)]),
        L(&[(9, 2), (11, 4), (11, 12), (9, 14)]),
        L(&[(12, 1), (14, 3), (14, 13), (12, 15)]),
    ])]),
    ("wifi-0", &[dim(&[DOT, BAND1, BAND2])]),
    ("wifi-1", &[lit(&[DOT]), dim(&[BAND1, BAND2])]),
    ("wifi-2", &[lit(&[DOT, BAND1]), dim(&[BAND2])]),
    ("wifi-3", &[lit(&[DOT, BAND1, BAND2])]),
    // scanning: the ends lit, the middle dim (the levels always light up from the dot)
    ("wifi-connecting", &[lit(&[DOT, BAND2]), dim(&[BAND1])]),
    ("wifi-off", &[dim(&[DOT, BAND1, BAND2, SLASH_GAP]), SLASH]),
    ("wired", &[lit(&[R(5, 1, 6, 4), R(7, 5, 2, 2), R(3, 7, 10, 2), R(3, 9, 2, 2), R(11, 9, 2, 2), R(1, 11, 6, 4), R(9, 11, 6, 4)])]),
];

fn index(name: &str) -> Option<usize> {
    ICONS.binary_search_by(|(n, _)| n.cmp(&name)).ok()
}

/// Closed polygons in grid units, all wound clockwise so overlaps unite (nonzero rule).
type Contours = Vec<Vec<Point>>;

/// A shape's fills and cuts.
type Built = (Contours, Contours);

/// Contours are built once per icon, one pair per shape, on first draw.
static CONTOURS: [OnceLock<Vec<Built>>; ICON_COUNT] = [const { OnceLock::new() }; ICON_COUNT];

fn contours(i: usize) -> &'static [Built] {
    CONTOURS[i].get_or_init(|| {
        ICONS[i]
            .1
            .iter()
            .map(|s| {
                let (mut fills, mut cuts) = (Vec::new(), Vec::new());
                for p in s.parts {
                    let (cs, cut) = part(p);
                    if cut { cuts.extend(cs) } else { fills.extend(cs) }
                }
                (fills, cuts)
            })
            .collect()
    })
}

fn pts(p: &[(i8, i8)]) -> Vec<Point> {
    p.iter().map(|&(x, y)| Point::from_xy(x as f32, y as f32)).collect()
}

fn rect(x: i8, y: i8, w: i8, h: i8) -> Vec<Point> {
    pts(&[(x, y), (x + w, y), (x + w, y + h), (x, y + h)])
}

fn part(p: &Part) -> (Contours, bool) {
    let (mut cs, cut) = match *p {
        R(x, y, w, h) => (vec![rect(x, y, w, h)], false),
        H(x, y, w, h) => (vec![rect(x, y, w, h)], true),
        P(p) => (vec![pts(p)], false),
        Q(p) => (vec![pts(p)], true),
        L(p) => (stroke(p), false),
    };
    // y points down: a positive shoelace area is clockwise on screen
    let area: f32 = cs.iter().map(|c| c.iter().zip(c.iter().cycle().skip(1)).map(|(a, b)| a.x * b.y - b.x * a.y).sum::<f32>()).sum();
    if area < 0.0 {
        cs.iter_mut().for_each(|c| c.reverse());
    }
    (cs, cut)
}

fn stroke(p: &[(i8, i8)]) -> Contours {
    let mut pb = PathBuilder::new();
    for (i, q) in pts(p).into_iter().enumerate() {
        if i == 0 { pb.move_to(q.x, q.y) } else { pb.line_to(q.x, q.y) }
    }
    // joins sharper than 60deg are beveled: the rune's tips stay inside the grid
    let st = Stroke { width: 2.0, line_cap: LineCap::Square, line_join: LineJoin::Miter, miter_limit: 2.0, ..Stroke::default() };
    let outline = pb.finish().and_then(|path| path.stroke(&st, 1.0)).expect("icon stroke");
    let mut cs: Contours = Vec::new();
    for seg in outline.segments() {
        match seg {
            PathSegment::MoveTo(q) => cs.push(vec![q]),
            // square caps and miters are straight lines only
            PathSegment::LineTo(q) | PathSegment::QuadTo(_, q) | PathSegment::CubicTo(_, _, q) => cs.last_mut().unwrap().push(q),
            PathSegment::Close => {}
        }
    }
    cs
}

/// Draws icon `name` scaled to `size` px at (x, y). Unknown names draw nothing.
pub fn draw(pm: &mut PixmapMut, name: &str, size: f32, x: f32, y: f32, color: Color, ts: Transform) {
    let Some(i) = index(name) else { return };
    let t = grid_transform(size, x, y, ts);
    for (shape, (fills, cuts)) in ICONS[i].1.iter().zip(contours(i)) {
        let Some(path) = device_path(fills, t) else { continue };
        let mut paint = Paint::default();
        let mut c = color;
        c.apply_opacity(shape.alpha);
        paint.set_color(c);
        paint.anti_alias = true;
        if cuts.is_empty() {
            pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
            continue;
        }
        // cuts erase from the shape's fills only: drawn on a pixel-aligned layer of the shape's size
        let b = path.bounds();
        let (x0, y0) = (b.left().floor(), b.top().floor());
        let Some(mut layer) = Pixmap::new((b.right().ceil() - x0) as u32, (b.bottom().ceil() - y0) as u32) else { continue };
        let at = Transform::from_translate(-x0, -y0);
        layer.fill_path(&path, &paint, FillRule::Winding, at, None);
        if let Some(cut) = device_path(cuts, t) {
            paint.blend_mode = BlendMode::DestinationOut;
            paint.set_color(Color::BLACK);
            layer.fill_path(&cut, &paint, FillRule::Winding, at, None);
        }
        pm.draw_pixmap(x0 as i32, y0 as i32, layer.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    }
}

/// Grid units to device pixels for an icon box of `size` at (x, y): the largest whole number of
/// pixels per unit that fits (36px draws the grid at 32px), centered on a whole pixel.
fn grid_transform(size: f32, x: f32, y: f32, ts: Transform) -> Transform {
    // the surface transform only scales and translates
    let dev = size * ts.sx;
    let k = if dev >= GRID { (dev / GRID).floor() } else { dev / GRID };
    let mut o = Point::from_xy(x, y);
    ts.map_point(&mut o);
    let pad = (dev - GRID * k) / 2.0;
    Transform::from_row(k, 0.0, 0.0, k, (o.x + pad).round(), (o.y + pad).round())
}

/// `cs` through `t`, in device pixels.
fn device_path(cs: &Contours, t: Transform) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    for c in cs {
        for (j, &p) in c.iter().enumerate() {
            let mut q = p;
            t.map_point(&mut q);
            if j == 0 { pb.move_to(q.x, q.y) } else { pb.line_to(q.x, q.y) }
        }
        pb.close();
    }
    pb.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_for_binary_search() {
        assert!(ICONS.windows(2).all(|w| w[0].0 < w[1].0));
        for (i, (n, _)) in ICONS.iter().enumerate() {
            assert_eq!(index(n), Some(i));
        }
    }

    /// Every icon at the sizes flurry draws, then 16px and 36px blown up per pixel:
    /// `FLURRY_ICON_SHEET=sheet.png cargo test sheet -- --ignored`.
    #[test]
    #[ignore = "writes a PNG to $FLURRY_ICON_SHEET"]
    fn sheet() {
        use crate::ui::text::{self, Font, Wrap};
        use tiny_skia::Pixmap;
        let out = std::env::var("FLURRY_ICON_SHEET").expect("FLURRY_ICON_SHEET");
        const SIZES: [u32; 7] = [16, 19, 22, 24, 32, 36, 48];
        const ZOOMS: [(u32, u32); 2] = [(16, 6), (36, 3)];
        const GAP: u32 = 16;
        const LABEL: u32 = 160;
        const ROW: u32 = 120;
        let bg = Color::from_rgba8(0x24, 0x24, 0x24, 0xff);
        let fg = Color::from_rgba8(0xee, 0xee, 0xee, 0xff);
        let col_w = LABEL + SIZES.iter().map(|s| s + GAP).sum::<u32>() + ZOOMS.iter().map(|(s, k)| s * k + GAP).sum::<u32>() + GAP;
        let rows = ICON_COUNT.div_ceil(2) as u32;
        let mut sheet = Pixmap::new(col_w * 2, rows * ROW).unwrap();
        sheet.fill(bg);
        let font = Font { family: "Sora", weight: 400, size: 15.0 };
        for (i, (name, _)) in ICONS.iter().enumerate() {
            let (ox, oy) = ((i as u32 / rows) * col_w, (i as u32 % rows) * ROW);
            let cy = oy + ROW / 2;
            let l = text::layout(name, &font, None, Wrap::None, false);
            text::draw(&mut sheet.as_mut(), &l, (ox + GAP) as f32, cy as f32 - (l.height() / 2.0).round(), fg, Transform::identity());
            let mut x = ox + LABEL;
            for s in SIZES {
                draw(&mut sheet.as_mut(), name, s as f32, x as f32, (cy - s / 2) as f32, fg, Transform::identity());
                x += s + GAP;
            }
            for (s, k) in ZOOMS {
                let mut one = Pixmap::new(s, s).unwrap();
                one.fill(bg);
                draw(&mut one.as_mut(), name, s as f32, 0.0, 0.0, fg, Transform::identity());
                let (w, top) = (sheet.width(), cy - s * k / 2);
                let data = sheet.data_mut();
                for py in 0..s * k {
                    for px in 0..s * k {
                        let src = ((py / k * s + px / k) * 4) as usize;
                        let dst = (((top + py) * w + x + px) * 4) as usize;
                        data[dst..dst + 4].copy_from_slice(&one.data()[src..src + 4]);
                    }
                }
                x += s * k + GAP;
            }
        }
        let file = std::io::BufWriter::new(std::fs::File::create(&out).unwrap());
        let mut enc = png::Encoder::new(file, sheet.width(), sheet.height());
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(sheet.data()).unwrap();
    }
}
