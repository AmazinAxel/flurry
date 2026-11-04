// Single-threaded event loop: one poll() over registered fds, a timer heap, an idle queue,
// and a tiny executor for async handlers. Nothing wakes up unless an fd is readable or a timer is
// due.

use std::cell::{Cell, RefCell};
use std::any::TypeId;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::future::Future;
use std::os::fd::RawFd;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

type Callback = Rc<dyn Fn()>;
type Task = (Pin<Box<dyn Future<Output = ()>>>, Waker);

#[derive(Default)]
struct Runtime {
    /// keyed by fd, tagged with the watch's generation: a stale revents for a closed fd must not
    /// reach a new watch that reused its number in the same pass
    fds: RefCell<BTreeMap<RawFd, (u64, Callback)>>,
    watch_seq: Cell<u64>,
    timers: RefCell<BTreeMap<(Instant, u64), Box<dyn FnOnce()>>>,
    timer_seq: Cell<u64>,
    idle: RefCell<VecDeque<Box<dyn FnOnce()>>>,
    /// `idle_once` jobs queued and not yet run, by the job's type
    idle_once: RefCell<HashSet<TypeId>>,
    tasks: RefCell<BTreeMap<u64, Task>>,
    task_seq: Cell<u64>,
    before_poll: RefCell<Vec<Callback>>,
    after_flush: RefCell<Vec<Box<dyn FnOnce()>>>,
    /// reused across iterations, with each entry's watch generation
    pollfds: RefCell<Vec<libc::pollfd>>,
    pollgens: RefCell<Vec<u64>>,
    /// watched fds the next poll also waits to become writable on
    poll_out: RefCell<Vec<RawFd>>,
    /// watched for POLLPRI only (sysfs attribute notifications)
    pri: RefCell<Vec<RawFd>>,
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

/// Sets or clears O_NONBLOCK on `fd`.
pub fn set_nonblocking(fd: RawFd, on: bool) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 {
            return;
        }
        let want = if on { fl | libc::O_NONBLOCK } else { fl & !libc::O_NONBLOCK };
        if want != fl {
            libc::fcntl(fd, libc::F_SETFL, want);
        }
    }
}

/// Calls `f` whenever `fd` is readable (or hung up), until `unwatch`.
pub fn watch(fd: RawFd, f: impl Fn() + 'static) {
    RT.with(|rt| {
        let seq = rt.watch_seq.get() + 1;
        rt.watch_seq.set(seq);
        rt.fds.borrow_mut().insert(fd, (seq, Rc::new(f)));
    });
}

/// Calls `f` on POLLPRI only: sysfs attributes are always readable but signal changes this way.
pub fn watch_pri(fd: RawFd, f: impl Fn() + 'static) {
    RT.with(|rt| rt.pri.borrow_mut().push(fd));
    watch(fd, f);
}

pub fn unwatch(fd: RawFd) {
    RT.with(|rt| {
        rt.fds.borrow_mut().remove(&fd);
        rt.pri.borrow_mut().retain(|&p| p != fd);
        rt.poll_out.borrow_mut().retain(|&p| p != fd);
    });
}

