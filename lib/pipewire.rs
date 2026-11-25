// PipeWire's native protocol (v3) without libpipewire: the framing over the unix socket and the
// SPA pods its messages are made of, the subset lib/audio.rs needs. Opcodes and message layouts
// follow pipewire's module-protocol-native (protocol-native.c, module-metadata/protocol-native.c).

use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

use crate::ui::runtime;

// pod types (spa/utils/type.h)
const NONE: u32 = 1;
const BOOL: u32 = 2;
const ID: u32 = 3;
const INT: u32 = 4;
const LONG: u32 = 5;
const FLOAT: u32 = 6;
const STRING: u32 = 8;
const ARRAY: u32 = 13;
const STRUCT: u32 = 14;
const OBJECT: u32 = 15;

// spa/param: object types, param ids, property keys
pub const OBJECT_PROPS: u32 = 0x40002;
pub const OBJECT_ROUTE: u32 = 0x40009;
pub const PARAM_PROPS: u32 = 2;
pub const PARAM_ROUTE: u32 = 13;
pub const PROP_MUTE: u32 = 0x10004;
pub const PROP_CHANNEL_VOLUMES: u32 = 0x10008;
pub const ROUTE_INDEX: u32 = 1;
pub const ROUTE_DIRECTION: u32 = 2;
pub const ROUTE_DEVICE: u32 = 3;
pub const ROUTE_PROPS: u32 = 10;
pub const ROUTE_SAVE: u32 = 13;
pub const DIRECTION_OUTPUT: i32 = 1;

// the core and client proxies every connection starts with
pub const CORE: u32 = 0;
const CLIENT: u32 = 1;

// methods, by interface
pub const CORE_SYNC: u8 = 2;
pub const CORE_PONG: u8 = 3;
pub const CORE_GET_REGISTRY: u8 = 5;
pub const CORE_CREATE_OBJECT: u8 = 6;
pub const REGISTRY_BIND: u8 = 1;
pub const REGISTRY_DESTROY: u8 = 2;
pub const PARAMS_SUBSCRIBE: u8 = 1;
pub const PARAMS_ENUM: u8 = 2;
pub const PARAMS_SET: u8 = 3;
pub const METADATA_SET_PROPERTY: u8 = 1;

// events, by interface
pub const CORE_DONE: u8 = 1;
pub const CORE_PING: u8 = 2;
/// the server dropped one of our proxies: its id may be taken again
pub const CORE_REMOVE_ID: u8 = 4;
pub const REGISTRY_GLOBAL: u8 = 0;
pub const REGISTRY_GLOBAL_REMOVE: u8 = 1;
pub const NODE_INFO: u8 = 0;
/// node and device alike
pub const PARAM: u8 = 1;
pub const METADATA_PROPERTY: u8 = 0;

