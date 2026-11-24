//! IP 真正变更后执行外部命令（邮件场景可用来 reload postfix/dovecot，或通知 acme.sh）。
//!
//! - 只在 A/AAAA 发生实际写入（Update/Add）后触发；`InSync`/`Skip`/`--dry-run` 不触发
//! - 同步执行但带超时，避免钩子挂死把主循环卡住（systemd watchdog 默认 300s）
//! - 钩子失败只告警，不回滚 DNS、不改变本轮退出码——DNS 已经写成功了
//! - 通过环境变量传递本次变更信息，避免把 IP 拼进 shell 命令造成注入

use crate::config::Target;
use crate::log;
use crate::plan::Desired;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub struct Hook<'a> {
    pub command: &'a str,
    pub timeout: Duration,
    pub changed: &'a [Target],
    pub desired: &'a Desired,
    pub dry_run: bool,
}

/// 返回 `Err` 表示钩子失败（非 0 退出、超时、无法启动）。
pub fn run(h: &Hook<'_>) -> Result<(), String> {
    if h.changed.is_empty() {
        return Ok(());
    }
    if h.dry_run {
        log::info(format!(
            "[dry-run] 将执行 on-change 钩子（{} 个变更）: {}",
            h.changed.len(),
            h.command
        ));
        return Ok(());
    }

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C");
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c");
        c
    };
    cmd.arg(h.command)
        .env("DDNS_CHANGED", "1")
        .env("DDNS_CHANGED_COUNT", h.changed.len().to_string())
        .env(
            "DDNS_CHANGED_TARGETS",
            h.changed.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(","),
        )
        .env("DDNS_IPV4", h.desired.v4.map(|v| v.to_string()).unwrap_or_default())
        .env("DDNS_IPV6", h.desired.v6.map(|v| v.to_string()).unwrap_or_default())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 on-change 钩子: {e}"))?;

    let deadline = Instant::now() + h.timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("on-change 钩子超时（{}s）", h.timeout.as_secs()));
            }
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("查询 on-change 钩子状态失败: {e}")),
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("读取 on-change 钩子输出失败: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.trim().is_empty() {
        log::debug(format!("on-change 钩子 stdout: {}", stdout.trim()));
    }
    if !stderr.trim().is_empty() {
        log::debug(format!("on-change 钩子 stderr: {}", stderr.trim()));
    }

    if output.status.success() {
        log::info(format!(
            "on-change 钩子执行成功（{} 个变更）",
            h.changed.len()
        ));
        Ok(())
    } else {
        Err(format!(
            "on-change 钩子退出码 {:?}: {}",
            output.status.code(),
            stderr.trim()
        ))
    }
}

// 等待子进程并读取输出的实现需要在 Unix 上验证（CI/本机为 Linux）。
#[cfg(all(test, unix))]
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

    fn desired() -> Desired {
        Desired {
            v6: Some("2408::1".parse().unwrap()),
            ..Desired::default()
        }
    }

    #[test]
    fn passes_change_env_to_shell() {
        let d = desired();
        let h = Hook {
            command: "test \"$DDNS_CHANGED\" = 1 && test \"$DDNS_IPV6\" = '2408::1' && test -n \"$DDNS_CHANGED_TARGETS\"",
            timeout: Duration::from_secs(5),
            changed: &[target()],
            desired: &d,
            dry_run: false,
        };
        run(&h).unwrap();
    }

    #[test]
    fn reports_nonzero_exit() {
        let d = desired();
        let h = Hook {
            command: "exit 7",
            timeout: Duration::from_secs(5),
            changed: &[target()],
            desired: &d,
            dry_run: false,
        };
        let e = run(&h).err().unwrap();
        assert!(e.contains("7"), "{e}");
    }

    #[test]
    fn kills_hanging_hook() {
        let d = desired();
        let h = Hook {
            command: "sleep 30",
            timeout: Duration::from_millis(150),
            changed: &[target()],
            desired: &d,
            dry_run: false,
        };
        let e = run(&h).err().unwrap();
        assert!(e.contains("超时"), "{e}");
    }

    #[test]
    fn dry_run_never_executes() {
        let d = desired();
        let mut h = Hook {
            command: "exit 9",
            timeout: Duration::from_secs(5),
            changed: &[target()],
            desired: &d,
            dry_run: false,
        };
        h.dry_run = true;
        run(&h).unwrap();
    }

    #[test]
    fn no_change_is_noop() {
        let d = desired();
        let h = Hook {
            command: "exit 9",
            timeout: Duration::from_secs(5),
            changed: &[],
            desired: &d,
            dry_run: false,
        };
        run(&h).unwrap();
    }
}
