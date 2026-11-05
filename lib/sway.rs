// i3-ipc over $SWAYSOCK: requests on one kept socket, subscriptions on their own.

use std::cell::RefCell;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use crate::lib::json::{self, Json};
use crate::ui::runtime;

const MAGIC: &[u8] = b"i3-ipc";

fn encode(kind: u32, payload: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(14 + payload.len());
    m.extend_from_slice(MAGIC);
    m.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    m.extend_from_slice(&kind.to_le_bytes());
    m.extend_from_slice(payload.as_bytes());
    m
}

/// Pops one complete message (type, body) off `buf`.
fn decode(buf: &mut Vec<u8>) -> Option<(u32, String)> {
    if buf.len() < 14 {
        return None;
    }
    let len = u32::from_le_bytes(buf[6..10].try_into().ok()?) as usize;
    let kind = u32::from_le_bytes(buf[10..14].try_into().ok()?);
    if buf.len() < 14 + len {
        return None;
    }
    let body = String::from_utf8_lossy(&buf[14..14 + len]).into_owned();
    buf.drain(..14 + len);
    Some((kind, body))
}

fn connect() -> Option<UnixStream> {
    UnixStream::connect(std::env::var_os("SWAYSOCK")?).ok()
}

thread_local! {
    static SOCK: RefCell<Option<UnixStream>> = const { RefCell::new(None) };
}

fn request(sock: &mut UnixStream, kind: u32, payload: &str) -> std::io::Result<String> {
    sock.write_all(&encode(kind, payload))?;
    let mut head = [0u8; 14];
    sock.read_exact(&mut head)?;
    let mut body = vec![0; u32::from_le_bytes([head[6], head[7], head[8], head[9]]) as usize];
    sock.read_exact(&mut body)?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Blocking round trip on the kept local socket; sway replies within microseconds.
/// A stuck sway costs at most the 1s timeout, not the whole loop.
fn roundtrip(kind: u32, payload: &str) -> Option<Json> {
    SOCK.with(|s| {
        let mut s = s.borrow_mut();
        for attempt in 0..2 {
            if s.is_none() {
                *s = connect().filter(|c| {
                    let t = Some(std::time::Duration::from_secs(1));
                    c.set_read_timeout(t).is_ok() && c.set_write_timeout(t).is_ok()
                });
            }
            match request(s.as_mut()?, kind, payload) {
                Ok(body) => return json::parse(&body),
                Err(e) => {
                    // a late reply would answer the next request: start over on a fresh socket
                    *s = None;
                    if attempt == 1 || matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) {
                        return None;
                    }
                }
            }
        }
        None
    })
}

pub fn command(cmd: &str) {
    let _ = roundtrip(0, cmd);
}

pub fn outputs() -> Option<Json> {
    roundtrip(3, "")
}

pub fn tree() -> Option<Json> {
    roundtrip(4, "")
}

pub fn workspaces() -> Option<Json> {
    roundtrip(1, "")
}

/// Subscribes to `events`; `cb` gets each event body.
pub fn subscribe(events: &[&str], cb: impl Fn(&Json) + 'static) {
    let Some(mut s) = connect() else { return };
    let list = format!("[{}]", events.iter().map(|e| json::quote(e)).collect::<Vec<_>>().join(","));
    if s.write_all(&encode(2, &list)).is_err() {
        return;
    }
    let _ = s.set_nonblocking(true);
    let fd = s.as_raw_fd();
    let buf = RefCell::new(Vec::new());
    runtime::watch(fd, move || {
        let mut chunk = [0u8; 65536];
        loop {
            match (&s).read(&mut chunk) {
                Ok(n) if n > 0 => buf.borrow_mut().extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                // EOF or a reset socket: poll() would report it forever
                _ => {
                    runtime::unwatch(fd);
                    return;
                }
            }
        }
        while let Some((kind, body)) = decode(&mut buf.borrow_mut()) {
            if kind & 0x8000_0000 != 0
                && let Some(j) = json::parse(&body)
            {
                cb(&j);
            }
        }
    });
}

/// Next/previous workspace from `cur` (the tracked focused one), clamped to 1-9; `take` moves the
/// focused window along.
pub fn step_workspace(cur: i64, forward: bool, take: bool) {
    let t = (cur + if forward { 1 } else { -1 }).clamp(1, 9);
    if t == cur {
        return;
    }
    command(&if take { format!("move container to workspace number {t}, workspace number {t}") } else { format!("workspace number {t}") });
}

thread_local! {
    /// the focused workspace holds windows other than TERMINAL_TILED_WITH: kept current from
    /// events so a keypress needs no tree read
    static TERMINAL_FLOATS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TERMINAL_QUEUED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Opens the terminal setting, floating when the focused workspace holds other apps.
pub fn open_terminal() {
    use crate::settings::{TERMINAL, TERMINAL_FLOATING};
    let float = TERMINAL_FLOATS.with(std::cell::Cell::get);
    let argv: Vec<String> = (if float { TERMINAL_FLOATING } else { TERMINAL }).iter().map(|s| s.to_string()).collect();
    crate::lib::apps::launch(&argv);
}

/// Re-reads the tree (once per burst, on idle) after workspace focus and window
/// open/close/move: what decides whether the terminal floats.
pub fn watch_terminal() {
    if crate::settings::TERMINAL_FLOATING.is_empty() {
        return;
    }
    refresh_terminal();
    subscribe(&["workspace", "window"], |ev| {
        let c = ev.get("change").str();
        let window = ev.get("container").get("id").int().is_some();
        let relevant = if window { matches!(c, Some("new" | "close" | "move")) } else { c == Some("focus") };
        if relevant && !TERMINAL_QUEUED.with(|q| q.replace(true)) {
            runtime::idle(|| {
                TERMINAL_QUEUED.with(|q| q.set(false));
                refresh_terminal();
            });
        }
    });
}

fn refresh_terminal() {
    use crate::settings::TERMINAL_TILED_WITH;
    fn focused_ws(n: &Json) -> Option<&Json> {
        if n.get("type").str() == Some("workspace") {
            return has_focus(n).then_some(n);
        }
        n.get("nodes").arr().iter().find_map(focused_ws)
    }
    fn has_focus(n: &Json) -> bool {
        n.get("focused").bool() == Some(true) || n.get("nodes").arr().iter().chain(n.get("floating_nodes").arr()).any(has_focus)
    }
    fn other(n: &Json) -> bool {
        let kids: Vec<&Json> = n.get("nodes").arr().iter().chain(n.get("floating_nodes").arr()).collect();
        if kids.is_empty() {
            let id = n.get("app_id").str().unwrap_or("");
            return n.get("type").str() != Some("workspace") && !TERMINAL_TILED_WITH.iter().any(|p| id.starts_with(p));
        }
        kids.into_iter().any(other)
    }
    if let Some(t) = tree() {
        TERMINAL_FLOATS.with(|f| f.set(focused_ws(&t).is_some_and(other)));
    }
}

pub fn focused_output() -> Option<String> {
    outputs()?.arr().iter().find(|o| o.get("focused").bool() == Some(true)).and_then(|o| o.get("name").str().map(String::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_roundtrip() {
        let mut buf = encode(4, "{}");
        buf.extend(encode(0x8000_0000, "[1]"));
        buf.extend_from_slice(b"i3-ip");
        assert_eq!(decode(&mut buf), Some((4, "{}".into())));
        assert_eq!(decode(&mut buf), Some((0x8000_0000, "[1]".into())));
        assert_eq!(decode(&mut buf), None);
        assert_eq!(buf, b"i3-ip");
    }
}
