# `menghunote-relay` —— 自建加密中继（ADR 0006）

单实例服务端：只存 MNE1 密文信封、日志与控制链，**不解密、不见密钥、不做搜索**。
本 crate 提供：

- `menghunote-relay` 二进制（生产部署 + 管理 CLI）
- 库 `menghunote_relay`（axum 路由、SQLite 存储、验签、配额/GC）

> 生产二进制**强制真实 Ed25519 验签**（`relay::verify::Ed25519Hook`），没有任何放行/注入
> 开关。测试与本地联调请用 `examples/dev_server.rs`（**DEV ONLY**，会醒目打印告警）。

## 安装与构建

```bash
cargo build -p menghunote-relay --release --bin menghunote-relay
```

产物：`target/release/menghunote-relay`。

## 两段式启动（推荐）

### 1. 初始化（生成一次性 setup secret）

```bash
menghunote-relay init --db /var/lib/menghunote/relay.sqlite3
# db=/var/lib/menghunote/relay.sqlite3
# instance_id=...
# setup_secret=<43 字符 base64url，只打印这一次>
# setup_secret_sha256=...
```

`init` 只做：建库、schema 迁移、把 `SHA256(setup secret)` 落库（不存明文）、打印 secret。
**不启网络**。重复执行会报错；重置必须先 `delete-vault`。

### 2. 启动服务

```bash
menghunote-relay serve --config /etc/menghunote/relay.toml
```

配置文件（扁平 TOML，未知字段直接报错）：

```toml
listen = "127.0.0.1:18080"                 # 生产通常放在反代后面
db = "/var/lib/menghunote/relay.sqlite3"   # 必须绝对路径
normal_limit = 4294967296                  # Q，默认 4GiB
maintenance_limit = 8589934592             # 峰值 Q+R，必须 > normal_limit，默认 2Q
cursor_key = "0123…（64 位十六进制 = 32B）" # 可选；缺省在 genesis 时随机生成并落库
# cursor_key_path = "cursor.key"           # 可选；相对路径按配置文件目录解析
max_snapshot_bytes = 67108864              # 可选；SNAPSHOT 对象明文上限，默认 64MiB
```

`cursor_key` 与 `cursor_key_path` 只能给一个。`db` 相对路径会被拒绝（会随 cwd 漂移）。

启动后设备用 `setup_secret` 调 `POST /v1/vaults` 完成 genesis；genesis 响应**键 5**是
初始游标（seq=0），客户端应写入 `sync.relay_cursor`。`setup_secret` 用完即废弃。

`Ctrl+C` / `SIGTERM` 触发优雅退出（停止接入 → 等 writer 线程收尾）。

## 运维

```bash
# 机器可读 key=value：state/epoch/control_rev/head_seq/gc_floor/两份 checkpoint/配额/设备数
menghunote-relay status --db /var/lib/menghunote/relay.sqlite3

# 永久删除（需 --yes + 手动输入 DELETE；relay 没有 HTTP 删除接口）
menghunote-relay delete-vault --db /var/lib/menghunote/relay.sqlite3 --yes
```

`status` 在 genesis 前也能跑：`state=uninitialized`、`devices=0`。

## 安全边界

- 服务端只见密文：MNE1 header + AEAD 密文；对象类型/分片/水位是明文元数据。
- 验签：设备 ObjectCommit/Checkpoint/Challenge、管理控制链、§17.1 设备证书全部走真实
  Ed25519（`relay::verify::Ed25519Hook`）。
- 请求日志只打 `method path -> status`，不含 body/query/header；不打印任何 token/密文。
- setup secret 只落 SHA256；`init` 打印一次，之后 relay 不再持有明文。
- `cursor_key` 是游标 HMAC key：多实例共享同一 DB 时不需要额外配置（落库），
  迁移/灾备时可用配置文件显式固定。

## 本地端到端联调

用 dev server（**DEV ONLY**，放行验签，会打印种子，勿暴露）：

```bash
cargo run -p menghunote-relay --example dev_server -- --reset --setup-only
# 默认 127.0.0.1:18080；--listen/--db 可覆盖；每个用例建议 --reset 或独立 --db 目录
```
