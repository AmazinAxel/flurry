// Widget tree with GTK's box model: margin > border > padding > content, min sizes on the content box,
// halign/valign/expand, boxes with spacing, overlays. Windows rebuild their tree from state on change,
// then lay it out and paint it.

use std::cell::Cell;
use std::rc::Rc;

use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PixmapMut, PixmapPaint, Rect, Transform};

use super::icons;
use super::image::blur;
use super::text::{self, Font, Layout, Wrap};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sides {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Sides {
    pub const fn all(v: f32) -> Self {
        Self { top: v, right: v, bottom: v, left: v }
    }
    pub const fn vh(v: f32, h: f32) -> Self {
        Self { top: v, right: h, bottom: v, left: h }
    }
    pub const fn trbl(top: f32, right: f32, bottom: f32, left: f32) -> Self {
        Self { top, right, bottom, left }
    }
    pub fn h(&self) -> f32 {
        self.left + self.right
    }
    pub fn v(&self) -> f32 {
        self.top + self.bottom
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    pub x: f32,
    pub y: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
    pub inset: bool,
}

pub fn shadow(x: f32, y: f32, blur: f32, color: Color) -> Shadow {
    Shadow { x, y, blur, spread: 0.0, color, inset: false }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Align {
    #[default]
    Fill,
    Start,
    Center,
    End,
}

#[derive(Clone, Debug)]
pub enum Background {
    None,
    Color(Color),
}

#[derive(Clone, Debug)]
pub struct Style {
    pub margin: Sides,
    pub padding: Sides,
    pub border: Sides,
    /// top, right, bottom, left
    pub border_color: [Color; 4],
    pub background: Background,
    pub color: Color,
    pub font: Font,
    pub min_width: f32,
    /// caps the content width (not CSS; GTK has no max-width)
    pub max_width: Option<f32>,
    pub min_height: f32,
    pub opacity: f32,
    pub box_shadow: Vec<Shadow>,
    pub outline: Option<(f32, f32, Color)>, // width, offset, color
    pub text_shadow: Vec<Shadow>,
    pub icon_shadow: Vec<Shadow>,
    pub translate_y: f32,
    /// CSS line-height in px (Pango absolute line height, extra space split above/below)
    pub line_height: Option<f32>,
}

pub const TRANSPARENT: Color = Color::TRANSPARENT;

impl Default for Style {
    fn default() -> Self {
        Self {
            margin: Sides::default(),
            padding: Sides::default(),
            border: Sides::default(),
            border_color: [TRANSPARENT; 4],
            background: Background::None,
            color: Color::WHITE,
            font: Font { family: "Sora", weight: 400, size: 16.0, letter_spacing: 0.0 },
            min_width: 0.0,
            max_width: None,
            min_height: 0.0,
            opacity: 1.0,
            box_shadow: vec![],
            outline: None,
            text_shadow: vec![],
            icon_shadow: vec![],
            translate_y: 0.0,
            line_height: None,
        }
    }
}

impl Style {
    /// Inheritable properties only (color, font, shadows), the way GTK cascades them to children.
    pub fn inherit(&self) -> Style {
        Style {
            color: self.color,
            font: self.font.clone(),
            text_shadow: self.text_shadow.clone(),
            icon_shadow: self.icon_shadow.clone(),
            line_height: self.line_height,
            ..Style::default()
        }
    }
    pub fn border(mut self, w: Sides, c: Color) -> Self {
        self.border = w;
        self.border_color = [c; 4];
        self
    }
}

pub struct LabelSpec {
    pub text: String,
    pub xalign: f32,
    pub wrap: bool,
    /// Pango ellipsize END
    pub ellipsize: bool,
    pub max_width_chars: Option<f32>,
    /// line-height multiplier from markup (`<span line_height>`), applied to all lines but the first
    pub line_height: Option<f32>,
}

pub enum Kind {
    Box { vertical: bool, spacing: f32 },
    /// First child is the main child, the rest are overlays positioned by their own align
    Overlay,
    Label(LabelSpec),
    Icon { name: &'static str, size: f32 },
    /// Pre-rendered pixmap (wallpaper crop, thumbnails), drawn scaled into the content box
    /// `density`: pixmap pixels per logical pixel of its natural size (pre-scaled thumbnails)
    Picture { pixmap: Img, cover: bool, blur: f32, density: f32 },
    /// Arbitrary paint into the content box, with a fixed natural size
    /// `key` identifies what `paint` draws (None: repainted every frame)
    Custom { width: f32, height: f32, key: Option<u64>, paint: Rc<dyn Fn(&mut PixmapMut, Rect, Transform)> },
    /// Clips its child to `max_height` and scrolls by `offset`
    Scroll { max_height: f32, offset: f32 },
}

pub struct Node {
    pub kind: Kind,
    pub style: Style,
    pub halign: Align,
    pub valign: Align,
    pub hexpand: bool,
    pub vexpand: bool,
    pub width_request: f32,
    pub height_request: f32,
    pub visible: bool,
    pub children: Vec<Node>,
    /// Focus/scroll target name
    pub id: Option<String>,
    pub focusable: bool,
    /// GtkBox homogeneous: every visible child gets the same main-axis size
    pub homogeneous: bool,
    // filled in by layout
    pub rect: Rect,
    /// measure_w, and the last measure_h by width: a tree is immutable once built, and every level of
    /// layout re-measures its subtree
    measured_w: Cell<Option<(f32, f32)>>,
    measured_h: Cell<Option<(u32, (f32, f32))>>,
}

impl Node {
    pub fn new(kind: Kind, style: Style) -> Self {
        Self {
            kind,
            style,
            halign: Align::Fill,
            valign: Align::Fill,
            hexpand: false,
            vexpand: false,
            width_request: 0.0,
            height_request: 0.0,
            visible: true,
            children: vec![],
            id: None,
            focusable: false,
            homogeneous: false,
            rect: Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap(),
            measured_w: Cell::new(None),
            measured_h: Cell::new(None),
        }
    }
    pub fn vbox(style: Style, spacing: f32, children: Vec<Node>) -> Self {
        Self { children, ..Self::new(Kind::Box { vertical: true, spacing }, style) }
    }
    pub fn hbox(style: Style, spacing: f32, children: Vec<Node>) -> Self {
        Self { children, ..Self::new(Kind::Box { vertical: false, spacing }, style) }
    }
    pub fn scroll(style: Style, max_height: f32, offset: f32, child: Node) -> Self {
        Self { children: vec![child], ..Self::new(Kind::Scroll { max_height, offset }, style) }
    }
    pub fn overlay(style: Style, children: Vec<Node>) -> Self {
        Self { children, ..Self::new(Kind::Overlay, style) }
    }
    pub fn label(style: Style, text: impl Into<String>) -> Self {
        let spec = LabelSpec {
            text: text.into(),
            xalign: 0.5,
            wrap: false,
            ellipsize: false,
            max_width_chars: None,
            line_height: None,
        };
        Self::new(Kind::Label(spec), style)
    }
    pub fn icon(style: Style, name: &'static str, size: f32) -> Self {
        Self::new(Kind::Icon { name, size }, style)
    }
    pub fn align(mut self, h: Align, v: Align) -> Self {
        self.halign = h;
        self.valign = v;
        self
    }
    pub fn expand(mut self, h: bool, v: bool) -> Self {
        self.hexpand = h;
        self.vexpand = v;
        self
    }
    pub fn size(mut self, w: f32, h: f32) -> Self {
        self.width_request = w;
        self.height_request = h;
        self
    }
    pub fn show(mut self, visible: bool) -> Self {
        self.visible = visible;
        self
    }
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }
    pub fn xalign(mut self, x: f32) -> Self {
        if let Kind::Label(l) = &mut self.kind {
            l.xalign = x;
        }
        self
    }
    pub fn wrap(mut self, max_chars: f32) -> Self {
        if let Kind::Label(l) = &mut self.kind {
            l.wrap = true;
            l.max_width_chars = Some(max_chars);
        }
        self
    }
    pub fn ellipsize(mut self) -> Self {
        if let Kind::Label(l) = &mut self.kind {
            l.ellipsize = true;
            l.max_width_chars = Some(1.0);
        }
        self
    }
    /// Ellipsize END without max-width-chars: natural width is the full text.
    pub fn ellipsize_natural(mut self) -> Self {
        if let Kind::Label(l) = &mut self.kind {
            l.ellipsize = true;
        }
        self
    }
    /// Makes this node a focus target named `id`.
    pub fn focusable(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self.focusable = true;
        self
    }

    fn compute_hexpand(&self) -> bool {
        self.hexpand || self.children.iter().any(|c| c.visible && c.compute_hexpand())
    }
    fn compute_vexpand(&self) -> bool {
        self.vexpand || self.children.iter().any(|c| c.visible && c.compute_vexpand())
    }

    fn frame_h(&self) -> f32 {
        self.style.padding.h() + self.style.border.h()
    }
    fn frame_v(&self) -> f32 {
        self.style.padding.v() + self.style.border.v()
    }

    /// (minimum, natural) border-box width.
    pub fn measure_w(&self) -> (f32, f32) {
        if let Some(m) = self.measured_w.get() {
            return m;
        }
        let m = self.measure_w_uncached();
        self.measured_w.set(Some(m));
        m
    }

    fn measure_w_uncached(&self) -> (f32, f32) {
        let (min, nat) = match &self.kind {
            Kind::Box { vertical, spacing } => {
                let kids = self.children.iter().filter(|c| c.visible).map(|c| c.outer_w());
                if *vertical {
                    kids.fold((0.0f32, 0.0f32), |a, b| (a.0.max(b.0), a.1.max(b.1)))
                } else {
                    let n = self.children.iter().filter(|c| c.visible).count();
                    let gaps = spacing * n.saturating_sub(1) as f32;
                    let (a, b) = kids.fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
                    (a + gaps, b + gaps)
                }
            }
            Kind::Overlay => self.children.first().filter(|c| c.visible).map(|c| c.outer_w()).unwrap_or_default(),
            Kind::Label(l) => {
                let font = &self.style.font;
                let full = text::layout(&l.text, font, None, Wrap::None, false).width();
                match l.max_width_chars {
                    _ if l.text.is_empty() => (0.0, 0.0),
                    Some(chars) if l.ellipsize => {
                        let dots = text::layout("…", font, None, Wrap::None, false).width();
                        let cap = chars * text::approx_char_width(font);
                        (dots, full.min(cap).max(dots))
                    }
                    None if l.ellipsize => {
                        let dots = text::layout("…", font, None, Wrap::None, false).width();
                        (dots.min(full), full)
                    }
                    Some(chars) if l.wrap => {
                        let char_w = text::approx_char_width(font);
                        // GtkLabel: a wrapping label asks for max-width-chars worth of width even for short text;
                        // overlong words break anywhere (Wrap::Word), so its minimum is one char, not the longest
                        // word, which pushed long addresses past a max-width parent
                        let _ = full;
                        (char_w, chars * char_w)
                    }
                    _ => (full, full),
                }
            }
            Kind::Icon { size, .. } => (*size, *size),
            Kind::Picture { pixmap, cover, density, .. } => {
                if *cover {
                    (0.0, pixmap.width() as f32 / density)
                } else {
                    (pixmap.width() as f32 / density, pixmap.width() as f32 / density)
                }
            }
            Kind::Custom { width, .. } => (*width, *width),
            Kind::Scroll { .. } => self.children.first().map(|c| c.outer_w()).unwrap_or_default(),
        };
        let (min, nat) = (min.max(self.style.min_width), nat.max(self.style.min_width));
        let cap = self.style.max_width.unwrap_or(f32::INFINITY);
        let (min, nat) = (min.min(cap), nat.min(cap));
        // GTK4 size requests cover the CSS margin box
        let req = (self.width_request - self.style.margin.h()).max(0.0);
        ((min + self.frame_h()).max(req), (nat + self.frame_h()).max(req))
    }

    /// (minimum, natural) border-box height for a border-box `width`.
    pub fn measure_h(&self, width: f32) -> (f32, f32) {
        if let Some((w, m)) = self.measured_h.get()
            && w == width.to_bits()
        {
            return m;
        }
        let m = self.measure_h_uncached(width);
        self.measured_h.set(Some((width.to_bits(), m)));
        m
    }

    fn measure_h_uncached(&self, width: f32) -> (f32, f32) {
        let inner_w = (width - self.frame_h()).max(0.0);
        let (min, nat) = match &self.kind {
            Kind::Box { vertical, spacing } => {
                let visible: Vec<&Node> = self.children.iter().filter(|c| c.visible).collect();
                if *vertical {
                    let gaps = spacing * visible.len().saturating_sub(1) as f32;
                    let (a, b) = visible.iter().map(|c| c.outer_h(inner_w)).fold((0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
                    (a + gaps, b + gaps)
                } else {
                    let widths = self.distribute(&visible, inner_w, *spacing);
                    visible.iter().zip(widths).map(|(c, w)| c.outer_h(w)).fold((0.0f32, 0.0f32), |a, b| (a.0.max(b.0), a.1.max(b.1)))
                }
            }
            Kind::Overlay => self.children.first().filter(|c| c.visible).map(|c| c.outer_h(inner_w)).unwrap_or_default(),
            Kind::Label(_) => {
                let l = self.label_layout(inner_w);
                (l.height(), l.height())
            }
            Kind::Icon { size, .. } => (*size, *size),
            Kind::Picture { pixmap, cover, density, .. } => {
                if *cover {
                    (0.0, pixmap.height() as f32 / density)
                } else {
                    let h = pixmap.height() as f32 / density;
                    (h, h)
                }
            }
            Kind::Custom { height, .. } => (*height, *height),
            Kind::Scroll { max_height, .. } => {
                let (_, nat) = self.children.first().map(|c| c.outer_h(inner_w)).unwrap_or_default();
                let h = nat.min(*max_height);
                (h, h)
            }
        };
        let (min, nat) = (min.max(self.style.min_height), nat.max(self.style.min_height));
        let req = (self.height_request - self.style.margin.v()).max(0.0);
        ((min + self.frame_v()).max(req), (nat + self.frame_v()).max(req))
    }

    fn outer_w(&self) -> (f32, f32) {
        let (a, b) = self.measure_w();
        let m = self.style.margin.h();
        (a + m, b + m)
    }
    fn outer_h(&self, outer_width: f32) -> (f32, f32) {
        let w = (outer_width - self.style.margin.h()).max(0.0);
        let w = match self.halign {
            Align::Fill => w,
            _ => {
                let (min, nat) = self.measure_w();
                nat.min(w).max(min)
            }
        };
        let (a, b) = self.measure_h(w);
        let m = self.style.margin.v();
        (a + m, b + m)
    }

    /// GTK box width distribution: everyone gets its minimum, extra goes to naturals, then to expanders.
    fn distribute(&self, kids: &[&Node], avail: f32, spacing: f32) -> Vec<f32> {
        if self.homogeneous && !kids.is_empty() {
            let each = ((avail - spacing * (kids.len() - 1) as f32) / kids.len() as f32).max(0.0);
            return vec![each; kids.len()];
        }
        let sizes: Vec<(f32, f32)> = kids.iter().map(|c| c.outer_w()).collect();
        let gaps = spacing * kids.len().saturating_sub(1) as f32;
        let mut out: Vec<f32> = sizes.iter().map(|s| s.0).collect();
        let mut extra = (avail - gaps - out.iter().sum::<f32>()).max(0.0);
        // grow towards natural sizes, smallest gap first (gtk_distribute_natural_allocation)
        let mut order: Vec<usize> = (0..kids.len()).collect();
        order.sort_by(|&a, &b| (sizes[a].1 - sizes[a].0).total_cmp(&(sizes[b].1 - sizes[b].0)));
        let mut left = order.len();
        for i in order {
            let share = extra / left as f32;
            let grow = (sizes[i].1 - sizes[i].0).min(share);
            out[i] += grow;
            extra -= grow;
            left -= 1;
        }
        let expanders: Vec<usize> = (0..kids.len()).filter(|&i| kids[i].compute_hexpand()).collect();
        if !expanders.is_empty() {
            let each = (extra / expanders.len() as f32).floor();
            let mut rest = extra - each * expanders.len() as f32;
            for i in expanders {
                out[i] += each;
                if rest >= 1.0 {
                    out[i] += 1.0;
                    rest -= 1.0;
                }
            }
        }
        out
    }

    fn distribute_v(&self, kids: &[&Node], width: f32, avail: f32, spacing: f32) -> Vec<f32> {
        let sizes: Vec<(f32, f32)> = kids.iter().map(|c| c.outer_h(width)).collect();
        let gaps = spacing * kids.len().saturating_sub(1) as f32;
        let mut out: Vec<f32> = sizes.iter().map(|s| s.0).collect();
        let mut extra = (avail - gaps - out.iter().sum::<f32>()).max(0.0);
        let mut order: Vec<usize> = (0..kids.len()).collect();
        order.sort_by(|&a, &b| (sizes[a].1 - sizes[a].0).total_cmp(&(sizes[b].1 - sizes[b].0)));
        let mut left = order.len();
        for i in order {
            let share = extra / left as f32;
            let grow = (sizes[i].1 - sizes[i].0).min(share);
            out[i] += grow;
            extra -= grow;
            left -= 1;
        }
        let expanders: Vec<usize> = (0..kids.len()).filter(|&i| kids[i].compute_vexpand()).collect();
        if !expanders.is_empty() {
            let each = (extra / expanders.len() as f32).floor();
            let mut rest = extra - each * expanders.len() as f32;
            for i in expanders {
                out[i] += each;
                if rest >= 1.0 {
                    out[i] += 1.0;
                    rest -= 1.0;
                }
            }
        }
        out
    }

    fn label_layout(&self, width: f32) -> Rc<Layout> {
        let Kind::Label(l) = &self.kind else { unreachable!() };
        let font = &self.style.font;
        let mut layout = if l.wrap {
            text::layout(&l.text, font, Some(width.max(1.0)), Wrap::Word, false)
        } else if l.ellipsize {
            text::layout(&l.text, font, Some(width.max(1.0)), Wrap::None, true)
        } else {
            text::layout(&l.text, font, None, Wrap::None, false)
        };
        if let Some(px) = self.style.line_height {
            let layout = Rc::make_mut(&mut layout);
            layout.ascent += (px - layout.line_height) / 2.0;
            layout.line_height = px;
        }
        if let Some(lh) = l.line_height {
            // Pango line-height factor: every line shrinks/grows, the difference split above/below
            let layout = Rc::make_mut(&mut layout);
            let h = layout.line_height * lh;
            layout.ascent += (h - layout.line_height) / 2.0;
            layout.line_height = h;
        }
        layout
    }

    /// Allocates the margin box `(x, y, w, h)`.
    pub fn allocate(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let m = self.style.margin;
        let (mut x, mut y, mut w, mut h) = (x + m.left, y + m.top, (w - m.h()).max(0.0), (h - m.v()).max(0.0));
        let (minw, natw) = self.measure_w();
        if self.halign != Align::Fill {
            let nw = natw.min(w).max(minw);
            x += match self.halign {
                Align::Center => ((w - nw) / 2.0).floor(),
                Align::End => w - nw,
                _ => 0.0,
            };
            w = nw;
        }
        if self.valign != Align::Fill {
            let (minh, nath) = self.measure_h(w);
            let nh = nath.min(h).max(minh);
            y += match self.valign {
                Align::Center => ((h - nh) / 2.0).floor(),
                Align::End => h - nh,
                _ => 0.0,
            };
            h = nh;
        }
        self.rect = Rect::from_xywh(x, y, w.max(0.0), h.max(0.0)).unwrap_or(self.rect);
        let b = self.style.border;
        let p = self.style.padding;
        let (cx, cy) = (x + b.left + p.left, y + b.top + p.top);
        let (cw, ch) = ((w - self.frame_h()).max(0.0), (h - self.frame_v()).max(0.0));
        match &self.kind {
            Kind::Box { vertical, spacing } => {
                let spacing = *spacing;
                let vertical = *vertical;
                let idx: Vec<usize> = (0..self.children.len()).filter(|&i| self.children[i].visible).collect();
                let sizes = {
                    let kids: Vec<&Node> = idx.iter().map(|&i| &self.children[i]).collect();
                    if vertical { self.distribute_v(&kids, cw, ch, spacing) } else { self.distribute(&kids, cw, spacing) }
                };
                let mut pos = 0.0;
                for (i, s) in idx.into_iter().zip(sizes) {
                    let c = &mut self.children[i];
                    if vertical {
                        c.allocate(cx, cy + pos, cw, s);
                    } else {
                        c.allocate(cx + pos, cy, s, ch);
                    }
                    pos += s + spacing;
                }
            }
            Kind::Overlay => {
                for c in self.children.iter_mut().filter(|c| c.visible) {
                    c.allocate(cx, cy, cw, ch);
                }
            }
            Kind::Scroll { offset, .. } => {
                let offset = *offset;
                if let Some(c) = self.children.first_mut() {
                    let (_, nat) = c.outer_h(cw);
                    c.allocate(cx, cy - offset, cw, nat);
                }
            }
            _ => {}
        }
    }

    pub fn content_rect(&self) -> Rect {
        let b = self.style.border;
        let p = self.style.padding;
        let r = self.rect;
        Rect::from_xywh(
            r.x() + b.left + p.left,
            r.y() + b.top + p.top,
            (r.width() - self.frame_h()).max(0.0),
            (r.height() - self.frame_v()).max(0.0),
        )
        .unwrap_or(r)
    }

    /// Bounding box of everything painted, shadows and outlines included (None: paints nothing).
    pub fn painted_bounds(&self) -> Option<Rect> {
        if !self.visible {
            return None;
        }
        let s = &self.style;
        let mut out: Option<Rect> = self.self_bounds();
        let mut add = |b: Rect| out = Some(out.map_or(b, |o| union(o, b)));
        for c in &self.children {
            if let Some(b) = c.painted_bounds() {
                let b = b.transform(Transform::from_translate(0.0, s.translate_y)).unwrap_or(b);
                add(match self.kind {
                    Kind::Scroll { .. } => vclip(b, self.content_rect()).unwrap_or(b),
                    _ => b,
                });
            }
        }
        out
    }

    /// What the node itself paints (children excluded), translate_y applied.
    fn self_bounds(&self) -> Option<Rect> {
        let s = &self.style;
        let paints_self = !matches!(s.background, Background::None)
            || s.border.top + s.border.right + s.border.bottom + s.border.left > 0.0
            || !s.box_shadow.is_empty()
            || s.outline.is_some()
            || !matches!(self.kind, Kind::Box { .. } | Kind::Overlay | Kind::Scroll { .. });
        if !paints_self {
            return None;
        }
        let grow = |r: Rect, d: f32, dx: f32, dy: f32| {
            Rect::from_ltrb(r.left() - d + dx.min(0.0), r.top() - d + dy.min(0.0), r.right() + d + dx.max(0.0), r.bottom() + d + dy.max(0.0)).unwrap_or(r)
        };
        let r = self.rect.transform(Transform::from_translate(0.0, s.translate_y)).unwrap_or(self.rect);
        let mut out = r;
        for sh in s.box_shadow.iter().filter(|s| !s.inset) {
            out = union(out, grow(r, sh.spread + sh.blur * 1.5, sh.x, sh.y));
        }
        if let Some((w, off, _)) = s.outline {
            out = union(out, grow(r, w + off, 0.0, 0.0));
        }
        for sh in s.text_shadow.iter().chain(&s.icon_shadow) {
            out = union(out, grow(r, sh.blur * 1.5, sh.x, sh.y));
        }
        Some(out)
    }

    /// Everything the tree paints as (device rect, hash of how), node by node. Two frames' lists
    /// differ exactly where their pixels can, so the difference is the damage.
    pub fn paint_items(&self, ts: Transform, salt: u64, clip: Option<Rect>, out: &mut Vec<(Rect, u64)>) {
        if !self.visible {
            return;
        }
        let s = &self.style;
        let salt = if s.opacity < 1.0 { hash(&(salt, s.opacity.to_bits())) } else { salt };
        if let Some(b) = self.self_bounds().and_then(|b| b.transform(ts))
            && let Some(b) = clip.map_or(Some(b), |c| vclip(b, c))
        {
            out.push((b, self.paint_hash(ts, salt)));
        }
        let ts = ts.pre_translate(0.0, s.translate_y);
        let clip = match self.kind {
            Kind::Scroll { .. } => self.content_rect().transform(ts).and_then(|c| clip.map_or(Some(c), |o| vclip(c, o))),
            _ => clip,
        };
        if matches!(self.kind, Kind::Scroll { .. }) && clip.is_none() {
            return;
        }
        for c in &self.children {
            c.paint_items(ts, salt, clip, out);
        }
    }

    fn paint_hash(&self, ts: Transform, salt: u64) -> u64 {
        use std::hash::{Hash, Hasher};
        thread_local! {
            static CUSTOM: Cell<u64> = const { Cell::new(0) };
        }
        let s = &self.style;
        let mut h = std::hash::DefaultHasher::new();
        let f = |h: &mut std::hash::DefaultHasher, v: &[f32]| v.iter().for_each(|x| x.to_bits().hash(h));
        let col = |h: &mut std::hash::DefaultHasher, c: Color| f(h, &[c.red(), c.green(), c.blue(), c.alpha()]);
        let shadows = |h: &mut std::hash::DefaultHasher, v: &[Shadow]| {
            for sh in v {
                f(h, &[sh.x, sh.y, sh.blur, sh.spread]);
                col(h, sh.color);
                sh.inset.hash(h);
            }
            0xff.hash(h);
        };
        salt.hash(&mut h);
        f(&mut h, &[ts.sx, ts.sy, ts.kx, ts.ky, ts.tx, ts.ty, s.translate_y]);
        f(&mut h, &[self.rect.left(), self.rect.top(), self.rect.width(), self.rect.height()]);
        for b in [s.border, s.padding] {
            f(&mut h, &[b.top, b.right, b.bottom, b.left]);
        }
        s.border_color.iter().for_each(|c| col(&mut h, *c));
        match s.background {
            Background::None => 0u8.hash(&mut h),
            Background::Color(c) => col(&mut h, c),
        }
        col(&mut h, s.color);
        shadows(&mut h, &s.box_shadow);
        if let Some((w, off, c)) = s.outline {
            f(&mut h, &[w, off]);
            col(&mut h, c);
        }
        match &self.kind {
            Kind::Label(l) => {
                shadows(&mut h, &s.text_shadow);
                (&l.text, l.wrap, l.ellipsize, s.font.family, s.font.weight).hash(&mut h);
                f(&mut h, &[l.xalign, l.max_width_chars.unwrap_or(-1.0), l.line_height.unwrap_or(-1.0), s.line_height.unwrap_or(-1.0)]);
                f(&mut h, &[s.font.size, s.font.letter_spacing]);
            }
            Kind::Icon { name, size } => {
                shadows(&mut h, &s.icon_shadow);
                name.hash(&mut h);
                f(&mut h, &[*size]);
            }
            Kind::Picture { pixmap, cover, blur, .. } => {
                (pixmap.key(), cover).hash(&mut h);
                f(&mut h, &[*blur]);
            }
            // arbitrary drawing: always repainted
            Kind::Custom { key: Some(k), .. } => k.hash(&mut h),
            Kind::Custom { key: None, .. } => CUSTOM.with(|c| c.replace(c.get() + 1)).hash(&mut h),
            Kind::Box { .. } | Kind::Overlay | Kind::Scroll { .. } => {}
        }
        h.finish()
    }

    pub fn paint_bounds(&self) -> Rect {
        self.painted_bounds().unwrap_or_else(|| Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap())
    }

    pub fn paint(&self, pm: &mut PixmapMut, ts: Transform) {
        if !self.visible {
            return;
        }
        // leaves scrolled out of the target (long lists) cost nothing
        if self.children.is_empty()
            && let Some(b) = self.painted_bounds().and_then(|b| b.transform(ts))
            && (b.right() < 0.0 || b.bottom() < 0.0 || b.left() > pm.width() as f32 || b.top() > pm.height() as f32)
        {
            return;
        }
        if self.style.opacity < 1.0 {
            // paint the subtree into a layer, then blend it at the given opacity
            let Some(b) = self.painted_bounds().and_then(|b| b.transform(ts)) else { return };
            let (ox, oy) = (b.left().floor() - 1.0, b.top().floor() - 1.0);
            let Some(mut layer) = Pixmap::new((b.width().ceil() + 3.0) as u32, (b.height().ceil() + 3.0) as u32) else { return };
            self.paint_inner(&mut layer.as_mut(), ts.post_translate(-ox, -oy));
            blit(pm, layer.as_ref(), ox as i32, oy as i32, self.style.opacity);
        } else {
            self.paint_inner(pm, ts);
        }
    }

    fn paint_inner(&self, pm: &mut PixmapMut, ts: Transform) {
        let ts = ts.pre_translate(0.0, self.style.translate_y);
        let r = self.rect;
        for s in self.style.box_shadow.iter().filter(|s| !s.inset) {
            paint_box_shadow(pm, r, s, ts);
        }
        match &self.style.background {
            Background::None => {}
            Background::Color(c) => fill_rect(pm, r, *c, ts),
        }
        for s in self.style.box_shadow.iter().filter(|s| s.inset) {
            paint_inset_shadow(pm, r, s, ts);
        }
        paint_border(pm, r, self.style.border, self.style.border_color, ts);

        let c = self.content_rect();
        match &self.kind {
            Kind::Label(l) => {
                let layout = self.label_layout(c.width());
                let x = c.x() + ((c.width() - layout.width()) * l.xalign).max(0.0);
                let y = c.y() + ((c.height() - layout.height()) / 2.0).max(0.0);
                for s in &self.style.text_shadow {
                    paint_text_shadow(pm, &layout, x, y, s, ts);
                }
                text::draw(pm, &layout, x, y, self.style.color, ts);
            }
            Kind::Icon { name, size } => {
                let x = c.x() + ((c.width() - size) / 2.0).floor();
                let y = c.y() + ((c.height() - size) / 2.0).floor();
                for s in &self.style.icon_shadow {
                    paint_icon_shadow(pm, name, *size, x, y, s, ts);
                }
                icons::draw(pm, name, *size, x, y, self.style.color, ts);
            }
            Kind::Picture { pixmap, cover, blur: radius, .. } => paint_picture(pm, pixmap, c, *cover, *radius, ts),
            Kind::Custom { paint, .. } => paint(pm, c, ts),
            _ => {}
        }

        let clip = matches!(self.kind, Kind::Scroll { .. });
        if clip {
            // clipped vertically only (the scrolling axis): rows' focus outlines may overhang the sides
            let kids = self.children.iter().filter_map(Node::painted_bounds).reduce(union);
            // only the part inside the target (partial repaints)
            let target = Rect::from_xywh(0.0, 0.0, pm.width() as f32, pm.height() as f32);
            let visible = kids.and_then(|k| vclip(k, c)).and_then(|r| r.transform(ts)).map(|d| {
                Rect::from_ltrb(d.left().round(), d.top().round(), d.right().round(), d.bottom().round()).and_then(|d| target.and_then(|t| d.intersect(&t)))
            });
            if let Some(Some(d)) = visible
                && let Some(mut layer) = Pixmap::new(d.width() as u32, d.height() as u32)
            {
                let (ox, oy) = (d.left(), d.top());
                for ch in &self.children {
                    ch.paint(&mut layer.as_mut(), ts.post_translate(-ox, -oy));
                }
                blit(pm, layer.as_ref(), ox as i32, oy as i32, 1.0);
            }
        } else {
            for ch in &self.children {
                ch.paint(pm, ts);
            }
        }

        if let Some((w, off, color)) = self.style.outline {
            let o = Rect::from_ltrb(r.left() - off - w, r.top() - off - w, r.right() + off + w, r.bottom() + off + w);
            if let Some(o) = o {
                paint_border(pm, o, Sides::all(w), [color; 4], ts);
            }
        }
    }
}

/// `b` limited to `c`'s vertical extent.
fn vclip(b: Rect, c: Rect) -> Option<Rect> {
    Rect::from_ltrb(b.left(), b.top().max(c.top()), b.right(), b.bottom().min(c.bottom()))
}

fn union(a: Rect, b: Rect) -> Rect {
    Rect::from_ltrb(a.left().min(b.left()), a.top().min(b.top()), a.right().max(b.right()), a.bottom().max(b.bottom())).unwrap_or(a)
}

/// A transparent layer covering the pixels a non-antialiased fill of `r` touches, and its device origin.
pub(crate) fn clip_layer(r: Rect, ts: Transform) -> Option<(Pixmap, (f32, f32))> {
    let d = r.transform(ts)?;
    let (x0, y0) = (d.left().round(), d.top().round());
    let layer = Pixmap::new((d.right().round() - x0).max(1.0) as u32, (d.bottom().round() - y0).max(1.0) as u32)?;
    Some((layer, (x0, y0)))
}

pub(crate) fn hash(v: &impl std::hash::Hash) -> u64 {
    use std::hash::Hasher;
    let mut s = std::hash::DefaultHasher::new();
    v.hash(&mut s);
    s.finish()
}

pub fn fill_rect(pm: &mut PixmapMut, r: Rect, c: Color, ts: Transform) {
    if ts.kx != 0.0 || ts.ky != 0.0 {
        let mut paint = Paint::default();
        paint.set_color(c);
        paint.anti_alias = false;
        pm.fill_rect(r, &paint, ts, None);
        return;
    }
    // axis-aligned, no antialiasing: the pixels whose centers fall inside, filled directly
    // (tiny-skia's pipeline costs several times more per pixel for a flat color)
    let Some(d) = r.transform(ts) else { return };
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let x0 = ((d.left() - 0.5).ceil() as i32).clamp(0, pw);
    let x1 = ((d.right() - 0.5).ceil() as i32).clamp(x0, pw);
    let y0 = ((d.top() - 0.5).ceil() as i32).clamp(0, ph);
    let y1 = ((d.bottom() - 0.5).ceil() as i32).clamp(y0, ph);
    let p = c.premultiply().to_color_u8();
    let src = [p.red(), p.green(), p.blue(), p.alpha()];
    let inv = 255 - src[3] as u32;
    if src[3] == 0 {
        return;
    }
    let data = pm.data_mut();
    for y in y0..y1 {
        let row = &mut data[(y * pw + x0) as usize * 4..(y * pw + x1) as usize * 4];
        if inv == 0 {
            row.as_chunks_mut::<4>().0.iter_mut().for_each(|px| *px = src);
        } else {
            for px in row.as_chunks_mut::<4>().0 {
                for i in 0..4 {
                    px[i] = (src[i] as u32 + (px[i] as u32 * inv + 127) / 255) as u8;
                }
            }
        }
    }
}

fn paint_border(pm: &mut PixmapMut, r: Rect, b: Sides, c: [Color; 4], ts: Transform) {
    let (l, t, rr, bb) = (r.left(), r.top(), r.right(), r.bottom());
    // GTK draws borders as four trapezoids; with square corners and equal colors rects are identical
    let parts = [
        (Rect::from_ltrb(l, t, rr, t + b.top), c[0]),
        (Rect::from_ltrb(rr - b.right, t + b.top, rr, bb - b.bottom), c[1]),
        (Rect::from_ltrb(l, bb - b.bottom, rr, bb), c[2]),
        (Rect::from_ltrb(l, t + b.top, l + b.left, bb - b.bottom), c[3]),
    ];
    for (rect, color) in parts {
        if let Some(rect) = rect
            && rect.width() > 0.0
            && rect.height() > 0.0
        {
            fill_rect(pm, rect, color, ts);
        }
    }
}

/// Finished shadow layers by what they depend on, least recently used dropped past a byte budget
/// (changing text like the clock would otherwise grow it forever).
#[derive(Default)]
struct LayerCache {
    map: std::collections::HashMap<u64, (Rc<Pixmap>, u64)>,
    bytes: usize,
    tick: u64,
}

impl LayerCache {
    const BUDGET: usize = 16 << 20;

    fn get(&mut self, key: u64) -> Option<Rc<Pixmap>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(&key).map(|(p, t)| {
            *t = tick;
            p.clone()
        })
    }

    fn insert(&mut self, key: u64, p: Rc<Pixmap>) {
        self.bytes += p.data().len();
        self.tick += 1;
        if let Some((old, _)) = self.map.insert(key, (p, self.tick)) {
            self.bytes -= old.data().len();
        }
        while self.bytes > Self::BUDGET && self.map.len() > 1 {
            let Some((&k, _)) = self.map.iter().min_by_key(|(_, (_, t))| *t) else { break };
            if let Some((p, _)) = self.map.remove(&k) {
                self.bytes -= p.data().len();
            }
        }
    }
}

thread_local! {
    static SHADOWS: std::cell::RefCell<LayerCache> = Default::default();
}

/// `layer`'s alpha blurred, then tinted `color` (premultiplied).
fn blur_tint(layer: &mut Pixmap, radius: f32, color: Color) {
    super::image::blur_alpha(layer, radius);
    let c = color.premultiply().to_color_u8();
    let (r, g, b, a) = (c.red() as u32, c.green() as u32, c.blue() as u32, c.alpha() as u32);
    for px in layer.data_mut().as_chunks_mut::<4>().0 {
        let al = px[3] as u32;
        *px = [((r * al + 127) / 255) as u8, ((g * al + 127) / 255) as u8, ((b * al + 127) / 255) as u8, ((a * al + 127) / 255) as u8];
    }
}

/// Renders `draw` into an alpha layer in device space, blurs it, and composites it tinted.
/// `content` identifies what `draw` paints, so identical shadows reuse the blurred layer.
pub(crate) fn blurred(pm: &mut PixmapMut, bounds: Rect, radius: f32, color: Color, ts: Transform, content: u64, draw: impl FnOnce(&mut PixmapMut, Transform)) {
    let scale = (ts.sx * ts.sx + ts.ky * ts.ky).sqrt(); // also right when rotated
    let pad = (radius * scale * 1.5).ceil() + 2.0;
    let dev = bounds.transform(ts).unwrap_or(bounds);
    let (ox, oy) = ((dev.left() - pad).floor(), (dev.top() - pad).floor());
    let (w, h) = ((dev.width() + pad * 2.0).ceil() as u32 + 1, (dev.height() + pad * 2.0).ceil() as u32 + 1);
    let c = color.to_color_u8();
    let key = hash(&(content, w, h, (dev.left() - ox).to_bits(), (dev.top() - oy).to_bits(), radius.to_bits(), scale.to_bits(), [c.red(), c.green(), c.blue(), c.alpha()]));
    let layer = match SHADOWS.with(|s| s.borrow_mut().get(key)) {
        Some(l) => l,
        None => {
            let Some(mut layer) = Pixmap::new(w, h) else { return };
            draw(&mut layer.as_mut(), ts.post_translate(-ox, -oy));
            blur_tint(&mut layer, radius * scale, color);
            let layer = Rc::new(layer);
            SHADOWS.with(|s| s.borrow_mut().insert(key, layer.clone()));
            layer
        }
    };
    blit(pm, Pixmap::as_ref(&layer), ox as i32, oy as i32, 1.0);
}

fn paint_box_shadow(pm: &mut PixmapMut, r: Rect, s: &Shadow, ts: Transform) {
    let Some(sr) = Rect::from_ltrb(r.left() - s.spread + s.x, r.top() - s.spread + s.y, r.right() + s.spread + s.x, r.bottom() + s.spread + s.y) else {
        return;
    };
    // drawn into a layer just big enough, then the border box is cut out (shadows are outside only)
    let scale = ts.sx;
    let pad = (s.blur * scale * 1.5).ceil() + 2.0;
    let dev = sr.transform(ts).unwrap_or(sr);
    let (ox, oy) = ((dev.left() - pad).floor(), (dev.top() - pad).floor());
    let (w, h) = ((dev.width() + pad * 2.0).ceil() as u32 + 2, (dev.height() + pad * 2.0).ceil() as u32 + 2);
    let lts = ts.post_translate(-ox, -oy);
    // the finished shadow only depends on its shape in the layer: buttons and rows repeat it
    let hole = r.transform(lts).unwrap_or(r);
    let c = s.color.to_color_u8();
    let key = hash(&(
        w,
        h,
        [(dev.left() - ox), (dev.top() - oy), dev.width(), dev.height(), hole.left(), hole.top(), hole.width(), hole.height(), s.blur, scale].map(f32::to_bits),
        [c.red(), c.green(), c.blue(), c.alpha()],
    ));
    if let Some(l) = SHADOWS.with(|m| m.borrow_mut().get(key)) {
        draw_around(pm, &l, ox, oy, hole);
        return;
    }
    let Some(mut layer) = Pixmap::new(w, h) else { return };
    if s.blur <= 0.0 {
        fill_rect(&mut layer.as_mut(), sr, s.color, lts);
    } else {
        fill_rect(&mut layer.as_mut(), sr, Color::BLACK, lts);
        blur_tint(&mut layer, s.blur * scale, s.color);
    }
    let clear = Paint { blend_mode: tiny_skia::BlendMode::Clear, anti_alias: false, ..Default::default() };
    layer.fill_rect(r, &clear, lts, None);
    draw_around(pm, &layer, ox, oy, hole);
    SHADOWS.with(|m| m.borrow_mut().insert(key, Rc::new(layer)));
}

/// Composites a box shadow layer except the cut-out border box (transparent): for a window-sized
/// shadow that interior is most of the layer, and compositing it cost as much as the rest.
fn draw_around(pm: &mut PixmapMut, layer: &Pixmap, ox: f32, oy: f32, hole: Rect) {
    let (w, h) = (layer.width() as i32, layer.height() as i32);
    // whole pixels inside the hole: cleared, so skipping them changes nothing
    let (x0, y0) = ((hole.left().ceil() as i32).clamp(0, w), (hole.top().ceil() as i32).clamp(0, h));
    let (x1, y1) = ((hole.right().floor() as i32).clamp(x0, w), (hole.bottom().floor() as i32).clamp(y0, h));
    for part in [(0, 0, w, y0), (0, y1, w, h), (0, y0, x0, y1), (x1, y0, w, y1)] {
        blit_part(pm, layer.as_ref(), ox as i32, oy as i32, part, 255);
    }
}

/// `src` composited (source-over) with its top-left at whole pixel (x, y). tiny-skia's draw_pixmap
/// runs every pixel through its general sampling pipeline (~10x slower than this for an
/// untransformed copy), and these layers are composited on every frame.
pub(crate) fn blit(pm: &mut PixmapMut, src: tiny_skia::PixmapRef, x: i32, y: i32, opacity: f32) {
    let all = (0, 0, src.width() as i32, src.height() as i32);
    blit_part(pm, src, x, y, all, (opacity.clamp(0.0, 1.0) * 255.0 + 0.5) as u32);
}

/// The (left, top, right, bottom) part of `src`, at `opacity` out of 255.
fn blit_part(pm: &mut PixmapMut, src: tiny_skia::PixmapRef, x: i32, y: i32, (l, t, r, b): (i32, i32, i32, i32), opacity: u32) {
    let (sw, pw, ph) = (src.width() as i32, pm.width() as i32, pm.height() as i32);
    // clipped to the target
    let (l, r) = (l.max(-x), r.min(pw - x));
    let (t, b) = (t.max(-y), b.min(ph - y));
    if l >= r || t >= b {
        return;
    }
    let s = src.data();
    let d = pm.data_mut();
    for row in t..b {
        let srow = &s[(row * sw + l) as usize * 4..(row * sw + r) as usize * 4];
        let drow = &mut d[((row + y) * pw + l + x) as usize * 4..((row + y) * pw + r + x) as usize * 4];
        for (sp, dp) in srow.as_chunks::<4>().0.iter().zip(drow.as_chunks_mut::<4>().0) {
            let a = sp[3] as u32 * opacity / 255;
            if a == 0 {
                continue;
            }
            if a == 255 {
                dp.copy_from_slice(sp);
                continue;
            }
            // premultiplied source-over
            let inv = 255 - a;
            for c in 0..4 {
                let sc = sp[c] as u32 * opacity / 255;
                dp[c] = (sc + (dp[c] as u32 * inv + 127) / 255) as u8;
            }
        }
    }
}

fn paint_inset_shadow(pm: &mut PixmapMut, r: Rect, s: &Shadow, ts: Transform) {
    // inset: the area between the padding box and a rect shrunk by spread (offset by x/y)
    // inner clipped to the box, so the even-odd fill stays inside it
    let inner = Rect::from_ltrb(r.left() + s.spread + s.x, r.top() + s.spread + s.y, r.right() - s.spread + s.x, r.bottom() - s.spread + s.y);
    let Some(inner) = inner.and_then(|i| i.intersect(&r)) else {
        fill_rect(pm, r, s.color, ts);
        return;
    };
    let mut pb = PathBuilder::new();
    pb.push_rect(r);
    pb.push_rect(inner);
    let Some(path) = pb.finish() else { return };
    let mut paint = Paint::default();
    paint.set_color(s.color);
    pm.fill_path(&path, &paint, FillRule::EvenOdd, ts, None);
}

pub(crate) fn paint_text_shadow(pm: &mut PixmapMut, layout: &Layout, x: f32, y: f32, s: &Shadow, ts: Transform) {
    let bounds = Rect::from_xywh(x + s.x, y + s.y, layout.width().max(1.0), layout.height().max(1.0)).unwrap();
    if s.blur <= 0.0 {
        text::draw(pm, layout, x + s.x, y + s.y, s.color, ts);
        return;
    }
    blurred(pm, bounds, s.blur, s.color, ts, layout.content_hash(), |p, t| text::draw(p, layout, x + s.x, y + s.y, Color::BLACK, t));
}

fn paint_icon_shadow(pm: &mut PixmapMut, name: &str, size: f32, x: f32, y: f32, s: &Shadow, ts: Transform) {
    let bounds = Rect::from_xywh(x + s.x, y + s.y, size, size).unwrap();
    if s.blur <= 0.0 {
        icons::draw(pm, name, size, x + s.x, y + s.y, s.color, ts);
        return;
    }
    blurred(pm, bounds, s.blur, s.color, ts, hash(&(name, size.to_bits())), |p, t| icons::draw(p, name, size, x + s.x, y + s.y, Color::BLACK, t));
}

fn paint_picture(pm: &mut PixmapMut, img: &Img, c: Rect, cover: bool, radius: f32, ts: Transform) {
    let pix = img.as_ref();
    let (iw, ih) = (pix.width() as f32, pix.height() as f32);
    let (sx, sy, sw, sh) = if cover { super::image::cover(iw, ih, c.width(), c.height()) } else { (0.0, 0.0, iw, ih) };
    let (dw, dh) = if cover {
        (c.width(), c.height())
    } else {
        let s = (c.width() / iw).min(c.height() / ih).min(1.0);
        (iw * s, ih * s)
    };
    let (dx, dy) = (c.x() + (c.width() - dw) / 2.0, c.y() + (c.height() - dh) / 2.0);
    let t = ts.pre_translate(dx, dy).pre_scale(dw / sw, dh / sh).pre_translate(-sx, -sy);
    // pre-scaled to device pixels (thumbnails): a plain copy, bicubic would cost ~10x per frame
    let exact = ts.kx == 0.0 && ts.ky == 0.0 && (ts.sx * dw / sw - 1.0).abs() < 1e-3 && (ts.sy * dh / sh - 1.0).abs() < 1e-3;
    let quality = if exact { tiny_skia::FilterQuality::Nearest } else { tiny_skia::FilterQuality::Bicubic };
    let paint = PixmapPaint { quality, ..Default::default() };
    if radius <= 0.0 {
        if exact && !cover {
            // pre-scaled to device pixels and wholly inside its box: a straight copy
            blit(pm, pix, (ts.sx * dx + ts.tx).round() as i32, (ts.sy * dy + ts.ty).round() as i32, 1.0);
            return;
        }
        // pattern fill over the visible part of the image, no clip mask
        let Some(area) = Rect::from_xywh(dx, dy, dw, dh).and_then(|d| d.intersect(&c)) else { return };
        let pt = Transform::from_translate(dx, dy).pre_scale(dw / sw, dh / sh).pre_translate(-sx, -sy);
        let shader = tiny_skia::Pattern::new(pix, tiny_skia::SpreadMode::Pad, paint.quality, 1.0, pt);
        pm.fill_rect(area, &Paint { shader, anti_alias: false, ..Default::default() }, ts, None);
        return;
    }
    // GTK `filter: blur()` blurs the rendered node; edges fade out over the radius.
    // The result only depends on the image and the device rect, so it is cached.
    let dev = c.transform(ts).unwrap_or(c);
    let pad = (radius * ts.sx * 1.5).ceil();
    let (ox, oy) = (dev.left().floor() - pad, dev.top().floor() - pad);
    let key = (img.key(), dev.width().to_bits(), dev.height().to_bits(), (dev.left() - dev.left().floor()).to_bits(), radius.to_bits());
    // one entry per window header: a single slot made every window switch re-blur the wallpaper (~60ms)
    thread_local! {
        static BLURRED: std::cell::RefCell<Vec<((usize, u32, u32, u32, u32), Rc<Pixmap>)>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let cached = BLURRED.with(|b| b.borrow().iter().find(|(k, _)| *k == key).map(|(_, p)| p.clone()));
    let layer = match cached {
        Some(l) => l,
        None => {
            let Some(mut layer) = Pixmap::new((dev.width() + pad * 2.0).ceil() as u32 + 1, (dev.height() + pad * 2.0).ceil() as u32 + 1) else {
                return;
            };
            let mut clip = tiny_skia::Mask::new(layer.width(), layer.height()).unwrap();
            clip.fill_path(&PathBuilder::from_rect(c), FillRule::Winding, false, ts.post_translate(-ox, -oy));
            // about to be blurred: bilinear is indistinguishable and much cheaper than bicubic
            let smooth = PixmapPaint { quality: tiny_skia::FilterQuality::Bilinear, ..paint };
            layer.draw_pixmap(0, 0, pix, &smooth, t.post_translate(-ox, -oy), Some(&clip));
            blur(&mut layer, radius * ts.sx);
            let l = Rc::new(layer);
            BLURRED.with(|b| {
                let mut b = b.borrow_mut();
                b.retain(|(k, _)| k.0 == key.0); // a new wallpaper drops the old one's blurs
                if b.len() >= 16 {
                    b.remove(0);
                }
                b.push((key, l.clone()));
            });
            l
        }
    };
    blit(pm, Pixmap::as_ref(&layer), ox as i32, oy as i32, 1.0);
}

/// rgba from 0-255 channels and 0-1 alpha.
pub fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color {
    match Color::from_rgba(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a) {
        Some(c) => c,
        None => Color::TRANSPARENT,
    }
}

pub fn hex(v: u32) -> Color {
    rgba((v >> 16) as u8, (v >> 8) as u8, v as u8, 1.0)
}

pub fn alpha(c: Color, a: f32) -> Color {
    match Color::from_rgba(c.red(), c.green(), c.blue(), c.alpha() * a) {
        Some(c) => c,
        None => Color::TRANSPARENT,
    }
}

/// Picture source: an owned pixmap (thumbnails) or the shared shm wallpaper.
#[derive(Clone)]
pub enum Img {
    Pix(Rc<Pixmap>),
    Shm(Rc<super::wayland::ShmImage>),
}

impl Img {
    pub fn as_ref(&self) -> tiny_skia::PixmapRef<'_> {
        match self {
            Img::Pix(p) => p.as_ref().as_ref(),
            Img::Shm(s) => s.pixmap(),
        }
    }
    pub fn width(&self) -> u32 {
        self.as_ref().width()
    }
    pub fn height(&self) -> u32 {
        self.as_ref().height()
    }
    fn key(&self) -> usize {
        match self {
            Img::Pix(p) => Rc::as_ptr(p) as usize,
            Img::Shm(s) => Rc::as_ptr(s) as usize,
        }
    }
}
