//! 参数 / 环境变量 / 配置文件解析与校验。
//!
//! 优先级：命令行 > 环境变量（非空） > config.json > 默认值。
//! 校验失败一律快速失败（退出码 78），不再"警告后静默用默认值"——配置错误应当
//! 让 systemd 直接把服务标成 failed，而不是每轮打印一条缺字段的日志。

use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// sysexits.h: EX_CONFIG
pub const EX_CONFIG: i32 = 78;
/// sysexits.h: EX_USAGE
pub const EX_USAGE: i32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RType {
    A,
    Aaaa,
}

impl RType {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "A" => Some(RType::A),
            "AAAA" => Some(RType::Aaaa),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RType::A => "A",
            RType::Aaaa => "AAAA",
        }
    }
}

impl std::fmt::Display for RType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一个待对账的 DNS 记录：域名 + 主机记录 + 线路 + 类型。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub domain: String,
    pub rr: String,
    pub line: String,
    pub rtype: RType,
}

impl Target {
    pub fn fqdn(&self) -> String {
        if self.rr == "@" {
            self.domain.clone()
        } else {
            format!("{}.{}", self.rr, self.domain)
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{}]", self.fqdn(), self.rtype)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum V6Prefer {
    Auto,
    Stable,
    Temporary,
}

impl V6Prefer {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(V6Prefer::Auto),
            "stable" => Some(V6Prefer::Stable),
            "temporary" | "temp" => Some(V6Prefer::Temporary),
            _ => None,
        }
    }
}

pub struct Config {
    pub access_key_id: String,
    pub access_key_secret: String,
    pub targets: Vec<Target>,
    pub want_v4: bool,
    pub want_v6: bool,
    pub interface: String,
    pub interval: u64,
    pub verify_interval: u64,
    pub ttl: u64,
    pub ipv6_prefer: V6Prefer,
    pub allow_ula: bool,
    pub dry_run: bool,
    pub verbose: bool,
    pub events: bool,
    /// IP 变更后执行的外部命令；None 表示不执行。
    pub on_change: Option<String>,
    pub on_change_timeout: u64,
    /// 状态文件路径；None 表示禁用持久化。
    pub state_file: Option<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Default)]
pub struct Args {
    pub access_key_id: Option<String>,
    pub access_key_secret: Option<String>,
    pub domain: Option<String>,
    pub rr: Option<String>,
    pub ipv: Option<String>,
    pub interface: Option<String>,
    pub interval: Option<u64>,
    pub verify_interval: Option<u64>,
    pub ttl: Option<u64>,
    pub line: Option<String>,
    pub ipv6_prefer: Option<String>,
    pub config: Option<String>,
    pub state_file: Option<String>,
    pub allow_ula: bool,
    pub no_state: bool,
    pub on_change: Option<String>,
    pub on_change_timeout: Option<u64>,
    pub dry_run: bool,
    pub verbose: bool,
    pub no_events: bool,
    pub help: bool,
}

pub fn usage() -> String {
    r#"alidns-ddns — 阿里云域名动态解析工具

用法:
  alidns-ddns [选项]

必填:
  --access-key-id <ID>            AccessKey ID      (ALIBABA_CLOUD_ACCESS_KEY_ID)
  --access-key-secret <SECRET>    AccessKey Secret  (ALIBABA_CLOUD_ACCESS_KEY_SECRET)
  --domain <DOMAIN>               域名，多个用逗号分隔 (DDNS_DOMAIN)

基础:
  --rr <RR>                       主机记录，多个用逗号分隔 (DDNS_RR, 默认 @)
  --ipv <MODE>                    IP 模式: 4 / 6 / 46 (DDNS_IPV, 默认 6)
  --interface <NAME>              取 IPv6 的网卡名 (DDNS_INTERFACE, 默认自动)
  --line <LINE>                   解析线路 (DDNS_LINE, 默认 default)
  --ttl <SECS>                    记录 TTL (DDNS_TTL, 默认 600)
  --interval <SECS>               轮询间隔 (DDNS_INTERVAL, 默认 300)
  --verify-interval <SECS>        IP 未变时的 DNS 对账周期 (DDNS_VERIFY_INTERVAL, 默认 21600)

IPv6 策略:
  --ipv6-prefer <MODE>            auto / stable / temporary (DDNS_IPV6_PREFER, 默认 auto)
  --allow-ula                     允许把 ULA(fc00::/7) 写入 AAAA（默认拒绝）
  --no-events                     关闭 netlink 地址变更事件订阅（Linux）
  --state-file <PATH>             状态文件路径 (DDNS_STATE_FILE；不放在 config.json)
  --no-state                      禁用状态持久化（每次运行都从零开始）
  --on-change <CMD>               IP 实际变更后执行的命令 (DDNS_ON_CHANGE)
  --on-change-timeout <SECS>      钩子超时 (DDNS_ON_CHANGE_TIMEOUT, 默认 60)

运行模式:
  --dry-run                       只打印计划，不调用写接口
  -v, --verbose                   输出候选地址与判定细节
  -c, --config <PATH>             配置文件路径（默认 config.json）
  -h, --help                      显示此帮助

优先级: 命令行参数 > 环境变量 > config.json > 默认值
"#
    .to_string()
}

