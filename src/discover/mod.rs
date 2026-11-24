//! 期望状态发现：公网 IPv4 探测 + 本机 IPv6 枚举 + 选取。

pub mod ipv4;
pub mod ipv6;
#[cfg(target_os = "macos")]
pub mod ipv6_macos;

use crate::config::Config;
use crate::log;
use crate::plan::Desired;
use crate::select::{self, Pick, V6Cand};
use crate::state::State;
use std::net::IpAddr;
use std::time::Duration;

/// 共享的 HTTP 客户端（连接池 / TLS 会话复用）。旧实现每次请求都新建 agent。
pub struct Http {
    pub agent: ureq::Agent,
}

impl Http {
    pub fn new(timeout: Duration) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(timeout)
            .timeout(timeout)
            .build();
        Self { agent }
    }
}

/// 观察一轮：探测公网 IPv4（如需要）并选取 IPv6（如需要）。
///
/// 失败不写入空值，而是返回 `None`——调用方据此跳过对应类型的 DNS 更新，
/// 保留旧记录（旧实现返回 `""`，会一路传到 API 参数里）。
pub fn observe(
    cfg: &Config,
    http: &Http,
    probe: &mut ipv4::Probe,
    state: &mut State,
) -> Desired {
    let mut desired = Desired::default();

    if cfg.want_v4 {
        match probe.probe(http) {
            Some(ip) => {
                log::debug(format!(
                    "公网 IPv4: {ip}（来源 {}）",
                    probe.endpoint().unwrap_or("?")
                ));
                desired.v4 = Some(IpAddr::V4(ip));
            }
            None => {
                if state.warn_once("v4-probe-failed") {
                    log::warn("所有 IPv4 探测端点均失败，本轮不更新 A 记录（保留旧值）");
                }
            }
        }
    }

    if cfg.want_v6 {
        match scan_v6(cfg, state) {
            Ok(cands) => {
                let pick = select::select(
                    &cands,
                    &select::Cfg {
                        prefer: cfg.ipv6_prefer,
                        allow_ula: cfg.allow_ula,
                    },
                    ipv6::kernel_source_addr(),
                );
                match pick {
                    Some(Pick::Use {
                        addr,
                        preferred_lft,
                        warn,
                    }) => {
                        log::debug(format!(
                            "IPv6 选定: {addr}（preferred_lft={}）",
                            if preferred_lft == u64::MAX {
                                "永久".to_string()
                            } else {
                                format!("{preferred_lft}s")
                            }
                        ));
                        if let Some(w) = warn {
                            if state.warn_once(w) {
                                log::warn(w);
                            }
                        }
                        desired.v6 = Some(IpAddr::V6(addr));
                        desired.v6_lft = if preferred_lft == u64::MAX {
                            None
                        } else {
                            Some(preferred_lft)
                        };
                    }
                    Some(Pick::Refuse { reason }) => {
                        if state.warn_once(reason) {
                            log::warn(format!("{reason}；本轮不更新 AAAA 记录"));
                        }
                    }
                    None => {
                        if state.warn_once("v6-none") {
                            log::warn("本机没有可用的公网 IPv6 地址，本轮不更新 AAAA 记录");
                        }
                    }
                }
            }
            Err(e) => {
                if state.warn_once("v6-scan-failed") {
                    log::warn(format!("读取 IPv6 地址失败: {e}；本轮不更新 AAAA 记录"));
                }
            }
        }
    }

    desired
}

/// 枚举地址并按网卡过滤。
///
/// 指定网卡存在但没取到可用地址时，回退到全部网卡并告警——与旧行为一致，
/// 但不再是静默的。
fn scan_v6(cfg: &Config, state: &mut State) -> Result<Vec<V6Cand>, String> {
    let all = ipv6::scan()?;
    if cfg.interface.is_empty() {
        return Ok(all);
    }

    let filtered: Vec<V6Cand> = all
        .iter()
        .filter(|c| c.ifname == cfg.interface)
        .cloned()
        .collect();
    if filtered.is_empty() {
        if state.warn_once(format!("iface-missing-{}", cfg.interface)) {
            log::warn(format!(
                "网卡 {} 上未找到 IPv6 地址（或网卡不存在），回退为自动搜索",
                cfg.interface
            ));
        }
        return Ok(all);
    }
    Ok(filtered)
}
