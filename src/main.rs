use clap::Parser;
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::thread;
use std::time::Duration;

macro_rules! map {
    ($($k:expr => $v:expr),* $(,)?) => {{
        let mut m = BTreeMap::new();
        $(m.insert($k.into(), $v.into());)*
        m
    }};
}

#[derive(Parser)]
#[command(name = "alidns-ddns", about = "阿里云域名动态解析工具")]
struct Args {
    /// AccessKey ID
    #[arg(long, env = "ALIBABA_CLOUD_ACCESS_KEY_ID")]
    access_key_id: Option<String>,

    /// AccessKey Secret
    #[arg(long, env = "ALIBABA_CLOUD_ACCESS_KEY_SECRET")]
    access_key_secret: Option<String>,

    /// 域名（如 example.com）
    #[arg(long, env = "ALIDNS_DOMAIN")]
    domain: Option<String>,

    /// 主机记录（如 www, @, a）
    #[arg(long, env = "ALIDNS_RR")]
    rr: Option<String>,

    /// IP模式: 4(IPv4), 6(IPv6), 46(双栈)
    #[arg(long, env = "DDNS_IPV")]
    ipv: Option<String>,

    /// 更新间隔（秒）
    #[arg(long, env = "DDNS_INTERVAL")]
    interval: Option<u64>,

    /// 配置文件路径
    #[arg(short, long, default_value = "config.json")]
    config: String,
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

impl Config {
    fn from_args(args: &Args) -> Self {
        // 从config.json加载基础配置
        let file_config: Config = fs::read_to_string(&args.config)
            .ok()
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or_default();

        // 优先级: 命令行参数 > 环境变量 > config.json > 默认值
        Self {
            AccessKeyID: args
                .access_key_id
                .clone()
                .or_else(|| {
                    env::var("ALIBABA_CLOUD_ACCESS_KEY_ID")
                        .ok()
                        .filter(|v| !v.is_empty())
                })
                .unwrap_or(file_config.AccessKeyID),
            AccessKeySecret: args
                .access_key_secret
                .clone()
                .or_else(|| {
                    env::var("ALIBABA_CLOUD_ACCESS_KEY_SECRET")
                        .ok()
                        .filter(|v| !v.is_empty())
                })
                .unwrap_or(file_config.AccessKeySecret),
            DomainName: args
                .domain
                .clone()
                .or_else(|| env::var("ALIDNS_DOMAIN").ok().filter(|v| !v.is_empty()))
                .unwrap_or(file_config.DomainName),
            RR: args
                .rr
                .clone()
                .or_else(|| env::var("ALIDNS_RR").ok().filter(|v| !v.is_empty()))
                .unwrap_or(file_config.RR)
                .if_empty(|| "@".into()),
            IPv: args
                .ipv
                .clone()
                .or_else(|| env::var("DDNS_IPV").ok().filter(|v| !v.is_empty()))
                .unwrap_or(file_config.IPv)
                .if_empty(|| "4".into()),
            Interval: args
                .interval
                .or_else(|| env::var("DDNS_INTERVAL").ok().and_then(|v| v.parse().ok()))
                .unwrap_or(file_config.Interval)
                .max(1),
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
    Client::new()
        .get("https://api.ipify.org?format=json")
        .send()
        .ok()
        .and_then(|r| r.json::<IpResponse>().ok())
        .map(|r| r.ip)
        .unwrap_or_default()
}

fn get_ipv6() -> String {
    local_ip_address::list_afinet_netifas()
        .ok()
        .and_then(|a| {
            a.into_iter()
                .find_map(|(_, ip)| (ip.is_ipv6() && !ip.is_loopback()).then(|| ip.to_string()))
        })
        .unwrap_or_default()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn api_call(config: &Config, action: &str, params: BTreeMap<String, String>) -> String {
    let host = "alidns.aliyuncs.com";
    let nonce = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let payload_hash = sha256_hex(b"");

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    headers.insert("host".into(), host.into());
    headers.insert("x-acs-action".into(), action.into());
    headers.insert("x-acs-version".into(), "2015-01-09".into());
    headers.insert("x-acs-date".into(), ts);
    headers.insert("x-acs-signature-nonce".into(), nonce);
    headers.insert("x-acs-content-sha256".into(), payload_hash.clone());

    let canonical_query = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
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
    let hashed_canonical = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!("ACS3-HMAC-SHA256\n{}", hashed_canonical);

    let mut mac = Hmac::<Sha256>::new_from_slice(config.AccessKeySecret.as_bytes())
        .expect("HMAC key creation failed");
    mac.update(string_to_sign.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    let authorization = format!(
        "ACS3-HMAC-SHA256 Credential={},SignedHeaders={},Signature={}",
        config.AccessKeyID, signed_headers_str, signature
    );
    headers.insert("Authorization".into(), authorization);

    let url = format!("https://{}/?{}", host, canonical_query);
    let req = Client::new().get(&url);
    let req = headers
        .iter()
        .fold(req, |r, (k, v)| r.header(k.as_str(), v.as_str()));
    match req.send() {
        Ok(resp) => {
            let status = resp.status();
            match resp.text() {
                Ok(body) => {
                    if !status.is_success() {
                        eprintln!("API错误 [{}]: {}", status, &body[..body.len().min(200)]);
                    }
                    body
                }
                Err(e) => {
                    eprintln!("读取响应失败: {}", e);
                    String::new()
                }
            }
        }
        Err(e) => {
            eprintln!("请求失败: {}", e);
            String::new()
        }
    }
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct ApiError {
    Code: Option<String>,
    Message: Option<String>,
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
    if let Ok(err) = serde_json::from_str::<ApiError>(resp) {
        if let Some(code) = err.Code {
            eprintln!(
                "{}记录失败: {} - {}",
                action,
                code,
                err.Message.unwrap_or_default()
            );
        } else {
            println!("{}记录成功", action);
        }
    } else {
        println!("{}记录成功", action);
    }
}

fn main() {
    let args = Args::parse();
    let c = Config::from_args(&args);

    if c.AccessKeyID.is_empty() || c.AccessKeySecret.is_empty() || c.DomainName.is_empty() {
        eprintln!("错误: 请配置 AccessKeyID, AccessKeySecret, DomainName");
        eprintln!("使用 --help 查看帮助");
        std::process::exit(1);
    }

    println!(
        "域名: {}.{} | IP模式: {} | 间隔: {}s",
        c.RR, c.DomainName, c.IPv, c.Interval
    );

    loop {
        let (v4, v6) = (
            if c.IPv.contains('4') {
                get_ipv4()
            } else {
                String::new()
            },
            if c.IPv.contains('6') {
                get_ipv6()
            } else {
                String::new()
            },
        );
        println!("IPv4: {} | IPv6: {}", v4, v6);

        if c.IPv.contains('4') {
            update_dns(&c, &v4, "A");
        }
        if c.IPv.contains('6') {
            update_dns(&c, &v6, "AAAA");
        }

        thread::sleep(Duration::from_secs(c.Interval));
    }
}
