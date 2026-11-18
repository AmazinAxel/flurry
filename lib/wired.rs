// Whether a wired link (an Ethernet port or adapter, or USB tethering: a phone showing up as a USB
// network card) carries a default route. Link and route changes arrive on an rtnetlink socket in
// the poll loop.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::os::fd::RawFd;
use std::path::Path;

use crate::ui::runtime;
use crate::ui::state::global_state;

pub const WIRED_ICON: &str = "network-wired-symbolic";

global_state! {
    /// A wired link is up and carries a default route (false while it is still getting an address).
    pub fn online() -> bool = false;
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
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PRIORITY: u16 = 6;
const RTA_TABLE: u16 = 15;
const RT_TABLE_MAIN: u32 = 254;
const RTN_UNICAST: u8 = 1;
const SEQ_LINKS: u32 = 1;
const SEQ_ROUTES: u32 = 2;

struct Link {
    name: String,
    up: bool,
    /// backed by a device and not wireless (not Wi-Fi, a bridge, veth or tunnel)
    wired: bool,
}

/// A default route: (family, output link, metric, gateway).
type Route = (u8, u32, u32, Vec<u8>);

thread_local! {
    static FD: Cell<RawFd> = const { Cell::new(-1) };
    /// Ethernet-type links by index
    static LINKS: RefCell<HashMap<u32, Link>> = RefCell::default();
    static ROUTES: RefCell<HashSet<Route>> = RefCell::default();
}

/// Whether `name` is a physical wired link, from sysfs (both are there once the link is announced).
fn is_wired(name: &str) -> bool {
    let base = Path::new("/sys/class/net").join(name);
    base.join("device").exists() && !base.join("wireless").exists() && !base.join("phy80211").exists()
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
    let mut name = None;
    let mut oper = IF_OPER_UNKNOWN;
    for (t, d) in attrs(&body[16..]) {
        match t {
            IFLA_IFNAME => name = std::ffi::CStr::from_bytes_until_nul(d).ok().map(|s| s.to_string_lossy().into_owned()),
            IFLA_OPERSTATE => oper = d.first().copied().unwrap_or(IF_OPER_UNKNOWN),
            _ => {}
        }
    }
    let up = ty == RTM_NEWLINK && kind == ARPHRD_ETHER && (oper == IF_OPER_UP || (oper == IF_OPER_UNKNOWN && flags & IFF_LOWER_UP != 0));
    if !up {
        // the kernel flushes IPv4 routes of a downed link without a DELROUTE
        ROUTES.with(|r| r.borrow_mut().retain(|(_, oif, _, _)| *oif != index));
    }
    LINKS.with(|l| {
        let mut l = l.borrow_mut();
        if ty == RTM_DELLINK || kind != ARPHRD_ETHER {
            l.remove(&index);
            return;
        }
        match (l.get_mut(&index), name) {
            (Some(link), None) => link.up = up,
            (Some(link), Some(n)) if n == link.name => link.up = up,
            // new or renamed
            (_, Some(n)) => {
                let wired = is_wired(&n);
                l.insert(index, Link { name: n, up, wired });
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
    let now = LINKS.with(|l| ROUTES.with(|r| r.borrow().iter().any(|(_, oif, _, _)| l.borrow().get(oif).is_some_and(|k| k.up && k.wired))));
    online().set(now);
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
