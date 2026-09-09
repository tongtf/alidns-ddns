use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

macro_rules! map {
    ($($k:expr => $v:expr),* $(,)?) => {{
        let mut m = BTreeMap::new();
        $(m.insert($k.into(), $v.into());)*
        m
    }};
}

#[derive(Debug)]
struct Args {
    access_key_id: Option<String>,
    access_key_secret: Option<String>,
    domain: Option<String>,
    rr: Option<String>,
    ipv: Option<String>,
    interface: Option<String>,
    interval: Option<u64>,
    config: String,
}

fn usage() -> String {
    r#"alidns-ddns — 阿里云域名动态解析工具

用法:
  alidns-ddns [选项]

选项:
  --access-key-id <ID>              AccessKey ID (环境变量 ALIBABA_CLOUD_ACCESS_KEY_ID)
  --access-key-secret <SECRET>      AccessKey Secret (环境变量 ALIBABA_CLOUD_ACCESS_KEY_SECRET)
  --domain <DOMAIN>                 域名，如 example.com (ALIDNS_DOMAIN)
  --rr <RR>                         主机记录，如 @、www (ALIDNS_RR, 默认 @)
  --ipv <MODE>                      IP 模式: 4 / 6 / 46 (DDNS_IPV, 默认 4)
  --interface <NAME>                网卡名，用于获取 IPv6 (ALIDNS_INTERFACE, 自动搜索)
  --interval <SECS>                 更新间隔秒，最小 1 (DDNS_INTERVAL, 默认 300)
  -c, --config <PATH>               配置文件路径 (默认 config.json)
  -h, --help                        显示此帮助

优先级: 命令行参数 > 环境变量 > config.json > 默认值
"#
    .to_string()
}

fn parse_args() -> Args {
    let mut args = Args {
        access_key_id: None,
        access_key_secret: None,
        domain: None,
        rr: None,
        ipv: None,
        interface: None,
        interval: None,
        config: "config.json".into(),
    };
    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "-h" || a == "--help" {
            print!("{}", usage());
            std::process::exit(0);
        } else if let Some(rest) = a.strip_prefix("--") {
            let (k, inline_v) = match rest.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (rest, None),
            };
            let mut v = inline_v.map(|s| s.to_string());
            if v.is_none() {
                v = it.next();
            }
            let v = match v {
                Some(x) => x,
                None => {
                    eprintln!("选项 --{k} 缺少值");
                    std::process::exit(2);
                }
            };
            match k {
                "access-key-id" => args.access_key_id = Some(v),
                "access-key-secret" => args.access_key_secret = Some(v),
                "domain" => args.domain = Some(v),
                "rr" => args.rr = Some(v),
                "ipv" => args.ipv = Some(v),
                "interface" => args.interface = Some(v),
                "interval" => match v.parse::<u64>() {
                    Ok(n) => args.interval = Some(n),
                    Err(_) => {
                        eprintln!("--interval 必须是正整数: {v}");
                        std::process::exit(2);
                    }
                },
                "config" => args.config = v,
                _ => {
                    eprintln!("未知选项: --{k}");
                    std::process::exit(2);
                }
            }
        } else if let Some(rest) = a.strip_prefix('-') {
            match rest {
                "c" => {
                    let v = it.next().unwrap_or_else(|| {
                        eprintln!("选项 -c 缺少值");
                        std::process::exit(2);
                    });
                    args.config = v;
                }
                "h" => {
                    print!("{}", usage());
                    std::process::exit(0);
                }
                _ => {
                    eprintln!("未知选项: -{rest}");
                    std::process::exit(2);
                }
            }
        } else {
            eprintln!("非法参数: {a}");
            std::process::exit(2);
        }
    }
    args
}

#[derive(Deserialize, Default)]
#[allow(non_snake_case)]
struct Config {
    #[serde(default)]
    AccessKeyID: String,
    #[serde(default)]
    AccessKeySecret: String,
    #[serde(default)]
    DomainName: String,
    #[serde(default = "default_rr")]
    RR: String,
    #[serde(default = "default_ipv")]
    IPv: String,
    #[serde(default = "default_interval")]
    Interval: u64,
    #[serde(default)]
    Interface: String,
}

