# ADR 0026：relay 已部署，默认只发布客户端

- 状态：已接受（2026-11-20）
- 相关：[ADR 0013](0013-production-relay-pinning.md)、[ADR 0018](0018-embedded-web-admin.md)、[ADR 0014](0014-multi-vault-invites-management.md)
- 操作指南：[Windows 下编译与部署 Linux ARM64 Relay](../guides/relay-build-deploy.md)
- 部署地址：`https://zh-menghu-relay.odn.cc`

## 背景

用户已在真实服务器上部署 relay，并完成 **TLS 连接验证**。

当前发布物是 Windows 客户端 zip；用户日常只重新编译/运行 app，不重新构建 relay。
过去 relay 与 app 在同一个 workspace 里，改动 `crates/relay`、`crates/sync-proto`、
relay 配置或 schema 时，很容易让服务器端与客户端版本不一致。

## 决策

1. **默认只发布 app**。用户构建/运行：
   ```bash
   cargo build -p menghunote-app --release
   ```
   或使用仓库打出的 Windows zip；不再默认要求用户编译/部署 relay。
2. **relay 程序的变化必须显式通知**。任何触及以下内容的改动，Lead 在交付说明里必须单独列出：
   - `crates/relay`
   - `crates/sync-proto` 的 wire/schema/版本
   - relay 的配置、数据库 migration、管理面/路由、限流/鉴权行为
   - 需要重新 `init`、迁移数据库、改 `admin_allowed_hosts`/`public_url` 的变更
   并给出**是否需要更新 relay、更新步骤、是否需要重启/迁移**的明确结论；不能只给 app zip。
3. **客户端 TLS 仍走系统信任根**，不内置私有 CA、不忽略证书；relay 继续由反向代理终止 TLS。
   真实公网 TLS 已由用户验证；这不等于完成安全审计，仍不因此打生产安全 tag。
4. ADR/交付说明记录部署事实；relay 的具体版本按提交哈希追踪。

## 后果

- 用户默认只更新 app，不会被无关的 relay 改动打扰。
- relay 改动需要额外的发布说明与真机/服务器操作；CI/打包脚本不应把 relay 默认混进客户端 zip。
- 若未来 relay 与客户端 wire 不兼容，必须通过显式版本协商/迁移处理，不能靠"用户顺手一起编译"。
- 本文只记录部署与发布流程决策，不改变 relay 的协议/认证/schema。
