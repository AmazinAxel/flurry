// PipeWire sinks (the AstalWp subset flurry uses): sink list, default sink, cubic volume + mute
// with WirePlumber mixer semantics (device Route when the node has one, else node Props), links.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::os::fd::AsRawFd;
use std::rc::Rc;

use pipewire as pw;
use pw::spa;
use spa::pod::serialize::PodSerializer;
use spa::pod::{Object, Pod, Property, PropertyFlags, Value, ValueArray, deserialize::PodDeserializer};

use crate::lib::state::State;
use crate::ui::runtime;

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Sink {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub serial: u64,
    /// cubic, max over channels (AstalWp)
    pub volume: f64,
    pub mute: bool,
    channels: Vec<f32>,
    device: Option<u32>,
    profile_device: Option<i32>,
}

impl Sink {
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

#[derive(Clone)]
struct Route {
    index: i32,
    device: i32,
}

struct PortInfo {
    node: u32,
    output: bool,
    monitor: bool,
    channel: String,
}

struct Pw {
    _main: pw::main_loop::MainLoop,
    _context: pw::context::Context,
    core: pw::core::Core,
    registry: Rc<pw::registry::Registry>,
    nodes: HashMap<u32, (pw::node::Node, pw::node::NodeListener)>,
    devices: HashMap<u32, (pw::device::Device, pw::device::DeviceListener, Vec<Route>)>,
    metadata: Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>,
    ports: HashMap<u32, PortInfo>,
    links: HashMap<u32, (u32, u32)>, // id -> (output node, input node)
    pending_links: Vec<pw::link::Link>,
    _registry_listener: Option<pw::registry::Listener>,
}

thread_local! {
    static PW: RefCell<Option<Pw>> = const { RefCell::new(None) };
    static SINKS: State<Vec<Sink>> = State::new(vec![]);
    static DEFAULT: State<Option<String>> = State::new(None);
    static READY: State<bool> = State::new(false);
}

pub fn speakers() -> State<Vec<Sink>> {
    SINKS.with(Clone::clone)
}
pub fn default_name() -> State<Option<String>> {
    DEFAULT.with(Clone::clone)
}
pub fn ready() -> State<bool> {
    READY.with(Clone::clone)
}

pub fn default_speaker() -> Option<Sink> {
    let name = default_name().peek()?;
    speakers().peek().into_iter().find(|s| s.name == name)
}

fn with<R>(f: impl FnOnce(&mut Pw) -> R) -> Option<R> {
    // try_borrow: a callback re-entering while PW is borrowed must not panic across the C boundary
    PW.with(|p| p.try_borrow_mut().ok()?.as_mut().map(f))
}

fn update_sink(id: u32, f: impl FnOnce(&mut Sink)) {
    speakers().update(|list| {
        if let Some(s) = list.iter_mut().find(|s| s.id == id) {
            f(s);
        }
    });
}

pub fn start() {
    pw::init();
    let Ok(main) = pw::main_loop::MainLoop::new(None) else { return };
    let Ok(context) = pw::context::Context::new(&main) else { return };
    let Ok(core) = context.connect(None) else { return };
    let Ok(registry) = core.get_registry() else { return };
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
                    bind_node(&reg, g.id, &get);
                }
                pw::types::ObjectType::Device if get("media.class").as_deref() == Some("Audio/Device") => bind_device(&reg, g.id),
                pw::types::ObjectType::Metadata if get("metadata.name").as_deref() == Some("default") => bind_metadata(&reg, g.id),
                pw::types::ObjectType::Port => {
                    let node = get("node.id").and_then(|v| v.parse().ok());
                    if let Some(node) = node {
                        let info = PortInfo {
                            node,
                            output: get("port.direction").as_deref() == Some("out"),
                            monitor: get("port.monitor").as_deref() == Some("true"),
                            channel: get("audio.channel").unwrap_or_default(),
                        };
                        with(|p| p.ports.insert(g.id, info));
                    }
                }
                pw::types::ObjectType::Link => {
                    let out = get("link.output.node").and_then(|v| v.parse().ok());
                    let inp = get("link.input.node").and_then(|v| v.parse().ok());
                    if let (Some(o), Some(i)) = (out, inp) {
                        with(|p| p.links.insert(g.id, (o, i)));
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
            speakers().update(|l| l.retain(|s| s.id != id));
        })
        .register();

    // initial sync done: everything known so far has been announced
    let done_core = core.clone();
    let pending = core.sync(0).ok();
    let core_listener = done_core
        .add_listener_local()
        .done(move |_, seq| {
            if Some(seq) == pending {
                ready().set(true);
            }
        })
        .error(move |id, _, res, _| {
            // core connection lost (pipewire restarted): drop everything and reconnect, like AstalWp
            if id == 0 && res == -libc::EPIPE {
                runtime::idle(|| {
                    let fd = PW.with(|p| p.borrow_mut().take().map(|p| p._main.loop_().fd().as_raw_fd()));
                    if let Some(fd) = fd {
                        runtime::unwatch(fd);
                    }
                    speakers().set(vec![]);
                    default_name().set(None);
                    ready().set(false);
                    runtime::timeout(1000, start);
                });
            }
        })
        .register();
    std::mem::forget(core_listener);

