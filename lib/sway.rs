// i3-ipc over $SWAYSOCK: request queue on one socket, subscriptions on their own (lib/sway.ts).

use std::cell::RefCell;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;

use crate::lib::json::{self, Json};
use crate::ui::runtime;

const MAGIC: &[u8] = b"i3-ipc";

pub fn encode(kind: u32, payload: &str) -> Vec<u8> {
    let mut m = Vec::with_capacity(14 + payload.len());
    m.extend_from_slice(MAGIC);
    m.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    m.extend_from_slice(&kind.to_le_bytes());
    m.extend_from_slice(payload.as_bytes());
    m
}

/// Pops one complete message (type, body) off `buf`.
pub fn decode(buf: &mut Vec<u8>) -> Option<(u32, String)> {
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
    static QUEUE: RefCell<Vec<(u32, String, runtime::Sender<Option<Json>>)>> = const { RefCell::new(Vec::new()) };
    static BUSY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Blocking round trip on a short-lived local socket; sway replies within microseconds.
fn roundtrip(kind: u32, payload: &str) -> Option<Json> {
    SOCK.with(|s| {
        let mut s = s.borrow_mut();
        for attempt in 0..2 {
            if s.is_none() {
                *s = connect();
            }
            let sock = s.as_mut()?;
            let ok = sock.write_all(&encode(kind, payload)).is_ok();
            let mut buf = vec![];
            let mut chunk = [0u8; 65536];
            let reply = ok
                .then(|| loop {
                    if let Some((_, body)) = decode(&mut buf) {
                        break Some(body);
                    }
                    match sock.read(&mut chunk) {
                        Ok(0) | Err(_) => break None,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                })
                .flatten();
            if let Some(body) = reply {
                return json::parse(&body);
            }
            *s = None;
            if attempt == 1 {
                return None;
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

/// Subscribes to `events`; `cb` gets each event body.
pub fn subscribe(events: &[&str], cb: impl Fn(&Json) + 'static) {
    let Some(mut s) = connect() else { return };
    let list = format!("[{}]", events.iter().map(|e| json::quote(e)).collect::<Vec<_>>().join(","));
    if s.write_all(&encode(2, &list)).is_err() {
        return;
    }
    let _ = s.set_nonblocking(true);
    let fd = s.as_raw_fd();
    let s = Rc::new(RefCell::new(s));
    let buf = RefCell::new(Vec::new());
    runtime::watch(fd, move || {
        let mut chunk = [0u8; 65536];
        loop {
            match s.borrow_mut().read(&mut chunk) {
                Ok(0) => {
                    runtime::unwatch(fd);
                    return;
                }
                Ok(n) => buf.borrow_mut().extend_from_slice(&chunk[..n]),
                Err(_) => break,
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
