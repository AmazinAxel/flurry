// Screenshot: every output frozen (its screencopy shown as an overlay's buffer) under a tint,
// drag a region, or click or press Space for the whole monitor; PNG to the clipboard. Esc or any
// other button cancels. The tint and the selection are viewport-stretched single pixels: dragging
// moves subsurfaces and paints nothing.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::lib::colorScheme::dark;
use crate::lib::style::blue3;
use crate::ui::node::{Node, Style};
use crate::ui::runtime;
use crate::ui::wayland::{self, Anchor, Capture, Keymode, Layer, PointerEvent, Surface};
use crate::ui::window::Spec;

/// light mode lightens, dark mode darkens
const TINT_LIGHT: [f32; 4] = [1.0, 1.0, 1.0, 0.25];
const TINT_DARK: [f32; 4] = [0.0, 0.0, 0.0, 0.4];
/// the focused sway window's border (nord10, 3px)
const BORDER: i32 = 3;

struct Shot {
    surface: Rc<Surface>,
    /// None until its capture lands (the surface is held unmapped till then)
    cap: Option<Capture>,
}

thread_local! {
    /// the session's outputs; empty when none is running
    static SHOTS: RefCell<Vec<Shot>> = RefCell::default();
    /// output index, press point, current point
    static DRAG: Cell<Option<(usize, (f64, f64), (f64, f64))>> = const { Cell::new(None) };
    /// output the pointer was last over (Space takes it)
    static HOVER: Cell<usize> = const { Cell::new(0) };
    /// a selection redraw is queued for the next idle pass
    static QUEUED: Cell<bool> = const { Cell::new(false) };
    /// bumped per session: a closed one's late captures are dropped
    static SESSION: Cell<u32> = const { Cell::new(0) };
}

/// One session at a time: a press while one runs does nothing.
pub fn screenshot() {
    if SHOTS.with(|s| !s.borrow().is_empty()) {
        return;
    }
    let outs = wayland::outputs();
    if outs.is_empty() {
        return;
    }
    let spec = Spec { name: "screenshot", layer: Layer::Overlay, anchor: Anchor::all(), keymode: Keymode::Exclusive, exclusive: -1, ..Default::default() };
    // the overlays' configure round trip runs alongside the captures
    let shots: Vec<Shot> = outs
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let s = Rc::new(Surface::layer(&spec, Some(&o.wl), Rc::new(|| Node::label(Style::default(), ""))));
            s.hold();
            s.track_pointer();
            s.on_pointer(move |ev| pointer(i, ev));
            s.on_key(|k| match k.sym {
                xkbcommon::xkb::Keysym::Escape => close(),
                xkbcommon::xkb::Keysym::space => take(HOVER.get(), None),
                _ => {}
            });
            s.on_closed(close);
            Shot { surface: s, cap: None }
        })
        .collect();
    SHOTS.with(|s| *s.borrow_mut() = shots);
    DRAG.set(None);
    HOVER.set(0);
    let session = SESSION.get().wrapping_add(1);
    SESSION.set(session);
    let left = Rc::new(Cell::new(outs.len()));
    for (i, o) in outs.iter().enumerate() {
        let left = left.clone();
        wayland::capture(&o.wl, move |cap| {
            if SESSION.get() != session {
                return;
            }
            SHOTS.with(|s| {
                if let Some(shot) = s.borrow_mut().get_mut(i) {
                    shot.cap = cap;
                }
            });
            left.set(left.get() - 1);
            if left.get() == 0 {
                freeze();
            }
        });
    }
}

/// Every capture is in: all outputs freeze in the same frame.
fn freeze() {
    let ready: Vec<Rc<Surface>> = SHOTS.with(|s| {
        s.borrow()
            .iter()
            .filter_map(|shot| {
                let cap = shot.cap.as_ref()?;
                shot.surface.set_image(cap.image.clone());
                Some(shot.surface.clone())
            })
            .collect()
    });
    if ready.is_empty() {
        return close();
    }
    for s in ready {
        if s.size().0 > 0 {
            s.set_rects(&rects(s.size(), None));
        } else {
            // captured before the configure: tinted in the same flush as the first frame
            let w = Rc::downgrade(&s);
            s.on_commit(move || {
                if let Some(s) = w.upgrade() {
                    s.set_rects(&rects(s.size(), None));
                }
            });
        }
        s.release();
    }
}

fn close() {
    DRAG.set(None);
    // dropped after the borrow: the surfaces' teardown may call back in
    let gone = SHOTS.with(|s| s.take());
    drop(gone);
}

/// Logical selection rect from two points.
fn sel((x0, y0): (f64, f64), (x1, y1): (f64, f64)) -> (i32, i32, i32, i32) {
    let (l, t) = (x0.min(x1).floor() as i32, y0.min(y1).floor() as i32);
    (l, t, x0.max(x1).ceil() as i32 - l, y0.max(y1).ceil() as i32 - t)
}

