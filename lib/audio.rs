// PipeWire sinks (the subset flurry uses): sink list, default sink, cubic volume + mute
// with WirePlumber mixer semantics (device Route when the node has one, else node Props), links.
// Spoken straight over the native protocol (lib/pipewire.rs): no libpipewire.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::{FromRawFd, OwnedFd};

use crate::lib::pipewire::{self as pw, Conn, Dict, Fields, Pod, W};
use crate::ui::runtime;
use crate::ui::state::global_state;

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Sink {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub serial: u64,
    /// cubic, max over channels
    pub volume: f64,
    pub mute: bool,
    channels: Vec<f32>,
    device: Option<u32>,
    profile_device: Option<i32>,
    /// volume/mute come from the device Route (what wpctl reads), not node Props
    routed: bool,
    /// volume/mute received at least once
    known: bool,
}

impl Sink {
    pub fn known(&self) -> bool {
        self.known
    }

    /// Node Props. A device sink's are only placeholders until its Route has answered: new nodes
    /// start at 1.0 and WirePlumber restores the saved volume on the Route.
    fn set_node_props(&mut self, channels: Option<Vec<f32>>, mute: Option<bool>) {
        if self.routed {
            return;
        }
        self.known |= (channels.is_some() || mute.is_some()) && self.device.zip(self.profile_device).is_none();
        self.set_props(channels, mute);
    }

    fn set_route_props(&mut self, channels: Option<Vec<f32>>, mute: Option<bool>) {
        self.routed = true;
        self.known |= channels.is_some() || mute.is_some();
        self.set_props(channels, mute);
    }

    fn set_props(&mut self, channels: Option<Vec<f32>>, mute: Option<bool>) {
        if let Some(c) = channels {
            self.volume = c.iter().fold(0.0f32, |a, b| a.max(b.cbrt())) as f64;
            self.channels = c;
        }
        if let Some(m) = mute {
            self.mute = m;
        }
    }

    pub fn volume_icon(&self) -> &'static str {
        if self.mute || self.volume == 0.0 {
            "volume-muted"
        } else if self.volume <= 0.33 {
            "volume-low"
        } else if self.volume <= 0.66 {
            "volume-medium"
        } else if self.volume <= 1.0 {
            "volume-high"
        } else {
            "volume-over"
        }
    }
}

#[derive(Clone, Copy)]
struct Route {
    index: i32,
    device: i32,
}

struct PortInfo {
    node: u32,
    output: bool,
    channel: String,
}

/// What a proxy id is bound to; bound globals by their id.
#[derive(Clone, Copy)]
enum Proxy {
    Registry,
    Node(u32),
    Device(u32),
    Metadata(u32),
}

struct Pw {
    conn: Conn,
    registry: u32,
    proxies: HashMap<u32, Proxy>,
    /// global id -> proxy id
    nodes: HashMap<u32, u32>,
    devices: HashMap<u32, (u32, Vec<Route>)>,
    metadata: Option<u32>,
    /// the initial sync: everything known so far has been announced once it's done
    initial: i32,
    /// sink ports and sink-to-sink links only: all `link` needs
    ports: HashMap<u32, PortInfo>,
    links: HashMap<u32, (u32, u32)>, // id -> (output node, input node)
}

thread_local! {
    static PW: RefCell<Option<Pw>> = const { RefCell::new(None) };
    /// Route reads in flight: (core sync sequence, sink), answered after the device's params
    static ROUTE_READS: RefCell<Vec<(i32, u32)>> = const { RefCell::new(Vec::new()) };
    /// `default.configured.audio.sink`: the user's choice WirePlumber restores
    static CONFIGURED: RefCell<Option<String>> = const { RefCell::new(None) };
}

global_state! {
    pub fn speakers() -> Vec<Sink> = vec![];
    pub fn default_name() -> Option<String> = None;
    pub fn ready() -> bool = false;
    /// default changes are the user's: WirePlumber has restored the configured default (at login
    /// it falls back to whatever exists first, e.g. a null sink), or the user set something
    pub fn settled() -> bool = false;
}

pub fn default_speaker() -> Option<Sink> {
    let name = default_name().peek()?;
    speakers().with(|l| l.iter().find(|s| s.name == name).cloned())
}

