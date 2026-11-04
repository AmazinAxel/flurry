// Reactive state: create, peek, subscribe.

use std::cell::RefCell;
use std::rc::Rc;

type Sub = Rc<dyn Fn()>;

struct Inner<T> {
    value: RefCell<T>,
    subs: RefCell<Vec<Sub>>,
}

pub struct State<T>(Rc<Inner<T>>);

impl<T> Clone for State<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Clone + PartialEq + 'static> State<T> {
    pub fn new(value: T) -> Self {
        Self(Rc::new(Inner { value: RefCell::new(value), subs: RefCell::new(vec![]) }))
    }

    pub fn peek(&self) -> T {
        self.0.value.borrow().clone()
    }

    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.0.value.borrow())
    }

    /// Sets and notifies subscribers if the value changed.
    pub fn set(&self, value: T) {
        if *self.0.value.borrow() == value {
            return;
        }
        *self.0.value.borrow_mut() = value;
        self.notify();
    }

    /// Runs subscribers even if the value didn't change.
    pub fn notify(&self) {
        let subs: Vec<Sub> = self.0.subs.borrow().clone();
        for f in subs {
            f();
        }
    }

    /// Mutates in place; `f` returns whether it changed the value, and subscribers run only then.
    pub fn update(&self, f: impl FnOnce(&mut T) -> bool) {
        let changed = f(&mut self.0.value.borrow_mut());
        if changed {
            self.notify();
        }
    }

    /// Calls `f` after every change.
    pub fn subscribe(&self, f: impl Fn() + 'static) {
        self.0.subs.borrow_mut().push(Rc::new(f));
    }

    /// Derived state that follows this one through `f`.
    pub fn map<U: Clone + PartialEq + 'static>(&self, f: impl Fn(&T) -> U + 'static) -> State<U> {
        let out = State::new(self.with(&f));
        let (src, dst) = (self.clone(), out.clone());
        self.subscribe(move || dst.set(src.with(&f)));
        out
    }
}

/// Getters for per-thread global states: `global_state! { pub fn name() -> T = init; }`.
macro_rules! global_state {
    ($($(#[$m:meta])* $vis:vis fn $name:ident() -> $t:ty = $init:expr;)*) => {$(
        $(#[$m])*
        $vis fn $name() -> $crate::ui::state::State<$t> {
            thread_local! {
                static S: $crate::ui::state::State<$t> = $crate::ui::state::State::new($init);
            }
            S.with(Clone::clone)
        }
    )*};
}
pub(crate) use global_state;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn set_notifies_only_on_change() {
        let s = State::new(1);
        let n = Rc::new(Cell::new(0));
        let m = n.clone();
        s.subscribe(move || m.set(m.get() + 1));
        s.set(1);
        s.set(2);
        assert_eq!(n.get(), 1);
    }

    #[test]
    fn notify_runs_without_change() {
        let s = State::new(true);
        let n = Rc::new(Cell::new(0));
        let m = n.clone();
        s.subscribe(move || m.set(m.get() + 1));
        s.notify();
        s.notify();
        assert_eq!(n.get(), 2);
    }

    #[test]
    fn map_follows() {
        let a = State::new(2);
        let doubled = a.map(|v| v * 2);
        a.set(5);
        assert_eq!(doubled.peek(), 10);
    }
}
