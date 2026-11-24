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
    /// Filled polygon in half units: for 45deg edges whose corners fall between grid lines (horizontal
    /// and vertical edges still sit on whole units).
    Ph(&'static [(i8, i8)]),
    /// Polygon cut out.
    Q(&'static [(i8, i8)]),
    /// Polyline stroked 2 units wide.
    L(&'static [(i8, i8)]),
    /// Polyline stroked 2 units wide, in half units.
    Lh(&'static [(i8, i8)]),
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

// Bands (Wi-Fi and volume): 2 thick across their flats and 3/sqrt2 (about 2.1) across their 45deg
// chamfers, each 2 (2.1) from the next, ends cut along the 45deg rays from the fan's apex.

// Wi-Fi: a dot and two bands fanning up from (8, 14).
const DOT: Part = P(&[(8, 14), (6, 12), (7, 11), (9, 11), (10, 12)]);
const BAND1: Part = Ph(&[(6, 18), (10, 14), (22, 14), (26, 18), (23, 21), (20, 18), (12, 18), (9, 21)]);
const BAND2: Part = Ph(&[(0, 12), (6, 6), (26, 6), (32, 12), (29, 15), (24, 10), (8, 10), (3, 15)]);

// "off" icons: a slash from the top left, with a 2 wide gap cut around it
// half a unit down-left of center: it then cuts the end of the Wi-Fi fan's middle band instead of running
// along it
const SLASH: Shape = lit(&[Lh(&[(3, 5), (27, 29)])]);
const SLASH_GAP: Part = Q(&[(-1, 4), (3, 0), (17, 14), (13, 18)]);

// Volume: speaker and two bands fanning right from (6, 8), the mouth's edge: the first band 2 in front
// of the mouth at its ends (3 at its flat), like every other gap.
const SPEAKER: Part = P(&[(1, 5), (3, 5), (6, 2), (6, 14), (3, 11), (1, 11)]);
const WAVE1: Part = Ph(&[(19, 9), (22, 12), (22, 20), (19, 23), (16, 20), (18, 18), (18, 14), (16, 12)]);
const WAVE2: Part = Ph(&[(25, 3), (30, 8), (30, 24), (25, 29), (22, 26), (26, 22), (26, 10), (22, 6)]);
// over 100%: the outer band a half unit longer at each end
const WAVE2_OVER: Part = Ph(&[(24, 2), (30, 8), (30, 24), (24, 30), (21, 27), (26, 22), (26, 10), (21, 5)]);

// Bluetooth rune: all 45deg (like the dots), the crossing through (8, 8); the arrowheads are as wide
// as the 14 tall stem allows.
const RUNE: Part = Lh(&[(9, 9), (23, 23), (16, 30), (16, 2), (23, 9), (9, 23)]);
// connected: a 45deg-turned dot each side of the crossing, 2.2 from the rune's tails and arrowheads
const BT_DOTS: [Part; 2] = [Ph(&[(4, 13), (7, 16), (4, 19), (1, 16)]), Ph(&[(28, 13), (31, 16), (28, 19), (25, 16)])];

const MIC: [Part; 6] = [R(6, 1, 4, 6), R(2, 5, 2, 4), R(12, 5, 2, 4), R(2, 9, 12, 2), R(7, 11, 2, 2), R(4, 13, 8, 2)];
const BELL: [Part; 3] = [R(7, 1, 2, 2), P(&[(5, 3), (11, 3), (12, 9), (14, 11), (2, 11), (4, 9)]), R(6, 13, 4, 2)];

// Design rules, in grid units:
// - content fits 14x14 at (1, 1), centered on (8, 8); only the wide keyboard and the Wi-Fi and volume
//   fans use the full 16
// - lines, frames and bars are 2 thick; separate parts are 2 apart, holes and slits at least 2
// - diagonals run at 45deg; corners are square, chamfered (never rounded) where a curve would be
// - unlit parts are DIM; "off" is SLASH over a SLASH_GAP cut
const ICON_COUNT: usize = 28;
/// Sorted by name: looked up with a binary search.
static ICONS: [(&str, &[Shape]); ICON_COUNT] = [
    // solid: a level block inside the 2 thick frame would leave only 1 unit gaps
    ("battery-full", &[lit(&[R(6, 1, 4, 2), R(3, 3, 10, 12)])]),
    // the bell's lower right edge lies on the gap's: cut past it, so no hairline is left
    ("bell-off", &[lit(&[BELL[0], BELL[1], BELL[2], SLASH_GAP, Q(&[(13, 9), (16, 12), (16, 14), (11, 9)])]), SLASH]),
    ("bluetooth", &[lit(&[RUNE])]),
    ("bluetooth-on", &[lit(&[RUNE, BT_DOTS[0], BT_DOTS[1]])]),
    // a 6x6 core chamfered 2.5 and eight 2x2 rays (the diagonal ones turned 45deg), each 2 (2.1) from it
    ("brightness", &[lit(&[
        Ph(&[(10, 15), (15, 10), (17, 10), (22, 15), (22, 17), (17, 22), (15, 22), (10, 17)]),
        R(7, 1, 2, 2),
        R(7, 13, 2, 2),
        R(1, 7, 2, 2),
        R(13, 7, 2, 2),
        Ph(&[(8, 5), (11, 8), (8, 11), (5, 8)]),
        Ph(&[(24, 5), (27, 8), (24, 11), (21, 8)]),
        Ph(&[(8, 21), (11, 24), (8, 27), (5, 24)]),
        Ph(&[(24, 21), (27, 24), (24, 27), (21, 24)]),
    ])]),
    // one band, 2 thick, square outside, widening inward along a 45deg step into flat 4 wide cups
    ("headphones", &[lit(&[P(&[(1, 15), (1, 1), (15, 1), (15, 15), (11, 15), (11, 9), (13, 7), (13, 3), (3, 3), (3, 7), (5, 9), (5, 15)])])]),
    // 2x2 keys (one 4 wide per row, staggered) and a space bar, 2 from each other and from the frame
    ("keyboard", &[lit(&[
        R(0, 1, 16, 14),
        H(2, 3, 2, 2),
        H(6, 3, 2, 2),
        H(10, 3, 4, 2),
        H(2, 7, 4, 2),
        H(8, 7, 2, 2),
        H(12, 7, 2, 2),
        H(4, 11, 8, 2),
    ])]),
    ("lock", &[lit(&[R(4, 1, 8, 2), R(4, 1, 2, 6), R(10, 1, 2, 6), R(2, 7, 12, 8), H(7, 10, 2, 2)])]),
    ("mic", &[lit(&MIC)]),
    ("mic-off", &[lit(&[MIC[0], MIC[1], MIC[2], MIC[3], MIC[4], MIC[5], SLASH_GAP]), SLASH]),
    ("mouse", &[lit(&[R(3, 1, 10, 14), H(7, 1, 2, 5), H(3, 6, 10, 2)])]),
    ("pause", &[lit(&[R(3, 1, 4, 14), R(9, 1, 4, 14)])]),
    ("phone", &[lit(&[R(4, 1, 8, 14), H(6, 3, 4, 10)])]),
    ("play", &[lit(&[P(&[(4, 1), (14, 8), (4, 15)])])]),
    // a 12x10 loop in two halves, each ending in a 6 tall arrowhead (2 past the band each side) whose
    // tip meets the loop's side above the other half, whose end is sliced at 45deg, 2 (2.1) under it
    ("refresh", &[lit(&[
        Ph(&[(4, 18), (4, 6), (22, 6), (22, 2), (28, 8), (22, 14), (22, 10), (8, 10), (8, 14)]),
        Ph(&[(28, 14), (28, 26), (10, 26), (10, 30), (4, 24), (10, 18), (10, 22), (24, 22), (24, 18)]),
    ])]),
    // a 10x14 cabinet, walls 2: a 2x2 tweeter over a 6x6 woofer ring (2 thick, around a 2x2 cone), 2 apart
    ("speaker", &[lit(&[R(3, 1, 10, 14), H(7, 3, 2, 2), H(5, 7, 6, 6)]), lit(&[R(7, 9, 2, 2)])]),
    ("volume-high", &[lit(&[SPEAKER, WAVE1, WAVE2])]),
    ("volume-low", &[lit(&[SPEAKER]), dim(&[WAVE1, WAVE2])]),
    ("volume-medium", &[lit(&[SPEAKER, WAVE1]), dim(&[WAVE2])]),
    // the X starts where the first band's ends do, 2 in front of the mouth
    ("volume-muted", &[lit(&[SPEAKER, Lh(&[(19, 12), (27, 20)]), Lh(&[(19, 20), (27, 12)])])]),
    ("volume-over", &[lit(&[SPEAKER, WAVE1, WAVE2_OVER])]),
    ("wifi-0", &[dim(&[DOT, BAND1, BAND2])]),
    ("wifi-1", &[lit(&[DOT]), dim(&[BAND1, BAND2])]),
    ("wifi-2", &[lit(&[DOT, BAND1]), dim(&[BAND2])]),
    ("wifi-3", &[lit(&[DOT, BAND1, BAND2])]),
    // scanning: the ends lit, the middle dim (the levels always light up from the dot)
    ("wifi-connecting", &[lit(&[DOT, BAND2]), dim(&[BAND1])]),
    ("wifi-off", &[lit(&[DOT, BAND1, BAND2, SLASH_GAP]), SLASH]),
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
        Ph(p) => (vec![pts(p).into_iter().map(|q| Point::from_xy(q.x / 2.0, q.y / 2.0)).collect()], false),
        Q(p) => (vec![pts(p)], true),
        L(p) => (stroke(p, 1.0), false),
        Lh(p) => (stroke(p, 0.5), false),
    };
    // y points down: a positive shoelace area is clockwise on screen
    let area: f32 = cs.iter().map(|c| c.iter().zip(c.iter().cycle().skip(1)).map(|(a, b)| a.x * b.y - b.x * a.y).sum::<f32>()).sum();
    if area < 0.0 {
        cs.iter_mut().for_each(|c| c.reverse());
    }
    (cs, cut)
}

fn stroke(p: &[(i8, i8)], unit: f32) -> Contours {
    let mut pb = PathBuilder::new();
    for (i, q) in pts(p).into_iter().map(|q| Point::from_xy(q.x * unit, q.y * unit)).enumerate() {
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

    /// Every icon at the sizes flurry draws (64: 32 at scale 2), then 16px and 36px blown up per pixel:
    /// `FLURRY_ICON_SHEET=sheet.png cargo test sheet -- --ignored`.
    #[test]
    #[ignore = "writes a PNG to $FLURRY_ICON_SHEET"]
    fn sheet() {
        use crate::ui::text::{self, Font, Wrap};
        use tiny_skia::Pixmap;
        let out = std::env::var("FLURRY_ICON_SHEET").expect("FLURRY_ICON_SHEET");
        const SIZES: [u32; 8] = [16, 19, 22, 24, 32, 36, 48, 64];
        const ZOOMS: [(u32, u32); 2] = [(16, 6), (36, 3)];
        const GAP: u32 = 16;
        const LABEL: u32 = 160;
        const ROW: u32 = 128;
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
