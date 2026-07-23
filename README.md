# alidns-ddns

[![CI](https://github.com/tongtf/alidns-ddns/actions/workflows/ci.yml/badge.svg)](https://github.com/tongtf/alidns-ddns/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)

> **⚠️ 第三方实现 | Third-Party Implementation**
> 本项目为社区维护的第三方工具，**非**阿里云官方项目。与阿里云 DNS 服务的 API 交互基于公开文档实现。

阿里云域名动态解析 (DDNS) 工具。自动获取公网 IPv4/IPv6 地址并更新阿里云 DNS 解析记录，支持双栈。

## 功能特性

- 自动检测公网 IPv4（通过 [ipify](https://www.ipify.org/)）和 IPv6（本地网卡）
- 支持 A (IPv4) / AAAA (IPv6) / 双栈模式
- 三种配置方式：命令行参数、环境变量、配置文件
- 配置优先级：CLI > 环境变量 > 配置文件 > 默认值
- ACS3-HMAC-SHA256 V3 签名认证
- systemd 服务支持，开机自启
- 单文件部署，二进制约 1MB（可 UPX 压缩）

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
  --access-key-id YOUR_KEY \
  --access-key-secret YOUR_SECRET \
  --domain example.com \
  --rr @ \
  --ipv 4 \
  --interval 300

# 方式 2: 环境变量
export ALIBABA_CLOUD_ACCESS_KEY_ID="your-key"
export ALIBABA_CLOUD_ACCESS_KEY_SECRET="your-secret"
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
| `--domain` | `ALIDNS_DOMAIN` | 域名 | 必填 |
| `--rr` | `ALIDNS_RR` | 主机记录 | `@` |
| `--ipv` | `DDNS_IPV` | IP 模式: `4`/`6`/`46` | `4` |
| `--interval` | `DDNS_INTERVAL` | 更新间隔（秒） | `300` |
| `-c, --config` | — | 配置文件路径 | `config.json` |

## 工作原理

1. 每隔 N 秒，通过 [ipify API](https://api.ipify.org/) 获取当前公网 IPv4 地址
2. 通过本地网卡枚举获取 IPv6 地址（使用 [`local-ip-address`](https://crates.io/crates/local-ip-address)）
3. 调用阿里云 DNS API（`DescribeDomainRecords`）查询现有记录
4. 对比 IP 是否变化，变化时调用 `UpdateDomainRecord` 或 `AddDomainRecord` 更新
5. API 认证采用 ACS3-HMAC-SHA256 签名（阿里云 V3 签名机制）

## 文件结构

```
alidns-ddns/
├── src/main.rs           # 主程序（单文件）
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
- 推荐使用 RAM 子账号，仅授予 `alidns:*` 权限
- 生产环境建议使用 `EnvironmentFile` 管理凭证

## 贡献

欢迎贡献！请查看 [CONTRIBUTING.md](CONTRIBUTING.md)。

## 许可证

本项目采用 [Apache License 2.0](LICENSE) 许可证。

---

**免责声明**: 本项目为社区维护的第三方工具，与阿里云无关。使用阿里云服务需遵守其[服务条款](https://www.aliyun.com/agreement)。
