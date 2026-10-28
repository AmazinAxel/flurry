// gpu-screen-recorder: manual recordings and the always-on replay clipper (widgets/record/service.ts).

use std::cell::{Cell, RefCell};

use crate::lib::battery::battery;
use crate::lib::clipboard::copy_uri;
use crate::lib::process::{self, Proc};
use crate::lib::state::State;
use crate::lib::sway;
use crate::lib::time::format;

type OnExit = std::rc::Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

thread_local! {
    static IS_REC: State<bool> = State::new(false);
    static REC_MIC: State<bool> = State::new(false);
    static REC_QUALITY: State<&'static str> = State::new("Ultra");
    static REC: RefCell<Option<(Proc, String, OnExit)>> = const { RefCell::new(None) };
    static CLIPPER: RefCell<Option<Proc>> = const { RefCell::new(None) };
    static CLIPPER_GEN: Cell<u32> = const { Cell::new(0) };
}

pub fn is_rec() -> State<bool> {
    IS_REC.with(Clone::clone)
}
pub fn rec_mic() -> State<bool> {
    REC_MIC.with(Clone::clone)
}
pub fn rec_quality() -> State<&'static str> {
    REC_QUALITY.with(Clone::clone)
}

fn videos() -> String {
    crate::lib::xdg::user_dir("VIDEOS")
}

fn stop_clipper() {
    CLIPPER_GEN.with(|g| g.set(g.get() + 1));
    if let Some(p) = CLIPPER.with(|c| c.borrow_mut().take()) {
        p.signal(libc::SIGINT);
    }
}

fn locked() -> bool {
    crate::widgets::lockscreen::lockscreen::locked().peek()
}

pub fn start_clipping_service() {
    stop_clipper();
    let generation = CLIPPER_GEN.with(Cell::get);
    if battery().peek().on_battery() || locked() {
        return;
    }
    let Some(monitor) = sway::focused_output() else { return };
    if generation != CLIPPER_GEN.with(Cell::get) || is_rec().peek() {
        return;
    }
    let clips = format!("{}/Clips", videos());
    let pid = std::rc::Rc::new(Cell::new(0u32));
    let p2 = pid.clone();
    let proc = process::spawn_lines(
        &["gpu-screen-recorder", "-a", "default_output|default_input", "-q", "medium", "-w", &monitor, "-o", &clips, "-f", "30", "-r", "30", "-c", "mp4"],
        copy_uri,
        |_| {},
        move |_| {
            CLIPPER.with(|c| {
                let mut c = c.borrow_mut();
                if c.as_ref().is_some_and(|x| x.pid == p2.get()) {
                    *c = None;
                }
            })
        },
    );
    if let Some(p) = proc {
        pid.set(p.pid);
        CLIPPER.with(|c| *c.borrow_mut() = Some(p));
    }
}

pub fn start() {
    let b = battery();
    let b2 = b.clone();
    let last = Cell::new(b.peek().state);
    b.subscribe(move || {
        // notify::state only
        let s = b2.peek().state;
        if last.replace(s) == s {
            return;
        }
        if b2.peek().on_battery() {
            stop_clipper();
        } else if CLIPPER.with(|c| c.borrow().is_none()) && !is_rec().peek() {
            start_clipping_service();
        }
    });
    let l = crate::widgets::lockscreen::lockscreen::locked();
    let l2 = l.clone();
    l.subscribe(move || {
        if l2.peek() {
            stop_clipper();
        } else if CLIPPER.with(|c| c.borrow().is_none()) && !is_rec().peek() {
            start_clipping_service();
        }
    });
}

pub fn start_rec() {
    stop_clipper();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let file = format!("{}/Captures/{}.mp4", videos(), format(now, "%Y-%m-%d_%H-%M-%S"));
    let audio = if rec_mic().peek() { "default_output|default_input" } else { "default_output" };
    is_rec().set(true);
    let Some(monitor) = sway::focused_output() else {
        is_rec().set(false);
        return;
    };
    if !is_rec().peek() {
        return;
    }
    let q = rec_quality().peek().to_lowercase();
    let on_exit: OnExit = Default::default();
    let hook = on_exit.clone();
    let exit = move |_| {
        if let Some(f) = hook.borrow_mut().take() {
            f();
        }
    };
    if let Some(p) = process::spawn_lines(&["gpu-screen-recorder", "-a", audio, "-q", &q, "-w", &monitor, "-o", &file], |_| {}, |_| {}, exit) {
        REC.with(|r| *r.borrow_mut() = Some((p, file, on_exit)));
    }
}

pub fn save_clip() {
    match CLIPPER.with(|c| c.borrow().as_ref().map(|p| p.pid)) {
        None => start_clipping_service(),
        Some(pid) => unsafe {
            libc::kill(pid as i32, libc::SIGUSR1);
        },
    }
}

pub fn stop_rec() {
    if let Some((p, file, on_exit)) = REC.with(|r| r.borrow_mut().take()) {
        // copy the file once the recorder has finalized it
        *on_exit.borrow_mut() = Some(Box::new(move || copy_uri(&file)));
        p.signal(libc::SIGINT);
    }
    is_rec().set(false);
    start_clipping_service();
}
