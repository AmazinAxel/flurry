// Reactive state: create, peek, subscribe.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

type Sub = Rc<dyn Fn()>;

struct Inner<T> {
    value: RefCell<T>,
    subs: RefCell<Vec<(u64, Sub)>>,
    seq: Cell<u64>,
}

pub struct State<T>(Rc<Inner<T>>);

impl<T> Clone for State<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T: Clone + PartialEq + 'static> State<T> {
    pub fn new(value: T) -> Self {
        Self(Rc::new(Inner { value: RefCell::new(value), subs: RefCell::new(vec![]), seq: Cell::new(0) }))
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
        let subs: Vec<Sub> = self.0.subs.borrow().iter().map(|(_, f)| f.clone()).collect();
        for f in subs {
            f();
        }
    }

    pub fn update(&self, f: impl FnOnce(&mut T)) {
        let mut v = self.peek();
        f(&mut v);
        self.set(v);
    }

    /// Calls `f` after every change; the returned guard-less id unsubscribes via `unsubscribe`.
    pub fn subscribe(&self, f: impl Fn() + 'static) -> u64 {
        let id = self.0.seq.get() + 1;
        self.0.seq.set(id);
        self.0.subs.borrow_mut().push((id, Rc::new(f)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.0.subs.borrow_mut().retain(|(i, _)| *i != id);
    }

    /// Derived state that follows this one through `f`.
    pub fn map<U: Clone + PartialEq + 'static>(&self, f: impl Fn(&T) -> U + 'static) -> State<U> {
        let out = State::new(self.with(&f));
        let (src, dst) = (self.clone(), out.clone());
        self.subscribe(move || dst.set(src.with(&f)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_notifies_only_on_change() {
        let s = State::new(1);
        let n = Rc::new(Cell::new(0));
        let m = n.clone();
        let id = s.subscribe(move || m.set(m.get() + 1));
        s.set(1);
        s.set(2);
        assert_eq!(n.get(), 1);
        s.unsubscribe(id);
        s.set(3);
        assert_eq!(n.get(), 1);
    }

    #[test]
    fn map_follows() {
        let a = State::new(2);
        let doubled = a.map(|v| v * 2);
        a.set(5);
        assert_eq!(doubled.peek(), 10);
    }
}