    PW.with(|p| {
        *p.borrow_mut() = Some(Pw {
            _main: main,
            _context: context,
            core,
            registry,
            nodes: HashMap::new(),
            devices: HashMap::new(),
            metadata: None,
            ports: HashMap::new(),
            links: HashMap::new(),
            pending_links: vec![],
            _registry_listener: Some(listener),
        })
    });
    // the loop is dispatched without holding PW: its callbacks (registry, params) borrow PW themselves
    let main_loop = PW.with(|p| p.borrow().as_ref().map(|p| p._main.clone()));
    if let Some(main_loop) = main_loop {
        runtime::watch(fd, move || {
            main_loop.loop_().iterate(std::time::Duration::ZERO);
        });
    }
}

fn bind_node(reg: &pw::registry::Registry, id: u32, get: &dyn Fn(&str) -> Option<String>) {
    let sink = Sink {
        id,
        name: get("node.name").unwrap_or_default(),
        description: get("node.description").or_else(|| get("node.nick")).unwrap_or_default(),
        serial: get("object.serial").and_then(|v| v.parse().ok()).unwrap_or(0),
        device: get("device.id").and_then(|v| v.parse().ok()),
        profile_device: get("card.profile.device").and_then(|v| v.parse().ok()),
        ..Default::default()
    };
    let Ok(node) = reg.bind::<pw::node::Node, _>(&global_props(id)) else { return };
    let listener = node
        .add_listener_local()
        .param(move |_, _, _, _, pod| {
            let Some(pod) = pod else { return };
            if let Some((channels, mute)) = parse_props(pod.as_bytes()) {
                update_sink(id, |s| {
                    if let Some(c) = channels {
                        s.volume = c.iter().fold(0.0f32, |a, b| a.max(b.cbrt())) as f64;
                        s.channels = c;
                    }
                    if let Some(m) = mute {
                        s.mute = m;
                    }
                });
            }
        })
        .register();
    node.subscribe_params(&[spa::param::ParamType::Props]);
    speakers().update(|l| {
        l.retain(|s| s.id != id);
        l.push(sink);
    });
    with(|p| p.nodes.insert(id, (node, listener)));
}

fn global_props(id: u32) -> pw::registry::GlobalObject<pw::properties::Properties> {
    pw::registry::GlobalObject { id, permissions: pw::permissions::PermissionFlags::all(), type_: pw::types::ObjectType::Node, version: 3, props: None }
}

fn bind_device(reg: &pw::registry::Registry, id: u32) {
    let mut g = global_props(id);
    g.type_ = pw::types::ObjectType::Device;
    let Ok(dev) = reg.bind::<pw::device::Device, _>(&g) else { return };
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
            with(|p| {
                if let Some((_, _, routes)) = p.devices.get_mut(&id) {
                    routes.retain(|r| r.device != device);
                    routes.push(Route { index, device });
                }
            });
        })
        .register();
    dev.subscribe_params(&[spa::param::ParamType::Route]);
    with(|p| p.devices.insert(id, (dev, listener, vec![])));
}

fn bind_metadata(reg: &pw::registry::Registry, id: u32) {
    let mut g = global_props(id);
    g.type_ = pw::types::ObjectType::Metadata;
    let Ok(md) = reg.bind::<pw::metadata::Metadata, _>(&g) else { return };
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
    with(|p| p.metadata = Some((md, listener)));
}

fn deserialize(bytes: &[u8]) -> Option<Value> {
    PodDeserializer::deserialize_any_from(bytes).ok().map(|(_, v)| v)
}

fn parse_props(bytes: &[u8]) -> Option<(Option<Vec<f32>>, Option<bool>)> {
    let Value::Object(o) = deserialize(bytes)? else { return None };
    let mut channels = None;
    let mut mute = None;
    for p in o.properties {
        match (p.key, p.value) {
            (spa::sys::SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(v))) => channels = Some(v),
            (spa::sys::SPA_PROP_mute, Value::Bool(m)) => mute = Some(m),
            _ => {}
        }
    }
    Some((channels, mute))
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
            p.devices.get(&dev).and_then(|(d, _, routes)| routes.iter().find(|r| r.device == pd).map(|r| (d, r.clone())))
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
        } else if let Some((node, _)) = p.nodes.get(&sink.id)
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
        vec![(v.powi(3)) as f32; 2]
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
    update_sink(sink.id, |s| s.volume = v);
    apply(sink, Some(channels), None);
}

pub fn set_mute(sink: &Sink, mute: bool) {
    apply(sink, None, Some(mute));
}

/// `isDefault = true`: what `wpctl set-default` writes.
pub fn set_default(sink: &Sink) {
    let value = format!("{{\"name\":{}}}", crate::lib::json::quote(&sink.name));
    with(|p| {
        if let Some((md, _)) = &p.metadata {
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
        if !connect {
            let ids: Vec<u32> = p.links.iter().filter(|(_, (a, b))| *a == o && *b == i).map(|(id, _)| *id).collect();
            for id in ids {
                p.registry.destroy_global(id);
            }
            return;
        }
        let outs: Vec<(u32, String)> = p.ports.iter().filter(|(_, x)| x.node == o && x.output).map(|(id, x)| (*id, x.channel.clone())).collect();
        let ins: Vec<(u32, String)> = p.ports.iter().filter(|(_, x)| x.node == i && !x.output && !x.monitor).map(|(id, x)| (*id, x.channel.clone())).collect();
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
