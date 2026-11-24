//! 运行期状态：最近一次同步值、失败退避、告警去重、IPv4 端点粘性。
//!
//! 计时用 `Instant`（单调钟），持久化时用 Unix 秒换算——单调钟的零点每次启动都不同，
//! 不能跨进程直接保存。时钟异常（回拨/跳变）时把旧记录视为"刚校验过"或"已过期"，
//! 宁可多查一次 DNS，也不会永久跳过对账。

use crate::config::{RType, Target};
use crate::log;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: u32 = 1;
/// 告警去重状态保留时长；超过后重新允许打印（避免一条过期告警永久静默）。
const WARN_TTL_SECS: i64 = 86_400;

#[derive(Clone)]
struct Entry {
    value: String,
    checked_at: Instant,
}

#[derive(Default)]
pub struct State {
    entries: HashMap<Target, Entry>,
    fails: u32,
    last_change: Option<Instant>,
    warned: HashSet<String>,
    warned_at: HashMap<String, i64>,
    ipv4_endpoint: Option<String>,
    path: Option<PathBuf>,
    dirty: bool,
}

#[derive(Serialize, Deserialize)]
struct PersistedEntry {
    domain: String,
    rr: String,
    line: String,
    rtype: String,
    value: String,
    checked_at_unix: i64,
}

#[derive(Serialize, Deserialize)]
struct PersistedWarn {
    key: String,
    at_unix: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Persisted {
    version: u32,
    entries: Vec<PersistedEntry>,
    fails: u32,
    last_change_unix: Option<i64>,
    warned: Vec<PersistedWarn>,
    ipv4_endpoint: Option<String>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            version: VERSION,
            entries: Vec::new(),
            fails: 0,
            last_change_unix: None,
            warned: Vec::new(),
            ipv4_endpoint: None,
        }
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把 Unix 秒还原成 `Instant`。已过去的时刻用 `Instant::now() - 间隔`；
/// 未来时刻（系统时钟回拨）按"刚刚"处理。
fn unix_to_instant(ts: i64) -> Instant {
    let elapsed = (now_unix() - ts).clamp(0, 31_536_000) as u64;
    Instant::now()
        .checked_sub(Duration::from_secs(elapsed))
        .unwrap_or_else(Instant::now)
}

fn instant_to_unix(t: Instant) -> i64 {
    let ahead = t.saturating_duration_since(Instant::now()).as_secs() as i64;
    now_unix() + ahead
}

impl State {
    /// 从文件恢复状态。文件不存在、损坏、版本不符时静默从零开始（不阻断启动）。
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut s = Self {
            path: path.clone(),
            ..Self::default()
        };
        let Some(path) = path else { return s };

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }

        let data = match fs::read_to_string(&path) {
            Ok(d) => d,
            Err(_) => return s,
        };
        let p: Persisted = match serde_json::from_str::<Persisted>(&data) {
            Ok(p) if p.version == VERSION => p,
            Ok(p) => {
                log::warn(format!(
                    "状态文件版本 {} 不匹配（期望 {VERSION}），忽略旧状态",
                    p.version
                ));
                return s;
            }
            Err(e) => {
                log::warn(format!("状态文件 {} 无法解析，忽略: {e}", path.display()));
                return s;
            }
        };

        for e in p.entries {
            let Some(rtype) = RType::parse(&e.rtype) else {
                continue;
            };
            let target = Target {
                domain: e.domain,
                rr: e.rr,
                line: e.line,
                rtype,
            };
            s.entries.insert(
                target,
                Entry {
                    value: e.value,
                    checked_at: unix_to_instant(e.checked_at_unix),
                },
            );
        }
        s.fails = p.fails;
        s.last_change = p.last_change_unix.map(unix_to_instant);
        s.ipv4_endpoint = p.ipv4_endpoint;