/// One pod, borrowed from the message it came in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pod<'a> {
    None,
    Bool(bool),
    Id(u32),
    Int(i32),
    Long(i64),
    Float(f32),
    String(&'a str),
    Array { child: u32, size: u32, items: &'a [u8] },
    Struct(&'a [u8]),
    Object { kind: u32, id: u32, props: &'a [u8] },
    Other,
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// The pod at the front of `b` and what follows it (past its padding).
fn split(b: &[u8]) -> Option<(Pod<'_>, &[u8])> {
    let (size, ty) = (u32_at(b, 0)? as usize, u32_at(b, 4)?);
    let body = b.get(8..8 + size)?;
    let rest = b.get((8 + size).next_multiple_of(8)..).unwrap_or_default();
    let pod = match ty {
        NONE => Pod::None,
        BOOL => Pod::Bool(u32_at(body, 0)? != 0),
        ID => Pod::Id(u32_at(body, 0)?),
        INT => Pod::Int(u32_at(body, 0)? as i32),
        LONG => Pod::Long(i64::from_ne_bytes(body.get(..8)?.try_into().ok()?)),
        FLOAT => Pod::Float(f32::from_bits(u32_at(body, 0)?)),
        STRING => Pod::String(std::str::from_utf8(body.split(|&c| c == 0).next()?).ok()?),
        ARRAY => Pod::Array { size: u32_at(body, 0)?, child: u32_at(body, 4)?, items: &body[8..] },
        STRUCT => Pod::Struct(body),
        OBJECT => Pod::Object { kind: u32_at(body, 0)?, id: u32_at(body, 4)?, props: body.get(8..)? },
        _ => Pod::Other,
    };
    Some((pod, rest))
}

impl<'a> Pod<'a> {
    pub fn int(self) -> Option<i32> {
        match self {
            Pod::Int(v) => Some(v),
            _ => None,
        }
    }

    /// The floats of a Float array.
    pub fn floats(self) -> Option<Vec<f32>> {
        match self {
            Pod::Array { child: FLOAT, size: 4, items } => Some(items.as_chunks::<4>().0.iter().map(|c| f32::from_ne_bytes(*c)).collect()),
            _ => None,
        }
    }

    /// An object's properties as (key, value).
    pub fn props(self) -> impl Iterator<Item = (u32, Pod<'a>)> {
        let mut b = match self {
            Pod::Object { props, .. } => props,
            _ => &[],
        };
        std::iter::from_fn(move || {
            let key = u32_at(b, 0)?;
            let (v, rest) = split(b.get(8..)?)?;
            b = rest;
            Some((key, v))
        })
    }
}

/// A message's fields, in order: its body is one Struct.
pub struct Fields<'a>(&'a [u8]);

impl<'a> Fields<'a> {
    pub fn new(msg: &'a [u8]) -> Option<Self> {
        match split(msg)?.0 {
            Pod::Struct(b) => Some(Fields(b)),
            _ => None,
        }
    }

    pub fn next(&mut self) -> Option<Pod<'a>> {
        let (p, rest) = split(self.0)?;
        self.0 = rest;
        Some(p)
    }

    pub fn int(&mut self) -> Option<i32> {
        self.next()?.int()
    }

    /// A String, or None (a null string).
    pub fn str(&mut self) -> Option<Option<&'a str>> {
        match self.next()? {
            Pod::String(s) => Some(Some(s)),
            Pod::None => Some(None),
            _ => None,
        }
    }

    pub fn skip(&mut self, n: usize) -> Option<&mut Self> {
        for _ in 0..n {
            self.next()?;
        }
        Some(self)
    }

    /// A dict: a Struct of its item count, then key and value strings.
    pub fn dict(&mut self) -> Option<Dict<'a>> {
        let Pod::Struct(b) = self.next()? else { return None };
        let mut f = Fields(b);
        let n = f.int()?.max(0) as usize;
        Some(Dict((0..n).map_while(|_| Some((f.str()??, f.str()?.unwrap_or_default()))).collect()))
    }
}

pub struct Dict<'a>(Vec<(&'a str, &'a str)>);

impl Dict<'_> {
    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.iter().find(|(key, _)| *key == k).map(|(_, v)| *v)
    }

    pub fn parse<T: std::str::FromStr>(&self, k: &str) -> Option<T> {
        self.get(k)?.parse().ok()
    }
}

/// Pod writer; every pod padded to 8 as SPA lays them out.
#[derive(Default)]
pub struct W(pub Vec<u8>);

impl W {
    fn pod(&mut self, ty: u32, body: &[u8]) -> &mut Self {
        self.nest(ty, body, |_| {})
    }

    pub fn int(&mut self, v: i32) -> &mut Self {
        self.pod(INT, &v.to_ne_bytes())
    }

    pub fn id(&mut self, v: u32) -> &mut Self {
        self.pod(ID, &v.to_ne_bytes())
    }

    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.pod(BOOL, &(v as i32).to_ne_bytes())
    }

    pub fn none(&mut self) -> &mut Self {
        self.pod(NONE, &[])
    }

    pub fn str(&mut self, s: &str) -> &mut Self {
        self.nest(STRING, s.as_bytes(), |w| w.0.push(0))
    }

    fn array(&mut self, child: u32, items: impl Iterator<Item = [u8; 4]>) -> &mut Self {
        self.nest(ARRAY, &[4u32.to_ne_bytes(), child.to_ne_bytes()].concat(), |w| items.for_each(|i| w.0.extend_from_slice(&i)))
    }

    pub fn floats(&mut self, v: &[f32]) -> &mut Self {
        self.array(FLOAT, v.iter().map(|f| f.to_ne_bytes()))
    }

    pub fn ids(&mut self, v: &[u32]) -> &mut Self {
        self.array(ID, v.iter().map(|i| i.to_ne_bytes()))
    }

    /// A pod: its header, `head`, then whatever `f` writes, sized after and padded.
    fn nest(&mut self, ty: u32, head: &[u8], f: impl FnOnce(&mut W)) -> &mut Self {
        let start = self.0.len();
        self.0.extend_from_slice(&[0; 4]);
        self.0.extend_from_slice(&ty.to_ne_bytes());
        self.0.extend_from_slice(head);
        f(self);
        let size = (self.0.len() - start - 8) as u32;
        self.0[start..start + 4].copy_from_slice(&size.to_ne_bytes());
        self.0.resize(self.0.len().next_multiple_of(8), 0);
        self
    }

    pub fn strukt(&mut self, f: impl FnOnce(&mut W)) -> &mut Self {
        self.nest(STRUCT, &[], f)
    }

    /// An object; `f` writes its properties as `prop(key)` then the value.
    pub fn object(&mut self, kind: u32, id: u32, f: impl FnOnce(&mut W)) -> &mut Self {
        self.nest(OBJECT, &[kind.to_ne_bytes(), id.to_ne_bytes()].concat(), f)
    }

    pub fn prop(&mut self, key: u32) -> &mut Self {
        self.0.extend_from_slice(&key.to_ne_bytes());
        self.0.extend_from_slice(&0u32.to_ne_bytes());
        self
    }

    pub fn dict(&mut self, items: &[(&str, &str)]) -> &mut Self {
        self.strukt(|w| {
            w.int(items.len() as i32);
            for (k, v) in items {
                w.str(k).str(v);
            }
        })
    }
}

