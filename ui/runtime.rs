// Single-threaded event loop: one poll() over registered fds, a timer heap, an idle queue,
// and a tiny executor so async code ports 1:1 from the TS promises. Nothing wakes up unless an fd
// is readable or a timer is due.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::os::fd::RawFd;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

type Callback = Rc<dyn Fn()>;
type Task = Pin<Box<dyn Future<Output = ()>>>;

#[derive(Default)]
struct Runtime {
    fds: RefCell<BTreeMap<RawFd, Callback>>,
    timers: RefCell<BTreeMap<(Instant, u64), Callback>>,
    timer_seq: Cell<u64>,
    idle: RefCell<VecDeque<Box<dyn FnOnce()>>>,
    tasks: RefCell<BTreeMap<u64, Task>>,
    task_seq: Cell<u64>,
    quit: Cell<bool>,
    before_poll: RefCell<Vec<Callback>>,
    after_flush: RefCell<Vec<Box<dyn FnOnce()>>>,
}

thread_local! {
    static RT: Runtime = Runtime::default();
}

// Woken task ids. Wakers never cross threads: worker threads (PAM) report back through a pipe.
thread_local! {
    static READY: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

struct TaskWaker(u64);
impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        READY.with(|r| r.borrow_mut().push(self.0));
    }
}

/// Calls `f` whenever `fd` is readable (or hung up), until `unwatch`.
pub fn watch(fd: RawFd, f: impl Fn() + 'static) {
    RT.with(|rt| rt.fds.borrow_mut().insert(fd, Rc::new(f)));
}

pub fn unwatch(fd: RawFd) {
    RT.with(|rt| rt.fds.borrow_mut().remove(&fd));
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timer(Instant, u64);

impl Timer {
    pub fn cancel(self) {
        RT.with(|rt| rt.timers.borrow_mut().remove(&(self.0, self.1)));
    }
}

/// Runs `f` once after `ms` milliseconds.
pub fn timeout(ms: u64, f: impl Fn() + 'static) -> Timer {
    RT.with(|rt| {
        let seq = rt.timer_seq.get() + 1;
        rt.timer_seq.set(seq);
        let at = Instant::now() + Duration::from_millis(ms);
        rt.timers.borrow_mut().insert((at, seq), Rc::new(f));
        Timer(at, seq)
    })
}

/// Queues `f` to run after the current dispatch (GLib idle_add equivalent).
pub fn idle(f: impl FnOnce() + 'static) {
    RT.with(|rt| rt.idle.borrow_mut().push_back(Box::new(f)));
}

pub fn spawn(fut: impl Future<Output = ()> + 'static) {
    RT.with(|rt| {
        let id = rt.task_seq.get() + 1;
        rt.task_seq.set(id);
        rt.tasks.borrow_mut().insert(id, Box::pin(fut));
        READY.with(|r| r.borrow_mut().push(id));
    });
}

/// Runs `f` right before every blocking poll (e.g. flushing the Wayland connection).
/// Runs `f` once everything queued so far has been flushed (before_poll hooks), ahead of waiting.
pub fn after_flush(f: impl FnOnce() + 'static) {
    RT.with(|rt| rt.after_flush.borrow_mut().push(Box::new(f)));
}

pub fn before_poll(f: impl Fn() + 'static) {
    RT.with(|rt| rt.before_poll.borrow_mut().push(Rc::new(f)));
}


fn poll_tasks() {
    loop {
        let ready: Vec<u64> = READY.with(|r| std::mem::take(&mut *r.borrow_mut()));
        if ready.is_empty() {
            return;
        }
        for id in ready {
            let Some(mut task) = RT.with(|rt| rt.tasks.borrow_mut().remove(&id)) else { continue };
            let waker = Waker::from(Arc::new(TaskWaker(id)));
            let mut pending = true;
            guard(|| pending = task.as_mut().poll(&mut Context::from_waker(&waker)).is_pending());
            if pending {
                RT.with(|rt| rt.tasks.borrow_mut().insert(id, task));
            }
        }
    }
}

fn run_idle() {
    while let Some(f) = RT.with(|rt| rt.idle.borrow_mut().pop_front()) {
        guard(f);
        poll_tasks();
    }
}

fn run_due_timers() {
    let now = Instant::now();
    loop {
        let due = RT.with(|rt| {
            let mut timers = rt.timers.borrow_mut();
            let key = *timers.keys().next().filter(|(at, _)| *at <= now)?;
            timers.remove(&key)
        });
        let Some(f) = due else { return };
        guard(|| f());
        poll_tasks();
    }
}

/// Runs until `quit()`.
pub fn run() {
    while !RT.with(|rt| rt.quit.get()) {
        iterate(true);
    }
}

/// One dispatch pass; `block` waits for the next fd event or timer.
pub fn iterate(block: bool) {
    poll_tasks();
    run_idle();
    run_due_timers();
    run_idle();

    let pending = RT.with(|rt| !rt.idle.borrow().is_empty()) || READY.with(|r| !r.borrow().is_empty());
    let timeout_ms = if !block || pending {
        0
    } else {
        RT.with(|rt| match rt.timers.borrow().keys().next() {
            Some((at, _)) => at.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32,
            None => -1,
        })
    };
    let hooks: Vec<Callback> = RT.with(|rt| rt.before_poll.borrow().clone());
    for h in hooks {
        h();
    }
    // work deferred until what's queued (a first frame) has gone out; then straight round again,
    // since it may have queued more
    let deferred = RT.with(|rt| std::mem::take(&mut *rt.after_flush.borrow_mut()));
    if !deferred.is_empty() {
        for f in deferred {
            guard(f);
        }
        return;
    }
    let mut fds: Vec<libc::pollfd> = RT.with(|rt| {
        rt.fds.borrow().keys().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect()
    });
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if n > 0 {
        for p in fds.iter().filter(|p| p.revents != 0) {
            if let Some(f) = RT.with(|rt| rt.fds.borrow().get(&p.fd).cloned()) {
                guard(|| f());
                poll_tasks();
            }
        }
    }
    run_due_timers();
    run_idle();
}

/// Future resolving after `ms` milliseconds.
#[cfg(test)]
pub fn sleep(ms: u64) -> impl Future<Output = ()> {
    let (tx, rx) = oneshot();
    timeout(ms, move || tx.send(()));
    async move { rx.await.unwrap_or(()) }
}

/// Single-value channel; the receiver resolves to None if the sender is dropped unsent.
pub fn oneshot<T: 'static>() -> (Sender<T>, Receiver<T>) {
    let shared = Rc::new(Shared { value: RefCell::new(None), waker: RefCell::new(None), closed: Cell::new(false) });
    (Sender(Some(shared.clone())), Receiver(shared))
}

