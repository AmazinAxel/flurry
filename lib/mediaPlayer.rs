// MPD client and playlist control (lib/mediaPlayer.tsx).

use std::cell::{Cell, RefCell};
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::Duration;

use crate::lib::state::State;
use crate::settings::{MPD_CROSSFADE, PLAYLISTS};
use crate::ui::runtime;

thread_local! {
    static PLAYING: State<bool> = State::new(false);
    static INDEX: State<usize> = State::new(0);
    static SOCK: RefCell<Option<Conn>> = const { RefCell::new(None) };
    static QUEUE_LOCK: Rc<runtime::Queue> = Rc::default();
    static STARTED: Cell<bool> = const { Cell::new(false) };
    static SCROLL: Cell<f64> = const { Cell::new(0.0) };
    static SONG: State<(String, String)> = State::new(Default::default());
    static WATCHING: Cell<bool> = const { Cell::new(false) };
}

pub fn is_playing() -> State<bool> {
    PLAYING.with(Clone::clone)
}
pub fn playlist_index() -> State<usize> {
    INDEX.with(Clone::clone)
}
/// Current playlist as (name, style color name); None when no playlists are configured.
pub fn playlist() -> Option<(&'static str, &'static str)> {
    PLAYLISTS.get(playlist_index().peek()).copied()
}
/// Current mpd song as (title, artist).
pub fn song() -> State<(String, String)> {
    SONG.with(Clone::clone)
}

struct Conn {
    /// the TCP or unix socket; File only for its Read/Write on the fd
    s: File,
    buf: Vec<u8>,
}

/// MPD_HOST: `[password@]host`, host being a name, an IPv6 address (bracketed or not) or a socket path.
fn parse_host(h: &str) -> (Option<&str>, &str) {
    match h.split_once('@') {
        Some((pw, host)) if !pw.is_empty() && !host.is_empty() => (Some(pw), host),
        _ => (None, h),
    }
}

/// `"s"` with mpd's escaping.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Connects on a thread (DNS and connect may block); returns the socket and the password if any.
async fn open() -> Option<(File, Option<String>)> {
    let env = std::env::var("MPD_HOST").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "127.0.0.1".into());
    let port = std::env::var("MPD_PORT").ok().and_then(|p| p.parse::<u16>().ok()).filter(|p| *p != 0).unwrap_or(6600);
    runtime::blocking(move || {
        let (pw, host) = parse_host(&env);
        let fd: OwnedFd = if host.starts_with('/') {
            UnixStream::connect(host).ok()?.into()
        } else {
            let addr = if host.contains(':') && !host.starts_with('[') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
            let s = addr.to_socket_addrs().ok()?.find_map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok())?;
            s.into()
        };
        Some((File::from(fd), pw.map(String::from)))
    })
    .await
    .flatten()
}

async fn read_line(c: &Rc<RefCell<Option<Conn>>>) -> Option<String> {
    loop {
        {
            let mut g = c.borrow_mut();
            let conn = g.as_mut()?;
            if let Some(i) = conn.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = conn.buf.drain(..=i).collect();
                return Some(String::from_utf8_lossy(&line[..i]).into_owned());
            }
            let mut chunk = [0u8; 4096];
            match conn.s.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => {
                    conn.buf.extend_from_slice(&chunk[..n]);
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return None,
            }
        }
        let fd = c.borrow().as_ref()?.s.as_raw_fd();
        runtime::readable(fd).await;
    }
}

async fn connect(c: &Rc<RefCell<Option<Conn>>>) -> Option<()> {
    let (s, pw) = open().await?;
    let fd = s.as_raw_fd();
    unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
    *c.borrow_mut() = Some(Conn { s, buf: vec![] });
    read_line(c).await?; // greeting
    if let Some(pw) = pw {
        exchange(c, &format!("password {}", quote(&pw))).await?;
    }
    Some(())
}

async fn exchange(c: &Rc<RefCell<Option<Conn>>>, cmd: &str) -> Option<Vec<String>> {
    c.borrow_mut().as_mut()?.s.write_all(format!("{cmd}\n").as_bytes()).ok()?;
    let mut lines = vec![];
    loop {
        let line = read_line(c).await?;
        if line == "OK" || line.starts_with("ACK") {
            break;
        }
        lines.push(line);
    }
    Some(lines)
}

