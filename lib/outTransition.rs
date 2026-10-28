// Flash windows (osd, workspaces): shown instantly, hidden after `ms` with a slide-out
// (GTK revealer, eased quintic-out per request) stepped on frame callbacks while animating.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use crate::ui::runtime;
use crate::ui::window::Win;

pub struct Flash {
    ms: u64,
    duration: f32,
    count: Cell<u32>,
    hiding: Cell<Option<Instant>>,
}

impl Flash {
    pub fn new(ms: u64, duration_ms: f32) -> Rc<Flash> {
        Rc::new(Flash { ms, duration: duration_ms, count: Cell::new(0), hiding: Cell::new(None) })
    }

    /// 1.0 fully revealed, 0.0 hidden.
    pub fn progress(&self) -> f32 {
        match self.hiding.get() {
            None => 1.0,
            Some(t) => {
                let x = (t.elapsed().as_secs_f32() * 1000.0 / self.duration).min(1.0);
                (1.0 - x).powi(5) // quintic ease-out: fast start, gentle end
            }
        }
    }

    pub fn show(self: &Rc<Self>, win: &Rc<Win>) {
        self.hiding.set(None);
        self.count.set(self.count.get() + 1);
        win.show();
        win.redraw();
        let (f, w) = (self.clone(), Rc::downgrade(win));
        runtime::timeout(self.ms, move || {
            f.count.set(f.count.get() - 1);
            if f.count.get() == 0
                && let Some(w) = w.upgrade()
            {
                f.hiding.set(Some(Instant::now()));
                f.animate(w);
            }
        });
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
