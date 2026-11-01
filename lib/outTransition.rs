// Flash windows (osd, workspaces): shown instantly, hidden after `ms` with a slide-out
// (GTK revealer, eased by cubic-bezier(0, 0.6, 0.3, 1) reversed: slow start, fast end) stepped on frame callbacks while animating.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use crate::ui::runtime;
use crate::ui::window::Win;

/// CSS cubic-bezier(0, 0.6, 0.3, 1): fast start, long gentle end.
fn ease(x: f32) -> f32 {
    const X1: f32 = 0.0;
    const Y1: f32 = 0.6;
    const X2: f32 = 0.3;
    const Y2: f32 = 1.0;
    if x >= 1.0 {
        return 1.0;
    }
    // exact at the start too: bisection lands near t=0 but not on it, so the hide never reached 0
    // and the window kept animating (and stayed mapped) forever
    if x <= 0.0 {
        return 0.0;
    }
    let bez = |t: f32, a: f32, b: f32| 3.0 * (1.0 - t) * (1.0 - t) * t * a + 3.0 * (1.0 - t) * t * t * b + t * t * t;
    // x(t) is monotonic: bisect for the t that gives x
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..20 {
        let mid = (lo + hi) / 2.0;
        if bez(mid, X1, X2) < x { lo = mid } else { hi = mid }
    }
    bez((lo + hi) / 2.0, Y1, Y2)
}

pub struct Flash {
    ms: u64,
    duration: f32,
    /// the pending hide, re-armed by every show
    timer: RefCell<Option<runtime::Timer>>,
    hiding: Cell<Option<Instant>>,
}

impl Flash {
    pub fn new(ms: u64, duration_ms: f32) -> Rc<Flash> {
        Rc::new(Flash { ms, duration: duration_ms, timer: RefCell::new(None), hiding: Cell::new(None) })
    }

    /// 1.0 fully revealed, 0.0 hidden.
    pub fn progress(&self) -> f32 {
        match self.hiding.get() {
            None => 1.0,
            Some(t) => {
                let x = (t.elapsed().as_secs_f32() * 1000.0 / self.duration).min(1.0);
                ease(1.0 - x)
            }
        }
    }

    pub fn show(self: &Rc<Self>, win: &Rc<Win>) {
        win.show();
        if self.hiding.take().is_some() {
            // shown again mid slide-out: the old content must not stay up for another frame
            win.redraw_now();
        } else {
            win.redraw();
        }
        if let Some(t) = self.timer.borrow_mut().take() {
            t.cancel();
        }
        let (f, w) = (self.clone(), Rc::downgrade(win));
        let t = runtime::timeout(self.ms, move || {
            f.timer.borrow_mut().take();
            if let Some(w) = w.upgrade() {
                f.hiding.set(Some(Instant::now()));
                f.animate(w);
            }
        });
        *self.timer.borrow_mut() = Some(t);
    }

    fn animate(self: &Rc<Self>, win: Rc<Win>) {
        let (f, w) = (self.clone(), Rc::downgrade(&win));
        win.animate(move || {
            if f.hiding.get().is_none() {
                return false; // shown again
            }
            if f.progress() <= 0.0 {
                f.hiding.set(None);
                if let Some(w) = w.upgrade() {
                    w.hide();
                }
                return false;
            }
            true
        });
    }
}