pub fn parse_args() -> Result<Args, String> {
    parse_args_from(env::args().skip(1))
}

fn parse_args_from<I: Iterator<Item = String>>(mut it: I) -> Result<Args, String> {
    let mut a = Args::default();
    while let Some(arg) = it.next() {
        if arg == "--" {
            if let Some(rest) = it.next() {
                return Err(format!("不支持位置参数: {rest}"));
            }
            break;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            let (k, inline) = match rest.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (rest, None),
            };
            match k {
                "help" => a.help = true,
                "dry-run" => a.dry_run = true,
                "verbose" => a.verbose = true,
                "allow-ula" => a.allow_ula = true,
                "no-events" => a.no_events = true,
                "no-state" => a.no_state = true,
                "on-change" => a.on_change = Some(value(k, inline, &mut it)?),
                "on-change-timeout" => a.on_change_timeout = Some(number(k, inline, &mut it)?),
                "access-key-id" => a.access_key_id = Some(value(k, inline, &mut it)?),
                "access-key-secret" => a.access_key_secret = Some(value(k, inline, &mut it)?),
                "domain" => a.domain = Some(value(k, inline, &mut it)?),
                "rr" => a.rr = Some(value(k, inline, &mut it)?),
                "ipv" => a.ipv = Some(value(k, inline, &mut it)?),
                "interface" => a.interface = Some(value(k, inline, &mut it)?),
                "line" => a.line = Some(value(k, inline, &mut it)?),
                "ipv6-prefer" => a.ipv6_prefer = Some(value(k, inline, &mut it)?),
                "config" => a.config = Some(value(k, inline, &mut it)?),
                "state-file" => a.state_file = Some(value(k, inline, &mut it)?),
                "interval" => a.interval = Some(number(k, inline, &mut it)?),
                "verify-interval" => a.verify_interval = Some(number(k, inline, &mut it)?),
                "ttl" => a.ttl = Some(number(k, inline, &mut it)?),
                _ => return Err(format!("未知选项: --{k}")),
            }
        } else if let Some(rest) = arg.strip_prefix('-') {
            let (k, inline) = match rest.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (rest, None),
            };
            match k {
                "h" => a.help = true,
                "v" => a.verbose = true,
                "c" => a.config = Some(value("c", inline, &mut it)?),
                _ => return Err(format!("未知选项: -{k}")),
            }
        } else {
            return Err(format!("非法参数: {arg}"));
        }
    }
    Ok(a)
}

fn value<I: Iterator<Item = String>>(
    k: &str,
    inline: Option<&str>,
    it: &mut I,
) -> Result<String, String> {
    match inline {
        Some(v) => Ok(v.to_string()),
        None => it.next().ok_or_else(|| format!("选项 --{k} 缺少值")),
    }
}

fn number<I: Iterator<Item = String>>(
    k: &str,
    inline: Option<&str>,
    it: &mut I,
) -> Result<u64, String> {
    let raw = value(k, inline, it)?;
    raw.trim()
        .parse::<u64>()
        .map_err(|_| format!("选项 --{k} 需要非负整数: {raw}"))
}

// 未知键直接报错，兑现"配置错误快速失败"的承诺（拼错 ipv6_prefer 等不再静默用默认值）
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    domain: String,
    rr: String,
    ipv: String,
    interface: String,
    line: String,
    ipv6_prefer: String,
    interval: Option<u64>,
    verify_interval: Option<u64>,
    ttl: Option<u64>,
    allow_ula: bool,
    on_change: String,
    on_change_timeout: Option<u64>,
}