/// The messages in `buf` (complete ones only, as `Conn::recv` returns): (proxy id, opcode, body).
pub fn messages(mut buf: &[u8]) -> impl Iterator<Item = (u32, u8, &[u8])> {
    std::iter::from_fn(move || {
        let (id, op) = (u32_at(buf, 0)?, u32_at(buf, 4)?);
        let len = (op & 0xffffff) as usize;
        let body = buf.get(16..16 + len)?;
        buf = &buf[16 + len..];
        Some((id, (op >> 24) as u8, body))
    })
}

/// A connection: nonblocking socket, the unsent tail and the unparsed head.
pub struct Conn {
    sock: UnixStream,
    out: Vec<u8>,
    inbuf: Vec<u8>,
    seq: u32,
    next_id: u32,
    /// ids the server has released (Core::RemoveId), taken before new ones
    free_ids: Vec<u32>,
}

impl Conn {
    /// Connects and says hello: the core (0) and client (1) proxies exist from here on.
    pub fn connect(path: &str) -> Option<Conn> {
        let sock = UnixStream::connect(path).ok()?;
        sock.set_nonblocking(true).ok()?;
        let mut c = Conn { sock, out: vec![], inbuf: vec![], seq: 0, next_id: 2, free_ids: vec![] };
        // Core::Hello(PW_VERSION_CORE), then Client::UpdateProperties
        c.send(CORE, 1, |w| {
            w.int(4);
        });
        let pid = std::process::id().to_string();
        c.send(CLIENT, 2, |w| {
            w.dict(&[("application.name", "flurry"), ("application.process.binary", "flurry-shell"), ("application.process.id", &pid)]);
        });
        Some(c)
    }

    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    /// A proxy id: one the server has released, else the next in line (the server takes either).
    pub fn new_id(&mut self) -> u32 {
        self.free_ids.pop().unwrap_or_else(|| {
            self.next_id += 1;
            self.next_id - 1
        })
    }

    /// Core::RemoveId: the server holds nothing more under `id`, so it may be bound again.
    pub fn release_id(&mut self, id: u32) {
        // the core and client proxies live as long as the connection
        if id > CLIENT && id < self.next_id && !self.free_ids.contains(&id) {
            self.free_ids.push(id);
        }
    }

    /// Sends method `op` on proxy `id`; `f` writes its fields, straight after the header.
    pub fn send(&mut self, id: u32, op: u8, f: impl FnOnce(&mut W)) {
        let mut w = W(std::mem::take(&mut self.out));
        let start = w.0.len();
        for v in [id, (op as u32) << 24, self.seq, 0] {
            w.0.extend_from_slice(&v.to_ne_bytes());
        }
        w.strukt(f);
        let size = (w.0.len() - start - 16) as u32;
        w.0[start + 4..start + 8].copy_from_slice(&((op as u32) << 24 | size).to_ne_bytes());
        self.out = w.0;
        self.seq = self.seq.wrapping_add(1);
        self.flush();
    }

    /// Core::Sync: answered by a Core::Done with the returned seq once everything sent before is handled.
    pub fn sync(&mut self) -> i32 {
        let seq = (self.seq & 0x3fff_ffff) as i32 | 0x4000_0000;
        self.send(CORE, CORE_SYNC, |w| {
            w.int(0).int(seq);
        });
        seq
    }

