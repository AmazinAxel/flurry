// The Sync and Cleanup reminders, woken only by what decides them: a wall-clock timer at the
// moment the next one comes due (a week after the last sync, a day after the last reminder),
// inotify under Downloads, and the reminders' own notifications closing.

use std::cell::{Cell, RefCell};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use crate::lib::{notifd, notifySend, process, time, xdg};
use crate::settings;
use crate::ui::runtime;

const DAY: u64 = 86_400;
const SYNC_EVERY: u64 = 7 * DAY;
/// Downloads past this many bytes get the Cleanup reminder
const DOWNLOADS_MAX: u64 = 100_000_000;

type Note = &'static std::thread::LocalKey<Cell<Option<(u32, u64)>>>;

thread_local! {
    /// each reminder last sent (id, unix secs): not again while it's up or for a day after
    static SYNC_NOTE: Cell<Option<(u32, u64)>> = const { Cell::new(None) };
    static DOWNLOADS_NOTE: Cell<Option<(u32, u64)>> = const { Cell::new(None) };
    /// Downloads' size at the last walk
    static DOWNLOADS_SIZE: Cell<u64> = const { Cell::new(0) };
    /// Downloads changed since the last walk (or was never walked). Walked again only once the
    /// reminder may go out: while it's up or resting, changes just mark it.
    static STALE: Cell<bool> = const { Cell::new(true) };
    /// a walk is running, and something changed since it started
    static WALKING: Cell<bool> = const { Cell::new(false) };
    static DIRTY: Cell<bool> = const { Cell::new(false) };
    /// inotify on every dir under Downloads, replaced by each walk
    static WATCH: RefCell<Option<OwnedFd>> = const { RefCell::new(None) };
    /// the next deadline
    static NEXT: RefCell<Option<time::WallTimer>> = const { RefCell::new(None) };
}

pub fn start() {
    notifd::on_resolved(|id, _| {
        // one of ours closed: its day may already be over
        if [&SYNC_NOTE, &DOWNLOADS_NOTE].iter().any(|n| n.with(Cell::get).is_some_and(|(i, _)| i == id)) {
            runtime::idle_once(check);
        }
    });
    check();
}

/// When `note` may go out again: now if it never did, a day after it did, not while it's up
/// (its closing checks again).
fn free_at(note: Note, app: &str) -> Option<u64> {
    match note.with(Cell::get) {
        None => Some(0),
        Some((id, _)) if notifd::get(id).is_some_and(|n| n.app_name == app) => None,
        Some((_, at)) => Some(at + DAY),
    }
}

fn last_sync_path() -> String {
    format!("{}/flurry/lastSync", xdg::state_dir())
}

/// When the Sync reminder is due: a week after the last successful sync (unix ms in the file).
fn sync_due() -> Option<u64> {
    if settings::SYNC_COMMAND.is_empty() {
        return None;
    }
    let last: u64 = std::fs::read_to_string(last_sync_path()).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    Some((last / 1000 + SYNC_EVERY).max(free_at(&SYNC_NOTE, "Sync")?))
}

/// Downloads, unless user-dirs points it at $HOME (whose size means nothing here).
fn downloads_dir() -> Option<String> {
    let d = xdg::user_dir("DOWNLOAD");
    (d.trim_end_matches('/') != xdg::home_dir().trim_end_matches('/')).then_some(d)
}

/// When Downloads needs looking at again: once the reminder may go out, if it's over the limit
/// or changed since.
fn downloads_due() -> Option<u64> {
    downloads_dir()?;
    if !STALE.with(Cell::get) && DOWNLOADS_SIZE.with(Cell::get) <= DOWNLOADS_MAX {
        return None;
    }
    free_at(&DOWNLOADS_NOTE, "Cleanup")
}