/// 布尔覆盖：CLI 只能显式开启；环境变量可显式 true/false；都未设置才用文件/默认值。
fn pick_bool(arg: bool, var: &str, from_file: bool) -> Result<bool, String> {
    if arg {
        return Ok(true);
    }
    match env::var(var) {
        Ok(s) if !s.trim().is_empty() => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(true),
            "0" | "false" | "no" | "off" => Ok(false),
            other => Err(format!("环境变量 {var} 需要 true/false，收到: {other}")),
        },
        _ => Ok(from_file),
    }
}

fn pick(arg: Option<String>, var: &str, from_file: String) -> String {
    arg.filter(|s| !s.is_empty())
        .or_else(|| env::var(var).ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or(from_file)
}

fn pick_u64(
    arg: Option<u64>,
    var: &str,
    from_file: Option<u64>,
    default: u64,
) -> Result<u64, String> {
    if let Some(v) = arg {
        return Ok(v);
    }
    match env::var(var) {
        Ok(s) if !s.trim().is_empty() => s
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("环境变量 {var} 需要非负整数: {s}")),
        _ => Ok(from_file.unwrap_or(default)),
    }
}

/// 从 systemd `LoadCredential=` 注入的目录读取凭据文件（内容按行读取并 trim）。
fn credential_from(dir: &str, name: &str) -> Option<String> {
    let path = Path::new(dir).join(name);
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn read_credential(name: &str) -> Option<String> {
    let dir = env::var("CREDENTIALS_DIRECTORY").ok()?;
    credential_from(dir.trim(), name)
}

/// 凭证优先级：CLI > LoadCredential 文件 > 环境变量。
/// 凭据文件由 systemd 以 root 读取并挂载，进程无需自己读 600 的源文件。
fn credential(arg: Option<String>, cred_name: &str, var: &str) -> Option<String> {
    arg.filter(|s| !s.is_empty())
        .or_else(|| read_credential(cred_name))
        .or_else(|| {
            env::var(var)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string())
        })
}

