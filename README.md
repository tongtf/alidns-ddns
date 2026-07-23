# alidns-ddns

[![CI](https://github.com/tongtf/alidns-ddns/actions/workflows/ci.yml/badge.svg)](https://github.com/tongtf/alidns-ddns/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)

> **⚠️ 第三方实现 | Third-Party Implementation**
> 本项目为社区维护的第三方工具，**非**阿里云官方项目。与阿里云 DNS 服务的 API 交互基于公开文档实现。

轻量级阿里云 DDNS 工具。**专注阿里云 DNS**，单文件 Rust 实现（~400 行），自动获取公网 IP 并更新解析记录。

## 功能特性

- **仅支持阿里云 DNS** — 专注单一服务商，API 调用精简高效
- **极致精简** — 单文件 ~400 行 Rust 代码，无多余抽象，编译后二进制约 1MB
- 自动检测公网 IPv4（通过 [ipify](https://www.ipify.org/)）和 IPv6（本地网卡）
- 支持 A (IPv4) / AAAA (IPv6) / 双栈模式
- 三种配置方式：命令行参数、环境变量、配置文件
- 配置优先级：CLI > 环境变量 > 配置文件 > 默认值
- ACS3-HMAC-SHA256 V3 签名认证
- systemd 服务支持，开机自启
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

> **安全建议**: 推荐使用 RAM 子用户的 AccessKey，仅授予 `AliyunDNSFullAccess` 权限，避免使用主账号 AK。

### 3. 配置 DNS 解析（可选）

如果域名尚未添加解析记录，程序会自动创建。你也可以提前在 [DNS 解析控制台](https://dns.console.aliyun.com/) 手动添加：

| 记录类型 | 主机记录 | 记录值 | 说明 |
|---------|---------|--------|------|
| A | `@` | `1.2.3.4` | 将根域名指向 IPv4 |
| AAAA | `@` | `2400:xxxx::1` | 将根域名指向 IPv6 |
| A | `www` | `1.2.3.4` | 将 www 子域名指向 IPv4 |

## 快速开始

### 编译

```bash
cargo build --release
# 可选: UPX 压缩
upx --best target/release/alidns-ddns
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
export ALIDNS_DOMAIN="example.com"
./alidns-ddns

# 方式 3: 配置文件
cp config.example.json config.json
# 编辑填入你的凭证
./alidns-ddns -c config.json
```

### systemd 部署

```bash
sudo ./install.sh
sudo vim /etc/alidns-ddns/env    # 填入 AccessKey
sudo systemctl start alidns-ddns
sudo systemctl enable alidns-ddns
sudo journalctl -u alidns-ddns -f
```

## 配置说明

| 参数 | 环境变量 | 说明 | 默认值 |
|------|---------|------|--------|
| `--access-key-id` | `ALIBABA_CLOUD_ACCESS_KEY_ID` | AccessKey ID | 必填 |
| `--access-key-secret` | `ALIBABA_CLOUD_ACCESS_KEY_SECRET` | AccessKey Secret | 必填 |
| `--domain` | `ALIDNS_DOMAIN` | 域名（如 `example.com`） | 必填 |
| `--rr` | `ALIDNS_RR` | 主机记录（如 `@`、`www`、`api`） | `@` |
| `--ipv` | `DDNS_IPV` | IP 模式：`4`=仅IPv4 / `6`=仅IPv6 / `46`=双栈 | `4` |
| `--interval` | `DDNS_INTERVAL` | 更新间隔（秒），最小 1 | `300` |
| `-c, --config` | — | 配置文件路径 | `config.json` |

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

**场景 3: 高频更新（最小间隔）**

```bash
./alidns-ddns --domain example.com --rr @ --ipv 4 --interval 1
```

效果：每秒检测一次 IP 变化（适用于频繁切换的网络环境）。

**配置文件示例 (`config.json`):**

```json
{
    "AccessKeyID": "LTAI5xxxxxxx",
    "AccessKeySecret": "xxxxSecretxxxx",
    "DomainName": "example.com",
    "RR": "@",
    "IPv": "46",
    "Interval": 300
}
```

## 工作原理

```
┌─────────────┐     ┌──────────────┐     ┌─────────────────┐
│  ipify API  │────▶│  alidns-ddns │────▶│  阿里云 DNS API  │
│  (IPv4)     │     │              │     │  (Update/Add)   │
└─────────────┘     │  每 N 秒循环  │     └─────────────────┘
                    │              │
┌─────────────┐     │  检测 IP 变化 │
│  本地网卡    │────▶│              │
│  (IPv6)     │     └──────────────┘
└─────────────┘
```

1. 每隔 N 秒，通过 [ipify API](https://api.ipify.org/) 获取当前公网 IPv4 地址
2. 通过本地网卡枚举获取 IPv6 地址（使用 [`local-ip-address`](https://crates.io/crates/local-ip-address)）
3. 调用阿里云 DNS API（`DescribeDomainRecords`）查询现有记录
4. 对比 IP 是否变化，变化时调用 `UpdateDomainRecord` 或 `AddDomainRecord` 更新
5. API 认证采用 ACS3-HMAC-SHA256 签名（阿里云 V3 签名机制）

## 文件结构

```
alidns-ddns/
├── src/main.rs           # 主程序（单文件，~400 行）
├── Cargo.toml            # 项目配置
├── config.example.json   # 配置文件模板
├── env.example           # 环境变量模板
├── alidns-ddns.service   # systemd 服务文件
├── install.sh            # 安装脚本
├── LICENSE               # Apache-2.0 许可证
├── CONTRIBUTING.md       # 贡献指南
└── SECURITY.md           # 安全策略
```

## 安全说明

- **切勿**将包含真实 AccessKey 的 `config.json` 提交到版本控制
- 推荐使用 RAM 子账号，仅授予 `AliyunDNSFullAccess` 权限
- 生产环境建议使用 `EnvironmentFile` 管理凭证

## 贡献

欢迎贡献！请查看 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

本项目采用 [Apache License 2.0](LICENSE) 许可证。

---

**免责声明**: 本项目为社区维护的第三方工具，与阿里云无关。使用阿里云服务需遵守其[服务条款](https://www.aliyun.com/agreement)。