/// try_borrow: never held across event handling, but a nested call must not panic either
fn with<R>(f: impl FnOnce(&mut Pw) -> R) -> Option<R> {
    PW.with(|p| p.try_borrow_mut().ok()?.as_mut().map(f))
}

fn update_sink(id: u32, f: impl FnOnce(&mut Sink)) {
    speakers().update(|list| {
        list.iter_mut().find(|s| s.id == id).is_some_and(|s| {
            let old = s.clone();
            f(s);
            *s != old
        })
    });
    try_settle();
}

/// Called after the update that may settle has been notified: subscribers see it unsettled, so
/// the OSD takes the restored default as its baseline instead of showing it as a switch.
fn try_settle() {
    if !settled().peek() && default_speaker().is_some_and(|s| s.known && CONFIGURED.with(|c| c.borrow().as_deref() == Some(&*s.name))) {
        settled().set(true);
    }
}

/// Connects to PipeWire, or waits for its socket to appear and connects then.
pub fn start() {
    if !connect() {
        eprintln!("flurry: pipewire: can't connect, waiting for its socket");
        wait_for_socket();
    }
}

/// `PIPEWIRE_REMOTE` (a path, or a name in the runtime dir: `PIPEWIRE_RUNTIME_DIR`, else
/// `XDG_RUNTIME_DIR`), else pipewire-0 there, as libpipewire resolves it.
fn socket_path() -> Option<String> {
    let name = std::env::var("PIPEWIRE_REMOTE").ok().filter(|r| !r.is_empty()).unwrap_or_else(|| "pipewire-0".into());
    if name.starts_with('/') {
        return Some(name);
    }
    let dir = std::env::var("PIPEWIRE_RUNTIME_DIR").ok().filter(|v| v.starts_with('/')).or_else(crate::lib::xdg::runtime_dir)?;
    Some(format!("{dir}/{name}"))
}

/// inotify on the socket's directory until it is (re)created: a restarted PipeWire that isn't
/// socket-activated binds a fresh one.
fn wait_for_socket() {
    let Some((dir, name)) = socket_path().and_then(|p| p.rsplit_once('/').map(|(d, n)| (d.to_string(), n.to_string()))) else { return };
    let Ok(cdir) = std::ffi::CString::new(if dir.is_empty() { "/".into() } else { dir }) else { return };
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    if fd < 0 {
        return;
    }
    let ino = File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    if unsafe { libc::inotify_add_watch(fd, cdir.as_ptr(), libc::IN_CREATE | libc::IN_MOVED_TO) } < 0 {
        return;
    }
    // created between the failed connect and the watch
    if connect() {
        return;
    }
    runtime::watch(fd, move || {
        let mut buf = [0u8; 4096];
        let mut hit = false;
        while let Ok(n @ 1..) = (&ino).read(&mut buf) {
            hit |= inotify_names(&buf[..n]).any(|n| n == name.as_bytes());
        }
        if hit && connect() {
            runtime::unwatch(fd);
        }
    });
}

/// The names in a buffer of inotify events.
fn inotify_names(mut buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    const HEAD: usize = size_of::<libc::inotify_event>();
    std::iter::from_fn(move || {
        let len = u32::from_ne_bytes(buf.get(HEAD - 4..HEAD)?.try_into().ok()?) as usize;
        let name = buf.get(HEAD..HEAD + len)?;
        buf = &buf[HEAD + len..];
        Some(name.split(|&b| b == 0).next().unwrap_or_default())
    })
}

fn connect() -> bool {
    let Some(mut conn) = socket_path().and_then(|p| Conn::connect(&p)) else { return false };
    let registry = conn.new_id();
    conn.send(pw::CORE, pw::CORE_GET_REGISTRY, |w| {
        w.int(3).int(registry as i32);
    });
    let initial = conn.sync();
    let fd = conn.fd();
    PW.with(|p| {
        *p.borrow_mut() = Some(Pw {
            conn,
            registry,
            proxies: HashMap::from([(registry, Proxy::Registry)]),
            nodes: HashMap::new(),
            devices: HashMap::new(),
            metadata: None,
            initial,
            ports: HashMap::new(),
            links: HashMap::new(),
        })
    });
    // messages are handled without holding PW: the handlers borrow it themselves
    runtime::watch(fd, || {
        let Some(read) = with(|p| {
            p.conn.flush();
            p.conn.recv()
        }) else {
            return;
        };
        match read {
            Ok(buf) => pw::messages(&buf).for_each(|(id, op, body)| {
                if let Some(f) = Fields::new(body) {
                    event(id, op, f);
                }
            }),
            Err(_) => lost(),
        }
    });
    true
}