    /// Writes what the socket takes; the rest waits for POLLOUT.
    pub fn flush(&mut self) {
        while !self.out.is_empty() {
            match self.sock.write(&self.out) {
                Ok(n) => drop(self.out.drain(..n)),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => return runtime::poll_out(self.fd()),
                // the read side reports the lost connection
                Err(_) => return self.out.clear(),
            }
        }
    }

    /// Everything readable, as whole messages (a partial one waits for the rest). Err: the
    /// connection is gone. Passed fds (memory for nodes, never sent to a client like this one)
    /// are closed by the kernel, read() takes no ancillary data.
    pub fn recv(&mut self) -> std::io::Result<Vec<u8>> {
        let mut tmp = [0u8; 16384];
        loop {
            match self.sock.read(&mut tmp) {
                Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.inbuf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        let mut end = 0;
        while let Some(op) = u32_at(&self.inbuf, end + 4) {
            let next = end + 16 + (op & 0xffffff) as usize;
            if next > self.inbuf.len() {
                break;
            }
            end = next;
        }
        // the whole messages handed over as they are, a partial tail kept
        let tail = self.inbuf.split_off(end);
        Ok(std::mem::replace(&mut self.inbuf, tail))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn props(w: &mut W, channels: &[f32], mute: Option<bool>) {
        w.object(OBJECT_PROPS, PARAM_PROPS, |w| {
            w.prop(PROP_CHANNEL_VOLUMES).floats(channels);
            if let Some(m) = mute {
                w.prop(PROP_MUTE).bool(m);
            }
        });
    }

    // what libspa (through the pipewire crate) wrote for the same objects
    const PROPS: &str = "400000000f00000002000400020000000800010000000000100000000d00000004000000060000000000003f0000803e040001000000000004000000020000000100000000000000";
    const ROUTE: &str = "880000000f000000090004000d0000000100000000000000040000000400000003000000000000000300000000000000040000000400000007000000000000000a00000000000000280000000f000000020004000200000008000100000000000c0000000d00000004000000060000000000003e000000000d0000000000000004000000020000000100000000000000";

    #[test]
    fn writes_pods_like_libspa() {
        let mut w = W::default();
        props(&mut w, &[0.5, 0.25], Some(true));
        assert_eq!(w.0, hex(PROPS));
        let mut w = W::default();
        w.object(OBJECT_ROUTE, PARAM_ROUTE, |w| {
            w.prop(ROUTE_INDEX).int(3);
            w.prop(ROUTE_DEVICE).int(7);
            w.prop(ROUTE_PROPS);
            props(w, &[0.125], None);
            w.prop(ROUTE_SAVE).bool(true);
        });
        assert_eq!(w.0, hex(ROUTE));
    }

    #[test]
    fn reads_pods() {
        let b = hex(ROUTE);
        let (route, rest) = split(&b).unwrap();
        assert!(rest.is_empty());
        let p: Vec<_> = route.props().collect();
        assert_eq!(p[0], (ROUTE_INDEX, Pod::Int(3)));
        assert_eq!(p[1], (ROUTE_DEVICE, Pod::Int(7)));
        let inner: Vec<_> = p[2].1.props().collect();
        assert_eq!(inner[0].1.floats(), Some(vec![0.125]));
        assert_eq!(p[3], (ROUTE_SAVE, Pod::Bool(true)));
    }

    #[test]
    fn frames_messages() {
        let mut w = W::default();
        w.strukt(|w| {
            w.int(7).str("k").none().dict(&[("a", "b")]).ids(&[2, 13]);
        });
        let mut buf = vec![];
        for (id, op) in [(3u32, 1u8), (0, 2)] {
            for v in [id, (op as u32) << 24 | w.0.len() as u32, 0, 0] {
                buf.extend_from_slice(&v.to_ne_bytes());
            }
            buf.extend_from_slice(&w.0);
        }
        let m: Vec<_> = messages(&buf).collect();
        assert_eq!((m.len(), m[0].0, m[0].1, m[1].0, m[1].1), (2, 3, 1, 0, 2));
        let mut f = Fields::new(m[0].2).unwrap();
        assert_eq!(f.int(), Some(7));
        assert_eq!(f.str(), Some(Some("k")));
        assert_eq!(f.str(), Some(None));
        assert_eq!(f.dict().unwrap().get("a"), Some("b"));
        assert!(matches!(f.next(), Some(Pod::Array { child: ID, size: 4, items }) if items.len() == 8));
        assert_eq!(f.next(), None);
    }
}
