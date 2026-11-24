//! alidns-ddns —— 阿里云域名动态解析。
//!
//! 结构：`observe → diff → apply → schedule` 的对账循环。
//! - `discover`：期望状态（公网 IPv4 探测、netlink 读取 IPv6 及其生命周期）
//! - `plan`：纯函数，比较期望值与 DNS 实际记录
//! - `aliyun`：签名、分页读取、幂等写入、错误分类
//! - `state`：单调钟计时（变更窗口、退避、告警去重）

mod aliyun;
mod config;
mod discover;
mod hook;
mod log;
mod plan;
mod select;
mod state;
mod util;

use aliyun::{ApiError, Client};
use config::{Config, Target, EX_CONFIG};
use plan::{Desired, Step, StepKind};
use state::State;
use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

fn main() {
    // 用 sysexits 的退出码区分"可重试"与"需人工介入"，配合 systemd 的
    // RestartPreventExitStatus=78 使用。
    std::process::exit(run());
}

fn run() -> i32 {
    let args = match config::parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("参数错误: {e}\n\n{}", config::usage());
            return config::EX_USAGE;
        }
    };
    if args.help {
        print!("{}", config::usage());
        return 0;
    }

    let cfg = match config::from_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("配置错误: {e}");
            return EX_CONFIG;
        }
    };

    log::set_verbose(cfg.verbose);
    for w in &cfg.warnings {
        log::warn(w);
    }
    if cfg.dry_run {
        log::warn("dry-run：只打印计划，不调用任何写接口");
    }
    log::info(format!(
        "启动：{} 个目标 [{}]，间隔 {}s，对账周期 {}s，TTL {}，线路 {}",
        cfg.targets.len(),
        cfg.targets
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        cfg.interval,
        cfg.verify_interval,
        cfg.ttl,
        cfg.targets
            .iter()
            .map(|t| t.line.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(",")
    ));

    let http = discover::Http::new(Duration::from_secs(8));
    let client = Client::new(&cfg);
    let mut state = State::load(cfg.state_file.clone());
    let mut probe = discover::ipv4::Probe::default();
    probe.set_sticky_endpoint(state.ipv4_endpoint());
    if let Some(path) = &cfg.state_file {
        log::info(format!("状态文件: {}", path.display()));
    } else {
        log::info("状态持久化已禁用（每次运行从零开始）");
    }
    let mut events: Option<Receiver<()>> = if cfg.events {
        discover::ipv6::spawn_change_watcher()
    } else {
        None
    };
    if events.is_some() {
        log::info("已订阅 netlink 地址变更事件（地址变化时提前唤醒）");
    }

    let by_domain = group_targets(&cfg.targets);

    notify("READY=1");

    loop {
        notify("WATCHDOG=1");
        let desired = discover::observe(&cfg, &http, &mut probe, &mut state);
        // Probe 持运行时值；State 只是持久化镜像（set_ipv4_endpoint 内部负责去重）
        state.set_ipv4_endpoint(probe.sticky_endpoint());

        let changed: Vec<Target>;
        match sync(&client, &cfg, &desired, &by_domain, &mut state) {
            Ok((sum, err)) => {
                changed = sum.changed;
                match err {
                    None => state.note_cycle_ok(),
                    Some(e) => {
                        state.note_cycle_fail();
                        log::warn(format!("本轮部分目标失败（{e}），退避后重试"));
                    }
                }
                let line = format!(
                    "本轮完成：更新 {}，新建 {}，一致 {}，跳过 {}",
                    sum.updated, sum.added, sum.synced, sum.skipped
                );
                if sum.updated + sum.added > 0 {
                    log::info(line);
                } else {
                    log::debug(line);
                }
            }
            // sync 只在鉴权/时钟这类致命错误时返回 Err
            Err(e) => {
                log::error(format!("不可重试错误，退出：{e}"));
                notify("STATUS=配置或鉴权错误，需要人工介入");
                notify("STOPPING=1");
                persist(&cfg, &mut state);
                return EX_CONFIG;
            }
        }

        // dry-run 不落盘：连状态文件都不应被修改
        persist(&cfg, &mut state);

        if let Some(command) = &cfg.on_change {
            let h = hook::Hook {
                command,
                timeout: Duration::from_secs(cfg.on_change_timeout),
                changed: &changed,
                desired: &desired,
                dry_run: cfg.dry_run,
            };
            if let Err(e) = hook::run(&h) {
                // 钩子失败不回滚 DNS，也不改变本轮退出码
                log::warn(e);
            }
        }

        let wait = schedule(&desired, &state, &cfg);
        log::debug(format!("{}s 后再次检查", wait.as_secs()));
        idle(&mut events, wait);
    }
}

/// 写路径的参数错误只影响单条记录；网络/限流等仍需计入整轮退避。
fn write_error_counts(e: &ApiError) -> bool {
    !matches!(e, ApiError::Param(_))
}

