// i3-ipc over $SWAYSOCK: requests on one kept socket, every subscription on one shared event socket.

use std::cell::RefCell;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;

use crate::lib::json::{self, Json};
use crate::ui::runtime;

const MAGIC: &[u8] = b"i3-ipc";
/// set on event message types
const EVENT: u32 = 0x8000_0000;
const WINDOW: u32 = 3;

fn encode(kind: u32, payload: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(14 + payload.len());
    m.extend_from_slice(MAGIC);
    m.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    m.extend_from_slice(&kind.to_le_bytes());
    m.extend_from_slice(payload.as_bytes());
    m
}

/// Takes every complete message (type, body) off `buf` in one pass, keeping a partial tail.
fn decode(buf: &mut Vec<u8>) -> Vec<(u32, String)> {
    let mut out = vec![];
    let mut off = 0;
    while let Some(head) = buf.get(off..off + 14) {
        let len = u32::from_le_bytes([head[6], head[7], head[8], head[9]]) as usize;
        let kind = u32::from_le_bytes([head[10], head[11], head[12], head[13]]);
        let Some(body) = buf.get(off + 14..off + 14 + len) else { break };
        out.push((kind, String::from_utf8_lossy(body).into_owned()));
        off += 14 + len;
    }
    buf.drain(..off);
    out
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

/// The event type sway numbers `name` with (the low bits of its message type).
fn event_kind(name: &str) -> Option<u32> {
    Some(match name {
        "workspace" => 0,
        "output" => 1,
        "mode" => 2,
        "window" => WINDOW,
        "barconfig_update" => 4,
        "binding" => 5,
        "shutdown" => 6,
        "tick" => 7,
        "bar_state_update" => 0x14,
        "input" => 0x15,
        _ => return None,
    })
}

/// An event's "change" (sway writes it first), read without parsing the rest. A "change" inside a
/// string value is escaped, so the first unescaped one is the event's own.
fn change_of(body: &str) -> Option<&str> {
    let rest = &body[body.find("\"change\"")? + 8..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    rest.split('"').next()
}

type EventCb = Rc<dyn Fn(&Json)>;

struct Events {
    sock: UnixStream,
    buf: Vec<u8>,
    /// event types already subscribed on `sock`
    subscribed: Vec<u32>,
    subs: Vec<(Vec<u32>, EventCb)>,
}

thread_local! {
    static EVENTS: RefCell<Option<Events>> = const { RefCell::new(None) };
}

/// Subscribes to `events`; `cb` gets each event body. All subscribers share one socket and each
/// event is parsed once. Window title changes are dropped unparsed: nothing here uses them, and
/// terminals send one per prompt.
pub fn subscribe(events: &[&str], cb: impl Fn(&Json) + 'static) {
    let kinds: Vec<u32> = events.iter().filter_map(|e| event_kind(e)).collect();
    if kinds.is_empty() {
        return;
    }
    EVENTS.with(|ev| {
        let mut ev = ev.borrow_mut();
        if ev.is_none() {
            let Some(sock) = connect() else { return };
            if sock.set_nonblocking(true).is_err() {
                return;
            }
            runtime::watch(sock.as_raw_fd(), read_events);
            *ev = Some(Events { sock, buf: vec![], subscribed: vec![], subs: vec![] });
        }
        let Some(e) = ev.as_mut() else { return };
        let new: Vec<&str> = events.iter().copied().filter(|n| event_kind(n).is_some_and(|k| !e.subscribed.contains(&k))).collect();
        if !new.is_empty() {
            // its reply comes back among the events (not an event type: skipped)
            let list = format!("[{}]", new.iter().map(|n| json::quote(n)).collect::<Vec<_>>().join(","));
            if (&e.sock).write_all(&encode(2, &list)).is_err() {
                return;
            }
            e.subscribed.extend(new.iter().filter_map(|n| event_kind(n)));
        }
        e.subs.push((kinds, Rc::new(cb)));
    });
}

fn read_events() {
    let mut chunk = [0u8; 65536];
    // None: the socket is gone
    let got = EVENTS.with(|ev| {
        let mut ev = ev.borrow_mut();
        let e = ev.as_mut()?;
        loop {
            match (&e.sock).read(&mut chunk) {
                Ok(n) if n > 0 => e.buf.extend_from_slice(&chunk[..n]),
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                // EOF or a reset socket: poll() would report it forever
                _ => {
                    runtime::unwatch(e.sock.as_raw_fd());
                    *ev = None;
                    return None;
                }
            }
        }
        Some((decode(&mut e.buf), e.subs.clone()))
    });
    let Some((msgs, subs)) = got else { return };
    // run outside the borrow: a handler may subscribe
    for (kind, body) in msgs {
        if kind & EVENT == 0 {
            continue;
        }
        let kind = kind & !EVENT;
        if kind == WINDOW && change_of(&body) == Some("title") {
            continue;
        }
        if !subs.iter().any(|(k, _)| k.contains(&kind)) {
            continue;
        }
        let Some(j) = json::parse(&body) else { continue };
        for (k, cb) in &subs {
            if k.contains(&kind) {
                cb(&j);
            }
        }
    }
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
        assert_eq!(decode(&mut buf), vec![(4, "{}".into()), (0x8000_0000, "[1]".into())]);
        assert_eq!(buf, b"i3-ip");
        assert_eq!(decode(&mut buf), vec![]);
    }

    #[test]
    fn change_read_without_parsing() {
        assert_eq!(change_of(r#"{ "change": "title", "container": { "name": "x" } }"#), Some("title"));
        assert_eq!(change_of(r#"{"change":"new","container":{"name":"\"change\": \"title\""}}"#), Some("new"));
        assert_eq!(change_of(r#"{ "container": {} }"#), None);
    }
}
