// Text: fontconfig picks the font files (once per family), HarfBuzz shapes and hands out glyph
// outlines, which tiny-skia fills. Unhinted, grayscale, fractional positioning: the same output GTK4
// gives at scale 2.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
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

    pub type MoveFn = extern "C" fn(*mut hb_draw_funcs, *mut c_void, *mut c_void, f32, f32, *mut c_void);
    pub type QuadFn = extern "C" fn(*mut hb_draw_funcs, *mut c_void, *mut c_void, f32, f32, f32, f32, *mut c_void);
    pub type CubicFn =
        extern "C" fn(*mut hb_draw_funcs, *mut c_void, *mut c_void, f32, f32, f32, f32, f32, f32, *mut c_void);
    pub type CloseFn = extern "C" fn(*mut hb_draw_funcs, *mut c_void, *mut c_void, *mut c_void);

    #[link(name = "harfbuzz")]
    unsafe extern "C" {
        pub fn hb_blob_create_from_file(path: *const c_char) -> *mut hb_blob;
        pub fn hb_blob_get_data(blob: *mut hb_blob, len: *mut c_uint) -> *const c_char;
        pub fn hb_blob_destroy(blob: *mut hb_blob);
        pub fn hb_face_create(blob: *mut hb_blob, index: c_uint) -> *mut hb_face;
        pub fn hb_face_get_upem(face: *mut hb_face) -> c_uint;
        pub fn hb_font_create(face: *mut hb_face) -> *mut hb_font;
        pub fn hb_font_set_scale(font: *mut hb_font, x: c_int, y: c_int);
        pub fn hb_font_get_h_extents(font: *mut hb_font, ext: *mut hb_font_extents) -> c_int;
        pub fn hb_font_get_nominal_glyph(font: *mut hb_font, cp: u32, glyph: *mut u32) -> c_int;
        pub fn hb_font_get_glyph_extents(font: *mut hb_font, glyph: u32, ext: *mut hb_glyph_extents) -> c_int;
        pub fn hb_font_draw_glyph(font: *mut hb_font, glyph: u32, funcs: *mut hb_draw_funcs, data: *mut c_void);
        pub fn hb_buffer_create() -> *mut hb_buffer;
        pub fn hb_buffer_destroy(buf: *mut hb_buffer);
        pub fn hb_buffer_add_utf8(buf: *mut hb_buffer, text: *const c_char, len: c_int, off: c_uint, n: c_int);
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

    pub type FcConfig = c_void;
    pub type FcPattern = c_void;
    #[repr(C)]
    pub struct FcFontSet {
        pub nfont: c_int,
        pub sfont: c_int,
        pub fonts: *mut *mut FcPattern,
    }

    #[link(name = "fontconfig")]
    unsafe extern "C" {
        pub fn FcInitLoadConfigAndFonts() -> *mut FcConfig;
        pub fn FcNameParse(name: *const u8) -> *mut FcPattern;
        pub fn FcConfigSubstitute(c: *mut FcConfig, p: *mut FcPattern, kind: c_int) -> c_int;
        pub fn FcDefaultSubstitute(p: *mut FcPattern);
        pub fn FcFontSort(c: *mut FcConfig, p: *mut FcPattern, trim: c_int, csp: *mut c_void, r: *mut c_int)
        -> *mut FcFontSet;
        pub fn FcPatternGetString(p: *mut FcPattern, obj: *const c_char, n: c_int, s: *mut *mut u8) -> c_int;
        pub fn FcPatternGetInteger(p: *mut FcPattern, obj: *const c_char, n: c_int, i: *mut c_int) -> c_int;
        pub fn FcPatternDestroy(p: *mut FcPattern);
        pub fn FcFontSetDestroy(s: *mut FcFontSet);
        pub fn FcConfigDestroy(c: *mut FcConfig);
        pub fn FcFini();
    }
}
use ffi::*;

/// Pango units: font sizes are in px, HarfBuzz runs at 64 subunits per px.
const SUB: f32 = 64.0;
/// How many fallback fonts per family fontconfig returns.
const FALLBACKS: usize = 6;

