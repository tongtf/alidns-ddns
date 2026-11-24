//! 公网 IPv4 探测。
//!
//! 多端点回退 + **端点粘性**：记住上次成功的端点，下次优先用它，避免在 ipify 不可达的
//! 网络里每轮先白等 8 秒超时。
//!
//! 返回结果必须通过 `is_public_v4` 校验；运营商劫持页/透明代理返回的私网地址一律拒绝。

use super::Http;
use crate::util::is_public_v4;
use serde::Deserialize;
use std::net::Ipv4Addr;

/// 优先使用仅含 A 记录的域名，确保连接走 IPv4、返回的就是公网 IPv4。
const ENDPOINTS: &[&str] = &[
    "https://api.ipify.org?format=json",
    "https://v4.ident.me",
    "https://ip.3322.net",
    "https://ipv4.icanhazip.com",
    "https://checkip.amazonaws.com",
];

#[derive(Deserialize)]
struct IpResponse {
    ip: String,
}

#[derive(Default)]
pub struct Probe {
    sticky: Option<usize>,
}

impl Probe {
    pub fn probe(&mut self, http: &Http) -> Option<Ipv4Addr> {
        let start = self.sticky.unwrap_or(0) % ENDPOINTS.len();
        for offset in 0..ENDPOINTS.len() {
            let idx = (start + offset) % ENDPOINTS.len();
            if let Some(ip) = fetch(http, ENDPOINTS[idx]) {
                self.sticky = Some(idx);
                return Some(ip);
            }
        }
        self.sticky = None;
        None
    }

    pub fn endpoint(&self) -> Option<&'static str> {
        self.sticky.map(|i| ENDPOINTS[i])
    }
}

fn fetch(http: &Http, url: &str) -> Option<Ipv4Addr> {
    let resp = match http.agent.get(url).call() {
        Ok(r) => r,
        Err(e) => {
            crate::log::debug(format!("IPv4 端点 {url} 失败: {e}"));
            return None;
        }
    };
    let body = match resp.into_string() {
        Ok(b) => b,
        Err(e) => {
            crate::log::debug(format!("IPv4 端点 {url} 读取响应失败: {e}"));
            return None;
        }
    };
    let body = body.trim();
    // 兼容 JSON（{"ip":"..."}）与纯文本两种响应
    let text = serde_json::from_str::<IpResponse>(body)
        .map(|r| r.ip)
        .unwrap_or_else(|_| body.to_string());
    let ip = match text.trim().parse::<Ipv4Addr>() {
        Ok(ip) => ip,
        Err(_) => {
            crate::log::debug(format!(
                "IPv4 端点 {url} 返回无法解析的内容: {}",
                crate::util::truncate(body, 80)
            ));
            return None;
        }
    };
    if !is_public_v4(ip) {
        crate::log::debug(format!("IPv4 端点 {url} 返回非公网地址 {ip}，已拒绝"));
        return None;
    }
    Some(ip)
}
