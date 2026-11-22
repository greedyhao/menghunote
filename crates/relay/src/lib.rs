//! menghunote relay —— 自建**密文中继**（ADR 0006 §5/§6/§7/§8/§13）。
//!
//! 定位：一个 Rust 进程 / 一个 SQLite（WAL + FULL）/ **同库多 vault**（schema 10，ADR 0014 §A）：
//! - relay 只见密文：不解析 CDE 载荷、不运行 Argon/Loro/AEAD，不做搜索与因果合并；
//! - 客户端自己保根、自己验对方、自己证明水位；`key_epoch` 是安全边界，`generation` 是数据边界；
//! - 依赖边界（§6/§11）：`relay → sync-proto` + rusqlite/HTTP/公钥签名，**不依赖** storage/Loro/
//!   Tantivy/GPUI。
//!
//! 模块地图：
//! | 模块 | 职责 |
//! | --- | --- |
//! | [`admin`] | 创建票据预留（Web 管理面「创建领取链接」与测试夹具；`/v2/vaults` 的一次性凭证）|
//! | [`admin_web`] | 内嵌 Web 管理面（ADR 0018 S1）：独立 listener/admin token/session/CSRF/Host 白名单 |
//! | [`ratelimit`] | v2 auth/create/data 的有界入口限流（固定窗口，不引新依赖）|
//! | [`registry`] | vault 生命周期权威（schema 13）：预留/claim/cancel 原子、只读列表/详情 |
//! | [`deletion`] | 在线不可逆删除作业（ADR 0018 S3）：admission fence、WS 关闭、分批 purge、崩溃恢复 |
//! | [`db`] | 单有界 writer 线程 + 只读连接池 + DDL/PRAGMA（`ScopeConn` 携带显式租户作用域）|
//! | [`scope`] | 显式 `VaultScope`/`ScopeConn`：禁止隐式默认 vault、按 subject 猜租户 |
//! | [`cursor`] | 89 字节不可伪造游标（HMAC） |
//! | [`quota`] | `Q`/`R` 预算与维护 reservation（逐片原子扣减） |
//! | [`auth`] | challenge/token/管理签名钩子 |
//! | [`objects`] | 分片上传 → complete → log → 幂等 ACK |
//! | [`checkpoints`] | writer lease、固定 S、refs、publish、两基线、bootstrap lease |
//! | [`gc`] | 7 天持久计划 + §13.3 删除前摘要校验 |
//! | [`rotations`] | freeze → 候选 epoch → staged grants → atomic activate |
//! | [`control`] | 控制链 typed 记录（`SignedControlRecord` + `epoch.activated` 事件）|
//! | [`approvals`] | ADR 0016 S4d2 审批收件箱（通道 B）：半签包存储、list/read/加签/取消、惰性终态 |
//! | [`management`] | §26：管理写只解析唯一 canonical body + `request_id` 幂等 |
//! | [`verify`] | 真实验签：`sync-proto` 解 CDE + relay 算 SHA/Ed25519 |
//! | [`wire`] | 控制面 `application/cbor` + CDE 记录适配 |
//! | [`pairing`] | 一次性邀请 / 设备批准（PENDING → ACTIVE） |
//! | [`revocation`] | 设备访问吊销（只推进 `auth_rev`，不动控制链/证书；ADR 0010）|
//! | [`slots`] | key slot 登记、`slot_set_rev` CAS 激活 |
//! | [`ticket_link`] | Web 管理面创建领取链接的 `mn-pair://create` 渲染（与 sync 逐字节一致）|
//!
//! 服务端**不写业务日志**（canary 审计要求：日志里不得出现正文/口令/root/真实资产 hash）；
//! 诊断只暴露计数与不透明 ID。

pub mod admin;
pub mod admin_web;
pub mod api;
pub mod approvals;
pub mod auth;
pub mod authz;
pub mod checkpoints;
pub mod config;
pub mod control;
pub mod control_v2;
pub mod cursor;
pub mod db;
pub mod deletion;
pub mod envelope;
pub mod error;
pub mod gc;
pub mod ids;
pub mod log;
pub mod management;
pub mod objects;
pub mod pairing;
pub mod quota;
pub mod ratelimit;
pub mod refs;
pub mod registry;
pub mod revocation;
pub mod rotations;
pub mod scope;
pub mod sig;
pub mod slots;
pub mod ticket_link;
pub mod time;
pub mod vault;
pub mod verify;
pub mod wire;

pub use config::RelayConfig;
pub use db::{Db, DbConfig};
pub use error::{ApiError, Error, ErrorCode, Result};
pub use ids::{Hash32, Id16};
pub use menghunote_sync_proto as proto;
pub use scope::{ScopeConn, VaultScope};
pub use sig::{Expectation, SignDomain, SignatureCheck, SignatureHook};
pub use time::{Clock, FakeClock, SystemClock};
pub use verify::Ed25519Hook;