pub struct Face {
    font: *mut hb_font,
    upem: f32,
    png: bool,
    outlines: RefCell<HashMap<u32, Option<Rc<Path>>>>,
}

impl Face {
    fn open(path: &str, index: u32) -> Option<Face> {
        let c = CString::new(path).ok()?;
        unsafe {
            let blob = hb_blob_create_from_file(c.as_ptr());
            let face = hb_face_create(blob, index);
            hb_blob_destroy(blob);
            let upem = hb_face_get_upem(face) as f32;
            if upem <= 1.0 {
                return None;
            }
            let font = hb_font_create(face);
            hb_font_set_scale(font, upem as c_int, upem as c_int);
            Some(Face { font, upem, png: hb_ot_color_has_png(face) != 0, outlines: RefCell::default() })
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
        self.outlines.borrow_mut().insert(glyph, path.clone());
        path
    }

    /// Embedded color bitmap (CBDT, e.g. Noto Color Emoji) and its extents in font units.
    fn color_png(&self, glyph: u32) -> Option<(tiny_skia::Pixmap, hb_glyph_extents)> {
        if !self.png {
            return None;
        }
        unsafe {
            let blob = hb_ot_color_glyph_reference_png(self.font, glyph);
            let mut len = 0;
            let data = hb_blob_get_data(blob, &mut len);
            let bytes = if data.is_null() { &[][..] } else { std::slice::from_raw_parts(data.cast::<u8>(), len as usize) };
            let pix = crate::ui::image::decode_png(bytes);
            hb_blob_destroy(blob);
            let mut ext = hb_glyph_extents::default();
            hb_font_get_glyph_extents(self.font, glyph, &mut ext);
            Some((pix?, ext))
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
pub struct Family {
    faces: Vec<Rc<Face>>,
    /// fontconfig 90-synthetic.conf: bold requested from a face no heavier than medium
    embolden: Vec<bool>,
}

thread_local! {
    static FAMILIES: RefCell<HashMap<String, Rc<Family>>> = RefCell::default();
    static FILES: RefCell<HashMap<(String, u32), Option<Rc<Face>>>> = RefCell::default();
}

/// Resolves `pattern` (fontconfig syntax, e.g. "Sora:weight=400") once and caches it.
pub fn family(pattern: &str) -> Rc<Family> {
    if let Some(f) = FAMILIES.with(|m| m.borrow().get(pattern).cloned()) {
        return f;
    }
    let requested: i32 = pattern.rsplit("weight=").next().and_then(|w| w.parse().ok()).unwrap_or(80);
    let mut faces = vec![];
    let mut embolden = vec![];
    for (file, index, weight) in fc_sort(pattern) {
        let key = (file, index);
        let face = FILES.with(|m| m.borrow_mut().entry(key.clone()).or_insert_with(|| Face::open(&key.0, key.1).map(Rc::new)).clone());
        if let Some(f) = face {
            faces.push(f);
            embolden.push(requested >= 200 && weight <= 100);
        }
    }
    let fam = Rc::new(Family { faces, embolden });
    FAMILIES.with(|m| m.borrow_mut().insert(pattern.into(), fam.clone()));
    fam
}

fn fc_sort(pattern: &str) -> Vec<(String, u32, i32)> {
    let mut out = vec![];
    let Ok(name) = CString::new(pattern) else { return out };
    unsafe {
        let cfg = FcInitLoadConfigAndFonts();
        let pat = FcNameParse(name.as_ptr().cast());
        FcConfigSubstitute(cfg, pat, 0);
        FcDefaultSubstitute(pat);
        let mut res = 0;
        let set = FcFontSort(cfg, pat, 1, std::ptr::null_mut(), &mut res);
        if !set.is_null() {
            let set_ref = &*set;
            for i in 0..(set_ref.nfont as usize).min(FALLBACKS) {
                let p = *set_ref.fonts.add(i);
                let mut file: *mut u8 = std::ptr::null_mut();
                let mut index = 0;
                if FcPatternGetString(p, c"file".as_ptr(), 0, &mut file) == 0 {
                    FcPatternGetInteger(p, c"index".as_ptr(), 0, &mut index);
                    let mut weight = 80;
                    FcPatternGetInteger(p, c"weight".as_ptr(), 0, &mut weight);
                    out.push((CStr::from_ptr(file.cast()).to_string_lossy().into_owned(), index as u32, weight));
                }
            }
            FcFontSetDestroy(set);
        }
        FcPatternDestroy(pat);
        FcConfigDestroy(cfg);
    }
    out
}

#[derive(Clone, PartialEq, Debug)]
pub struct Font {
    /// fontconfig pattern without size, e.g. "Sora:weight=400"
    pub family: &'static str,
    /// CSS weight (400 regular, 500 medium, 700 bold)
    pub weight: u16,
    pub size: f32,
    pub letter_spacing: f32,
}

impl Font {
    fn pattern(&self) -> String {
        let fc = match self.weight { 0..=449 => 80, 450..=599 => 100, _ => 200 };
        format!("{}:weight={fc}", self.family)
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
#[derive(Clone)]
pub struct Layout {
    family: Option<Rc<Family>>,
    pub size: f32,
    pub lines: Vec<Line>,
    pub ascent: f32,
    pub descent: f32,
    pub line_height: f32,
}

impl Layout {
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

struct Run {
    glyphs: Vec<(usize, u32, f32, f32, f32, usize)>, // face, glyph, advance, x_off, y_off, byte cluster
}

fn shape_run(fam: &Family, size: f32, spacing: f32, text: &str) -> Run {
    // Split into runs per face: first face that has the char, like Pango's fallback.
    let mut glyphs = vec![];
    let mut start = 0;
    let face_for = |c: char| fam.faces.iter().position(|f| f.has(c)).unwrap_or(0);
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let face = face_for(chars[i].1);
        let mut j = i + 1;
        while j < chars.len() && (chars[j].1.is_whitespace() || face_for(chars[j].1) == face) {
            j += 1;
        }
        let begin = chars[i].0;
        let end = if j < chars.len() { chars[j].0 } else { text.len() };
        shape_face(&fam.faces[face], face, size, spacing, &text[begin..end], begin, &mut glyphs);
        start = end;
        i = j;
    }
    let _ = start;
    Run { glyphs }
}

fn shape_face(
    f: &Face, idx: usize, size: f32, spacing: f32, s: &str, base: usize, out: &mut Vec<(usize, u32, f32, f32, f32, usize)>,
) {
    let scale = size / f.upem;
    unsafe {
        let buf = hb_buffer_create();
        hb_buffer_add_utf8(buf, s.as_ptr().cast(), s.len() as c_int, 0, s.len() as c_int);
        hb_buffer_guess_segment_properties(buf);
        hb_shape(f.font, buf, std::ptr::null(), 0);
        let mut n = 0;
        let infos = std::slice::from_raw_parts(hb_buffer_get_glyph_infos(buf, &mut n), n as usize);
        let pos = std::slice::from_raw_parts(hb_buffer_get_glyph_positions(buf, &mut n), n as usize);
        for (info, p) in infos.iter().zip(pos) {
            out.push((
                idx,
                info.codepoint,
                p.x_advance as f32 * scale + spacing,
                p.x_offset as f32 * scale,
                -p.y_offset as f32 * scale,
                base + info.cluster as usize,
            ));
        }
        hb_buffer_destroy(buf);
    }
}

/// Lays out `text`; `max_width` triggers wrapping (Wrap::Word) or ellipsizing (`ellipsize`).
pub fn layout(text: &str, font: &Font, max_width: Option<f32>, wrap: Wrap, ellipsize: bool) -> Layout {
    let fam = family(&font.pattern());
    let (ascent, descent) = fam.faces.first().map(|f| f.extents()).unwrap_or((0.8, 0.2));
    let (ascent, descent) = (ascent * font.size, descent * font.size);
    let mut lines = vec![];
    for para in text.split('\n') {
        let run = shape_run(&fam, font.size, font.letter_spacing, para);
        match (max_width, wrap) {
            (Some(max), Wrap::Word) => wrap_run(para, &run, max, &mut lines),
            (Some(max), Wrap::None) if ellipsize => lines.push(ellipsize_run(&fam, font, &run, max)),
            _ => lines.push(to_line(&run.glyphs)),
        }
    }
    let _ = SUB;
    Layout { family: Some(fam), size: font.size, lines, ascent, descent, line_height: (ascent + descent).ceil() }
}

fn to_line(glyphs: &[(usize, u32, f32, f32, f32, usize)]) -> Line {
    let mut x = 0.0;
    let mut out = Vec::with_capacity(glyphs.len());
    for &(face, id, adv, xo, yo, _) in glyphs {
        out.push(Glyph { face, id, x: x + xo, y: yo });
        x += adv;
    }
    Line { glyphs: out, width: x }
}

fn wrap_run(text: &str, run: &Run, max: f32, lines: &mut Vec<Line>) {
    let g = &run.glyphs;
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
        start = end;
    }
    if g.is_empty() {
        lines.push(Line::default());
    }
}

fn ellipsize_run(fam: &Family, font: &Font, run: &Run, max: f32) -> Line {
    let full = to_line(&run.glyphs);
    if full.width <= max {
        return full;
    }
    let dots = shape_run(fam, font.size, font.letter_spacing, "…");
    let dots_w: f32 = dots.glyphs.iter().map(|g| g.2).sum();
    let mut keep = 0;
    let mut x = 0.0;
    while keep < run.glyphs.len() && x + run.glyphs[keep].2 + dots_w <= max {
        x += run.glyphs[keep].2;
        keep += 1;
    }
    let mut glyphs = run.glyphs[..keep].to_vec();
    glyphs.extend(dots.glyphs);
    to_line(&glyphs)
}

/// Draws `layout` with its first baseline at (x, y + ascent), in `color`.
pub fn draw(pixmap: &mut tiny_skia::PixmapMut, layout: &Layout, x: f32, y: f32, color: tiny_skia::Color, ts: tiny_skia::Transform) {
    let Some(fam) = &layout.family else { return };
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    for (i, line) in layout.lines.iter().enumerate() {
        let baseline = y + layout.ascent + i as f32 * layout.line_height;
        for g in &line.glyphs {
            let face = &fam.faces[g.face];
            let s = layout.size / face.upem;
            if let Some((png, ext)) = face.color_png(g.id) {
                let w = ext.width as f32 * s;
                let h = -ext.height as f32 * s;
                let px = x + g.x + ext.x_bearing as f32 * s;
                let py = baseline + g.y - ext.y_bearing as f32 * s;
                let sx = w / png.width() as f32;
                let sy = h / png.height() as f32;
                let t = ts.pre_translate(px, py).pre_scale(sx, sy);
                let mut pp = tiny_skia::PixmapPaint::default();
                pp.quality = tiny_skia::FilterQuality::Bilinear;
                pp.opacity = color.alpha();
                pixmap.draw_pixmap(0, 0, png.as_ref(), &pp, t, None);
            } else if let Some(path) = face.outline(g.id) {
                let t = ts.pre_translate(x + g.x, baseline + g.y).pre_scale(s, s);
                pixmap.fill_path(&path, &paint, tiny_skia::FillRule::Winding, t, None);
                if fam.embolden[g.face] {
                    // cairo/FreeType synthetic bold: outline grown by em/24
                    let stroke = tiny_skia::Stroke { width: face.upem / 24.0, ..Default::default() };
                    pixmap.stroke_path(&path, &paint, &stroke, t, None);
                }
            }
        }
    }
}

/// Average char width, used for GTK's max-width-chars (pango approximate_char_width).
pub fn approx_char_width(font: &Font) -> f32 {
    // pango_font_metrics approximate_char_width: the language sample string over its length
    const SAMPLE: &str = "The quick brown fox jumps over the lazy dog.";
    let chars = layout(SAMPLE, font, None, Wrap::None, false).width() / SAMPLE.chars().count() as f32;
    // GtkLabel uses MAX (approximate_char_width, approximate_digit_width)
    let digits = layout("0123456789", font, None, Wrap::None, false).width() / 10.0;
    chars.max(digits)
}

/// Releases fontconfig's caches once every family is resolved.
pub fn release_fontconfig() {
    unsafe { FcFini() };
}

