//! 本机 IPv6 地址枚举。
//!
//! Linux 走 netlink `RTM_GETADDR`：只有这条路径能同时拿到 `IFA_FLAGS`（完整 u32）与
//! `IFA_CACHEINFO`（preferred/valid 剩余秒数 + 创建时刻）。
//!
//! - `/proc/net/if_inet6` 只有 flags，且被内核截断成 u8，没有生命周期；
//! - `getifaddrs()` 的 `ifa_flags` 是**接口**标志（`IFF_*`），不是地址标志。
//!
//! 详见 `docs/ipv6-address-selection.md` §3.4 / §3.5。

use crate::select::{V6Cand, V6Kind, V6State};
use std::net::Ipv6Addr;

// IFA_F_*（/usr/include/linux/if_addr.h）
const IFA_F_TEMPORARY: u32 = 0x01;
const IFA_F_OPTIMISTIC: u32 = 0x04;
const IFA_F_DADFAILED: u32 = 0x08;
const IFA_F_DEPRECATED: u32 = 0x20;
const IFA_F_TENTATIVE: u32 = 0x40;
const IFA_F_PERMANENT: u32 = 0x80;

/// 从 netlink `IFA_ADDRESS` 属性负载解析 IPv6（网络字节序的 16 字节）。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn decode_addr(bytes: &[u8]) -> Option<Ipv6Addr> {
    if bytes.len() < 16 {
        return None;
    }
    let mut b = [0u8; 16];
    b.copy_from_slice(&bytes[..16]);
    Some(Ipv6Addr::from(b))
}

/// 解析 `struct ifa_cacheinfo { u32 ifa_prefered; u32 ifa_valid; u32 cstamp; }`。
/// 三个字段均为 native-endian；`0xFFFFFFFF` 表示永久，归一化成 `u64::MAX`。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn decode_cacheinfo(bytes: &[u8]) -> Option<(u64, u64, u32)> {
    if bytes.len() < 12 {
        return None;
    }
    let word = |i: usize| u32::from_ne_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
    let norm = |v: u32| {
        if v == u32::MAX {
            u64::MAX
        } else {
            u64::from(v)
        }
    };
    Some((norm(word(0)), norm(word(4)), word(8)))
}