/// Does what's due (a reminder, a walk of Downloads), then sleeps until the next deadline.
fn check() {
    let now = time::now_secs();
    if sync_due().is_some_and(|t| t <= now) {
        send_sync();
    }
    if downloads_due().is_some_and(|t| t <= now) {
        if STALE.with(Cell::get) {
            // its end checks again, with the size
            rescan();
        } else if let Some(dir) = downloads_dir() {
            let argv: Vec<String> = settings::FILE_MANAGER.iter().map(|s| s.to_string()).chain([dir]).collect();
            let id = notifySend::notify_send("Cleanup", "Empty Downloads", "", vec![(1, "View folder".into(), argv)]);
            DOWNLOADS_NOTE.with(|n| n.set(Some((id, now))));
        }
    }
    // what's due now has been handled (a walk under way checks when it ends)
    let next = [sync_due(), downloads_due()].into_iter().flatten().filter(|&t| t > now).min();
    // replaced: the old one (maybe the one firing now) is cancelled
    NEXT.with(|n| *n.borrow_mut() = next.and_then(|t| time::at_wall(t, || runtime::idle_once(check))));
}

fn send_sync() {
    let id = notifd::send_local("Sync", "Sync system", "", &[("1".into(), "Update & Sync".into())], |action| {
        if action != "1" {
            return;
        }
        runtime::spawn(async {
            if process::status(settings::SYNC_COMMAND).await != Some(0) {
                return;
            }
            let path = last_sync_path();
            if let Some(dir) = Path::new(&path).parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = xdg::write_atomic(&path, (time::now_secs() * 1000).to_string().as_bytes(), 0o644) {
                eprintln!("flurry: {path}: {e}");
            }
            // the next one is a week out now
            runtime::idle_once(check);
        });
    });
    SYNC_NOTE.with(|n| n.set(Some((id, time::now_secs()))));
}

/// Re-walks Downloads off the loop (one walk at a time; changes during it walk again after).
fn rescan() {
    let Some(dir) = downloads_dir() else { return };
    if WALKING.with(|w| w.replace(true)) {
        DIRTY.with(|d| d.set(true));
        return;
    }
    DIRTY.with(|d| d.set(false));
    STALE.with(|s| s.set(false));
    runtime::spawn(async move {
        let walked = runtime::blocking(move || walk(&dir)).await;
        WALKING.with(|w| w.set(false));
        // None: the walk panicked; not retried until the next change (it would just panic again)
        if let Some((size, ino)) = walked {
            DOWNLOADS_SIZE.with(|s| s.set(size));
            watch(ino);
        }
        if DIRTY.with(Cell::get) {
            STALE.with(|s| s.set(true));
        }
        check();
    });
}

/// The size of everything under `dir` (`du -sb`), and an inotify fd watching each dir before it
/// is read, so a change during the walk still fires.
fn walk(dir: &str) -> (u64, Option<OwnedFd>) {
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    let ino = (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) });
    (size(Path::new(dir), fd), ino)
}

fn size(p: &Path, ino: RawFd) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else { return 0 };
    let mut total = meta.len();
    if meta.is_dir() {
        if ino >= 0
            && let Ok(c) = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(p.as_os_str()))
        {
            // a finished write, or an entry coming or going
            let mask = libc::IN_CLOSE_WRITE | libc::IN_CREATE | libc::IN_DELETE | libc::IN_MOVED_FROM | libc::IN_MOVED_TO | libc::IN_DELETE_SELF;
            unsafe { libc::inotify_add_watch(ino, c.as_ptr(), mask) };
        }
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                total += size(&e.path(), ino);
            }
        }
    }
    total
}

/// Replaces the inotify watch. Its first event marks Downloads changed (the fd is done: the
/// next walk watches anew) and checks, which walks only if the reminder may go out.
fn watch(ino: Option<OwnedFd>) {
    if let Some(old) = WATCH.with(|w| w.borrow_mut().take()) {
        runtime::unwatch(old.as_raw_fd());
    }
    let Some(ino) = ino else { return };
    let fd = ino.as_raw_fd();
    WATCH.with(|w| *w.borrow_mut() = Some(ino));
    runtime::watch(fd, move || {
        runtime::unwatch(fd);
        STALE.with(|s| s.set(true));
        runtime::idle_once(check);
    });
}
