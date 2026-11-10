//! The bridge, read and written over raw rtnetlink (the node agent, #32).
//!
//! Four operations, the same messages iproute2 sends:
//!
//! | here                     | iproute2                                        |
//! |--------------------------|-------------------------------------------------|
//! | [`Netlink::port_vlans`]  | `bridge vlan show` (RTM_GETLINK dump, AF_BRIDGE, `RTEXT_FILTER_BRVLAN`) |
//! | [`Netlink::add_vid`]     | `bridge vlan add dev <port> vid <id>` (RTM_SETLINK, AF_BRIDGE) |
//! | [`Netlink::del_vid`]     | `bridge vlan del dev <port> vid <id>` (RTM_DELLINK, AF_BRIDGE) |
//! | [`Netlink::set_vlan_filtering`] | `ip link set <br> type bridge vlan_filtering 1` |
//!
//! A VLAN is added to a port tagged, never as its PVID, so the port's
//! untagged traffic (the node's own, PVID 1) is untouched. Reads that sysfs
//! answers (ifindex, `vlan_filtering`, whether a port is a NIC or a bond) are
//! read there. Building and parsing messages is pure and unit-tested; only
//! [`Netlink`] touches a socket.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

const NETLINK_ROUTE: i32 = 0;

const RTM_NEWLINK: u16 = 16;
const RTM_DELLINK: u16 = 17;
const RTM_GETLINK: u16 = 18;
const RTM_SETLINK: u16 = 19;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;

const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_DUMP: u16 = 0x300;

const AF_UNSPEC: u8 = 0;
const AF_BRIDGE: u8 = 7;

const IFLA_IFNAME: u16 = 3;
const IFLA_MASTER: u16 = 10;
const IFLA_LINKINFO: u16 = 18;
const IFLA_AF_SPEC: u16 = 26;
const IFLA_EXT_MASK: u16 = 29;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const IFLA_BR_VLAN_FILTERING: u16 = 7;
const IFLA_BRIDGE_VLAN_INFO: u16 = 2;
const RTEXT_FILTER_BRVLAN: u32 = 1 << 1;

/// The flag bits an attribute type may carry (NLA_F_NESTED, NLA_F_NET_BYTEORDER).
const NLA_TYPE_MASK: u16 = 0x3fff;

const HDR: usize = 16;
const IFINFO: usize = 16;

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

fn pad(buf: &mut Vec<u8>) {
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

fn push_attr(buf: &mut Vec<u8>, ty: u16, payload: &[u8]) {
    buf.extend_from_slice(&((4 + payload.len()) as u16).to_ne_bytes());
    buf.extend_from_slice(&ty.to_ne_bytes());
    buf.extend_from_slice(payload);
    pad(buf);
}

/// A nested attribute, as iproute2's `addattr_nest` writes it.
fn push_nested(buf: &mut Vec<u8>, ty: u16, inner: impl FnOnce(&mut Vec<u8>)) {
    let start = buf.len();
    buf.extend_from_slice(&[0; 4]);
    inner(buf);
    let len = (buf.len() - start) as u16;
    buf[start..start + 2].copy_from_slice(&len.to_ne_bytes());
    buf[start + 2..start + 4].copy_from_slice(&ty.to_ne_bytes());
}

/// An rtnetlink link message: header, `ifinfomsg`, attributes.
fn link_message(ty: u16, flags: u16, seq: u32, family: u8, index: i32, attrs: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&0u32.to_ne_bytes()); // length, set below
    buf.extend_from_slice(&ty.to_ne_bytes());
    buf.extend_from_slice(&flags.to_ne_bytes());
    buf.extend_from_slice(&seq.to_ne_bytes());
    buf.extend_from_slice(&0u32.to_ne_bytes()); // port id: the kernel fills it
    // ifinfomsg: family, pad, type, index, flags, change
    buf.push(family);
    buf.push(0);
    buf.extend_from_slice(&0u16.to_ne_bytes());
    buf.extend_from_slice(&index.to_ne_bytes());
    buf.extend_from_slice(&0u32.to_ne_bytes());
    buf.extend_from_slice(&0u32.to_ne_bytes());
    attrs(&mut buf);
    let len = buf.len() as u32;
    buf[0..4].copy_from_slice(&len.to_ne_bytes());
    buf
}

