//! IPv6 地址选取（纯函数）。
//!
//! 输入是内核给出的地址元数据（netlink `RTM_GETADDR`：`IFA_FLAGS` + `IFA_CACHEINFO`），
//! 输出是"该写哪一条 AAAA"。
//!
//! 判据来自 RFC 4862 §5.5.3（preferred/valid lifetime）与 RFC 4941（临时地址）：
//! - `preferred_lft == 0` / `IFA_F_DEPRECATED` ⇒ 已弃用，不应再作为新值发布
//! - `valid_lft == 0` / `IFA_F_TENTATIVE` / `IFA_F_DADFAILED` ⇒ 不可用
//! - 轮换期新旧地址并存，`cstamp`（创建时刻）越大越新
//!
//! 详见 `docs/ipv6-address-selection.md`。

use crate::config::V6Prefer;
use std::cmp::Ordering;
use std::net::Ipv6Addr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum V6Kind {
    Stable,
    Temporary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum V6State {
    Preferred,
    Deprecated,
}

#[derive(Clone, Debug)]
pub struct V6Cand {
    pub ifindex: u32,
    pub ifname: String,
    pub addr: Ipv6Addr,
    pub kind: V6Kind,
    pub state: V6State,
    pub usable: bool,
    pub permanent: bool,
    /// 剩余秒数；`u64::MAX` 表示永久。
    pub preferred_lft: u64,
    pub valid_lft: u64,
    /// 创建时刻（开机后 1/100 秒）。u32 会回绕，比较时用 `wrapping_sub`。
    pub cstamp: u32,
}

pub struct Cfg {
    pub prefer: V6Prefer,
    pub allow_ula: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// 正常写入。
    Use {
        addr: Ipv6Addr,
        preferred_lft: u64,
        /// 非空时调用方应告警（但依然写入）。
        warn: Option<&'static str>,
    },
    /// 有候选地址，但策略要求不写（例如只有 ULA）。
    Refuse { reason: &'static str },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Global,
    Ula,
    Skip,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Bucket {
    /// Global + Stable + Preferred
    GlobalStable,
    /// Global + Temporary + Preferred
    GlobalTemporary,
    /// ULA + Stable + Preferred
    UlaStable,
    /// ULA + Temporary + Preferred
    UlaTemporary,
    /// 任意 + Deprecated（仍 valid）
    Deprecated,
}

fn scope_of(addr: &Ipv6Addr) -> Scope {
    let o = addr.octets();
    if o[0] == 0xff {
        return Scope::Skip; // multicast
    }
    if addr.is_loopback() || addr.is_unspecified() {
        return Scope::Skip;
    }
    // IPv4-mapped ::ffff:0:0/96
    if o[..10].iter().all(|&b| b == 0) && o[10] == 0xff && o[11] == 0xff {
        return Scope::Skip;
    }
    if o[0] == 0xfe && (o[1] & 0xc0) == 0x80 {
        return Scope::Skip; // fe80::/10
    }
    if (o[0] & 0xfe) == 0xfc {
        return Scope::Ula; // fc00::/7
    }
    // 2001:db8::/32 文档地址
    if o[0] == 0x20 && o[1] == 0x01 && o[2] == 0x0d && o[3] == 0xb8 {
        return Scope::Skip;
    }
    // 64:ff9b::/96 NAT64
    if o[0] == 0x00 && o[1] == 0x64 && o[2] == 0xff && o[3] == 0x9b {
        return Scope::Skip;
    }
    // 2002::/16 6to4（RFC 7526 已弃用）
    if o[0] == 0x20 && o[1] == 0x02 {
        return Scope::Skip;
    }
    Scope::Global
}

fn bucket_of(c: &V6Cand) -> Option<Bucket> {
    if !c.usable || c.valid_lft == 0 {
        return None;
    }
    let scope = scope_of(&c.addr);
    if scope == Scope::Skip {
        return None;
    }
    if c.state == V6State::Deprecated {
        return Some(Bucket::Deprecated);
    }
    Some(match (scope, c.kind) {
        (Scope::Global, V6Kind::Stable) => Bucket::GlobalStable,
        (Scope::Global, V6Kind::Temporary) => Bucket::GlobalTemporary,
        (Scope::Ula, V6Kind::Stable) => Bucket::UlaStable,
        (Scope::Ula, V6Kind::Temporary) => Bucket::UlaTemporary,
        (Scope::Skip, _) => return None,
    })
}

fn bucket_order(prefer: V6Prefer) -> [Bucket; 5] {
    match prefer {
        V6Prefer::Auto => [
            Bucket::GlobalStable,
            Bucket::GlobalTemporary,
            Bucket::UlaStable,
            Bucket::UlaTemporary,
            Bucket::Deprecated,
        ],
        V6Prefer::Stable => [
            Bucket::GlobalStable,
            Bucket::UlaStable,
            Bucket::GlobalTemporary,
            Bucket::UlaTemporary,
            Bucket::Deprecated,
        ],
        V6Prefer::Temporary => [
            Bucket::GlobalTemporary,
            Bucket::UlaTemporary,
            Bucket::GlobalStable,
            Bucket::UlaStable,
            Bucket::Deprecated,
        ],
    }
}

/// 同一桶内的排序：`Greater` 表示 a 更优。
fn better(a: &V6Cand, b: &V6Cand, kernel_pick: Option<Ipv6Addr>) -> Ordering {
    // 1) 创建时间更新者优先（u32 回绕安全）
    let dc = a.cstamp.wrapping_sub(b.cstamp) as i32;
    if dc != 0 {
        return dc.cmp(&0);
    }
    // 2) 剩余 valid_lft 更大者优先（形态 B：旧前缀寿命在倒数）
    match a.valid_lft.cmp(&b.valid_lft) {
        Ordering::Equal => {}
        other => return other,
    }
    // 3) 永久地址优先（手工/静态配置，不会随 RA 消失）
    match a.permanent.cmp(&b.permanent) {
        Ordering::Equal => {}
        other => return other,
    }
    // 4) 内核会选作源地址的那个
    match kernel_pick {
        Some(k) if a.addr == k && b.addr != k => return Ordering::Greater,
        Some(k) if b.addr == k && a.addr != k => return Ordering::Less,
        _ => {}
    }
    // 5) 确定性：ifindex 小者、地址小者优先
    b.ifindex
        .cmp(&a.ifindex)
        .then_with(|| b.addr.cmp(&a.addr))
}

pub fn select(cands: &[V6Cand], cfg: &Cfg, kernel_pick: Option<Ipv6Addr>) -> Option<Pick> {
    let any = cands.iter().any(|c| bucket_of(c).is_some());
    if !any {
        return None;
    }

    for bucket in bucket_order(cfg.prefer) {
        let best = cands
            .iter()
            .filter(|c| bucket_of(c) == Some(bucket))
            .max_by(|a, b| better(a, b, kernel_pick));
        let Some(c) = best else { continue };
        let warn = match bucket {
            Bucket::Deprecated => Some("所有可用地址都已 deprecated，沿用旧地址；新地址尚未就绪"),
            Bucket::GlobalTemporary => {
                if cfg.prefer == V6Prefer::Temporary {
                    None
                } else {
                    Some("仅存在临时地址（RFC 4941），会周期性轮换；建议关闭隐私扩展或改用稳定地址")
                }
            }
            Bucket::UlaStable | Bucket::UlaTemporary => {
                if !cfg.allow_ula {
                    return Some(Pick::Refuse {
                        reason: "仅存在 ULA 地址（fc00::/7），公网不可达；如确需写入请加 --allow-ula",
                    });
                }
                Some("使用 ULA 地址，公网不可达")
            }
            Bucket::GlobalStable => None,
        };
        return Some(Pick::Use {
            addr: c.addr,
            preferred_lft: c.preferred_lft,
            warn,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(addr: &str, kind: V6Kind, state: V6State, cstamp: u32, valid: u64) -> V6Cand {
        V6Cand {
            ifindex: 2,
            ifname: "eth0".into(),
            addr: addr.parse().unwrap(),
            kind,
            state,
            usable: true,
            permanent: false,
            preferred_lft: if state == V6State::Deprecated { 0 } else { 3600 },
            valid_lft: valid,
            cstamp,
        }
    }

    fn cfg() -> Cfg {
        Cfg {
            prefer: V6Prefer::Auto,
            allow_ula: false,
        }
    }

    #[test]
    fn stable_wins_over_temporary() {
        let set = [
            cand("2408::aaaa", V6Kind::Temporary, V6State::Preferred, 200, 1000),
            cand("2408::bbbb", V6Kind::Stable, V6State::Preferred, 100, 1000),
        ];
        match select(&set, &cfg(), None).unwrap() {
            Pick::Use { addr, .. } => assert_eq!(addr, "2408::bbbb".parse::<Ipv6Addr>().unwrap()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rotation_picks_new_temporary() {
        let set = [
            cand("2408::1", V6Kind::Temporary, V6State::Deprecated, 100, 1000),
            cand("2408::2", V6Kind::Temporary, V6State::Preferred, 200, 2000),
        ];
        match select(&set, &cfg(), None).unwrap() {
            Pick::Use { addr, warn, .. } => {
                assert_eq!(addr, "2408::2".parse::<Ipv6Addr>().unwrap());
                assert!(warn.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn prefix_change_by_cstamp() {
        let set = [
            cand("2408:1::1", V6Kind::Stable, V6State::Preferred, 100, 500_000),
            cand("2408:2::1", V6Kind::Stable, V6State::Preferred, 900, 600_000),
        ];
        match select(&set, &cfg(), None).unwrap() {
            Pick::Use { addr, warn, .. } => {
                assert_eq!(addr, "2408:2::1".parse::<Ipv6Addr>().unwrap());
                assert!(warn.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cstamp_wraparound() {
        // 旧地址 cstamp 接近 u32 上限，新地址刚回绕到 0
        let set = [
            cand("2408:ffff::1", V6Kind::Stable, V6State::Preferred, u32::MAX - 5, 1000),
            cand("2408:1::1", V6Kind::Stable, V6State::Preferred, 5, 1000),
        ];
        match select(&set, &cfg(), None).unwrap() {
            Pick::Use { addr, .. } => assert_eq!(addr, "2408:1::1".parse::<Ipv6Addr>().unwrap()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn all_deprecated_still_returns_one() {
        let set = [cand("2408::a", V6Kind::Stable, V6State::Deprecated, 1, 600)];
        match select(&set, &cfg(), None).unwrap() {
            Pick::Use { warn, .. } => assert!(warn.is_some()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ula_refused_by_default() {
        let set = [cand("fd00::1", V6Kind::Stable, V6State::Preferred, 1, 1000)];
        assert!(matches!(select(&set, &cfg(), None).unwrap(), Pick::Refuse { .. }));
    }

    #[test]
    fn ula_allowed_when_asked() {
        let set = [cand("fd00::1", V6Kind::Stable, V6State::Preferred, 1, 1000)];
        let c = Cfg { prefer: V6Prefer::Auto, allow_ula: true };
        match select(&set, &c, None).unwrap() {
            Pick::Use { warn, .. } => assert!(warn.is_some()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tentative_and_dadfailed_are_skipped() {
        let mut c = cand("2408::a", V6Kind::Stable, V6State::Preferred, 1, 1000);
        c.usable = false;
        assert!(select(&[c], &cfg(), None).is_none());
    }

    #[test]
    fn link_local_and_multicast_skipped() {
        for a in ["fe80::1", "ff02::1", "::1", "::"] {
            let mut c = cand("2408::a", V6Kind::Stable, V6State::Preferred, 1, 1000);
            c.addr = a.parse().unwrap();
            assert!(select(&[c], &cfg(), None).is_none(), "{a} 应被排除");
        }
    }

    #[test]
    fn empty_returns_none() {
        assert!(select(&[], &cfg(), None).is_none());
    }

    #[test]
    fn temporary_prefer_overrides() {
        let set = [
            cand("2408::b", V6Kind::Temporary, V6State::Preferred, 200, 1000),
            cand("2408::a", V6Kind::Stable, V6State::Preferred, 100, 1000),
        ];
        let c = Cfg { prefer: V6Prefer::Temporary, allow_ula: false };
        match select(&set, &c, None).unwrap() {
            Pick::Use { addr, .. } => assert_eq!(addr, "2408::b".parse::<Ipv6Addr>().unwrap()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn kernel_pick_breaks_tie() {
        let set = [
            cand("2408::a", V6Kind::Stable, V6State::Preferred, 5, 1000),
            cand("2408::b", V6Kind::Stable, V6State::Preferred, 5, 1000),
        ];
        let k = Some("2408::b".parse::<Ipv6Addr>().unwrap());
        match select(&set, &cfg(), k).unwrap() {
            Pick::Use { addr, .. } => assert_eq!(addr, "2408::b".parse::<Ipv6Addr>().unwrap()),
            other => panic!("{other:?}"),
        }
    }
}
