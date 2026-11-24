//! 阿里云 DNS 客户端：签名调用、分页读取、错误分类。
//!
//! 错误分类是重试策略的前提：网络/限流类可以退避后重试，鉴权/时钟类必须快速失败，
//! 参数类只影响单个目标。全部失败都压成 `String::new()` 是旧实现最大的健壮性缺口。

pub mod model;
pub mod sign;

use crate::config::{Config, Target};
use crate::log;
use crate::util::{now_rfc3339, truncate};
use model::{Ack, ErrorResp, ListResp, Record};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 单页最大条数（阿里云文档上限 500）。
const PAGE_SIZE: u64 = 500;
/// 翻页上限，防止 `TotalCount` 异常导致死循环。
const MAX_PAGES: u64 = 20;

#[derive(Debug)]
pub enum ApiError {
    /// 传输层失败（超时、TLS、连接被拒）：可重试。
    Transport(String),
    /// 限流 / 服务端临时错误：退避后重试。
    Throttled(String),
    /// 鉴权失败：重试无意义，必须退出。
    Auth(String),
    /// 本机时钟偏差过大：文件签名必然失败，需提示 NTP。
    Clock(String),
    /// 参数 / 业务错误：只影响当前目标。
    Param(String),
    /// 域名级配置错误（如 InvalidDomainName）：重试无意义，必须退出 78。
    Config(String),
}

impl ApiError {
    pub fn fatal(&self) -> bool {
        matches!(
            self,
            ApiError::Auth(_) | ApiError::Clock(_) | ApiError::Config(_)
        )
    }

    pub fn message(&self) -> &str {
        match self {
            ApiError::Transport(m)
            | ApiError::Throttled(m)
            | ApiError::Auth(m)
            | ApiError::Clock(m)
            | ApiError::Param(m)
            | ApiError::Config(m) => m,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            ApiError::Transport(_) => "网络",
            ApiError::Throttled(_) => "限流",
            ApiError::Auth(_) => "鉴权",
            ApiError::Clock(_) => "时钟",
            ApiError::Param(_) => "参数",
            ApiError::Config(_) => "配置",
        };
        write!(f, "{kind}错误: {}", self.message())
    }
}

pub struct Client {
    agent: ureq::Agent,
    ak: String,
    sk: String,
}

impl Client {
    pub fn new(cfg: &Config) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build();
        Self {
            agent,
            ak: cfg.access_key_id.clone(),
            sk: cfg.access_key_secret.clone(),
        }
    }

    fn call(&self, action: &str, params: BTreeMap<String, String>) -> Result<String, ApiError> {
        let ts = now_rfc3339();
        let nonce = nonce();
        let signed = sign::sign(action, &params, &self.ak, &self.sk, &ts, &nonce);

        let mut req = self.agent.get(&signed.url);
        for (k, v) in &signed.headers {
            req = req.set(k, v);
        }

        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                return Err(classify(code, &body));
            }
            Err(e) => return Err(ApiError::Transport(format!("{action}: {e}"))),
        };

        resp.into_string()
            .map_err(|e| ApiError::Transport(format!("{action}: 读取响应失败: {e}")))
    }

    /// 一次性取回该域名下**全部**记录（自动翻页）。
    ///
    /// 旧实现不传 `PageSize`，阿里云默认只返回 20 条；记录数超过 20 时目标可能不在
    /// 首屏，于是被误判为"不存在"并新建重复记录。
    pub fn list_records(&self, domain: &str) -> Result<Vec<Record>, ApiError> {
        let mut out = Vec::new();
        let mut page = 1u64;
        loop {
            let mut p = BTreeMap::new();
            p.insert("DomainName".to_string(), domain.to_string());
            p.insert("PageNumber".to_string(), page.to_string());
            p.insert("PageSize".to_string(), PAGE_SIZE.to_string());

            let body = self.call("DescribeDomainRecords", p)?;
            let parsed: ListResp = serde_json::from_str(&body).map_err(|e| {
                ApiError::Transport(format!(
                    "DescribeDomainRecords 响应解析失败: {e}; 原始响应: {}",
                    truncate(&body, 300)
                ))
            })?;

            if let Some(code) = parsed.code.as_deref().filter(|c| !c.is_empty()) {
                return Err(classify_code(code, parsed.message.as_deref().unwrap_or("")));
            }

            let got = parsed.records.record.len() as u64;
            log::debug(format!(
                "{domain}: 第 {} 页 {} 条, TotalCount={}, RequestId={}",
                parsed.page, got, parsed.total, parsed.request_id
            ));
            out.extend(parsed.records.record);
            if page >= MAX_PAGES && got > 0 && page * PAGE_SIZE < parsed.total {
                log::warn(format!(
                    "{domain} 记录数超过 {MAX_PAGES} 页（TotalCount={}），已截断；请拆分域名或清理记录",
                    parsed.total
                ));
            }
            if got == 0 || page * PAGE_SIZE >= parsed.total || page >= MAX_PAGES {
                break;
            }
            page += 1;
        }
        Ok(out)
    }

    pub fn update(
        &self,
        target: &Target,
        record_id: &str,
        value: &str,
        ttl: u64,
    ) -> Result<(), ApiError> {
        let mut p = BTreeMap::new();
        p.insert("RecordId".to_string(), record_id.to_string());
        p.insert("RR".to_string(), target.rr.clone());
        p.insert("Type".to_string(), target.rtype.as_str().to_string());
        p.insert("Value".to_string(), value.to_string());
        p.insert("TTL".to_string(), ttl.to_string());
        p.insert("Line".to_string(), target.line.clone());
        self.ack("UpdateDomainRecord", p)
    }

    pub fn add(&self, target: &Target, value: &str, ttl: u64) -> Result<(), ApiError> {
        let mut p = BTreeMap::new();
        p.insert("DomainName".to_string(), target.domain.clone());
        p.insert("RR".to_string(), target.rr.clone());
        p.insert("Type".to_string(), target.rtype.as_str().to_string());
        p.insert("Value".to_string(), value.to_string());
        p.insert("TTL".to_string(), ttl.to_string());
        p.insert("Line".to_string(), target.line.clone());
        self.ack("AddDomainRecord", p)
    }

    fn ack(&self, action: &str, params: BTreeMap<String, String>) -> Result<(), ApiError> {
        let body = self.call(action, params)?;
        match serde_json::from_str::<Ack>(&body) {
            Ok(ack) => match ack.code {
                // HTTP 200 但带 Code，仍是错误
                Some(code) if !code.is_empty() => {
                    Err(classify_code(&code, ack.message.as_deref().unwrap_or("")))
                }
                _ => {
                    if let Some(id) = ack.request_id {
                        log::debug(format!("{action} 成功, RequestId={id}"));
                    }
                    Ok(())
                }
            },
            Err(e) => Err(ApiError::Transport(format!(
                "{action} 响应解析失败: {e}; 原始响应: {}",
                truncate(&body, 300)
            ))),
        }
    }
}

fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = u128::from(std::process::id());
    let mixed = nanos ^ (pid << 64) ^ (u128::from(n) << 32);
    format!("{mixed:032x}")
}

/// 错误大类。判据以错误码前缀为主，避免依赖具体文案。
enum Kind {
    Auth,
    Clock,
    Throttled,
    Param,
}

fn kind_of(code: &str) -> Kind {
    let lower = code.to_ascii_lowercase();
    if lower.starts_with("invalidaccesskeyid")
        || lower.starts_with("invalidsignature")
        || lower.starts_with("signaturedoesnotmatch")
        || lower.starts_with("incompletesignature")
        || lower.starts_with("forbidden")
        || lower.starts_with("nopermission")
        || lower.starts_with("unauthorized")
    {
        return Kind::Auth;
    }
    if lower.starts_with("invalidtimestamp") || lower.starts_with("invaliddate") {
        return Kind::Clock;
    }
    if lower.starts_with("throttling")
        || lower.starts_with("serviceunavailable")
        || lower.starts_with("internalerror")
        || lower.starts_with("unknownerror")
    {
        return Kind::Throttled;
    }
    Kind::Param
}

fn with_kind(kind: Kind, text: String) -> ApiError {
    match kind {
        Kind::Auth => ApiError::Auth(text),
        Kind::Clock => ApiError::Clock(format!(
            "{text} —— 本机时钟与阿里云偏差过大，请检查 NTP（timedatectl status）"
        )),
        Kind::Throttled => ApiError::Throttled(text),
        Kind::Param => ApiError::Param(text),
    }
}

pub fn classify_code(code: &str, message: &str) -> ApiError {
    with_kind(kind_of(code), format!("{code}({message})"))
}

fn classify(status: u16, body: &str) -> ApiError {
    // 先看业务错误码：阿里云用 404 返回 InvalidAccessKeyId.NotFound 这类鉴权错误，
    // 只看 HTTP 状态会把"必须退出"误判成"仅该目标失败"。
    if let Ok(e) = serde_json::from_str::<ErrorResp>(body) {
        if !e.code.is_empty() {
            let text = format!(
                "HTTP {status} {}({}) RequestId={}",
                e.code, e.message, e.request_id
            );
            return with_kind(kind_of(&e.code), text);
        }
    }

    let text = format!("HTTP {status} {}", truncate(body, 300));
    match status {
        401 | 403 => ApiError::Auth(text),
        429 => ApiError::Throttled(text),
        c if c >= 500 => ApiError::Transport(text),
        _ => ApiError::Param(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_codes_are_fatal() {
        for code in [
            "InvalidAccessKeyId.NotFound",
            "SignatureDoesNotMatch",
            "Forbidden.RAM",
            "NoPermission",
        ] {
            let e = classify_code(code, "x");
            assert!(e.fatal(), "{code} 应判为致命");
        }
    }

    #[test]
    fn clock_code_suggests_ntp() {
        let e = classify_code("InvalidTimeStamp.Expired", "expired");
        assert!(e.fatal());
        assert!(e.message().contains("NTP"));
    }

    #[test]
    fn throttling_is_retryable() {
        let e = classify_code("Throttling.User", "slow down");
        assert!(!e.fatal());
        assert!(matches!(e, ApiError::Throttled(_)));
    }

    #[test]
    fn duplicate_is_param_scoped() {
        let e = classify_code("DomainRecordDuplicate", "exists");
        assert!(!e.fatal());
        assert!(matches!(e, ApiError::Param(_)));
    }

    #[test]
    fn http_5xx_is_retryable() {
        assert!(matches!(classify(503, ""), ApiError::Transport(_)));
        assert!(matches!(classify(429, ""), ApiError::Throttled(_)));
        assert!(classify(403, "").fatal());
    }

    #[test]
    fn domain_config_error_is_fatal() {
        let e = ApiError::Config("InvalidDomainName".into());
        assert!(e.fatal());
        assert!(e.to_string().contains("配置"));
    }

    #[test]
    fn body_code_beats_http_status() {
        // 阿里云用 404 返回鉴权错误
        let body = r#"{"Code":"InvalidAccessKeyId.NotFound","Message":"Specified access key is not found.","RequestId":"x"}"#;
        let e = classify(404, body);
        assert!(e.fatal(), "{e:?}");
        assert!(matches!(e, ApiError::Auth(_)));
    }

    #[test]
    fn nonce_is_unique() {
        let a = nonce();
        let b = nonce();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
    }
}
