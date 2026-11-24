# alidns-ddns

[![CI](https://github.com/tongtf/alidns-ddns/actions/workflows/ci.yml/badge.svg)](https://github.com/tongtf/alidns-ddns/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)

> **⚠️ 第三方实现 | Third-Party Implementation**
> 本项目为社区维护的第三方工具，**非**阿里云官方项目。与阿里云 DNS 服务的 API 交互基于公开文档实现。

轻量级阿里云 DDNS 工具。**专注阿里云 DNS**，Rust 实现、无异步运行时、单二进制部署。
采用 `observe → diff → apply → schedule` 对账循环，自动获取公网 IP 并更新解析记录。

## 功能特性

- **仅支持阿里云 DNS** — 专注单一服务商，API 调用精简高效
- **依赖极简** — ureq / serde / sha2 / hmac + Linux 下 neli（已在依赖树中）；无 tokio，二进制约 1.8MB（strip + LTO + opt-level=s，不做 UPX 压缩，见下文安全说明）
- 自动探测公网 IPv4（5 个端点回退 + 端点粘性 + 公网性校验，适配大陆网络）
- **IPv6 按 RFC 4862/4941 选取**：Linux 用 netlink 读取 `IFA_FLAGS` + `IFA_CACHEINFO`，macOS 解析 `ifconfig -a -L`；
  区分 preferred / deprecated / tentative / 临时地址，按 `cstamp`（macOS 用 `valid_lft`）取最新；
  **ULA 默认拒绝**（公网不可达），fe80/回环/组播一律排除
- **对账式更新**：按域名批量查询（`PageSize=500` 自动翻页）、精确匹配 `RR + 线路 + 类型`、显式写 TTL，
  IP 未变且未到校验周期时完全不查 DNS
- **状态持久化**：`state.json` 原子落盘，跨重启保留期望值缓存/退避/告警去重/IPv4 端点粘性
- **故障自愈**：错误分类 + 指数退避（30s→5min）+ 鉴权错误退出 78（不再无效重启）+ systemd watchdog
- **IP 变更钩子** `--on-change`：A/AAAA 真正写入后执行外部命令，支持 reload 邮件服务 / 通知 acme.sh
- 支持 A (IPv4) / AAAA (IPv6) / 双栈模式；常驻进程运行
- 三种配置方式：命令行参数、环境变量、配置文件；优先级 CLI > 环境变量 > 配置文件 > 默认值
- ACS3-HMAC-SHA256 V3 签名认证
- 服务支持：Linux systemd / macOS launchd，开机自启
- 支持 Linux（x86_64 / ARM64）与 macOS（Intel / Apple Silicon）
- 零运行时依赖，单二进制部署

## 前置准备

### 1. 购买阿里云域名

1. 登录 [阿里云控制台](https://dns.console.aliyun.com/)
2. 如果还没有域名，前往 [域名注册](https://wanwang.aliyun.com/domain/) 购买
3. 确保域名已在阿里云 DNS 解析管理中（导入或直接购买）

### 2. 创建 AccessKey

1. 登录 [阿里云控制台](https://home.console.aliyun.com/)
2. 鼠标悬停右上角头像 → 点击 **AccessKey 管理**
3. 点击 **创建 AccessKey**
4. 记录 **AccessKey ID** 和 **AccessKey Secret**（Secret 仅创建时显示一次）

> **安全建议**: 按本项目约定，不使用子用户 AccessKey；请为专用账号授予 DNS 管理权限（`alidns:*`），
> 并优先通过环境变量 / systemd `LoadCredential` 传递凭证，不要写进 world-readable 的 `config.json`。

### 3. 配置 DNS 解析（可选）

如果域名尚未添加解析记录，程序会自动创建。你也可以提前在 [DNS 解析控制台](https://dns.console.aliyun.com/) 手动添加：

| 记录类型 | 主机记录 | 记录值 | 说明 |
|---------|---------|--------|------|
| A | `@` | `1.2.3.4` | 将根域名指向 IPv4 |
| AAAA | `@` | `2400:xxxx::1` | 将根域名指向 IPv6 |
| A | `www` | `1.2.3.4` | 将 www 子域名指向 IPv4 |

## 快速开始

### 环境适配检测（可选，推荐先看一眼）

`adapt-install.sh` 检测 OS / systemd 版本 / 是否容器 / 是否有 IPv6，并自动读取**当前已安装的旧配置**
（`/etc/alidns-ddns/config.json`、`env`、systemd 单元里的路径），迁移为最新版配置；
AK/SK 会自动转成 systemd 凭据（优先 `systemd-creds` 加密，失败则 600 明文凭据文件），
不写进 `config.json`。默认只生成不安装，`--apply` 才会安装并重启：

```bash
./adapt-install.sh --dry-run                        # 只看检测结论和迁移来源
./adapt-install.sh --output ./deploy                # 生成到 ./deploy（默认）
./adapt-install.sh --source /old/dir --output ./deploy   # 从指定目录读取旧配置
sudo ./adapt-install.sh --apply --yes               # 直接备份旧文件、安装并重启服务
```

迁移规则：环境变量优先于旧 `config.json`（与运行时优先级一致）；旧键 `DomainName`/`IPv`/
`Ipv6Prefer`/`StateFile` 等自动映射到新格式；旧密钥从 env 或旧 JSON 读取后写入凭据文件。
若读不到 AK/SK，凭证方式自动降级为 `EnvironmentFile` 并留占位符。

加密凭据需要 root 读取 host key：脚本会尝试免密 `sudo -n systemd-creds ...`；
若不可用，会明确提示并回退到 600 明文凭据文件（可用 `sudo ./adapt-install.sh ...` 重跑启用加密）。
生成服务单元时，若脚本同目录没有 `alidns-ddns.service` 模板，会自动改用**已部署的单元文件**作为模板
（macOS 同理回退到已安装的 plist），因此可以只把脚本单独拷到目标机使用。

生成内容按环境不同：

| 环境 | 启动脚本 | 凭证方式 |
|---|---|---|
| Linux + systemd ≥ 247 + systemd-creds ≥ 250 | `alidns-ddns.service`（`LoadCredentialEncrypted`） | 密文凭据文件 |
| Linux + systemd ≥ 247 | `alidns-ddns.service`（`LoadCredential`） | 600 明文凭据文件 |
| Linux + 旧 systemd | `alidns-ddns.service` + 显式 `DDNS_STATE_FILE` | `EnvironmentFile` |
| Linux 无 systemd / 容器 | `run-alidns-ddns.sh`（前台，可交 runit/supervisord） | `EnvironmentFile` |
| macOS | 已替换路径的 launchd plist（root/用户自动判断） | `EnvironmentFile` |

`--apply` 会自动备份被覆盖的配置、重载 systemd 并重启服务；容器环境自动加 `--no-events`。
说明见目录里的 `README.generated.txt`。

### 编译

```bash
cargo build --release
```

### 运行

```bash
# 方式 1: 命令行参数
./alidns-ddns \
  --access-key-id LTAI5xxxxxxx \
  --access-key-secret xxxxSecretxxxx \
  --domain example.com \
  --rr @ \
  --ipv 4 \
  --interval 300

# 方式 2: 环境变量
export ALIBABA_CLOUD_ACCESS_KEY_ID="LTAI5xxxxxxx"
export ALIBABA_CLOUD_ACCESS_KEY_SECRET="xxxxSecretxxxx"
export DDNS_DOMAIN="example.com"
./alidns-ddns

# 方式 3: 配置文件 + 环境变量凭证
cp config.example.json config.json
# 编辑 config.json 填入 domain（密钥只放环境变量或 LoadCredential）
./alidns-ddns -c config.json
```

### 服务部署

`install.sh` 自动检测操作系统（Linux → systemd，macOS → launchd）。

**Linux (systemd):**

```bash
sudo ./install.sh
sudo vim /etc/alidns-ddns/env    # 填入 AccessKey
sudo systemctl start alidns-ddns
sudo systemctl enable alidns-ddns
sudo journalctl -u alidns-ddns -f
```

#### 用 systemd LoadCredential 传递凭证（推荐，systemd ≥ 247）

`config.json` 因为 `DynamicUser` 需要可读而保持 644，不应写入真实 Secret。
更稳妥的做法是用 `LoadCredential` 把 AK/SK 作为独立凭据文件注入：systemd 以 root 读取
源文件并挂载到服务的 `$CREDENTIALS_DIRECTORY`，进程只需读挂载后的文件。

```bash
# 1) 创建 root:root 600 的凭据文件（注意只写值，不要 KEY=VALUE，末尾换行会被自动 trim）
sudo sh -c 'umask 077; printf "%s" "LTAI5xxxxxxx" > /etc/alidns-ddns/access_key_id'
sudo sh -c 'umask 077; printf "%s" "xxxxSecretxxxx" > /etc/alidns-ddns/access_key_secret'

# 2) 用 drop-in 启用 LoadCredential（不要直接改已安装的单元）
sudo mkdir -p /etc/systemd/system/alidns-ddns.service.d
sudo cp examples/alidns-ddns-credentials.conf /etc/systemd/system/alidns-ddns.service.d/credentials.conf

# 3) 生效
sudo systemctl daemon-reload
sudo systemctl restart alidns-ddns
sudo journalctl -u alidns-ddns -n 20 -f
```

程序固定读取两个凭据名：`alidns-access-key-id` 和 `alidns-access-key-secret`；
配置优先级为 **CLI > LoadCredential > 环境变量 > config.json**。
启用后可以从 `/etc/alidns-ddns/env` 中删掉 `ALIBABA_CLOUD_ACCESS_KEY_ID/SECRET`
两行（其余非敏感配置仍可保留在 env 或 config.json 中）。

> 若 systemd 版本较老不支持 `LoadCredential`，请继续使用 `chmod 600` 的 `EnvironmentFile`
> 并在其中设置 `ALIBABA_CLOUD_ACCESS_KEY_ID/SECRET`；密钥不要写进 `config.json`（旧键会报错）。

##### 可选：加密凭据（LoadCredentialEncrypted，systemd ≥ 250）

明文凭据文件虽然只有 600，但仍是磁盘上的明文。若系统支持 `systemd-creds`，
可以把 AK/SK **加密后落盘**：systemd 在启动服务时解密鉴权，再把明文挂载到
`$CREDENTIALS_DIRECTORY/`——**本程序无需任何改动**，仍按同样的路径读取。

密钥选择（`--with-key=`）：

| 取值 | 说明 |
|---|---|
| `host` | 用 `/var/lib/systemd/credential.secret`（root 600）加解密；无 TPM 机器适用 |
| `tpm2` | 只用 TPM2 派生密钥；换主板/清 TPM 后无法解密 |
| `host+tpm2`（默认 `auto`） | 同时绑定机器与系统安装，安全性最高；升级 BIOS/换盘需规划恢复 |
| `null` | 不加密不鉴权，**仅测试用**，生产禁用 |

生成密文（凭据名必须写成下面这两个 ID，systemd-creds 会把名字嵌进密文防改名挪用）：

```bash
sudo systemd-creds setup    # 生成 host key（encrypt 时也会自动生成）

printf '%s' 'LTAI5xxxxxxx' | \
  sudo systemd-creds encrypt --with-key=host --name=alidns-access-key-id - \
  /etc/alidns-ddns/alidns-access-key-id.cred
printf '%s' 'xxxxSecretxxxx' | \
  sudo systemd-creds encrypt --with-key=host --name=alidns-access-key-secret - \
  /etc/alidns-ddns/alidns-access-key-secret.cred
sudo chmod 600 /etc/alidns-ddns/*.cred
```

用加密版 drop-in 替换明文版（**二者只能启用一个**）：

```bash
sudo cp examples/alidns-ddns-credentials-encrypted.conf \
  /etc/systemd/system/alidns-ddns.service.d/credentials.conf
sudo systemctl daemon-reload
sudo systemctl restart alidns-ddns
```

验证：服务能正常进入 API 调用即说明解密成功；如果报"缺少必填项"，说明凭据路径或名字不匹配。

轮换 AK/SK 时重新执行加密命令并 `systemctl restart` 即可。使用 host key 时请备份
`/var/lib/systemd/credential.secret`（它本身是敏感文件）——丢失后旧密文无法解密，只能重新加密。
`LoadCredentialEncrypted=` 从 systemd 247 起可用，`systemd-creds` 工具从 250 起提供。
如果版本较老，退回上面的明文 `LoadCredential` 方案。

**macOS (launchd) — 推荐系统级服务（开机即启，不依赖图形登录）:**

```bash
sudo ./install.sh                                # 编译 + 安装 + 开机自启（以当前用户身份运行）
sudo vim /etc/alidns-ddns/config.json            # 填入 AccessKey
sudo launchctl kickstart -k system/com.tongtf.alidns-ddns   # 重启生效
tail -f /usr/local/var/log/alidns-ddns.log        # 查看日志

# 卸载
sudo launchctl bootout system/com.tongtf.alidns-ddns
sudo rm /Library/LaunchDaemons/com.tongtf.alidns-ddns.plist
```

也可用免 sudo 的用户级服务（`./install.sh`，SSH 下可安装，图形登录后自动加载）：

```bash
./install.sh                                     # 装到 ~/.local/bin，配置 ~/.config/alidns-ddns
launchctl kickstart -k user/$(id -u)/com.tongtf.alidns-ddns
tail -f ~/Library/Logs/alidns-ddns.log

# 卸载
launchctl bootout user/$(id -u)/com.tongtf.alidns-ddns
rm ~/Library/LaunchAgents/com.tongtf.alidns-ddns.plist
```

> 无图形会话时 `gui/$(id -u)` 域不可用，服务会加载到 `user/$(id -u)`；`install.sh` 自动选择并打印实际使用的域。两种模式互斥，已有系统级服务时会拒绝用户级安装（避免双实例）。

安装位置（macOS）：系统级 — 二进制 `/usr/local/bin/alidns-ddns`、配置 `/etc/alidns-ddns/config.json`、日志 `/usr/local/var/log/alidns-ddns.log`；用户级 — `~/.local/bin/alidns-ddns`、`~/.config/alidns-ddns/config.json`、`~/Library/Logs/alidns-ddns.log`。

> IPv6 网卡名 macOS 为 `en0`/`en1`（`--interface en0`），Linux 多为 `eth0`/`enpXsY`；留空自动搜索。

## 配置说明

## 配置说明

配置文件只放**业务配置**，密钥不写入 `config.json`（出现旧键 `AccessKeyID` / `AccessKeySecret` / `StateFile` 会直接报错）。
密钥只从环境变量 `ALIBABA_CLOUD_ACCESS_KEY_*` 或 systemd LoadCredential 提供；
`env` 文件只放凭证和少量部署覆盖。

> **升级到 2.0.3 的用户请注意**：本版本不做旧配置兼容。旧键名（`DomainName`、`RR`、`IPv`、
> `Interval`、`Ipv6Prefer`、`StateFile`、`AccessKeyID` 等）与旧环境变量（`ALIDNS_DOMAIN`、
> `ALIDNS_RR`、`ALIDNS_INTERFACE`、`ALIDNS_IPV6_PREFER`）会被直接拒绝。请按上面的新表改写
> `config.json` 与 `env`；可用 `adapt-install.sh --dry-run` 先查看新格式。

| 参数 | 环境变量 | config.json | 说明 | 默认值 |
|------|---------|-------------|------|--------|
| `--access-key-id` | `ALIBABA_CLOUD_ACCESS_KEY_ID` | —（不支持） | AccessKey ID，也可由 LoadCredential 提供 | 必填 |
| `--access-key-secret` | `ALIBABA_CLOUD_ACCESS_KEY_SECRET` | —（不支持） | AccessKey Secret，也可由 LoadCredential 提供 | 必填 |
| `--domain` | `DDNS_DOMAIN` | `domain` | 域名（多个用逗号分隔） | 必填 |
| `--rr` | `DDNS_RR` | `rr` | 主机记录（多个用逗号分隔） | `@` |
| `--ipv` | `DDNS_IPV` | `ipv` | `4` / `6` / `46` | `6` |
| `--interval` | `DDNS_INTERVAL` | `interval` | 唤醒间隔（秒），1–2592000 | `300` |
| `--verify-interval` | `DDNS_VERIFY_INTERVAL` | `verify_interval` | IP 未变时的 DNS 对账周期（秒） | `21600` |
| `--ttl` | `DDNS_TTL` | `ttl` | 写入记录的 TTL（秒） | `600` |
| `--line` | `DDNS_LINE` | `line` | 解析线路 | `default` |
| `--interface` | `DDNS_INTERFACE` | `interface` | 指定获取 IPv6 的网卡，留空自动 | 空 |
| `--ipv6-prefer` | `DDNS_IPV6_PREFER` | `ipv6_prefer` | `auto` / `stable` / `temporary` | `auto` |
| `--allow-ula` | `DDNS_ALLOW_ULA` | `allow_ula` | 允许把 ULA 写进 AAAA；环境变量可显式 `false` 覆盖文件 | false |
| `--state-file` | `DDNS_STATE_FILE` | —（不支持） | 状态文件路径（部署项） | `$STATE_DIRECTORY/state.json` 等 |
| `--no-state` | `DDNS_NO_STATE` | — | 禁用状态持久化 | false |
| `--on-change` | `DDNS_ON_CHANGE` | `on_change` | IP 变更后执行的命令 | 无 |
| `--on-change-timeout` | `DDNS_ON_CHANGE_TIMEOUT` | `on_change_timeout` | 钩子超时（秒），1–3600 | `60` |
| `--no-events` | `DDNS_NO_EVENTS` | — | 关闭 Linux netlink 地址变更订阅（部署项） | false |
| `--dry-run` | — | — | 只打印计划，不写 API、不执行钩子、不写状态 | false |
| `-v, --verbose` | `DDNS_VERBOSE` | — | 输出候选地址与判定细节 | false |
| `-c, --config` | — | — | 配置文件路径 | `config.json` |

优先级：**CLI > LoadCredential（仅密钥） > 环境变量 > config.json > 默认值**。
布尔值支持三态：环境变量写 `false` 可以覆盖 `config.json` 里的 `true`；未设置才回退。

配置解析失败会**快速失败并退出 78**（不再静默用默认值）；`config.json` 出现未知键
（例如 `Intervall`）也会报错，避免拼写错误被无声忽略。systemd 配置
`RestartPreventExitStatus=78`，避免配置错误导致无限重启刷屏。

### 配置示例

**场景 1: 双栈 DDNS，每 5 分钟更新**

```bash
./alidns-ddns --domain example.com --rr @ --ipv 46 --interval 300
```

效果：同时维护 A 记录（IPv4）和 AAAA 记录（IPv6），每 300 秒检测一次 IP 变化。

**场景 2: 仅 IPv6，子域名**

```bash
./alidns-ddns --domain example.com --rr api --ipv 6
```

效果：仅更新 `api.example.com` 的 AAAA 记录。

**场景 3: 单进程维护多个域名（解析到同一个 IP）**

```bash
./alidns-ddns --domain example.com,example.net --rr @,www --ipv 46
```

或多个域名共用同一个 RR：

```bash
./alidns-ddns --domain example.com,example.net --rr @
```

效果：所有域名共享同一次公网 IP 探测结果，各自更新 / 新建对应的 A 与 AAAA 记录。域名数与 RR 数一对多、多对一或等长时自动配对，否则报错退出。

**场景 4: 高频更新（最小间隔）**

```bash
./alidns-ddns --domain example.com --rr @ --ipv 4 --interval 1
```

效果：每秒检测一次 IP 变化（适用于频繁切换的网络环境）。

**配置文件示例 (`config.json`):**

```json
{
    "domain": "example.com",
    "rr": "@",
    "ipv": "46",
    "interval": 300,
    "verify_interval": 21600,
    "ttl": 600,
    "line": "default",
    "ipv6_prefer": "auto",
    "allow_ula": false,
    "interface": "",
    "on_change": "",
    "on_change_timeout": 60
}
```

## 工作原理

```
        ┌─────────────────────┐
        │ observe()           │  公网 IPv4（HTTP 端点回退 + 公网性校验）
        │                     │  本机 IPv6（netlink / ifconfig -L + 生命周期）
        └──────────┬──────────┘
                   ▼
        ┌─────────────────────┐
        │ plan::diff()        │  纯函数：desired vs DNS actual → 动作
        └──────────┬──────────┘
                   ▼
        ┌─────────────────────┐
        │ apply()             │  幂等 Update/Add + 错误分类
        └──────────┬──────────┘
                   ▼
        ┌─────────────────────┐
        │ schedule()          │  自适应唤醒 / 退避 / netlink 事件
        └─────────────────────┘
```

1. **发现**：IPv4 通过 `api.ipify.org` → `v4.ident.me` → `ip.3322.net` → `ipv4.icanhazip.com` → `checkip.amazonaws.com`
   串行回退（8s 超时、记住上次成功端点）；结果必须是公网地址，私网 / CGNAT / 组播等一律拒绝。
   IPv6 在 Linux 走 netlink `RTM_GETADDR`，在 macOS 走 `ifconfig -a -L`，均带地址状态与生命周期。
2. **对账**：每个域名一次 `DescribeDomainRecords`（`PageSize=500` 自动翻页），按 `RR + Line + Type` 精确匹配；
   期望值未变且未到 `verify-interval` 时跳过查询。同时处理 `Status=Disable` / `Locked` / 重复记录。
3. **写入**：值不同才 `UpdateDomainRecord` / `AddDomainRecord`，显式带 `TTL` 与 `Line`；
   写入成功后记录状态、执行 `--on-change` 钩子。
4. **调度**：默认 300s；刚变更后 60s 内复查；`preferred_lft` 过半前提前唤醒；失败指数退避 30s→5min；
   Linux 订阅 `RTMGRP_IPV6_IFADDR` 地址变更事件，秒级唤醒。
5. **凭证与签名**：ACS3-HMAC-SHA256（V3），签名 Key 为 AccessKeySecret；查询串与头部的百分号编码用大写十六进制。

详细设计见 [`docs/ddns-redesign.md`](docs/ddns-redesign.md)；IPv6 选取依据见
[`docs/ipv6-address-selection.md`](docs/ipv6-address-selection.md)。

## IP 变化钩子（邮件服务器场景）

A/AAAA 真正写入后，`--on-change` 指定的命令会被同步执行（带超时）。可用环境变量：
`DDNS_CHANGED=1`、`DDNS_CHANGED_COUNT`、`DDNS_CHANGED_TARGETS`、`DDNS_IPV4`、`DDNS_IPV6`。
钩子失败只告警，不影响 DNS 与退出码；`--dry-run` 不执行。

> Linux 服务以 `DynamicUser` + 空 capability + `ProtectSystem=strict` 运行，钩子**没有权限直接 reload postfix**。
> 仓库提供了"标志文件 + root path 单元"的完整样例：

```bash
# 1) 安装钩子脚本（在 StateDirectory 里写变更日志并 touch 标志文件）
sudo install -m 0755 examples/ddns-changed.sh /usr/local/bin/ddns-changed

# 2) 在 /etc/alidns-ddns/env 中启用
echo 'DDNS_ON_CHANGE=/usr/local/bin/ddns-changed' | sudo tee -a /etc/alidns-ddns/env

# 3) 安装 root 单元：监听标志文件并 reload 邮件服务
sudo cp examples/alidns-ddns-reload.path examples/alidns-ddns-reload.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now alidns-ddns-reload.path
sudo systemctl restart alidns-ddns
```

标志文件位于 `/var/lib/private/alidns-ddns/ip-changed.flag`（`StateDirectory` 在 DynamicUser 下的实际路径），
变更日志在同目录的 `ip-change.log`。按需修改 `alidns-ddns-reload.service` 中的 `ExecStart`。

钩子继承服务进程的环境变量（含 `ALIBABA_CLOUD_ACCESS_KEY_SECRET`，便于 acme.sh 复用），只应配置可信命令。

## 文件结构

```
alidns-ddns/
├── src/
│   ├── main.rs               # 主循环、sd_notify、调度、钩子触发
│   ├── config.rs             # 参数 / 环境变量 / 配置文件解析与校验
│   ├── plan.rs               # 纯函数：期望值 vs DNS 记录 → 动作
│   ├── select.rs             # 纯函数：IPv6 地址选取
│   ├── state.rs              # 状态缓存 / 退避 / 原子落盘
│   ├── hook.rs               # IP 变更钩子（带超时）
│   ├── log.rs / util.rs      # 日志、UTF-8 安全截断、公网地址判定
│   ├── discover/             # 公网 IPv4 探测 + IPv6 枚举（Linux netlink / macOS ifconfig）
│   └── aliyun/               # ACS3 签名、响应模型、分页查询与错误分类
├── docs/                     # 重构方案与 IPv6 选取规范
├── examples/                 # on-change 钩子 + root reload 单元样例
├── Cargo.toml
├── config.example.json / env.example
├── alidns-ddns.service
├── alidns-ddns.plist / alidns-ddns-daemon.plist   # macOS launchd 模板
├── install.sh                # 自动检测 Linux / macOS，root/普通用户分模式
├── LICENSE / CONTRIBUTING.md / SECURITY.md
```

## 安全说明

- **不要对二进制做 UPX 压缩**：UPX 运行时需要 `mprotect(PROT_READ|PROT_WRITE|PROT_EXEC)` 解压自身，
  与 systemd 单元的 `MemoryDenyWriteExecute=yes`（W^X 加固）直接冲突，会导致服务启动即崩。
  发布包因此不使用 UPX；约 900KB 的体积收益不值得牺牲这条缓解措施。

- **切勿**将包含真实 AccessKey 的 `config.json` 提交到版本控制
- 按项目约定**不使用子用户 AccessKey**；为专用账号授予 DNS 管理权限（`alidns:*`）
- 生产环境 Linux 优先用 `EnvironmentFile`（600）或 systemd `LoadCredential` 传递凭证；
  `config.json` 因 DynamicUser 需要可读而保持 644，因此不应写入真实 Secret
- `--on-change` 命令继承环境变量，只应配置可信脚本

## 贡献

欢迎贡献！请查看 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

本项目采用 [Apache License 2.0](LICENSE) 许可证。

---

**免责声明**: 本项目为社区维护的第三方工具，与阿里云无关。使用阿里云服务需遵守其[服务条款](https://www.aliyun.com/agreement)。