/// 内核为出站流量会选的源地址（RFC 6724）。UDP connect 不发包，只做路由查找。
/// 仅作 tie-break / netlink 不可用时的兜底。
pub fn kernel_source_addr() -> Option<Ipv6Addr> {
    let sock = std::net::UdpSocket::bind("[::]:0").ok()?;
    sock.connect(("2001:4860:4860::8888", 53)).ok()?;
    match sock.local_addr().ok()? {
        std::net::SocketAddr::V6(a) => Some(*a.ip()),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
pub fn scan() -> Result<Vec<V6Cand>, String> {
    use neli::attr::Attribute;
    use neli::consts::nl::NlmF;
    use neli::consts::rtnl::{Ifa, RtAddrFamily, RtScope, Rtm};
    use neli::consts::socket::NlFamily;
    use neli::nl::{NlPayload, Nlmsghdr};
    use neli::router::synchronous::NlRouter;
    use neli::rtnl::{Ifaddrmsg, IfaddrmsgBuilder};
    use neli::utils::Groups;

    let msg = IfaddrmsgBuilder::default()
        .ifa_family(RtAddrFamily::Unspecified)
        .ifa_prefixlen(0)
        .ifa_scope(RtScope::Universe)
        .ifa_index(0)
        .build()
        .map_err(|e| format!("构造 RTM_GETADDR 请求失败: {e}"))?;

    let (sock, _) = NlRouter::connect(NlFamily::Route, None, Groups::empty())
        .map_err(|e| format!("连接 netlink 失败: {e}"))?;
    let recv = sock
        .send(
            Rtm::Getaddr,
            NlmF::REQUEST | NlmF::DUMP,
            NlPayload::Payload(msg),
        )
        .map_err(|e| format!("发送 RTM_GETADDR 失败: {e}"))?;

    let names = interface_names();
    let mut out = Vec::new();
    for rsp in recv {
        let hdr: Nlmsghdr<Rtm, Ifaddrmsg> =
            rsp.map_err(|e| format!("读取 netlink 响应失败: {e}"))?;
        if *hdr.nl_type() != Rtm::Newaddr {
            continue;
        }
        let Some(p) = hdr.get_payload() else { continue };
        if *p.ifa_family() != RtAddrFamily::Inet6 {
            continue;
        }
        let ifindex = *p.ifa_index();

        let mut addr: Option<Ipv6Addr> = None;
        // ifaddrmsg.ifa_flags 只有低 8 位；完整值在 IFA_FLAGS 属性里
        let mut flags = u32::from(p.ifa_flags().bits());
        let mut cache: Option<(u64, u64, u32)> = None;

        for a in p.rtattrs().iter() {
            match *a.rta_type() {
                Ifa::Address => {
                    // 直接按字节解析：netlink 里 in6_addr 就是 16 字节网络序，
                    // 不依赖 neli u128 的 NativeEndian 行为（大端机器也正确）。
                    addr = decode_addr(a.payload().as_ref());
                }
                Ifa::Flags => {
                    if let Ok(v) = a.get_payload_as::<u32>() {
                        flags = v;
                    }
                }
                Ifa::Cacheinfo => {
                    cache = decode_cacheinfo(a.payload().as_ref());
                }
                _ => {}
            }
        }

        let Some(addr) = addr else { continue };
        let preferred = cache.map(|c| c.0).unwrap_or(u64::MAX);
        let valid = cache.map(|c| c.1).unwrap_or(u64::MAX);
        out.push(V6Cand {
            ifindex,
            ifname: names
                .get(&ifindex)
                .cloned()
                .unwrap_or_else(|| format!("if{ifindex}")),
            addr,
            kind: if flags & IFA_F_TEMPORARY != 0 {
                V6Kind::Temporary
            } else {
                V6Kind::Stable
            },
            state: if flags & IFA_F_DEPRECATED != 0 || preferred == 0 {
                V6State::Deprecated
            } else {
                V6State::Preferred
            },
            usable: flags & (IFA_F_OPTIMISTIC | IFA_F_DADFAILED | IFA_F_TENTATIVE) == 0,
            permanent: flags & IFA_F_PERMANENT != 0,
            preferred_lft: preferred,
            valid_lft: valid,
            cstamp: cache.map(|c| c.2).unwrap_or(0),
        });
    }
    Ok(out)
}

/// macOS：`ifconfig -a -L` 能给出地址标志与 pltime/vltime，语义与 netlink 基本等价。
/// 解析失败时回退到 `local-ip-address`，并打印 warning 告知用户寿命信息缺失。
#[cfg(target_os = "macos")]
pub fn scan() -> Result<Vec<V6Cand>, String> {
    match super::ipv6_macos::scan() {
        Ok(cands) => Ok(cands),
        Err(e) => {
            crate::log::warn(format!(
                "ifconfig -L 解析失败（{e}），回退到 local-ip-address：无法区分临时/弃用地址，\
                也无法按生命周期提前复查"
            ));
            scan_via_local_ip_address()
        }
    }
}

/// 其他非 Linux / 非 macOS 平台的兜底：能拿到地址，但没有标志与生命周期。
#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
pub fn scan() -> Result<Vec<V6Cand>, String> {
    scan_via_local_ip_address()
}

#[cfg(not(target_os = "linux"))]
pub fn scan_via_local_ip_address() -> Result<Vec<V6Cand>, String> {
    let netifs = local_ip_address::list_afinet_netifas().map_err(|e| e.to_string())?;
    Ok(netifs
        .into_iter()
        .filter_map(|(name, ip)| match ip {
            std::net::IpAddr::V6(a) => Some(V6Cand {
                ifindex: 0,
                ifname: name,
                addr: a,
                kind: V6Kind::Stable,
                state: V6State::Preferred,
                usable: true,
                permanent: false,
                preferred_lft: u64::MAX,
                valid_lft: u64::MAX,
                cstamp: 0,
            }),
            _ => None,
        })
        .collect())
}

/// ifindex → 网卡名。读 `/sys/class/net/<name>/ifindex`，无需额外依赖与 unsafe。
#[cfg(target_os = "linux")]
fn interface_names() -> std::collections::HashMap<u32, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return map;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(raw) = std::fs::read_to_string(entry.path().join("ifindex")) else {
            continue;
        };
        if let Ok(index) = raw.trim().parse::<u32>() {
            map.insert(index, name);
        }
    }
    map
}