pub fn from_args(args: &Args) -> Result<Config, String> {
    let mut warnings = Vec::new();
    let path = args.config.clone().unwrap_or_else(|| "config.json".into());
    let file = load_file(&path)?;

    if args.access_key_secret.is_some() {
        warnings.push(
            "--access-key-secret 会出现在 /proc/<pid>/cmdline，建议改用环境变量或 systemd LoadCredential"
                .into(),
        );
    }

    let access_key_id = credential(
        args.access_key_id.clone(),
        "alidns-access-key-id",
        "ALIBABA_CLOUD_ACCESS_KEY_ID",
    );
    let access_key_secret = credential(
        args.access_key_secret.clone(),
        "alidns-access-key-secret",
        "ALIBABA_CLOUD_ACCESS_KEY_SECRET",
    );
    let domain_raw = pick(args.domain.clone(), "DDNS_DOMAIN", file.domain);
    let rr_raw = pick(args.rr.clone(), "DDNS_RR", file.rr);
    let interface = pick(args.interface.clone(), "DDNS_INTERFACE", file.interface)
        .trim()
        .to_string();
    let ipv = pick(args.ipv.clone(), "DDNS_IPV", file.ipv)
        .trim()
        .to_string();
    let line = pick(args.line.clone(), "DDNS_LINE", file.line)
        .trim()
        .to_string();
    let prefer_raw = pick(
        args.ipv6_prefer.clone(),
        "DDNS_IPV6_PREFER",
        file.ipv6_prefer,
    );

    if access_key_id.as_deref().unwrap_or("").trim().is_empty()
        || access_key_secret.as_deref().unwrap_or("").trim().is_empty()
        || domain_raw.trim().is_empty()
    {
        return Err(
            "缺少必填项: DomainName / AccessKeyID / AccessKeySecret（密钥请通过环境变量或 LoadCredential 提供）"
                .into(),
        );
    }
    let access_key_id = access_key_id.unwrap();
    let access_key_secret = access_key_secret.unwrap();

    let (want_v4, want_v6) = match ipv.as_str() {
        "" | "6" => (false, true),
        "4" => (true, false),
        "46" => (true, true),
        other => {
            return Err(format!(
                "--ipv 只接受 4 / 6 / 46（也可用 DDNS_IPV 环境变量），收到: {other}"
            ))
        }
    };

    let interval = pick_u64(args.interval, "DDNS_INTERVAL", file.interval, 300)?;
    // 上界防止 `Instant::now() + Duration` 溢出（release profile 是 panic=abort）
    if !(1..=2_592_000).contains(&interval) {
        return Err("--interval 取值范围 1..2592000（30 天），收到: {interval}".into());
    }
    let verify_interval = pick_u64(
        args.verify_interval,
        "DDNS_VERIFY_INTERVAL",
        file.verify_interval,
        21_600,
    )?;
    if !(1..=31_536_000).contains(&verify_interval) {
        return Err(
            "--verify-interval 取值范围 1..31536000（1 年），收到: {verify_interval}".into(),
        );
    }
    let ttl = pick_u64(args.ttl, "DDNS_TTL", file.ttl, 600)?;
    if !(1..=86_400).contains(&ttl) {
        return Err(format!("--ttl 取值范围 1..86400，收到: {ttl}"));
    }

    let ipv6_prefer = if prefer_raw.trim().is_empty() {
        V6Prefer::Auto
    } else {
        V6Prefer::parse(&prefer_raw).ok_or_else(|| {
            format!("--ipv6-prefer 只接受 auto / stable / temporary，收到: {prefer_raw}")
        })?
    };

    let line = if line.is_empty() {
        "default".to_string()
    } else {
        line
    };
    let rr_raw = if rr_raw.trim().is_empty() {
        "@".to_string()
    } else {
        rr_raw
    };

    let on_change_timeout = pick_u64(
        args.on_change_timeout,
        "DDNS_ON_CHANGE_TIMEOUT",
        file.on_change_timeout,
        60,
    )?;
    if !(1..=3600).contains(&on_change_timeout) {
        return Err(format!(
            "--on-change-timeout 取值范围 1..3600，收到: {on_change_timeout}"
        ));
    }

    let targets = expand_targets(&domain_raw, &rr_raw, &line, want_v4, want_v6)?;

    let no_state = args.no_state || pick_bool(false, "DDNS_NO_STATE", false)?;
    let state_file = if no_state {
        None
    } else {
        let raw = pick(args.state_file.clone(), "DDNS_STATE_FILE", String::new());
        if raw.trim().is_empty() {
            default_state_path()
        } else {
            Some(PathBuf::from(raw.trim()))
        }
    };

    Ok(Config {
        access_key_id: access_key_id.trim().to_string(),
        // 环境变量 / 配置文件里常见的尾随空白会导致 InvalidSignature
        access_key_secret: access_key_secret.trim().to_string(),
        targets,
        want_v4,
        want_v6,
        interface,
        interval,
        verify_interval,
        ttl,
        ipv6_prefer,
        allow_ula: pick_bool(args.allow_ula, "DDNS_ALLOW_ULA", file.allow_ula)?,
        dry_run: args.dry_run,
        verbose: pick_bool(args.verbose, "DDNS_VERBOSE", false)?,
        events: !pick_bool(args.no_events, "DDNS_NO_EVENTS", false)?,
        on_change: {
            let cmd = pick(
                args.on_change.clone(),
                "DDNS_ON_CHANGE",
                file.on_change.clone(),
            )
            .trim()
            .to_string();
            if cmd.is_empty() {
                None
            } else {
                Some(cmd)
            }
        },
        on_change_timeout,
        state_file,
        warnings,
    })
}

/// 默认状态文件路径。systemd 的 `StateDirectory=` 会导出 `STATE_DIRECTORY`，
/// 用它可在 DynamicUser + ProtectSystem=strict 下正确写入私有状态目录。
fn default_state_path() -> Option<PathBuf> {
    if let Ok(dir) = env::var("STATE_DIRECTORY") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir.trim()).join("state.json"));
        }
    }
    if let Ok(base) = env::var("XDG_STATE_HOME") {
        if !base.trim().is_empty() {
            return Some(PathBuf::from(base.trim()).join("alidns-ddns/state.json"));
        }
    }
    if let Ok(home) = env::var("HOME") {
        if !home.trim().is_empty() {
            return Some(PathBuf::from(home.trim()).join(".local/state/alidns-ddns/state.json"));
        }
    }
    None
}

