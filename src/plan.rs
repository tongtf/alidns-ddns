//! 对账计划（纯函数）：期望值 × 实际记录 → 待执行动作。
//!
//! 这一层不碰网络、不碰文件，因此可以完整单测：给定"本机有什么地址"和"DNS 里有什么记录"，
//! 断言"应该发什么请求"。

use crate::aliyun::model::Record;
use crate::config::{RType, Target};
use std::net::IpAddr;

/// 本轮观察到、准备发布的地址。
#[derive(Clone, Debug, Default)]
pub struct Desired {
    pub v4: Option<IpAddr>,
    pub v6: Option<IpAddr>,
    /// IPv6 preferred lifetime 剩余秒数；`None` 表示永久或未知。
    pub v6_lft: Option<u64>,
}

impl Desired {
    pub fn value_for(&self, t: &Target) -> Option<IpAddr> {
        match t.rtype {
            RType::A => self.v4,
            RType::Aaaa => self.v6,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// 已有记录，值与期望不同。
    Update { id: String, value: IpAddr },
    /// 没有记录。
    Add { value: IpAddr },
    /// 已一致。
    InSync { value: IpAddr },
    /// 不做写入（记录被锁定等）。
    Skip { value: IpAddr },
}

#[derive(Clone, Debug)]
pub struct Step {
    pub target: Target,
    pub kind: StepKind,
    /// 需要打印的告警（重复记录、记录被停用等）。
    pub note: Option<String>,
}

fn same_value(a: &str, b: IpAddr) -> bool {
    match a.trim().parse::<IpAddr>() {
        Ok(x) => x == b,
        // 解析不了就退化成去空白后的字符串比较
        Err(_) => a.trim().eq_ignore_ascii_case(&b.to_string()),
    }
}

/// 为一批目标生成计划。
///
/// `records` 是该域名下的全量记录（已翻页），匹配条件严格为 `RR + Line + Type`：
/// 只比 `RR + Type` 会在多线路场景下命中错误的一条。
pub fn diff(targets: &[&Target], desired: &Desired, records: &[Record], ttl: u64) -> Vec<Step> {
    let mut out = Vec::new();

    for t in targets {
        let Some(value) = desired.value_for(t) else {
            continue;
        };

        let matched: Vec<&Record> = records
            .iter()
            .filter(|r| {
                // DNS 的 RR 不区分大小写，别因为阿里云返回 "WWW" 而误判为缺失
                r.rr.eq_ignore_ascii_case(&t.rr)
                    && r.rtype.eq_ignore_ascii_case(t.rtype.as_str())
                    && r.line_or_default().eq_ignore_ascii_case(&t.line)
            })
            .collect();

        let Some(first) = matched.first() else {
            out.push(Step {
                target: (*t).clone(),
                kind: StepKind::Add { value },
                note: None,
            });
            continue;
        };

        let mut note = None;
        if matched.len() > 1 {
            note = Some(format!(
                "{} 存在 {} 条相同 RR/线路/类型的记录，只更新第一条（RecordId={}），建议手工清理",
                t,
                matched.len(),
                first.id
            ));
        }
        if first.is_disabled() {
            let msg = format!(
                "{} 的记录处于停用状态（Status={}），解析不会生效；更新值后请手动启用",
                t, first.status
            );
            note = Some(match note.take() {
                Some(n) => format!("{n}; {msg}"),
                None => msg,
            });
        }
        if first.locked {
            out.push(Step {
                target: (*t).clone(),
                kind: StepKind::Skip { value },
                note: Some(format!("{} 的记录被锁定（Locked=true），跳过", t)),
            });
            continue;
        }

        // 显式传 TTL 是必要的：不传时阿里云会把 TTL 重置为默认值。
        // 只在真的要写入时提示差异，避免稳态下刷日志。
        if !same_value(&first.value, value) && first.ttl != 0 && first.ttl != ttl {
            let msg = format!("{} 的 TTL 将由 {} 改为 {}", t, first.ttl, ttl);
            note = Some(match note.take() {
                Some(n) => format!("{n}; {msg}"),
                None => msg,
            });
        }

        let kind = if same_value(&first.value, value) {
            StepKind::InSync { value }
        } else {
            StepKind::Update {
                id: first.id.clone(),
                value,
            }
        };
        out.push(Step {
            target: (*t).clone(),
            kind,
            note,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(rr: &str, line: &str, rtype: RType) -> Target {
        Target {
            domain: "a.com".into(),
            rr: rr.into(),
            line: line.into(),
            rtype,
        }
    }

    fn record(rr: &str, line: &str, rtype: &str, value: &str) -> Record {
        Record {
            id: format!("id-{rr}-{line}"),
            rr: rr.into(),
            rtype: rtype.into(),
            value: value.into(),
            line: line.into(),
            status: "ENABLE".into(),
            locked: false,
            ttl: 600,
        }
    }

    fn desired(v4: Option<&str>, v6: Option<&str>) -> Desired {
        Desired {
            v4: v4.map(|s| s.parse().unwrap()),
            v6: v6.map(|s| s.parse().unwrap()),
            v6_lft: None,
        }
    }

    #[test]
    fn creates_when_missing() {
        let t = target("@", "default", RType::A);
        let steps = diff(&[&t], &desired(Some("1.2.3.4"), None), &[], 600);
        assert!(matches!(steps[0].kind, StepKind::Add { .. }));
    }

    #[test]
    fn in_sync_when_equal() {
        let t = target("@", "default", RType::A);
        let r = record("@", "default", "A", "1.2.3.4");
        let steps = diff(&[&t], &desired(Some("1.2.3.4"), None), &[r], 600);
        assert!(matches!(steps[0].kind, StepKind::InSync { .. }));
    }

    #[test]
    fn updates_when_value_differs() {
        let t = target("@", "default", RType::Aaaa);
        let r = record("@", "default", "AAAA", "2408::1");
        let steps = diff(&[&t], &desired(None, Some("2408::2")), &[r], 600);
        match &steps[0].kind {
            StepKind::Update { id, value } => {
                assert_eq!(id, "id-@-default");
                assert_eq!(value.to_string(), "2408::2");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn line_is_respected() {
        let t = target("www", "telecom", RType::A);
        let others = record("www", "default", "A", "1.1.1.1");
        let steps = diff(&[&t], &desired(Some("2.2.2.2"), None), &[others], 600);
        assert!(
            matches!(steps[0].kind, StepKind::Add { .. }),
            "线路不同应视为缺失"
        );
    }

    #[test]
    fn type_is_respected() {
        let t = target("@", "default", RType::Aaaa);
        let a = record("@", "default", "A", "1.2.3.4");
        let steps = diff(&[&t], &desired(None, Some("2408::1")), &[a], 600);
        assert!(matches!(steps[0].kind, StepKind::Add { .. }));
    }

    #[test]
    fn duplicates_produce_note_but_still_update() {
        let t = target("@", "default", RType::A);
        let rs = vec![
            record("@", "default", "A", "1.1.1.1"),
            record("@", "default", "A", "1.1.1.1"),
        ];
        let steps = diff(&[&t], &desired(Some("9.9.9.9"), None), &rs, 600);
        assert!(steps[0].note.is_some());
        assert!(matches!(steps[0].kind, StepKind::Update { .. }));
    }

    #[test]
    fn locked_record_is_skipped() {
        let t = target("@", "default", RType::A);
        let mut r = record("@", "default", "A", "1.1.1.1");
        r.locked = true;
        let steps = diff(&[&t], &desired(Some("9.9.9.9"), None), &[r], 600);
        assert!(matches!(steps[0].kind, StepKind::Skip { .. }));
    }

    #[test]
    fn disabled_record_warns() {
        let t = target("@", "default", RType::A);
        let mut r = record("@", "default", "A", "1.1.1.1");
        r.status = "Disable".into();
        let steps = diff(&[&t], &desired(Some("9.9.9.9"), None), &[r], 600);
        assert!(steps[0].note.as_deref().unwrap().contains("停用"));
        assert!(matches!(steps[0].kind, StepKind::Update { .. }));
    }

    #[test]
    fn missing_desired_is_ignored() {
        let t = target("@", "default", RType::Aaaa);
        assert!(diff(&[&t], &desired(Some("1.2.3.4"), None), &[], 600).is_empty());
    }

    #[test]
    fn ttl_change_is_reported_on_update() {
        let t = target("@", "default", RType::A);
        let r = record("@", "default", "A", "1.1.1.1");
        let steps = diff(&[&t], &desired(Some("9.9.9.9"), None), &[r], 300);
        assert!(steps[0].note.as_deref().unwrap().contains("TTL"));
    }

    #[test]
    fn value_comparison_is_normalized() {
        // 阿里云返回的压缩形式与期望值必须等价
        let t = target("@", "default", RType::Aaaa);
        let r = record("@", "default", "AAAA", "2408:0:0:0:0:0:0:1");
        let steps = diff(&[&t], &desired(None, Some("2408::1")), &[r], 600);
        assert!(matches!(steps[0].kind, StepKind::InSync { .. }));
    }

    #[test]
    fn rr_is_case_insensitive() {
        let t = target("WWW", "default", RType::A);
        let r = record("www", "default", "A", "1.1.1.1");
        let steps = diff(&[&t], &desired(Some("1.1.1.1"), None), &[r], 600);
        assert!(matches!(steps[0].kind, StepKind::InSync { .. }));
    }

    #[test]
    fn empty_line_in_record_matches_default() {
        let t = target("@", "default", RType::A);
        let mut r = record("@", "default", "A", "1.1.1.1");
        r.line.clear();
        let steps = diff(&[&t], &desired(Some("1.1.1.1"), None), &[r], 600);
        assert!(matches!(steps[0].kind, StepKind::InSync { .. }));
    }
}
