// Video compression to a size target.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::backgroundSection::{background_section, header_style};
use crate::lib::clipboard::copy_uri;
use crate::lib::inputControl::{Opts, input_control};
use crate::lib::process::{exec, spawn_lines};
use crate::lib::style::*;
use crate::ui::node::{Align, Node, TRANSPARENT, levelbar};
use crate::ui::wayland::Anchor;
use crate::ui::window::{self, Spec, Win};
use crate::ui::runtime;
use crate::ui::state::global_state;

const TARGETS: [u32; 5] = [10, 20, 50, 100, 500]; // MB

thread_local! {
    static SOURCE: RefCell<(String, u64)> = const { RefCell::new((String::new(), 0)) };
    /// the last output: replaced by the next one, so they don't pile up in the cache
    static LAST_OUT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// ffmpeg's stderr kept for the failure notification: only its end.
const MAX_ERR: usize = 4096;

global_state! {
    fn target() -> usize = 0;
    fn progress() -> f64 = -1.0;
}

async fn ffmpeg(args: Vec<String>, duration_us: f64) -> Result<(), String> {
    let mut argv = vec!["ffmpeg", "-v", "error", "-nostats", "-stats_period", "0.1", "-progress", "pipe:1", "-y"];
    argv.extend(args.iter().map(String::as_str));
    let err = Rc::new(RefCell::new(String::new()));
    let (tx, rx) = runtime::oneshot::<i32>();
    let e = err.clone();
    let spawned = spawn_lines(
        &argv,
        move |line| {
            if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok())
                && us >= 0.0
            {
                progress().set((us / duration_us).min(1.0));
            }
        },
        move |line| {
            let mut e = e.borrow_mut();
            e.push_str(line);
            e.push('\n');
            if e.len() > MAX_ERR {
                let mut cut = e.len() - MAX_ERR;
                while !e.is_char_boundary(cut) {
                    cut += 1;
                }
                e.drain(..cut);
            }
        },
        move |code| tx.send(code),
    );
    if spawned.is_none() {
        return Err("failed to spawn ffmpeg".into());
    }
    match rx.await {
        Some(0) => Ok(()),
        _ => Err(err.borrow().trim().to_string()),
    }
}

fn compress() {
    if progress().peek() >= 0.0 {
        return;
    }
    progress().set(0.0);
    window::set_visible("compress", false);
    let limit = TARGETS[target().peek()] as f64;
    let (source, id) = SOURCE.with(|s| s.borrow().clone());
    let file = source.rsplit('/').next().unwrap_or("").to_string();
    let base = match file.rfind('.') {
        Some(i) => file[..i].to_string(),
        None => file,
    };
    // videos are big: on disk, not in the RAM-backed runtime dir
    let dir = format!("{}/flurry", crate::lib::xdg::cache_dir());
    let _ = std::fs::create_dir_all(&dir);
    let out = format!("{dir}/{base}_{id}.mp4");
    if let Some(old) = LAST_OUT.with(|l| l.borrow_mut().replace(out.clone()))
        && old != source
    {
        let _ = std::fs::remove_file(old);
    }
    runtime::spawn(async move {
        let result: Result<(), String> = async {
            let d = exec(&["ffprobe", "-v", "error", "-protocol_whitelist", "file", "-show_entries", "format=duration", "-of", "csv=p=0", &source]).await?;
            let duration: f64 = d.trim().parse().ok().filter(|d: &f64| *d > 0.0).ok_or_else(|| d.clone())?;
            let vb = ((limit * 7_600_000.0 / duration - 128_000.0).round() as i64).max(100_000);
            let args = [
                "-protocol_whitelist", "file", "-i", &source, "-c:v", "libx264", "-preset", "veryfast", "-b:v", &vb.to_string(), "-maxrate", &vb.to_string(), "-bufsize",
                &(vb * 2).to_string(), "-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart", &out,
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            ffmpeg(args, duration * 1e6).await?;
            // the history was wiped meanwhile: the output goes with it
            if LAST_OUT.with(|l| l.borrow().as_deref() != Some(out.as_str())) {
                let _ = std::fs::remove_file(&out);
                return Ok(());
            }
            copy_uri(&out);
            Ok(())
        }
        .await;
        if let Err(e) = result {
            let _ = std::fs::remove_file(&out); // a partial output
            crate::lib::notifySend::notify_send("Compress", "Compression failed", &e, vec![]);
        }
        progress().set(-1.0);
    });
}

/// A history wipe takes the last output (which the history pointed at) too; one still being
/// written is deleted when it finishes.
pub fn wipe_last_output() {
    if let Some(old) = LAST_OUT.with(|l| l.borrow_mut().take()) {
        let _ = std::fs::remove_file(old);
    }
}

pub fn open_compress(file: &str, id: u64) {
    if progress().peek() >= 0.0 {
        return;
    }
    SOURCE.with(|s| *s.borrow_mut() = (file.to_string(), id));
    target().set(0);
    window::set_visible("compress", true);
}

pub fn compress_window() {
    let w = input_control(
        "compress",
        |_| {
            let base = window();
            let header = Node::label(header_style(&base), format!("{}MB", TARGETS[target().peek()])).expand(true, true).align(Align::Center, Align::Center);
            background_section(&base, header, None, 100.0, 350.0)
        },
        Opts {
            actions: vec![
                (Keysym::Tab, Rc::new(|| target().set((target().peek() + 1) % TARGETS.len()))),
                (Keysym::ISO_Left_Tab, Rc::new(|| target().set((target().peek() + TARGETS.len() - 1) % TARGETS.len()))),
                (Keysym::Return, Rc::new(compress)),
            ],
            ..Default::default()
        },
    );
    w.watch(&target());
}

pub fn compress_progress() -> Rc<Win> {
    let spec = Spec { name: "compressProgress", anchor: Anchor::Bottom | Anchor::Left | Anchor::Right, input: false, per_monitor: true, ..Default::default() };
    let w = Win::new(spec, |_| {
        let base = window();
        levelbar(&base, progress().peek().max(0.0), green(), TRANSPARENT, 4.0)
    });
    let ww = Rc::downgrade(&w);
    let last = Cell::new(false);
    progress().subscribe(move || {
        let Some(w) = ww.upgrade() else { return };
        let p = progress().peek();
        let show = (0.0..0.99).contains(&p);
        if last.replace(show) != show {
            w.set_visible(show);
        } else if w.visible() {
            w.redraw();
        }
    });
    w
}