        let now = now_unix();
        for w in p.warned {
            if now - w.at_unix < WARN_TTL_SECS {
                s.warned.insert(w.key.clone());
                s.warned_at.insert(w.key, w.at_unix);
            }
        }
        log::debug(format!(
            "已从 {} 恢复状态：{} 个目标，{} 次连续失败",
            path.display(),
            s.entries.len(),
            s.fails
        ));
        s
    }

    /// 有变更才落盘；原子替换避免定时器与常驻进程交替运行时读到半截 JSON。
    pub fn save_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = &self.path else { return };

        let mut entries = Vec::with_capacity(self.entries.len());
        for (target, e) in &self.entries {
            entries.push(PersistedEntry {
                domain: target.domain.clone(),
                rr: target.rr.clone(),
                line: target.line.clone(),
                rtype: target.rtype.as_str().to_string(),
                value: e.value.clone(),
                checked_at_unix: instant_to_unix(e.checked_at),
            });
        }
        let p = Persisted {
            version: VERSION,
            entries,
            fails: self.fails,
            last_change_unix: self.last_change.map(instant_to_unix),
            warned: self
                .warned_at
                .iter()
                .map(|(key, at)| PersistedWarn {
                    key: key.clone(),
                    at_unix: *at,
                })
                .collect(),
            ipv4_endpoint: self.ipv4_endpoint.clone(),
        };

        let data = match serde_json::to_string_pretty(&p) {
            Ok(d) => d,
            Err(e) => {
                log::warn(format!("序列化状态失败: {e}"));
                return;
            }
        };
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        if let Err(e) = fs::write(&tmp, data) {
            log::warn(format!("写状态文件 {} 失败: {e}", tmp.display()));
            return;
        }
        if let Err(e) = fs::rename(&tmp, path) {
            log::warn(format!("替换状态文件 {} 失败: {e}", path.display()));
            return;
        }
        self.dirty = false;
    }

    /// 是否需要向 DNS 查询实际值。
    ///
    /// 期望值未变且距上次校验未超 `verify` 时跳过——稳态下几乎不产生 API 调用；
    /// 超过校验周期则做一次对账，纠正被人工改动过的记录。
    pub fn needs_lookup(&self, target: &Target, desired: &str, verify: Duration) -> bool {
        match self.entries.get(target) {
            None => true,
            Some(e) => e.value != desired || e.checked_at.elapsed() >= verify,
        }
    }

    /// 记录"已确认该目标与期望一致"。
    pub fn note_synced(&mut self, target: &Target, value: &str) {
        self.entries.insert(
            target.clone(),
            Entry {
                value: value.to_string(),
                checked_at: Instant::now(),
            },
        );
        self.dirty = true;
    }

    pub fn note_change(&mut self) {
        self.last_change = Some(Instant::now());
        self.dirty = true;
    }

    pub fn recently_changed(&self, window: Duration) -> bool {
        self.last_change
            .map(|t| t.elapsed() < window)
            .unwrap_or(false)
    }

    pub fn note_cycle_ok(&mut self) {
        if self.fails != 0 {
            self.fails = 0;
            self.dirty = true;
        }
    }

    pub fn note_cycle_fail(&mut self) {
        self.fails = self.fails.saturating_add(1);
        self.dirty = true;
    }

    /// 连续失败时的退避下限：30s、60s、120s、240s、300s（封顶）。
    pub fn backoff(&self) -> Option<Duration> {
        if self.fails == 0 {
            return None;
        }
        let shift = self.fails.min(5) - 1;
        Some(Duration::from_secs(30u64 << shift).min(Duration::from_secs(300)))
    }

    /// 同一告警只打印一次（跨进程重启，24h 后重新允许）。返回 true 表示应打印。
    pub fn warn_once(&mut self, key: impl Into<String>) -> bool {
        let key = key.into();
        if self.warned.insert(key.clone()) {
            self.warned_at.insert(key, now_unix());
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// 上次成功的 IPv4 探测端点（跨进程保留端点粘性）。
    pub fn ipv4_endpoint(&self) -> Option<&str> {
        self.ipv4_endpoint.as_deref()
    }

    pub fn set_ipv4_endpoint(&mut self, endpoint: Option<&str>) {
        if self.ipv4_endpoint.as_deref() != endpoint {
            self.ipv4_endpoint = endpoint.map(str::to_string);
            self.dirty = true;
        }
    }

    /// 仅供测试：状态文件路径。
    #[cfg(test)]
    pub fn path(&self) -> Option<&std::path::Path> {
        self.path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn target() -> Target {
        Target {
            domain: "a.com".into(),
            rr: "@".into(),
            line: "default".into(),
            rtype: RType::Aaaa,
        }
    }

    fn temp_path() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "alidns-ddns-state-test-{}-{n}.json",
            std::process::id()
        ))
    }

    #[test]
    fn lookup_until_synced() {
        let t = target();
        let mut s = State::default();
        assert!(s.needs_lookup(&t, "::1", Duration::from_secs(3600)));
        s.note_synced(&t, "::1");
        assert!(!s.needs_lookup(&t, "::1", Duration::from_secs(3600)));
        assert!(s.needs_lookup(&t, "::2", Duration::from_secs(3600)));
    }

    #[test]
    fn verify_interval_expires() {
        let t = target();
        let mut s = State::default();
        s.note_synced(&t, "::1");
        assert!(!s.needs_lookup(&t, "::1", Duration::from_secs(3600)));
        assert!(s.needs_lookup(&t, "::1", Duration::ZERO));
    }

    #[test]
    fn backoff_grows_and_caps() {
        let mut s = State::default();
        assert_eq!(s.backoff(), None);
        s.note_cycle_fail();
        assert_eq!(s.backoff(), Some(Duration::from_secs(30)));
        s.note_cycle_fail();
        assert_eq!(s.backoff(), Some(Duration::from_secs(60)));
        for _ in 0..10 {
            s.note_cycle_fail();
        }
        assert_eq!(s.backoff(), Some(Duration::from_secs(300)));
        s.note_cycle_ok();
        assert_eq!(s.backoff(), None);
    }

    #[test]
    fn roundtrip_preserves_cache_backoff_and_sticky_endpoint() {
        let path = temp_path();
        let t = target();
        let mut s = State::load(Some(path.clone()));
        s.note_synced(&t, "2408::1");
        s.note_cycle_fail();
        s.note_change();
        s.set_ipv4_endpoint(Some("https://v4.ident.me"));
        s.warn_once("v6-none");
        s.save_if_dirty();

        let mut loaded = State::load(Some(path.clone()));
        assert!(!loaded.needs_lookup(&t, "2408::1", Duration::from_secs(3600)));
        assert!(loaded.needs_lookup(&t, "2408::2", Duration::from_secs(3600)));
        assert_eq!(loaded.backoff(), Some(Duration::from_secs(30)));
        assert!(loaded.recently_changed(Duration::from_secs(60)));
        assert_eq!(loaded.ipv4_endpoint(), Some("https://v4.ident.me"));
        assert!(!loaded.warn_once("v6-none"), "告警去重应跨进程保留");
        assert!(loaded.warn_once("v4-probe-failed"));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(path.with_extension("tmp"));
    }

    #[test]
    fn missing_or_corrupt_file_is_ignored() {
        let path = temp_path();
        let s = State::load(Some(path.clone()));
        assert_eq!(s.backoff(), None);
        assert!(s.path().is_some());

        fs::write(&path, "not json").unwrap();
        let s = State::load(Some(path.clone()));
        assert_eq!(s.backoff(), None);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn stale_warnings_expire() {
        let path = temp_path();
        let mut s = State::load(Some(path.clone()));
        s.warn_once("old-warning");
        s.warned_at
            .insert("old-warning".into(), now_unix() - WARN_TTL_SECS - 1);
        s.save_if_dirty();

        let mut loaded = State::load(Some(path.clone()));
        assert!(loaded.warn_once("old-warning"), "过期告警应重新允许打印");

        let _ = fs::remove_file(path);
    }
}
