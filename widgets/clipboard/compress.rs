// Video compression to a size target (widgets/clipboard/compress.tsx).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use xkbcommon::xkb::Keysym;

use crate::lib::backgroundSection::{background_section, header_style};
use crate::lib::clipboard::copy_uri;
use crate::lib::inputControl::{Opts, input_control};
use crate::lib::process::{exec, spawn_lines};
use crate::lib::state::State;
use crate::lib::style::*;
use crate::ui::node::{Align, Node, Style, TRANSPARENT};
use crate::ui::wayland::Anchor;
use crate::ui::window::{self, Spec, Win};
use crate::ui::runtime;
use crate::widgets::clipboard::clipboard::TMP_DIR;
use crate::widgets::focus::levelbar;

const TARGETS: [u32; 5] = [10, 20, 50, 100, 500]; // MB

thread_local! {
    static TARGET: State<usize> = State::new(0);
    static PROGRESS: State<f64> = State::new(-1.0);
    static SOURCE: RefCell<(String, u64)> = const { RefCell::new((String::new(), 0)) };
}

fn target() -> State<usize> {
    TARGET.with(Clone::clone)
}
fn progress() -> State<f64> {
    PROGRESS.with(Clone::clone)
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
            e.borrow_mut().push_str(line);
            e.borrow_mut().push('\n');
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
    let out = format!("{TMP_DIR}/{}.mp4", if base == id.to_string() { base } else { format!("{base}_{id}") });
    runtime::spawn(async move {
        let result: Result<(), String> = async {
            let d = exec(&["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", &source]).await?;
            let duration: f64 = d.trim().parse().map_err(|_| d.clone())?;
            let vb = ((limit * 7_600_000.0 / duration - 128_000.0).round() as i64).max(100_000);
            let args = [
                "-i", &source, "-c:v", "libx264", "-preset", "veryfast", "-b:v", &vb.to_string(), "-maxrate", &vb.to_string(), "-bufsize",
                &(vb * 2).to_string(), "-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart", &out,
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            ffmpeg(args, duration * 1e6).await?;
            copy_uri(&out);
            Ok(())
        }
        .await;
        if let Err(e) = result {
            crate::lib::notifySend::notify_send("Compress", "Compression failed", &e, vec![]);
        }
        progress().set(-1.0);
    });
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
            actions: vec![(Keysym::Tab, Rc::new(|| target().set((target().peek() + 1) % TARGETS.len()))), (Keysym::Return, Rc::new(compress))],
            ..Default::default()
        },
    );
    w.watch(&target());
}

pub fn compress_progress() -> Rc<Win> {
    let spec = Spec { name: "compressProgress", anchor: Anchor::Bottom | Anchor::Left | Anchor::Right, input: false, per_monitor: true, ..Default::default() };
    let w = Win::new(spec, |_| {
        let base = window();
        levelbar(&Style { ..base.inherit() }, progress().peek().max(0.0), green(), TRANSPARENT, 4.0)
    });
    let ww = Rc::downgrade(&w);
    let last = Cell::new(false);
    progress().subscribe(move || {
        let Some(w) = ww.upgrade() else { return };
        let p = progress().peek();
        let show = (0.0..0.99).contains(&p);
        if last.replace(show) != show {
            w.set_visible(show);
        }
        w.redraw();
    });
    w
}
