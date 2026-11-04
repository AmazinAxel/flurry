// gpu-screen-recorder: manual recordings and the always-on replay clipper.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::lib::battery::battery;
use crate::lib::clipboard::copy_uri;
use crate::lib::notifySend::notify_send;
use crate::lib::process::{self, Proc};
use crate::lib::sway;
use crate::lib::time::{format, now_secs};
use crate::ui::state::global_state;

/// A clipper that dies sooner than this after starting is reported instead of restarted.
const MIN_RUN: Duration = Duration::from_secs(60);

thread_local! {
    /// the manual recorder and its file, until it exits
    static REC: RefCell<Option<(Proc, String)>> = const { RefCell::new(None) };
    /// the replay clipper and the output it records
    static CLIPPER: RefCell<Option<(Proc, String)>> = const { RefCell::new(None) };
}

global_state! {
    pub fn is_rec() -> bool = false;
    pub fn rec_mic() -> bool = false;
    pub fn rec_quality() -> &'static str = "Ultra";
}

fn videos() -> String {
    crate::lib::xdg::user_dir("VIDEOS")
}

fn audio_sources() -> &'static str {
    if rec_mic().peek() { "default_output|default_input" } else { "default_output" }
}

fn locked() -> bool {
    crate::widgets::lockscreen::lockscreen::locked().peek()
}

/// Keeps the last stderr line, the likely reason for a failure.
fn keep_last(into: &Rc<RefCell<String>>) -> impl Fn(&str) + 'static {
    let into = into.clone();
    move |l: &str| {
        let l = l.trim();
        if !l.is_empty() {
            *into.borrow_mut() = l.chars().take(200).collect();
        }
    }
}

fn report(title: &str, stderr: &str, code: i32) {
    let reason = if stderr.is_empty() { format!("exit code {code}") } else { stderr.to_string() };
    notify_send("Recorder", title, &reason, vec![]);
}

fn stop_clipper() {
    if let Some((p, _)) = CLIPPER.with(|c| c.borrow_mut().take()) {
        p.signal(libc::SIGINT);
    }
}

/// Runs the clipper on AC power while unlocked and no recording holds the GPU, else stops it;
/// `restart` replaces a running one (new audio sources or output).
fn sync_clipper(restart: bool) {
    let want = !battery().peek().on_battery() && !locked() && REC.with(|r| r.borrow().is_none());
    if restart || !want {
        stop_clipper();
    }
    if !want || CLIPPER.with(|c| c.borrow().is_some()) {
        return;
    }
    let Some(monitor) = sway::focused_output() else { return };
    let clips = format!("{}/Clips", videos());
    let err = Rc::new(RefCell::new(String::new()));
    let started = Instant::now();
    let exit = {
        let err = err.clone();
        move |code| {
            // still listed: it died on its own, not stopped by us
            let died = CLIPPER.with(|c| c.borrow_mut().take_if(|(p, _)| p.exited()).is_some());
            if !died {
                return;
            }
            if started.elapsed() >= MIN_RUN {
                sync_clipper(false);
            } else {
                report("Replay clipper stopped", &err.borrow(), code);
            }
        }
    };
    let argv = ["gpu-screen-recorder", "-a", audio_sources(), "-q", "medium", "-w", &monitor, "-o", &clips, "-f", "30", "-r", "30", "-c", "mp4", "-v", "no"];
    if let Some(p) = process::spawn_lines(&argv, copy_uri, keep_last(&err), exit) {
        CLIPPER.with(|c| *c.borrow_mut() = Some((p, monitor)));
    }
}

pub fn start_clipping_service() {
    sync_clipper(false);
}

pub fn start() {
    battery().map(|b| b.on_battery()).subscribe(|| sync_clipper(false));
    crate::widgets::lockscreen::lockscreen::locked().subscribe(|| sync_clipper(false));
    rec_mic().subscribe(|| sync_clipper(true));
    // an output change can take the clipper's output away (or bring the first one)
    sway::subscribe(&["output"], |_| {
        let Some(mon) = CLIPPER.with(|c| c.borrow().as_ref().map(|(_, m)| m.clone())) else { return sync_clipper(false) };
        let active = sway::outputs().is_some_and(|o| o.arr().iter().any(|x| x.get("name").str() == Some(mon.as_str()) && x.get("active").bool() == Some(true)));
        if !active {
            sync_clipper(true);
        }
    });
}

pub fn start_rec() {
    if REC.with(|r| r.borrow().is_some()) {
        return;
    }
    let Some(monitor) = sway::focused_output() else { return };
    stop_clipper();
    let file = format!("{}/Captures/{}.mp4", videos(), format(now_secs() as i64, "%Y-%m-%d_%H-%M-%S"));
    let q = rec_quality().peek().to_lowercase();
    let err = Rc::new(RefCell::new(String::new()));
    let exit = {
        let err = err.clone();
        move |code| {
            let Some((_, file)) = REC.with(|r| r.borrow_mut().take()) else { return };
            // stop_rec clears is_rec before signalling; still set means it ended on its own
            let stopped = !is_rec().peek();
            is_rec().set(false);
            if stopped && std::fs::metadata(&file).is_ok_and(|m| m.len() > 0) {
                copy_uri(&file);
            } else {
                report("Recording failed", &err.borrow(), code);
            }
            sync_clipper(false);
        }
    };
    let argv = ["gpu-screen-recorder", "-a", audio_sources(), "-q", &q, "-w", &monitor, "-o", &file, "-v", "no"];
    match process::spawn_lines(&argv, |_| {}, keep_last(&err), exit) {
        Some(p) => {
            REC.with(|r| *r.borrow_mut() = Some((p, file)));
            is_rec().set(true);
        }
        None => sync_clipper(false),
    }
}

pub fn save_clip() {
    let saved = CLIPPER.with(|c| c.borrow().as_ref().map(|(p, _)| p.signal(libc::SIGUSR1)).is_some());
    if !saved {
        sync_clipper(false);
    }
}

/// The recorder finalizes the file on SIGINT; its exit copies it and restarts the clipper.
pub fn stop_rec() {
    is_rec().set(false);
    let running = REC.with(|r| r.borrow().as_ref().map(|(p, _)| p.signal(libc::SIGINT)).is_some());
    if !running {
        sync_clipper(false);
    }
}
