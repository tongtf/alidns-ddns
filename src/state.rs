//! 运行期状态：最近一次同步值、失败退避、告警去重。
//!
//! 只用 `Instant`（单调钟）计时，避免系统时间被调整后出现"永久不刷新"或"疯狂刷新"。

use crate::config::Target;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

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
}

impl State {
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
    }

    pub fn note_change(&mut self) {
        self.last_change = Some(Instant::now());
    }

    pub fn recently_changed(&self, window: Duration) -> bool {
        self.last_change
            .map(|t| t.elapsed() < window)
            .unwrap_or(false)
    }

    pub fn note_cycle_ok(&mut self) {
        self.fails = 0;
    }

    pub fn note_cycle_fail(&mut self) {
        self.fails = self.fails.saturating_add(1);
    }

    /// 连续失败时的退避下限：30s、60s、120s、240s、300s（封顶）。
    pub fn backoff(&self) -> Option<Duration> {
        if self.fails == 0 {
            return None;
        }
        let shift = self.fails.min(5) - 1;
        Some(Duration::from_secs(30u64 << shift).min(Duration::from_secs(300)))
    }

    /// 同一告警只打印一次（进程生命周期内）。返回 true 表示这是首次出现。
    pub fn warn_once(&mut self, key: impl Into<String>) -> bool {
        self.warned.insert(key.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RType;

    fn target() -> Target {
        Target {
            domain: "a.com".into(),
            rr: "@".into(),
            line: "default".into(),
            rtype: RType::Aaaa,
        }
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
        // verify 周期为 0 时应总是对账
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
}