/// The connection is gone (pipewire restarted): drop everything and reconnect.
fn lost() {
    if let Some(fd) = PW.with(|p| p.borrow_mut().take().map(|p| p.conn.fd())) {
        runtime::unwatch(fd);
    }
    ROUTE_READS.with(|r| r.borrow_mut().clear());
    speakers().set(vec![]);
    default_name().set(None);
    CONFIGURED.with(|c| c.borrow_mut().take());
    settled().set(false);
    ready().set(false);
    start();
}

fn event(id: u32, op: u8, mut f: Fields) -> Option<()> {
    if id == pw::CORE {
        match op {
            pw::CORE_DONE => done(f.skip(1)?.int()?),
            pw::CORE_PING => {
                let (id, seq) = (f.int()?, f.int()?);
                with(|p| {
                    p.conn.send(pw::CORE, pw::CORE_PONG, |w| {
                        w.int(id).int(seq);
                    })
                });
            }
            _ => {}
        }
        return Some(());
    }
    match (with(|p| p.proxies.get(&id).copied())??, op) {
        (Proxy::Registry, pw::REGISTRY_GLOBAL) => {
            let (gid, ty) = (f.int()? as u32, f.skip(1)?.str()??);
            global(gid, ty, &f.skip(1)?.dict()?);
        }
        (Proxy::Registry, pw::REGISTRY_GLOBAL_REMOVE) => global_remove(f.int()? as u32),
        // id, max in/out ports, change mask, in/out ports, state, error, then props
        (Proxy::Node(gid), pw::NODE_INFO) => node_info(gid, &f.skip(8)?.dict()?),
        // seq, param id, index, next, then the param
        (Proxy::Node(gid), pw::PARAM) => node_param(gid, f.skip(4)?.next()?),
        (Proxy::Device(gid), pw::PARAM) => device_param(gid, f.skip(4)?.next()?),
        (Proxy::Metadata(_), pw::METADATA_PROPERTY) => {
            let (subject, key, value) = (f.int()?, f.str()?, f.skip(1)?.str()?);
            metadata_property(subject, key, value);
        }
        _ => {}
    }
    Some(())
}

fn done(seq: i32) {
    if with(|p| p.initial == seq) == Some(true) {
        ready().set(true);
    }
    // the device has no Route for this sink: its node Props are all there is
    let read = ROUTE_READS.with(|r| {
        let mut r = r.borrow_mut();
        r.iter().position(|(s, _)| *s == seq).map(|i| r.remove(i).1)
    });
    if let Some(id) = read {
        update_sink(id, |s| s.known = true);
    }
}

/// Binds global `gid` as `ty` (version 3, all of these are) under a fresh proxy id.
fn bind(p: &mut Pw, gid: u32, ty: &str, proxy: Proxy) -> u32 {
    let id = p.conn.new_id();
    p.conn.send(p.registry, pw::REGISTRY_BIND, |w| {
        w.int(gid as i32).str(ty).int(3).int(id as i32);
    });
    p.proxies.insert(id, proxy);
    id
}

fn global(gid: u32, ty: &str, props: &Dict) {
    let class = props.get("media.class");
    match ty.strip_prefix("PipeWire:Interface:") {
        Some("Node") if class == Some("Audio/Sink") => bind_node(gid, props),
        Some("Device") if class == Some("Audio/Device") => {
            with(|p| {
                let id = bind(p, gid, "PipeWire:Interface:Device", Proxy::Device(gid));
                p.conn.send(id, pw::PARAMS_SUBSCRIBE, |w| {
                    w.ids(&[pw::PARAM_ROUTE]);
                });
                p.devices.insert(gid, (id, vec![]));
            });
        }
        Some("Metadata") if props.get("metadata.name") == Some("default") => {
            with(|p| p.metadata = Some(bind(p, gid, "PipeWire:Interface:Metadata", Proxy::Metadata(gid))));
        }
        // a node is announced before its ports and links
        Some("Port") => {
            if let Some(node) = props.parse("node.id") {
                let info = PortInfo { node, output: props.get("port.direction") == Some("out"), channel: props.get("audio.channel").unwrap_or_default().into() };
                with(|p| p.nodes.contains_key(&node).then(|| p.ports.insert(gid, info)));
            }
        }
        Some("Link") => {
            if let (Some(o), Some(i)) = (props.parse("link.output.node"), props.parse("link.input.node")) {
                with(|p| (p.nodes.contains_key(&o) && p.nodes.contains_key(&i)).then(|| p.links.insert(gid, (o, i))));
            }
        }
        _ => {}
    }
}

