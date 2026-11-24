//! 阿里云 ACS3-HMAC-SHA256（V3 签名），纯函数，便于单测。
//!
//! 要点：
//! - 签名 Key 就是 `AccessKeySecret` 本身（不追加 `&`）
//! - 规范化查询串与头部的百分号编码必须用**大写**十六进制（如 `%3A`）
//! - 哈希/签名字节用小写十六进制

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const HOST: &str = "alidns.aliyuncs.com";
const API_VERSION: &str = "2015-01-09";
/// 空 body 的 SHA-256：GET 请求无负载，为已知常量
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const PERCENT_HEX: &[u8; 16] = b"0123456789ABCDEF";
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

pub struct Signed {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// RFC3986：保留 `A-Za-z0-9-_.~`，其余按 UTF-8 字节大写编码。
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(PERCENT_HEX[(b >> 4) as usize] as char);
                out.push(PERCENT_HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

fn hex_lower(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for &b in data {
        s.push(HEX_DIGITS[(b >> 4) as usize] as char);
        s.push(HEX_DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

/// BTreeMap 保证按 key 升序，符合规范化要求。
pub fn canonical_query(params: &BTreeMap<String, String>) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn sign(
    action: &str,
    params: &BTreeMap<String, String>,
    access_key_id: &str,
    access_key_secret: &str,
    timestamp: &str,
    nonce: &str,
) -> Signed {
    let canonical_query = canonical_query(params);

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    headers.insert("host".into(), HOST.into());
    headers.insert("x-acs-action".into(), action.into());
    headers.insert("x-acs-version".into(), API_VERSION.into());
    headers.insert("x-acs-date".into(), timestamp.into());
    headers.insert("x-acs-signature-nonce".into(), nonce.into());
    headers.insert("x-acs-content-sha256".into(), EMPTY_SHA256.into());

    let mut canonical_headers = String::new();
    let mut signed_headers = Vec::new();
    for (k, v) in &headers {
        canonical_headers.push_str(&format!("{k}:{v}\n"));
        signed_headers.push(k.clone());
    }
    let signed_headers = signed_headers.join(";");

    let canonical_request =
        format!("GET\n/\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{EMPTY_SHA256}");

    let hashed_canonical = hex_lower(&Sha256::digest(canonical_request.as_bytes()));
    let string_to_sign = format!("ACS3-HMAC-SHA256\n{hashed_canonical}");

    // hmac 0.12 对任意长度密钥都返回 Ok（超长会先做一次 hash），无需处理 Err
    let mut mac = Hmac::<Sha256>::new_from_slice(access_key_secret.as_bytes())
        .expect("HMAC-SHA256 接受任意长度密钥");
    mac.update(string_to_sign.as_bytes());
    let signature = hex_lower(&mac.finalize().into_bytes());

    headers.insert(
        "Authorization".into(),
        format!(
            "ACS3-HMAC-SHA256 Credential={access_key_id},SignedHeaders={signed_headers},Signature={signature}"
        ),
    );

    Signed {
        url: format!("https://{HOST}/?{canonical_query}"),
        headers: headers.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("DomainName".into(), "example.com".into());
        m.insert("RRKeyWord".into(), "@".into());
        m
    }

    #[test]
    fn percent_encoding_uses_uppercase_hex() {
        assert_eq!(percent_encode("a b/c~d"), "a%20b%2Fc~d");
        assert_eq!(percent_encode("@"), "%40");
        assert_eq!(percent_encode("中"), "%E4%B8%AD");
    }

    #[test]
    fn canonical_query_is_sorted_and_encoded() {
        let q = canonical_query(&params());
        assert_eq!(q, "DomainName=example.com&RRKeyWord=%40");
    }

    #[test]
    fn signature_is_deterministic_and_well_formed() {
        let p = params();
        let a = sign(
            "DescribeDomainRecords",
            &p,
            "AKID",
            "SECRET",
            "2026-01-01T00:00:00Z",
            "n1",
        );
        let b = sign(
            "DescribeDomainRecords",
            &p,
            "AKID",
            "SECRET",
            "2026-01-01T00:00:00Z",
            "n1",
        );
        assert_eq!(a.url, b.url);
        assert_eq!(a.headers, b.headers);

        let auth = a
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(auth.starts_with("ACS3-HMAC-SHA256 Credential=AKID,SignedHeaders="));
        let sig = auth.rsplit("Signature=").next().unwrap();
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn signed_headers_are_sorted() {
        let s = sign(
            "A",
            &params(),
            "AKID",
            "SECRET",
            "2026-01-01T00:00:00Z",
            "n",
        );
        let auth = s
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .map(|(_, v)| v.clone())
            .unwrap();
        let signed = auth.split("SignedHeaders=").nth(1).unwrap();
        let signed = signed.split(',').next().unwrap();
        let mut sorted: Vec<&str> = signed.split(';').collect();
        let original = sorted.clone();
        sorted.sort();
        assert_eq!(original, sorted);
    }
}
