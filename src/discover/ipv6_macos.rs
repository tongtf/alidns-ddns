//! macOS IPv6 地址发现。
//!
//! macOS 的 `getifaddrs()` 和 Linux 一样只有接口标志（IFF_*），拿不到地址标志与生命周期。
//! 但 `/sbin/ifconfig -a -L` 会输出我们需要的全部语义：
//!
//! ```text
//! en0: flags=8863<UP,BROADCAST,...> mtu 1500
//!     inet6 2001:db8:800:140:e820:4ebc:8f69:178d prefixlen 64 autoconf secured pltime 138 vltime 251
//!     inet6 2001:db8:800:140:75e4:235e:b46c:d2e7 prefixlen 64 autoconf temporary pltime 0 vltime 2591981
//!     inet6 fe80::...%en0 prefixlen 64 secured scopeid 0x6
//! ```
//!
//! 关键字 `temporary` / `deprecated` / `tentative` / `dadfailed` / `permanent`
//! 分别映射到 RFC 4941 / RFC 4862 的地址状态；`-L` 提供的 `pltime` / `vltime`
//! 就是 `preferred_lft` / `valid_lft`。这样 macOS 也能用上 Linux 侧的同一套
//! 选取规则与"过期前提前复查"的自适应调度。

use crate::select::{V6Cand, V6Kind, V6State};
use std::net::Ipv6Addr;
use std::process::Command;

