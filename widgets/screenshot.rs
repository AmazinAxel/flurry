// Screenshot: every output frozen (screencopy shown as each overlay's backdrop), drag a region or
// click for the whole monitor, PNG to the clipboard. Esc or any other button cancels.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::lib::style::blue2;
use crate::ui::node::{Align, Node, Sides, Style, rgba};
use crate::ui::runtime;
use crate::ui::wayland::{self, Anchor, Capture, Keymode, Layer, PointerEvent, Surface};
use crate::ui::window::Spec;

thread_local! {
    /// the frozen outputs, while selecting
    static SHOTS: RefCell<Vec<(Rc<Surface>, Capture)>> = RefCell::default();
    /// captures still in flight
    static WAITING: Cell<bool> = const { Cell::new(false) };
    /// output index, press point, current point
    static DRAG: Cell<Option<(usize, (f64, f64), (f64, f64))>> = const { Cell::new(None) };
}

pub fn screenshot() {
    if WAITING.get() || SHOTS.with(|s| !s.borrow().is_empty()) {
        return;
    }
    let outs = wayland::outputs();
    if outs.is_empty() {
        return;
    }
    WAITING.set(true);
    let n = outs.len();
    let got: Rc<RefCell<Vec<(wayland::Output, Capture)>>> = Rc::default();
    let left = Rc::new(Cell::new(n));
    for o in outs {
        let (got, left) = (got.clone(), left.clone());
        let out = o.clone();
        wayland::capture(&o.wl, move |cap| {
            if let Some(c) = cap {
                got.borrow_mut().push((out, c));
            }
            left.set(left.get() - 1);
            if left.get() == 0 {
                WAITING.set(false);
                freeze(got.take());
            }
        });
    }
}

/// Maps the frozen frames together, so every output freezes in the same frame.
fn freeze(caps: Vec<(wayland::Output, Capture)>) {
    let spec = Spec {
        name: "screenshot",
        layer: Layer::Overlay,
        anchor: Anchor::all(),
        keymode: Keymode::Exclusive,
        exclusive: -1,
        dim: Some([0.0; 4]),
        ..Default::default()
    };
    let shots = caps
        .into_iter()
        .enumerate()
        .map(|(i, (out, cap))| {
            let s = Rc::new(Surface::layer(&spec, Some(&out.wl), Rc::new(move || selection(i))));
            s.set_image(cap.image.clone());
            s.track_pointer();
            s.on_pointer(move |ev| pointer(i, ev));
            s.on_key(|k| {
                if k.sym == xkbcommon::xkb::Keysym::Escape {
                    close();
                }
            });
            s.on_closed(close);
            (s, cap)
        })
        .collect();
    SHOTS.with(|s| *s.borrow_mut() = shots);
}

fn close() {
    DRAG.set(None);
    // dropped after the borrow: the surfaces' teardown may call back in
    let gone = SHOTS.with(|s| s.take());
    drop(gone);
}

/// The drag rectangle on output `i`, if it's there.
fn selection(i: usize) -> Node {
    let mut kids = vec![];
    if let Some((at, (x0, y0), (x1, y1))) = DRAG.get()
        && at == i
    {
        let (x, y, w, h) = (x0.min(x1) as f32, y0.min(y1) as f32, (x1 - x0).abs() as f32, (y1 - y0).abs() as f32);
        let s = Style {
            margin: Sides::trbl(y, 0.0, 0.0, x),
            border: Sides::all(2.0),
            border_color: [blue2(); 4],
            background: Some(rgba(129, 161, 193, 0.15)),
            ..Default::default()
        };
        kids.push(Node::hbox(s, 0.0, vec![]).size(w, h).align(Align::Start, Align::Start));
    }
    Node::hbox(Style::default(), 0.0, kids).align(Align::Start, Align::Start)
}

fn redraw(i: usize) {
    if let Some(s) = SHOTS.with(|s| s.borrow().get(i).map(|(s, _)| s.clone())) {
        s.redraw();
    }
}

fn pointer(i: usize, ev: PointerEvent) {
    match ev {
        PointerEvent::Motion { x, y } => {
            if let Some((at, p0, _)) = DRAG.get()
                && at == i
            {
                DRAG.set(Some((at, p0, (x, y))));
                redraw(i);
            }
        }
        PointerEvent::Button { x, y, left: true, pressed: true } => DRAG.set(Some((i, (x, y), (x, y)))),
        PointerEvent::Button { left: true, pressed: false, .. } => {
            if let Some((at, p0, p1)) = DRAG.take()
                && at == i
            {
                take(i, p0, p1);
            }
        }
        PointerEvent::Button { pressed: true, .. } => close(),
        _ => {}
    }
}

/// Crops output `i`'s frame to the selection (a click: all of it), unfreezes, then encodes off the
/// loop and copies.
fn take(i: usize, (x0, y0): (f64, f64), (x1, y1): (f64, f64)) {
    let crop = SHOTS.with(|s| {
        let s = s.borrow();
        let (surface, cap) = s.get(i)?;
        let (sw, sh) = surface.size();
        let (iw, ih) = (cap.image.width, cap.image.height);
        let (fx, fy) = (iw as f64 / sw.max(1) as f64, ih as f64 / sh.max(1) as f64);
        let click = (x1 - x0).abs() < 2.0 && (y1 - y0).abs() < 2.0;
        let (l, t, r, b) = if click {
            (0, 0, iw, ih)
        } else {
            let px = |v: f64, f: f64, max: u32, up: bool| ((if up { (v * f).ceil() } else { (v * f).floor() }).max(0.0) as u32).min(max);
            (px(x0.min(x1), fx, iw, false), px(y0.min(y1), fy, ih, false), px(x0.max(x1), fx, iw, true), px(y0.max(y1), fy, ih, true))
        };
        let (w, h) = (r - l, b - t);
        if w == 0 || h == 0 {
            return None;
        }
        let src = cap.image.bytes();
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in t..b {
            let row = if cap.y_invert { ih - 1 - y } else { y } as usize;
            let start = (row * iw as usize + l as usize) * 4;
            for p in src[start..start + w as usize * 4].as_chunks::<4>().0 {
                if cap.bgr {
                    rgb.extend_from_slice(&[p[2], p[1], p[0]]);
                } else {
                    rgb.extend_from_slice(&p[..3]);
                }
            }
        }
        Some((w, h, rgb))
    });
    close();
    let Some((w, h, rgb)) = crop else { return };
    runtime::spawn(async move {
        let png = runtime::blocking(move || {
            let mut out = Vec::new();
            let mut e = png::Encoder::new(&mut out, w, h);
            e.set_color(png::ColorType::Rgb);
            e.set_depth(png::BitDepth::Eight);
            e.set_compression(png::Compression::Fast);
            e.write_header().ok()?.write_image_data(&rgb).ok()?;
            Some(out)
        });
        if let Some(png) = png.await.flatten() {
            crate::lib::clipboard::copy_png(png);
        }
    });
}