/// 订阅 `RTMGRP_IPV6_IFADDR`，地址增删时立刻唤醒主循环。
///
/// 失败（无 netlink 权限、容器限制）时返回 `None`，主循环退回纯定时轮询。
#[cfg(target_os = "linux")]
pub fn spawn_change_watcher() -> Option<std::sync::mpsc::Receiver<()>> {
    use neli::consts::rtnl::Rtm;
    use neli::consts::socket::NlFamily;
    use neli::rtnl::Ifaddrmsg;
    use neli::socket::synchronous::NlSocketHandle;
    use neli::utils::Groups;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    /// RTMGRP_IPV6_IFADDR（`/usr/include/linux/rtnetlink.h`）
    const RTMGRP_IPV6_IFADDR: u32 = 0x100;

    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new()
        .name("rtnl-event".into())
        .spawn(move || {
            let Ok(sock) =
                NlSocketHandle::connect(NlFamily::Route, None, Groups::new_bitmask(RTMGRP_IPV6_IFADDR))
            else {
                return;
            };
            loop {
                match sock.recv_all::<Rtm, Ifaddrmsg>() {
                    Ok((buf, _)) if !buf.is_empty() => {
                        if tx.send(()).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => std::thread::sleep(Duration::from_secs(5)),
                }
            }
        })
        .is_ok();
    if spawned {
        Some(rx)
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
pub fn spawn_change_watcher() -> Option<std::sync::mpsc::Receiver<()>> {
    None
}

/// 把内核标志位翻译成 `V6Cand`（供单测复用，不依赖真实网卡）。
#[cfg(test)]
pub fn cand_from_flags(addr: Ipv6Addr, flags: u32, cache: Option<(u64, u64, u32)>) -> V6Cand {
    V6Cand {
        ifindex: 2,
        ifname: "test0".into(),
        addr,
        kind: if flags & IFA_F_TEMPORARY != 0 {
            V6Kind::Temporary
        } else {
            V6Kind::Stable
        },
        state: if flags & IFA_F_DEPRECATED != 0 {
            V6State::Deprecated
        } else {
            V6State::Preferred
        },
        usable: flags & (IFA_F_OPTIMISTIC | IFA_F_DADFAILED | IFA_F_TENTATIVE) == 0,
        permanent: flags & IFA_F_PERMANENT != 0,
        preferred_lft: cache.map(|c| c.0).unwrap_or(u64::MAX),
        valid_lft: cache.map(|c| c.1).unwrap_or(u64::MAX),
        cstamp: cache.map(|c| c.2).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_decodes_network_order() {
        let bytes = [
            0x20, 0x01, 0x0d, 0xf1, 0x78, 0x80, 0x01, 0x00, 0, 0, 0, 0, 0, 0, 0x04, 0xc3,
        ];
        assert_eq!(
            decode_addr(&bytes),
            Some("2001:df1:7880:100::4c3".parse().unwrap())
        );
        assert_eq!(decode_addr(&bytes[..15]), None);
    }

    #[test]
    fn cacheinfo_decodes() {
        let mut b = [0u8; 12];
        b[0..4].copy_from_slice(&3600u32.to_ne_bytes());
        b[4..8].copy_from_slice(&7200u32.to_ne_bytes());
        b[8..12].copy_from_slice(&123456u32.to_ne_bytes());
        assert_eq!(decode_cacheinfo(&b), Some((3600, 7200, 123456)));
    }

    #[test]
    fn cacheinfo_infinity() {
        let mut b = [0xffu8; 12];
        b[8..12].copy_from_slice(&7u32.to_ne_bytes());
        assert_eq!(decode_cacheinfo(&b), Some((u64::MAX, u64::MAX, 7)));
    }

    #[test]
    fn cacheinfo_short_payload() {
        assert_eq!(decode_cacheinfo(&[0u8; 8]), None);
    }

    #[test]
    fn flags_translate() {
        let a = "2408::1".parse().unwrap();
        let c = cand_from_flags(a, IFA_F_TEMPORARY | IFA_F_DEPRECATED, Some((0, 100, 5)));
        assert_eq!(c.kind, V6Kind::Temporary);
        assert_eq!(c.state, V6State::Deprecated);
        assert!(c.usable);

        let c = cand_from_flags(a, IFA_F_TENTATIVE, None);
        assert!(!c.usable);
    }
}
