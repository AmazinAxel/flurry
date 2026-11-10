// MPD client and playlist control.

use std::cell::Cell;
use std::future::Future;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::Duration;

use crate::settings::{MPD_CROSSFADE, PLAYLISTS};
use crate::ui::runtime;
use crate::ui::state::global_state;

thread_local! {
    static SOCK: Cell<Option<Conn>> = const { Cell::new(None) };
    static QUEUE_LOCK: Rc<runtime::Queue> = Rc::default();
    static STARTED: Cell<bool> = const { Cell::new(false) };
    static SCROLL: Cell<f64> = const { Cell::new(0.0) };
    static WATCHING: Cell<bool> = const { Cell::new(false) };
}

global_state! {
    pub fn is_playing() -> bool = false;
    pub fn playlist_index() -> usize = 0;
    /// Current mpd song as (title, artist).
    pub fn song() -> (String, String) = Default::default();
    /// mpd's volume, 0..1; None without a mixer.
    pub fn volume() -> Option<f64> = None;
}
/// Current playlist as (name, style color name); None when no playlists are configured.
pub fn playlist() -> Option<(&'static str, &'static str)> {
    PLAYLISTS.get(playlist_index().peek()).copied()
}

struct Conn {
    /// the TCP or unix socket; UnixStream only for its Read/Write on the fd
    s: UnixStream,
    buf: Vec<u8>,
}

/// None: the connection broke; Err: mpd answered ACK.
type Reply = Option<Result<Vec<String>, String>>;

/// MPD_HOST: `[password@]host`, host being a name, an IPv6 address (bracketed or not), a socket path
/// or `@name` (abstract socket).
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

fn field<'a>(lines: &'a [String], k: &str) -> &'a str {
    lines.iter().find_map(|l| l.strip_prefix(k)).unwrap_or_default()
}

fn list(cmds: &[String]) -> String {
    format!("command_list_begin\n{}\ncommand_list_end", cmds.join("\n"))
}

/// A remote host that vanishes never closes the socket: keepalive and a user timeout turn that into
/// a read error after ~25s instead of a reply awaited forever.
fn keepalive(s: &TcpStream) {
    let fd = s.as_raw_fd();
    let set = |level, opt, v: libc::c_int| unsafe {
        libc::setsockopt(fd, level, opt, (&raw const v).cast(), size_of::<libc::c_int>() as libc::socklen_t);
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, 10);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, 5);
    set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
    set(libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT, 25_000);
}

/// Connects on a thread (DNS and connect may block); returns the socket and the password if any.
async fn open() -> Option<(OwnedFd, Option<String>)> {
    let env = std::env::var("MPD_HOST").ok().filter(|h| !h.is_empty()).unwrap_or_else(|| "127.0.0.1".into());
    let port = std::env::var("MPD_PORT").ok().and_then(|p| p.parse::<u16>().ok()).filter(|p| *p != 0).unwrap_or(6600);
    runtime::blocking(move || {
        let (pw, host) = parse_host(&env);
        let fd: OwnedFd = if host.starts_with('/') {
            UnixStream::connect(host).ok()?.into()
        } else if let Some(name) = host.strip_prefix('@') {
            use std::os::linux::net::SocketAddrExt;
            UnixStream::connect_addr(&std::os::unix::net::SocketAddr::from_abstract_name(name).ok()?).ok()?.into()
        } else {
            let addr = if host.contains(':') && !host.starts_with('[') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
            let s = addr.to_socket_addrs().ok()?.find_map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(2)).ok())?;
            keepalive(&s);
            s.into()
        };
        Some((fd, pw.map(String::from)))
    })
    .await
    .flatten()
}

async fn read_line(c: &mut Conn) -> Option<String> {
    loop {
        if let Some(i) = c.buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&c.buf[..i]).into_owned();
            c.buf.drain(..=i);
            return Some(line);
        }
        let mut chunk = [0u8; 4096];
        match c.s.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => c.buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => runtime::readable(c.s.as_raw_fd()).await,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

async fn connect() -> Option<Conn> {
    let (fd, pw) = open().await?;
    let s = UnixStream::from(fd);
    s.set_nonblocking(true).ok()?;
    let mut c = Conn { s, buf: vec![] };
    read_line(&mut c).await?; // greeting
    if let Some(pw) = pw {
        exchange(&mut c, &format!("password {}", quote(&pw))).await?.ok()?;
    }
    Some(c)
}

async fn exchange(c: &mut Conn, cmd: &str) -> Reply {
    c.s.write_all(format!("{cmd}\n").as_bytes()).ok()?;
    let mut lines = vec![];
    loop {
        let line = read_line(c).await?;
        if line == "OK" {
            return Some(Ok(lines));
        }
        if line.starts_with("ACK") {
            return Some(Err(line));
        }
        lines.push(line);
    }
}

