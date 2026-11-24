# alidns-ddns 整体梳理与重构方案

状态：评审稿
范围：`src/main.rs`（764 行，单体）、`alidns-ddns.service`、`install.sh`、配置模型
配套：`docs/ipv6-address-selection.md`（IPv6 选取细节，本文只引用不重复）

---

## 0. 结论摘要

| 维度 | 现状 | 建议 |
|---|---|---|
| 结构 | 764 行单文件，无模块边界，无测试 | 拆 8 个模块，纯函数集中可测 |
| 模型 | 无状态轮询：每轮"取 IP → 查 DNS → 写 DNS" | reconcile 循环：`desired` vs `actual` → `plan` → `apply` |
| IPv6 | 取第一个全局地址，不区分临时/稳定/deprecated | netlink `RTM_GETADDR` + 地址状态模型 |
| DNS 读取 | 每次 1 次 `DescribeDomainRecords`，默认只取前 20 条 | 按域名批量取（PageSize=500）+ 精确过滤 + 线路感知 |
| 错误处理 | 所有失败都变成空串，固定间隔无限重试 | 错误分类 + 分级退避 + 配置类错误快速失败 |
| 调度 | 固定 `sleep(3600)` | 自适应（变更后加速、按 `preferred_lft` 定时、失败退避） |
| 运维 | `Type=simple`，无心跳，日志无时间戳 | `Type=notify` + watchdog + 结构化日志 + sysexits 退出码 |

**必须先修的正确性问题（P0，见 §2.3）**：UTF-8 字节切片 panic、DNS 记录只查前 20 条、
IPv4 结果不校验公网性。这三个会分别导致崩溃、重复记录、写入错误 A 记录。

---

## 1. 现状梳理

### 1.1 代码结构

| 区段 | 行数 | 职责 | 评价 |
|---|---|---|---|
| 参数解析 | 19–129 | 手写 `--k v` / `--k=v` / `-c` | 可用，但缺 `--`、`-c=path`、`--once` |
| 配置解析 | 131–249 | JSON + 三级优先级 | 字段名用 `#[allow(non_snake_case)]` |
| ACS3 签名 | 251–323 | RFC3339 / nonce / percent / HMAC | 正确，但混在 main 里 |
| 响应类型 | 325–350 | serde 结构 | `Type` 用了 rename，命名不统一 |
| IPv4 探测 | 352–381 | 5 端点回退 | 只校验"是合法 IPv4"，不校验公网性 |
| IPv6 选取 | 383–455 | `local-ip-address` + `find()` | **核心缺陷**，见 §2.3-① |
| API 调用 | 457–540 | 签名 + ureq + 错误吞掉 | 所有错误 → `String::new()` |
| DNS 写入 | 542–646 | 查 → 比 → 改/增 | 无分页、无线路、无 TTL |
| 主循环 | 655–764 | 配对目标 + 无限循环 | 固定 sleep，无退避 |

依赖 7 个直接 crate，发布产物体积已做 `opt-level=s + lto + strip + panic=abort`。

### 1.2 数据流

```
每 N 秒
  ├─ get_ipv4()   → 串行试 5 个 HTTPS 端点（每个 8s 超时，最坏 40s）
  ├─ get_ipv6()   → local-ip-address 列地址 → find(第一个全局) → 兜底 ULA
  └─ for target in targets:                 # M 个目标 = M 次查询 + M 次写入
       update_dns()
         ├─ DescribeDomainRecords(RRKeyWord)   # 默认 PageSize=20
         ├─ find(RR==rr && Type==type)         # 忽略 Line
         └─ Update / Add（不带 TTL、不带 Line）
  sleep(N)
```

特征：**无记忆、无优先级、无退避、无区分错误类型**。重启后与首次运行完全相同。

### 1.3 逐项体检

#### P0 — 会崩溃或写错记录

