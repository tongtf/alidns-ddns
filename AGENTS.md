# AGENTS.md

## 项目概述

阿里云DDNS工具：自动获取公网IPv4/IPv6并更新域名解析记录。

当前实现为对账循环 `observe → diff → apply → schedule`，按职责拆分为多个模块：
`main.rs`（主循环/sd_notify）、`config.rs`、`select.rs`（IPv6 纯函数选取）、
`plan.rs`（纯函数对账）、`state.rs`（退避/变更窗口）、`util.rs`、`log.rs`、
`discover/{ipv4,ipv6}.rs`、`aliyun/{sign,model}.rs`。约 2800 行，无 async runtime。

设计依据见 `docs/ddns-redesign.md`（整体重构方案）与 `docs/ipv6-address-selection.md`（IPv6 选取细节）。

## 配置优先级

CLI参数（手写解析，支持 `--k=v`） > 环境变量 > `config.json` > 默认值

```bash
# CLI 参数
./alidns-ddns --access-key-id AKID --access-key-secret SECRET --domain example.com --rr @ --ipv 4 --interval 300

# 排查用：只跑一轮 / 只打印计划不写 API
./alidns-ddns --once --dry-run -v

支持逗号分隔的多域名/多 RR（`--domain a.com,b.com --rr @,www`），域名数与 RR 数一对多、多对一或等长时配对，统一解析到同一个获取的公网 IP。

# 等价环境变量
ALIBABA_CLOUD_ACCESS_KEY_ID / ALIBABA_CLOUD_ACCESS_KEY_SECRET
ALIDNS_DOMAIN / ALIDNS_RR（默认 @）
DDNS_IPV（4/6/46，默认 6）
DDNS_INTERVAL（秒，默认 300，最小 1）
DDNS_VERIFY_INTERVAL（IP 未变时的 DNS 对账周期，默认 21600）
DDNS_TTL（默认 600）、DDNS_LINE（默认 default）
ALIDNS_IPV6_PREFER（auto/stable/temporary，默认 auto）
DDNS_ALLOW_ULA（默认 false，不允许把 ULA 写入公网 AAAA）
ALIDNS_INTERFACE（网卡名，如 eth0，默认自动搜索；不存在时 warning 并回退）

> 网卡选择仅用于本地 IPv6 获取。Linux 用 netlink `RTM_GETADDR` 读取 `IFA_FLAGS` 和
> `IFA_CACHEINFO`（preferred/valid lifetime、cstamp），按 RFC 4862/4941 区分
> preferred / deprecated / tentative / 临时地址；同一桶内按 cstamp 取最新（回绕安全）。
> ULA 默认拒绝（公网不可达），仅在显式 `--allow-ula` 时写入。回环、fe80、组播等一律排除。
> 指定网卡没有可用地址时打印 warning 并回退自动搜索。IPv4 探测带端点粘性，
> 且必须校验公网性，私网/CGNAT/保留地址一律拒绝。

# 配置文件路径
./alidns-ddns -c /path/to/config.json    # 默认 ./config.json
```

## 构建 / 检查

```bash
cargo build --release        # release profile: opt-level=s, strip, lto
cargo check                  # 快速类型检查
cargo clippy                 # lint（无预配置的门控）
upx --best target/release/alidns-ddns  # 可选压缩
```

无 `[dev-dependencies]`：核心逻辑均为纯函数，直接用 `cargo test` 即可（53 个单测）。

## 关键行为

1. 每轮 `discover`：IPv4 多端点回退（`api.ipify.org` → `v4.ident.me` → `ip.3322.net` → `ipv4.icanhazip.com` → `checkip.amazonaws.com`，8s 超时、端点粘性、公网性校验）；IPv6 在 Linux 走 netlink，macOS 退回 `local-ip-address`
2. `plan::diff` 对每个域名生成计划：DNS 查询按域名批量（`PageSize=500` 自动翻页），匹配 `RR + Line + Type`，显式传 TTL
3. `apply`：IP 变化时更新 / 新建 A / AAAA；`Status=Disable` 告警、Locked 跳过
4. `schedule`：默认 300s；刚变更后 60s 内复查；`preferred_lft` 快到期时提前唤醒；失败指数退避 30s→5min；Linux 订阅 `RTMGRP_IPV6_IFADDR` 事件提前唤醒
5. 期望值未变且未到 `verify-interval` 时完全跳过 DNS 查询；错误分类：网络退避、限流退避、鉴权/时钟退出 78

API 签名为手写 **ACS3-HMAC-SHA256（V3 签名）**，签名 Key 为 `AccessKeySecret`（不附加 `&`）。参数通过 HTTP Header（`Authorization`/`x-acs-*`）传递，非 Query 参数。

> 签名时查询串与头部的百分号编码必须使用**大写十六进制**（如 `%3A`），与阿里云规范化要求一致；签名字节 / 哈希则用小写。大小写混用会导致 `Specified signature does not match our calculation`。

## 注意事项

- `config.json` 含占位凭证，**勿提交真实 Key 到仓库**
- 需要阿里云 DNS 管理权限（`alidns:*`）
- 退出码：`78` = 配置/鉴权错误（systemd `RestartPreventExitStatus=78` 不再重启），`2` = 参数错误，`1` = `--once` 单轮失败，`0` = 正常
- 配置解析失败现在快速失败（退出 78），不再静默用默认值
- 不要使用子用户 AccessKey
- 服务部署按 OS 分发（`install.sh` 用 `uname -s` 检测）：
  - Linux：systemd `alidns-ddns.service`（常驻，`Type=notify` + watchdog）+ `EnvironmentFile=/etc/alidns-ddns/env`，需 root；另有可选的 `alidns-ddns-once.service` + `alidns-ddns.timer` 定时模式（`--once`，无需常驻进程）
  - macOS：launchd 两种模式（`install.sh` 按是否 root 分发，模板均含 `@INSTALL_DIR@`/`@CONFIG_DIR@`/`@LOG_DIR@` 占位符，sed 生成）：
    - root（`sudo ./install.sh`）→ 系统级 LaunchDaemon `alidns-ddns-daemon.plist`（另含 `@USERNAME@`）装到 `/Library/LaunchDaemons/`，`UserName=$SUDO_USER`，配置 `/etc/alidns-ddns/config.json`，日志 `/usr/local/var/log/alidns-ddns.log`，Label `system/com.tongtf.alidns-ddns`；**不依赖登录会话，开机即启**，并会清掉用户级残留避免双实例
    - 普通用户 → LaunchAgent `alidns-ddns.plist` 装到 `~/Library/LaunchAgents/`，配置 `~/.config/alidns-ddns/config.json`，日志 `~/Library/Logs/alidns-ddns.log`；域探测 `launchctl print gui/$UID` 成功用 gui，否则回退 user；**模板必须带 `LimitLoadToSessionType` 数组 `[Aqua, Background]`**——默认值 Aqua 在 SSH/无图形会话下 bootstrap 会报 `Bootstrap failed: 5`，仅 Background 又会在图形登录时报 134 拒载
  - macOS 下载不依赖 sudo 的普通用户模式时若已存在系统级 plist 会直接报错退出（防双实例）；`cargo build` 一律以 `SUDO_USER` 身份执行（root 无 cargo，且避免 target/ 属主变 root）
