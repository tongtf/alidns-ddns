//! 通用小工具：时间戳、UTF-8 安全截断、公网地址判定。

use std::net::Ipv4Addr;
use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 UTC 时间，RFC3339（秒精度）。用于日志前缀与 ACS3 签名。
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let sec_of_day = (secs % 86_400) as i64;
    let (hour, minute, second) = (sec_of_day / 3600, (sec_of_day % 3600) / 60, sec_of_day % 60);
    // Howard Hinnant 的 civil 日期算法（days since 1970-01-01 -> y/m/d）
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// 按字符边界截断。用于打印服务端原始响应，避免在 UTF-8 多字节字符中间切片导致 panic。
pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 是否为可公网路由的 IPv4（RFC 6890 特殊用途地址全部排除）。
///
/// 对外部探测端点返回的值必须做这项校验：运营商劫持页、透明代理或错误配置的端点
/// 都可能返回 `10.x` / `192.168.x`，直接写进 A 记录会让域名指向不可达地址。
pub fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        // 100.64.0.0/10 CGNAT
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
        // 192.0.0.0/24 IETF 协议专用
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        // 198.18.0.0/15 基准测试
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
        // 192.88.99.0/24 6to4 中继（RFC 7526 已弃用）
        || (o[0] == 192 && o[1] == 88 && o[2] == 99)
        // 240.0.0.0/4 保留
        || o[0] >= 240)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "abcdef中文中文中文";
        for n in 0..s.len() + 4 {
            let out = truncate(s, n);
            assert!(out.len() <= n.min(s.len()));
            assert!(s.starts_with(out));
        }
    }

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn public_v4_rejects_special_ranges() {
        for ip in ["10.0.0.1", "192.168.1.1", "172.16.0.1", "100.64.0.1",
                   "127.0.0.1", "169.254.1.1", "0.0.0.0", "198.18.0.1",
                   "192.0.2.1", "203.0.113.9", "240.0.0.1", "255.255.255.255",
                   "224.0.0.1", "239.255.255.250"] {
            assert!(!is_public_v4(ip.parse().unwrap()), "{ip} 应被拒绝");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "223.5.5.5", "114.114.114.114"] {
            assert!(is_public_v4(ip.parse().unwrap()), "{ip} 应被接受");
        }
    }

    #[test]
    fn rfc3339_shape() {
        let t = now_rfc3339();
        assert_eq!(t.len(), 20, "{t}");
        assert!(t.ends_with('Z'));
    }
}