| # | 位置 | 问题 | 后果 |
|---|---|---|---|
| ① | `418-424` | `select_ipv6` 取第一个全局地址；不区分临时/稳定/deprecated/tentative；ULA 兜底会写入公网不可达的 AAAA | 轮换期写入旧地址；邮件入站中断 |
| ② | `531` / `613` / `643` | `&body[..body.len().min(200)]` 按**字节**切片 UTF-8 | 阿里云错误信息含中文，切在多字节字符中间即 panic；`panic=abort` 直接杀进程 |
| ③ | `548-555` | `DescribeDomainRecords` 未传 `PageSize`（默认 20）也未翻页 | 域名记录 >20 条时目标可能不在首屏 → 误判"不存在" → `AddDomainRecord` 重复/报错 |
| ④ | `576-583` | 匹配只看 `RR + Type`，忽略 `Line` | 多条线路记录时命中错误的一条 |
| ⑤ | `588-607` | `UpdateDomainRecord` 不带 `TTL`、不带 `Line` | 文档默认 `TTL=600`、`Line=default`，可能把自定义 TTL 重置、或改不到目标线路 |
| ⑥ | `363-381` | 只校验 `parse::<Ipv4Addr>()` 成功 | 运营商劫持页/私网端点返回 `10.x`、`192.168.x` 会被当作公网 A 记录写出去 |

#### P1 — 健壮性与运维

| # | 位置 | 问题 |
|---|---|---|
| ⑦ | `518-535` | 所有传输失败 `Err(_) => String::new()`，丢失原因；无法区分超时/限流/鉴权失败 |
| ⑧ | `740-763` | 固定 `sleep(N)`：失败不退避、变更后不加速、无抖动 |
| ⑨ | `180-197` | 配置文件解析失败 → 警告后**静默用默认值**（fail-open），随后每轮报"缺 DomainName" |
| ⑩ | `222-226` | `DDNS_INTERVAL` 解析失败被静默忽略（`.and_then(parse().ok())`） |
| ⑪ | `716-721` | `--ipv` 用 `contains('4')/contains('6')` 校验，`6x`、`46z` 都能通过 |
| ⑫ | 全文 | `println!` 无级别、无时间戳；macOS 写文件后无法定位时序 |
| ⑬ | `514` | 每次 `ureq::get` 新建 agent，不复用连接/TLS 会话 |
| ⑭ | `363-381` | 端点无粘性：大陆网络下 ipify 每轮超时 8s 才回退 |
| ⑮ | `740` | 默认 `Interval=3600`：邮件服务器的 IP 变化最长 1 小时才生效 |
| ⑯ | — | 无 `--once` / `--dry-run`，无法用 systemd timer 或排查 |

#### P2 — 可维护性

| # | 问题 |
|---|---|
| ⑰ | 单文件 764 行；纯函数（签名、百分号编码、选取）无单测 |
| ⑱ | `AGENTS.md` 标注 `main.rs` ~614 行，已过期 |
| ⑲ | `le-renew.sh` / `le.env.example` 为未跟踪的历史遗留（acme.sh 方案已取代） |
| ⑳ | 记录 `Status=Disable` / `Locked=true` 未处理：更新会"成功"但解析不生效 / 直接报错 |

---

## 2. Linux 对应实现规范

原则：**能用内核已有的语义，就不要在用户态猜**。

### 2.1 地址与生命周期

| 需求 | 内核设施 | 规范要点 |
|---|---|---|
| 枚举本机 IPv6 | netlink `RTM_GETADDR` | `IFA_FLAGS`（完整 u32）+ `IFA_CACHEINFO`（preferred/valid/cstamp） |
| 生命周期语义 | RFC 4862 §5.5.3 | lifetime 只可延长不可缩短；`preferred_lft=0` ⇒ deprecated；`valid_lft=0` ⇒ 删除 |
| 隐私地址 | RFC 4941 | `IFA_F_TEMPORARY`；轮换期新旧并存，旧地址变 deprecated |
| DAD | RFC 4862 §5.4 | `IFA_F_TENTATIVE` 期间**不可发布**；`IFA_F_DADFAILED` 永久排除 |
| 特殊地址 | RFC 6890 / 4291 | 排除 `::1`、`fe80::/10`、`ff00::/8`、`2001:db8::/32`、`64:ff9b::/96`、`2002::/16` |
| 出站源地址 | RFC 6724 + `RTM_GETROUTE` | 仅作 tie-break；`use_tempaddr=2` 时内核会优先临时地址，不能当唯一判据 |

**不能用**：`getifaddrs()` 的 `ifa_flags` 是接口标志（`IFF_*`），不含 `IFA_F_*`；
`/proc/net/if_inet6` 的 flags 被内核截断成 u8 且没有 lifetime。详见
`docs/ipv6-address-selection.md` §3.4 / §3.5。

### 2.2 时间

