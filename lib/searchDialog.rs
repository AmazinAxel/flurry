// Search entry + result rows dialog (lib/searchDialog.tsx), shared by launcher and pass.

use std::cell::RefCell;
use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::backgroundSection::background_section;
use crate::lib::inputControl::{EntryBinding, Opts, input_control};
use crate::lib::style::*;
use crate::ui::entry::{self, EntryRef, Look};
use crate::ui::node::{Align, Background, Node, Sides, Style, rgba};
use crate::ui::window::{self, Win};

pub struct SearchSpec<T> {
    pub placeholder: &'static str,
    pub search: Box<dyn Fn(&str) -> Vec<T>>,
    pub activate: Rc<dyn Fn(&T)>,
    pub shift_activate: Option<Rc<dyn Fn(&T)>>,
    pub row: Box<dyn Fn(&T, &Style) -> Node>,
    pub sensitive: Option<Box<dyn Fn(&T) -> bool>>,
    pub on_show: Option<Rc<dyn Fn()>>,
}

/// `.searchableDialog entry` inside a BackgroundSection header, with its primary icon.
pub fn search_entry(base: &Style, entry: &EntryRef, icon: &'static str, focused: bool) -> Node {
    let mut s = crate::lib::backgroundSection::header_style(base);
    s.font.size = 24.0;
    s.padding = Sides::trbl(0.0, 8.0, 0.0, 32.0);
    s.min_height = 36.0;
    s.outline = Some((2.0, -3.0, rgba(255, 255, 255, 0x1c as f32 / 255.0)));
    let mut img_style = s.inherit();
    img_style.margin = Sides::trbl(0.0, 6.0, 2.0, 2.0);
    let look = Look { color: s.color, placeholder: s.color, selection_bg: hex_sel(), selection_fg: sel_fg(), caret: false };
    let _ = focused;
    Node::hbox(s.clone(), 0.0, vec![
        Node::icon(img_style, icon, 16.0).align(Align::Fill, Align::Center),
        entry::text_node(entry, s.inherit(), look),
    ])
    .focusable("entry")
}

/// Graphite `entry` (passSave fields, wifi password) with flurry's square corners.
pub fn plain_entry(base: &Style, e: &EntryRef, id: &'static str) -> Node {
    let mut s = base.inherit();
    s.min_height = 36.0;
    s.padding = Sides::vh(0.0, 8.0);
    s.background = Background::Color(rgba(255, 255, 255, 0.04));
    // Graphite entry text; :focus-within never matches (GTK does not consider layer windows active)
    s.color = rgba(255, 255, 255, 0.7);
    let look = Look { color: s.color, placeholder: s.color, selection_bg: hex_sel(), selection_fg: sel_fg(), caret: false };
    Node::hbox(s.clone(), 0.0, vec![entry::text_node(e, s.inherit(), look)]).focusable(id).expand(true, false)
}

pub fn hex_sel() -> tiny_skia::Color {
    crate::ui::node::hex(0xdbdee5)
}
pub fn sel_fg() -> tiny_skia::Color {
    rgba(0, 0, 0, 0.87)
}

/// `.searchableDialog button` row, selected/disabled states.
pub fn row_style(base: &Style, selected: bool, sensitive: bool) -> Style {
    let mut s = button(base);
    s.padding = Sides::all(8.0);
    if selected {
        s = focus_outline(s);
        s.translate_y = -2.0;
    }
    if !sensitive {
        s.opacity = 0.35;
    }
    s
}

pub fn search_dialog<T: Clone + 'static>(name: &'static str, spec: SearchSpec<T>) -> Rc<Win> {
    let entry = entry::new(spec.placeholder);
    let items: Rc<RefCell<Vec<T>>> = Rc::default();
    let selected: Rc<RefCell<Option<usize>>> = Rc::default();
    let spec = Rc::new(spec);

    {
        let (items, selected, spec) = (items.clone(), selected.clone(), spec.clone());
        entry.borrow_mut().on_change = Some(Rc::new(move |text: &str| {
            let list = (spec.search)(text);
            *selected.borrow_mut() = (!list.is_empty()).then_some(0);
            *items.borrow_mut() = list;
            if let Some(w) = window::get(name) {
                w.redraw();
            }
        }));
    }

    let build = {
        let (entry, items, selected, spec) = (entry.clone(), items.clone(), selected.clone(), spec.clone());
        move |v: &window::View| {
            let base = window();
            let list = items.borrow();
            let sel = *selected.borrow();
            let rows: Vec<Node> = list
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let sensitive = spec.sensitive.as_ref().is_none_or(|f| f(item));
                    let s = row_style(&base, sel == Some(i), sensitive);
                    let inner = (spec.row)(item, &s);
                    Node::hbox(s, 0.0, vec![inner])
                })
                .collect();
            let content = Node::vbox(Style { margin: Sides::trbl(6.0, 0.0, 0.0, 0.0), ..base.inherit() }, 6.0, rows).show(!list.is_empty());
            let header = search_entry(&base, &entry, "system-search-symbolic", v.focused("entry"));
            background_section(&base, header, Some(content), 700.0, 500.0)
        }
    };

    let on_key = {
        let (items, selected, spec) = (items.clone(), selected.clone(), spec.clone());
        Rc::new(move |ev: &crate::ui::wayland::KeyEvent| {
            if let Some(sa) = &spec.shift_activate
                && matches!(ev.sym, Keysym::Return | Keysym::KP_Enter)
                && ev.shift
            {
                let top = selected.borrow().and_then(|i| items.borrow().get(i).cloned());
                if let Some(t) = top {
                    sa(&t);
                }
                return true;
            }
            // arrows, or Ctrl+j/k (plain letters go to the search entry)
            let delta = match ev.sym {
                Keysym::Down => 1,
                Keysym::Up => -1,
                Keysym::j if ev.ctrl => 1,
                Keysym::k if ev.ctrl => -1,
                _ => return false,
            };
            let len = items.borrow().len();
            if len > 0 {
                let cur = selected.borrow().map(|i| i as i64).unwrap_or(-1);
                *selected.borrow_mut() = Some(crate::lib::wrapFocus::wrap_index(cur, delta, len as i64) as usize);
            }
            true
        })
    };

    let activate = {
        let (items, selected, spec) = (items.clone(), selected.clone(), spec.clone());
        Rc::new(move || {
            let top = selected.borrow().and_then(|i| items.borrow().get(i).cloned());
            if let Some(t) = top {
                (spec.activate)(&t);
            }
        })
    };

    input_control(name, build, Opts {
        on_show: spec.on_show.clone(),
        on_key: Some(on_key),
        focus: Some("entry"),
        entries: vec![EntryBinding { id: "entry", entry, on_activate: Some(activate), on_key: None }],
        ..Default::default()
    })
}

#[allow(dead_code)]
fn unused(_: Background) {}
