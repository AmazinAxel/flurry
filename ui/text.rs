// Text: fontconfig picks the font files at build time (settings.rs FONTS), HarfBuzz shapes and
// hands out glyph outlines, which tiny-skia fills. Unhinted, grayscale, fractional positioning: the same output GTK4
// gives at scale 2.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int, c_uint, c_void};
use std::rc::Rc;

use tiny_skia::{Path, PathBuilder};

#[allow(non_camel_case_types)]
mod ffi {
    use super::*;
    pub type hb_blob = c_void;
    pub type hb_face = c_void;
    pub type hb_font = c_void;
    pub type hb_buffer = c_void;
    pub type hb_draw_funcs = c_void;

    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct hb_glyph_info {
        pub codepoint: u32,
        pub mask: u32,
        pub cluster: u32,
        pub var1: u32,
        pub var2: u32,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct hb_glyph_position {
        pub x_advance: i32,
        pub y_advance: i32,
        pub x_offset: i32,
        pub y_offset: i32,
        pub var: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct hb_font_extents {
        pub ascender: i32,
        pub descender: i32,
        pub line_gap: i32,
        pub reserved: [i32; 9],
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct hb_glyph_extents {
        pub x_bearing: i32,
        pub y_bearing: i32,
        pub width: i32,
        pub height: i32,
    }
    #[repr(C)]
    pub struct hb_variation {
        pub tag: u32,
        pub value: f32,
    }

    pub type MoveFn = extern "C" fn(*mut hb_draw_funcs, *mut c_void, *mut c_void, f32, f32, *mut c_void);
    pub type QuadFn = extern "C" fn(*mut hb_draw_funcs, *mut c_void, *mut c_void, f32, f32, f32, f32, *mut c_void);
    pub type CubicFn =
        extern "C" fn(*mut hb_draw_funcs, *mut c_void, *mut c_void, f32, f32, f32, f32, f32, f32, *mut c_void);
    pub type CloseFn = extern "C" fn(*mut hb_draw_funcs, *mut c_void, *mut c_void, *mut c_void);

    pub const HB_DIRECTION_LTR: c_uint = 4;

    #[link(name = "harfbuzz")]
    unsafe extern "C" {
        pub fn hb_blob_create_from_file(path: *const c_char) -> *mut hb_blob;
        pub fn hb_blob_get_data(blob: *mut hb_blob, len: *mut c_uint) -> *const c_char;
        pub fn hb_blob_destroy(blob: *mut hb_blob);
        pub fn hb_face_create(blob: *mut hb_blob, index: c_uint) -> *mut hb_face;
        pub fn hb_face_get_upem(face: *mut hb_face) -> c_uint;
        pub fn hb_face_get_glyph_count(face: *mut hb_face) -> c_uint;
        pub fn hb_face_destroy(face: *mut hb_face);
        pub fn hb_font_create(face: *mut hb_face) -> *mut hb_font;
        pub fn hb_font_set_scale(font: *mut hb_font, x: c_int, y: c_int);
        pub fn hb_font_set_variations(font: *mut hb_font, vars: *const hb_variation, n: c_uint);
        pub fn hb_ot_var_has_data(face: *mut hb_face) -> c_int;
        pub fn hb_font_get_h_extents(font: *mut hb_font, ext: *mut hb_font_extents) -> c_int;
        pub fn hb_font_get_nominal_glyph(font: *mut hb_font, cp: u32, glyph: *mut u32) -> c_int;
        pub fn hb_font_get_glyph_extents(font: *mut hb_font, glyph: u32, ext: *mut hb_glyph_extents) -> c_int;
        pub fn hb_font_draw_glyph(font: *mut hb_font, glyph: u32, funcs: *mut hb_draw_funcs, data: *mut c_void);
        pub fn hb_buffer_create() -> *mut hb_buffer;
        pub fn hb_buffer_destroy(buf: *mut hb_buffer);
        pub fn hb_buffer_add_utf8(buf: *mut hb_buffer, text: *const c_char, len: c_int, off: c_uint, n: c_int);
        pub fn hb_buffer_set_direction(buf: *mut hb_buffer, dir: c_uint);
        pub fn hb_buffer_guess_segment_properties(buf: *mut hb_buffer);
        pub fn hb_buffer_get_glyph_infos(buf: *mut hb_buffer, len: *mut c_uint) -> *mut hb_glyph_info;
        pub fn hb_buffer_get_glyph_positions(buf: *mut hb_buffer, len: *mut c_uint) -> *mut hb_glyph_position;
        pub fn hb_shape(font: *mut hb_font, buf: *mut hb_buffer, features: *const c_void, n: c_uint);
        pub fn hb_draw_funcs_create() -> *mut hb_draw_funcs;
        pub fn hb_draw_funcs_set_move_to_func(f: *mut hb_draw_funcs, cb: MoveFn, u: *mut c_void, d: *mut c_void);
        pub fn hb_draw_funcs_set_line_to_func(f: *mut hb_draw_funcs, cb: MoveFn, u: *mut c_void, d: *mut c_void);
        pub fn hb_draw_funcs_set_quadratic_to_func(f: *mut hb_draw_funcs, cb: QuadFn, u: *mut c_void, d: *mut c_void);
        pub fn hb_draw_funcs_set_cubic_to_func(f: *mut hb_draw_funcs, cb: CubicFn, u: *mut c_void, d: *mut c_void);
        pub fn hb_draw_funcs_set_close_path_func(f: *mut hb_draw_funcs, cb: CloseFn, u: *mut c_void, d: *mut c_void);
        pub fn hb_ot_color_has_png(face: *mut hb_face) -> c_int;
        pub fn hb_ot_color_glyph_reference_png(font: *mut hb_font, glyph: u32) -> *mut hb_blob;
    }
}
use ffi::*;

struct Face {
    font: *mut hb_font,
    upem: f32,
    png: bool,
    /// has a wght axis (or others): opened once more per weight
    variable: bool,
    /// bounded, dropped by trim_caches
    outlines: RefCell<HashMap<u32, Option<Rc<Path>>>>,
}

impl Face {
    /// `wght` > 0 sets that axis on a variable face. fontconfig's named-instance bits of `index` are
    /// dropped: it never reports the variations it matched (Sora's Regular for medium), so the
    /// weight is set here instead.
    fn open(path: &str, index: u32, wght: f32) -> Option<Face> {
        let c = CString::new(path).ok()?;
        unsafe {
            let blob = hb_blob_create_from_file(c.as_ptr());
            let face = hb_face_create(blob, index & 0xFFFF);
            hb_blob_destroy(blob);
            // an unreadable file still makes an (empty) face
            if hb_face_get_glyph_count(face) == 0 {
                hb_face_destroy(face);
                return None;
            }
            let upem = hb_face_get_upem(face) as f32;
            let png = hb_ot_color_has_png(face) != 0;
            let variable = hb_ot_var_has_data(face) != 0;
            let font = hb_font_create(face);
            hb_face_destroy(face); // the font holds its own reference
            hb_font_set_scale(font, upem as c_int, upem as c_int);
            if variable && wght > 0.0 {
                // clamped to the axis range by HarfBuzz
                hb_font_set_variations(font, &hb_variation { tag: u32::from_be_bytes(*b"wght"), value: wght }, 1);
            }
            Some(Face { font, upem, png, variable, outlines: RefCell::default() })
        }
    }

    fn has(&self, ch: char) -> bool {
        let mut g = 0;
        unsafe { hb_font_get_nominal_glyph(self.font, ch as u32, &mut g) != 0 }
    }

    /// Glyph outline in font units (y up), cached.
    fn outline(&self, glyph: u32) -> Option<Rc<Path>> {
        if let Some(p) = self.outlines.borrow().get(&glyph) {
            return p.clone();
        }
        let path = draw_glyph(self.font, glyph).map(Rc::new);
        let mut m = self.outlines.borrow_mut();
        if m.len() >= 2048 {
            m.clear();
        }
        m.insert(glyph, path.clone());
        path
    }

    /// Embedded color bitmap (CBDT, e.g. Noto Color Emoji) resampled to `scale` device px per font
    /// unit, and its top-left in device px from the pen.
    fn color_png(&self, glyph: u32, scale: f32) -> Option<(tiny_skia::Pixmap, f32, f32)> {
        unsafe {
            let blob = hb_ot_color_glyph_reference_png(self.font, glyph);
            let mut len = 0;
            let data = hb_blob_get_data(blob, &mut len);
            let bytes = if data.is_null() { &[][..] } else { std::slice::from_raw_parts(data.cast::<u8>(), len as usize) };
            let pix = crate::ui::image::decode_png(bytes);
            hb_blob_destroy(blob);
            let pix = pix?;
            let mut ext = hb_glyph_extents::default();
            hb_font_get_glyph_extents(self.font, glyph, &mut ext);
            let (w, h) = (ext.width as f32 * scale, -ext.height as f32 * scale);
            let mut out = tiny_skia::Pixmap::new(w.round().max(1.0) as u32, h.round().max(1.0) as u32)?;
            let t = tiny_skia::Transform::from_scale(out.width() as f32 / pix.width() as f32, out.height() as f32 / pix.height() as f32);
            let pp = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
            out.draw_pixmap(0, 0, pix.as_ref(), &pp, t, None);
            Some((out, ext.x_bearing as f32 * scale, -ext.y_bearing as f32 * scale))
        }
    }

    fn extents(&self) -> (f32, f32) {
        let mut e = hb_font_extents::default();
        unsafe { hb_font_get_h_extents(self.font, &mut e) };
        (e.ascender as f32 / self.upem, -e.descender as f32 / self.upem)
    }
}

extern "C" fn move_to(_: *mut hb_draw_funcs, d: *mut c_void, _: *mut c_void, x: f32, y: f32, _: *mut c_void) {
    unsafe { &mut *d.cast::<PathBuilder>() }.move_to(x, -y);
}
extern "C" fn line_to(_: *mut hb_draw_funcs, d: *mut c_void, _: *mut c_void, x: f32, y: f32, _: *mut c_void) {
    unsafe { &mut *d.cast::<PathBuilder>() }.line_to(x, -y);
}
extern "C" fn quad_to(_: *mut hb_draw_funcs, d: *mut c_void, _: *mut c_void, cx: f32, cy: f32, x: f32, y: f32, _: *mut c_void) {
    unsafe { &mut *d.cast::<PathBuilder>() }.quad_to(cx, -cy, x, -y);
}
#[allow(clippy::too_many_arguments)]
extern "C" fn cubic_to(
    _: *mut hb_draw_funcs, d: *mut c_void, _: *mut c_void, ax: f32, ay: f32, bx: f32, by: f32, x: f32, y: f32,
    _: *mut c_void,
) {
    unsafe { &mut *d.cast::<PathBuilder>() }.cubic_to(ax, -ay, bx, -by, x, -y);
}
extern "C" fn close(_: *mut hb_draw_funcs, d: *mut c_void, _: *mut c_void, _: *mut c_void) {
    unsafe { &mut *d.cast::<PathBuilder>() }.close();
}

thread_local! {
    static DRAW: *mut hb_draw_funcs = unsafe {
        let f = hb_draw_funcs_create();
        let n = std::ptr::null_mut();
        hb_draw_funcs_set_move_to_func(f, move_to, n, n);
        hb_draw_funcs_set_line_to_func(f, line_to, n, n);
        hb_draw_funcs_set_quadratic_to_func(f, quad_to, n, n);
        hb_draw_funcs_set_cubic_to_func(f, cubic_to, n, n);
        hb_draw_funcs_set_close_path_func(f, close, n, n);
        f
    };
}

fn draw_glyph(font: *mut hb_font, glyph: u32) -> Option<Path> {
    let mut pb = PathBuilder::new();
    DRAW.with(|f| unsafe { hb_font_draw_glyph(font, glyph, *f, (&raw mut pb).cast()) });
    pb.finish()
}

/// A family resolved to its primary face plus fontconfig's fallback chain.
struct Family {
    /// and embolden (fontconfig 90-synthetic.conf: bold requested from a face no heavier than medium)
    faces: Vec<(Rc<Face>, bool)>,
}

thread_local! {
    static FAMILIES: RefCell<HashMap<(&'static str, i32), Rc<Family>>> = RefCell::default();
    /// (file, index, wght bits; 0 unvaried)
    static FILES: RefCell<HashMap<(&'static str, u32, u32), Option<Rc<Face>>>> = RefCell::default();
}

/// A file's face, at `wght` if it is variable.
fn open_face(file: &'static str, index: u32, wght: f32) -> Option<Rc<Face>> {
    let get = |w: f32| FILES.with(|m| m.borrow_mut().entry((file, index, w.to_bits())).or_insert_with(|| Face::open(file, index, w).map(Rc::new)).clone());
    let base = get(0.0)?;
    if base.variable { get(wght) } else { Some(base) }
}

/// (family, fontconfig weight, file, index, embolden), in fallback order.
type FontRow = (&'static str, i32, &'static str, u32, bool);

/// The fallback chains fontconfig sorted at build time (package.nix). The checked-in settings.rs has
/// none: dev builds take the same rows, tab-separated, from the dev shell's FLURRY_FONTS.
fn font_rows() -> &'static [FontRow] {
    static DEV: std::sync::OnceLock<Vec<FontRow>> = std::sync::OnceLock::new();
    if !crate::settings::FONTS.is_empty() {
        return crate::settings::FONTS;
    }
    DEV.get_or_init(|| {
        let rows = option_env!("FLURRY_FONTS").unwrap_or_default().lines();
        rows.filter_map(|l| {
            let mut f = l.split('\t');
            Some((f.next()?, f.next()?.parse().ok()?, f.next()?, f.next()?.parse().ok()?, f.next() == Some("True")))
        })
        .collect()
    })
}

/// Resolves `name` at fontconfig `weight` (the nearest one in the table) once and caches it.
fn family(name: &'static str, weight: i32) -> Rc<Family> {
    if let Some(f) = FAMILIES.with(|m| m.borrow().get(&(name, weight)).cloned()) {
        return f;
    }
    let rows = font_rows().iter().filter(|r| r.0 == name);
    let nearest = rows.clone().map(|r| r.1).min_by_key(|w| (w - weight).abs());
    // the CSS weight fc_weight came from
    let wght = match weight { ..=80 => 400.0, 81..=100 => 500.0, 101..=180 => 600.0, _ => 700.0 };
    let rows = rows.filter(|r| Some(r.1) == nearest);
    let faces = rows.filter_map(|&(_, _, file, index, bold)| Some((open_face(file, index, wght)?, bold))).collect();
    let fam = Rc::new(Family { faces });
    FAMILIES.with(|m| m.borrow_mut().insert((name, weight), fam.clone()));
    fam
}

#[derive(Clone, PartialEq, Debug)]
pub struct Font {
    /// fontconfig family name, e.g. "Sora"
    pub family: &'static str,
    /// CSS weight (400 regular, 500 medium, 700 bold)
    pub weight: u16,
    pub size: f32,
}

impl Font {
    /// fontconfig weight
    fn fc_weight(&self) -> i32 {
        match self.weight { 0..=449 => 80, 450..=599 => 100, _ => 200 }
    }
}

#[derive(Clone, Debug)]
pub struct Glyph {
    face: usize,
    id: u32,
    x: f32,
    y: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Line {
    pub glyphs: Vec<Glyph>,
    pub width: f32,
}

/// Shaped and broken text, in px.
pub struct Layout {
    family: Rc<Family>,
    pub size: f32,
    pub lines: Vec<Line>,
    pub ascent: f32,
    pub line_height: f32,
    /// content_hash, computed on first use (0: not yet)
    hash: Cell<u64>,
}

impl Layout {
    /// Identifies the painted glyphs (shadow caches).
    pub fn content_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        if self.hash.get() != 0 {
            return self.hash.get();
        }
        let mut s = std::hash::DefaultHasher::new();
        Rc::as_ptr(&self.family).hash(&mut s);
        self.size.to_bits().hash(&mut s);
        self.line_height.to_bits().hash(&mut s);
        for l in &self.lines {
            for g in &l.glyphs {
                (g.face, g.id, g.x.to_bits(), g.y.to_bits()).hash(&mut s);
            }
            u32::MAX.hash(&mut s);
        }
        let h = s.finish().max(1);
        self.hash.set(h);
        h
    }

    pub fn width(&self) -> f32 {
        self.lines.iter().map(|l| l.width).fold(0.0, f32::max)
    }
    pub fn height(&self) -> f32 {
        self.line_height * self.lines.len().max(1) as f32
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Wrap {
    None,
    /// Break at spaces, overlong words break anywhere (Pango WORD_CHAR; WORD behaves the same at our sizes)
    Word,
}

/// face, glyph, advance, x_off, y_off, byte cluster
type Shaped = (usize, u32, f32, f32, f32, usize);

/// Combining marks, ZWJ, variation selectors, skin tones and tags: shaped with what precedes them.
fn joins_previous(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05C7 | 0x064B..=0x065F | 0x0900..=0x0903 | 0x093A..=0x094F
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200D | 0x20D0..=0x20FF | 0xFE00..=0xFE0F | 0xFE20..=0xFE2F
        | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F | 0xE0100..=0xE01EF)
}

/// Shapes one paragraph left to right. Runs split per face (first face that has the char, like
/// Pango's fallback).
fn shape_run(fam: &Family, size: f32, text: &str) -> Vec<Shaped> {
    let mut glyphs = vec![];
    if fam.faces.is_empty() {
        return glyphs;
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    // a char asking for emoji presentation (VS16) takes a color face that has it
    let face_for = |k: usize| {
        let c = chars[k].1;
        let color = chars.get(k + 1).is_some_and(|n| n.1 == '\u{FE0F}');
        let faces = &fam.faces;
        color.then(|| faces.iter().position(|(f, _)| f.png && f.has(c))).flatten().or_else(|| faces.iter().position(|(f, _)| f.has(c))).unwrap_or(0)
    };
    let mut i = 0;
    while i < chars.len() {
        let face = face_for(i);
        let mut j = i + 1;
        while j < chars.len() {
            let c = chars[j].1;
            if !joins_previous(c) && !c.is_whitespace() && face_for(j) != face {
                break;
            }
            j += 1;
        }
        let begin = chars[i].0;
        let end = if j < chars.len() { chars[j].0 } else { text.len() };
        shape_face(&fam.faces[face].0, face, size, &text[begin..end], begin, &mut glyphs);
        i = j;
    }
    glyphs
}

fn shape_face(f: &Face, idx: usize, size: f32, s: &str, base: usize, out: &mut Vec<Shaped>) {
    let scale = size / f.upem;
    unsafe {
        let buf = hb_buffer_create();
        hb_buffer_add_utf8(buf, s.as_ptr().cast(), s.len() as c_int, 0, s.len() as c_int);
        hb_buffer_set_direction(buf, HB_DIRECTION_LTR);
        hb_buffer_guess_segment_properties(buf);
        hb_shape(f.font, buf, std::ptr::null(), 0);
        let (mut n, mut m) = (0, 0);
        let (infos, pos) = (hb_buffer_get_glyph_infos(buf, &mut n), hb_buffer_get_glyph_positions(buf, &mut m));
        if !infos.is_null() && !pos.is_null() && n > 0 {
            let infos = std::slice::from_raw_parts(infos, n.min(m) as usize);
            let pos = std::slice::from_raw_parts(pos, n.min(m) as usize);
            for (info, p) in infos.iter().zip(pos) {
                out.push((
                    idx,
                    info.codepoint,
                    p.x_advance as f32 * scale,
                    p.x_offset as f32 * scale,
                    -p.y_offset as f32 * scale,
                    base + info.cluster as usize,
                ));
            }
        }
        hb_buffer_destroy(buf);
    }
}

thread_local! {
    /// Shaped layouts by a hash of their inputs (the text is kept to rule out collisions): a label is
    /// measured and painted several times per render, and most text is unchanged between renders.
    /// Bounded: when full, entries unused since the previous trim (generation) go.
    static LAYOUTS: RefCell<HashMap<u64, (String, Rc<Layout>, Cell<u32>)>> = RefCell::default();
    static LAYOUT_GEN: Cell<u32> = const { Cell::new(0) };
    static CHAR_WIDTHS: RefCell<HashMap<(&'static str, u16, u32), f32>> = RefCell::default();
}

const LAYOUT_CACHE: usize = 512;

/// Drops every cached layout, outline, glyph mask and color glyph (nothing on screen needs them
/// right now).
pub fn trim_caches() {
    LAYOUTS.with(|c| *c.borrow_mut() = HashMap::new());
    FILES.with(|m| m.borrow().values().flatten().for_each(|f| *f.outlines.borrow_mut() = HashMap::new()));
    GLYPHS.with(|c| *c.borrow_mut() = (HashMap::new(), 0));
    EMOJI.with(|c| *c.borrow_mut() = HashMap::new());
}

/// Lays out `text`; `max_width` triggers wrapping (Wrap::Word) or ellipsizing (`ellipsize`).
pub fn layout(text: &str, font: &Font, max_width: Option<f32>, wrap: Wrap, ellipsize: bool) -> Rc<Layout> {
    layout_spaced(text, font, max_width, wrap, ellipsize, None, None, None)
}

/// Width of `text` on one line, not cached (an entry's caret and selection prefixes would only
/// crowd out the layouts that are reused).
pub fn width_uncached(text: &str, font: &Font) -> f32 {
    layout_uncached(text, font, None, Wrap::None, false).width()
}

/// `layout` with CSS line-height `px` (extra space split above/below), then a Pango line-height
/// `factor` on top; past `max_lines` lines the rest is cut and the last kept line ellipsized.
#[allow(clippy::too_many_arguments)]
pub fn layout_spaced(
    text: &str, font: &Font, max_width: Option<f32>, wrap: Wrap, ellipsize: bool, px: Option<f32>, factor: Option<f32>, max_lines: Option<usize>,
) -> Rc<Layout> {
    let key = crate::ui::node::hash(&(
        text,
        font.family,
        font.weight,
        font.size.to_bits(),
        max_width.map(f32::to_bits),
        wrap == Wrap::Word,
        ellipsize,
        px.map(f32::to_bits),
        factor.map(f32::to_bits),
        max_lines,
    ));
    let generation = LAYOUT_GEN.with(Cell::get);
    let hit = LAYOUTS.with(|c| {
        c.borrow().get(&key).filter(|(t, ..)| t == text).map(|(_, l, g)| {
            g.set(generation);
            l.clone()
        })
    });
    if let Some(l) = hit {
        return l;
    }
    let mut l = layout_uncached(text, font, max_width, wrap, ellipsize);
    if let Some(n) = max_lines
        && l.lines.len() > n
    {
        l.lines.truncate(n.max(1));
        let fam = l.family.clone();
        if let Some(last) = l.lines.last_mut() {
            ellipsize_line(&fam, font, last, max_width.unwrap_or(f32::INFINITY));
        }
    }
    let h = px.unwrap_or(l.line_height) * factor.unwrap_or(1.0);
    l.ascent += (h - l.line_height) / 2.0;
    l.line_height = h;
    let l = Rc::new(l);
    LAYOUTS.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= LAYOUT_CACHE {
            // keep what was used since the last trim; if that is most of it, start over
            c.retain(|_, (.., g)| g.get() == generation);
            if c.len() >= LAYOUT_CACHE * 3 / 4 {
                c.clear();
            }
            LAYOUT_GEN.with(|g| g.set(generation.wrapping_add(1)));
        }
        c.insert(key, (text.to_string(), l.clone(), Cell::new(LAYOUT_GEN.with(Cell::get))));
    });
    l
}

fn layout_uncached(text: &str, font: &Font, max_width: Option<f32>, wrap: Wrap, ellipsize: bool) -> Layout {
    let fam = family(font.family, font.fc_weight());
    let (ascent, descent) = fam.faces.first().map(|(f, _)| f.extents()).unwrap_or((0.8, 0.2));
    let (ascent, descent) = (ascent * font.size, descent * font.size);
    // no tab stops or carriage returns: tabs are spaces, CRs are dropped
    let clean;
    let text = if text.contains(['\r', '\t']) {
        clean = text.replace('\r', "").replace('\t', " ");
        &clean
    } else {
        text
    };
    let mut lines = vec![];
    for para in text.split('\n') {
        let run = shape_run(&fam, font.size, para);
        match (max_width, wrap) {
            (Some(max), Wrap::Word) => wrap_run(para, &run, max, &mut lines),
            (Some(max), Wrap::None) if ellipsize => lines.push(ellipsize_run(&fam, font, &run, max)),
            _ => lines.push(to_line(&run)),
        }
    }
    Layout { family: fam, size: font.size, lines, ascent, line_height: (ascent + descent).ceil(), hash: Cell::new(0) }
}

/// Places `glyphs` after the end of `line`.
fn append(line: &mut Line, glyphs: &[Shaped]) {
    for &(face, id, adv, xo, yo, _) in glyphs {
        line.glyphs.push(Glyph { face, id, x: line.width + xo, y: yo });
        line.width += adv;
    }
}

fn to_line(glyphs: &[Shaped]) -> Line {
    let mut line = Line { glyphs: Vec::with_capacity(glyphs.len()), width: 0.0 };
    append(&mut line, glyphs);
    line
}

fn wrap_run(text: &str, g: &[Shaped], max: f32, lines: &mut Vec<Line>) {
    let mut start = 0;
    while start < g.len() {
        let mut x = 0.0;
        let mut end = start;
        let mut last_break = None;
        while end < g.len() {
            if x + g[end].2 > max && end > start {
                break;
            }
            x += g[end].2;
            // UAX14 break opportunities Pango uses here: after spaces, solidus (SY) and hyphens (BA)
            if text[g[end].5..].starts_with([' ', '/', '-']) {
                last_break = Some(end);
            }
            end += 1;
        }
        if end < g.len()
            && let Some(b) = last_break
        {
            end = b + 1;
        }
        // trailing spaces don't count towards the line width
        let mut visible = end;
        while visible > start && text[g[visible - 1].5..].starts_with(' ') {
            visible -= 1;
        }
        lines.push(to_line(&g[start..visible]));
        // nor do the spaces a break lands before
        start = end;
        while start < g.len() && text[g[start].5..].starts_with(' ') {
            start += 1;
        }
    }
    if g.is_empty() {
        lines.push(Line::default());
    }
}

/// "…" and its width.
fn dots(fam: &Family, font: &Font) -> (Vec<Shaped>, f32) {
    let dots = shape_run(fam, font.size, "…");
    let w = dots.iter().map(|g| g.2).sum();
    (dots, w)
}

fn ellipsize_run(fam: &Family, font: &Font, g: &[Shaped], max: f32) -> Line {
    let full = to_line(g);
    if full.width <= max {
        return full;
    }
    let (dots, dots_w) = dots(fam, font);
    let mut keep = 0;
    let mut x = 0.0;
    while keep < g.len() && x + g[keep].2 + dots_w <= max {
        x += g[keep].2;
        keep += 1;
    }
    // never split a cluster (base + marks, ligatures)
    while keep > 0 && keep < g.len() && g[keep].5 == g[keep - 1].5 {
        keep -= 1;
    }
    let mut line = to_line(&g[..keep]);
    append(&mut line, &dots);
    line
}

/// A cut line ended with an ellipsis, glyphs dropped from its end until it fits in `max`.
fn ellipsize_line(fam: &Family, font: &Font, line: &mut Line, max: f32) {
    let (dots, dots_w) = dots(fam, font);
    while line.width + dots_w > max
        && let Some(g) = line.glyphs.pop()
    {
        line.width = g.x;
    }
    append(line, &dots);
}

thread_local! {
    /// (face, glyph, device px per font unit) -> color bitmap at that size and its offset from the pen
    static EMOJI: RefCell<HashMap<(usize, u32, u32), Option<Rc<(tiny_skia::Pixmap, f32, f32)>>>> = RefCell::default();
}

/// Draws `layout` with its first baseline at (x, y + ascent), in `color`.
pub fn draw(pixmap: &mut tiny_skia::PixmapMut, layout: &Layout, x: f32, y: f32, color: tiny_skia::Color, ts: tiny_skia::Transform) {
    let fam = &layout.family;
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let axis = ts.kx == 0.0 && ts.ky == 0.0;
    let dev_scale = (ts.sx * ts.sx + ts.ky * ts.ky).sqrt();
    for (i, line) in layout.lines.iter().enumerate() {
        let baseline = y + layout.ascent + i as f32 * layout.line_height;
        // lines outside the target (long text) cost nothing; a line's slack covers overhangs
        if axis {
            let top = ts.sy * (baseline - layout.ascent - layout.line_height) + ts.ty;
            let bottom = ts.sy * (baseline - layout.ascent + 2.0 * layout.line_height) + ts.ty;
            if top.max(bottom) < 0.0 || top.min(bottom) > pixmap.height() as f32 {
                continue;
            }
        }
        // nor glyphs left or right of it (a scrolled entry); two em cover any glyph's overhang
        let margin = 2.0 * layout.size * dev_scale;
        for g in &line.glyphs {
            if axis {
                let gx = ts.sx * (x + g.x) + ts.tx;
                if gx < -margin || gx > pixmap.width() as f32 + margin {
                    continue;
                }
            }
            let (face, bold) = &fam.faces[g.face];
            let s = layout.size / face.upem;
            if face.png
                && let Some(cg) = emoji(face, g.id, s * dev_scale)
            {
                let (png, ox, oy) = &*cg;
                if axis && ts.sx == ts.sy && ts.sx > 0.0 {
                    let (px, py) = (ts.sx * (x + g.x) + ts.tx + ox, ts.sy * (baseline + g.y) + ts.ty + oy);
                    crate::ui::node::blit(pixmap, png.as_ref(), px.round() as i32, py.round() as i32, color.alpha());
                } else {
                    let t = ts.pre_translate(x + g.x, baseline + g.y).pre_scale(1.0 / dev_scale, 1.0 / dev_scale).pre_translate(*ox, *oy);
                    let pp = tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bilinear, opacity: color.alpha(), ..Default::default() };
                    pixmap.draw_pixmap(0, 0, png.as_ref(), &pp, t, None);
                }
                continue;
            }
            let t = ts.pre_translate(x + g.x, baseline + g.y).pre_scale(s, s);
            if t.kx == 0.0 && t.ky == 0.0 && t.sx == t.sy && t.sx > 0.0 {
                blit_glyph(pixmap, face, g.id, *bold, t, color);
            } else if let Some(path) = face.outline(g.id) {
                pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, t, None);
                if *bold {
                    // cairo/FreeType synthetic bold: outline grown by em/24
                    let stroke = tiny_skia::Stroke { width: face.upem / 24.0, ..Default::default() };
                    pixmap.stroke_path(&path, &paint, &stroke, t, None);
                }
            }
        }
    }
}

/// A color glyph pre-scaled to the device size it is drawn at, cached (bounded, cleared when full).
fn emoji(face: &Rc<Face>, glyph: u32, scale: f32) -> Option<Rc<(tiny_skia::Pixmap, f32, f32)>> {
    let key = (Rc::as_ptr(face) as usize, glyph, scale.to_bits());
    if let Some(e) = EMOJI.with(|c| c.borrow().get(&key).cloned()) {
        return e;
    }
    let e = face.color_png(glyph, scale).map(Rc::new);
    EMOJI.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= 256 {
            c.clear();
        }
        c.insert(key, e.clone());
    });
    e
}

/// A rasterized glyph's coverage and where its top-left sits relative to the pen's whole pixel.
struct Coverage {
    mask: tiny_skia::Mask,
    x: i32,
    y: i32,
}

type GlyphKey = (usize, u32, u32, bool, u8, u8);

thread_local! {
    /// (face, glyph, device scale, embolden, quarter-pixel phase x/y) -> coverage, and the masks' bytes
    static GLYPHS: RefCell<(HashMap<GlyphKey, Option<Rc<Coverage>>>, usize)> = RefCell::default();
}

/// Mask bytes kept before the glyph cache starts over (a few windows' worth of text).
const GLYPH_BUDGET: usize = 4 << 20;

/// Draws an outline glyph from its cached coverage mask: rasterizing every glyph every frame was
/// most of a text-heavy window's paint time. Positions snap to quarter pixels; the outline is only
/// fetched on a miss.
fn blit_glyph(pm: &mut tiny_skia::PixmapMut, face: &Rc<Face>, glyph: u32, embolden: bool, t: tiny_skia::Transform, color: tiny_skia::Color) {
    // snapped first, then split: a phase of 4/4 is the next pixel's 0, not a separate cache entry
    let (qx, qy) = ((t.tx * 4.0).round(), (t.ty * 4.0).round());
    let (px, py) = ((qx / 4.0).floor(), (qy / 4.0).floor());
    let (fx, fy) = ((qx - px * 4.0) as u8, (qy - py * 4.0) as u8);
    let key = (Rc::as_ptr(face) as usize, glyph, t.sx.to_bits(), embolden, fx, fy);
    let mask = GLYPHS.with(|c| c.borrow().0.get(&key).cloned()).unwrap_or_else(|| {
        let m = face.outline(glyph).and_then(|path| rasterize(face, &path, embolden, t.sx, fx as f32 / 4.0, fy as f32 / 4.0)).map(Rc::new);
        GLYPHS.with(|c| {
            let (map, bytes) = &mut *c.borrow_mut();
            let size = m.as_ref().map_or(0, |m| m.mask.data().len()) + 64;
            if map.len() >= 8192 || *bytes + size > GLYPH_BUDGET {
                map.clear();
                *bytes = 0;
            }
            *bytes += size;
            map.insert(key, m.clone());
        });
        m
    });
    let Some(m) = mask else { return };
    let (mw, mh) = (m.mask.width() as i32, m.mask.height() as i32);
    let alpha = m.mask.data();
    let c = color.premultiply().to_color_u8();
    let (cr, cg, cb, ca) = (c.red() as u32, c.green() as u32, c.blue() as u32, c.alpha() as u32);
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let (ox, oy) = (px as i32 + m.x, py as i32 + m.y);
    let (x0, x1) = (ox.max(0), (ox + mw).min(pw));
    let (y0, y1) = (oy.max(0), (oy + mh).min(ph));
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let data = pm.data_mut();
    for y in y0..y1 {
        let src = &alpha[((y - oy) * mw) as usize..];
        let row = &mut data[(y * pw) as usize * 4..];
        for x in x0..x1 {
            let a = src[(x - ox) as usize] as u32;
            if a == 0 {
                continue;
            }
            // source-over of premultiplied color * coverage
            let d = &mut row[x as usize * 4..x as usize * 4 + 4];
            let sa = ca * a / 255;
            let inv = 255 - sa;
            d[0] = ((cr * a / 255) + (d[0] as u32 * inv + 127) / 255) as u8;
            d[1] = ((cg * a / 255) + (d[1] as u32 * inv + 127) / 255) as u8;
            d[2] = ((cb * a / 255) + (d[2] as u32 * inv + 127) / 255) as u8;
            d[3] = (sa + (d[3] as u32 * inv + 127) / 255) as u8;
        }
    }
}

fn rasterize(face: &Face, path: &Path, embolden: bool, scale: f32, fx: f32, fy: f32) -> Option<Coverage> {
    let pad = if embolden { face.upem / 48.0 } else { 0.0 };
    let b = path.bounds();
    let s = scale; // font units -> device px already folded into `scale` by the caller's transform
    let left = ((b.left() - pad) * s + fx).floor() - 1.0;
    let top = ((b.top() - pad) * s + fy).floor() - 1.0;
    let w = ((b.right() + pad) * s + fx).ceil() + 1.0 - left;
    let h = ((b.bottom() + pad) * s + fy).ceil() + 1.0 - top;
    let mut mask = tiny_skia::Mask::new(w.max(1.0) as u32, h.max(1.0) as u32)?;
    let t = tiny_skia::Transform::from_translate(fx - left, fy - top).pre_scale(s, s);
    mask.fill_path(path, tiny_skia::FillRule::Winding, true, t);
    if embolden && let Some(grown) = path.stroke(&tiny_skia::Stroke { width: face.upem / 24.0, ..Default::default() }, s) {
        mask.fill_path(&grown, tiny_skia::FillRule::Winding, true, t);
    }
    Some(Coverage { mask, x: left as i32, y: top as i32 })
}

/// Average char width, used for GTK's max-width-chars (pango approximate_char_width).
pub fn approx_char_width(font: &Font) -> f32 {
    let key = (font.family, font.weight, font.size.to_bits());
    if let Some(w) = CHAR_WIDTHS.with(|c| c.borrow().get(&key).copied()) {
        return w;
    }
    // pango_font_metrics approximate_char_width: the language sample string over its length
    const SAMPLE: &str = "The quick brown fox jumps over the lazy dog.";
    let chars = layout_uncached(SAMPLE, font, None, Wrap::None, false).width() / SAMPLE.chars().count() as f32;
    // GtkLabel uses MAX (approximate_char_width, approximate_digit_width)
    let digits = layout_uncached("0123456789", font, None, Wrap::None, false).width() / 10.0;
    let w = chars.max(digits);
    CHAR_WIDTHS.with(|c| c.borrow_mut().insert(key, w));
    w
}
