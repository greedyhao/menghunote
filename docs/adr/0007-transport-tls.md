# ADR 0007 — 传输层 TLS 与 relay 明文门禁

- 状态：已采纳（2026-11-12）
- 关联：ADR 0006 §5/§28、评审 0002「生产阻断：客户端只有 HTTP」
- 实现：`crates/sync/src/http_transport.rs`、`crates/sync/tests/tls_transport.rs`、`crates/relay/src/config.rs`

## 背景

v1 开发期客户端只用 `StdTcpTransport` 明文 HTTP，且接受任意 HTTP host。E2EE 保护正文，但不保护 bearer token、控制面/设备元数据与可用性；
"反代开 TLS"不能替代客户端证书/hostname 校验，也不能阻止降级。生产前必须有真实 TLS 客户端与 relay 明文门禁。

## 决策

### 客户端

- **rustls 0.23**（ring 后端、default-features=false、TLS1.2/1.3），不用 native-tls/openssl（系统依赖、schannel 差异、交叉编译成本），
  不用 webpki-roots（根快照滞后、无法注入私有 CA）。
- 信任根：`rustls-native-certs` 系统证书存储 + 可选 `with_extra_roots` 注入私有 CA（测试用）。
- 校验：证书链、hostname（SNI/DNS SAN 或 IP SAN）、有效期；无客户端证书。
- **明文默认拒绝**：仅 loopback（127.0.0.0/8、::1、localhost）且显式 `MENGHUNOTE_ALLOW_PLAINTEXT_HTTP=1` 或
  `TransportPolicy::localhost_development()` / `new_local_dev()`；非 loopback 任何策略下拒绝。
- **重定向**：只跟随同 origin（scheme/host/port 全等），最多 4 跳；跨 origin/跨端口/协议相对/`https→http` 降级/缺 Location/控制字符
  → `TransportPolicy` 拒绝；Authorization 不跨 origin；307/308 保留方法，303（及 301/302 的 POST）降 GET。
- HTTP/1.1（无 h2）。

### relay

- relay **不内置 TLS**，生产必须由反向代理终止 TLS（HTTP/1.1 上游；代理需开 TLS1.2+）。
- 默认只允许 loopback 明文绑定；非 loopback 且未显式 `allow_plaintext = true` → **启动前拒绝**并打印 nginx/Caddy 示例；
  显式允许时 stderr 警告。
- E2EE 信封/控制面 wire 零改动。

## 后果

- 开发脚本对 `http://127.0.0.1` 继续可用，但需要显式开发策略（examples 已用 `new_local_dev`）。
- 企业 MITM 根证书会按系统信任语义被信任（native roots 的预期行为，不是漏洞）。
- 系统证书存储为空/损坏时首次 https 返回清晰 `Tls` 错误（不可重试）。
- `ring` 的 C 源在本机 build-script 上下文带 `-Z7` 会触发 `D8050`，与既有 `libsqlite3-sys`/`stacker`/`zstd-sys` 同因；
  已在根 `Cargo.toml` 加 `[profile.dev.package.ring] debug = false`（AGENTS §6 的既有做法），换到可写 `%TEMP%` 环境后可删。
- 依赖新增：生产 `rustls` + `rustls-native-certs`；测试 dev-dep `rcgen`（自签 CA/叶子证书），不进生产。
