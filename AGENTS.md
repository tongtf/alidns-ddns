# AGENTS.md

## 项目概述

阿里云DDNS工具：自动获取公网IPv4/IPv6并更新域名解析记录。单文件入口 `src/main.rs`（~311行）。

## 配置优先级

CLI参数（clap） > 环境变量 > `config.json` > 默认值

```bash
# CLI 参数（所有参数都支持 --env 从环境变量读取）
./alidns-ddns --access-key-id AKID --access-key-secret SECRET --domain example.com --rr @ --ipv 4 --interval 300

# 等价环境变量
ALIBABA_CLOUD_ACCESS_KEY_ID / ALIBABA_CLOUD_ACCESS_KEY_SECRET
ALIDNS_DOMAIN / ALIDNS_RR（默认 @）
DDNS_IPV（4/6/46，默认 4）
DDNS_INTERVAL（秒，默认 300，最小 1）

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

1. 每 N 秒获取 IPv4（`api.ipify.org`）和 IPv6（本地网卡，`local-ip-address` 库）
2. 调用阿里云 DNS API（`DescribeDomainRecords`/`UpdateDomainRecord`/`AddDomainRecord`）
3. IP 变化时自动更新 / 新建 A / AAAA 记录

API 签名为手写 **ACS3-HMAC-SHA256（V3 签名）**，签名 Key 为 `AccessKeySecret`（不附加 `&`）。参数通过 HTTP Header（`Authorization`/`x-acs-*`）传递，非 Query 参数。

## 注意事项

- `config.json` 含占位凭证，**勿提交真实 Key 到仓库**
- 需要阿里云 DNS 管理权限（`alidns:*`）
- 不要使用子用户 AccessKey
- systemd 服务：`alidns-ddns.service` + `EnvironmentFile=/etc/alidns-ddns/env`