fn global_remove(gid: u32) {
    with(|p| {
        p.nodes.remove(&gid);
        p.devices.remove(&gid);
        p.ports.remove(&gid);
        p.links.remove(&gid);
        p.proxies.retain(|_, x| !matches!(x, Proxy::Node(g) | Proxy::Device(g) | Proxy::Metadata(g) if *g == gid));
        p.metadata = p.metadata.filter(|m| p.proxies.contains_key(m));
    });
    speakers().update(|l| {
        let n = l.len();
        l.retain(|s| s.id != gid);
        l.len() != n
    });
}

fn bind_node(id: u32, props: &Dict) {
    let sink = Sink {
        id,
        name: props.get("node.name").unwrap_or_default().into(),
        description: props.get("node.description").or_else(|| props.get("node.nick")).unwrap_or_default().into(),
        serial: props.parse("object.serial").unwrap_or(0),
        device: props.parse("device.id"),
        profile_device: props.parse("card.profile.device"),
        ..Default::default()
    };
    let bound = with(|p| {
        let proxy = bind(p, id, "PipeWire:Interface:Node", Proxy::Node(id));
        p.conn.send(proxy, pw::PARAMS_SUBSCRIBE, |w| {
            w.ids(&[pw::PARAM_PROPS]);
        });
        p.nodes.insert(id, proxy);
    });
    if bound.is_none() {
        return;
    }
    speakers().update(|l| {
        if l.last() == Some(&sink) {
            return false;
        }
        l.retain(|s| s.id != id);
        l.push(sink);
        true
    });
}

// registry globals omit card.profile.device: without it no Route matched and volume went to
// node Props, which never reach the hardware mixer and drift from what wpctl shows
fn node_info(id: u32, props: &Dict) {
    let device: Option<u32> = props.parse("device.id");
    let pd: Option<i32> = props.parse("card.profile.device");
    update_sink(id, |s| {
        s.device = device.or(s.device);
        s.profile_device = pd.or(s.profile_device);
    });
    // a device's cached Route can predate the volume restore: always ask for the current one
    let unread = speakers().with(|l| l.iter().any(|s| s.id == id && !s.known));
    if let (Some(dev), Some(_), true) = (device, pd, unread) {
        with(|p| {
            if read_routes(p, dev) {
                let seq = p.conn.sync();
                ROUTE_READS.with(|r| r.borrow_mut().push((seq, id)));
            }
        });
    }
}

fn node_param(id: u32, param: Pod) {
    if !matches!(param, Pod::Object { .. }) {
        return;
    }
    let (channels, mute) = parse_props(param);
    update_sink(id, |s| s.set_node_props(channels, mute));
    // a routed sink's volume lives on its device, whose Route param isn't re-sent when it changes
    // (only at subscribe): the node's Props change is the cue to re-read it
    let device = speakers().with(|l| l.iter().find(|s| s.id == id && s.profile_device.is_some()).and_then(|s| s.device));
    if let Some(dev) = device {
        with(|p| read_routes(p, dev));
    }
}

/// Asks device `dev` for its Routes (answered through its param events); false if it isn't bound.
fn read_routes(p: &mut Pw, dev: u32) -> bool {
    let Some((proxy, _)) = p.devices.get(&dev) else { return false };
    p.conn.send(*proxy, pw::PARAMS_ENUM, |w| {
        w.int(0).id(pw::PARAM_ROUTE).int(0).int(-1).none();
    });
    true
}

