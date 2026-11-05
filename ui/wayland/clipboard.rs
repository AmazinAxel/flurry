// Clipboard over wlr-data-control: pasting, the clipboard history and copying.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::Arc;

use wayland_client::protocol::wl_seat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, backend::ObjectId, delegate_noop};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as dc_device, zwlr_data_control_manager_v1 as dc_manager, zwlr_data_control_offer_v1 as dc_offer,
    zwlr_data_control_source_v1 as dc_source,
};

use super::{Wl, ctx, runtime};
use crate::ui::entry::{Secret, wipe_bytes};

/// Reads the clipboard's first line as text (Ctrl+V into entries), at most 64 KiB; nothing if the
/// owner doesn't finish within 2 s. Through data-control, so no keyboard focus is needed.
pub fn paste(cb: impl FnOnce(String) + 'static) {
    let Some((offer, mimes)) = DC.with(|d| d.borrow().selection.clone()) else { return };
    let Some(mime) = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"].into_iter().find(|m| mimes.iter().any(|x| x == m)) else {
        return;
    };
    let Some(rd) = receive(|wr| offer.receive(mime.into(), wr)) else { return };
    read_pipe(rd, 64 * 1024, true, 2000, move |data| {
        if let Some(d) = data {
            // moved, not copied: the entry wipes the whole buffer; a lossy copy leaves the original
            match String::from_utf8(d) {
                Ok(s) => cb(s),
                Err(e) => {
                    let mut d = e.into_bytes();
                    let s = String::from_utf8_lossy(&d).into_owned();
                    wipe_bytes(&mut d); // pasted text may be a secret
                    cb(s);
                }
            }
        }
    });
}

/// A pipe whose write end `send` hands to the selection's owner.
fn receive(send: impl FnOnce(std::os::fd::BorrowedFd)) -> Option<OwnedFd> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return None;
    }
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // only the read end: the owner may write with plain blocking writes
    runtime::set_nonblocking(rd.as_raw_fd(), true);
    send(wr.as_fd());
    drop(wr);
    let _ = ctx(|c| c.conn.flush());
    Some(rd)
}

/// The read end and callback of a `read_pipe`, until done or given up on.
type Pending = Rc<RefCell<Option<(OwnedFd, Box<dyn FnOnce(Option<Vec<u8>>)>)>>>;

/// A `read_pipe` in progress.
struct Reading {
    raw: std::os::fd::RawFd,
    pending: Pending,
    give_up: runtime::Timer,
}

impl Reading {
    /// Stops it (if still running): the callback gets None.
    fn cancel(self) {
        let taken = self.pending.borrow_mut().take();
        if let Some((_rd, cb)) = taken {
            runtime::unwatch(self.raw);
            self.give_up.cancel();
            cb(None);
        }
    }
}

/// Wiped when dropped unhanded (a read that failed or was given up on).
struct Bytes(Vec<u8>);

impl Drop for Bytes {
    fn drop(&mut self) {
        wipe_bytes(&mut self.0);
    }
}

/// Reads `rd` to its end on the loop (`line`: to the first newline). Past `max` bytes a line is cut
/// there and anything else dropped (None), as is a writer that doesn't finish within `ms`.
fn read_pipe(rd: OwnedFd, max: usize, line: bool, ms: u64, cb: impl FnOnce(Option<Vec<u8>>) + 'static) -> Reading {
    const CHUNK: usize = 16384;
    /// reads per wakeup: a fast writer can't hold the loop until `max`
    const BURST: usize = 4;
    let raw = rd.as_raw_fd();
    let pending: Pending = Rc::new(RefCell::new(Some((rd, Box::new(cb)))));
    let give_up = {
        let pending = pending.clone();
        runtime::timeout(ms, move || {
            let taken = pending.borrow_mut().take();
            if let Some((_rd, cb)) = taken {
                runtime::unwatch(raw);
                cb(None);
            }
        })
    };
    // room for a whole small read up front: no reallocation leaves copies behind
    let data = RefCell::new(Bytes(Vec::with_capacity(max.min(64 * 1024) + CHUNK)));
    let p = pending.clone();
    runtime::watch(raw, move || {
        let mut data = data.borrow_mut();
        let data = &mut data.0;
        let mut buf = [0u8; CHUNK];
        let mut reads = 0;
        let ok = loop {
            if reads == BURST {
                // still watched: the rest comes on the next pass
                buf.iter_mut().for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
                return;
            }
            reads += 1;
            let n = unsafe { libc::read(raw, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                data.extend_from_slice(&buf[..n as usize]);
                if line && let Some(nl) = data.iter().position(|&b| b == b'\n') {
                    data.truncate(nl);
                    break true;
                }
                if data.len() > max {
                    data.truncate(max);
                    break line;
                }
                continue;
            }
            if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
                buf.iter_mut().for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
                return;
            }
            break n == 0;
        };
        buf.iter_mut().for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
        give_up.cancel();
        runtime::unwatch(raw);
        let Some((_rd, cb)) = p.borrow_mut().take() else { return };
        cb(ok.then(|| std::mem::take(data)));
    });
    Reading { raw, pending, give_up }
}