fn default_rr() -> String {
    "@".into()
}
fn default_ipv() -> String {
    "4".into()
}
fn default_interval() -> u64 {
    300
}

/// 三级优先级取值: 命令行参数 > 环境变量(非空) > 配置文件; 末尾兜底默认值
///
/// 把配置解析的契约集中在一点, 新增/修改取值来源时只需改这里, 避免六处各自复制导致分叉。
fn resolve(
    arg: Option<String>,
    var: &str,
    file: String,
    fallback: impl FnOnce() -> String,
) -> String {
    arg.or_else(|| env::var(var).ok().filter(|s| !s.is_empty()))
        .unwrap_or(file)
        .if_empty(fallback)
}

impl Config {
    fn from_args(args: &Args) -> Self {
        // 从config.json加载基础配置
        let file_config: Config = fs::read_to_string(&args.config)
            .ok()
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or_default();

        // 优先级: 命令行参数 > 环境变量 > config.json > 默认值
        Self {
            AccessKeyID: resolve(
                args.access_key_id.clone(),
                "ALIBABA_CLOUD_ACCESS_KEY_ID",
                file_config.AccessKeyID,
                String::new,
            ),
            AccessKeySecret: resolve(
                args.access_key_secret.clone(),
                "ALIBABA_CLOUD_ACCESS_KEY_SECRET",
                file_config.AccessKeySecret,
                String::new,
            ),
            DomainName: resolve(
                args.domain.clone(),
                "ALIDNS_DOMAIN",
                file_config.DomainName,
                String::new,
            ),
            RR: resolve(args.rr.clone(), "ALIDNS_RR", file_config.RR, default_rr),
            IPv: resolve(args.ipv.clone(), "DDNS_IPV", file_config.IPv, default_ipv),
            // Interval 为 u64(需解析 + 下限保护), 不符合字符串取值契约, 保留内联
            Interval: args
                .interval
                .or_else(|| env::var("DDNS_INTERVAL").ok().and_then(|v| v.parse().ok()))
                .unwrap_or(file_config.Interval)
                .max(1),
            Interface: resolve(
                args.interface.clone(),
                "ALIDNS_INTERFACE",
                file_config.Interface,
                String::new,
            ),
        }
    }
}

trait StringExt {
    fn if_empty(self, f: impl FnOnce() -> Self) -> Self;
}

impl StringExt for String {
    fn if_empty(self, f: impl FnOnce() -> Self) -> Self {
        if self.is_empty() {
            f()
        } else {
            self
        }
    }
}

fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let sec_of_day = (secs % 86_400) as i32;
    let hour = sec_of_day / 3600;
    let minute = (sec_of_day % 3600) / 60;
    let second = sec_of_day % 60;
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
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, m, d, hour, minute, second
    )
}