fn device_param(id: u32, param: Pod) {
    // Int or Id: the direction is an Id
    let int = |k: u32| {
        param.props().find(|p| p.0 == k).and_then(|p| match p.1 {
            Pod::Int(v) => Some(v),
            Pod::Id(v) => Some(v as i32),
            _ => None,
        })
    };
    let (Some(index), Some(device), Some(pw::DIRECTION_OUTPUT)) = (int(pw::ROUTE_INDEX), int(pw::ROUTE_DEVICE), int(pw::ROUTE_DIRECTION)) else {
        return;
    };
    let (channels, mute) = param.props().find(|p| p.0 == pw::ROUTE_PROPS).map_or((None, None), |p| parse_props(p.1));
    with(|p| {
        if let Some((_, routes)) = p.devices.get_mut(&id) {
            routes.retain(|r| r.device != device);
            routes.push(Route { index, device });
        }
    });
    speakers().update(|list| {
        let mut changed = false;
        for s in list.iter_mut().filter(|s| s.device == Some(id) && s.profile_device == Some(device)) {
            let old = s.clone();
            s.set_route_props(channels.clone(), mute);
            changed |= *s != old;
        }
        changed
    });
    try_settle();
}

fn metadata_property(subject: i32, key: Option<&str>, value: Option<&str>) {
    let name = || value.and_then(crate::lib::json::parse).and_then(|j| j.get("name").str().map(String::from));
    match key {
        Some("default.audio.sink") if subject == 0 => default_name().set(name()),
        Some("default.configured.audio.sink") if subject == 0 => CONFIGURED.with(|c| *c.borrow_mut() = name()),
        _ => return,
    }
    try_settle();
}

/// channelVolumes and mute of a Props object.
fn parse_props(o: Pod) -> (Option<Vec<f32>>, Option<bool>) {
    let mut channels = None;
    let mut mute = None;
    for (k, v) in o.props() {
        match (k, v) {
            (pw::PROP_CHANNEL_VOLUMES, v) => channels = v.floats().or(channels),
            (pw::PROP_MUTE, Pod::Bool(m)) => mute = Some(m),
            _ => {}
        }
    }
    (channels, mute)
}

fn props_object(w: &mut W, channels: Option<&[f32]>, mute: Option<bool>) {
    w.object(pw::OBJECT_PROPS, pw::PARAM_PROPS, |w| {
        if let Some(c) = channels {
            w.prop(pw::PROP_CHANNEL_VOLUMES).floats(c);
        }
        if let Some(m) = mute {
            w.prop(pw::PROP_MUTE).bool(m);
        }
    });
}

/// Writes volume/mute like WirePlumber's mixer-api: through the device Route if present, else node Props.
fn apply(sink: &Sink, channels: Option<Vec<f32>>, mute: Option<bool>) {
    settled().set(true);
    let channels = channels.as_deref();
    with(|p| {
        let route = sink.device.zip(sink.profile_device).and_then(|(dev, pd)| {
            p.devices.get(&dev).and_then(|(proxy, routes)| routes.iter().find(|r| r.device == pd).map(|r| (*proxy, *r)))
        });
        if let Some((dev, r)) = route {
            p.conn.send(dev, pw::PARAMS_SET, |w| {
                w.id(pw::PARAM_ROUTE).int(0).object(pw::OBJECT_ROUTE, pw::PARAM_ROUTE, |w| {
                    w.prop(pw::ROUTE_INDEX).int(r.index);
                    w.prop(pw::ROUTE_DEVICE).int(r.device);
                    w.prop(pw::ROUTE_PROPS);
                    props_object(w, channels, mute);
                    w.prop(pw::ROUTE_SAVE).bool(true);
                });
            });
        } else if let Some(&node) = p.nodes.get(&sink.id) {
            p.conn.send(node, pw::PARAMS_SET, |w| {
                w.id(pw::PARAM_PROPS).int(0);
                props_object(w, channels, mute);
            });
        }
    });
}