/// Polls also call `fd`'s watch once it's writable (a write that hit a full socket); it stays
/// armed until that happens or `unwatch`.
pub fn poll_out(fd: RawFd) {
    RT.with(|rt| {
        let mut out = rt.poll_out.borrow_mut();
        if !out.contains(&fd) {
            out.push(fd);
        }
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timer(Instant, u64);

impl Timer {
    pub fn cancel(self) {
        RT.with(|rt| rt.timers.borrow_mut().remove(&(self.0, self.1)));
    }
}

/// Runs `f` once after `ms` milliseconds.
pub fn timeout(ms: u64, f: impl FnOnce() + 'static) -> Timer {
    RT.with(|rt| {
        let seq = rt.timer_seq.get() + 1;
        rt.timer_seq.set(seq);
        let at = Instant::now() + Duration::from_millis(ms);
        rt.timers.borrow_mut().insert((at, seq), Box::new(f));
        Timer(at, seq)
    })
}

/// Queues `f` to run after the current dispatch (GLib idle_add equivalent).
pub fn idle(f: impl FnOnce() + 'static) {
    RT.with(|rt| rt.idle.borrow_mut().push_back(Box::new(f)));
}

/// Runs `f` once at the next idle pass, however often it is requested before then. Requests are
/// told apart by `f`'s type (each fn item or closure expression is its own); the first one runs.
pub fn idle_once<F: FnOnce() + 'static>(f: F) {
    // a capturing closure's later data would be dropped with its request
    const { assert!(std::mem::size_of::<F>() == 0, "idle_once takes non-capturing closures") };
    let key = TypeId::of::<F>();
    if !RT.with(|rt| rt.idle_once.borrow_mut().insert(key)) {
        return;
    }
    idle(move || {
        // cleared first: `f` may queue itself again
        RT.with(|rt| rt.idle_once.borrow_mut().remove(&key));
        f();
    });
}

pub fn spawn(fut: impl Future<Output = ()> + 'static) {
    RT.with(|rt| {
        let id = rt.task_seq.get() + 1;
        rt.task_seq.set(id);
        rt.tasks.borrow_mut().insert(id, (Box::pin(fut), Waker::from(Arc::new(TaskWaker(id)))));
        READY.with(|r| r.borrow_mut().push(id));
    });
}

/// Runs `f` once everything queued so far has been flushed (before_poll hooks), ahead of waiting.
pub fn after_flush(f: impl FnOnce() + 'static) {
    RT.with(|rt| rt.after_flush.borrow_mut().push(Box::new(f)));
}

/// Runs `f` right before every blocking poll (e.g. flushing the Wayland connection).
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
            // a task that panicked stays dropped
            let mut pending = false;
            guard(|| pending = task.0.as_mut().poll(&mut Context::from_waker(&task.1)).is_pending());
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
        guard(f);
        poll_tasks();
    }
}

pub fn run() -> ! {
    loop {
        iterate(true);
    }
}