fn make_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!(
        "{:x}-{:x}-{:x}",
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed),
        std::process::id()
    )
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn hex_encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for &b in data {
        s.push(HEX_DIGITS[(b >> 4) as usize] as char);
        s.push(HEX_DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

fn percent_encode(s: &str) -> String {
    // RFC3986: 保留 A-Za-z0-9-_.~ 不编码，其余按 UTF-8 字节编码
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX_DIGITS[(b >> 4) as usize] as char);
                out.push(HEX_DIGITS[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct IpResponse {
    ip: String,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct RecordsResponse {
    DomainRecords: DomainRecords,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct DomainRecords {
    Record: Vec<Record>,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct Record {
    RecordId: String,
    RR: String,
    Value: String,
    #[serde(rename = "Type")]
    Type: String,
}

fn get_ipv4() -> String {
    match ureq::get("https://api.ipify.org?format=json").call() {
        Ok(resp) => resp
            .into_string()
            .ok()
            .and_then(|s| serde_json::from_str::<IpResponse>(&s).ok())
            .map(|r| r.ip)
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

fn get_ipv6(interface: Option<&str>) -> String {
    match local_ip_address::list_afinet_netifas() {
        Ok(netifs) => {
            // 指定网卡时先精确匹配，未命中则告警并回退自动搜索
            if let Some(iface) = interface.filter(|s| !s.is_empty()) {
                let mut found = false;
                let mut matched: Option<String> = None;
                for (name, ip) in &netifs {
                    if name.as_str() != iface {
                        continue;
                    }
                    found = true;
                    if ip.is_ipv6() && !ip.is_loopback() {
                        matched = Some(ip.to_string());
                        break;
                    }
                }
                if let Some(ip) = matched {
                    return ip;
                }
                if found {
                    eprintln!(
                        "⚠️ 警告: 网卡 \"{}\" 未获取到IPv6地址，自动搜索可用网卡。",
                        iface
                    );
                } else {
                    eprintln!("⚠️ 警告: 指定网卡 \"{}\" 不存在，自动搜索可用网卡。", iface);
                }
            }
            netifs
                .into_iter()
                .find_map(|(_, ip)| (ip.is_ipv6() && !ip.is_loopback()).then(|| ip.to_string()))
                .unwrap_or_default()
        }
        Err(e) => {
            eprintln!("获取本地网络地址失败: {}", e);
            String::new()
        }
    }
}

/// 空 body 的 SHA-256：GET 请求无负载，为已知常量，避免每轮重复计算
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn api_call(config: &Config, action: &str, params: BTreeMap<String, String>) -> String {
    let host = "alidns.aliyuncs.com";
    let nonce = make_nonce();
    let ts = rfc3339_now();
    let payload_hash = EMPTY_SHA256;

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    headers.insert("host".into(), host.into());
    headers.insert("x-acs-action".into(), action.into());
    headers.insert("x-acs-version".into(), "2015-01-09".into());
    headers.insert("x-acs-date".into(), ts);
    headers.insert("x-acs-signature-nonce".into(), nonce);
    headers.insert("x-acs-content-sha256".into(), payload_hash.to_string());

    let canonical_query = params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    let mut canonical_headers = String::new();
    let mut signed_headers = Vec::new();
    for (k, v) in &headers {
        let lk = k.to_lowercase();
        canonical_headers.push_str(&format!("{}:{}\n", lk, v));
        signed_headers.push(lk);
    }
    let signed_headers_str = signed_headers.join(";");

    let canonical_request = format!(
        "GET\n/\n{}\n{}\n{}\n{}",
        canonical_query, canonical_headers, signed_headers_str, payload_hash
    );
    let hashed_canonical = hex_encode(&sha256_hex(canonical_request.as_bytes()));
    let string_to_sign = format!("ACS3-HMAC-SHA256\n{}", hashed_canonical);

    let mut mac = match Hmac::<Sha256>::new_from_slice(config.AccessKeySecret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => {
            eprintln!("错误: AccessKeySecret 长度非法（HMAC-SHA256 密钥上限 64 字节）");
            std::process::exit(1);
        }
    };
    mac.update(string_to_sign.as_bytes());
    let sig = mac.finalize().into_bytes();
    let signature = hex_encode(&sig);

    let authorization = format!(
        "ACS3-HMAC-SHA256 Credential={},SignedHeaders={},Signature={}",
        config.AccessKeyID, signed_headers_str, signature
    );
    headers.insert("Authorization".into(), authorization);

    let url = format!("https://{}/?{}", host, canonical_query);
    let mut req = ureq::get(&url);
    for (k, v) in &headers {
        req = req.set(k.as_str(), v.as_str());
    }
    match req.call() {
        Ok(resp) => {
            let body = match resp.into_string() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("读取响应失败: {}", e);
                    String::new()
                }
            };
            body
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            eprintln!("API错误 [{}]: {}", code, &body[..body.len().min(200)]);
            String::new()
        }
        Err(_) => String::new(),
    }
}

fn sha256_hex(data: &[u8]) -> Vec<u8> {
    Sha256::digest(data).to_vec()
}

fn update_dns(config: &Config, ip: &str, record_type: &str) {
    if ip.is_empty() {
        return;
    }

    let resp = api_call(
        config,
        "DescribeDomainRecords",
        map! {
            "DomainName" => &config.DomainName,
            "RRKeyWord" => &config.RR,
        },
    );

    // 检查API错误
    if let Ok(err) = serde_json::from_str::<ApiError>(&resp) {
        if let Some(code) = err.Code {
            eprintln!("查询失败: {} - {}", code, err.Message.unwrap_or_default());
            return;
        }
    }

    match serde_json::from_str::<RecordsResponse>(&resp) {
        Ok(records) => {
            match records
                .DomainRecords
                .Record
                .iter()
                .find(|r| r.RR == config.RR && r.Type == record_type)
            {
                Some(r) if r.Value == ip => {
                    println!("{}记录 {} 无需更新", record_type, ip);
                }
                Some(r) => {
                    println!("更新{}记录: {} -> {}", record_type, r.Value, ip);
                    let resp = api_call(
                        config,
                        "UpdateDomainRecord",
                        map! {
                            "RecordId" => &r.RecordId, "RR" => &config.RR, "Value" => ip, "Type" => record_type,
                        },
                    );
                    check_api_response(&resp, "更新");
                }
                None => {
                    println!("添加{}记录: {}", record_type, ip);
                    let resp = api_call(
                        config,
                        "AddDomainRecord",
                        map! {
                            "DomainName" => &config.DomainName, "RR" => &config.RR, "Value" => ip, "Type" => record_type,
                        },
                    );
                    check_api_response(&resp, "添加");
                }
            }
        }
        Err(e) => {
            eprintln!("解析响应失败: {}", e);
            eprintln!("响应内容: {}", &resp[..resp.len().min(500)]);
        }
    }
}

fn check_api_response(resp: &str, action: &str) {
    if resp.is_empty() {
        eprintln!("{}记录失败: 无响应", action);
        return;
    }
    // 阿里云：成功响应为不含 Code 字段的 JSON 对象，错误响应带 Code+Message。
    // 仅当确认为"已知成功结构"时才报成功，避免把截断/非标准报文谎报为成功。
    match serde_json::from_str::<serde_json::Value>(resp) {
        Ok(serde_json::Value::Object(map)) if map.contains_key("Code") => {
            let code = map.get("Code").and_then(|v| v.as_str()).unwrap_or("");
            let msg = map
                .get("Message")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            eprintln!("{}记录失败: {} - {}", action, code, msg);
        }
        Ok(serde_json::Value::Object(_)) => println!("{}记录成功", action),
        Ok(_) => eprintln!("{}记录: 未识别的响应结构，视为失败以待重试", action),
        Err(_) => eprintln!(
            "{}记录: 响应无法解析（{}），视为失败以待重试",
            action,
            &resp[..resp.len().min(120)]
        ),
    }
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct ApiError {
    Code: Option<String>,
    Message: Option<String>,
}

fn main() {
    let args = parse_args();
    let c = Config::from_args(&args);

    if c.AccessKeyID.is_empty() || c.AccessKeySecret.is_empty() || c.DomainName.is_empty() {
        eprintln!("错误: 请配置 AccessKeyID, AccessKeySecret, DomainName");
        eprintln!("使用 --help 查看帮助");
        std::process::exit(1);
    }

    // IP 模式必须至少包含 4 或 6，否则无意义且会静默空转
    let want_v4 = c.IPv.contains('4');
    let want_v6 = c.IPv.contains('6');
    if !want_v4 && !want_v6 {
        eprintln!("错误: --ipv 模式无效（需包含 4 和/或 6）: {}", c.IPv);
        std::process::exit(1);
    }

    let iface_name = if c.Interface.is_empty() {
        "自动"
    } else {
        c.Interface.as_str()
    };
    println!(
        "域名: {}.{} | IP模式: {} | 间隔: {}s | 网卡: {}",
        c.RR, c.DomainName, c.IPv, c.Interval, iface_name
    );

    loop {
        let (v4, v6) = (
            if want_v4 { get_ipv4() } else { String::new() },
            if want_v6 {
                get_ipv6(Some(c.Interface.as_str()))
            } else {
                String::new()
            },
        );
        println!("IPv4: {} | IPv6: {}", v4, v6);

        if want_v4 {
            update_dns(&c, &v4, "A");
        }
        if want_v6 {
            update_dns(&c, &v6, "AAAA");
        }

        thread::sleep(Duration::from_secs(c.Interval));
    }
}
