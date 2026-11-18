// Wired links over rtnetlink: Ethernet ports and adapters, and USB tethering (a phone showing up as
// a USB network card). Link and route changes arrive on a netlink socket in the poll loop; a link
// counts as online once it carries a default route.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::os::fd::RawFd;
use std::path::Path;

use crate::ui::runtime;
use crate::ui::state::global_state;

#[derive(Clone, PartialEq)]
pub struct Wired {
    pub name: String,
    /// USB tethering rather than an Ethernet port or adapter
    pub tether: bool,
    /// carries a default route; false while it is still getting an address
    pub online: bool,
}

global_state! {
    /// Physical wired links that are up, by name.
    pub fn links() -> Vec<Wired> = vec![];
}

/// Whether a wired link carries a default route.
pub fn online() -> bool {
    links().with(|l| l.iter().any(|w| w.online))
}

const NLM_F_REQUEST: u16 = 1;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_DONE: u16 = 3;
const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const RTM_GETROUTE: u16 = 26;
const RTMGRP_LINK: u32 = 1;
const RTMGRP_IPV4_ROUTE: u32 = 0x40;
const RTMGRP_IPV6_ROUTE: u32 = 0x400;
const IFLA_IFNAME: u16 = 3;
const IFLA_OPERSTATE: u16 = 16;
const IF_OPER_UNKNOWN: u8 = 0;
const IF_OPER_UP: u8 = 6;
const IFF_LOWER_UP: u32 = 0x10000;
const ARPHRD_ETHER: u16 = 1;
const RTA_GATEWAY: u16 = 5;
const RTA_OIF: u16 = 4;
const RTA_PRIORITY: u16 = 6;
const RTA_TABLE: u16 = 15;
const RT_TABLE_MAIN: u32 = 254;
const RTN_UNICAST: u8 = 1;
const SEQ_LINKS: u32 = 1;
const SEQ_ROUTES: u32 = 2;

/// USB network drivers phones use for tethering (Android: RNDIS or NCM, iPhone: ipheth).
const TETHER_DRIVERS: &[&str] = &["rndis_host", "cdc_ncm", "cdc_ether", "cdc_eem", "ipheth"];

struct Link {
    name: String,
    up: bool,
    /// tether; None for wireless and virtual links
    kind: Option<bool>,
}

/// A default route: (family, output link, metric, gateway).
type Route = (u8, u32, u32, Vec<u8>);

thread_local! {
    static FD: Cell<RawFd> = const { Cell::new(-1) };
    static LINKS: RefCell<HashMap<u32, Link>> = RefCell::default();
    static ROUTES: RefCell<HashSet<Route>> = RefCell::default();
}

/// Whether `name` is USB tethering, from sysfs; wireless and virtual links (no backing device) are
/// None.
fn classify(name: &str) -> Option<bool> {
    let base = Path::new("/sys/class/net").join(name);
    if base.join("wireless").exists() || base.join("phy80211").exists() {
        return None;
    }
    let dev = base.join("device");
    let leaf = |p: &Path| p.read_link().ok().and_then(|t| t.file_name().map(|f| f.to_string_lossy().into_owned()));
    let driver = leaf(&dev.join("driver"))?;
    Some(leaf(&dev.join("subsystem")).as_deref() == Some("usb") && TETHER_DRIVERS.contains(&driver.as_str()))
}