pub fn set_volume(sink: &Sink, volume: f64) {
    let v = volume.clamp(0.0, 1.5);
    let channels: Vec<f32> = if sink.channels.is_empty() {
        vec![v.powi(3) as f32; 2]
    } else {
        sink.channels
            .iter()
            .map(|c| {
                let cur = c.cbrt() as f64;
                let nv = if sink.volume == 0.0 { v } else { cur * v / sink.volume };
                nv.powi(3) as f32
            })
            .collect()
    };
    // channels too: a drag sends many steps before PipeWire echoes back, and scaling stale channels
    // by a fresh volume shrank every step (dragging to max landed at a fraction)
    update_sink(sink.id, |s| {
        s.volume = v;
        s.channels = channels.clone();
    });
    apply(sink, Some(channels), None);
}

/// Volume keys on the default speaker: `delta` steps (unmuting, clamped to 0-100%), or None toggles mute.
/// False when nothing changed (no default, or already at the end).
pub fn step_volume(delta: Option<f64>) -> bool {
    let Some(s) = default_speaker().filter(Sink::known) else { return false };
    let Some(d) = delta else {
        set_mute(&s, !s.mute);
        return true;
    };
    let v = (s.volume + d).clamp(0.0, 1.0);
    if s.mute {
        set_mute(&s, false);
    }
    if v != s.volume {
        set_volume(&s, v);
    }
    s.mute || v != s.volume
}

pub fn set_mute(sink: &Sink, mute: bool) {
    apply(sink, None, Some(mute));
}

/// `isDefault = true`: what `wpctl set-default` writes.
pub fn set_default(sink: &Sink) {
    let value = format!("{{\"name\":{}}}", crate::lib::json::quote(&sink.name));
    with(|p| {
        if let Some(md) = p.metadata {
            p.conn.send(md, pw::METADATA_SET_PROPERTY, |w| {
                w.int(0).str("default.configured.audio.sink").str("Spa:String:JSON").str(&value);
            });
        }
    });
}