fn load_file(path: &str) -> Result<FileConfig, String> {
    match fs::read_to_string(path) {
        Ok(data) => {
            // 旧版键名直接给出迁移提示，而不是让用户面对 serde 的 unknown field
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) {
                if let Some(obj) = value.as_object() {
                    for old in ["AccessKeyID", "AccessKeySecret", "StateFile"] {
                        if obj.contains_key(old) {
                            return Err(format!(
                                "配置文件 {path} 不再支持 {old}；密钥请用 ALIBABA_CLOUD_ACCESS_KEY_* 或 LoadCredential，状态文件请用 DDNS_STATE_FILE"
                            ));
                        }
                    }
                }
            }
            serde_json::from_str(&data).map_err(|e| format!("配置文件 {path} 解析失败: {e}"))
        }
        // 文件不存在是正常情况（纯用 env/CLI 时），不报错
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(format!(
            "无法读取配置文件 {path}: {e}（检查路径与权限；服务以 DynamicUser 运行时需可读）"
        )),
    }
}

/// 按 DNS label 规则校验域名：非法域名应在配置阶段退出 78，
/// 而不是等到 API 返回 InvalidDomainName 后无限退避。
fn valid_domain(d: &str) -> bool {
    if d.is_empty() || d.len() > 253 {
        return false;
    }
    d.split('.').all(|label| {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        let bytes = label.as_bytes();
        bytes[0].is_ascii_alphanumeric()
            && bytes[bytes.len() - 1].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
    })
}

