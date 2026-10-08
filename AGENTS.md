# AGENTS.md

## 项目概述

阿里云DDNS工具：自动获取公网IPv4/IPv6并更新域名解析记录。单文件入口 `src/main.rs`（~614行）。

## 配置优先级

CLI参数（clap） > 环境变量 > `config.json` > 默认值

```bash
# CLI 参数（所有参数都支持 --env 从环境变量读取）
./alidns-ddns --access-key-id AKID --access-key-secret SECRET --domain example.com --rr @ --ipv 4 --interval 300

支持逗号分隔的多域名/多 RR（`--domain a.com,b.com --rr @,www`），域名数与 RR 数一对多、多对一或等长时配对，统一解析到同一个获取的公网 IP。

# 等价环境变量
ALIBABA_CLOUD_ACCESS_KEY_ID / ALIBABA_CLOUD_ACCESS_KEY_SECRET
ALIDNS_DOMAIN / ALIDNS_RR（默认 @）
DDNS_IPV（4/6/46，默认 6）
DDNS_INTERVAL（秒，默认 3600，最小 1）
ALIDNS_INTERFACE（网卡名，如 eth0，默认自动搜索；不存在时 warning 并回退）

> 网卡选择仅用于本地 IPv6 获取（IPv4 走 api.ipify.org 外部服务，不涉及本地网卡）。选取 IPv6 时**优先全局可路由地址，不存在时兜底 ULA（fc00::/7）**；回环与 fe80 链接本地地址公网不可达，一律排除。指定网卡存在但未取到可用 IPv6 时，打印 warning 并自动搜索可用网卡。

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

无测试基础设施（`[dev-dependencies]` 为空）。

## 关键行为

1. 每 N 秒获取 IPv4（多端点回退：`api.ipify.org` → `v4.ident.me` → `ip.3322.net` → `ipv4.icanhazip.com` → `checkip.amazonaws.com`，单请求 8s 超时；ipify 在大陆网络常不可达，故需回退）和 IPv6（本地网卡，`local-ip-address` 库）
2. 调用阿里云 DNS API（`DescribeDomainRecords`/`UpdateDomainRecord`/`AddDomainRecord`）
3. IP 变化时自动更新 / 新建 A / AAAA 记录

API 签名为手写 **ACS3-HMAC-SHA256（V3 签名）**，签名 Key 为 `AccessKeySecret`（不附加 `&`）。参数通过 HTTP Header（`Authorization`/`x-acs-*`）传递，非 Query 参数。

> 签名时查询串与头部的百分号编码必须使用**大写十六进制**（如 `%3A`），与阿里云规范化要求一致；签名字节 / 哈希则用小写。大小写混用会导致 `Specified signature does not match our calculation`。

## 注意事项

- `config.json` 含占位凭证，**勿提交真实 Key 到仓库**
- 需要阿里云 DNS 管理权限（`alidns:*`）
- 不要使用子用户 AccessKey
- 服务部署按 OS 分发（`install.sh` 用 `uname -s` 检测）：
  - Linux：systemd `alidns-ddns.service` + `EnvironmentFile=/etc/alidns-ddns/env`，需 root
  - macOS：launchd 两种模式（`install.sh` 按是否 root 分发，模板均含 `@INSTALL_DIR@`/`@CONFIG_DIR@`/`@LOG_DIR@` 占位符，sed 生成）：
    - root（`sudo ./install.sh`）→ 系统级 LaunchDaemon `alidns-ddns-daemon.plist`（另含 `@USERNAME@`）装到 `/Library/LaunchDaemons/`，`UserName=$SUDO_USER`，配置 `/etc/alidns-ddns/config.json`，日志 `/usr/local/var/log/alidns-ddns.log`，Label `system/com.tongtf.alidns-ddns`；**不依赖登录会话，开机即启**，并会清掉用户级残留避免双实例
    - 普通用户 → LaunchAgent `alidns-ddns.plist` 装到 `~/Library/LaunchAgents/`，配置 `~/.config/alidns-ddns/config.json`，日志 `~/Library/Logs/alidns-ddns.log`；域探测 `launchctl print gui/$UID` 成功用 gui，否则回退 user；**模板必须带 `LimitLoadToSessionType` 数组 `[Aqua, Background]`**——默认值 Aqua 在 SSH/无图形会话下 bootstrap 会报 `Bootstrap failed: 5`，仅 Background 又会在图形登录时报 134 拒载
  - macOS 下载不依赖 sudo 的普通用户模式时若已存在系统级 plist 会直接报错退出（防双实例）；`cargo build` 一律以 `SUDO_USER` 身份执行（root 无 cargo，且避免 target/ 属主变 root）
