//! sync —— 同步/加密 v1 的密钥与设备身份层（见 `docs/adr/0006-sync-v1.md`）。
//!
//! 本 crate 目前只实现 ADR 0006 §11 的**第 2 步**（密钥与设备身份）：
//!
//! - 根密钥 / 口令 KEK / 恢复码（Argon2id + HKDF，Bech32m `mnrec`）；
//! - 密钥槽（`sync-proto` 的 `SlotDescriptorV1`）、wrap/unwrap、改口令纯逻辑；
//! - 本地身份 BLOB 封装（[`identity_wrap`]）：Windows DPAPI（CurrentUser），
//!   Linux 用户主密钥文件 + XChaCha20-Poly1305（ADR 0008）；
//! - 设备 Ed25519/X25519 身份、`DeviceCertV1`（`sync-proto` 类型）、独立管理密钥、
//!   邀请令牌、设备指纹；
//! - HPKE（RFC 9180 base mode：X25519-HKDF-SHA256 / HKDF-SHA256 / ChaCha20Poly1305）。
//!
//! 边界（ADR §11）：不做网络、不做 SQLite、不碰 GPUI。**wire 类型与 CDE 编码的真源是
//! `menghunote-sync-proto`**（header/AAD/记录/KDF 联合体/域常量），本 crate 不另写一套；
//! 它只负责"把待编码字节算成 SHA/签名/HPKE/AEAD"，以及本 crate 私有的本地结构
//! （本地身份 BLOB、HPKE grant AAD）经同一 CDE 编码器编码。
//!
//! **秘密纪律**：口令、根、恢复种子、私钥、Linux 用户主密钥一律包在
//! [`keys::Secret32`] / `Zeroizing` 里，这些类型不实现 `Debug`/`Display`；错误类型只带
//! 定位信息，不带秘密。
//!
//! 平台（[`identity_wrap`] 分派）：Windows 用 DPAPI（ADR 0006 §3）；Linux 用用户主密钥
//! 文件（ADR 0008，安全性弱于 DPAPI）；其它平台编译通过但调用明确返回
//! [`error::SyncError::LocalWrapUnsupportedPlatform`]，**不假装成功、不回退明文**。

// 除 DPAPI 的 Win32 FFI（`dpapi.rs` 内 `#[allow(unsafe_code)]`）外，本 crate 不用 unsafe。
#![deny(unsafe_code)]

pub mod api_version;
pub mod approval_v1;
pub(crate) mod asset_sync;
pub mod base64url;
pub mod bootstrap;
pub mod catchup;
pub mod cbor;
pub mod checkpoint;
pub mod control_pin;
pub mod create_link;
pub mod dev_local;
pub mod dev_local_rotation;
pub mod device;
pub mod device_management;
pub mod domain;
pub mod dpapi;
pub mod engine;
pub mod error;
pub mod genesis;
pub mod hpke;
pub mod http_relay;
pub mod http_transport;
pub mod identity;
pub mod identity_wrap;
pub mod ids;
pub mod init_journal;
pub mod init_session;
pub mod invite_link;
pub mod kdf;
pub mod keys;
pub mod management_v2;
pub mod random;
pub mod recovery;
pub mod recovery_enroll;
pub mod relay;
pub mod relay_context;
pub mod relay_policy;
pub mod remote_init;
pub mod rotation;
pub mod rotation_activate;
pub mod rotation_apply;
pub mod rotation_journal;
pub mod rotation_repack;
pub mod rotation_restart;
pub mod rotation_slots;
pub mod rotation_stage;
pub mod slot;
pub mod storage_view;
pub mod upload;
pub mod wire;

#[cfg(test)]
mod test_support;

pub use error::SyncError;

// 复用 wire 真源的标识符类型（避免各 crate 各定义一份）。
pub use menghunote_sync_proto::{DeviceId, ObjectId, VaultId};