/// One dispatch pass; `block` waits for the next fd event or timer.
pub fn iterate(block: bool) {
    poll_tasks();
    run_idle();
    run_due_timers();
    run_idle();

    // hooks added while they run go round too
    let mut i = 0;
    while let Some(h) = RT.with(|rt| rt.before_poll.borrow().get(i).cloned()) {
        guard(|| h());
        i += 1;
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
    // after the hooks: the handlers they dispatch queue idle work and timers
    let pending = RT.with(|rt| !rt.idle.borrow().is_empty()) || READY.with(|r| !r.borrow().is_empty());
    let timeout_ms = if !block || pending {
        0
    } else {
        RT.with(|rt| match rt.timers.borrow().keys().next() {
            // rounded up: a truncated wait wakes just before the timer and spins until it's due
            Some((at, _)) => at.saturating_duration_since(Instant::now()).as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32,
            None => -1,
        })
    };
    let (mut fds, gens) = RT.with(|rt| {
        let mut v = std::mem::take(&mut *rt.pollfds.borrow_mut());
        let mut g = std::mem::take(&mut *rt.pollgens.borrow_mut());
        v.clear();
        g.clear();
        let out = rt.poll_out.borrow();
        let pri = rt.pri.borrow();
        let events = |fd| {
            if pri.contains(&fd) {
                libc::POLLPRI
            } else if out.contains(&fd) {
                libc::POLLIN | libc::POLLOUT
            } else {
                libc::POLLIN
            }
        };
        for (&fd, &(gen_, _)) in rt.fds.borrow().iter() {
            v.push(libc::pollfd { fd, events: events(fd), revents: 0 });
            g.push(gen_);
        }
        (v, g)
    });
    let current = |fd: RawFd, gen_: u64| RT.with(|rt| rt.fds.borrow().get(&fd).filter(|w| w.0 == gen_).map(|w| w.1.clone()));
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if n > 0 {
        for (p, &gen_) in fds.iter().zip(&gens).filter(|(p, _)| p.revents != 0) {
            // unwatched (and maybe re-watched) by an earlier handler in this pass: stale
            let Some(f) = current(p.fd, gen_) else { continue };
            if p.revents & libc::POLLNVAL != 0 {
                // closed while still watched: it would report POLLNVAL forever
                unwatch(p.fd);
                continue;
            }
            // signalled writable (or failed): disarmed; a write that fills it again re-arms
            if p.revents & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP) != 0 {
                RT.with(|rt| rt.poll_out.borrow_mut().retain(|&o| o != p.fd));
            }
            guard(|| f());
            poll_tasks();
            // sysfs reports every change as POLLPRI|POLLERR, and a removed attribute the same way
            // forever: one that can't be read anymore is gone, stop watching it
            if p.revents & libc::POLLERR != 0 && current(p.fd, gen_).is_some() && RT.with(|rt| rt.pri.borrow().contains(&p.fd)) {
                let mut b = 0u8;
                if unsafe { libc::pread(p.fd, (&raw mut b).cast(), 1, 0) } < 0
                    && !matches!(std::io::Error::last_os_error().kind(), std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock)
                {
                    unwatch(p.fd);
                }
            }
        }
    }
    RT.with(|rt| {
        *rt.pollfds.borrow_mut() = fds;
        *rt.pollgens.borrow_mut() = gens;
    });
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
pub async fn blocking<T: Send + 'static, F: FnOnce() -> T + Send + 'static>(f: F) -> Option<T> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let inline = |f: F| std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok();
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return inline(f);
    }
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // shared so a refused spawn (thread limit) hands `f` back to run here
    let job = Arc::new(std::sync::Mutex::new(Some(f)));
    let theirs = job.clone();
    let spawned = std::thread::Builder::new().spawn(move || {
        let f = theirs.lock().ok().and_then(|mut j| j.take());
        let _wr = wr;
        f.map(|f| f())
    });
    let handle = match spawned {
        Ok(h) => h,
        Err(e) => {
            eprintln!("flurry: thread spawn failed, running inline: {e}");
            return job.lock().ok().and_then(|mut j| j.take()).and_then(inline);
        }
    };
    readable(rd.as_raw_fd()).await;
    // the hangup comes as the thread finishes
    handle.join().ok().flatten()
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

    #[test]
    fn queue_skips_dropped_waiters() {
        let q = Queue::default();
        let first = Box::pin(q.lock());
        let mut cx = Context::from_waker(Waker::noop());
        let mut first = first;
        let Poll::Ready(g) = first.as_mut().poll(&mut cx) else { panic!("free queue") };
        let mut second = Box::pin(q.lock());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        drop(second);
        drop(g);
        assert!(!q.busy.get(), "dropped waiter must not keep the queue busy");
        // handed the lock, then dropped before taking it
        let Poll::Ready(g) = Box::pin(q.lock()).as_mut().poll(&mut cx) else { panic!("free queue") };
        let mut third = Box::pin(q.lock());
        assert!(third.as_mut().poll(&mut cx).is_pending());
        drop(g);
        drop(third);
        assert!(!q.busy.get());
    }

    #[test]
    fn idle_once_coalesces() {
        thread_local! {
            static RUNS: Cell<u32> = const { Cell::new(0) };
        }
        fn bump() {
            RUNS.with(|r| r.set(r.get() + 1));
        }
        idle_once(bump);
        idle_once(bump);
        drive(|| RUNS.with(Cell::get) == 1);
        // queued again once it has run
        idle_once(bump);
        drive(|| RUNS.with(Cell::get) == 2);
    }

    #[test]
    fn panicked_task_is_dropped() {
        spawn(async { panic!("boom") });
        poll_tasks();
        assert!(RT.with(|rt| rt.tasks.borrow().is_empty()));
    }
}

/// FIFO async lock: serializes async jobs without polling.
#[derive(Default)]
pub struct Queue {
    busy: Cell<bool>,
    waiters: RefCell<VecDeque<Sender<()>>>,
}

pub struct QueueGuard<'a>(&'a Queue);

/// A queued `lock` call; dropped after being handed the lock but before taking it, it passes it on.
struct Waiting<'a>(&'a Queue, Receiver<()>);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.1.0.value.borrow_mut().take().is_some() {
            self.0.unlock();
        }
    }
}

impl Queue {
    pub async fn lock(&self) -> QueueGuard<'_> {
        if self.busy.replace(true) {
            let (tx, rx) = oneshot();
            self.waiters.borrow_mut().push_back(tx);
            let mut w = Waiting(self, rx);
            (&mut w.1).await;
        }
        QueueGuard(self)
    }

    fn unlock(&self) {
        loop {
            let next = self.waiters.borrow_mut().pop_front();
            match next {
                // a waiter whose future was dropped can't take it
                Some(tx) if tx.0.as_ref().is_some_and(|s| Rc::strong_count(s) < 2) => continue,
                // ownership passes straight to the next waiter
                Some(tx) => return tx.send(()),
                None => return self.busy.set(false),
            }
        }
    }
}

impl Drop for QueueGuard<'_> {
    fn drop(&mut self) {
        self.0.unlock();
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