/// 展开 域名 × RR 笛卡尔目标，再按 IP 模式拆成 A / AAAA。
///
/// 配对规则保持旧行为：一对多、多对一、等长配对；其余报错。
fn expand_targets(
    domain_raw: &str,
    rr_raw: &str,
    line: &str,
    want_v4: bool,
    want_v6: bool,
) -> Result<Vec<Target>, String> {
    let split = |s: &str| -> Vec<String> {
        s.split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect()
    };
    let domains = split(domain_raw);
    let rrs = split(rr_raw);
    if domains.is_empty() {
        return Err("DomainName 为空".into());
    }
    if rrs.is_empty() {
        return Err("RR 为空".into());
    }
    for d in &domains {
        if !valid_domain(d) {
            return Err(format!("域名格式不合法: {d}"));
        }
    }

    let pairs: Vec<(String, String)> = if domains.len() == 1 && rrs.len() > 1 {
        rrs.iter()
            .map(|r| (domains[0].clone(), r.clone()))
            .collect()
    } else if rrs.len() == 1 {
        domains
            .iter()
            .map(|d| (d.clone(), rrs[0].clone()))
            .collect()
    } else if domains.len() == rrs.len() {
        domains.iter().cloned().zip(rrs.iter().cloned()).collect()
    } else {
        return Err(format!(
            "域名数({})与 RR 数({})无法配对（需一对多、多对一或等长）",
            domains.len(),
            rrs.len()
        ));
    };

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (domain, rr) in pairs {
        for rtype in [RType::A, RType::Aaaa] {
            let wanted = match rtype {
                RType::A => want_v4,
                RType::Aaaa => want_v6,
            };
            if !wanted {
                continue;
            }
            let t = Target {
                domain: domain.clone(),
                rr: rr.clone(),
                line: line.to_string(),
                rtype,
            };
            if seen.insert(t.clone()) {
                out.push(t);
            }
        }
    }
    if out.is_empty() {
        return Err("无有效的域名/RR 目标".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_one_domain_many_rr() {
        let t = expand_targets("a.com", "@,www", "default", false, true).unwrap();
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].fqdn(), "a.com");
        assert_eq!(t[1].fqdn(), "www.a.com");
        assert!(t.iter().all(|x| x.rtype == RType::Aaaa));
    }

    #[test]
    fn expand_many_domains_one_rr() {
        let t = expand_targets("a.com,b.com", "@", "default", true, true).unwrap();
        assert_eq!(t.len(), 4);
    }

    #[test]
    fn expand_rejects_mismatch() {
        assert!(expand_targets("a.com,b.com", "@,www,x", "default", false, true).is_err());
    }

    #[test]
    fn invalid_domains_are_rejected_early() {
        for d in [
            "a..com", "-a.com", "a-.com", "a/b.com", "a b.com", ".a.com", "a.com.",
        ] {
            assert!(!valid_domain(d), "{d} 应被拒绝");
        }
        for d in ["a.com", "xn--fiqs8s.com", "a-b.example.com"] {
            assert!(valid_domain(d), "{d} 应被接受");
        }
    }

    #[test]
    fn expand_dedupes() {
        let t = expand_targets("a.com,a.com", "@", "default", false, true).unwrap();
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn flag_parsing_supports_inline_and_space() {
        let a = parse_args_from(
            ["--domain=a.com", "--interval", "60"]
                .into_iter()
                .map(String::from),
        )
        .unwrap();
        assert_eq!(a.domain.as_deref(), Some("a.com"));
        assert_eq!(a.interval, Some(60));
    }

    #[test]
    fn flag_parsing_rejects_bad_number() {
        let r = parse_args_from(["--interval", "abc"].into_iter().map(String::from));
        assert!(r.is_err());
    }

    #[test]
    fn credential_file_is_read_and_trimmed() {
        let dir = std::env::temp_dir().join(format!(
            "alidns-cred-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("alidns-access-key-id"), "  AKID\n").unwrap();
        assert_eq!(
            credential_from(dir.to_str().unwrap(), "alidns-access-key-id").as_deref(),
            Some("AKID")
        );
        assert_eq!(credential_from(dir.to_str().unwrap(), "missing"), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn secret_is_trimmed() {
        let args = Args {
            access_key_id: Some("id".into()),
            access_key_secret: Some("  secret \n".into()),
            domain: Some("a.com".into()),
            config: Some("/nonexistent-alidns-config.json".into()),
            ..Args::default()
        };
        let cfg = from_args(&args).unwrap();
        assert_eq!(cfg.access_key_secret, "secret");
    }

    #[test]
    fn ipv_64_alias_is_rejected() {
        let args = Args {
            access_key_id: Some("id".into()),
            access_key_secret: Some("sk".into()),
            domain: Some("a.com".into()),
            ipv: Some("64".into()),
            config: Some("/nonexistent-alidns-config.json".into()),
            no_state: true,
            ..Args::default()
        };
        assert!(from_args(&args).is_err());
    }

    #[test]
    fn unknown_config_key_fails_fast() {
        let dir =
            std::env::temp_dir().join(format!("alidns-unknown-key-{}.json", std::process::id()));
        fs::write(&dir, r#"{"domain":"a.com","intervall":60}"#).unwrap();
        let args = Args {
            access_key_id: Some("id".into()),
            access_key_secret: Some("sk".into()),
            config: Some(dir.to_str().unwrap().into()),
            ..Args::default()
        };
        assert!(from_args(&args).is_err());
        let _ = fs::remove_file(dir);
    }

    #[test]
    fn old_config_keys_are_rejected_with_hint() {
        let dir = std::env::temp_dir().join(format!("alidns-old-key-{}.json", std::process::id()));
        fs::write(&dir, r#"{"AccessKeyID":"x","domain":"a.com"}"#).unwrap();
        let args = Args {
            config: Some(dir.to_str().unwrap().into()),
            no_state: true,
            ..Args::default()
        };
        let e = match from_args(&args) {
            Ok(_) => panic!("旧键名应被拒绝"),
            Err(e) => e,
        };
        assert!(e.contains("不再支持 AccessKeyID"), "{e}");
        let _ = fs::remove_file(dir);
    }

    #[test]
    fn bool_override_hierarchy() {
        env::set_var("DDNS_TEST_BOOL_OVERRIDE", "false");
        // 环境变量 false 覆盖文件 true
        assert!(!pick_bool(false, "DDNS_TEST_BOOL_OVERRIDE", true).unwrap());
        // CLI 开启优先级最高
        assert!(pick_bool(true, "DDNS_TEST_BOOL_OVERRIDE", false).unwrap());
        env::remove_var("DDNS_TEST_BOOL_OVERRIDE");

        env::set_var("DDNS_TEST_BOOL_INVALID", "maybe");
        assert!(pick_bool(false, "DDNS_TEST_BOOL_INVALID", false).is_err());
        env::remove_var("DDNS_TEST_BOOL_INVALID");
    }

    #[test]
    fn new_env_names_are_used() {
        env::set_var("DDNS_DOMAIN", "env.example.com");
        let args = Args {
            access_key_id: Some("id".into()),
            access_key_secret: Some("sk".into()),
            config: Some("/nonexistent-alidns-config.json".into()),
            no_state: true,
            ..Args::default()
        };
        let cfg = from_args(&args).unwrap();
        assert_eq!(cfg.targets[0].domain, "env.example.com");
        env::remove_var("DDNS_DOMAIN");
    }

    #[test]
    fn prefer_parse() {
        assert_eq!(V6Prefer::parse("stable"), Some(V6Prefer::Stable));
        assert_eq!(V6Prefer::parse("TEMP"), Some(V6Prefer::Temporary));
        assert_eq!(V6Prefer::parse("x"), None);
    }
}