fn request(ty: u16, seq: u32, body: &[u8]) {
    let len = 16 + body.len();
    let mut b = Vec::with_capacity(len);
    b.extend((len as u32).to_ne_bytes());
    b.extend(ty.to_ne_bytes());
    b.extend((NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    b.extend(seq.to_ne_bytes());
    b.extend(0u32.to_ne_bytes());
    b.extend(body);
    unsafe { libc::send(FD.with(Cell::get), b.as_ptr().cast(), b.len(), 0) };
}

/// Starts over from a link dump; the route dump follows once it is done (one dump at a time).
fn resync() {
    LINKS.with(|l| l.borrow_mut().clear());
    ROUTES.with(|r| r.borrow_mut().clear());
    // an ifinfomsg for any family
    request(RTM_GETLINK, SEQ_LINKS, &[0; 16]);
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_ne_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_ne_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// rtattrs: (type, payload).
fn attrs(mut b: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        if b.len() < 4 {
            return None;
        }
        let len = u16_at(b, 0) as usize;
        if len < 4 || len > b.len() {
            return None;
        }
        let item = (u16_at(b, 2) & 0x3fff, &b[4..len]);
        b = &b[((len + 3) & !3).min(b.len())..];
        Some(item)
    })
}

fn on_link(ty: u16, body: &[u8]) {
    if body.len() < 16 {
        return;
    }
    let (kind, index, flags) = (u16_at(body, 2), u32_at(body, 4), u32_at(body, 8));
    if ty == RTM_DELLINK || kind != ARPHRD_ETHER {
        LINKS.with(|l| l.borrow_mut().remove(&index));
        return;
    }
    let (mut name, mut oper) = (None, IF_OPER_UNKNOWN);
    for (t, d) in attrs(&body[16..]) {
        match t {
            IFLA_IFNAME => name = std::ffi::CStr::from_bytes_until_nul(d).ok().map(|s| s.to_string_lossy().into_owned()),
            IFLA_OPERSTATE => oper = d.first().copied().unwrap_or(IF_OPER_UNKNOWN),
            _ => {}
        }
    }
    let up = oper == IF_OPER_UP || (oper == IF_OPER_UNKNOWN && flags & IFF_LOWER_UP != 0);
    LINKS.with(|l| {
        let mut l = l.borrow_mut();
        match (l.get_mut(&index), name) {
            (Some(link), n) if n.as_ref().is_none_or(|n| *n == link.name) => {
                link.up = up;
                // a hotplugged link is announced before its driver binds: classified again once up
                if up && link.kind.is_none() {
                    link.kind = classify(&link.name);
                }
            }
            (_, Some(n)) => {
                let kind = classify(&n);
                l.insert(index, Link { name: n, up, kind });
            }
            (None, None) => {}
        }
    });
}

fn on_route(ty: u16, body: &[u8]) {
    if body.len() < 12 {
        return;
    }
    let (family, dst_len, mut table, rtype) = (body[0], body[1], body[4] as u32, body[7]);
    let (mut oif, mut metric, mut gateway) = (0, 0, vec![]);
    for (t, d) in attrs(&body[12..]) {
        match t {
            RTA_TABLE if d.len() >= 4 => table = u32_at(d, 0),
            RTA_OIF if d.len() >= 4 => oif = u32_at(d, 0),
            RTA_PRIORITY if d.len() >= 4 => metric = u32_at(d, 0),
            RTA_GATEWAY => gateway = d.to_vec(),
            _ => {}
        }
    }
    if dst_len != 0 || table != RT_TABLE_MAIN || rtype != RTN_UNICAST || oif == 0 {
        return;
    }
    ROUTES.with(|r| {
        let route = (family, oif, metric, gateway);
        if ty == RTM_NEWROUTE {
            r.borrow_mut().insert(route);
        } else {
            r.borrow_mut().remove(&route);
        }
    });
}

fn publish() {
    let mut list: Vec<Wired> = LINKS.with(|l| {
        ROUTES.with(|r| {
            let r = r.borrow();
            l.borrow()
                .iter()
                .filter(|(_, link)| link.up)
                .filter_map(|(i, link)| {
                    Some(Wired { name: link.name.clone(), tether: link.kind?, online: r.iter().any(|(_, oif, _, _)| oif == i) })
                })
                .collect()
        })
    });
    list.sort_by(|a, b| a.name.cmp(&b.name));
    links().set(list);
}

fn read() {
    let fd = FD.with(Cell::get);
    // dump parts are at most 32 KiB
    let mut buf = [0u8; 32768];
    loop {
        let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            // the kernel dropped messages: the state is unknown until a fresh dump
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOBUFS) {
                resync();
                continue;
            }
            break;
        }
        let b = &buf[..n as usize];
        let mut off = 0;
        while off + 16 <= b.len() {
            let len = u32_at(b, off) as usize;
            if len < 16 || off + len > b.len() {
                break;
            }
            let (ty, seq, body) = (u16_at(b, off + 4), u32_at(b, off + 8), &b[off + 16..off + len]);
            match ty {
                RTM_NEWLINK | RTM_DELLINK => on_link(ty, body),
                RTM_NEWROUTE | RTM_DELROUTE => on_route(ty, body),
                // an rtmsg for any family
                NLMSG_DONE if seq == SEQ_LINKS => request(RTM_GETROUTE, SEQ_ROUTES, &[0; 12]),
                _ => {}
            }
            off += (len + 3) & !3;
        }
    }
    publish();
}

/// Called once at launch.
pub fn start() {
    let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE) };
    if fd < 0 {
        return;
    }
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as u16;
    addr.nl_groups = RTMGRP_LINK | RTMGRP_IPV4_ROUTE | RTMGRP_IPV6_ROUTE;
    if unsafe { libc::bind(fd, (&raw const addr).cast(), size_of::<libc::sockaddr_nl>() as u32) } < 0 {
        unsafe { libc::close(fd) };
        return;
    }
    FD.with(|f| f.set(fd));
    runtime::watch(fd, read);
    resync();
}