/// 读路径的参数错误（InvalidDomainName 等）是域名级配置问题：
/// 整个域名都没完成对账，且重试无意义，升级为 fatal。
fn domain_read_error(domain: &str, e: ApiError) -> ApiError {
    match e {
        ApiError::Param(m) => ApiError::Config(format!("域名 {domain} 的解析配置错误: {m}")),
        other => other,
    }
}

#[derive(Default)]
struct Summary {
    updated: usize,
    added: usize,
    synced: usize,
    skipped: usize,
    /// 实际发生写入（Update/Add）的目标，供 on-change 钩子使用。
    changed: Vec<Target>,
}

/// 唯一的落盘入口；dry-run 连状态文件都不碰。
fn persist(cfg: &Config, state: &mut State) {
    if !cfg.dry_run {
        state.save_if_dirty();
    }
}

fn group_targets(targets: &[Target]) -> BTreeMap<String, Vec<Target>> {
    let mut map: BTreeMap<String, Vec<Target>> = BTreeMap::new();
    for t in targets {
        map.entry(t.domain.clone()).or_default().push(t.clone());
    }
    map
}

/// 读取 + diff：每个域名只查一次 DNS，只为"需要校验"的目标生成动作。
/// 返回动作列表和读取阶段的非致命错误（不丢掉已经生成的动作）。
fn plan_cycle(
    client: &Client,
    cfg: &Config,
    desired: &Desired,
    by_domain: &BTreeMap<String, Vec<Target>>,
    state: &State,
) -> Result<(Vec<Step>, Option<ApiError>), ApiError> {
    let mut steps = Vec::new();
    let mut first_err: Option<ApiError> = None;
    let verify = Duration::from_secs(cfg.verify_interval);

    for (domain, targets) in by_domain {
        // 域名很多时单轮可能超过 watchdog 窗口，逐个域名喂狗
        notify("WATCHDOG=1");
        // 期望值未变且距上次校验未超 verify-interval 时完全跳过 DNS 查询。
        let need: Vec<&Target> = targets
            .iter()
            .filter(|t| match desired.value_for(t) {
                Some(v) => state.needs_lookup(t, &v.to_string(), verify),
                // 本轮没有该类型的地址：不动 DNS，也不记录校验时间。
                None => false,
            })
            .collect();
        if need.is_empty() {
            continue;
        }

        let records = match client.list_records(domain) {
            Ok(r) => r,
            Err(e) if e.fatal() => return Err(e),
            Err(e) => {
                let e = domain_read_error(domain, e);
                if e.fatal() {
                    return Err(e);
                }
                log::warn(format!("读取 {domain} 的记录失败：{e}"));
                if first_err.is_none() {
                    first_err = Some(e);
                }
                continue;
            }
        };

        steps.extend(plan::diff(&need, desired, &records, cfg.ttl));
    }

    Ok((steps, first_err))
}

