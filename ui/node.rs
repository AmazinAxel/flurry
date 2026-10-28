// Widget tree with GTK's box model: margin > border > padding > content, min sizes on the content box,
// halign/valign/expand, boxes with spacing, overlays. Windows rebuild their tree from state on change,
// then lay it out and paint it.

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
    fn h(&self) -> f32 {
        self.left + self.right
    }
    fn v(&self) -> f32 {
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
    /// top-to-bottom linear gradient
    Vertical(Color, Color),
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
    pub min_height: f32,
    pub opacity: f32,
    pub box_shadow: Vec<Shadow>,
    pub outline: Option<(f32, f32, Color)>, // width, offset, color
    pub text_shadow: Vec<Shadow>,
    pub icon_shadow: Vec<Shadow>,
    pub translate_y: f32,
    pub radius: f32,
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
            min_height: 0.0,
            opacity: 1.0,
            box_shadow: vec![],
            outline: None,
            text_shadow: vec![],
            icon_shadow: vec![],
            translate_y: 0.0,
            radius: 0.0,
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
    Picture { pixmap: Img, cover: bool, blur: f32 },
    /// Arbitrary paint into the content box, with a fixed natural size
    Custom { width: f32, height: f32, paint: Rc<dyn Fn(&mut PixmapMut, Rect, Transform)> },
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
        }
    }
    pub fn vbox(style: Style, spacing: f32, children: Vec<Node>) -> Self {
        Self { children, ..Self::new(Kind::Box { vertical: true, spacing }, style) }
    }
    pub fn hbox(style: Style, spacing: f32, children: Vec<Node>) -> Self {
        Self { children, ..Self::new(Kind::Box { vertical: false, spacing }, style) }
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
                        let cap = chars * text::approx_char_width(font);
                        let longest_word = l
                            .text
                            .split_whitespace()
                            .map(|w| text::layout(w, font, None, Wrap::None, false).width())
                            .fold(0.0, f32::max);
                        // GtkLabel: a wrapping label asks for max-width-chars worth of width even for short text
                        let _ = full;
                        (longest_word.min(cap), cap)
                    }
                    _ => (full, full),
                }
            }
            Kind::Icon { size, .. } => (*size, *size),
            Kind::Picture { pixmap, cover, .. } => {
                if *cover {
                    (0.0, pixmap.width() as f32)
                } else {
                    (pixmap.width() as f32, pixmap.width() as f32)
                }
            }
            Kind::Custom { width, .. } => (*width, *width),
            Kind::Scroll { .. } => self.children.first().map(|c| c.outer_w()).unwrap_or_default(),
        };
        let (min, nat) = (min.max(self.style.min_width), nat.max(self.style.min_width));
        // GTK4 size requests cover the CSS margin box
        let req = (self.width_request - self.style.margin.h()).max(0.0);
        ((min + self.frame_h()).max(req), (nat + self.frame_h()).max(req))
    }

    /// (minimum, natural) border-box height for a border-box `width`.
    pub fn measure_h(&self, width: f32) -> (f32, f32) {
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
            Kind::Picture { pixmap, cover, .. } => {
                if *cover {
                    (0.0, pixmap.height() as f32)
                } else {
                    let h = pixmap.height() as f32;
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
            _ => self.measure_w().1.min(w).max(self.measure_w().0),
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

    fn label_layout(&self, width: f32) -> Layout {
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
            layout.ascent += (px - layout.line_height) / 2.0;
            layout.line_height = px;
        }
        if let Some(lh) = l.line_height {
            // Pango line-height factor: every line shrinks/grows, the difference split above/below
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

    pub fn find(&self, id: &str) -> Option<&Node> {
        if self.id.as_deref() == Some(id) {
            return Some(self);
        }
        self.children.iter().filter(|c| c.visible).find_map(|c| c.find(id))
    }

    /// Bounding box of everything painted, shadows and outlines included (None: paints nothing).
    pub fn painted_bounds(&self) -> Option<Rect> {
        if !self.visible {
            return None;
        }
        let s = &self.style;
        let paints_self = !matches!(s.background, Background::None)
            || s.border.top + s.border.right + s.border.bottom + s.border.left > 0.0
            || !s.box_shadow.is_empty()
            || s.outline.is_some()
            || !matches!(self.kind, Kind::Box { .. } | Kind::Overlay | Kind::Scroll { .. });
        let grow = |r: Rect, d: f32, dx: f32, dy: f32| {
            Rect::from_ltrb(r.left() - d + dx.min(0.0), r.top() - d + dy.min(0.0), r.right() + d + dx.max(0.0), r.bottom() + d + dy.max(0.0)).unwrap_or(r)
        };
        let mut out: Option<Rect> = None;
        let mut add = |b: Rect| out = Some(out.map_or(b, |o| union(o, b)));
        if paints_self {
            let r = self.rect.transform(Transform::from_translate(0.0, s.translate_y)).unwrap_or(self.rect);
            add(r);
            for sh in s.box_shadow.iter().filter(|s| !s.inset) {
                add(grow(r, sh.spread + sh.blur * 1.5, sh.x, sh.y));
            }
            if let Some((w, off, _)) = s.outline {
                add(grow(r, w + off, 0.0, 0.0));
            }
            for sh in s.text_shadow.iter().chain(&s.icon_shadow) {
                add(grow(r, sh.blur * 1.5, sh.x, sh.y));
            }
        }
        for c in &self.children {
            if let Some(b) = c.painted_bounds() {
                let b = b.transform(Transform::from_translate(0.0, s.translate_y)).unwrap_or(b);
                add(match self.kind {
                    Kind::Scroll { .. } => b.intersect(&self.content_rect()).unwrap_or(b),
                    _ => b,
                });
            }
        }
        out
    }

    pub fn paint_bounds(&self) -> Rect {
        self.painted_bounds().unwrap_or_else(|| Rect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap())
    }

    pub fn paint(&self, pm: &mut PixmapMut, ts: Transform) {
        if !self.visible {
            return;
        }
        if self.style.opacity < 1.0 {
            // paint the subtree into a layer, then blend it at the given opacity
            let Some(b) = self.painted_bounds().and_then(|b| b.transform(ts)) else { return };
            let (ox, oy) = (b.left().floor() - 1.0, b.top().floor() - 1.0);
            let Some(mut layer) = Pixmap::new((b.width().ceil() + 3.0) as u32, (b.height().ceil() + 3.0) as u32) else { return };
            self.paint_inner(&mut layer.as_mut(), ts.post_translate(-ox, -oy));
            let paint = PixmapPaint { opacity: self.style.opacity, ..Default::default() };
            pm.draw_pixmap(ox as i32, oy as i32, layer.as_ref(), &paint, Transform::identity(), None);
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
            Background::Vertical(a, b) => {
                if let Some(shader) = tiny_skia::LinearGradient::new(
                    (0.0, r.top()).into(),
                    (0.0, r.bottom()).into(),
                    vec![tiny_skia::GradientStop::new(0.0, *a), tiny_skia::GradientStop::new(1.0, *b)],
                    tiny_skia::SpreadMode::Pad,
                    Transform::identity(),
                ) {
                    let paint = Paint { shader, anti_alias: false, ..Default::default() };
                    pm.fill_rect(r, &paint, ts, None);
                }
            }
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
            Kind::Picture { pixmap, cover, blur: radius } => paint_picture(pm, pixmap, c, *cover, *radius, ts),
            Kind::Custom { paint, .. } => paint(pm, c, ts),
            _ => {}
        }

        let clip = matches!(self.kind, Kind::Scroll { .. });
        if clip {
            // children render into a clipped layer
            let (w, h) = (pm.width(), pm.height());
            if let Some(mut layer) = Pixmap::new(w, h) {
                for ch in &self.children {
                    ch.paint(&mut layer.as_mut(), ts);
                }
                let mut mask = tiny_skia::Mask::new(w, h).unwrap();
                mask.fill_path(&PathBuilder::from_rect(c), FillRule::Winding, false, ts);
                pm.draw_pixmap(0, 0, layer.as_ref(), &PixmapPaint::default(), Transform::identity(), Some(&mask));
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

fn union(a: Rect, b: Rect) -> Rect {
    Rect::from_ltrb(a.left().min(b.left()), a.top().min(b.top()), a.right().max(b.right()), a.bottom().max(b.bottom())).unwrap_or(a)
}

pub fn fill_rect(pm: &mut PixmapMut, r: Rect, c: Color, ts: Transform) {
    let mut paint = Paint::default();
    paint.set_color(c);
    paint.anti_alias = false;
    pm.fill_rect(r, &paint, ts, None);
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

/// Renders `draw` into an alpha layer in device space, blurs it, and composites it tinted.
fn blurred(pm: &mut PixmapMut, bounds: Rect, radius: f32, color: Color, ts: Transform, draw: impl FnOnce(&mut PixmapMut, Transform)) {
    let scale = ts.sx;
    let pad = (radius * scale * 1.5).ceil() + 2.0;
    let dev = bounds.transform(ts).unwrap_or(bounds);
    let (ox, oy) = ((dev.left() - pad).floor(), (dev.top() - pad).floor());
    let (w, h) = ((dev.width() + pad * 2.0).ceil() as u32 + 1, (dev.height() + pad * 2.0).ceil() as u32 + 1);
    let Some(mut layer) = Pixmap::new(w, h) else { return };
    draw(&mut layer.as_mut(), ts.post_translate(-ox, -oy));
    blur(&mut layer, radius * scale);
    // tint: keep alpha, replace color
    let (r, g, b, a) = (color.red(), color.green(), color.blue(), color.alpha());
    for px in layer.data_mut().as_chunks_mut::<4>().0 {
        let al = px[3] as f32 / 255.0 * a;
        px[0] = (r * al * 255.0).round() as u8;
        px[1] = (g * al * 255.0).round() as u8;
        px[2] = (b * al * 255.0).round() as u8;
        px[3] = (al * 255.0).round() as u8;
    }
    pm.draw_pixmap(ox as i32, oy as i32, layer.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
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
    let Some(mut layer) = Pixmap::new(w, h) else { return };
    let lts = ts.post_translate(-ox, -oy);
    if s.blur <= 0.0 {
        fill_rect(&mut layer.as_mut(), sr, s.color, lts);
    } else {
        blurred(&mut layer.as_mut(), sr, s.blur, s.color, lts, |p, t| fill_rect(p, sr, Color::BLACK, t));
    }
    let clear = Paint { blend_mode: tiny_skia::BlendMode::Clear, anti_alias: false, ..Default::default() };
    layer.fill_rect(r, &clear, lts, None);
    pm.draw_pixmap(ox as i32, oy as i32, layer.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
}

fn paint_inset_shadow(pm: &mut PixmapMut, r: Rect, s: &Shadow, ts: Transform) {
    // inset: the area between the padding box and a rect shrunk by spread (offset by x/y)
    let Some(inner) = Rect::from_ltrb(r.left() + s.spread + s.x, r.top() + s.spread + s.y, r.right() - s.spread + s.x, r.bottom() - s.spread + s.y) else {
        fill_rect(pm, r, s.color, ts);
        return;
    };
    let mut pb = PathBuilder::new();
    pb.push_rect(r);
    pb.push_rect(inner);
    let Some(path) = pb.finish() else { return };
    let mut clip = tiny_skia::Mask::new(pm.width(), pm.height()).unwrap();
    clip.fill_path(&PathBuilder::from_rect(r), FillRule::Winding, false, ts);
    let mut paint = Paint::default();
    paint.set_color(s.color);
    pm.fill_path(&path, &paint, FillRule::EvenOdd, ts, Some(&clip));
}

pub(crate) fn paint_text_shadow(pm: &mut PixmapMut, layout: &Layout, x: f32, y: f32, s: &Shadow, ts: Transform) {
    let bounds = Rect::from_xywh(x + s.x, y + s.y, layout.width().max(1.0), layout.height().max(1.0)).unwrap();
    if s.blur <= 0.0 {
        text::draw(pm, layout, x + s.x, y + s.y, s.color, ts);
        return;
    }
    blurred(pm, bounds, s.blur, s.color, ts, |p, t| text::draw(p, layout, x + s.x, y + s.y, Color::BLACK, t));
}

fn paint_icon_shadow(pm: &mut PixmapMut, name: &str, size: f32, x: f32, y: f32, s: &Shadow, ts: Transform) {
    let bounds = Rect::from_xywh(x + s.x, y + s.y, size, size).unwrap();
    if s.blur <= 0.0 {
        icons::draw(pm, name, size, x + s.x, y + s.y, s.color, ts);
        return;
    }
    blurred(pm, bounds, s.blur, s.color, ts, |p, t| icons::draw(p, name, size, x + s.x, y + s.y, Color::BLACK, t));
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
    let paint = PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() };
    if radius <= 0.0 {
        let mut clip = tiny_skia::Mask::new(pm.width(), pm.height()).unwrap();
        clip.fill_path(&PathBuilder::from_rect(c), FillRule::Winding, false, ts);
        pm.draw_pixmap(0, 0, pix, &paint, t, Some(&clip));
        return;
    }
    // GTK `filter: blur()` blurs the rendered node; edges fade out over the radius.
    // The result only depends on the image and the device rect, so it is cached.
    let dev = c.transform(ts).unwrap_or(c);
    let pad = (radius * ts.sx * 1.5).ceil();
    let (ox, oy) = (dev.left().floor() - pad, dev.top().floor() - pad);
    let key = (img.key(), dev.width().to_bits(), dev.height().to_bits(), (dev.left() - dev.left().floor()).to_bits(), radius.to_bits());
    thread_local! {
        static BLURRED: std::cell::RefCell<Option<((usize, u32, u32, u32, u32), Rc<Pixmap>)>> = const { std::cell::RefCell::new(None) };
    }
    let cached = BLURRED.with(|b| b.borrow().as_ref().filter(|(k, _)| *k == key).map(|(_, p)| p.clone()));
    let layer = match cached {
        Some(l) => l,
        None => {
            let Some(mut layer) = Pixmap::new((dev.width() + pad * 2.0).ceil() as u32 + 1, (dev.height() + pad * 2.0).ceil() as u32 + 1) else {
                return;
            };
            let mut clip = tiny_skia::Mask::new(layer.width(), layer.height()).unwrap();
            clip.fill_path(&PathBuilder::from_rect(c), FillRule::Winding, false, ts.post_translate(-ox, -oy));
            layer.draw_pixmap(0, 0, pix, &paint, t.post_translate(-ox, -oy), Some(&clip));
            blur(&mut layer, radius * ts.sx);
            let l = Rc::new(layer);
            BLURRED.with(|b| *b.borrow_mut() = Some((key, l.clone())));
            l
        }
    };
    pm.draw_pixmap(ox as i32, oy as i32, Pixmap::as_ref(&layer), &PixmapPaint::default(), Transform::identity(), None);
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

impl Style {
    pub fn margin_h(&self) -> f32 {
        self.margin.h()
    }
    pub fn margin_v(&self) -> f32 {
        self.margin.v()
    }
}

impl Node {
    /// Makes this node a focus target named `id`.
    pub fn focusable(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self.focusable = true;
        self
    }
}

impl Node {
    /// Ellipsize END without max-width-chars: natural width is the full text.
    pub fn ellipsize_natural(mut self) -> Self {
        if let Kind::Label(l) = &mut self.kind {
            l.ellipsize = true;
        }
        self
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