| 用途 | 时钟 | 注意 |
|---|---|---|
| 轮询间隔、退避 | `CLOCK_MONOTONIC`（`thread::sleep` 天然满足） | 不受改系统时间影响 |
| 签名 `x-acs-date` | `CLOCK_REALTIME` | 偏差 > 15 分钟 → `InvalidTimeStamp.Expired`，需在日志中明确指出并建议 NTP |
| 变更检测窗口 | 单调钟 | 不能用 `SystemTime` 比较 |

### 2.3 进程监督（systemd）

现有单元已经不错（`DynamicUser`、`ProtectSystem=strict`、
`RestrictAddressFamilies=AF_NETLINK AF_LOCAL AF_INET AF_INET6`——正好覆盖 netlink 与
`NOTIFY_SOCKET`）。缺口：

| 项 | 现状 | 建议 |
|---|---|---|
| 存活检测 | `Type=simple`，进程挂死无人知 | `Type=notify` + `WatchdogSec=300` + 发送 `READY=1` / `WATCHDOG=1` |
| 重启策略 | `Restart=always` + `RestartSec=10` | `Restart=on-failure` + `RestartPreventExitStatus=78`（配置错误不重启） |
| 状态目录 | 无（`ProtectSystem=strict` 下无处可写） | `StateDirectory=alidns-ddns`（DynamicUser 下自动落到 `/var/lib/private/...`） |
| 时钟同步 | 未声明 | `After=time-sync.target` |
| 能力 | 未限制 | `CapabilityBoundingSet=`（空，本工具不需要任何 capability） |
| 系统调用 | 未限制 | 视测试结果加 `SystemCallFilter=@system-service` |

`sd_notify` 不需要任何依赖，Rust 1.70 的
`std::os::linux::net::SocketAddrExt::from_abstract_name` 即可发 datagram：

```rust
fn notify(state: &str) {
    let Ok(path) = std::env::var("NOTIFY_SOCKET") else { return };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else { return };
    let _ = if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .and_then(|a| sock.send_to(state.as_bytes(), a))
    } else {
        std::os::unix::net::SocketAddr::from_pathname(&path)
            .and_then(|a| sock.send_to(state.as_bytes(), a))
    };
}
```

### 2.4 密钥与配置

现有约定：`config.json` 644（`DynamicUser` 临时 UID 需要可读）、`env` 600。
这是自相矛盾的：**最敏感的两个字段（AK/SK）被允许写在 world-readable 文件里**。

建议改用 systemd 凭据（≥ 247）：

```ini
LoadCredential=alidns-env:/etc/alidns-ddns/env
EnvironmentFile=-/etc/alidns-ddns/env
```

或直接规定 `AccessKeySecret` **只能**来自环境变量/凭据，`config.json` 中出现非空
secret 时打 warning 并忽略（配合 `chmod 600` + 专用静态用户 `User=alidns`）。

另外：`--access-key-secret` 会出现在 `/proc/<pid>/cmdline`，建议标记为 deprecated。

### 2.5 日志与退出码

- 一行一条、带级别、带 RFC3339 时间戳（journald 会再加一层前缀，无害）
- 关键状态变化才打 INFO，稳态打 DEBUG（避免 60s 一条垃圾日志）
- 退出码遵循 `sysexits.h`：配置/鉴权错误 `78 (EX_CONFIG)`，被信号终止 `0`，其余 `1`

### 2.6 规范对照总表

| 能力 | Linux 原生做法 | 现实现 | 差距 |
|---|---|---|---|
| IPv6 地址枚举 | netlink `RTM_GETADDR` | `local-ip-address`（只给名字+IP） | 拿不到状态 |
| 地址变更通知 | `RTMGRP_IPV6_IFADDR` 组播 | 固定轮询 | 延迟一个周期 |
| 源地址选择 | `RTM_GETROUTE` / `ip -6 route get` | 未用 | 多网卡时无裁决 |
| 进程就绪/存活 | `sd_notify` + `WatchdogSec` | `Type=simple` | 挂死不可见 |
| 密钥传递 | `LoadCredential` | 644 的 JSON 或 600 的 env | 存在明文暴露面 |
| 调度 | `systemd timer` + `--once` | 进程内 `sleep` | 无外部可控性 |
| 退出语义 | `sysexits.h` | 一律 `1` | 无法区分可重试/不可重试 |

---

## 3. 目标架构

### 3.1 一句话

从**无状态轮询**改为**带状态的对账循环（reconcile loop）**：

