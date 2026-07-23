# 安全策略

## 报告漏洞

如果你发现了安全漏洞，请**不要**通过公开的 GitHub Issues 报告。

请通过以下方式联系维护者：

- 发送邮件至项目维护者（通过 GitHub 个人资料获取联系方式）
- 在 GitHub 上发起 Private Vulnerability Disclosure

请提供以下信息：

- 漏洞描述
- 复现步骤
- 潜在影响
- 建议的修复方案（如有）

## 处理流程

1. 确认漏洞报告
2. 评估影响范围
3. 开发修复
4. 发布安全更新
5. 公开披露（如适用）

## 安全最佳实践

- **不要**将 AccessKey 提交到版本控制
- 使用 RAM 子账号，仅授予最小必要权限
- 定期轮换 AccessKey
- 使用 systemd 的 `EnvironmentFile` 管理凭证