async fn send(cmd: &str) -> Option<Vec<String>> {
    let c = Rc::new(RefCell::new(SOCK.with(|s| s.borrow_mut().take())));
    if c.borrow().is_none() {
        connect(&c).await?;
    }
    let lines = exchange(&c, cmd).await?;
    SOCK.with(|s| *s.borrow_mut() = c.borrow_mut().take());
    Some(lines)
}

/// Keeps `is_playing` and `song` in sync on a second connection parked in `idle player`.
/// Started at launch and again by any command, so a restarted mpd is picked back up.
pub fn watch() {
    if WATCHING.with(|w| w.replace(true)) {
        return;
    }
    runtime::spawn(async {
        let c = Rc::new(RefCell::new(None));
        if connect(&c).await.is_some() {
            while let Some(lines) = exchange(&c, "command_list_begin\nstatus\ncurrentsong\ncommand_list_end").await {
                let field = |k: &str| lines.iter().find_map(|l| l.strip_prefix(k)).unwrap_or_default().to_string();
                let file = field("file: ");
                let title = Some(field("Title: ")).filter(|t| !t.is_empty()).unwrap_or_else(|| file.rsplit('/').next().unwrap_or_default().to_string());
                is_playing().set(field("state: ") == "play");
                song().set((title, field("Artist: ")));
                if exchange(&c, "idle player").await.is_none() {
                    break;
                }
            }
        }
        WATCHING.with(|w| w.set(false));
    });
}

/// Queues commands; runs batches one at a time, reconnecting once on failure.
fn mpd(cmds: &[&str]) -> runtime::Receiver<Vec<String>> {
    watch();
    let (tx, rx) = runtime::oneshot();
    let cmds: Vec<String> = cmds.iter().map(|s| s.to_string()).collect();
    runtime::spawn(async move {
        let queue = QUEUE_LOCK.with(Clone::clone);
        let _turn = queue.lock().await;
        let mut lines = vec![];
        for cmd in &cmds {
            lines = match send(cmd).await {
                Some(l) => l,
                None => {
                    SOCK.with(|s| s.borrow_mut().take());
                    send(cmd).await.unwrap_or_default()
                }
            };
        }
        tx.send(lines);
    });
    rx
}

fn load_playlist() {
    let mut cmds = vec![];
    if !STARTED.with(|s| s.replace(true)) && MPD_CROSSFADE != 0 {
        cmds.push(format!("crossfade {MPD_CROSSFADE}"));
    }
    if let Some((name, _)) = playlist() {
        cmds.extend(["clear".to_string(), format!("add {}", quote(name)), "shuffle".into()]);
    }
    if !cmds.is_empty() {
        let refs: Vec<&str> = cmds.iter().map(String::as_str).collect();
        mpd(&refs);
    }
}

fn start() {
    if !STARTED.with(Cell::get) {
        load_playlist();
    }
}

pub fn upd_track(next: bool) {
    start();
    mpd(&[if next { "next" } else { "previous" }, "play"]);
    is_playing().set(true);
}

pub fn play_pause() {
    start();
    let rx = mpd(&["status"]);
    runtime::spawn(async move {
        let status = rx.await.unwrap_or_default();
        let playing = status.iter().any(|l| l == "state: play");
        mpd(&[if playing { "pause 1" } else { "play" }]);
    });
    let p = is_playing();
    p.set(!p.peek());
}

pub fn chng_playlist(next: bool) {
    let n = PLAYLISTS.len();
    if n == 0 {
        return;
    }
    let i = playlist_index();
    i.set((i.peek() + if next { 1 } else { n - 1 }) % n);
    mpd(&["pause 1"]);
    is_playing().set(false);
    load_playlist();
    play_pause();
}

/// Quickstatus media block scroll: accumulate and send whole volume steps.
pub fn media_scroll(dy: f64, wheel: bool) {
    let step = crate::lib::scrollStep::scroll_step(dy, wheel, 5.0, 0.6);
    let n = SCROLL.with(|a| {
        let v = a.get() - step;
        let n = v.trunc();
        a.set(v - n);
        n as i64
    });
    if n != 0 {
        mpd(&[&format!("volume {n}")]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpd_host_forms() {
        assert_eq!(parse_host("localhost"), (None, "localhost"));
        assert_eq!(parse_host("pw@[::1]"), (Some("pw"), "[::1]"));
        assert_eq!(parse_host("/run/mpd/socket"), (None, "/run/mpd/socket"));
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