// ---------------------------------------------------------------- data control
// wlr-data-control sees every selection, focused or not, and owns one without a surface: pasting,
// the clipboard history and copying. Without it (another compositor) they quietly do nothing.

/// What a copy offers: each mime type with its bytes (wiped once the last copy is gone).
pub type Offers = Arc<Vec<(String, Arc<Secret>)>>;

/// Offered (with no data) alongside everything `set_selection` offers: a selection listing it is
/// flurry's own.
pub const OWN_MIME: &str = "application/x-flurry";

#[derive(Default)]
struct DataControl {
    manager: Option<dc_manager::ZwlrDataControlManagerV1>,
    device: Option<dc_device::ZwlrDataControlDeviceV1>,
    selection: Option<(dc_offer::ZwlrDataControlOfferV1, Vec<String>)>,
    /// offers whose mime types are still arriving
    offers: HashMap<ObjectId, (dc_offer::ZwlrDataControlOfferV1, Vec<String>)>,
    /// the selection flurry owns, with its token
    source: Option<(dc_source::ZwlrDataControlSourceV1, u64)>,
    tokens: u64,
    /// counts selection events: a listener call queued for an older one is skipped
    generation: u64,
}

thread_local! {
    static DC: RefCell<DataControl> = RefCell::default();
    static SELECTION_LISTENER: RefCell<Option<Rc<dyn Fn(Option<Vec<String>>)>>> = RefCell::default();
    /// the one `read_selection` in flight: a newer selection's read replaces it
    static SELECTION_READ: RefCell<Option<Reading>> = const { RefCell::new(None) };
    /// `serve`s waiting on a full pipe
    static SERVING: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn bind(globals: &wayland_client::globals::GlobalList, qh: &QueueHandle<Wl>, seat: &wl_seat::WlSeat) {
    let Ok(manager) = globals.bind::<dc_manager::ZwlrDataControlManagerV1, _, _>(qh, 1..=1, ()) else { return };
    DC.with(|d| {
        let mut d = d.borrow_mut();
        d.device = Some(manager.get_data_device(seat, qh, ()));
        d.manager = Some(manager);
    });
}

/// Calls `f` with the mime types of every new selection (flurry's own too: they list `OWN_MIME`),
/// or None once the clipboard is empty (its owner exited or cleared it), after dispatch.
pub fn on_selection(f: impl Fn(Option<Vec<String>>) + 'static) {
    SELECTION_LISTENER.with(|l| *l.borrow_mut() = Some(Rc::new(f)));
}

fn cancel_selection_read() {
    if let Some(r) = SELECTION_READ.with(|r| r.borrow_mut().take()) {
        r.cancel();
    }
}

/// Reads the current selection as `mime`: None past `max` bytes, if the owner doesn't finish
/// within `ms`, or once a newer selection comes (one read at a time).
pub fn read_selection(mime: &str, max: usize, ms: u64, cb: impl FnOnce(Option<Vec<u8>>) + 'static) {
    cancel_selection_read();
    let offer = DC.with(|d| d.borrow().selection.as_ref().map(|s| s.0.clone()));
    match offer.and_then(|o| receive(|wr| o.receive(mime.into(), wr))) {
        Some(rd) => {
            let r = read_pipe(rd, max, false, ms, cb);
            SELECTION_READ.with(|s| *s.borrow_mut() = Some(r));
        }
        None => cb(None),
    }
}

/// Owns the clipboard with `offers` (plus `OWN_MIME`) until another client takes it over. The
/// token (None without data-control) empties it again with `clear_selection`.
pub fn set_selection(offers: Offers) -> Option<u64> {
    let qh = ctx(|c| c.qh.clone());
    DC.with(|d| {
        let mut d = d.borrow_mut();
        let (manager, device) = (d.manager.clone()?, d.device.clone()?);
        let source = manager.create_data_source(&qh, offers.clone());
        for (mime, _) in offers.iter() {
            source.offer(mime.clone());
        }
        source.offer(OWN_MIME.into());
        device.set_selection(Some(&source));
        if let Some((old, _)) = d.source.take() {
            old.destroy();
        }
        d.tokens += 1;
        d.source = Some((source, d.tokens));
        Some(d.tokens)
    })
}

/// Empties the clipboard if it still holds what `set_selection` returned `token` for.
pub fn clear_selection(token: u64) {
    DC.with(|d| {
        let mut d = d.borrow_mut();
        if d.source.as_ref().is_some_and(|s| s.1 == token)
            && let (Some(device), Some((source, _))) = (d.device.clone(), d.source.take())
        {
            device.set_selection(None);
            source.destroy();
        }
    });
}

/// Writes `data` to a reader without blocking the loop; a reader that takes nothing for 10 s is
/// dropped, as is any beyond the first 8 waiting at once.
fn serve(fd: OwnedFd, data: Arc<Secret>) {
    const MAX_WAITING: usize = 8;
    const STALL_MS: u64 = 10_000;
    let raw = fd.as_raw_fd();
    runtime::set_nonblocking(raw, true);
    // true while the pipe is full
    let step = move |pos: &Cell<usize>| loop {
        let rest = &data[pos.get()..];
        if rest.is_empty() {
            return false;
        }
        let n = unsafe { libc::write(raw, rest.as_ptr().cast(), rest.len()) };
        if n > 0 {
            pos.set(pos.get() + n as usize);
            continue;
        }
        return n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock;
    };
    let pos = Cell::new(0);
    if !step(&pos) || SERVING.get() >= MAX_WAITING {
        return;
    }
    SERVING.set(SERVING.get() + 1);
    // the fd and its stall timer, until done or given up on
    let state: Rc<RefCell<Option<(OwnedFd, runtime::Timer)>>> = Rc::new(RefCell::new(None));
    let finish = move |state: &RefCell<Option<(OwnedFd, runtime::Timer)>>| {
        let taken = state.borrow_mut().take();
        if let Some((_fd, t)) = taken {
            t.cancel();
            runtime::unwatch(raw);
            SERVING.set(SERVING.get() - 1);
        }
    };
    let arm = move |state: &Rc<RefCell<Option<(OwnedFd, runtime::Timer)>>>| {
        let s = state.clone();
        runtime::timeout(STALL_MS, move || finish(&s))
    };
    *state.borrow_mut() = Some((fd, arm(&state)));
    let st = state;
    runtime::watch(raw, move || {
        let before = pos.get();
        if !step(&pos) {
            return finish(&st);
        }
        // progress: the reader gets another full stall period
        if pos.get() != before {
            let t = arm(&st);
            if let Some((_, old)) = st.borrow_mut().as_mut() {
                std::mem::replace(old, t).cancel();
            }
        }
        runtime::poll_out(raw);
    });
    runtime::poll_out(raw);
}

impl Dispatch<dc_device::ZwlrDataControlDeviceV1, ()> for Wl {
    fn event(_: &mut Self, _: &dc_device::ZwlrDataControlDeviceV1, ev: dc_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            dc_device::Event::DataOffer { id } => DC.with(|d| {
                d.borrow_mut().offers.insert(id.id(), (id, vec![]));
            }),
            dc_device::Event::Selection { id } => {
                let (mimes, generation) = DC.with(|d| {
                    let mut d = d.borrow_mut();
                    d.generation += 1;
                    if let Some((old, _)) = d.selection.take() {
                        old.destroy();
                    }
                    let mimes = id.as_ref().and_then(|o| d.offers.remove(&o.id())).map(|(_, m)| m).unwrap_or_default();
                    for (_, (o, _)) in d.offers.drain() {
                        o.destroy();
                    }
                    d.selection = id.map(|o| (o, mimes.clone()));
                    (d.selection.is_some().then_some(mimes), d.generation)
                });
                runtime::idle(move || {
                    // a read of the one before is stale now
                    cancel_selection_read();
                    // superseded within the same dispatch (an app clearing before it copies): an
                    // empty clipboard here would restore the last copy over the new one
                    if DC.with(|d| d.borrow().generation) != generation {
                        return;
                    }
                    if let Some(f) = SELECTION_LISTENER.with(|l| l.borrow().clone()) {
                        f(mimes);
                    }
                });
            }
            dc_device::Event::Finished => DC.with(|d| {
                let mut d = d.borrow_mut();
                if let Some(device) = d.device.take() {
                    device.destroy();
                }
            }),
            _ => {}
        }
    }

    wayland_client::event_created_child!(Wl, dc_device::ZwlrDataControlDeviceV1, [
        dc_device::EVT_DATA_OFFER_OPCODE => (dc_offer::ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<dc_offer::ZwlrDataControlOfferV1, ()> for Wl {
    fn event(_: &mut Self, o: &dc_offer::ZwlrDataControlOfferV1, ev: dc_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let dc_offer::Event::Offer { mime_type } = ev {
            DC.with(|d| {
                if let Some((_, m)) = d.borrow_mut().offers.get_mut(&o.id()) {
                    m.push(mime_type);
                }
            });
        }
    }
}

impl Dispatch<dc_source::ZwlrDataControlSourceV1, Offers> for Wl {
    fn event(_: &mut Self, s: &dc_source::ZwlrDataControlSourceV1, ev: dc_source::Event, offers: &Offers, _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            // an unknown type gets an empty answer (the fd closes)
            dc_source::Event::Send { mime_type, fd } => {
                if let Some((_, data)) = offers.iter().find(|(m, _)| *m == mime_type) {
                    serve(fd, data.clone());
                }
            }
            dc_source::Event::Cancelled => {
                DC.with(|d| {
                    let mut d = d.borrow_mut();
                    if d.source.as_ref().is_some_and(|x| x.0 == *s) {
                        d.source = None;
                    }
                });
                s.destroy();
            }
            _ => {}
        }
    }
}

delegate_noop!(Wl: ignore dc_manager::ZwlrDataControlManagerV1);

#[cfg(test)]
mod tests {
    use super::*;

    type Got = Rc<RefCell<Option<Option<Vec<u8>>>>>;

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        runtime::set_nonblocking(rd.as_raw_fd(), true);
        (rd, wr)
    }

    fn write_all(fd: &OwnedFd, data: &[u8]) {
        assert_eq!(unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) }, data.len() as isize);
    }

    /// Reads `rd` with `read_pipe`, driving the loop until its callback has run.
    fn read(rd: OwnedFd, max: usize, line: bool) -> Option<Vec<u8>> {
        let got: Got = Rc::default();
        let g = got.clone();
        read_pipe(rd, max, line, 2000, move |d| *g.borrow_mut() = Some(d));
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while got.borrow().is_none() {
            assert!(std::time::Instant::now() < end, "read_pipe did not finish");
            runtime::iterate(false);
        }
        got.take().unwrap()
    }

    #[test]
    fn read_pipe_reads_to_eof() {
        let (rd, wr) = pipe();
        write_all(&wr, b"one\ntwo");
        drop(wr);
        assert_eq!(read(rd, 100, false).as_deref(), Some(&b"one\ntwo"[..]));
    }

    #[test]
    fn read_pipe_line_stops_at_newline() {
        let (rd, wr) = pipe();
        // the writer stays open: the newline alone ends it
        write_all(&wr, b"first\nsecond");
        assert_eq!(read(rd, 100, true).as_deref(), Some(&b"first"[..]));
    }

    #[test]
    fn read_pipe_past_max() {
        let (rd, wr) = pipe();
        write_all(&wr, b"0123456789");
        drop(wr);
        assert_eq!(read(rd, 4, false), None);
        let (rd, wr) = pipe();
        write_all(&wr, b"0123456789");
        drop(wr);
        assert_eq!(read(rd, 4, true).as_deref(), Some(&b"0123"[..]));
    }

    #[test]
    fn serve_writes_everything() {
        // more than a pipe holds: serve has to wait for the reader on the loop
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let (rd, wr) = pipe();
        serve(wr, Arc::new(Secret::new(data.clone())));
        assert_eq!(SERVING.get(), 1);
        assert_eq!(read(rd, data.len(), false), Some(data));
        assert_eq!(SERVING.get(), 0);
    }
}
