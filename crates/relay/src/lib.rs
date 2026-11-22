//! menghunote relay —— 自建**密文中继**（ADR 0006 §5/§6/§7/§8/§13）。
//!
//! 定位：一个 Rust 进程 / 一个 vault / 一个 SQLite（WAL + FULL）：
//! - relay 只见密文：不解析 CDE 载荷、不运行 Argon/Loro/AEAD，不做搜索与因果合并；
//! - 客户端自己保根、自己验对方、自己证明水位；`key_epoch` 是安全边界，`generation` 是数据边界；
//! - 依赖边界（§6/§11）：`relay → sync-proto` + rusqlite/HTTP/公钥签名，**不依赖** storage/Loro/
//!   Tantivy/GPUI。
//!
//! 模块地图：
//! | 模块 | 职责 |
//! | --- | --- |
//! | [`db`] | 单有界 writer 线程 + 只读连接池 + DDL/PRAGMA |
//! | [`cursor`] | 89 字节不可伪造游标（HMAC） |
//! | [`quota`] | `Q`/`R` 预算与维护 reservation（逐片原子扣减） |
//! | [`auth`] | challenge/token/管理签名钩子 |
//! | [`objects`] | 分片上传 → complete → log → 幂等 ACK |
//! | [`checkpoints`] | writer lease、固定 S、refs、publish、两基线、bootstrap lease |
//! | [`gc`] | 7 天持久计划 + §13.3 删除前摘要校验 |
//! | [`rotations`] | freeze → 候选 epoch → staged grants → atomic activate |
//! | [`verify`] | 真实验签：`sync-proto` 解 CDE + relay 算 SHA/Ed25519 |
//! | [`wire`] | 控制面 `application/cbor` + CDE 记录适配 |
//! | [`pairing`] | 一次性邀请 / 设备批准（PENDING → ACTIVE） |
//! | [`slots`] | key slot 登记、`slot_set_rev` CAS 激活 |
//!
//! 服务端**不写业务日志**（canary 审计要求：日志里不得出现正文/口令/root/真实资产 hash）；
//! 诊断只暴露计数与不透明 ID。

pub mod api;
pub mod auth;
pub mod checkpoints;
pub mod config;
pub mod cursor;
pub mod db;
pub mod envelope;
pub mod error;
pub mod gc;
pub mod ids;
pub mod log;
pub mod objects;
pub mod pairing;
pub mod quota;
pub mod refs;
pub mod rotations;
pub mod sig;
pub mod slots;
pub mod time;
pub mod vault;
pub mod verify;
pub mod wire;

pub use config::RelayConfig;
pub use db::{Db, DbConfig};
pub use error::{ApiError, Error, ErrorCode, Result};
pub use ids::{Hash32, Id16};
pub use menghunote_sync_proto as proto;
pub use sig::{Expectation, SignDomain, SignatureCheck, SignatureHook};
pub use time::{Clock, FakeClock, SystemClock};
pub use verify::Ed25519Hook;
