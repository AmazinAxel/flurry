// Single-line text entry: caret, selection, word ops, clipboard. Rendering is a Custom node so the
// owning window controls layout and styling.

use std::cell::RefCell;
use std::rc::Rc;

use tiny_skia::{Color, PixmapMut, Rect, Transform};
use xkbcommon::xkb::Keysym;

use super::node::{Kind, Node, Style, fill_rect};
use super::text::{self, Wrap};
use super::wayland::{self, KeyEvent};

#[derive(Default)]
pub struct Entry {
    pub text: String,
    /// caret and selection anchor, byte offsets
    pub cursor: usize,
    pub anchor: usize,
    pub password: bool,
    /// GTK invisible-char; None draws nothing for hidden text (lockscreen invisibleChar=0)
    pub invisible_char: Option<char>,
    pub max_len: Option<usize>,
    pub placeholder: String,
    pub on_change: Option<Rc<dyn Fn(&str)>>,
    /// Horizontal scroll so the caret stays visible
    scroll: f32,
}

pub type EntryRef = Rc<RefCell<Entry>>;

pub fn new(placeholder: &str) -> EntryRef {
    Rc::new(RefCell::new(Entry { placeholder: placeholder.into(), invisible_char: Some('●'), ..Default::default() }))
}

fn prev_char(s: &str, i: usize) -> usize {
    s[..i].char_indices().next_back().map(|(j, _)| j).unwrap_or(0)
}
fn next_char(s: &str, i: usize) -> usize {
    s[i..].chars().next().map(|c| i + c.len_utf8()).unwrap_or(i)
}
fn prev_word(s: &str, i: usize) -> usize {
    let mut j = i;
    while j > 0 && s[..j].ends_with(|c: char| !c.is_alphanumeric()) {
        j = prev_char(s, j);
    }
    while j > 0 && s[..j].ends_with(char::is_alphanumeric) {
        j = prev_char(s, j);
    }
    j
}
fn next_word(s: &str, i: usize) -> usize {
    let mut j = i;
    while j < s.len() && !s[j..].starts_with(char::is_alphanumeric) {
        j = next_char(s, j);
    }
    while j < s.len() && s[j..].starts_with(char::is_alphanumeric) {
        j = next_char(s, j);
    }
    j
}

impl Entry {
    pub fn set_text(&mut self, t: &str) {
        self.text = t.into();
        self.cursor = self.text.len();
        self.anchor = self.cursor;
        self.changed();
    }

    fn changed(&self) {
        if let Some(f) = &self.on_change {
            f(&self.text);
        }
    }

    fn selection(&self) -> (usize, usize) {
        (self.cursor.min(self.anchor), self.cursor.max(self.anchor))
    }

    fn delete_selection(&mut self) -> bool {
        let (a, b) = self.selection();
        if a == b {
            return false;
        }
        self.text.replace_range(a..b, "");
        self.cursor = a;
        self.anchor = a;
        true
    }

    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        let mut s: String = s.chars().filter(|c| !c.is_control()).collect();
        if let Some(max) = self.max_len {
            let room = max.saturating_sub(self.text.chars().count());
            s = s.chars().take(room).collect();
        }
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
        self.anchor = self.cursor;
        self.changed();
    }

    fn selected_text(&self) -> String {
        let (a, b) = self.selection();
        self.text[a..b].to_string()
    }

    /// Handles editing keys; returns true when consumed.
    pub fn key(this: &EntryRef, ev: &KeyEvent) -> bool {
        let mut e = this.borrow_mut();
        let sym = ev.sym;
        let moving = |e: &mut Entry, to: usize, shift: bool| {
            e.cursor = to;
            if !shift {
                e.anchor = to;
            }
        };
        if ev.ctrl {
            match ev.lower() {
                Keysym::a => {
                    e.anchor = 0;
                    e.cursor = e.text.len();
                    return true;
                }
                Keysym::c | Keysym::x if !e.password => {
                    let t = e.selected_text();
                    if !t.is_empty() {
                        crate::lib::clipboard::copy(&t);
                        if ev.lower() == Keysym::x {
                            e.delete_selection();
                            e.changed();
                        }
                    }
                    return true;
                }
                Keysym::v => {
                    drop(e);
                    let weak = Rc::downgrade(this);
                    wayland::paste(move |s| {
                        if let Some(e) = weak.upgrade() {
                            let line = s.lines().next().unwrap_or("").to_string();
                            e.borrow_mut().insert(&line);
                        }
                    });
                    return true;
                }
                _ => {}
            }
        }
        match sym {
            Keysym::BackSpace => {
                if !e.delete_selection() && e.cursor > 0 {
                    let to = if ev.ctrl { prev_word(&e.text, e.cursor) } else { prev_char(&e.text, e.cursor) };
                    let c = e.cursor;
                    e.text.replace_range(to..c, "");
                    e.cursor = to;
                    e.anchor = to;
                }
                e.changed();
                true
            }
            Keysym::Left => {
                let to = if ev.ctrl {
                    prev_word(&e.text, e.cursor)
                } else if !ev.shift && e.cursor != e.anchor {
                    e.selection().0
                } else {
                    prev_char(&e.text, e.cursor)
                };
                moving(&mut e, to, ev.shift);
                true
            }
            Keysym::Right => {
                let to = if ev.ctrl {
                    next_word(&e.text, e.cursor)
                } else if !ev.shift && e.cursor != e.anchor {
                    e.selection().1
                } else {
                    next_char(&e.text, e.cursor)
                };
                moving(&mut e, to, ev.shift);
                true
            }
            _ if !ev.ctrl && !ev.alt && !ev.text.is_empty() => {
                let t = ev.text.clone();
                e.insert(&t);
                true
            }
            _ => false,
        }
    }

    fn display(&self) -> String {
        if self.password {
            self.invisible_char.map(|c| std::iter::repeat_n(c, self.text.chars().count()).collect()).unwrap_or_default()
        } else {
            self.text.clone()
        }
    }

    fn display_index(&self, byte: usize) -> usize {
        if !self.password {
            return byte;
        }
        let chars = self.text[..byte].chars().count();
        chars * self.invisible_char.map(char::len_utf8).unwrap_or(0)
    }
}