/// 调用 `ifconfig -a -L`。失败时由调用方回退到 `local-ip-address`。
pub fn scan() -> Result<Vec<V6Cand>, String> {
    let args = ["-a", "-L"];
    let output = Command::new("/sbin/ifconfig")
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Command::new("ifconfig")
                    .args(args)
                    .env("LC_ALL", "C")
                    .output()
            } else {
                Err(e)
            }
        })
        .map_err(|e| format!("执行 ifconfig -a -L 失败: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ifconfig 退出码 {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_ifconfig(&text)
}

/// 纯函数：解析 `ifconfig -a -L` 文本，便于用 fixture 单测。
pub fn parse_ifconfig(text: &str) -> Result<Vec<V6Cand>, String> {
    let mut out = Vec::new();
    let mut ifname = String::new();
    let mut ifindex = 0u32;

    for raw in text.lines() {
        if raw.trim().is_empty() {
            continue;
        }

        // 接口头行：en0: flags=... ；地址行总是以空白开头
        if !raw.starts_with(char::is_whitespace) {
            if let Some((name, _)) = raw.split_once(':') {
                ifname = name.trim().to_string();
            } else {
                ifname.clear();
            }
            ifindex = 0;
            continue;
        }

        let line = raw.trim();
        if !line.starts_with("inet6 ") {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 2 || ifname.is_empty() {
            continue;
        }

        // 兼容 `fe80::1%en0` 与 `2001:db8::1/64` 两种写法
        let raw_addr = tokens[1].split('/').next().unwrap_or(tokens[1]);
        let addr_text = raw_addr.split('%').next().unwrap_or(raw_addr);
        let Ok(addr) = addr_text.parse::<Ipv6Addr>() else {
            continue;
        };

        let has = |key: &str| tokens.iter().any(|t| *t == key);
        let mut pltime = None;
        let mut vltime = None;
        for (i, tok) in tokens.iter().enumerate() {
            let next = tokens.get(i + 1).copied();
            match *tok {
                "pltime" => pltime = next.and_then(|s| s.parse::<u64>().ok()),
                "vltime" => vltime = next.and_then(|s| s.parse::<u64>().ok()),
                "scopeid" => {
                    ifindex = next
                        .and_then(|s| parse_scopeid(s))
                        .unwrap_or(0);
                }
                _ => {}
            }
        }
        let kind = if has("temporary") {
            V6Kind::Temporary
        } else {
            V6Kind::Stable
        };
        let state = if has("deprecated") || pltime == Some(0) {
            V6State::Deprecated
        } else {
            V6State::Preferred
        };
        let usable = !has("tentative") && !has("dadfailed") && vltime != Some(0);

        // 手工配置的全局地址通常既没有 autoconf 也没有 temporary/dynamic；
        // 它等价于 Linux 的 IFA_F_PERMANENT，应在同桶排序中优先。
        let permanent = has("permanent")
            || (!has("autoconf") && !has("temporary") && !has("dynamic"));

        out.push(V6Cand {
            ifindex,
            ifname: ifname.clone(),
            addr,
            kind,
            state,
            usable,
            permanent,
            preferred_lft: pltime.unwrap_or(u64::MAX),
            valid_lft: vltime.unwrap_or(u64::MAX),
            // macOS 没有 cstamp。现有排序在 cstamp 相同后比较 valid_lft，
            // SLAAC 新地址的剩余寿命更长，仍会胜出。
            cstamp: 0,
        });
    }

    if out.is_empty() {
        Err("ifconfig 输出中未找到可解析的 inet6 地址".into())
    } else {
        Ok(out)
    }
}

fn parse_scopeid(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\
\tinet 127.0.0.1 netmask 0xff000000\n\
\tinet6 ::1 prefixlen 128\n\
en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\
\tinet6 fe80::ae90:fcff:fe00:7e5%en0 prefixlen 64 secured scopeid 0x6\n\
\tinet6 2001:df1:7880:100:e820:4ebc:8f69:178d prefixlen 64 autoconf secured pltime 138 vltime 251\n\
\tinet6 2001:df1:7880:100:75e4:235e:b46c:d2e7 prefixlen 64 autoconf temporary pltime 0 vltime 2591981\n\
";

    #[test]
    fn parses_flags_and_lifetimes() {
        let v = parse_ifconfig(SAMPLE).unwrap();
        // ::1 与 fe80 也在列表里，但会被 select::scope_of 过滤
        assert_eq!(v.len(), 4);

        let stable = v
            .iter()
            .find(|c| c.addr.to_string() == "2001:df1:7880:100:e820:4ebc:8f69:178d")
            .unwrap();
        assert_eq!(stable.ifname, "en0");
        assert_eq!(stable.ifindex, 6);
        assert_eq!(stable.kind, V6Kind::Stable);
        assert_eq!(stable.state, V6State::Preferred);
        assert_eq!(stable.preferred_lft, 138);
        assert_eq!(stable.valid_lft, 251);
        assert!(stable.usable);
        assert!(!stable.permanent);

        let temp = v
            .iter()
            .find(|c| c.addr.to_string() == "2001:df1:7880:100:75e4:235e:b46c:d2e7")
            .unwrap();
        assert_eq!(temp.kind, V6Kind::Temporary);
        assert_eq!(temp.state, V6State::Deprecated);
        assert_eq!(temp.preferred_lft, 0);
        assert_eq!(temp.valid_lft, 2_591_981);
    }

    #[test]
    fn link_local_and_loopback_are_reported_but_excluded_by_select() {
        let v = parse_ifconfig(SAMPLE).unwrap();
        assert!(v.iter().any(|c| c.addr.is_loopback()));
        assert!(v.iter().any(|c| c.addr.octets()[0] == 0xfe));
    }

    #[test]
    fn static_address_is_permanent_with_infinite_lifetime() {
        let text = "\
en5: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\
\tinet6 2001:df1:7880:100::4c3 prefixlen 64\n\
";
        let v = parse_ifconfig(text).unwrap();
        assert!(v[0].permanent);
        assert_eq!(v[0].preferred_lft, u64::MAX);
        assert_eq!(v[0].valid_lft, u64::MAX);
    }

    #[test]
    fn tentative_is_unusable() {
        let text = "\
en0: flags=8863<UP,BROADCAST> mtu 1500\n\
\tinet6 2001:db8::1 prefixlen 64 autoconf tentative pltime 10 vltime 20\n\
";
        let v = parse_ifconfig(text).unwrap();
        assert!(!v[0].usable);
    }

    #[test]
    fn cidr_and_zone_suffixes_are_accepted() {
        let text = "\
en0: flags=8863<UP> mtu 1500\n\
\tinet6 2001:db8::2/64 prefixlen 64 autoconf pltime 100 vltime 200\n\
\tinet6 fe80::1%en0 prefixlen 64 scopeid 0x6\n\
";
        let v = parse_ifconfig(text).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].preferred_lft, 100);
        assert_eq!(v[1].ifindex, 6);
    }

    #[test]
    fn empty_or_malformed_output_is_an_error() {
        assert!(parse_ifconfig("").is_err());
        assert!(parse_ifconfig("en0: flags=8863<UP> mtu 1500\n\tinet 1.2.3.4 netmask 0xffffff00\n").is_err());
    }

    #[test]
    fn scopeid_parsing() {
        assert_eq!(parse_scopeid("0x6"), Some(6));
        assert_eq!(parse_scopeid("12"), Some(12));
        assert_eq!(parse_scopeid("0xzz"), None);
    }
}