/// On the shared command connection, (re)connecting if there is none.
async fn send(cmd: &str) -> Reply {
    let mut c = match SOCK.take() {
        Some(c) => c,
        None => connect().await?,
    };
    let r = exchange(&mut c, cmd).await?;
    SOCK.set(Some(c));
    Some(r)
}

/// Reconnects once if the connection broke. None when mpd is unreachable or refused (logged).
async fn cmd(cmd: &str) -> Option<Vec<String>> {
    let r = match send(cmd).await {
        Some(r) => r,
        None => send(cmd).await?,
    };
    r.map_err(|ack| eprintln!("flurry: mpd: {ack}")).ok()
}

/// Keeps `is_playing`, `song` and `volume` in sync on a second connection parked in `idle player`.
/// Started at launch and again by any command, so a restarted mpd is picked back up.
pub fn watch() {
    if WATCHING.replace(true) {
        return;
    }
    runtime::spawn(async {
        if let Some(mut c) = connect().await {
            while let Some(Ok(lines)) = exchange(&mut c, "command_list_begin\nstatus\ncurrentsong\ncommand_list_end").await {
                let file = field(&lines, "file: ");
                let title = Some(field(&lines, "Title: ")).filter(|t| !t.is_empty()).unwrap_or_else(|| file.rsplit('/').next().unwrap_or_default());
                is_playing().set(field(&lines, "state: ") == "play");
                song().set((title.to_string(), field(&lines, "Artist: ").to_string()));
                volume().set(field(&lines, "volume: ").parse::<f64>().ok().filter(|v| *v >= 0.0).map(|v| v / 100.0));
                if !matches!(exchange(&mut c, "idle player mixer").await, Some(Ok(_))) {
                    break;
                }
            }
        }
        is_playing().set(false);
        song().set(Default::default());
        volume().set(None);
        WATCHING.set(false);
    });
}

/// Runs `job` once every earlier job has finished.
fn queued(job: impl Future<Output = ()> + 'static) {
    watch();
    runtime::spawn(async move {
        let queue = QUEUE_LOCK.with(Clone::clone);
        let _turn = queue.lock().await;
        job.await;
    });
}

/// Appends the current playlist shuffled, then drops the old queue (`len` songs): mpd stops a
/// command list at the first ACK, so a failed add leaves the queue as it was.
async fn load_playlist(len: usize, play: bool) -> Option<()> {
    let (name, _) = playlist()?;
    let mut cmds = vec![format!("add {}", quote(name))];
    if len > 0 {
        cmds.push(format!("delete 0:{len}"));
    }
    cmds.push("shuffle".into());
    if play {
        cmds.push("play".into());
    }
    cmd(&list(&cmds)).await.map(drop)
}

/// The first command after launch sets crossfade, and loads the playlist only into an empty
/// queue (a shell restart must not replace what's playing) unless the caller loads its own.
/// Returns mpd's status.
async fn start(load: bool) -> Option<Vec<String>> {
    let status = cmd("status").await?;
    if STARTED.get() {
        return Some(status);
    }
    if MPD_CROSSFADE != 0 {
        cmd(&format!("crossfade {MPD_CROSSFADE}")).await?;
    }
    if load && field(&status, "playlistlength: ") == "0" && playlist().is_some() {
        load_playlist(0, false).await?;
    }
    STARTED.set(true);
    Some(status)
}

pub fn upd_track(next: bool) {
    queued(async move {
        if start(true).await.is_some() {
            cmd(&list(&[if next { "next" } else { "previous" }.into(), "play".into()])).await;
        }
    });
}

pub fn play_pause() {
    queued(async {
        if let Some(status) = start(true).await {
            cmd(if field(&status, "state: ") == "play" { "pause 1" } else { "play" }).await;
        }
    });
}

pub fn chng_playlist(next: bool) {
    let n = PLAYLISTS.len();
    if n == 0 {
        return;
    }
    let i = playlist_index();
    i.set((i.peek() + if next { 1 } else { n - 1 }) % n);
    queued(async {
        if let Some(status) = start(false).await {
            load_playlist(field(&status, "playlistlength: ").parse().unwrap_or(0), true).await;
        }
    });
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
        queued(async move {
            cmd(&format!("volume {n}")).await;
        });
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
        assert_eq!(parse_host("@mpd"), (None, "@mpd"));
        assert_eq!(parse_host("pw@@mpd"), (Some("pw"), "@mpd"));
        assert_eq!(quote(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