pub struct Look {
    pub color: Color,
    pub placeholder: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub caret: bool,
}

/// The text area as a node (GTK's `text` node inside `entry`), filling its allocation.
pub fn text_node(this: &EntryRef, style: Style, look: Look) -> Node {
    let this = this.clone();
    let font = style.font.clone();
    let shadows = style.text_shadow.clone();
    let height = text::layout("Ag", &font, None, Wrap::None, false).height();
    let paint = Rc::new(move |pm: &mut PixmapMut, r: Rect, ts: Transform| {
        let mut e = this.borrow_mut();
        let shown = e.display();
        let (text_s, color) = if shown.is_empty() { (e.placeholder.clone(), look.placeholder) } else { (shown.clone(), look.color) };
        let layout = text::layout(&text_s, &font, None, Wrap::None, false);
        let x_at = |byte: usize| text::layout(&shown[..byte.min(shown.len())], &font, None, Wrap::None, false).width();
        let caret_x = x_at(e.display_index(e.cursor));
        // keep the caret inside the visible area
        if caret_x - e.scroll > r.width() - 1.0 {
            e.scroll = caret_x - r.width() + 1.0;
        } else if caret_x < e.scroll {
            e.scroll = caret_x;
        }
        let x = r.x() - e.scroll;
        let y = r.y() + ((r.height() - layout.height()) / 2.0).floor();
        let clip = r;
        let (a, b) = e.selection();
        if a != b && !shown.is_empty() {
            let (sa, sb) = (x_at(e.display_index(a)), x_at(e.display_index(b)));
            if let Some(sel) = Rect::from_xywh(x + sa, y, sb - sa, layout.height()).and_then(|s| s.intersect(&clip)) {
                fill_rect(pm, sel, look.selection_bg, ts);
            }
        }
        for sh in &shadows {
            super::node::paint_text_shadow(pm, &layout, x, y, sh, ts);
        }
        text::draw(pm, &layout, x, y, color, ts);
        if a != b && !shown.is_empty() {
            // selected glyphs in the selection color, clipped to the selection
            let (sa, sb) = (x_at(e.display_index(a)), x_at(e.display_index(b)));
            if let Some(sel) = Rect::from_xywh(x + sa, y, sb - sa, layout.height()) {
                let mut mask = tiny_skia::Mask::new(pm.width(), pm.height()).unwrap();
                mask.fill_path(&tiny_skia::PathBuilder::from_rect(sel), tiny_skia::FillRule::Winding, false, ts);
                let mut layer = tiny_skia::Pixmap::new(pm.width(), pm.height()).unwrap();
                text::draw(&mut layer.as_mut(), &layout, x, y, look.selection_fg, ts);
                pm.draw_pixmap(0, 0, layer.as_ref(), &tiny_skia::PixmapPaint::default(), Transform::identity(), Some(&mask));
            }
        }
        if look.caret && a == b {
            // GTK caret: 1px (aspect 0.04 of the font size, at least 1)
            let w = (font.size * 0.04).max(1.0);
            if let Some(c) = Rect::from_xywh(x + caret_x, y, w, layout.height()) {
                fill_rect(pm, c, look.color, ts);
            }
        }
    });
    let mut n = Node::new(Kind::Custom { width: 0.0, height, paint }, style);
    n.hexpand = true;
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_jumps() {
        let s = "hello big world";
        assert_eq!(prev_word(s, s.len()), 10);
        assert_eq!(prev_word(s, 10), 6);
        assert_eq!(next_word(s, 0), 5);
        assert_eq!(next_word(s, 5), 9);
    }

    #[test]
    fn insert_respects_max_len_and_selection() {
        let mut e = Entry { max_len: Some(5), ..Default::default() };
        e.insert("abcdefg");
        assert_eq!(e.text, "abcde");
        e.anchor = 1;
        e.cursor = 3;
        e.insert("X");
        assert_eq!(e.text, "aXde");
        assert_eq!(e.cursor, 2);
    }
}
