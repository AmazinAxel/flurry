// PipeWire sinks (the subset flurry uses): sink list, default sink, cubic volume + mute
// with WirePlumber mixer semantics (device Route when the node has one, else node Props), links.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Cursor, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;

use pipewire as pw;
use pw::spa;
use spa::pod::serialize::PodSerializer;
use spa::pod::{Object, Pod, Property, PropertyFlags, Value, ValueArray, deserialize::PodDeserializer};

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
            "audio-volume-muted-symbolic"
        } else if self.volume <= 0.33 {
            "audio-volume-low-symbolic"
        } else if self.volume <= 0.66 {
            "audio-volume-medium-symbolic"
        } else if self.volume <= 1.0 {
            "audio-volume-high-symbolic"
        } else {
            "audio-volume-overamplified-symbolic"
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

type Global<'a> = pw::registry::GlobalObject<&'a spa::utils::dict::DictRef>;

struct Pw {
    // fields (and tuples) drop in order: listeners first, a hook before the object it hangs on, then
    // proxies, registry, core, context and the loop they all live on
    _core_listener: pw::core::Listener,
    _registry_listener: Option<pw::registry::Listener>,
    /// link proxies from the last `link` call, kept until the next (object.linger keeps the links)
    pending_links: Vec<pw::link::Link>,
    metadata: Option<(pw::metadata::MetadataListener, pw::metadata::Metadata)>,
    nodes: HashMap<u32, (pw::node::NodeListener, pw::node::Node)>,
    devices: HashMap<u32, (pw::device::DeviceListener, pw::device::Device, Vec<Route>)>,
    registry: Rc<pw::registry::Registry>,
    core: pw::core::Core,
    _context: pw::context::Context,
    _main: pw::main_loop::MainLoop,
    /// sink ports and sink-to-sink links only: all `link` needs
    ports: HashMap<u32, PortInfo>,
    links: HashMap<u32, (u32, u32)>, // id -> (output node, input node)
}

thread_local! {
    static PW: RefCell<Option<Pw>> = const { RefCell::new(None) };
    /// Route reads in flight: (core sync sequence, sink), answered after the device's params
    static ROUTE_READS: RefCell<Vec<(spa::utils::result::AsyncSeq, u32)>> = const { RefCell::new(Vec::new()) };
    /// a reconnect is queued: one per lost connection, however many errors it reports
    static RECONNECT: Cell<bool> = const { Cell::new(false) };
}

global_state! {
    pub fn speakers() -> Vec<Sink> = vec![];
    pub fn default_name() -> Option<String> = None;
    pub fn ready() -> bool = false;
}

pub fn default_speaker() -> Option<Sink> {
    let name = default_name().peek()?;
    speakers().with(|l| l.iter().find(|s| s.name == name).cloned())
}

fn with<R>(f: impl FnOnce(&mut Pw) -> R) -> Option<R> {
    // try_borrow: a callback re-entering while PW is borrowed must not panic across the C boundary
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
}

/// Connects to PipeWire, or waits for its socket to appear and connects then.
pub fn start() {
    pw::init();
    if !connect() {
        eprintln!("flurry: pipewire: can't connect, waiting for its socket");
        wait_for_socket();
    }
}

/// inotify on the runtime dir until the socket is (re)created: a restarted PipeWire that isn't
/// socket-activated binds a fresh one.
fn wait_for_socket() {
    let Some(dir) = crate::lib::xdg::runtime_dir() else { return };
    let name = std::env::var("PIPEWIRE_REMOTE").ok().filter(|r| !r.is_empty() && !r.contains('/')).unwrap_or_else(|| "pipewire-0".into());
    let Ok(cdir) = std::ffi::CString::new(dir) else { return };
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
    let Ok(main) = pw::main_loop::MainLoop::new(None) else { return false };
    let Ok(context) = pw::context::Context::new(&main) else { return false };
    let Ok(core) = context.connect(None) else { return false };
    let Ok(registry) = core.get_registry() else { return false };
    let registry = Rc::new(registry);
    let fd = main.loop_().fd().as_raw_fd();

    let reg = Rc::downgrade(&registry);
    let listener = registry
        .add_listener_local()
        .global(move |g| {
            let Some(reg) = reg.upgrade() else { return };
            let props = g.props;
            let get = |k: &str| props.and_then(|p| p.get(k)).map(String::from);
            match g.type_ {
                pw::types::ObjectType::Node if get("media.class").as_deref() == Some("Audio/Sink") => {
                    bind_node(&reg, g, &get);
                }
                pw::types::ObjectType::Device if get("media.class").as_deref() == Some("Audio/Device") => bind_device(&reg, g),
                pw::types::ObjectType::Metadata if get("metadata.name").as_deref() == Some("default") => bind_metadata(&reg, g),
                // a node is announced before its ports and links
                pw::types::ObjectType::Port => {
                    if let Some(node) = get("node.id").and_then(|v| v.parse().ok()) {
                        let info = PortInfo { node, output: get("port.direction").as_deref() == Some("out"), channel: get("audio.channel").unwrap_or_default() };
                        with(|p| p.nodes.contains_key(&node).then(|| p.ports.insert(g.id, info)));
                    }
                }
                pw::types::ObjectType::Link => {
                    let out = get("link.output.node").and_then(|v| v.parse().ok());
                    let inp = get("link.input.node").and_then(|v| v.parse().ok());
                    if let (Some(o), Some(i)) = (out, inp) {
                        with(|p| (p.nodes.contains_key(&o) && p.nodes.contains_key(&i)).then(|| p.links.insert(g.id, (o, i))));
                    }
                }
                _ => {}
            }
        })
        .global_remove(|id| {
            with(|p| {
                p.nodes.remove(&id);
                p.devices.remove(&id);
                p.ports.remove(&id);
                p.links.remove(&id);
            });
            speakers().update(|l| {
                let n = l.len();
                l.retain(|s| s.id != id);
                l.len() != n
            });
        })
        .register();

    // initial sync done: everything known so far has been announced
    let pending = core.sync(0).ok();
    let core_listener = core
        .add_listener_local()
        .done(move |_, seq| {
            if Some(seq) == pending {
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
        })
        .error(move |id, _, res, _| {
            // core connection lost (pipewire restarted): drop everything and reconnect
            if id == 0 && res == -libc::EPIPE && !RECONNECT.replace(true) {
                runtime::idle(|| {
                    RECONNECT.set(false);
                    let fd = PW.with(|p| p.borrow_mut().take().map(|p| p._main.loop_().fd().as_raw_fd()));
                    if let Some(fd) = fd {
                        runtime::unwatch(fd);
                    }
                    ROUTE_READS.with(|r| r.borrow_mut().clear());
                    speakers().set(vec![]);
                    default_name().set(None);
                    ready().set(false);
                    start();
                });
            }
        })
        .register();

    let main_loop = main.clone();
    PW.with(|p| {
        *p.borrow_mut() = Some(Pw {
            _core_listener: core_listener,
            _registry_listener: Some(listener),
            pending_links: vec![],
            metadata: None,
            nodes: HashMap::new(),
            devices: HashMap::new(),
            registry,
            core,
            _context: context,
            _main: main,
            ports: HashMap::new(),
            links: HashMap::new(),
        })
    });
    // the loop is dispatched without holding PW: its callbacks (registry, params) borrow PW themselves
    runtime::watch(fd, move || {
        main_loop.loop_().iterate(std::time::Duration::ZERO);
    });
    true
}

fn bind_node(reg: &pw::registry::Registry, g: &Global, get: &dyn Fn(&str) -> Option<String>) {
    let id = g.id;
    let sink = Sink {
        id,
        name: get("node.name").unwrap_or_default(),
        description: get("node.description").or_else(|| get("node.nick")).unwrap_or_default(),
        serial: get("object.serial").and_then(|v| v.parse().ok()).unwrap_or(0),
        device: get("device.id").and_then(|v| v.parse().ok()),
        profile_device: get("card.profile.device").and_then(|v| v.parse().ok()),
        ..Default::default()
    };
    let Ok(node) = reg.bind::<pw::node::Node, _>(g) else { return };
    let listener = node
        .add_listener_local()
        // registry globals omit card.profile.device: without it no Route matched and volume went to
        // node Props, which never reach the hardware mixer and drift from what wpctl shows
        .info(move |info| {
            let Some(props) = info.props() else { return };
            let device: Option<u32> = props.get("device.id").and_then(|v| v.parse().ok());
            let pd: Option<i32> = props.get("card.profile.device").and_then(|v| v.parse().ok());
            update_sink(id, |s| {
                s.device = device.or(s.device);
                s.profile_device = pd.or(s.profile_device);
            });
            // a device's cached Route can predate the volume restore: always ask for the current one
            let unread = speakers().with(|l| l.iter().any(|s| s.id == id && !s.known));
            if let (Some(dev), Some(_), true) = (device, pd, unread) {
                with(|p| {
                    if read_routes(p, dev)
                        && let Ok(seq) = p.core.sync(0)
                    {
                        ROUTE_READS.with(|r| r.borrow_mut().push((seq, id)));
                    }
                });
            }
        })
        .param(move |_, _, _, _, pod| {
            let Some(pod) = pod else { return };
            if let Some(Value::Object(o)) = deserialize(pod.as_bytes()) {
                let (channels, mute) = parse_props(o);
                update_sink(id, |s| s.set_node_props(channels, mute));
                // a routed sink's volume lives on its device, whose Route param isn't re-sent when
                // it changes (only at subscribe): the node's Props change is the cue to re-read it
                let device = speakers().with(|l| l.iter().find(|s| s.id == id && s.profile_device.is_some()).and_then(|s| s.device));
                if let Some(dev) = device {
                    with(|p| read_routes(p, dev));
                }
            }
        })
        .register();
    node.subscribe_params(&[spa::param::ParamType::Props]);
    speakers().update(|l| {
        if l.last() == Some(&sink) {
            return false;
        }
        l.retain(|s| s.id != id);
        l.push(sink);
        true
    });
    with(|p| p.nodes.insert(id, (listener, node)));
}

/// Asks device `dev` for its Routes (answered through its param listener); false if it isn't bound.
fn read_routes(p: &Pw, dev: u32) -> bool {
    let Some((_, d, _)) = p.devices.get(&dev) else { return false };
    d.enum_params(0, Some(spa::param::ParamType::Route), 0, u32::MAX);
    true
}

fn bind_device(reg: &pw::registry::Registry, g: &Global) {
    let id = g.id;
    let Ok(dev) = reg.bind::<pw::device::Device, _>(g) else { return };
    let listener = dev
        .add_listener_local()
        .param(move |_, _, _, _, pod| {
            let Some(pod) = pod else { return };
            let Some(Value::Object(o)) = deserialize(pod.as_bytes()) else { return };
            let int = |k: u32| o.properties.iter().find(|p| p.key == k).and_then(|p| match p.value {
                Value::Int(v) => Some(v),
                Value::Id(v) => Some(v.0 as i32),
                _ => None,
            });
            let (Some(index), Some(device), Some(dir)) =
                (int(spa::sys::SPA_PARAM_ROUTE_index), int(spa::sys::SPA_PARAM_ROUTE_device), int(spa::sys::SPA_PARAM_ROUTE_direction))
            else {
                return;
            };
            if dir != spa::sys::SPA_DIRECTION_OUTPUT as i32 {
                return;
            }
            let (channels, mute) = match o.properties.into_iter().find(|p| p.key == spa::sys::SPA_PARAM_ROUTE_props).map(|p| p.value) {
                Some(Value::Object(props)) => parse_props(props),
                _ => (None, None),
            };
            with(|p| {
                if let Some((_, _, routes)) = p.devices.get_mut(&id) {
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
        })
        .register();
    dev.subscribe_params(&[spa::param::ParamType::Route]);
    with(|p| p.devices.insert(id, (listener, dev, vec![])));
}

fn bind_metadata(reg: &pw::registry::Registry, g: &Global) {
    let Ok(md) = reg.bind::<pw::metadata::Metadata, _>(g) else { return };
    let listener = md
        .add_listener_local()
        .property(|subject, key, _, value| {
            if subject == 0 && key == Some("default.audio.sink") {
                let name = value.and_then(crate::lib::json::parse).and_then(|j| j.get("name").str().map(String::from));
                default_name().set(name);
            }
            0
        })
        .register();
    with(|p| p.metadata = Some((listener, md)));
}

fn deserialize(bytes: &[u8]) -> Option<Value> {
    PodDeserializer::deserialize_any_from(bytes).ok().map(|(_, v)| v)
}

fn parse_props(o: Object) -> (Option<Vec<f32>>, Option<bool>) {
    let mut channels = None;
    let mut mute = None;
    for p in o.properties {
        match (p.key, p.value) {
            (spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(v))) => channels = Some(v),
            (spa::sys::SPA_PROP_mute, Value::Bool(m)) => mute = Some(m),
            _ => {}
        }
    }
    (channels, mute)
}

fn serialize(v: Value) -> Option<Vec<u8>> {
    PodSerializer::serialize(Cursor::new(Vec::new()), &v).ok().map(|(c, _)| c.into_inner())
}

fn props_object(channels: Option<Vec<f32>>, mute: Option<bool>) -> Object {
    let mut properties = vec![];
    if let Some(c) = channels {
        properties.push(Property { key: spa::sys::SPA_PROP_channelVolumes, flags: PropertyFlags::empty(), value: Value::ValueArray(ValueArray::Float(c)) });
    }
    if let Some(m) = mute {
        properties.push(Property { key: spa::sys::SPA_PROP_mute, flags: PropertyFlags::empty(), value: Value::Bool(m) });
    }
    Object { type_: spa::sys::SPA_TYPE_OBJECT_Props, id: spa::sys::SPA_PARAM_Props, properties }
}

/// Writes volume/mute like WirePlumber's mixer-api: through the device Route if present, else node Props.
fn apply(sink: &Sink, channels: Option<Vec<f32>>, mute: Option<bool>) {
    let props = props_object(channels, mute);
    with(|p| {
        let route = sink.device.zip(sink.profile_device).and_then(|(dev, pd)| {
            p.devices.get(&dev).and_then(|(_, d, routes)| routes.iter().find(|r| r.device == pd).map(|r| (d, *r)))
        });
        if let Some((dev, r)) = route {
            let obj = Object {
                type_: spa::sys::SPA_TYPE_OBJECT_ParamRoute,
                id: spa::sys::SPA_PARAM_Route,
                properties: vec![
                    Property { key: spa::sys::SPA_PARAM_ROUTE_index, flags: PropertyFlags::empty(), value: Value::Int(r.index) },
                    Property { key: spa::sys::SPA_PARAM_ROUTE_device, flags: PropertyFlags::empty(), value: Value::Int(r.device) },
                    Property { key: spa::sys::SPA_PARAM_ROUTE_props, flags: PropertyFlags::empty(), value: Value::Object(props) },
                    Property { key: spa::sys::SPA_PARAM_ROUTE_save, flags: PropertyFlags::empty(), value: Value::Bool(true) },
                ],
            };
            if let Some(bytes) = serialize(Value::Object(obj))
                && let Some(pod) = Pod::from_bytes(&bytes)
            {
                dev.set_param(spa::param::ParamType::Route, 0, pod);
            }
        } else if let Some((_, node)) = p.nodes.get(&sink.id)
            && let Some(bytes) = serialize(Value::Object(props))
            && let Some(pod) = Pod::from_bytes(&bytes)
        {
            node.set_param(spa::param::ParamType::Props, 0, pod);
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

pub fn set_mute(sink: &Sink, mute: bool) {
    apply(sink, None, Some(mute));
}

/// `isDefault = true`: what `wpctl set-default` writes.
pub fn set_default(sink: &Sink) {
    let value = format!("{{\"name\":{}}}", crate::lib::json::quote(&sink.name));
    with(|p| {
        if let Some((_, md)) = &p.metadata {
            md.set_property(0, "default.configured.audio.sink", Some("Spa:String:JSON"), Some(&value));
        }
    });
}

/// pw-link [-d] <output node> <input node>: pairs output (monitor) ports with input ports by channel.
pub fn link(output: &str, input: &str, connect: bool) {
    let list = speakers().peek();
    let Some(o) = list.iter().find(|s| s.name == output).map(|s| s.id) else { return };
    let Some(i) = list.iter().find(|s| s.name == input).map(|s| s.id) else { return };
    with(|p| {
        p.pending_links.clear();
        if !connect {
            let ids: Vec<u32> = p.links.iter().filter(|(_, (a, b))| *a == o && *b == i).map(|(id, _)| *id).collect();
            for id in ids {
                p.registry.destroy_global(id);
            }
            return;
        }
        let outs: Vec<(u32, String)> = p.ports.iter().filter(|(_, x)| x.node == o && x.output).map(|(id, x)| (*id, x.channel.clone())).collect();
        let ins: Vec<(u32, String)> = p.ports.iter().filter(|(_, x)| x.node == i && !x.output).map(|(id, x)| (*id, x.channel.clone())).collect();
        for (op, ch) in &outs {
            let Some((ip, _)) = ins.iter().find(|(_, c)| c == ch) else { continue };
            let props = pw::properties::properties! {
                "link.output.port" => op.to_string(),
                "link.input.port" => ip.to_string(),
                "link.output.node" => o.to_string(),
                "link.input.node" => i.to_string(),
                "object.linger" => "true",
            };
            if let Ok(l) = p.core.create_object::<pw::link::Link>("link-factory", &props) {
                p.pending_links.push(l);
            }
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
}