```
        ┌──────────────┐
        │  observe()   │  desired: netlink(RFC 4862/4941) + HTTP(公网 IPv4)
        │              │  actual : DNS(精确 + 分页 + 线路)
        └──────┬───────┘
               ▼
        ┌──────────────┐
        │   diff()     │  纯函数：Vec<Target> × State → Vec<Change>
        └──────┬───────┘
               ▼
        ┌──────────────┐
        │   apply()    │  幂等写入 + 错误分类
        └──────┬───────┘
               ▼
        ┌──────────────┐
        │  schedule()  │  自适应下次唤醒时间
        └──────────────┘
```

三个直接收益：

1. **可解释**：每一轮都能打印"期望 / 实际 / 计划"，`--dry-run` 零风险预演
2. **可退避**：错误有类型，才有重试策略
3. **可测试**：`diff()` 与 `select()` 是纯函数，无网卡无网络也能单测

### 3.2 模块划分

```
src/
  main.rs            启动、信号、主循环、sd_notify          ~150
  config.rs          解析 + 校验 + 三级优先级 + 目标展开     ~180
  log.rs             带时间戳/级别的日志宏                   ~40
  state.rs           内存态 + 可选持久化（原子写）            ~80
  select.rs          纯函数：地址选取（含 IPv6 状态模型）      ~120
  discover/
    mod.rs           Desired 汇总                           ~40
    ipv4.rs          端点粘性 + 公网性校验                  ~90
    ipv6_linux.rs    netlink RTM_GETADDR → V6Cand           ~110
    ipv6_other.rs    macOS 回退（保持现状 + 告警）           ~30
  aliyun/
    mod.rs           客户端、分页、错误分类                  ~180
    sign.rs          ACS3（纯函数）                          ~90
    model.rs         serde 结构 + 记录匹配                   ~80
  plan.rs            diff + 目标匹配（纯函数）               ~100
```

总量约 1300 行，`main.rs` 降到 ~150 行。依赖：`neli` 提为直接依赖（已在树中），
Linux 下移除 `local-ip-address`。

### 3.3 核心类型

```rust
#[derive(Clone, PartialEq, Eq, Hash)]
struct Target { domain: String, rr: String, line: String, rtype: RType }

struct Desired { v4: Option<Ipv4Addr>, v6: Option<V6Pick> }

enum V6Pick {
    Strict(Ipv6Addr),          // 全局 + preferred（稳定或临时）
    Degraded(Ipv6Addr, Why),   // deprecated / ULA / 仅临时
}

enum Outcome {
    InSync,
    Updated,
    SkippedLocal(Reason),      // 本地无可用地址：不动 DNS
    Retry(Reason, Duration),   // 网络/限流：退避后重试
    Fatal(Reason),             // 鉴权/配置：退出 78
    TargetError(Target, Reason) // 单目标错误：跳过该目标
}
```

### 3.4 主循环

```rust
loop {
    notify("WATCHDOG=1");
    let desired = discover(&cfg);              // 不 panic、不返回空串
    let actual  = aliyun::lookup_all(&cfg, &targets);   // 按域名批量，1 次/域名
    let plan    = plan::diff(&desired, &actual, &state);

    match apply(&cfg, &plan) {
        Ok(applied) => state.commit(applied),
        Err(Fatal(e)) => { log::error(e); exit(78); }
        Err(e) => { /* 退避 */ }
    }
    sleep(schedule(&desired, &state, &cfg));
}
```

---

## 4. 关键设计

### 4.1 地址选取

见 `docs/ipv6-address-selection.md`：netlink + 状态模型，桶序
`Global/Stable/Preferred > Global/Temporary/Preferred > ULA > Deprecated`，
桶内按 `cstamp`（u32 回绕用 `wrapping_sub`）取最新。

**修正一处策略**：ULA 不应写入公网 AAAA。当前 `select_ipv6` 把 ULA 当兜底，
会把 `fc00::/7` 发布到公网，客户端永远连不上。新方案：ULA 仅作为
"日志告警 + 跳过"，除非显式 `--allow-ula`。

### 4.2 公网 IPv4 校验

```rust
fn is_public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_private() || ip.is_loopback() || ip.is_link_local()
      || ip.is_broadcast() || ip.is_documentation() || ip.is_unspecified()
      || (o[0] == 100 && (o[1] & 0xc0) == 64)   // 100.64/10 CGNAT
      || (o[0] == 192 && o[1] == 0 && o[2] == 0)
      || (o[0] == 198 && (o[1] & 0xfe) == 18)   // 198.18/15
      || o[0] >= 240)                            // 保留
}
```