/// pw-link [-d] <output node> <input node>: pairs output (monitor) ports with input ports by channel.
pub fn link(output: &str, input: &str, connect: bool) {
    let list = speakers().peek();
    let Some(o) = list.iter().find(|s| s.name == output).map(|s| s.id) else { return };
    let Some(i) = list.iter().find(|s| s.name == input).map(|s| s.id) else { return };
    with(|p| {
        if !connect {
            for (&id, _) in p.links.iter().filter(|(_, (a, b))| *a == o && *b == i) {
                p.conn.send(p.registry, pw::REGISTRY_DESTROY, |w| {
                    w.int(id as i32);
                });
            }
            return;
        }
        let ports = |node: u32, out: bool| p.ports.iter().filter(move |(_, x)| x.node == node && x.output == out);
        for (op, x) in ports(o, true) {
            let Some((ip, _)) = ports(i, false).find(|(_, y)| y.channel == x.channel) else { continue };
            // the link outlives this proxy (object.linger), which is never destroyed: nothing to keep
            let id = p.conn.new_id();
            let (op, ip, o, i) = (op.to_string(), ip.to_string(), o.to_string(), i.to_string());
            p.conn.send(pw::CORE, pw::CORE_CREATE_OBJECT, |w| {
                w.str("link-factory").str("PipeWire:Interface:Link").int(3);
                w.dict(&[("link.output.port", &op), ("link.input.port", &ip), ("link.output.node", &o), ("link.input.node", &i), ("object.linger", "true")]);
                w.int(id as i32);
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_sink_volume_comes_from_its_route() {
        let mut s = Sink { device: Some(1), profile_device: Some(3), ..Default::default() };
        s.set_node_props(Some(vec![1.0, 1.0]), Some(false));
        assert!(!s.known(), "a new node's 1.0 is not its volume yet");
        s.set_route_props(Some(vec![0.027, 0.027]), Some(false));
        assert!(s.known());
        assert!((s.volume - 0.3).abs() < 1e-3);
        s.set_node_props(Some(vec![1.0, 1.0]), None);
        assert!((s.volume - 0.3).abs() < 1e-3, "node Props never override the Route");
    }

    #[test]
    fn deviceless_sink_volume_comes_from_node_props() {
        let mut s = Sink::default();
        s.set_node_props(Some(vec![0.125, 0.125]), Some(false));
        assert!(s.known());
        assert!((s.volume - 0.5).abs() < 1e-3);
    }

    /// Against the session's PipeWire (`cargo test live_pipewire -- --ignored --nocapture`):
    /// lists the sinks, then re-applies the default's own volume and waits for its round trip.
    #[test]
    #[ignore]
    fn live_pipewire() {
        start();
        let until = |what: &str, done: &dyn Fn() -> bool| {
            let late = std::rc::Rc::new(std::cell::Cell::new(false));
            let l = late.clone();
            let _t = runtime::timeout(3000, move || l.set(true));
            while !done() && !late.get() {
                runtime::iterate(true);
            }
            assert!(done(), "timed out waiting for {what}");
        };
        until("ready", &|| ready().peek());
        for s in speakers().peek() {
            println!("{} {:?} {:?} vol={:.3} mute={} routed={} known={}", s.id, s.name, s.description, s.volume, s.mute, s.routed, s.known);
        }
        println!("default: {:?} configured: {:?}", default_name().peek(), CONFIGURED.with(|c| c.borrow().clone()));
        until("the default sink's volume", &|| default_speaker().is_some_and(|s| s.known));
        for s in speakers().peek() {
            println!("{} {:?} vol={:.3} mute={} routed={} known={}", s.id, s.name, s.volume, s.mute, s.routed, s.known);
        }
        let d = default_speaker().expect("a default sink");
        set_volume(&d, d.volume);
        let seq = with(|p| p.conn.sync()).unwrap();
        ROUTE_READS.with(|r| r.borrow_mut().push((seq, u32::MAX)));
        until("the write's round trip", &|| ROUTE_READS.with(|r| r.borrow().iter().all(|(s, _)| *s != seq)));
        let after = default_speaker().unwrap();
        assert!((after.volume - d.volume).abs() < 1e-3, "{} -> {}", d.volume, after.volume);
        assert_eq!(after.mute, d.mute);
        let wpctl = |id: u32| String::from_utf8(std::process::Command::new("wpctl").args(["get-volume", &id.to_string()]).output().unwrap().stdout).unwrap();
        let wait_for = |what: &str, id: u32, want: &dyn Fn(&Sink) -> bool| until(what, &|| speakers().peek().iter().any(|s| s.id == id && want(s)));
        // a routed sink: mute through its device Route, then back
        let routed = speakers().peek().into_iter().find(|s| s.routed).expect("a routed sink");
        set_mute(&routed, !routed.mute);
        wait_for("the routed mute", routed.id, &|s| s.mute != routed.mute);
        println!("wpctl after mute: {}", wpctl(routed.id).trim());
        set_mute(&routed, routed.mute);
        wait_for("the routed unmute", routed.id, &|s| s.mute == routed.mute);
        println!("wpctl restored: {}", wpctl(routed.id).trim());
        // a deviceless sink: node Props
        if let Some(null) = speakers().peek().into_iter().find(|s| !s.routed && s.device.is_none()) {
            let v = if null.volume > 0.5 { 0.25 } else { 0.75 };
            set_volume(&null, v);
            let seq = with(|p| p.conn.sync()).unwrap();
            ROUTE_READS.with(|r| r.borrow_mut().push((seq, u32::MAX)));
            until("the props write", &|| ROUTE_READS.with(|r| r.borrow().iter().all(|(s, _)| *s != seq)));
            println!("wpctl after set {v}: {}", wpctl(null.id).trim());
            let now = speakers().peek().into_iter().find(|s| s.id == null.id).unwrap();
            set_volume(&now, null.volume);
            let seq = with(|p| p.conn.sync()).unwrap();
            ROUTE_READS.with(|r| r.borrow_mut().push((seq, u32::MAX)));
            until("the props restore", &|| ROUTE_READS.with(|r| r.borrow().iter().all(|(s, _)| *s != seq)));
            println!("wpctl restored: {}", wpctl(null.id).trim());
        }
        // link the deviceless sink's monitor into the routed one, then unlink
        if let Some(null) = speakers().peek().into_iter().find(|s| s.device.is_none()) {
            let count = || with(|p| p.links.values().filter(|l| **l == (null.id, routed.id)).count()).unwrap();
            let before = count();
            link(&null.name, &routed.name, true);
            until("the links", &|| count() > before);
            println!("links made: {}", count() - before);
            link(&null.name, &routed.name, false);
            until("the unlink", &|| count() == 0);
        }
    }
}
