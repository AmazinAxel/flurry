// flurry's own symbolic icons, filled with the current color like GTK recolors symbolic SVGs.
// Straight edges and square corners only: every icon is rectangles, polygons and 2-unit strokes
// (square caps, mitered joins) on a 16-unit grid, drawn at a whole number of device pixels per unit
// on a whole pixel, so every unit is the same width and horizontal and vertical edges stay crisp.

use std::f32::consts::{FRAC_1_SQRT_2 as S, SQRT_2 as R2};
use std::sync::OnceLock;

use tiny_skia::{BlendMode, Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, PathSegment, Pixmap, PixmapMut, PixmapPaint, Point, Stroke, Transform};

const GRID: f32 = 16.0;
/// Opacity of the unlit parts (signal bands, volume waves), as Adwaita dims them.
const DIM: f32 = 0.35;
/// tan 22.5deg: half a regular octagon's side over its inradius.
const T: f32 = R2 - 1.0;

/// One piece of a shape, in grid units. A shape is its fills united, minus its cuts.
#[derive(Clone, Copy)]
enum Part {
    /// Filled rectangle x, y, w, h.
    R(i8, i8, i8, i8),
    /// Rectangle cut out.
    H(i8, i8, i8, i8),
    /// Filled polygon.
    P(&'static [(i8, i8)]),
    /// Filled polygon off the grid: 45deg edges placed for exact thicknesses and gaps (its horizontal
    /// and vertical edges still sit on whole units).
    Pf(&'static [(f32, f32)]),
    /// Polygon cut out, off the grid.
    Qf(&'static [(f32, f32)]),
    /// Polyline stroked 2 units wide, off the grid.
    Lf(&'static [(f32, f32)]),
    /// A fan's band (see `Fan`).
    B(Fan),
    /// A 2x2 square turned 45deg, centered on x, y.
    D(f32, f32),
}
use Part::*;

/// The part of a regular octagon ring around `apex` between the 45deg rays either side of its axis,
/// up (or right), from inradius `r0` (0: a wedge from the apex) to `r1`: exactly as thick across its
/// 45deg faces as across its flat one. `ext` lengthens both ends along their faces.
#[derive(Clone, Copy)]
struct Fan {
    apex: (f32, f32),
    right: bool,
    r0: f32,
    r1: f32,
    ext: f32,
}

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

const fn band(apex: (f32, f32), right: bool, r0: f32, r1: f32) -> Part {
    B(Fan { apex, right, r0, r1, ext: 0.0 })
}

// Wi-Fi: a dot and two bands fanning up from (8, 14), whole radii, so the flats are on whole units.
const WIFI: (f32, f32) = (8.0, 14.0);
const DOT: Part = band(WIFI, false, 0.0, 3.0);
const BAND1: Part = band(WIFI, false, 5.0, 7.0);
const BAND2: Part = band(WIFI, false, 9.0, 11.0);

// "off" icons: a slash corner to corner, with a gap 2 wide cut around it and its caps. Wi-Fi's band ends
// and right faces run along it, so it sits where the gap leaves those ends exactly 2 thick and takes the
// middle band's right face whole: SLASH_OFF (x - y) up-right of the center line, no slivers anywhere.
// Its ends stop where their square caps' outer corners touch the 14x14 box (y 1 at the top, x 15 at the
// bottom).
const SLASH_OFF: f32 = 5.0 * R2 - 6.0;
const SLASH_A: (f32, f32) = (1.0 + R2 + SLASH_OFF, 1.0 + R2);
const SLASH_B: (f32, f32) = (15.0 - R2, 15.0 - R2 - SLASH_OFF);
const SLASH: Shape = lit(&[Lf(&[SLASH_A, SLASH_B])]);
const SLASH_GAP: Part = Qf(&[
    (SLASH_A.0 - 6.0 * S, SLASH_A.1),
    (SLASH_A.0, SLASH_A.1 - 6.0 * S),
    (SLASH_B.0 + 6.0 * S, SLASH_B.1),
    (SLASH_B.0, SLASH_B.1 + 6.0 * S),
]);

// Volume: speaker and two bands fanning right, the first band's ends 2 in front of the mouth (x 6) and
// its flat at x 9: the apex sits where both hold, so every flat is on a whole unit.
const SPEAKER: Part = P(&[(1, 5), (3, 5), (6, 2), (6, 14), (3, 11), (1, 11)]);
const VOLUME: (f32, f32) = (7.0 - R2, 8.0);
const WAVE1: Part = band(VOLUME, true, 2.0 + R2, 4.0 + R2);
const WAVE2: Part = band(VOLUME, true, 6.0 + R2, 8.0 + R2);
// over 100%: the outer band half a unit longer at each end, in x and y
const WAVE2_OVER: Part = B(Fan { apex: VOLUME, right: true, r0: 6.0 + R2, r1: 8.0 + R2, ext: S });

// Bluetooth rune, solid: a 2 wide stem from y 1 to 15 (flat ends), two arrowheads off its top and
// bottom right corners whose inner faces are the 2 thick crossing bars, and the bars' tails, ending
// square where their outer corners are as far left (8 - 4.71) as the arrowhead tips are right. Hollow
// arrowheads could only have 0.9 wide holes. All 45deg, crossing at (8, 8).
const RUNE: [Part; 4] = [
    // upper arrowhead and the stem above the crossing
    Pf(&[(7.0, 1.0), (9.0, 1.0), (12.0 + S, 4.0 + S), (7.0, 9.0 + R2)]),
    // lower arrowhead and the stem below it
    Pf(&[(7.0, 7.0 - R2), (12.0 + S, 12.0 - S), (9.0, 15.0), (7.0, 15.0)]),
    // upper left tail, of the bar into the lower arrowhead
    Pf(&[(4.0 - S, 4.0 + S), (4.0 + S, 4.0 - S), (8.0 + S, 8.0 - S), (8.0 - S, 8.0 + S)]),
    // lower left tail, of the bar into the upper arrowhead
    Pf(&[(4.0 - S, 12.0 - S), (8.0 - S, 8.0 - S), (8.0 + S, 8.0 + S), (4.0 + S, 12.0 + S)]),
];
// connected: a dot each side of the crossing, 2 from the bars either side of it: full diamonds
// (2x2 turned 45deg, like the diagonal brightness rays), so their tips sit 7.07 out: 0.07 past the box
const BT_DOTS: [Part; 2] = [D(8.0 - 4.0 * R2, 8.0), D(8.0 + 4.0 * R2, 8.0)];

// Brightness: an octagon core (6 across its flats) and eight 2x2 rays (the diagonal ones turned 45deg).
// The straight rays are 2 from the core; the diagonal ones sit RAY_D out along both axes, where each is
// exactly 2 from the straight rays beside it (any closer and those gaps shrink to 1.98), and the core's
// 45deg faces sit 0.05 further out than a regular octagon's, so they are 2 from the diagonal rays too.
// The levels light the straight rays, then the diagonal ones.
/// Solves |(7, 3) - (8 - d + sqrt2, 8 - d)| = 2: (6 + sqrt2 + sqrt(8 sqrt2 - 10)) / 2.
const RAY_D: f32 = 4.280_192_4;
/// Half the core's flat faces: where its 45deg faces, RAY_D sqrt2 - 3 from the center, meet them.
const SUN_C: f32 = 2.0 * RAY_D - 3.0 * R2 - 3.0;
const SUN: Part = Pf(&[
    (8.0 - SUN_C, 5.0),
    (8.0 + SUN_C, 5.0),
    (11.0, 8.0 - SUN_C),
    (11.0, 8.0 + SUN_C),
    (8.0 + SUN_C, 11.0),
    (8.0 - SUN_C, 11.0),
    (5.0, 8.0 + SUN_C),
    (5.0, 8.0 - SUN_C),
]);
const RAYS: [Part; 8] = [
    R(7, 1, 2, 2),
    R(7, 13, 2, 2),
    R(1, 7, 2, 2),
    R(13, 7, 2, 2),
    D(8.0 - RAY_D, 8.0 - RAY_D),
    D(8.0 + RAY_D, 8.0 - RAY_D),
    D(8.0 - RAY_D, 8.0 + RAY_D),
    D(8.0 + RAY_D, 8.0 + RAY_D),
];

// Refresh: a 12x10 loop in two halves, each ending in a 6 tall arrowhead (2 past the band each side)
// whose tip meets the loop's side above the other half, whose end is sliced at 45deg, 2 under it.
const REFRESH: [Part; 2] = [
    Pf(&[(2.0, 12.0 - 2.0 * R2), (2.0, 3.0), (11.0, 3.0), (11.0, 1.0), (14.0, 4.0), (11.0, 7.0), (11.0, 5.0), (4.0, 5.0), (4.0, 10.0 - 2.0 * R2)]),
    Pf(&[(14.0, 4.0 + 2.0 * R2), (14.0, 13.0), (5.0, 13.0), (5.0, 15.0), (2.0, 12.0), (5.0, 9.0), (5.0, 11.0), (12.0, 11.0), (12.0, 6.0 + 2.0 * R2)]),
];

// Muted: an X starting where the first band's ends do, 2 in front of the mouth.
const MUTE_X: [Part; 2] = [Lf(&[(8.0 + R2, 6.0), (12.0 + R2, 10.0)]), Lf(&[(8.0 + R2, 10.0), (12.0 + R2, 6.0)])];

const MIC: [Part; 6] = [R(6, 1, 4, 6), R(2, 5, 2, 4), R(12, 5, 2, 4), R(2, 9, 12, 2), R(7, 11, 2, 2), R(4, 13, 8, 2)];
// 14 wide at the flare: narrower, the slash's gap would leave its upper right corner under 2 thick
const BELL: [Part; 3] = [R(7, 1, 2, 2), P(&[(4, 3), (12, 3), (13, 9), (15, 11), (1, 11), (3, 9)]), R(6, 13, 4, 2)];

// Design rules, in grid units (the full spec, for drawing new icons: ui/ICONS.md):
// - content fits 14x14 at (1, 1), centered on (8, 8); only the Wi-Fi and volume
//   fans and the Bluetooth dots (2 from the crossing's bars puts their tips 0.07 out) use the full 16
// - one style: solid silhouettes with 2 wide cutouts, plus 2 thick lines for glyphs that have no
//   silhouette (Wi-Fi, waves, the rune's tails, refresh, slash, shackle, headphone band)
// - lines, bands and rays are exactly 2 thick, across 45deg edges too; every part is exactly 2 from its
//   nearest neighbor and never closer to any; no part, fragment, hole or slit is narrower than 2
//   (`exact_widths_and_gaps` measures the geometry; `no_slivers_or_tight_gaps`, `nearest_gaps_are_exactly_2`
//   and `sizes` every rendered icon)
// - upright objects (battery, mouse, phone, speaker, play, pause) are 10x14, the wide keyboard 14x10,
//   lock, mic and refresh 12x14
// - horizontal and vertical edges sit on whole units; 45deg edges sit wherever those 2s put them
// - diagonals run at 45deg (frozen exceptions: play, the bell's upper sides); corners are square,
//   chamfered (never rounded) where a curve would be
// - leveled icons (Wi-Fi, volume, brightness) light parts in order and DIM the rest; "off" is SLASH
//   over a SLASH_GAP cut
const ICON_COUNT: usize = 30;
/// Sorted by name: looked up with a binary search.
static ICONS: [(&str, &[Shape]); ICON_COUNT] = [
    // solid: a level block inside the 2 thick frame would leave only 1 unit gaps
    ("battery-full", &[lit(&[R(6, 1, 4, 2), R(3, 3, 10, 12)])]),
    ("bell-off", &[lit(&[BELL[0], BELL[1], BELL[2], SLASH_GAP]), SLASH]),
    ("bluetooth", &[lit(&RUNE)]),
    ("bluetooth-on", &[lit(&[RUNE[0], RUNE[1], RUNE[2], RUNE[3], BT_DOTS[0], BT_DOTS[1]])]),
    ("brightness-high", &[lit(&[SUN, RAYS[0], RAYS[1], RAYS[2], RAYS[3], RAYS[4], RAYS[5], RAYS[6], RAYS[7]])]),
    ("brightness-low", &[lit(&[SUN]), dim(&RAYS)]),
    ("brightness-medium", &[lit(&[SUN, RAYS[0], RAYS[1], RAYS[2], RAYS[3]]), dim(&[RAYS[4], RAYS[5], RAYS[6], RAYS[7]])]),
    // one band, 2 thick, square outside, widening inward along a 45deg step into flat 4 wide cups
    ("headphones", &[lit(&[P(&[(1, 15), (1, 1), (15, 1), (15, 15), (11, 15), (11, 9), (13, 7), (13, 3), (3, 3), (3, 7), (5, 9), (5, 15)])])]),
    // 14x10, like the upright objects turned: three 2x2 keys over a 6 wide space bar, 2 from each other and the frame
    ("keyboard", &[lit(&[
        R(1, 3, 14, 10),
        H(3, 5, 2, 2),
        H(7, 5, 2, 2),
        H(11, 5, 2, 2),
        H(5, 9, 6, 2),
    ])]),
    // 12x14: a centered 2x4 keyhole slot, 2 under the shackle's opening and 2 above the bottom
    ("lock", &[lit(&[R(4, 1, 8, 2), R(4, 1, 2, 6), R(10, 1, 2, 6), R(2, 7, 12, 8), H(7, 9, 2, 4)])]),
    ("mic", &[lit(&MIC)]),
    ("mic-off", &[lit(&[MIC[0], MIC[1], MIC[2], MIC[3], MIC[4], MIC[5], SLASH_GAP]), SLASH]),
    ("mouse", &[lit(&[R(3, 1, 10, 14), H(7, 1, 2, 5), H(3, 6, 10, 2)])]),
    ("pause", &[lit(&[R(3, 1, 4, 14), R(9, 1, 4, 14)])]),
    // a 10x14 slab, as wide as the other upright objects, with a 4x2 earpiece cut out
    ("phone", &[lit(&[R(3, 1, 10, 14), H(6, 3, 4, 2)])]),
    ("play", &[lit(&[P(&[(4, 1), (14, 8), (4, 15)])])]),
    ("refresh", &[lit(&REFRESH)]),
    // a 10x14 cabinet, walls 2: a 2x2 tweeter over a 6x6 woofer ring (2 thick, around a 2x2 cone), 2 apart
    ("speaker", &[lit(&[R(3, 1, 10, 14), H(7, 3, 2, 2), H(5, 7, 6, 6)]), lit(&[R(7, 9, 2, 2)])]),
    ("volume-high", &[lit(&[SPEAKER, WAVE1, WAVE2])]),
    ("volume-low", &[lit(&[SPEAKER]), dim(&[WAVE1, WAVE2])]),
    ("volume-medium", &[lit(&[SPEAKER, WAVE1]), dim(&[WAVE2])]),
    ("volume-muted", &[lit(&[SPEAKER, MUTE_X[0], MUTE_X[1]])]),
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

fn pts(p: &[(i8, i8)], unit: f32) -> Vec<Point> {
    p.iter().map(|&(x, y)| Point::from_xy(x as f32 * unit, y as f32 * unit)).collect()
}

fn ptsf(p: &[(f32, f32)]) -> Vec<Point> {
    p.iter().map(|&(x, y)| Point::from_xy(x, y)).collect()
}

fn rect(x: i8, y: i8, w: i8, h: i8) -> Vec<Point> {
    pts(&[(x, y), (x + w, y), (x + w, y + h), (x, y + h)], 1.0)
}

/// A fan band's outer edge and inner edge (a single point for a wedge), each from end to end.
fn fan_edges(f: &Fan) -> (Vec<Point>, Vec<Point>) {
    let edge = |r: f32| -> Vec<Point> {
        if r == 0.0 {
            return vec![Point::from_xy(0.0, 0.0)];
        }
        // the ends are the 45deg faces' midpoints, on the rays; up is -y
        let (e, c) = (r * S + f.ext * S, -r * S + f.ext * S);
        vec![Point::from_xy(-e, c), Point::from_xy(-r * T, -r), Point::from_xy(r * T, -r), Point::from_xy(e, c)]
    };
    let place = |p: Point| if f.right { Point::from_xy(f.apex.0 - p.y, f.apex.1 + p.x) } else { Point::from_xy(f.apex.0 + p.x, f.apex.1 + p.y) };
    let place_all = |v: Vec<Point>| v.into_iter().map(place).collect();
    (place_all(edge(f.r1)), place_all(edge(f.r0)))
}

fn part(p: &Part) -> (Contours, bool) {
    let (mut cs, cut) = match *p {
        R(x, y, w, h) => (vec![rect(x, y, w, h)], false),
        H(x, y, w, h) => (vec![rect(x, y, w, h)], true),
        P(p) => (vec![pts(p, 1.0)], false),
        Pf(p) => (vec![ptsf(p)], false),
        Qf(p) => (vec![ptsf(p)], true),
        Lf(p) => (stroke(ptsf(p)), false),
        B(f) => {
            let (outer, mut inner) = fan_edges(&f);
            inner.reverse();
            (vec![outer.into_iter().chain(inner).collect()], false)
        }
        D(x, y) => (vec![ptsf(&[(x, y - R2), (x + R2, y), (x, y + R2), (x - R2, y)])], false),
    };
    // y points down: a positive shoelace area is clockwise on screen
    let area: f32 = cs.iter().map(|c| c.iter().zip(c.iter().cycle().skip(1)).map(|(a, b)| a.x * b.y - b.x * a.y).sum::<f32>()).sum();
    if area < 0.0 {
        cs.iter_mut().for_each(|c| c.reverse());
    }
    (cs, cut)
}

fn stroke(p: Vec<Point>) -> Contours {
    let mut pb = PathBuilder::new();
    for (i, q) in p.into_iter().enumerate() {
        if i == 0 { pb.move_to(q.x, q.y) } else { pb.line_to(q.x, q.y) }
    }
    // only straight strokes are left (slash, muted X): the join never applies
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


    fn seg_dist(p: Point, a: Point, b: Point) -> f32 {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        (p.x - a.x - t * dx).hypot(p.y - a.y - t * dy)
    }

    /// Shortest distance between two sets of polylines (closed: each contour's last point joins its first).
    fn dist(a: &[Vec<Point>], b: &[Vec<Point>], closed: bool) -> f32 {
        let one_way = |a: &[Vec<Point>], b: &[Vec<Point>]| {
            let mut d = f32::MAX;
            for c in b {
                let n = if closed { c.len() } else { c.len().saturating_sub(1) };
                for i in 0..n {
                    let (s, e) = (c[i], c[(i + 1) % c.len()]);
                    for &p in a.iter().flatten() {
                        d = d.min(seg_dist(p, s, e));
                    }
                }
            }
            d
        };
        one_way(a, b).min(one_way(b, a))
    }

    /// Width of a convex contour: its narrowest extent across one of its edges.
    fn width(c: &[Point]) -> f32 {
        (0..c.len())
            .map(|i| {
                let (a, b) = (c[i], c[(i + 1) % c.len()]);
                let len = (b.x - a.x).hypot(b.y - a.y);
                c.iter().map(|p| ((b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x)).abs() / len).fold(0.0, f32::max)
            })
            .fold(f32::MAX, f32::min)
    }

    fn close(what: &str, got: f32) {
        assert!((got - 2.0).abs() < 1e-4, "{what}: {got}, not 2");
    }

    /// Every band, ray and dot exactly 2 thick, across 45deg edges too, and every neighbor exactly 2 apart.
    #[test]
    fn exact_widths_and_gaps() {
        let c = |p: &Part| part(p).0;
        for (name, p) in [("BAND1", BAND1), ("BAND2", BAND2), ("WAVE1", WAVE1), ("WAVE2", WAVE2), ("WAVE2_OVER", WAVE2_OVER)] {
            let B(f) = p else { unreachable!() };
            let (outer, inner) = fan_edges(&f);
            close(name, dist(&[outer], &[inner], false));
        }
        for (i, p) in RAYS.iter().chain(&BT_DOTS).chain(&RUNE[2..]).enumerate() {
            close(&format!("ray/dot/tail {i}"), width(&c(p)[0]));
        }
        let gaps = [
            ("DOT-BAND1", DOT, BAND1),
            ("BAND1-BAND2", BAND1, BAND2),
            ("SPEAKER-WAVE1", SPEAKER, WAVE1),
            ("WAVE1-WAVE2", WAVE1, WAVE2),
            ("WAVE1-WAVE2_OVER", WAVE1, WAVE2_OVER),
            ("SPEAKER-X", SPEAKER, MUTE_X[0]),
            ("SPEAKER-X", SPEAKER, MUTE_X[1]),
            ("refresh halves", REFRESH[0], REFRESH[1]),
        ];
        for (name, a, b) in gaps {
            close(name, dist(&c(&a), &c(&b), true));
        }
        for (i, r) in RAYS.iter().enumerate() {
            let d = dist(&c(&SUN), &c(r), true);
            close(&format!("SUN-ray {i}"), d);
        }
        // each diagonal ray exactly 2 from the straight rays either side
        for (d, s) in [(4, 0), (4, 2), (5, 0), (5, 3), (6, 1), (6, 2), (7, 1), (7, 3)] {
            close(&format!("ray {d}-ray {s}"), dist(&c(&RAYS[d]), &c(&RAYS[s]), true));
        }
        close("SLASH-SLASH_GAP", dist(&c(&SLASH.parts[0]), &c(&SLASH_GAP), true));
        let rune: Contours = RUNE.iter().flat_map(c).collect();
        for (i, dot) in BT_DOTS.iter().enumerate() {
            close(&format!("RUNE-dot {i}"), dist(&rune, &c(dot), true));
        }
    }

    /// 4-connected components of `mask` (n x n), as pixel indices.
    fn components(mask: &[bool], n: usize) -> Vec<Vec<usize>> {
        let mut seen = vec![false; mask.len()];
        let mut out = Vec::new();
        for start in 0..mask.len() {
            if !mask[start] || seen[start] {
                continue;
            }
            seen[start] = true;
            let (mut comp, mut stack) = (Vec::new(), vec![start]);
            while let Some(i) = stack.pop() {
                comp.push(i);
                let (x, y) = (i % n, i / n);
                let next = [(x > 0).then(|| i - 1), (x + 1 < n).then(|| i + 1), (y > 0).then(|| i - n), (y + 1 < n).then(|| i + n)];
                for j in next.into_iter().flatten() {
                    if mask[j] && !seen[j] {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
            out.push(comp);
        }
        out
    }

    /// Euclidean distance (pixels) from every pixel to the nearest `target` one (Felzenszwalb and Huttenlocher).
    fn edt(target: &[bool], n: usize) -> Vec<f32> {
        const INF: f32 = 1e12;
        let pass = |f: &[f32]| -> Vec<f32> {
            let (mut v, mut z, mut d) = (vec![0usize; n], vec![0f32; n + 1], vec![0f32; n]);
            let mut k = 0;
            z[0] = -INF;
            z[1] = INF;
            for q in 1..n {
                loop {
                    let p = v[k];
                    let s = ((f[q] + (q * q) as f32) - (f[p] + (p * p) as f32)) / (2.0 * (q as f32 - p as f32));
                    if s <= z[k] && k > 0 {
                        k -= 1;
                        continue;
                    }
                    if s <= z[k] {
                        v[0] = q;
                        z[1] = INF;
                        break;
                    }
                    k += 1;
                    v[k] = q;
                    z[k] = s;
                    z[k + 1] = INF;
                    break;
                }
            }
            k = 0;
            for (q, out) in d.iter_mut().enumerate() {
                while z[k + 1] < q as f32 {
                    k += 1;
                }
                let p = v[k];
                *out = (q as f32 - p as f32).powi(2) + f[p];
            }
            d
        };
        let mut g: Vec<f32> = target.iter().map(|&t| if t { 0.0 } else { INF }).collect();
        for x in 0..n {
            let col: Vec<f32> = (0..n).map(|y| g[y * n + x]).collect();
            for (y, v) in pass(&col).into_iter().enumerate() {
                g[y * n + x] = v;
            }
        }
        for y in 0..n {
            let row = pass(&g[y * n..(y + 1) * n]);
            g[y * n..(y + 1) * n].copy_from_slice(&row);
        }
        g.into_iter().map(f32::sqrt).collect()
    }

    /// Every icon rendered at 16px per unit: no part (or what a cut leaves of one) and no hole is
    /// narrower than 2, and no two parts are closer than 2, within a pixel of anti-aliasing.
    #[test]
    fn no_slivers_or_tight_gaps() {
        const K: usize = 16;
        const N: usize = 16 * K;
        const TOL: f32 = 1.5;
        let mut bad = Vec::new();
        for (name, _) in ICONS.iter() {
            let mut pm = Pixmap::new(N as u32, N as u32).unwrap();
            draw(&mut pm.as_mut(), name, N as f32, 0.0, 0.0, Color::BLACK, Transform::identity());
            // half covered, dim parts included
            let inside: Vec<bool> = pm.pixels().iter().map(|p| p.alpha() as f32 >= 255.0 * DIM / 2.0).collect();
            let parts = components(&inside, N);
            let outside: Vec<bool> = inside.iter().map(|b| !b).collect();
            let edge = |i: &usize| i.is_multiple_of(N) || i / N == 0 || i % N == N - 1 || i / N == N - 1;
            let holes: Vec<Vec<usize>> = components(&outside, N).into_iter().filter(|c| !c.iter().any(edge)).collect();
            for (what, comps) in [("part", &parts), ("hole", &holes)] {
                for c in comps.iter() {
                    let mut target = vec![true; N * N];
                    c.iter().for_each(|&i| target[i] = false);
                    let d = edt(&target, N);
                    let r = c.iter().map(|&i| d[i]).fold(0.0, f32::max);
                    // r is a radius plus half a pixel
                    if r + TOL < K as f32 {
                        let (x, y) = (c.iter().map(|i| i % N).min().unwrap() / K, c.iter().map(|i| i / N).min().unwrap() / K);
                        bad.push(format!("{name}: a {what} at ({x}, {y}) only {:.2} wide", 2.0 * r / K as f32));
                    }
                }
            }
            for (i, a) in parts.iter().enumerate() {
                let mut target = vec![false; N * N];
                a.iter().for_each(|&j| target[j] = true);
                let d = edt(&target, N);
                for b in &parts[i + 1..] {
                    // centers of the facing pixels: the gap plus one pixel
                    let m = b.iter().map(|&j| d[j]).fold(f32::MAX, f32::min) - 1.0;
                    if m + TOL < 2.0 * K as f32 {
                        bad.push(format!("{name}: two parts {:.2} apart", m / K as f32));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// Every icon rendered at 16px per unit, as a mask of its half covered pixels (dim parts included).
    fn mask(name: &str, n: usize) -> Vec<bool> {
        let mut pm = Pixmap::new(n as u32, n as u32).unwrap();
        draw(&mut pm.as_mut(), name, n as f32, 0.0, 0.0, Color::BLACK, Transform::identity());
        pm.pixels().iter().map(|p| p.alpha() as f32 >= 255.0 * DIM / 2.0).collect()
    }

    /// Every part's nearest neighbor is exactly 2 away, and so is every hole's (another hole, or the
    /// outside, across the wall between them), within a pixel of anti-aliasing.
    #[test]
    fn nearest_gaps_are_exactly_2() {
        const K: usize = 16;
        const N: usize = 16 * K;
        const TOL: f32 = 1.5;
        let mut bad = Vec::new();
        for (name, _) in ICONS.iter() {
            let inside = mask(name, N);
            let outside: Vec<bool> = inside.iter().map(|b| !b).collect();
            let parts = components(&inside, N);
            let edge = |i: &usize| i.is_multiple_of(N) || i / N == 0 || i % N == N - 1 || i / N == N - 1;
            let holes: Vec<Vec<usize>> = components(&outside, N).into_iter().filter(|c| !c.iter().any(edge)).collect();
            // a part's neighbors are the other parts; a hole's are all the other outside pixels
            let groups: [(&str, &Vec<Vec<usize>>, &Vec<bool>); 2] = [("part", &parts, &inside), ("hole", &holes, &outside)];
            for (what, comps, pool) in groups {
                if what == "part" && comps.len() < 2 {
                    continue;
                }
                for c in comps.iter() {
                    let mut target = pool.clone();
                    c.iter().for_each(|&i| target[i] = false);
                    let d = edt(&target, N);
                    // centers of the facing pixels: the gap plus one pixel
                    let m = (c.iter().map(|&i| d[i]).fold(f32::MAX, f32::min) - 1.0) / K as f32;
                    if (m - 2.0).abs() > TOL / K as f32 {
                        let (x, y) = (c.iter().map(|i| i % N).min().unwrap() / K, c.iter().map(|i| i / N).min().unwrap() / K);
                        bad.push(format!("{name}: the {what} at ({x}, {y}) is {m:.2} from its nearest neighbor"));
                    }
                }
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// Ink bounds per icon, as the design rules size them: inside the 14x14 box (the Wi-Fi and volume fans
    /// and the Bluetooth dots may use the full 16), upright objects exactly 10x14, the keyboard 14x10. The
    /// table (with ink coverage, in square units): `cargo test sizes -- --nocapture`.
    #[test]
    fn sizes() {
        const UPRIGHT: [&str; 6] = ["battery-full", "mouse", "pause", "phone", "play", "speaker"];
        let mut bad = Vec::new();
        const K: usize = 16;
        const N: usize = 16 * K;
        for (name, _) in ICONS.iter() {
            let m = mask(name, N);
            let on: Vec<usize> = (0..N * N).filter(|&i| m[i]).collect();
            let (x0, x1) = (on.iter().map(|i| i % N).min().unwrap(), on.iter().map(|i| i % N).max().unwrap() + 1);
            let (y0, y1) = (on.iter().map(|i| i / N).min().unwrap(), on.iter().map(|i| i / N).max().unwrap() + 1);
            let u = |p: usize| p as f32 / K as f32;
            let cover = on.len() as f32 / (K * K) as f32;
            println!("{name:18} x {:5.2}..{:5.2}  y {:5.2}..{:5.2}  {:5.2}x{:5.2}  ink {cover:6.2}", u(x0), u(x1), u(y0), u(y1), u(x1 - x0), u(y1 - y0));
            // within a pixel of anti-aliasing
            let (w, h, near) = (u(x1 - x0), u(y1 - y0), |a: f32, b: f32| (a - b).abs() <= 1.0 / K as f32);
            let fan = name.starts_with("wifi") || name.starts_with("volume") || *name == "bluetooth-on";
            let inside = fan || (u(x0) >= 1.0 - 1.0 / K as f32 && u(y0) >= 1.0 - 1.0 / K as f32 && u(x1) <= 15.0 + 1.0 / K as f32 && u(y1) <= 15.0 + 1.0 / K as f32);
            let sized = if UPRIGHT.contains(name) { near(w, 10.0) && near(h, 14.0) } else if *name == "keyboard" { near(w, 14.0) && near(h, 10.0) } else { true };
            if !inside || !sized {
                bad.push(format!("{name}: {w:.2}x{h:.2} at ({:.2}, {:.2})", u(x0), u(y0)));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// Every icon at 64px (32 at scale 2, the largest flurry draws) over its name, in Nord colors:
    /// `FLURRY_ICON_SHEET=sheet.png cargo test sheet -- --ignored`.
    #[test]
    #[ignore = "writes a PNG to $FLURRY_ICON_SHEET"]
    fn sheet() {
        use crate::lib::style::IOSEVKA;
        use crate::ui::text::{self, Font, Wrap};
        let out = std::env::var("FLURRY_ICON_SHEET").expect("FLURRY_ICON_SHEET");
        const SIZE: u32 = 64;
        const COLS: u32 = 6;
        const CELL_W: u32 = 192;
        const CELL_H: u32 = 136;
        const GAP: u32 = 16;
        // Polar Night background and cells, Frost icons, Snow Storm names
        let bg = Color::from_rgba8(0x2e, 0x34, 0x40, 0xff);
        let cell = Color::from_rgba8(0x3b, 0x42, 0x52, 0xff);
        let ink = Color::from_rgba8(0x88, 0xc0, 0xd0, 0xff);
        let label = Color::from_rgba8(0xd8, 0xde, 0xe9, 0xff);
        let rows = ICON_COUNT.div_ceil(COLS as usize) as u32;
        let mut sheet = Pixmap::new(GAP + COLS * (CELL_W + GAP), GAP + rows * (CELL_H + GAP)).unwrap();
        sheet.fill(bg);
        let font = Font { family: IOSEVKA, weight: 400, size: 16.0 };
        let mut paint = Paint::default();
        paint.set_color(cell);
        for (i, (name, _)) in ICONS.iter().enumerate() {
            let (x, y) = (GAP + (i as u32 % COLS) * (CELL_W + GAP), GAP + (i as u32 / COLS) * (CELL_H + GAP));
            let r = tiny_skia::Rect::from_xywh(x as f32, y as f32, CELL_W as f32, CELL_H as f32).unwrap();
            sheet.fill_rect(r, &paint, Transform::identity(), None);
            draw(&mut sheet.as_mut(), name, SIZE as f32, (x + (CELL_W - SIZE) / 2) as f32, (y + GAP) as f32, ink, Transform::identity());
            let l = text::layout(name, &font, None, Wrap::None, false);
            let lx = x as f32 + ((CELL_W as f32 - l.width()) / 2.0).round();
            text::draw(&mut sheet.as_mut(), &l, lx, (y + GAP + SIZE + GAP) as f32, label, Transform::identity());
        }
        let file = std::io::BufWriter::new(std::fs::File::create(&out).unwrap());
        let mut enc = png::Encoder::new(file, sheet.width(), sheet.height());
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(sheet.data()).unwrap();
    }
}