不通过 → 视为本轮探测失败并告警（`10.x` 通常意味着被劫持到运营商门户或代理）。

### 4.3 DNS 读取

| 项 | 现状 | 目标 |
|---|---|---|
| 粒度 | 每个 `(domain, rr)` 一次调用 | **每个 domain 一次**，`PageSize=500`，翻页直到 `TotalCount` 覆盖 |
| 过滤 | `RRKeyWord` 模糊 + 客户端 `==` | `SearchMode=COMBINATION` + `RRKeyWord`/`TypeKeyWord` 精确，客户端再兜一层 |
| 线路 | 忽略 | 客户端按 `Line` 精确匹配，默认 `default` |
| 缓存 | 无 | 期望 IP 未变且距上次校验 < `verify-interval` 时**跳过查询** |

`TotalCount` 已在响应里，可直接判断是否需要翻页，不必猜。

### 4.4 DNS 写入

| 项 | 现状 | 目标 |
|---|---|---|
| TTL | 不传 | 显式传 `TTL`（默认 600，可配）；避免把自定义 TTL 重置 |
| Line | 不传 | 从实际记录回填 |
| 状态 | 忽略 | `Status=Disable` 告警（解析不生效）；`Locked=true` 直接跳过并告警 |
| 成功判定 | 看有无 `Code` | 同上，但补充：`RequestId` 缺失视为异常 |
| 幂等 | 值相同即跳过 | 同左，且写入后**回读校验**（可选，默认关） |

### 4.5 错误分类

| 类别 | 判据（示例，以实际返回为准） | 动作 |
|---|---|---|
| 本地未就绪 | 无全局地址 / 全部 tentative | 15–30s 后重试，**不动 DNS**，状态变化才打日志 |
| 网络 | 超时、TLS、DNS 解析失败 | 指数退避（30s → 5min 封顶），保留旧记录 |
| 限流 | `Throttling.*`、`ServiceUnavailable` | 退避 ≥ 60s |
| 鉴权 | `InvalidAccessKeyId.*`、`SignatureDoesNotMatch`、`Forbidden*` | **Fatal**，打印一次，退出 78 |
| 参数 | `DomainRecordDuplicate`、`InvalidDomainName*`、RR 非法 | 仅该目标失效，其余继续 |
| 时钟 | `InvalidTimeStamp.*` | Fatal + 提示 NTP |

### 4.6 自适应调度

```rust
fn schedule(d: &Desired, s: &State, cfg: &Config) -> Duration {
    let mut next = cfg.interval;                    // 默认 300s（不再是 3600）
    if s.last_change_within(Duration::from_secs(600)) {
        next = next.min(Duration::from_secs(60));   // 刚变更：加速确认稳定
    }
    if let Some(lft) = d.v6.as_ref().and_then(|v| v.preferred_lft()) {
        if lft < next * 3 {                         // 快到期：提前复查
            next = next.min(Duration::from_secs(lft / 2).max(Duration::from_secs(15)));
        }
    }
    if let Some(backoff) = s.backoff() {
        next = next.max(backoff);
    }
    next
}
```

稳态下（地址稳定）几乎不产生 API 调用；变更时秒级跟上。

### 4.7 端点粘性

记录上一次成功的 IPv4 端点，下次先试它；失败才回退列表。避免大陆网络下每轮先
等 ipify 超时 8 秒。`--ipv 4` 单栈场景下最坏耗时从 40s 降到 8s。

---

## 5. 配置与接口

保持现有键与优先级（CLI > env > config.json > 默认），新增项：

| CLI | 环境变量 | config.json | 默认 | 说明 |
|---|---|---|---|---|
| `--interval` | `DDNS_INTERVAL` | `Interval` | **300** | 由 3600 降到 300（邮件场景） |
| `--verify-interval` | `DDNS_VERIFY_INTERVAL` | `VerifyInterval` | 21600 | 期望值未变时的 DNS 对账周期 |
| `--ttl` | `DDNS_TTL` | `Ttl` | 600 | 写入 TTL |
| `--line` | `DDNS_LINE` | `Line` | `default` | 记录线路 |
| `--ipv6-prefer` | `ALIDNS_IPV6_PREFER` | `Ipv6Prefer` | `auto` | `auto`/`stable`/`temporary` |
| `--allow-ula` | `DDNS_ALLOW_ULA` | `AllowUla` | false | 显式允许发布 ULA（默认拒绝） |
| `--once` | — | — | false | 跑一轮即退出（timer 模式/排查） |
| `--dry-run` | — | — | false | 只打印计划，不写 API |
| `-v/--verbose` | `DDNS_VERBOSE` | — | false | 打印每个候选地址的判定过程 |