/// `bridge vlan add|del dev <index> vid <vid>`: tagged, not PVID, on the
/// port (the default `master`).
pub fn vid_message(add: bool, seq: u32, port_index: i32, vid: u16) -> Vec<u8> {
    let ty = if add { RTM_SETLINK } else { RTM_DELLINK };
    link_message(ty, NLM_F_REQUEST | NLM_F_ACK, seq, AF_BRIDGE, port_index, |b| {
        push_nested(b, IFLA_AF_SPEC, |b| {
            // struct bridge_vlan_info { u16 flags; u16 vid; }; flags 0 = tagged.
            let mut info = Vec::with_capacity(4);
            info.extend_from_slice(&0u16.to_ne_bytes());
            info.extend_from_slice(&vid.to_ne_bytes());
            push_attr(b, IFLA_BRIDGE_VLAN_INFO, &info);
        })
    })
}

/// `ip link set <index> type bridge vlan_filtering 1`.
pub fn vlan_filtering_message(seq: u32, bridge_index: i32) -> Vec<u8> {
    link_message(RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, seq, AF_UNSPEC, bridge_index, |b| {
        push_nested(b, IFLA_LINKINFO, |b| {
            push_attr(b, IFLA_INFO_KIND, b"bridge\0");
            push_nested(b, IFLA_INFO_DATA, |b| push_attr(b, IFLA_BR_VLAN_FILTERING, &[1]));
        })
    })
}

/// The dump of every bridge port's VLANs.
pub fn dump_message(seq: u32) -> Vec<u8> {
    link_message(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, seq, AF_BRIDGE, 0, |b| {
        push_attr(b, IFLA_EXT_MASK, &RTEXT_FILTER_BRVLAN.to_ne_bytes())
    })
}

/// One netlink message in a receive buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply<'a> {
    /// `NLMSG_ERROR`: 0 is an ACK, else a negative errno.
    Error { seq: u32, errno: i32 },
    Done { seq: u32 },
    Link { seq: u32, index: i32, attrs: &'a [u8] },
    Other,
}

/// Split a receive buffer into messages.
pub fn replies(buf: &[u8]) -> Vec<Reply<'_>> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + HDR <= buf.len() {
        let len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if len < HDR || off + len > buf.len() {
            break;
        }
        let ty = u16::from_ne_bytes(buf[off + 4..off + 6].try_into().unwrap());
        let seq = u32::from_ne_bytes(buf[off + 8..off + 12].try_into().unwrap());
        let body = &buf[off + HDR..off + len];
        out.push(match ty {
            NLMSG_ERROR if body.len() >= 4 => Reply::Error { seq, errno: i32::from_ne_bytes(body[0..4].try_into().unwrap()) },
            NLMSG_DONE => Reply::Done { seq },
            RTM_NEWLINK if body.len() >= IFINFO => Reply::Link {
                seq,
                index: i32::from_ne_bytes(body[4..8].try_into().unwrap()),
                attrs: &body[IFINFO..],
            },
            _ => Reply::Other,
        });
        off += align4(len);
    }
    out
}

/// `(type, payload)` of each attribute in `buf`.
pub fn attrs(buf: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + 4 <= buf.len() {
        let len = u16::from_ne_bytes(buf[off..off + 2].try_into().unwrap()) as usize;
        let ty = u16::from_ne_bytes(buf[off + 2..off + 4].try_into().unwrap()) & NLA_TYPE_MASK;
        if len < 4 || off + len > buf.len() {
            break;
        }
        out.push((ty, &buf[off + 4..off + len]));
        off += align4(len);
    }
    out
}

/// One bridge port from the dump.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortVlans {
    pub name: String,
    pub master: Option<i32>,
    pub vids: BTreeSet<u16>,
}

/// A dumped link's name, master and VLANs.
pub fn port_vlans(attrs_buf: &[u8]) -> PortVlans {
    let mut p = PortVlans::default();
    for (ty, payload) in attrs(attrs_buf) {
        match ty {
            IFLA_IFNAME => p.name = String::from_utf8_lossy(payload).trim_end_matches('\0').to_string(),
            IFLA_MASTER if payload.len() >= 4 => p.master = Some(i32::from_ne_bytes(payload[0..4].try_into().unwrap())),
            IFLA_AF_SPEC => {
                for (ty, info) in attrs(payload) {
                    if ty == IFLA_BRIDGE_VLAN_INFO && info.len() >= 4 {
                        p.vids.insert(u16::from_ne_bytes(info[2..4].try_into().unwrap()));
                    }
                }
            }
            _ => {}
        }
    }
    p
}

