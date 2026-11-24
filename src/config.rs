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
use std::path::PathBuf;

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
    pub line: String,
    pub ipv6_prefer: V6Prefer,
    pub allow_ula: bool,
    pub once: bool,
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
    pub once: bool,
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
  --domain <DOMAIN>               域名，多个用逗号分隔 (ALIDNS_DOMAIN)

基础:
  --rr <RR>                       主机记录，多个用逗号分隔 (ALIDNS_RR, 默认 @)
  --ipv <MODE>                    IP 模式: 4 / 6 / 46 (DDNS_IPV, 默认 6)
  --interface <NAME>              取 IPv6 的网卡名 (ALIDNS_INTERFACE, 默认自动)
  --line <LINE>                   解析线路 (DDNS_LINE, 默认 default)
  --ttl <SECS>                    记录 TTL (DDNS_TTL, 默认 600)
  --interval <SECS>               轮询间隔 (DDNS_INTERVAL, 默认 300)
  --verify-interval <SECS>        IP 未变时的 DNS 对账周期 (DDNS_VERIFY_INTERVAL, 默认 21600)

IPv6 策略:
  --ipv6-prefer <MODE>            auto / stable / temporary (ALIDNS_IPV6_PREFER, 默认 auto)
  --allow-ula                     允许把 ULA(fc00::/7) 写入 AAAA（默认拒绝）
  --no-events                     关闭 netlink 地址变更事件订阅（Linux）
  --state-file <PATH>             状态文件路径 (DDNS_STATE_FILE / config.json StateFile)
  --no-state                      禁用状态持久化（每次运行都从零开始）
  --on-change <CMD>               IP 实际变更后执行的命令 (DDNS_ON_CHANGE)
  --on-change-timeout <SECS>      钩子超时 (DDNS_ON_CHANGE_TIMEOUT, 默认 60)

运行模式:
  --once                          只执行一轮即退出（配合 systemd timer）
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
                "once" => a.once = true,
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

#[derive(Deserialize, Default)]
struct FileConfig {
    #[serde(default, rename = "AccessKeyID")]
    access_key_id: String,
    #[serde(default, rename = "AccessKeySecret")]
    access_key_secret: String,
    #[serde(default, rename = "DomainName")]
    domain_name: String,
    #[serde(default, rename = "RR")]
    rr: String,
    #[serde(default, rename = "IPv")]
    ipv: String,
    #[serde(default, rename = "Interface")]
    interface: String,
    #[serde(default, rename = "Line")]
    line: String,
    #[serde(default, rename = "Ipv6Prefer")]
    ipv6_prefer: String,
    #[serde(default, rename = "Interval")]
    interval: Option<u64>,
    #[serde(default, rename = "VerifyInterval")]
    verify_interval: Option<u64>,
    #[serde(default, rename = "Ttl")]
    ttl: Option<u64>,
    #[serde(default, rename = "AllowUla")]
    allow_ula: bool,
    #[serde(default, rename = "StateFile")]
    state_file: String,
    #[serde(default, rename = "OnChange")]
    on_change: String,
    #[serde(default, rename = "OnChangeTimeout")]
    on_change_timeout: Option<u64>,
}

fn env_truthy(var: &str) -> bool {
    matches!(
        env::var(var)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
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

pub fn from_args(args: &Args) -> Result<Config, String> {
    let mut warnings = Vec::new();
    let path = args.config.clone().unwrap_or_else(|| "config.json".into());
    let file = load_file(&path)?;

    if !file.access_key_secret.trim().is_empty() {
        warnings.push(format!(
            "配置文件 {path} 中包含明文 AccessKeySecret；若该文件权限较宽（如 644），\
             建议改用环境变量或 systemd LoadCredential 传递密钥"
        ));
    }
    if args.access_key_secret.is_some() {
        warnings.push("--access-key-secret 会出现在 /proc/<pid>/cmdline，建议改用环境变量".into());
    }

    let access_key_id = pick(
        args.access_key_id.clone(),
        "ALIBABA_CLOUD_ACCESS_KEY_ID",
        file.access_key_id,
    );
    let access_key_secret = pick(
        args.access_key_secret.clone(),
        "ALIBABA_CLOUD_ACCESS_KEY_SECRET",
        file.access_key_secret,
    );
    let domain_raw = pick(args.domain.clone(), "ALIDNS_DOMAIN", file.domain_name);
    let rr_raw = pick(args.rr.clone(), "ALIDNS_RR", file.rr);
    let interface = pick(args.interface.clone(), "ALIDNS_INTERFACE", file.interface)
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
        "ALIDNS_IPV6_PREFER",
        file.ipv6_prefer,
    );

    if access_key_id.trim().is_empty()
        || access_key_secret.trim().is_empty()
        || domain_raw.trim().is_empty()
    {
        return Err("缺少必填项: AccessKeyID / AccessKeySecret / DomainName".into());
    }

    let (want_v4, want_v6) = match ipv.as_str() {
        "" | "6" => (false, true),
        "4" => (true, false),
        "46" | "64" => (true, true),
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

    let state_file = if args.no_state {
        None
    } else {
        let raw = pick(
            args.state_file.clone(),
            "DDNS_STATE_FILE",
            file.state_file.clone(),
        );
        if raw.trim().is_empty() {
            default_state_path()
        } else {
            Some(PathBuf::from(raw.trim()))
        }
    };

    Ok(Config {
        access_key_id: access_key_id.trim().to_string(),
        access_key_secret,
        targets,
        want_v4,
        want_v6,
        interface,
        interval,
        verify_interval,
        ttl,
        line,
        ipv6_prefer,
        allow_ula: args.allow_ula || file.allow_ula || env_truthy("DDNS_ALLOW_ULA"),
        once: args.once,
        dry_run: args.dry_run,
        verbose: args.verbose || env_truthy("DDNS_VERBOSE"),
        events: !args.no_events,
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
            serde_json::from_str(&data).map_err(|e| format!("配置文件 {path} 解析失败: {e}"))
        }
        // 文件不存在是正常情况（纯用 env/CLI 时），不报错
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(FileConfig::default()),
        Err(e) => Err(format!(
            "无法读取配置文件 {path}: {e}（检查路径与权限；服务以 DynamicUser 运行时需可读）"
        )),
    }
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
        if d.contains('/') || d.contains(' ') || d.starts_with('.') || d.ends_with('.') {
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
    fn expand_dedupes() {
        let t = expand_targets("a.com,a.com", "@", "default", false, true).unwrap();
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn flag_parsing_supports_inline_and_space() {
        let a = parse_args_from(
            ["--domain=a.com", "--interval", "60", "--once"]
                .into_iter()
                .map(String::from),
        )
        .unwrap();
        assert_eq!(a.domain.as_deref(), Some("a.com"));
        assert_eq!(a.interval, Some(60));
        assert!(a.once);
    }

    #[test]
    fn flag_parsing_rejects_bad_number() {
        let r = parse_args_from(["--interval", "abc"].into_iter().map(String::from));
        assert!(r.is_err());
    }

    #[test]
    fn prefer_parse() {
        assert_eq!(V6Prefer::parse("stable"), Some(V6Prefer::Stable));
        assert_eq!(V6Prefer::parse("TEMP"), Some(V6Prefer::Temporary));
        assert_eq!(V6Prefer::parse("x"), None);
    }
}