/// 应用阶段：执行动作、维护状态、累计摘要。
fn apply(
    cfg: &Config,
    steps: Vec<Step>,
    client: &Client,
    state: &mut State,
) -> Result<(Summary, Option<ApiError>), ApiError> {
    let mut sum = Summary::default();
    let mut first_err: Option<ApiError> = None;

    for step in steps {
        if let Some(note) = &step.note {
            log::warn(note);
        }
        match step.kind {
            StepKind::InSync { ref value } => {
                state.note_synced(&step.target, &value.to_string());
                sum.synced += 1;
            }
            StepKind::Skip { ref actual } => {
                // 缓存 DNS 里的真实值（不是期望值），用户解锁后下一轮即可发现并修正
                state.note_skipped(&step.target, actual);
                sum.skipped += 1;
            }
            StepKind::Update { ref id, ref value } => {
                if cfg.dry_run {
                    log::info(format!("[dry-run] 更新 {} -> {value}", step.target));
                    sum.updated += 1;
                    sum.changed.push(step.target.clone());
                    continue;
                }
                match client.update(&step.target, id, &value.to_string(), cfg.ttl) {
                    Ok(()) => {
                        log::info(format!("更新 {} -> {value}", step.target));
                        state.note_synced(&step.target, &value.to_string());
                        state.note_change();
                        sum.updated += 1;
                        sum.changed.push(step.target.clone());
                    }
                    Err(e) if e.fatal() => return Err(e),
                    Err(e) => {
                        log::warn(format!("更新 {} 失败：{e}", step.target));
                        if write_error_counts(&e) && first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
            StepKind::Add { ref value } => {
                if cfg.dry_run {
                    log::info(format!("[dry-run] 新建 {} -> {value}", step.target));
                    sum.added += 1;
                    sum.changed.push(step.target.clone());
                    continue;
                }
                match client.add(&step.target, &value.to_string(), cfg.ttl) {
                    Ok(()) => {
                        log::info(format!("新建 {} -> {value}", step.target));
                        state.note_synced(&step.target, &value.to_string());
                        state.note_change();
                        sum.added += 1;
                        sum.changed.push(step.target.clone());
                    }
                    Err(e) if e.fatal() => return Err(e),
                    Err(e) => {
                        log::warn(format!("新建 {} 失败：{e}", step.target));
                        if write_error_counts(&e) && first_err.is_none() {
                            first_err = Some(e);
                        }
                    }
                }
            }
        }
    }

    Ok((sum, first_err))
}

/// 编排：读+diff → apply。致命错误立即上抛；非致命错误与摘要一起返回。
fn sync(
    client: &Client,
    cfg: &Config,
    desired: &Desired,
    by_domain: &BTreeMap<String, Vec<Target>>,
    state: &mut State,
) -> Result<(Summary, Option<ApiError>), ApiError> {
    let (steps, read_err) = plan_cycle(client, cfg, desired, by_domain, state)?;
    let (sum, write_err) = apply(cfg, steps, client, state)?;
    let err = write_err.or(read_err);
    Ok((sum, err))
}

/// 自适应调度：默认间隔，变更后加速确认，`preferred_lft` 快到期时提前复查，失败则退避。
fn schedule(d: &Desired, s: &State, cfg: &Config) -> Duration {
    let mut next = Duration::from_secs(cfg.interval);

    // 刚变更过：用更短的间隔确认新值稳定（也更快发现运营商的二次变更）
    if s.recently_changed(Duration::from_secs(600)) {
        next = next.min(Duration::from_secs(60));
    }

    // IPv6 生命周期即将到期：在过期前复查，避免把 deprecated 地址留在 DNS 里
    if let Some(lft) = d.v6_lft {
        let half = Duration::from_secs((lft / 2).max(15));
        next = next.min(half);
    }

    if let Some(backoff) = s.backoff() {
        next = next.max(backoff);
    }
    next
}

/// 等待下一轮；同时把长等待切成 ≤20s 的片，期间持续喂 systemd watchdog。
fn idle(events: &mut Option<Receiver<()>>, total: Duration) {
    let deadline = Instant::now() + total;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left < Duration::from_millis(100) {
            break;
        }
        notify("WATCHDOG=1");
        let slice = left.min(Duration::from_secs(20));

        // 事件线程异常退出后必须永久降级，否则每轮都会重复告警
        let mut disconnected = false;
        let woke = match events.as_ref() {
            Some(rx) => match rx.recv_timeout(slice) {
                Ok(()) => true,
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    false
                }
            },
            None => {
                std::thread::sleep(slice);
                false
            }
        };
        if disconnected {
            log::warn("netlink 地址变更事件通道已断开，后续退回定时轮询");
            *events = None;
            continue;
        }
        if woke {
            log::debug("收到地址变更事件，提前唤醒");
            break;
        }
    }
}

/// `sd_notify`：不需要任何依赖。`NOTIFY_SOCKET` 未设置时静默返回。
#[cfg(target_os = "linux")]
fn notify(state: &str) {
    let Ok(path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else {
        return;
    };
    let sent = if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .and_then(|addr| sock.send_to_addr(state.as_bytes(), &addr))
    } else {
        std::os::unix::net::SocketAddr::from_pathname(&path)
            .and_then(|addr| sock.send_to_addr(state.as_bytes(), &addr))
    };
    let _ = sent;
}

#[cfg(not(target_os = "linux"))]
fn notify(_state: &str) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Args;

    fn test_cfg(interval: u64) -> Config {
        config::from_args(&Args {
            access_key_id: Some("id".into()),
            access_key_secret: Some("sk".into()),
            domain: Some("a.com".into()),
            interval: Some(interval),
            config: Some("/nonexistent-alidns-test.json".into()),
            no_state: true,
            ..Args::default()
        })
        .unwrap()
    }

    #[test]
    fn write_error_classification() {
        assert!(!write_error_counts(&ApiError::Param("Duplicate".into())));
        assert!(write_error_counts(&ApiError::Transport("timeout".into())));
    }

    #[test]
    fn domain_read_param_error_becomes_fatal_config_error() {
        let e = domain_read_error("a..com", ApiError::Param("InvalidDomainName".into()));
        assert!(e.fatal());
        assert!(e.to_string().contains("a..com"));

        let e = domain_read_error("a.com", ApiError::Transport("timeout".into()));
        assert!(!e.fatal());
    }

    #[test]
    fn schedule_base_and_recent_change() {
        let c = test_cfg(300);
        assert_eq!(
            schedule(&Desired::default(), &State::default(), &c).as_secs(),
            300
        );

        let mut s = State::default();
        s.note_change();
        assert_eq!(schedule(&Desired::default(), &s, &c).as_secs(), 60);
    }

    #[test]
    fn schedule_lifetime_and_backoff() {
        let c = test_cfg(300);
        let mut d = Desired::default();
        d.v6_lft = Some(40); // 半衰 20s，但下限 15s
        assert_eq!(schedule(&d, &State::default(), &c).as_secs(), 20);

        let mut s = State::default();
        s.note_cycle_fail();
        s.note_cycle_fail();
        s.note_cycle_fail(); // 退避 120s
        assert_eq!(schedule(&d, &s, &c).as_secs(), 120);
    }
}