/// A route netlink socket.
pub struct Netlink {
    fd: OwnedFd,
    seq: u32,
}

impl Netlink {
    pub fn open() -> io::Result<Netlink> {
        // SAFETY: plain socket(2); the result is checked before it is owned.
        let raw = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, NETLINK_ROUTE) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        // SAFETY: an all-zero sockaddr_nl is valid; family set below.
        let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // SAFETY: addr outlives the call and the length is its size.
        let r = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }

        // A lost reply must not hang the agent: time a receive out.
        // SAFETY: zeroed timeval is valid; setsockopt reads it for its size.
        let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
        tv.tv_sec = 5;
        let r = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const libc::timeval as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Netlink { fd, seq: 1 })
    }

    fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1).max(1);
        self.seq
    }

    fn send(&self, msg: &[u8]) -> io::Result<()> {
        // SAFETY: msg is a live slice of the given length.
        let n = unsafe { libc::send(self.fd.as_raw_fd(), msg.as_ptr() as *const libc::c_void, msg.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: buf is a live, writable slice of the given length.
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// Send a request and wait for its ACK.
    fn request(&mut self, build: impl FnOnce(u32) -> Vec<u8>) -> io::Result<()> {
        let seq = self.next_seq();
        self.send(&build(seq))?;
        let mut buf = vec![0u8; 65536];
        loop {
            let n = self.recv(&mut buf)?;
            for r in replies(&buf[..n]) {
                if let Reply::Error { seq: s, errno } = r {
                    if s == seq {
                        return if errno == 0 { Ok(()) } else { Err(io::Error::from_raw_os_error(-errno)) };
                    }
                }
            }
        }
    }

    /// Every bridge port (on any bridge) and the VLANs it is a member of, by
    /// ifindex.
    pub fn port_vlans(&mut self) -> io::Result<BTreeMap<i32, PortVlans>> {
        let seq = self.next_seq();
        self.send(&dump_message(seq))?;
        let mut out = BTreeMap::new();
        let mut buf = vec![0u8; 65536];
        loop {
            let n = self.recv(&mut buf)?;
            for r in replies(&buf[..n]) {
                match r {
                    Reply::Link { seq: s, index, attrs } if s == seq => {
                        out.insert(index, port_vlans(attrs));
                    }
                    Reply::Done { seq: s } if s == seq => return Ok(out),
                    Reply::Error { seq: s, errno } if s == seq && errno != 0 => {
                        return Err(io::Error::from_raw_os_error(-errno));
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn add_vid(&mut self, port_index: i32, vid: u16) -> io::Result<()> {
        self.request(|seq| vid_message(true, seq, port_index, vid))
    }

    pub fn del_vid(&mut self, port_index: i32, vid: u16) -> io::Result<()> {
        self.request(|seq| vid_message(false, seq, port_index, vid))
    }

    pub fn set_vlan_filtering(&mut self, bridge_index: i32) -> io::Result<()> {
        self.request(|seq| vlan_filtering_message(seq, bridge_index))
    }
}

/// A link's ifindex, from sysfs.
pub fn ifindex(sys: &Path, name: &str) -> io::Result<i32> {
    let text = std::fs::read_to_string(sys.join(name).join("ifindex"))?;
    text.trim()
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("{name}: ifindex {text:?}")))
}

/// Whether `bridge` has `vlan_filtering` on, from sysfs.
pub fn vlan_filtering(sys: &Path, bridge: &str) -> io::Result<bool> {
    Ok(std::fs::read_to_string(sys.join(bridge).join("bridge/vlan_filtering"))?.trim() == "1")
}

/// Whether a bridge port carries traffic off the node: a NIC (it has a
/// `device`) or a bond. Veths, taps and the like are not.
pub fn is_uplink(sys: &Path, port: &str) -> bool {
    let dir = sys.join(port);
    dir.join("device").exists() || dir.join("bonding").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vid_add_is_the_message_bridge_vlan_add_sends() {
        let m = vid_message(true, 7, 3, 10);
        // header(16) + ifinfomsg(16) + AF_SPEC(4) + VLAN_INFO(4 + 4)
        assert_eq!(m.len(), 44);
        assert_eq!(u32::from_ne_bytes(m[0..4].try_into().unwrap()), 44);
        assert_eq!(u16::from_ne_bytes(m[4..6].try_into().unwrap()), RTM_SETLINK);
        assert_eq!(u16::from_ne_bytes(m[6..8].try_into().unwrap()), NLM_F_REQUEST | NLM_F_ACK);
        assert_eq!(u32::from_ne_bytes(m[8..12].try_into().unwrap()), 7);
        assert_eq!(m[16], AF_BRIDGE);
        assert_eq!(i32::from_ne_bytes(m[20..24].try_into().unwrap()), 3);
        let top = attrs(&m[32..]);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].0, IFLA_AF_SPEC);
        let inner = attrs(top[0].1);
        assert_eq!(inner, vec![(IFLA_BRIDGE_VLAN_INFO, &[0u8, 0, 10, 0][..])], "flags 0 (tagged, not PVID), vid 10");

        let d = vid_message(false, 8, 3, 10);
        assert_eq!(u16::from_ne_bytes(d[4..6].try_into().unwrap()), RTM_DELLINK);
    }

    #[test]
    fn vlan_filtering_is_set_through_linkinfo() {
        let m = vlan_filtering_message(1, 5);
        assert_eq!(u16::from_ne_bytes(m[4..6].try_into().unwrap()), RTM_NEWLINK);
        assert_eq!(m[16], AF_UNSPEC);
        let top = attrs(&m[32..]);
        assert_eq!(top[0].0, IFLA_LINKINFO);
        let info = attrs(top[0].1);
        assert_eq!(info[0], (IFLA_INFO_KIND, &b"bridge\0"[..]));
        assert_eq!(info[1].0, IFLA_INFO_DATA);
        assert_eq!(attrs(info[1].1), vec![(IFLA_BR_VLAN_FILTERING, &[1u8][..])]);
    }

    #[test]
    fn the_dump_asks_for_per_vlan_entries() {
        let m = dump_message(9);
        assert_eq!(u16::from_ne_bytes(m[6..8].try_into().unwrap()), NLM_F_REQUEST | NLM_F_DUMP);
        assert_eq!(m[16], AF_BRIDGE);
        assert_eq!(attrs(&m[32..]), vec![(IFLA_EXT_MASK, &RTEXT_FILTER_BRVLAN.to_ne_bytes()[..])]);
    }

    /// A reply as the kernel would send it, built with the same helpers.
    fn port_reply(seq: u32, index: i32, name: &str, master: i32, vids: &[u16]) -> Vec<u8> {
        let mut m = link_message(RTM_NEWLINK, 0, seq, AF_BRIDGE, index, |b| {
            let mut n = name.as_bytes().to_vec();
            n.push(0);
            push_attr(b, IFLA_IFNAME, &n);
            push_attr(b, IFLA_MASTER, &master.to_ne_bytes());
            push_nested(b, IFLA_AF_SPEC, |b| {
                for v in vids {
                    let flags: u16 = if *v == 1 { 0x6 } else { 0 }; // PVID|UNTAGGED
                    let mut info = flags.to_ne_bytes().to_vec();
                    info.extend_from_slice(&v.to_ne_bytes());
                    push_attr(b, IFLA_BRIDGE_VLAN_INFO, &info);
                }
            });
        });
        m.extend(link_message(NLMSG_DONE, 0, seq, 0, 0, |_| {}));
        m
    }

    #[test]
    fn a_dump_reply_parses_to_ports_and_vids() {
        let buf = port_reply(4, 2, "eth0", 5, &[1, 10, 20]);
        let r = replies(&buf);
        assert_eq!(r.len(), 2);
        let Reply::Link { seq, index, attrs } = r[0] else { panic!("{:?}", r[0]) };
        assert_eq!((seq, index), (4, 2));
        let p = port_vlans(attrs);
        assert_eq!(p.name, "eth0");
        assert_eq!(p.master, Some(5));
        assert_eq!(p.vids, [1, 10, 20].into_iter().collect());
        assert_eq!(r[1], Reply::Done { seq: 4 });
    }

    #[test]
    fn errors_and_acks_parse() {
        let mut ack = link_message(NLMSG_ERROR, 0, 3, 0, 0, |_| {});
        // The error code is the first four bytes of the body.
        ack[16..20].copy_from_slice(&(-17i32).to_ne_bytes());
        assert_eq!(replies(&ack)[0], Reply::Error { seq: 3, errno: -17 });
        assert!(replies(&[1, 2, 3]).is_empty(), "a short buffer is not a message");
    }
}