struct Shared<T> {
    value: RefCell<Option<T>>,
    waker: RefCell<Option<Waker>>,
    closed: Cell<bool>,
}

pub struct Sender<T>(Option<Rc<Shared<T>>>);

impl<T> Sender<T> {
    pub fn send(&self, v: T) {
        if let Some(s) = &self.0 {
            *s.value.borrow_mut() = Some(v);
            s.closed.set(true);
            if let Some(w) = s.waker.borrow_mut().take() {
                w.wake();
            }
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if let Some(s) = self.0.take()
            && !s.closed.replace(true)
            && let Some(w) = s.waker.borrow_mut().take()
        {
            w.wake();
        }
    }
}

pub struct Receiver<T>(Rc<Shared<T>>);

impl<T> Future for Receiver<T> {
    type Output = Option<T>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        if let Some(v) = self.0.value.borrow_mut().take() {
            return Poll::Ready(Some(v));
        }
        if self.0.closed.get() {
            return Poll::Ready(None);
        }
        *self.0.waker.borrow_mut() = Some(cx.waker().clone());
        Poll::Pending
    }
}

/// Future resolving once `fd` is readable.
pub fn readable(fd: RawFd) -> impl Future<Output = ()> {
    let (tx, rx) = oneshot();
    watch(fd, move || {
        unwatch(fd);
        tx.send(());
    });
    async move { rx.await.unwrap_or(()) }
}

/// Runs CPU/disk-bound `f` (image decodes, directory walks) on a worker thread so the loop never
/// stalls; resolves when it finishes (None if it panicked). The pipe's hangup wakes the loop.
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Some(f());
    }
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
        drop(wr);
    });
    readable(rd.as_raw_fd()).await;
    rx.try_recv().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(until: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end {
            iterate(false);
            if until() {
                return;
            }
        }
        panic!("did not finish");
    }

    #[test]
    fn timers_fire_in_order_and_cancel() {
        let log = Rc::new(RefCell::new(vec![]));
        let (a, b, c) = (log.clone(), log.clone(), log.clone());
        timeout(20, move || a.borrow_mut().push(2));
        timeout(5, move || b.borrow_mut().push(1));
        timeout(10, move || c.borrow_mut().push(9)).cancel();
        drive(|| log.borrow().len() == 2);
        assert_eq!(*log.borrow(), vec![1, 2]);
    }

    #[test]
    fn async_sleep_and_oneshot() {
        let done = Rc::new(Cell::new(0));
        let d = done.clone();
        let (tx, rx) = oneshot::<u32>();
        spawn(async move {
            sleep(5).await;
            d.set(rx.await.unwrap());
        });
        timeout(10, move || tx.send(7));
        drive(|| done.get() == 7);
    }

    #[test]
    fn dropped_sender_resolves_none() {
        let got = Rc::new(Cell::new(Some(1)));
        let g = got.clone();
        let (tx, rx) = oneshot::<u32>();
        spawn(async move { g.set(rx.await) });
        drop(tx);
        drive(|| got.get().is_none());
    }
}

/// FIFO async lock: serializes async jobs (the TS promise queues) without polling.
#[derive(Default)]
pub struct Queue {
    busy: Cell<bool>,
    waiters: RefCell<VecDeque<Sender<()>>>,
}

pub struct QueueGuard<'a>(&'a Queue);

impl Queue {
    pub async fn lock(&self) -> QueueGuard<'_> {
        if self.busy.replace(true) {
            let (tx, rx) = oneshot();
            self.waiters.borrow_mut().push_back(tx);
            rx.await;
        }
        QueueGuard(self)
    }
}

impl Drop for QueueGuard<'_> {
    fn drop(&mut self) {
        match self.0.waiters.borrow_mut().pop_front() {
            // ownership passes straight to the next waiter
            Some(tx) => tx.send(()),
            None => self.0.busy.set(false),
        }
    }
}

/// Runs `f`, containing panics: a bug in one handler must never take the shell down (a dead
/// lockscreen client leaves sway's red "locked" screen).
pub fn guard(f: impl FnOnce()) {
    if let Err(e) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        let msg = e.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| e.downcast_ref::<String>().cloned()).unwrap_or_default();
        eprintln!("flurry: handler panicked: {msg}");
    }
}

