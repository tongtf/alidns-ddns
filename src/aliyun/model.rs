//! 阿里云 DNS 响应模型。

use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct ListResp {
    #[serde(rename = "TotalCount", default)]
    pub total: u64,
    #[serde(rename = "PageNumber", default)]
    pub page: u64,
    #[serde(rename = "RequestId", default)]
    pub request_id: String,
    #[serde(rename = "DomainRecords", default)]
    pub records: RecordSet,
}

#[derive(Deserialize, Debug, Default)]
pub struct RecordSet {
    #[serde(rename = "Record", default)]
    pub record: Vec<Record>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Record {
    #[serde(rename = "RecordId", default)]
    pub id: String,
    #[serde(rename = "RR", default)]
    pub rr: String,
    #[serde(rename = "Type", default)]
    pub rtype: String,
    #[serde(rename = "Value", default)]
    pub value: String,
    /// 解析线路。旧记录或非分线路版本可能为空，按 `default` 处理。
    #[serde(rename = "Line", default)]
    pub line: String,
    #[serde(rename = "Status", default)]
    pub status: String,
    #[serde(rename = "Locked", default)]
    pub locked: bool,
    #[serde(rename = "TTL", default)]
    pub ttl: u64,
}

impl Record {
    pub fn line_or_default(&self) -> &str {
        if self.line.is_empty() {
            "default"
        } else {
            &self.line
        }
    }

    /// 阿里云把停用记录标成 `Disable` / `DISABLE`。
    pub fn is_disabled(&self) -> bool {
        self.status.eq_ignore_ascii_case("disable")
    }
}

/// 错误响应：`{"Code":"...","Message":"...","RequestId":"..."}`
#[derive(Deserialize, Debug)]
pub struct ErrorResp {
    #[serde(rename = "Code", default)]
    pub code: String,
    #[serde(rename = "Message", default)]
    pub message: String,
    #[serde(rename = "RequestId", default)]
    pub request_id: String,
}

/// 成功响应里只关心是否有错误码。
#[derive(Deserialize, Debug)]
pub struct Ack {
    #[serde(rename = "Code", default)]
    pub code: Option<String>,
    #[serde(rename = "Message", default)]
    pub message: Option<String>,
    #[serde(rename = "RequestId", default)]
    pub request_id: Option<String>,
}