校验改为快速失败（**不再 fail-open**）：配置文件存在但解析失败 → 退出 78；
`--ipv` 只接受 `4`/`6`/`46`；`--interval` 越界报错。

---

## 6. 部署

### 6.1 单元文件增量

```ini
[Unit]
After=network-online.target time-sync.target
StartLimitIntervalSec=300
StartLimitBurst=5

[Service]
Type=notify
NotifyAccess=main
WatchdogSec=300
Restart=on-failure
RestartPreventExitStatus=78
StateDirectory=alidns-ddns
UMask=0077
CapabilityBoundingSet=
# 可选（systemd >= 247）
# LoadCredential=env:/etc/alidns-ddns/env
```

`RestartPreventExitStatus=78` 是关键：配置/鉴权错误进 `failed` 状态而不是无限重启，
运维 `systemctl status` 一眼可见。

### 6.2 两种运行形态

| 形态 | 配置 | 适用 |
|---|---|---|
| 常驻 | 默认（`Type=notify`，内部自适应调度） | 需要秒级响应 |
| 定时 | `alidns-ddns.timer` + `--once` | 偏好"无守护进程"，systemd 负责调度 |

两者共用同一份代码，`--once` 只是把循环跑 1 次。

### 6.3 邮件场景附注

- AAAA/A 的 TTL 建议 600（阿里云免费版下限）；修改前若要快速切换，可先临时调低
- MX 必须指向主机名（而非 IP），否则改 A/AAAA 对邮件无意义
- SPF 用 `a` / `mx` 机制，不需要随 IP 改 TXT
- **反向解析（PTR）无法由 DDNS 解决**：动态 IP 的 PTR 属于运营商，多数接收方会因此
  拒信。DDNS 只保证"能连上"，不保证"能投递"

---

## 7. 测试

无 `[dev-dependencies]` 也能测，因为核心逻辑是纯函数：

| 层 | 用例 |
|---|---|
| `select` | 临时/稳定并存、轮换期、前缀变更形态 A/B、cstamp 回绕、ULA-only、全 deprecated、tentative |
| `plan` | 值相同、值不同、记录缺失、多线路、Disabled、Locked、目标去重 |
| `sign` | 固定输入对比阿里云官方示例的签名字符串 |
| 解析 | netlink 属性字节 fixture → `V6Cand`（不依赖真实网卡） |
| 错误分类 | 各类错误码 → `Outcome` 映射 |

---

## 8. 分阶段落地

| 阶段 | 内容 | 行数 | 风险 | 价值 |
|---|---|---|---|---|
| **P0 热修** | ② UTF-8 切片、⑥ 公网校验、③④⑤ 分页/线路/TTL、⑦ 错误不吞 | ~120 | 低 | 立刻消除崩溃与写错记录 |
| **P1 选取** | netlink + 地址状态模型 + ULA 拒绝 + 单测 | ~330 | 中 | 解决 IPv6 轮换核心问题 |
| **P2 对账** | 模块拆分、`plan`/`Outcome`、错误分类、退避、自适应调度、批量查询 | ~300 | 中 | API 调用降一个数量级，故障可自愈 |
| **P3 运维** | 日志、`sd_notify` watchdog、`--once`/`--dry-run`、退出码、单元文件 | ~180 | 低 | 挂死可发现，配置错误不刷屏 |
| **P4 可选** | netlink 事件订阅（`RTMGRP_IPV6_IFADDR`）、状态持久化 | ~150 | 中 | 延迟从"一个周期"降到秒级 |

建议顺序：P0 → P1 → P3 → P2 → P4。P3 提前做，是因为它能立刻改善线上可观测性，
而 P2 的重构在可观测性到位后更好验证。

---

## 9. 非目标

- 不引入 async runtime（tokio 等）：单线程阻塞模型足够
- 不内嵌 ACME 客户端：证书用 acme.sh `--dns dns_aliyun`
- 不改 ACS3 签名算法（当前实现已与阿里云规范一致）
- 不支持多账号/多区域：单账号单端点足够
- 不做 DDNS 之外的 DNS 管理（改 MX/TXT 等）