/// Tint around the selection (all of it without one), then the border around it. Clipped to the
/// surface: past its edge a subsurface would show on the neighbouring output.
fn rects((w, h): (u32, u32), s: Option<(i32, i32, i32, i32)>) -> [([f32; 4], Option<(i32, i32, i32, i32)>); 8] {
    let tint = if dark().peek() { TINT_DARK } else { TINT_LIGHT };
    let b = blue3();
    let line = [b.red(), b.green(), b.blue(), 1.0];
    let (w, h) = (w as i32, h as i32);
    let clip = |(x, y, rw, rh): (i32, i32, i32, i32)| {
        let (l, t) = (x.max(0), y.max(0));
        Some((l, t, (x + rw).min(w) - l, (y + rh).min(h) - t))
    };
    // clamped: the pointer can drag past the edge
    let Some((x, y, sw, sh)) = s.map(|(x, y, sw, sh)| {
        let (l, t) = (x.clamp(0, w), y.clamp(0, h));
        (l, t, (x + sw).clamp(0, w) - l, (y + sh).clamp(0, h) - t)
    }) else {
        return [(tint, Some((0, 0, w, h))), (tint, None), (tint, None), (tint, None), (line, None), (line, None), (line, None), (line, None)];
    };
    let o = BORDER;
    [
        (tint, Some((0, 0, w, y))),
        (tint, Some((0, y + sh, w, h - y - sh))),
        (tint, Some((0, y, x, sh))),
        (tint, Some((x + sw, y, w - x - sw, sh))),
        (line, clip((x - o, y - o, sw + 2 * o, o))),
        (line, clip((x - o, y + sh, sw + 2 * o, o))),
        (line, clip((x - o, y, o, sh))),
        (line, clip((x + sw, y, o, sh))),
    ]
}

fn surface(i: usize) -> Option<Rc<Surface>> {
    SHOTS.with(|s| s.borrow().get(i).map(|s| s.surface.clone()))
}

/// The selection follows the pointer once per loop pass, however many motions came in.
fn queue_selection() {
    if QUEUED.replace(true) {
        return;
    }
    runtime::idle(|| {
        QUEUED.set(false);
        if let Some((i, p0, p1)) = DRAG.get()
            && let Some(s) = surface(i)
        {
            s.set_rects(&rects(s.size(), Some(sel(p0, p1))));
        }
    });
}

fn pointer(i: usize, ev: PointerEvent) {
    match ev {
        PointerEvent::Motion { x, y } => {
            HOVER.set(i);
            if let Some((at, p0, _)) = DRAG.get()
                && at == i
            {
                DRAG.set(Some((at, p0, (x, y))));
                queue_selection();
            }
        }
        PointerEvent::Button { x, y, left: true, pressed: true } => {
            DRAG.set(Some((i, (x, y), (x, y))));
            queue_selection();
        }
        PointerEvent::Button { left: true, pressed: false, .. } => {
            // the press's output: the implicit grab may still end over another
            if let Some((at, p0, p1)) = DRAG.take() {
                // a click (no drag): the whole monitor
                let r = sel(p0, p1);
                take(at, (r.2 > 1 || r.3 > 1).then_some(r));
            }
        }
        PointerEvent::Button { pressed: true, .. } => close(),
        _ => {}
    }
}

/// Unfreezes at once; once that's sent, crops output `i`'s frame to `region` (logical; None: all
/// of it) and encodes off the loop, then copies.
fn take(i: usize, region: Option<(i32, i32, i32, i32)>) {
    let shot = SHOTS.with(|s| {
        let mut s = s.borrow_mut();
        let size = s.get(i)?.surface.size();
        Some((s.get_mut(i)?.cap.take()?, size))
    });
    close();
    let Some((cap, (sw, sh))) = shot else { return };
    runtime::after_flush(move || {
        let Some((w, h, rgb)) = crop(&cap, (sw, sh), region) else { return };
        drop(cap);
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
    });
}

/// `region` (logical, of a `sw`×`sh` surface) of the frame as packed RGB.
fn crop(cap: &Capture, (sw, sh): (u32, u32), region: Option<(i32, i32, i32, i32)>) -> Option<(u32, u32, Vec<u8>)> {
    let (iw, ih) = (cap.image.width, cap.image.height);
    let (l, t, r, b) = match region {
        None => (0, 0, iw, ih),
        Some((x, y, w, h)) => {
            let (fx, fy) = (iw as f64 / sw.max(1) as f64, ih as f64 / sh.max(1) as f64);
            let px = |v: i32, f: f64, max: u32| ((v as f64 * f).round().max(0.0) as u32).min(max);
            (px(x, fx, iw), px(y, fy, ih), px(x + w, fx, iw), px(y + h, fy, ih))
        }
    };
    let (w, h) = (r.checked_sub(l).filter(|&w| w > 0)?, b.checked_sub(t).filter(|&h| h > 0)?);
    let src = cap.image.bytes();
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    for y in t..b {
        let row = if cap.y_invert { ih - 1 - y } else { y } as usize;
        let start = (row * iw as usize + l as usize) * 4;
        for p in src[start..start + w as usize * 4].as_chunks::<4>().0 {
            rgb.extend_from_slice(&if cap.bgr { [p[2], p[1], p[0]] } else { [p[0], p[1], p[2]] });
        }
    }
    Some((w, h, rgb))
}
