//! relay 集成测试公共夹具。
//!
//! 约定：
//! - 每个用例一个**独立且唯一**的 SQLite 文件（`CARGO_TARGET_TMPDIR/relay-it-<pid>-<counter>-<熵>/`），
//!   同进程并行、多 test binary 并发、以及**系统复用 pid 后的重复运行**都不冲突；
//! - 夹具在 `Drop` 时删除自己的临时目录（需要重开同一库的用例显式 `into_db_path()`）；
//! - 时间一律 `FakeClock`，**不 sleep**（ADR 0006 §9）；
//! - 测试用 `TestHook` 只做"摘要相等"的弱校验（真实验签在 `relay::verify` 的单元测试里）；
//! - 全部走公开 API（`Db::write_blocking` + 领域模块），除"模拟崩溃点"外不直接改表。

#![allow(dead_code)]

pub mod v2;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub use menghunote_relay as relay;

use ed25519_dalek::{Signer, SigningKey};
use menghunote_sync_proto as proto;
use rusqlite::OptionalExtension;

use relay::auth::{self, AuthContext, Realm};
use relay::checkpoints;
use relay::config::RelayConfig;
use relay::db::Db;
use relay::envelope::{self, Header, ObjectType};
use relay::error::{Error, Result};
use relay::ids::{self, sha256, Hash32, Id16};
use relay::objects;
use relay::quota;
use relay::ratelimit::RateLimitSpec;
use relay::refs::RefRole;
use relay::rotations;
use relay::scope::{ScopeConn, VaultScope};
use relay::sig::{Expectation, SignDomain, SignatureCheck, SignatureHook};
use relay::slots::SlotKind;
use relay::time::{Clock, FakeClock, SystemClock};
use relay::vault::{self, Vault};
use relay::wire;

/// 固定基准时刻（2027-01-15），所有 TTL 都用 FakeClock 推进。
pub const BASE_TIME: i64 = 1_800_000_000;

/// relay 的对象引用元数据 → §15 公开 refs（`refs_digest` 的输入）。
fn proto_ref(meta: &relay::refs::ObjectRefMeta) -> proto::ObjectRefV1 {
    proto::ObjectRefV1 {
        object_id: meta.object_id,
        object_type: proto::ObjectType::try_from(meta.object_type.as_u8()).expect("object type"),
        generation: meta.generation,
        key_epoch: meta.key_epoch,
        part_count: meta.part_count,
        total_plain_len: meta.total_plain_len,
        object_digest: meta.object_digest,
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 构造 §17.1 设备证书的 CDE 字节（签名由 hook 校验）。
pub fn certificate_bytes(
    vault_id: &Id16,
    device_id: &Id16,
    sign_pk: &[u8; 32],
    hpke_pk: &[u8; 32],
    manager_rev: u64,
) -> Vec<u8> {
    let cert = relay::verify::new_certificate(vault_id, device_id, sign_pk, hpke_pk, manager_rev);
    cert.encode().expect("certificate cde")
}

/// 每个用例独立的数据库路径。
///
/// 只靠 `pid + counter` 不够：Windows 会复用 pid，上次运行留下的库会让 `initialize` 报
/// `AlreadyInitialized`。这里再掺入时间/随机熵，并在建目录前清掉同名残留。
pub fn temp_db_path(label: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "relay-it-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst),
        unique_nonce()
    ));
    if dir.exists() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.join(format!("{label}.db"))
}

/// 时间纳秒 + CSPRNG 的低成本熵（两者都拿不到时退化为 0，但 pid/counter 仍在）。
fn unique_nonce() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let random = relay::ids::random_bytes::<8>()
        .map(u64::from_le_bytes)
        .unwrap_or(0);
    nanos ^ random.rotate_left(17)
}

/// 删除一条临时库所在的目录（用于需要显式重开、不走 `Fixture::Drop` 的用例）。
pub fn cleanup_db_dir(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// 测试验签 hook：fail-closed + 可审计调用记录 + 摘要相等校验。
#[derive(Debug, Clone)]
pub struct TestHook {
    /// 是否接受签名（默认 true）。
    pub allow: Arc<AtomicBool>,
    /// 记录每次验签的 domain（断言"确实调用了验签"）。
    pub seen: Arc<Mutex<Vec<SignDomain>>>,
}

impl Default for TestHook {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHook {
    pub fn new() -> Self {
        Self {
            allow: Arc::new(AtomicBool::new(true)),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn reject_all(&self) {
        self.allow.store(false, Ordering::SeqCst);
    }

    pub fn seen_domains(&self) -> Vec<SignDomain> {
        self.seen.lock().expect("seen lock").clone()
    }

    pub fn seen(&self) -> usize {
        self.seen.lock().expect("seen lock").len()
    }
}

impl SignatureHook for TestHook {
    fn verify(&self, check: &SignatureCheck<'_>) -> Result<()> {
        self.seen.lock().expect("seen lock").push(check.domain);
        if !self.allow.load(Ordering::SeqCst) {
            return Err(Error::SignatureRejected);
        }
        match check.expect {
            Expectation::ObjectCommit { object } => {
                // 约定：commit 前 32B = 摘要（真实实现见 relay::verify）。
                if check.message.len() >= 32
                    && &check.message[..32] == object.computed_digest.as_slice()
                {
                    Ok(())
                } else {
                    Err(Error::SignatureRejected)
                }
            }
            _ => Ok(()),
        }
    }
}

/// §34：按 relay 侧事实构造一份与本次 activate 完全一致的 activation T（测试用；签名由
/// 调用方补）。真实 relay 用 `rotations::activate` 内的同口径函数重建并逐项比对。
pub fn activation_t_for(
    conn: &ScopeConn,
    vault: &Vault,
    row: &rotations::RotationRow,
    transition: &rotations::ManagerTransition,
    primary: &Id16,
    fallback: &Id16,
) -> Result<proto::ActivationTV1> {
    let primary_row = checkpoints::get(conn, primary)?;
    let fallback_row = checkpoints::get(conn, fallback)?;
    let commit_digest = |checkpoint: &checkpoints::CheckpointRow| -> Result<Hash32> {
        Ok(sha256(checkpoint.signed_commit.as_deref().ok_or(
            Error::Invariant("test activation T: signed_commit"),
        )?))
    };
    let (head_rev, prev_control_hash) = vault::control_head(conn)?;
    let expected_control_rev = head_rev
        .checked_add(1)
        .ok_or(Error::Invariant("test activation T: control rev"))?;
    let (_, grants_digest) = rotations::grants_digest(conn, row.to_epoch)?;
    Ok(proto::ActivationTV1 {
        vault_id: vault.vault_id,
        instance_id: vault.instance_id,
        rotation_id: row.rotation_id,
        from_epoch: row.from_epoch,
        to_epoch: row.to_epoch,
        freeze_seq: row.freeze_seq,
        old_manager_rev: u32::try_from(transition.old_rev)
            .map_err(|_| Error::Invariant("test activation T: old rev"))?,
        new_manager_rev: u32::try_from(transition.new_rev)
            .map_err(|_| Error::Invariant("test activation T: new rev"))?,
        new_manager_pubkey: transition.new_pubkey,
        prev_control_hash,
        expected_control_rev,
        primary_cid: *primary,
        primary_commit_digest: commit_digest(&primary_row)?,
        fallback_cid: *fallback,
        fallback_commit_digest: commit_digest(&fallback_row)?,
        slot_set_digest: rotations::slot_set_digest(conn, row.to_epoch)?,
        member_cert_digest: rotations::member_cert_digest(conn)?,
        grants_digest,
        old_asset_set_digest: row
            .asset_set_digest
            .ok_or(Error::Invariant("test activation T: asset digest"))?,
        new_asset_set_digest: objects::candidate_asset_set_digest(
            conn,
            row.to_epoch,
            &row.rotation_id,
        )?,
    })
}

/// 带显式租户作用域的测试 DB 句柄（ADR 0014 §A）。
///
/// `read`/`write_blocking` 自动带上夹具的 `VaultScope`；`Deref` 到 [`Db`] 保留
/// `clock()`/`path()`/`close()` 等实例级能力。测试里因此无需处处手写 scope，
/// 但**作用域仍是显式的**：跨 vault 的用例必须自己构造另一个 [`TestDb`]。
#[derive(Debug, Clone)]
pub struct TestDb {
    inner: Db,
    scope: VaultScope,
}

impl TestDb {
    pub fn new(inner: Db, scope: VaultScope) -> Self {
        Self { inner, scope }
    }

    pub fn scope(&self) -> VaultScope {
        self.scope
    }

    pub fn inner(&self) -> &Db {
        &self.inner
    }

    pub fn read<T, F>(&self, f: F) -> relay::Result<T>
    where
        F: FnOnce(&ScopeConn<'_>) -> relay::Result<T>,
    {
        self.inner.read(self.scope, f)
    }

    pub fn write_blocking<T, F>(&self, f: F) -> relay::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&ScopeConn<'_>) -> relay::Result<T> + Send + 'static,
    {
        self.inner.write_blocking(self.scope, f)
    }
}

impl std::ops::Deref for TestDb {
    type Target = Db;

    fn deref(&self) -> &Db {
        &self.inner
    }
}

/// schema 9（单 vault）全量 fixture：ADR 0014 S2a 的 9→10 迁移测试基线。
///
/// 用途：
/// - `tests/migration_v10.rs`：逐项断言迁移不丢 ID/密文字节/签名链/rev/cursor/邀请事实；
/// - `tests/http.rs`：真实 v9 库升级后 HTTP 仍可服务旧 vault。
///
/// 这里**原样**保留 v9 的 `vault(singleton=1)` 与全部租户表（无 `vault_id`），
/// 迁移实现只能靠自己的升级逻辑通过；测试不会用 v10 的表"假装"旧库。
pub mod v9 {
    use super::*;
    use rusqlite::Connection;

    /// 迁移前 schema 版本。
    pub const SCHEMA_VERSION: i64 = 9;
    /// 固定 seed：所有断言都用它比对。
    pub const INSTANCE: Id16 = [0x22u8; 16];
    pub const VAULT: Id16 = [0x11u8; 16];
    pub const LOG_ID: Id16 = [0x33u8; 16];
    pub const LOG_INCARNATION: Id16 = [0x44u8; 16];
    pub const CURSOR_KEY: Hash32 = [0x55u8; 32];
    pub const DEVICE_ACTIVE: Id16 = [0xA1u8; 16];
    pub const DEVICE_PENDING: Id16 = [0xA2u8; 16];
    pub const DEVICE_REVOKED: Id16 = [0xA3u8; 16];
    pub const MANAGER_PUB: [u8; 32] = [0x66u8; 32];
    pub const INVITE_ID: Id16 = [0x12u8; 16];
    pub const INVITE_TOKEN_HASH: Hash32 = [0x13u8; 32];
    pub const UPDATE_OBJECT: Hash32 = [0x21u8; 32];
    pub const ASSET_OBJECT: Hash32 = [0x31u8; 32];
    pub const SLOT_OBJECT: Hash32 = [0x51u8; 32];
    pub const CHECKPOINT: Id16 = [0x41u8; 16];
    pub const ROTATION: Id16 = [0x71u8; 16];
    pub const RESERVATION: Id16 = [0x14u8; 16];
    pub const LEASE: Id16 = [0x81u8; 16];
    pub const CHALLENGE: Id16 = [0xD1u8; 16];
    pub const TOKEN_HASH: Hash32 = [0xF1u8; 32];
    pub const PART_ENVELOPE: [u8; 300] = [0x5Au8; 300];

    /// v9 完整 DDL（从 `schema 9` 二进制快照抄出，含索引）。
    pub const DDL: &str = r#"
-- ===== 实例元数据 =====
CREATE TABLE IF NOT EXISTS relay_meta(
  key   TEXT PRIMARY KEY,
  value BLOB NOT NULL
) STRICT;

-- ===== 单 vault（ADE 0006 §6：单实例只允许一个 vault）=====
CREATE TABLE IF NOT EXISTS vault(
  singleton         INTEGER PRIMARY KEY CHECK(singleton = 1),
  vault_id          BLOB NOT NULL CHECK(length(vault_id) = 16),
  instance_id       BLOB NOT NULL CHECK(length(instance_id) = 16),
  log_id            BLOB NOT NULL CHECK(length(log_id) = 16),
  log_incarnation   BLOB NOT NULL CHECK(length(log_incarnation) = 16),
  cursor_key        BLOB NOT NULL CHECK(length(cursor_key) = 32),
  state             TEXT NOT NULL CHECK(state IN('initializing','active','rotating')),
  active_epoch      INTEGER NOT NULL CHECK(active_epoch > 0),
  next_epoch        INTEGER NOT NULL CHECK(next_epoch > active_epoch),
  manager_rev       INTEGER NOT NULL CHECK(manager_rev >= 1),
  control_rev       INTEGER NOT NULL CHECK(control_rev >= 1),
  auth_rev          INTEGER NOT NULL CHECK(auth_rev >= 1),
  head_seq          INTEGER NOT NULL CHECK(head_seq >= 0),
  gc_floor          INTEGER NOT NULL CHECK(gc_floor >= 0 AND gc_floor <= head_seq),
  normal_limit      INTEGER NOT NULL CHECK(normal_limit > 0),
  maintenance_limit INTEGER NOT NULL CHECK(maintenance_limit >= normal_limit),
  created_at        INTEGER NOT NULL
) STRICT;

-- ===== 管理与控制链（不随密文 GC 消失，ADR §6 末段）=====
CREATE TABLE IF NOT EXISTS manager_keys(
  rev           INTEGER PRIMARY KEY CHECK(rev >= 1),
  pubkey        BLOB NOT NULL CHECK(length(pubkey) = 32),
  transition    BLOB NOT NULL,
  old_rev       INTEGER NOT NULL DEFAULT 0 CHECK(old_rev >= 0),
  old_signature BLOB,
  new_signature BLOB,
  created_at    INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS control_commits(
  rev           INTEGER PRIMARY KEY CHECK(rev >= 1),
  prev_hash     BLOB NOT NULL CHECK(length(prev_hash) = 32),
  record_hash   BLOB NOT NULL CHECK(length(record_hash) = 32),
  signed_record BLOB NOT NULL,
  created_at    INTEGER NOT NULL
) STRICT;

-- ===== epoch =====
CREATE TABLE IF NOT EXISTS epochs(
  epoch        INTEGER PRIMARY KEY CHECK(epoch > 0),
  state        TEXT NOT NULL CHECK(state IN('staged','active','retired','aborted')),
  revoked_once INTEGER NOT NULL DEFAULT 0 CHECK(revoked_once IN (0, 1)),
  created_at   INTEGER NOT NULL,
  retire_at    INTEGER
) STRICT;

-- ===== 设备 =====
CREATE TABLE IF NOT EXISTS devices(
  device_id   BLOB PRIMARY KEY CHECK(length(device_id) = 16),
  sign_pk     BLOB NOT NULL CHECK(length(sign_pk) = 32),
  hpke_pk     BLOB NOT NULL CHECK(length(hpke_pk) = 32),
  certificate BLOB NOT NULL,
  certificate_signature BLOB,
  status      TEXT NOT NULL CHECK(status IN('pending','active','revoked')),
  created_at  INTEGER NOT NULL,
  revoked_at  INTEGER,
  last_seen   INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS devices_status ON devices(status);

-- ===== 鉴权 =====
CREATE TABLE IF NOT EXISTS challenges(
  challenge_id BLOB PRIMARY KEY CHECK(length(challenge_id) = 16),
  realm        TEXT NOT NULL CHECK(realm IN('device','management')),
  subject      BLOB NOT NULL CHECK(length(subject) = 16),
  nonce        BLOB NOT NULL CHECK(length(nonce) = 32),
  expires_at   INTEGER NOT NULL,
  used_at      INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS challenges_expiry ON challenges(expires_at);

CREATE TABLE IF NOT EXISTS tokens(
  token_hash BLOB PRIMARY KEY CHECK(length(token_hash) = 32),
  realm      TEXT NOT NULL CHECK(realm IN('device','management')),
  subject    BLOB NOT NULL CHECK(length(subject) = 16),
  auth_rev   INTEGER NOT NULL CHECK(auth_rev >= 1),
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS tokens_expiry ON tokens(expires_at);

CREATE TABLE IF NOT EXISTS invites(
  invite_id       BLOB PRIMARY KEY CHECK(length(invite_id) = 16),
  token_hash      BLOB NOT NULL UNIQUE CHECK(length(token_hash) = 32),
  expires_at      INTEGER NOT NULL,
  pending_device  BLOB CHECK(pending_device IS NULL OR length(pending_device) = 16),
  used_at         INTEGER
) STRICT;

-- ===== 维护预算（ADR §13.2）=====
CREATE TABLE IF NOT EXISTS maintenance_reservations(
  reservation_id BLOB PRIMARY KEY CHECK(length(reservation_id) = 16),
  purpose        TEXT NOT NULL CHECK(purpose IN('checkpoint','rotation')),
  resource_id    BLOB NOT NULL UNIQUE,
  owner_device   BLOB NOT NULL REFERENCES devices(device_id),
  reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes >= 0),
  consumed_bytes INTEGER NOT NULL CHECK(consumed_bytes >= 0 AND consumed_bytes <= reserved_bytes),
  state          TEXT NOT NULL CHECK(state IN('active','complete','expired')),
  expires_at     INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS reservations_state ON maintenance_reservations(state, expires_at);

CREATE TABLE IF NOT EXISTS usage(
  singleton         INTEGER PRIMARY KEY CHECK(singleton = 1),
  normal_bytes      INTEGER NOT NULL CHECK(normal_bytes >= 0),
  maintenance_bytes INTEGER NOT NULL CHECK(maintenance_bytes >= 0)
) STRICT;

-- ===== 对象与分片 =====
CREATE TABLE IF NOT EXISTS objects(
  object_id      BLOB PRIMARY KEY CHECK(length(object_id) = 32),
  message_id     BLOB NOT NULL CHECK(length(message_id) = 16),
  producer       BLOB NOT NULL CHECK(length(producer) = 16),
  epoch          INTEGER NOT NULL CHECK(epoch > 0),
  type           INTEGER NOT NULL CHECK(type BETWEEN 1 AND 5),
  generation     INTEGER NOT NULL CHECK(generation >= 0),
  sender_seq     INTEGER NOT NULL CHECK(sender_seq >= 0),
  part_count     INTEGER NOT NULL CHECK(part_count >= 1),
  plain_len      INTEGER NOT NULL CHECK(plain_len >= 1),
  state          TEXT NOT NULL CHECK(state IN('staging','complete')),
  budget_class   TEXT NOT NULL CHECK(budget_class IN('normal','maintenance','retired')),
  digest         BLOB CHECK(digest IS NULL OR length(digest) = 32),
  signed_commit  BLOB,
  commit_signature BLOB,
  reservation_id BLOB REFERENCES maintenance_reservations(reservation_id),
  created_at     INTEGER NOT NULL,
  touched_at     INTEGER NOT NULL,
  completed_at   INTEGER,
  UNIQUE(epoch, message_id)
) STRICT;
CREATE INDEX IF NOT EXISTS objects_state ON objects(state, touched_at);
CREATE INDEX IF NOT EXISTS objects_epoch ON objects(epoch, type);

CREATE TABLE IF NOT EXISTS object_parts(
  object_id BLOB NOT NULL REFERENCES objects(object_id),
  part_no   INTEGER NOT NULL CHECK(part_no >= 0),
  envelope  BLOB NOT NULL CHECK(length(envelope) BETWEEN 212 AND 4194304),
  digest    BLOB NOT NULL CHECK(length(digest) = 32),
  PRIMARY KEY(object_id, part_no)
) STRICT;

-- ===== 日志（`relay_seq` 只表示送达顺序）=====
CREATE TABLE IF NOT EXISTS log(
  relay_seq  INTEGER PRIMARY KEY CHECK(relay_seq > 0),
  kind       TEXT NOT NULL CHECK(kind IN('update','asset','checkpoint','control')),
  object_id  BLOB CHECK(object_id IS NULL OR length(object_id) = 32),
  epoch      INTEGER NOT NULL CHECK(epoch > 0),
  event      BLOB NOT NULL,
  signature  BLOB,
  created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS log_epoch ON log(epoch);

-- ===== checkpoint =====
CREATE TABLE IF NOT EXISTS checkpoints(
  checkpoint_id      BLOB PRIMARY KEY CHECK(length(checkpoint_id) = 16),
  epoch              INTEGER NOT NULL CHECK(epoch > 0),
  covered_seq        INTEGER NOT NULL CHECK(covered_seq >= 0),
  manifest_oid       BLOB CHECK(manifest_oid IS NULL OR length(manifest_oid) = 32),
  refs_digest        BLOB CHECK(refs_digest IS NULL OR length(refs_digest) = 32),
  signed_commit      BLOB,
  commit_signature   BLOB,
  state              TEXT NOT NULL CHECK(state IN('staging','candidate','published','aborted','pruned')),
  role               TEXT CHECK(role IS NULL OR role IN('primary','fallback')),
  validation_version INTEGER NOT NULL CHECK(validation_version >= 1),
  health             TEXT NOT NULL CHECK(health IN('unknown','healthy','unhealthy')),
  created_at         INTEGER NOT NULL,
  published_at       INTEGER
) STRICT;
CREATE INDEX IF NOT EXISTS checkpoints_state ON checkpoints(state, epoch);
CREATE INDEX IF NOT EXISTS checkpoints_published ON checkpoints(published_at);

CREATE TABLE IF NOT EXISTS checkpoint_refs(
  checkpoint_id BLOB NOT NULL REFERENCES checkpoints(checkpoint_id),
  object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
  role          TEXT NOT NULL CHECK(role IN('data','manifest')),
  PRIMARY KEY(checkpoint_id, object_id)
) STRICT;
CREATE INDEX IF NOT EXISTS checkpoint_refs_object ON checkpoint_refs(object_id);

-- §25.1：不透明资产登记（只有 HMAC object_id/ObjectRefV1 元数据；真实 sha256/MIME/byte_len 不进 relay）。
CREATE TABLE IF NOT EXISTS asset_objects(
  epoch         INTEGER NOT NULL CHECK(epoch > 0),
  object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
  state         TEXT NOT NULL CHECK(state IN('active','retired')),
  finalized_seq INTEGER NOT NULL CHECK(finalized_seq >= 0),
  rotation_id   BLOB CHECK(rotation_id IS NULL OR length(rotation_id) = 16),
  PRIMARY KEY(epoch, object_id)
) STRICT;
CREATE INDEX IF NOT EXISTS asset_objects_epoch ON asset_objects(epoch, state, finalized_seq);

-- ===== key slot =====
CREATE TABLE IF NOT EXISTS key_slots(
  slot_id      BLOB PRIMARY KEY CHECK(length(slot_id) = 32) REFERENCES objects(object_id),
  epoch        INTEGER NOT NULL CHECK(epoch > 0),
  kind         TEXT NOT NULL CHECK(kind IN('password','recovery')),
  descriptor   BLOB NOT NULL,
  state        TEXT NOT NULL CHECK(state IN('staging','active','retiring')),
  slot_set_rev INTEGER NOT NULL CHECK(slot_set_rev >= 0),
  retire_at    INTEGER,
  created_at   INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX IF NOT EXISTS active_slot_kind ON key_slots(kind) WHERE state = 'active';

-- ===== HPKE grants（按 epoch）=====
CREATE TABLE IF NOT EXISTS device_grants(
  epoch           INTEGER NOT NULL CHECK(epoch > 0),
  device_id       BLOB NOT NULL REFERENCES devices(device_id),
  manager_rev     INTEGER NOT NULL CHECK(manager_rev >= 1),
  hpke_enc        BLOB NOT NULL CHECK(length(hpke_enc) = 32),
  hpke_ciphertext BLOB NOT NULL,
  signed_header   BLOB NOT NULL,
  signature       BLOB,
  grant_id        BLOB CHECK(grant_id IS NULL OR length(grant_id) = 16),
  activation_ref  BLOB CHECK(activation_ref IS NULL OR length(activation_ref) = 16),
  PRIMARY KEY(epoch, device_id)
) STRICT;

-- ===== 轮换 =====
CREATE TABLE IF NOT EXISTS rotations(
  rotation_id    BLOB PRIMARY KEY CHECK(length(rotation_id) = 16),
  from_epoch     INTEGER NOT NULL CHECK(from_epoch > 0),
  to_epoch       INTEGER NOT NULL CHECK(to_epoch > from_epoch),
  freeze_seq     INTEGER NOT NULL CHECK(freeze_seq >= 0),
  member_set_hash BLOB CHECK(member_set_hash IS NULL OR length(member_set_hash) = 32),
  asset_set_digest BLOB CHECK(asset_set_digest IS NULL OR length(asset_set_digest) = 32),
  revoked_count  INTEGER NOT NULL CHECK(revoked_count >= 0),
  state          TEXT NOT NULL CHECK(state IN('frozen','staging','activated','aborted')),
  primary_checkpoint BLOB CHECK(primary_checkpoint IS NULL OR length(primary_checkpoint) = 16),
  fallback_checkpoint BLOB CHECK(fallback_checkpoint IS NULL OR length(fallback_checkpoint) = 16),
  activation_commit  BLOB CHECK(activation_commit IS NULL OR length(activation_commit) = 32),
  activated_at       INTEGER,
  activated_control_rev INTEGER CHECK(activated_control_rev IS NULL OR activated_control_rev >= 0),
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS rotations_state ON rotations(state);

-- ===== bootstrap lease =====
CREATE TABLE IF NOT EXISTS bootstrap_leases(
  lease_id           BLOB PRIMARY KEY CHECK(length(lease_id) = 16),
  device_id          BLOB NOT NULL REFERENCES devices(device_id),
  primary_checkpoint BLOB NOT NULL CHECK(length(primary_checkpoint) = 16),
  fallback_checkpoint BLOB NOT NULL CHECK(length(fallback_checkpoint) = 16),
  floor_seq          INTEGER NOT NULL CHECK(floor_seq >= 0),
  created_at         INTEGER NOT NULL,
  expires_at         INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS leases_expiry ON bootstrap_leases(expires_at);

-- ===== 设备 ACK 水位 =====
CREATE TABLE IF NOT EXISTS device_acks(
  device_id     BLOB PRIMARY KEY REFERENCES devices(device_id),
  received_seq  INTEGER NOT NULL CHECK(received_seq >= 0),
  integrated_seq INTEGER NOT NULL CHECK(integrated_seq >= 0),
  epoch         INTEGER NOT NULL CHECK(epoch > 0),
  updated_at    INTEGER NOT NULL,
  CHECK(received_seq >= integrated_seq)
) STRICT;

-- ===== 幂等 / tombstone（不随密文 GC 消失，ADR §6 末段）=====
CREATE TABLE IF NOT EXISTS dedup(
  scope        TEXT NOT NULL,
  id           BLOB NOT NULL,
  request_hash BLOB NOT NULL CHECK(length(request_hash) = 32),
  accepted_seq INTEGER,
  response     BLOB NOT NULL,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(scope, id)
) STRICT;

-- ===== GC 持久计划（ADR §7：后来的 checkpoint 不能重置已有 deadline）=====
CREATE TABLE IF NOT EXISTS gc_plans(
  epoch       INTEGER NOT NULL CHECK(epoch > 0),
  cut_seq     INTEGER NOT NULL CHECK(cut_seq >= 0),
  eligible_at INTEGER NOT NULL,
  proof       BLOB NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY(epoch, cut_seq)
) STRICT;
"#;

    /// 建一个 schema 9 文件库（已含 `relay_meta` 版本 9）。
    pub fn create(path: &Path) -> Connection {
        let conn = Connection::open(path).expect("open v9 db");
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .expect("busy");
        conn.pragma_update(None, "journal_mode", "WAL")
            .expect("wal");
        conn.pragma_update(None, "synchronous", "FULL")
            .expect("sync");
        conn.pragma_update(None, "foreign_keys", "ON").expect("fk");
        conn.execute_batch(DDL).expect("v9 ddl");
        conn.execute(
            "INSERT INTO relay_meta(key, value) VALUES ('instance_id', ?1)",
            [INSTANCE.as_slice()],
        )
        .expect("instance");
        conn.execute(
            "INSERT INTO relay_meta(key, value) VALUES ('schema_version', ?1)",
            [SCHEMA_VERSION.to_string().as_bytes()],
        )
        .expect("version");
        conn.execute(
            "INSERT INTO relay_meta(key, value) VALUES ('setup.consumed', ?1)",
            [b"1".as_slice()],
        )
        .expect("setup consumed");
        conn
    }

    /// 写入覆盖全部租户表的一整套旧数据（含已消费邀请、游标口径的 head/gc_floor）。
    pub fn seed(conn: &Connection) {
        conn.execute(
            "INSERT INTO vault(singleton, vault_id, instance_id, log_id, log_incarnation,
                               cursor_key, state, active_epoch, next_epoch, manager_rev,
                               control_rev, auth_rev, head_seq, gc_floor, normal_limit,
                               maintenance_limit, created_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, 'active', 1, 3, 2, 2, 4, 3, 1, 1048576, 2097152, 100)",
            (
                VAULT.as_slice(),
                INSTANCE.as_slice(),
                LOG_ID.as_slice(),
                LOG_INCARNATION.as_slice(),
                CURSOR_KEY.as_slice(),
            ),
        )
        .expect("vault");
        conn.execute(
            "INSERT INTO manager_keys(rev, pubkey, transition, old_rev, old_signature,
                                      new_signature, created_at)
             VALUES (1, ?1, ?2, 0, NULL, NULL, 100)",
            (MANAGER_PUB.as_slice(), b"genesis-cert".as_slice()),
        )
        .expect("manager key");
        // 控制链按真实公式算 `record_hash = SHA256(prev_hash || signed_record)`，
        // 迁移后必须仍可逐条重算（签名链不可被重写）。
        let genesis = relay::ids::sha256_parts(&[[0u8; 32].as_slice(), b"genesis-cert"]);
        conn.execute(
            "INSERT INTO control_commits(rev, prev_hash, record_hash, signed_record, created_at)
             VALUES (1, ?1, ?2, ?3, 100)",
            (
                [0u8; 32].as_slice(),
                genesis.as_slice(),
                b"genesis-cert".as_slice(),
            ),
        )
        .expect("control rev 1");
        let begin = relay::ids::sha256_parts(&[genesis.as_slice(), b"rotation-begin"]);
        conn.execute(
            "INSERT INTO control_commits(rev, prev_hash, record_hash, signed_record, created_at)
             VALUES (2, ?1, ?2, ?3, 150)",
            (
                genesis.as_slice(),
                begin.as_slice(),
                b"rotation-begin".as_slice(),
            ),
        )
        .expect("control rev 2");
        conn.execute(
            "INSERT INTO epochs(epoch, state, revoked_once, created_at, retire_at)
             VALUES (1, 'active', 0, 100, NULL)",
            [],
        )
        .expect("epoch 1");
        conn.execute(
            "INSERT INTO epochs(epoch, state, revoked_once, created_at, retire_at)
             VALUES (2, 'staged', 1, 150, NULL)",
            [],
        )
        .expect("epoch 2");
        conn.execute(
            "INSERT INTO devices(device_id, sign_pk, hpke_pk, certificate, certificate_signature,
                                 status, created_at, revoked_at, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, 'active', 100, NULL, 200)",
            (
                DEVICE_ACTIVE.as_slice(),
                [0xB1u8; 32].as_slice(),
                [0xC1u8; 32].as_slice(),
                b"cert-active".as_slice(),
                b"cert-sig-active".as_slice(),
            ),
        )
        .expect("active device");
        conn.execute(
            "INSERT INTO devices(device_id, sign_pk, hpke_pk, certificate, certificate_signature,
                                 status, created_at, revoked_at, last_seen)
             VALUES (?1, ?2, ?3, X'', NULL, 'pending', 110, NULL, NULL)",
            (
                DEVICE_PENDING.as_slice(),
                [0xB2u8; 32].as_slice(),
                [0xC2u8; 32].as_slice(),
            ),
        )
        .expect("pending device");
        conn.execute(
            "INSERT INTO devices(device_id, sign_pk, hpke_pk, certificate, certificate_signature,
                                 status, created_at, revoked_at, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, 'revoked', 100, 120, 130)",
            (
                DEVICE_REVOKED.as_slice(),
                [0xB3u8; 32].as_slice(),
                [0xC3u8; 32].as_slice(),
                b"cert-revoked".as_slice(),
                b"cert-sig-revoked".as_slice(),
            ),
        )
        .expect("revoked device");
        // 旧 challenge / token：迁移必须失效（ADR 0014 §A 维护窗口）。
        conn.execute(
            "INSERT INTO challenges(challenge_id, realm, subject, nonce, expires_at, used_at)
             VALUES (?1, 'device', ?2, ?3, 1000, NULL)",
            (
                CHALLENGE.as_slice(),
                DEVICE_ACTIVE.as_slice(),
                [0xE1u8; 32].as_slice(),
            ),
        )
        .expect("challenge");
        conn.execute(
            "INSERT INTO tokens(token_hash, realm, subject, auth_rev, expires_at, created_at)
             VALUES (?1, 'device', ?2, 4, 1000, 100)",
            (TOKEN_HASH.as_slice(), DEVICE_ACTIVE.as_slice()),
        )
        .expect("token");
        // 已消费邀请：事实必须保留（used_at + pending_device）。
        conn.execute(
            "INSERT INTO invites(invite_id, token_hash, expires_at, pending_device, used_at)
             VALUES (?1, ?2, 2000, ?3, 150)",
            (
                INVITE_ID.as_slice(),
                INVITE_TOKEN_HASH.as_slice(),
                DEVICE_PENDING.as_slice(),
            ),
        )
        .expect("invite");
        conn.execute(
            "INSERT INTO maintenance_reservations(reservation_id, purpose, resource_id,
                                                  owner_device, reserved_bytes, consumed_bytes,
                                                  state, expires_at)
             VALUES (?1, 'checkpoint', ?2, ?3, 1000, 400, 'active', 2000)",
            (
                RESERVATION.as_slice(),
                [0x15u8; 16].as_slice(),
                DEVICE_ACTIVE.as_slice(),
            ),
        )
        .expect("reservation");
        conn.execute(
            "INSERT INTO usage(singleton, normal_bytes, maintenance_bytes) VALUES (1, 600, 400)",
            [],
        )
        .expect("usage");
        for (object_id, object_type, message, part_count) in [
            (UPDATE_OBJECT, 1i64, [0x23u8; 16], 1i64),
            (ASSET_OBJECT, 4i64, [0x24u8; 16], 1i64),
            (SLOT_OBJECT, 5i64, [0x25u8; 16], 1i64),
        ] {
            conn.execute(
                "INSERT INTO objects(object_id, message_id, producer, epoch, type, generation,
                                     sender_seq, part_count, plain_len, state, budget_class,
                                     digest, signed_commit, commit_signature, reservation_id,
                                     created_at, touched_at, completed_at)
                 VALUES (?1, ?2, ?3, 1, ?4, 0, 0, ?5, 300, 'complete', 'normal', ?6, ?7, ?8,
                         NULL, 100, 100, 100)",
                (
                    object_id.as_slice(),
                    message.as_slice(),
                    DEVICE_ACTIVE.as_slice(),
                    object_type,
                    part_count,
                    [0x26u8; 32].as_slice(),
                    b"signed-commit".as_slice(),
                    b"commit-signature".as_slice(),
                ),
            )
            .expect("object");
            conn.execute(
                "INSERT INTO object_parts(object_id, part_no, envelope, digest)
                 VALUES (?1, 0, ?2, ?3)",
                (
                    object_id.as_slice(),
                    PART_ENVELOPE.to_vec(),
                    [0x27u8; 32].as_slice(),
                ),
            )
            .expect("part");
        }
        conn.execute(
            "INSERT INTO log(relay_seq, kind, object_id, epoch, event, signature, created_at)
             VALUES (1, 'update', ?1, 1, ?2, ?3, 100)",
            (
                UPDATE_OBJECT.as_slice(),
                b"event-update".as_slice(),
                b"sig-update".as_slice(),
            ),
        )
        .expect("log 1");
        conn.execute(
            "INSERT INTO log(relay_seq, kind, object_id, epoch, event, signature, created_at)
             VALUES (2, 'asset', ?1, 1, ?2, ?3, 110)",
            (
                ASSET_OBJECT.as_slice(),
                b"event-asset".as_slice(),
                b"sig-asset".as_slice(),
            ),
        )
        .expect("log 2");
        conn.execute(
            "INSERT INTO log(relay_seq, kind, object_id, epoch, event, signature, created_at)
             VALUES (3, 'control', NULL, 1, ?1, ?2, 150)",
            (b"event-control".as_slice(), b"sig-control".as_slice()),
        )
        .expect("log 3");
        conn.execute(
            "INSERT INTO checkpoints(checkpoint_id, epoch, covered_seq, manifest_oid,
                                     refs_digest, signed_commit, commit_signature, state, role,
                                     validation_version, health, created_at, published_at)
             VALUES (?1, 1, 2, NULL, ?2, ?3, ?4, 'staging', NULL, 1, 'healthy', 100, NULL)",
            (
                CHECKPOINT.as_slice(),
                [0x28u8; 32].as_slice(),
                b"checkpoint-commit".as_slice(),
                b"checkpoint-sig".as_slice(),
            ),
        )
        .expect("checkpoint");
        conn.execute(
            "INSERT INTO checkpoint_refs(checkpoint_id, object_id, role) VALUES (?1, ?2, 'data')",
            (CHECKPOINT.as_slice(), UPDATE_OBJECT.as_slice()),
        )
        .expect("checkpoint ref");
        conn.execute(
            "INSERT INTO asset_objects(epoch, object_id, state, finalized_seq, rotation_id)
             VALUES (1, ?1, 'active', 2, ?2)",
            (ASSET_OBJECT.as_slice(), ROTATION.as_slice()),
        )
        .expect("asset registry");
        conn.execute(
            "INSERT INTO key_slots(slot_id, epoch, kind, descriptor, state, slot_set_rev,
                                   retire_at, created_at)
             VALUES (?1, 1, 'password', ?2, 'active', 1, NULL, 100)",
            (SLOT_OBJECT.as_slice(), b"slot-descriptor".as_slice()),
        )
        .expect("key slot");
        conn.execute(
            "INSERT INTO device_grants(epoch, device_id, manager_rev, hpke_enc, hpke_ciphertext,
                                       signed_header, signature, grant_id, activation_ref)
             VALUES (1, ?1, 2, ?2, ?3, ?4, ?5, ?6, NULL)",
            (
                DEVICE_ACTIVE.as_slice(),
                [0x61u8; 32].as_slice(),
                b"grant-ciphertext".as_slice(),
                b"grant-header".as_slice(),
                b"grant-signature".as_slice(),
                [0x62u8; 16].as_slice(),
            ),
        )
        .expect("grant");
        conn.execute(
            "INSERT INTO rotations(rotation_id, from_epoch, to_epoch, freeze_seq, member_set_hash,
                                   asset_set_digest, revoked_count, state, primary_checkpoint,
                                   fallback_checkpoint, activation_commit, activated_at,
                                   activated_control_rev, created_at, updated_at)
             VALUES (?1, 1, 2, 2, ?2, ?3, 1, 'frozen', NULL, NULL, NULL, NULL, NULL, 150, 150)",
            (
                ROTATION.as_slice(),
                [0x29u8; 32].as_slice(),
                [0x2Au8; 32].as_slice(),
            ),
        )
        .expect("rotation");
        conn.execute(
            "INSERT INTO bootstrap_leases(lease_id, device_id, primary_checkpoint,
                                          fallback_checkpoint, floor_seq, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?3, 2, 100, 2000)",
            (
                LEASE.as_slice(),
                DEVICE_ACTIVE.as_slice(),
                CHECKPOINT.as_slice(),
            ),
        )
        .expect("lease");
        conn.execute(
            "INSERT INTO device_acks(device_id, received_seq, integrated_seq, epoch, updated_at)
             VALUES (?1, 3, 2, 1, 200)",
            [DEVICE_ACTIVE.as_slice()],
        )
        .expect("ack");
        conn.execute(
            "INSERT INTO dedup(scope, id, request_hash, accepted_seq, response, created_at)
             VALUES ('object-complete', ?1, ?2, 1, ?3, 200)",
            (
                UPDATE_OBJECT.as_slice(),
                [0x2Bu8; 32].as_slice(),
                b"ack-response".as_slice(),
            ),
        )
        .expect("dedup");
        conn.execute(
            "INSERT INTO gc_plans(epoch, cut_seq, eligible_at, proof, created_at)
             VALUES (1, 1, 5000, ?1, 200)",
            [b"gc-proof".as_slice()],
        )
        .expect("gc plan");
    }

    /// 建库 + 全套种子数据。
    pub fn fixture(path: &Path) -> Connection {
        let conn = create(path);
        seed(&conn);
        conn
    }
}

/// 任意 vault 的管理 realm 上下文（弱夹具的管理 subject 由 `manager_pub` 派生）。
pub fn manager_ctx_for(fx: &Fixture, vault_id: Id16) -> AuthContext {
    AuthContext {
        realm: Realm::Management,
        subject: Realm::management_subject(&fx.manager_pub),
        device_id: None,
        auth_rev: fx.vault().auth_rev,
        authz_rev: 0,
        authority_rev: 0,
        cap_id: None,
        vault: VaultScope::new(vault_id),
    }
}

/// 测试夹具：一个已初始化的单 vault + 单设备。
pub struct Fixture {
    pub db: TestDb,
    pub clock: Arc<FakeClock>,
    pub hook: Arc<TestHook>,
    pub cfg: Arc<RelayConfig>,
    pub vault_id: Id16,
    pub device: Id16,
    pub manager_pub: [u8; 32],
    /// 管理私钥（只有 [`Fixture::with_real_manager`] 提供；其余用例用 `TestHook`，
    /// 管理签名不参与密码学校验，避免把弱 hook 当成安全证据）。
    pub manager_signing_key: Option<SigningKey>,
    /// genesis 设备的真实 Ed25519 私钥（§20.1：测试端要能用返回的 commit+signature 验签）。
    pub device_signing_key: SigningKey,
    /// 最后声明的清理守卫：Rust 在 `Drop` 后按**声明顺序** drop 字段，
    /// 因此 `db` 先关闭、守卫最后删目录（Windows 上文件未关闭时删除会失败）。
    cleanup: DbCleanup,
}

/// `Fixture` 的临时目录清理守卫（声明在最后，保证 SQLite 文件已关闭）。
struct DbCleanup {
    path: PathBuf,
    enabled: bool,
}

impl Drop for DbCleanup {
    fn drop(&mut self) {
        if self.enabled {
            cleanup_db_dir(&self.path);
        }
    }
}

/// `devices` 行的测试快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSnapshot {
    pub status: String,
    pub revoked_at: Option<i64>,
    pub certificate: Vec<u8>,
    pub sign_pk: Vec<u8>,
    pub hpke_pk: Vec<u8>,
    pub certificate_signature: Option<Vec<u8>>,
}

impl Fixture {
    /// 默认配额：`Q = 1MiB`，峰值 `Q+R = 2MiB`。
    pub fn new(label: &str) -> Self {
        Self::with_limits(label, 1024 * 1024, 2 * 1024 * 1024)
    }

    /// 带**真实管理私钥**的夹具：管理写走真实 Ed25519（配合 `Ed25519Hook` 的 AppState），
    /// 才能证明"签名绑定 canonical body"这类安全断言（弱 `TestHook` 证明不了密码学不变量）。
    pub fn with_real_manager(label: &str) -> Self {
        Self::build(label, 1024 * 1024, 2 * 1024 * 1024, Some([0x4Du8; 32]))
    }

    /// 真实管理私钥 + 指定配额（轮换 Q 门禁/预算归类用例用）。
    pub fn with_real_manager_limits(
        label: &str,
        normal_limit: i64,
        maintenance_limit: i64,
    ) -> Self {
        Self::build(label, normal_limit, maintenance_limit, Some([0x4Du8; 32]))
    }

    /// 指定配额的夹具。
    pub fn with_limits(label: &str, normal_limit: i64, maintenance_limit: i64) -> Self {
        Self::build(label, normal_limit, maintenance_limit, None)
    }

    /// 指定 v2 入口限流的夹具（测试用低阈值证明 429；`None` = 关闭）。
    pub fn with_v2_rate_limits(
        label: &str,
        create: Option<RateLimitSpec>,
        auth: Option<RateLimitSpec>,
    ) -> Self {
        Self::build_with(label, 1024 * 1024, 2 * 1024 * 1024, None, move |cfg| {
            cfg.with_v2_rate_limits(create, auth)
        })
    }

    /// v2 **业务**入口限流（create/auth 关闭）：证明业务 429 且 v1 不受影响。
    pub fn with_v2_data_limit(label: &str, data: Option<RateLimitSpec>) -> Self {
        Self::build_with(label, 1024 * 1024, 2 * 1024 * 1024, None, move |cfg| {
            cfg.with_v2_rate_limits(None, None).with_v2_data_limit(data)
        })
    }

    fn build(
        label: &str,
        normal_limit: i64,
        maintenance_limit: i64,
        manager_seed: Option<[u8; 32]>,
    ) -> Self {
        Self::build_with(
            label,
            normal_limit,
            maintenance_limit,
            manager_seed,
            |cfg| cfg,
        )
    }

    fn build_with(
        label: &str,
        normal_limit: i64,
        maintenance_limit: i64,
        manager_seed: Option<[u8; 32]>,
        configure: impl FnOnce(RelayConfig) -> RelayConfig,
    ) -> Self {
        let clock = Arc::new(FakeClock::new(BASE_TIME));
        let hook = Arc::new(TestHook::new());
        let cfg = Arc::new(configure(
            RelayConfig::new(temp_db_path(label))
                .with_setup_secret("setup-secret")
                .with_limits(normal_limit, maintenance_limit),
        ));
        let db = Db::open(cfg.db.clone(), clock.clone()).expect("open db");
        let db_path = cfg.db.path.clone();
        // 唯一路径下不应有历史库；若真撞上残留/复用，直接给出可操作的断言而不是 AlreadyInitialized。
        let existing = db
            .read_instance(relay::db::schema::list_vault_ids)
            .expect("read vault list before init");
        assert!(
            existing.is_empty(),
            "测试库 {} 已初始化（路径复用/上次运行残留）：测试隔离被破坏",
            db_path.display()
        );
        let vault_id = [1u8; 16];
        let device = [2u8; 16];
        let manager_signing_key = manager_seed.map(|seed| SigningKey::from_bytes(&seed));
        let manager_pub = match &manager_signing_key {
            Some(key) => key.verifying_key().to_bytes(),
            None => [7u8; 32],
        };
        // genesis 设备用真实 Ed25519 key：checkpoint 发布走 Ed25519Hook，测试端用公钥验签。
        let device_signing_key = SigningKey::from_bytes(&[3u8; 32]);
        let sign_pk = device_signing_key.verifying_key().to_bytes();
        let hpke_pk = [4u8; 32];
        // §17.1：首台设备证书必须是管理 key 签发的 CDE(DeviceCertV1)。
        let certificate = certificate_bytes(&vault_id, &device, &sign_pk, &hpke_pk, 1);
        let certificate_signature = match &manager_signing_key {
            Some(key) => {
                let cert = proto::DeviceCertV1::decode(&certificate).expect("decode certificate");
                key.sign(&proto::device_cert_signed_bytes(&cert).expect("device cert signed bytes"))
                    .to_bytes()
                    .to_vec()
            }
            None => b"certificate-sig".to_vec(),
        };
        let init = vault::InitRequest {
            vault_id,
            device_id: device,
            sign_pk,
            hpke_pk,
            manager_pub,
            setup_secret: "setup-secret".to_string(),
            certificate,
            certificate_signature,
        };
        let db_for_init = db.clone();
        let scope_for_init = VaultScope::new(vault_id);
        let clock_for_init = clock.clone();
        // 有真实管理私钥时，genesis 证书签名也必须走真实 Ed25519 校验（不再用 TestHook
        // 接受占位签名）；只有弱夹具（无管理私钥）保留 TestHook。
        let hook_for_init: Arc<dyn SignatureHook> = if manager_signing_key.is_some() {
            Arc::new(relay::verify::Ed25519Hook)
        } else {
            hook.clone()
        };
        let cfg_for_init = cfg.clone();
        db_for_init
            .write_blocking(scope_for_init, move |conn| {
                vault::initialize(
                    conn,
                    clock_for_init.as_ref(),
                    &cfg_for_init,
                    hook_for_init.as_ref(),
                    &init,
                )
            })
            .unwrap_or_else(|err| panic!("initialize vault at {}: {err}", db_path.display()));
        Self {
            db: TestDb::new(db, VaultScope::new(vault_id)),
            clock,
            hook,
            cfg,
            vault_id,
            device,
            manager_pub,
            manager_signing_key,
            device_signing_key,
            cleanup: DbCleanup {
                path: db_path,
                enabled: true,
            },
        }
    }

    /// 交出 DB 路径并**保留**目录（崩溃恢复/迁移等需要重开同一文件的用例）。
    pub fn into_db_path(mut self) -> PathBuf {
        self.cleanup.enabled = false;
        self.cfg.db.path.clone()
    }

    pub fn vault(&self) -> Vault {
        self.db.read(vault::load).expect("load vault")
    }

    pub fn device_ctx(&self) -> AuthContext {
        AuthContext {
            realm: Realm::Device,
            subject: self.device,
            device_id: Some(self.device),
            auth_rev: self.vault().auth_rev,
            authz_rev: 0,
            authority_rev: 0,
            cap_id: None,
            vault: self.db.scope(),
        }
    }

    pub fn manager_ctx(&self) -> AuthContext {
        AuthContext {
            realm: Realm::Management,
            subject: Realm::management_subject(&self.manager_pub),
            device_id: None,
            auth_rev: self.vault().auth_rev,
            authz_rev: 0,
            authority_rev: 0,
            cap_id: None,
            vault: self.db.scope(),
        }
    }

    pub fn usage(&self) -> quota::Usage {
        self.db.read(quota::usage).expect("usage")
    }

    pub fn head_seq(&self) -> u64 {
        self.vault().head_seq
    }

    /// 走完整 challenge → token 流程拿一个设备 token（HTTP 测试用）。
    pub fn device_token(&self) -> String {
        self.device_token_for(&self.device)
    }

    /// 指定设备 id 的 token（自批/恢复路径测试用）。
    pub fn device_token_for(&self, device: &Id16) -> String {
        let subject = *device;
        let challenge = {
            let clock = self.clock.clone();
            self.db
                .write_blocking(move |conn| {
                    auth::create_challenge(conn, clock.as_ref(), Realm::Device, &subject)
                })
                .expect("challenge")
        };
        let clock = self.clock.clone();
        let hook = self.hook.clone();
        self.db
            .write_blocking(move |conn| {
                auth::issue_token(
                    conn,
                    clock.as_ref(),
                    hook.as_ref(),
                    &challenge.challenge_id,
                    b"challenge-message",
                    b"signature",
                )
            })
            .expect("token")
            .token
    }

    /// 管理 token。
    pub fn manager_token(&self) -> String {
        let subject = Realm::management_subject(&self.manager_pub);
        let challenge = {
            let clock = self.clock.clone();
            self.db
                .write_blocking(move |conn| {
                    auth::create_challenge(conn, clock.as_ref(), Realm::Management, &subject)
                })
                .expect("challenge")
        };
        let clock = self.clock.clone();
        let hook = self.hook.clone();
        self.db
            .write_blocking(move |conn| {
                auth::issue_token(
                    conn,
                    clock.as_ref(),
                    hook.as_ref(),
                    &challenge.challenge_id,
                    b"challenge-message",
                    b"signature",
                )
            })
            .expect("token")
            .token
    }

    // ---------------- 对象流水线 ----------------

    pub fn put_part(
        &self,
        envelope: &[u8],
        reservation: Option<&Id16>,
    ) -> Result<objects::PartPutOutcome> {
        let ctx = self.device_ctx();
        let header = Header::parse(envelope)?;
        let db = self.db.clone();
        let clock = self.clock.clone();
        let vault_id = self.vault_id;
        let envelope = envelope.to_vec();
        let reservation = reservation.copied();
        db.write_blocking(move |conn| {
            objects::put_part(
                conn,
                clock.as_ref(),
                &objects::PartUpload {
                    actor: &ctx,
                    vault_id: &vault_id,
                    object_id: &header.object_id,
                    part_no: header.part_index,
                    envelope: &envelope,
                    reservation: reservation.as_ref(),
                },
            )
        })
    }

    pub fn complete_object(
        &self,
        object_id: &Hash32,
        envelope: &[u8],
    ) -> Result<objects::CompleteOutcome> {
        let digest = digest_of(&[envelope.to_vec()])?;
        let commit = commit_for(&digest);
        self.complete_with_commit(object_id, &commit)
    }

    pub fn complete_with_commit(
        &self,
        object_id: &Hash32,
        commit: &[u8],
    ) -> Result<objects::CompleteOutcome> {
        let ctx = self.device_ctx();
        let db = self.db.clone();
        let clock = self.clock.clone();
        let hook = self.hook.clone();
        let vault_id = self.vault_id;
        let object_id = *object_id;
        let commit = commit.to_vec();
        db.write_blocking(move |conn| {
            objects::complete(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &objects::CompleteUpload {
                    actor: &ctx,
                    vault_id: &vault_id,
                    object_id: &object_id,
                    commit: &commit,
                    signature: b"signature",
                },
            )
        })
    }

    /// 上传 + complete 一个对象，返回 (object_id, envelope, relay_seq)。
    pub fn create_object(
        &self,
        object_type: ObjectType,
        generation: u64,
        sender_seq: u64,
        plain_len: u64,
    ) -> (Hash32, Vec<u8>, Option<u64>) {
        let object_id = random_object_id();
        let envelope = single_part_envelope(
            object_type,
            self.vault_id,
            object_id,
            generation,
            sender_seq,
            plain_len,
        );
        self.put_part(&envelope, None).expect("put part");
        let outcome = self
            .complete_object(&object_id, &envelope)
            .expect("complete");
        (object_id, envelope, outcome.relay_seq)
    }

    /// 上传一个 snapshot/manifest 数据对象并返回 object_id。
    pub fn create_data_object(&self, object_type: ObjectType, plain_len: u64) -> Hash32 {
        let generation = if object_type == ObjectType::Snapshot {
            1
        } else {
            0
        };
        let (object_id, _, _) = self.create_object(object_type, generation, 0, plain_len);
        object_id
    }

    /// 建立并发布一个 baseline（S = 当前 head_seq）。
    pub fn publish_baseline(&self) -> Id16 {
        let snapshot = self.create_data_object(ObjectType::Snapshot, 64);
        let manifest = self.create_data_object(ObjectType::Manifest, 32);
        let ctx = self.device_ctx();
        let db = self.db.clone();
        let clock = self.clock.clone();
        let outcome = db
            .write_blocking(move |conn| {
                checkpoints::create(conn, clock.as_ref(), &ctx, 4096, None, None, None)
            })
            .expect("create checkpoint");
        let cid = outcome.checkpoint_id;
        let db = self.db.clone();
        db.write_blocking(move |conn| {
            checkpoints::add_refs(
                conn,
                1024,
                &cid,
                &[(snapshot, RefRole::Data), (manifest, RefRole::Manifest)],
            )
        })
        .expect("refs");
        let covered = outcome.covered_seq;
        let ctx = self.device_ctx();
        let db = self.db.clone();
        let clock = self.clock.clone();
        let hook = self.hook.clone();
        db.write_blocking(move |conn| {
            checkpoints::publish(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &cid,
                &checkpoints::PublishRequest {
                    covered_seq: covered,
                    manifest_oid: manifest,
                    refs_digest: [9u8; 32],
                    signed_commit: b"checkpoint-commit",
                    signature: b"signature",
                },
            )
        })
        .expect("publish checkpoint");
        cid
    }

    /// genesis 设备的 Ed25519 公钥（bootstrap 返回签名 → 测试端验签用）。
    pub fn device_pubkey(&self) -> [u8; 32] {
        self.device_signing_key.verifying_key().to_bytes()
    }

    /// 用真实 Ed25519 签名发布一份基线（`Ed25519Hook` 校验通过，签名随后可从 bootstrap 取回验证）。
    pub fn publish_signed_baseline(&self) -> Id16 {
        let snapshot = self.create_data_object(ObjectType::Snapshot, 64);
        let manifest = self.create_data_object(ObjectType::Manifest, 32);
        let epoch = self.vault().active_epoch;
        let ctx = self.device_ctx();
        let handle = self.db.clone();
        let clock = self.clock.clone();
        let outcome = handle
            .write_blocking(move |conn| {
                checkpoints::create(conn, clock.as_ref(), &ctx, 4096, None, None, None)
            })
            .expect("create signed checkpoint");
        let cid = outcome.checkpoint_id;
        let covered = outcome.covered_seq;
        let handle = self.db.clone();
        handle
            .write_blocking(move |conn| {
                checkpoints::add_refs(
                    conn,
                    1024,
                    &cid,
                    &[(snapshot, RefRole::Data), (manifest, RefRole::Manifest)],
                )
            })
            .expect("signed checkpoint refs");
        let snapshot_meta = self
            .db
            .read(|conn| objects::object_ref_meta(conn, &snapshot))
            .expect("snapshot meta");
        let manifest_meta = self
            .db
            .read(|conn| objects::object_ref_meta(conn, &manifest))
            .expect("manifest meta");
        let data_refs = vec![proto_ref(&snapshot_meta)];
        let refs_input = proto::refs_digest_input(&data_refs).expect("refs digest input");
        let refs_digest = sha256(&refs_input);
        let commit = proto::CheckpointCommit {
            vault_id: self.vault_id,
            checkpoint_id: cid,
            epoch,
            covered_relay_seq: covered,
            manifest_object_id: manifest,
            manifest_digest: manifest_meta.object_digest,
            refs_digest,
            producer_device_id: self.device,
            validation_version: 1,
        };
        let message = commit.encode().expect("checkpoint commit cde");
        let signing_input =
            proto::checkpoint_signing_input(&commit).expect("checkpoint signing input");
        let signature = self
            .device_signing_key
            .sign(&signing_input)
            .to_bytes()
            .to_vec();
        let handle = self.db.clone();
        let clock = self.clock.clone();
        let ctx = self.device_ctx();
        handle
            .write_blocking(move |conn| {
                checkpoints::publish(
                    conn,
                    clock.as_ref(),
                    &relay::verify::Ed25519Hook,
                    &ctx,
                    &cid,
                    &checkpoints::PublishRequest {
                        covered_seq: covered,
                        manifest_oid: manifest,
                        refs_digest,
                        signed_commit: &message,
                        signature: &signature,
                    },
                )
            })
            .expect("publish signed checkpoint");
        cid
    }

    /// 登记一个 staging key slot（不经 HTTP，供 realm 可见性用例使用）。
    pub fn put_staging_slot(&self, kind: SlotKind) -> Hash32 {
        let epoch = self.vault().active_epoch;
        let slot_id = random_object_id();
        let descriptor = slot_descriptor(slot_id, self.vault_id, epoch, kind, BASE_TIME as u64);
        let envelope = key_slot_envelope(self.vault_id, slot_id, epoch, &descriptor, 64);
        self.put_part(&envelope, None).expect("staging slot part");
        self.complete_object(&slot_id, &envelope)
            .expect("staging slot complete");
        let ctx = self.device_ctx();
        let handle = self.db.clone();
        let clock = self.clock.clone();
        let hook = self.hook.clone();
        handle
            .write_blocking(move |conn| {
                relay::slots::put(
                    conn,
                    clock.as_ref(),
                    hook.as_ref(),
                    &ctx,
                    &relay::slots::SlotPutRequest {
                        slot_id,
                        epoch,
                        kind,
                        descriptor,
                        signed_message: b"staging-slot".to_vec(),
                        signature: b"slot-signature".to_vec(),
                    },
                )
            })
            .expect("put staging slot");
        slot_id
    }

    // ---------------- 轮换 ----------------

    pub fn begin_rotation(&self, revoke: &[Id16]) -> rotations::RotationView {
        self.begin_rotation_at(revoke, [5u8; 16])
    }

    /// 指定 request_id 的 begin（多次轮换/幂等测试用；同 id 不同 body 会 409）。
    pub fn begin_rotation_at(&self, revoke: &[Id16], request_id: Id16) -> rotations::RotationView {
        let ctx = self.manager_ctx();
        let control_rev = self.vault().control_rev;
        let active_epoch = self.vault().active_epoch;
        let revoke = revoke.to_vec();
        let db = self.db.clone();
        let clock = self.clock.clone();
        let hook = self.hook.clone();
        db.write_blocking(move |conn| {
            rotations::begin(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &rotations::BeginRequest {
                    revoke: &revoke,
                    expected_epoch: active_epoch,
                    expected_control_rev: control_rev,
                    request_id,
                    path: "/v1/vaults/1/rotations",
                    body_sha256: sha256(&request_id),
                    signed: b"rotation-record",
                    signature: b"signature",
                },
            )
        })
        .expect("begin rotation")
    }

    /// 为候选 epoch 建两份 candidate checkpoint（轮换材料）。
    ///
    /// 候选 epoch 的对象必须用该 checkpoint 的维护 reservation 覆盖（ADR §13.2）。
    pub fn publish_candidate_baselines(&self, to_epoch: u64) -> (Id16, Id16) {
        let mut ids = Vec::new();
        for _ in 0..2 {
            let outcome = self.create_candidate_checkpoint(to_epoch);
            let rid = outcome.reservation_id;
            let snapshot = self.create_candidate_object(ObjectType::Snapshot, 1, to_epoch, &rid);
            let manifest = self.create_candidate_object(ObjectType::Manifest, 0, to_epoch, &rid);
            self.add_candidate_refs(outcome.checkpoint_id, snapshot, manifest);
            ids.push(self.publish_candidate_checkpoint(
                outcome.checkpoint_id,
                outcome.covered_seq,
                manifest,
            ));
        }
        (ids[0], ids[1])
    }

    /// 建一个候选 checkpoint（staging，占用轮换 reservation）。
    pub fn create_candidate_checkpoint(&self, to_epoch: u64) -> checkpoints::CreateOutcome {
        let ctx = self.device_ctx();
        let db = self.db.clone();
        let clock = self.clock.clone();
        db.write_blocking(move |conn| {
            checkpoints::create(conn, clock.as_ref(), &ctx, 4096, Some(to_epoch), None, None)
        })
        .expect("create candidate checkpoint")
    }

    /// 给候选 checkpoint 加 refs（允许两个 cid 引用同一对象——独立性负例用）。
    pub fn add_candidate_refs(&self, cid: Id16, snapshot: Hash32, manifest: Hash32) {
        self.db
            .write_blocking(move |conn| {
                checkpoints::add_refs(
                    conn,
                    1024,
                    &cid,
                    &[(snapshot, RefRole::Data), (manifest, RefRole::Manifest)],
                )
            })
            .expect("candidate refs");
    }

    /// 发布候选 checkpoint（测试 hook，状态应是 candidate）。
    ///
    /// §28.1 条目 3 要求 activate 校验 "commit/摘要绑定"，因此这里构造**真实**
    /// `CheckpointCommit`（绑定 cid/S/manifest/refs_digest），而不是任意字节。
    pub fn publish_candidate_checkpoint(&self, cid: Id16, covered: u64, manifest: Hash32) -> Id16 {
        let checkpoint = self.checkpoint_row(&cid);
        let data_refs: Vec<proto::ObjectRefV1> = self
            .db
            .read(move |conn| {
                let mut refs = Vec::new();
                for (object_id, role) in checkpoints::refs(conn, &cid)? {
                    if role == RefRole::Data {
                        refs.push(proto_ref(&objects::object_ref_meta(conn, &object_id)?));
                    }
                }
                Ok(refs)
            })
            .expect("candidate data refs");
        let refs_input = proto::refs_digest_input(&data_refs).expect("refs digest input");
        let refs_digest = sha256(&refs_input);
        let manifest_digest = self
            .db
            .read(move |conn| objects::object_ref_meta(conn, &manifest))
            .expect("manifest meta")
            .object_digest;
        let commit = proto::CheckpointCommit {
            vault_id: self.vault_id,
            checkpoint_id: cid,
            epoch: checkpoint.epoch,
            covered_relay_seq: covered,
            manifest_object_id: manifest,
            manifest_digest,
            refs_digest,
            producer_device_id: self.device,
            validation_version: 1,
        };
        let signed_commit = commit.encode().expect("candidate commit cde");
        let ctx = self.device_ctx();
        let db = self.db.clone();
        let clock = self.clock.clone();
        let hook = self.hook.clone();
        db.write_blocking(move |conn| {
            checkpoints::publish(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &cid,
                &checkpoints::PublishRequest {
                    covered_seq: covered,
                    manifest_oid: manifest,
                    refs_digest,
                    signed_commit: &signed_commit,
                    signature: b"signature",
                },
            )
        })
        .expect("publish candidate");
        cid
    }

    /// 候选 epoch 对象：必须带 reservation 上传。
    pub fn create_candidate_object(
        &self,
        object_type: ObjectType,
        generation: u64,
        key_epoch: u64,
        reservation: &Id16,
    ) -> Hash32 {
        let object_id = random_object_id();
        let envelope = single_part_envelope_at(
            object_type,
            self.vault_id,
            object_id,
            generation,
            0,
            if object_type == ObjectType::Snapshot {
                64
            } else {
                32
            },
            key_epoch,
        );
        self.put_part(&envelope, Some(reservation))
            .expect("candidate part");
        self.complete_object(&object_id, &envelope)
            .expect("candidate complete");
        object_id
    }

    /// stage 一份 grant（轮换用）。
    pub fn stage_grant(&self, rotation: &Id16, recipient: &Id16) {
        let ctx = self.manager_ctx();
        let rotation = *rotation;
        let recipient = *recipient;
        let db = self.db.clone();
        let hook = self.hook.clone();
        let clock = self.clock.clone();
        let manager_rev = self.vault().manager_rev;
        db.write_blocking(move |conn| {
            rotations::stage_grant(
                conn,
                hook.as_ref(),
                &ctx,
                &rotation,
                &recipient,
                &rotations::GrantInput {
                    grant_id: [3u8; 16],
                    hpke_enc: [1u8; 32],
                    hpke_ciphertext: b"ciphertext",
                    signed_header: b"aad||enc||hash",
                    signature: b"signature",
                    manager_rev,
                },
                clock.as_ref(),
            )
        })
        .expect("stage grant");
    }

    /// 构造绑定本次 rotation 的 §27.2 转换记录（可指定 old/new rev 用于负例）。
    pub fn manager_transition(
        &self,
        rotation: &rotations::RotationRow,
        old_rev: u64,
        new_rev: u64,
        new_pubkey: [u8; 32],
    ) -> Vec<u8> {
        let vault = self.vault();
        rotations::ManagerTransition {
            vault_id: self.vault_id,
            instance_id: vault.instance_id,
            rotation_id: rotation.rotation_id,
            from_epoch: rotation.from_epoch,
            to_epoch: rotation.to_epoch,
            old_rev,
            new_rev,
            new_pubkey,
            covered_seq: rotation.freeze_seq,
        }
        .encode()
        .expect("manager transition cde")
    }

    fn default_new_manager(&self, rotation: &rotations::RotationRow) -> rotations::NewManager {
        let vault = self.vault();
        rotations::NewManager {
            transition: self.manager_transition(
                rotation,
                vault.manager_rev,
                vault.manager_rev + 1,
                [0x5Au8; 32],
            ),
            old_signature: Some(vec![0x11; 64]),
            new_signature: Some(vec![0x22; 64]),
            // T 由 activate_rotation_inner 在拿到被指名的 pair 后补齐（需要 DB 事实）。
            activation_t: None,
            old_t_signature: None,
            new_t_signature: None,
        }
    }

    /// 候选 epoch 的自动提名对（role 优先、S 倒序；不足两份 → None）。
    pub fn candidate_pair(&self, rotation: &Id16) -> Option<(Id16, Id16)> {
        let row = self.rotation_view(rotation).rotation;
        let candidates = self
            .db
            .read(move |conn| checkpoints::candidates_for_epoch(conn, row.to_epoch))
            .expect("candidates");
        if candidates.len() < 2 {
            return None;
        }
        Some((candidates[0].checkpoint_id, candidates[1].checkpoint_id))
    }

    pub fn activate_rotation(&self, rotation: &Id16) -> Result<rotations::ActivateOutcome> {
        self.activate_rotation_at(rotation, [6u8; 16])
    }

    /// 指定 request_id 的 activate（多次轮换必须用不同 id，避免 dedup 回放上一轮）。
    pub fn activate_rotation_at(
        &self,
        rotation: &Id16,
        request_id: Id16,
    ) -> Result<rotations::ActivateOutcome> {
        let row = self.rotation_view(rotation).rotation;
        let new_manager = self.default_new_manager(&row);
        self.activate_rotation_inner(rotation, None, Some(new_manager), request_id)
    }

    /// 显式指名两个 cid 的 activate（独立性负例/多候选用例）。
    pub fn activate_rotation_named(
        &self,
        rotation: &Id16,
        primary: Id16,
        fallback: Id16,
    ) -> Result<rotations::ActivateOutcome> {
        let row = self.rotation_view(rotation).rotation;
        let new_manager = self.default_new_manager(&row);
        self.activate_rotation_named_with_manager(rotation, primary, fallback, new_manager)
    }

    pub fn activate_rotation_named_with_manager(
        &self,
        rotation: &Id16,
        primary: Id16,
        fallback: Id16,
        new_manager: rotations::NewManager,
    ) -> Result<rotations::ActivateOutcome> {
        self.activate_rotation_inner(
            rotation,
            Some((primary, fallback)),
            Some(new_manager),
            [6u8; 16],
        )
    }

    /// 缺 new_manager → 必须 409（§27.2：不允许绕过新管理权要求）。
    pub fn activate_rotation_without_manager(
        &self,
        rotation: &Id16,
    ) -> Result<rotations::ActivateOutcome> {
        self.activate_rotation_inner(rotation, None, None, [6u8; 16])
    }

    /// 不带任何候选 cid 的 activate（§28.1 条目 2：必须指名两个 cid → 409 INCOMPLETE）。
    pub fn activate_rotation_unnamed(&self, rotation: &Id16) -> Result<rotations::ActivateOutcome> {
        let row = self.rotation_view(rotation).rotation;
        let new_manager = self.default_new_manager(&row);
        let ctx = self.manager_ctx();
        let rotation = *rotation;
        let control_rev = self.vault().control_rev;
        let db = self.db.clone();
        let clock = self.clock.clone();
        let hook = self.hook.clone();
        db.write_blocking(move |conn| {
            rotations::activate(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &rotation,
                &rotations::ActivateRequest {
                    request_id: [6u8; 16],
                    path: "/v1/vaults/1/rotations/1/activate",
                    body_sha256: sha256(b"activate"),
                    expected_control_rev: control_rev,
                    signed: b"activate-record",
                    signature: b"signature",
                    new_manager: Some(new_manager),
                    primary_checkpoint: None,
                    fallback_checkpoint: None,
                    member_cert_digest: None,
                    grants_digest: None,
                    slot_set_digest: None,
                    asset_set_digest: None,
                },
            )
        })
    }

    /// activate 的唯一入口（测试侧）：自动提名或显式指名两个 cid。
    fn activate_rotation_inner(
        &self,
        rotation: &Id16,
        named: Option<(Id16, Id16)>,
        new_manager: Option<rotations::NewManager>,
        request_id: Id16,
    ) -> Result<rotations::ActivateOutcome> {
        let pair = named.or_else(|| self.candidate_pair(rotation));
        let ctx = self.manager_ctx();
        let rotation = *rotation;
        let control_rev = self.vault().control_rev;
        let db = self.db.clone();
        let clock = self.clock.clone();
        let hook = self.hook.clone();
        db.write_blocking(move |conn| {
            let row = rotations::get(conn, &rotation)?;
            let (_, grants_digest) = rotations::grants_digest(conn, row.to_epoch)?;
            // 默认在测试侧补齐与 DB 事实一致的 activation T（签名用占位值，TestHook 接受）。
            // 负例的 transition 本身可能不合法，构造失败时留空，由真实 activate 先拒绝
            // transition（保持各负例原本的 409 语义）。
            let mut new_manager = new_manager;
            if let (Some(manager), Some((primary, fallback))) = (new_manager.as_mut(), pair) {
                if manager.activation_t.is_none() {
                    let filled = (|| -> Option<Vec<u8>> {
                        let transition =
                            rotations::ManagerTransition::decode(&manager.transition).ok()?;
                        let vault = vault::load(conn).ok()?;
                        let activation_t =
                            activation_t_for(conn, &vault, &row, &transition, &primary, &fallback)
                                .ok()?;
                        activation_t.encode().ok()
                    })();
                    if let Some(bytes) = filled {
                        manager.activation_t = Some(bytes);
                        manager.old_t_signature = Some(vec![0x33; 64]);
                        manager.new_t_signature = Some(vec![0x44; 64]);
                    }
                }
            }
            rotations::activate(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &rotation,
                &rotations::ActivateRequest {
                    request_id,
                    path: "/v1/vaults/1/rotations/1/activate",
                    body_sha256: sha256(b"activate"),
                    expected_control_rev: control_rev,
                    signed: b"activate-record",
                    signature: b"signature",
                    new_manager,
                    primary_checkpoint: pair.map(|(primary, _)| primary),
                    fallback_checkpoint: pair.map(|(_, fallback)| fallback),
                    member_cert_digest: Some(rotations::member_cert_digest(conn)?),
                    grants_digest: Some(grants_digest),
                    slot_set_digest: Some(rotations::slot_set_digest(conn, row.to_epoch)?),
                    asset_set_digest: row.asset_set_digest,
                },
            )
        })
    }

    /// 带管理权转换的 activate（测试双签/rev 校验用；hook 仍是 fixture 的 TestHook，
    /// 因此只覆盖"rev 不匹配 / 缺签名"这类在调用 hook 前就拒绝的路径）。
    pub fn activate_rotation_with_manager(
        &self,
        rotation: &Id16,
        new_manager: rotations::NewManager,
    ) -> Result<rotations::ActivateOutcome> {
        self.activate_rotation_inner(rotation, None, Some(new_manager), [6u8; 16])
    }

    pub fn gc(&self) -> relay::gc::GcOutcome {
        let db = self.db.clone();
        let clock = self.clock.clone();
        db.write_blocking(move |conn| relay::gc::tick(conn, clock.as_ref()))
            .expect("gc tick")
    }

    pub fn clock_now(&self) -> i64 {
        self.clock.now_secs()
    }

    /// checkpoint 的维护 reservation id（轮换候选对象/槽要用）。
    pub fn checkpoint_reservation(&self, checkpoint: &Id16) -> Id16 {
        let checkpoint = *checkpoint;
        self.db
            .read(move |conn| {
                let bytes: Vec<u8> = conn.query_row(
                    "SELECT reservation_id FROM maintenance_reservations
                     WHERE resource_id = ?1 AND vault_id = ?",
                    (checkpoint.as_slice(), conn.scope().as_slice()),
                    |row| row.get(0),
                )?;
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("reservation_id"))
            })
            .expect("checkpoint reservation")
    }

    /// 轮换维护 reservation id（候选对象/新槽共享同一份预算）。
    pub fn rotation_reservation(&self, rotation: &Id16) -> Id16 {
        let rotation = *rotation;
        self.db
            .read(move |conn| {
                let bytes: Vec<u8> = conn.query_row(
                    "SELECT reservation_id FROM maintenance_reservations
                     WHERE purpose = 'rotation' AND resource_id = ?1 AND state = 'active'
                       AND vault_id = ?",
                    (rotation.as_slice(), conn.scope().as_slice()),
                    |row| row.get(0),
                )?;
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| Error::Invariant("reservation_id"))
            })
            .expect("rotation reservation")
    }

    /// 为候选 epoch 发布新槽（password + recovery，ADR §8 步 5）。
    pub fn stage_candidate_slots(&self, to_epoch: u64, reservation: &Id16) -> (Hash32, Hash32) {
        let mut ids = Vec::new();
        for kind in [SlotKind::Password, SlotKind::Recovery] {
            let slot_id = random_object_id();
            let descriptor =
                slot_descriptor(slot_id, self.vault_id, to_epoch, kind, BASE_TIME as u64);
            let envelope = key_slot_envelope(self.vault_id, slot_id, to_epoch, &descriptor, 64);
            self.put_part(&envelope, Some(reservation))
                .expect("slot part");
            self.complete_object(&slot_id, &envelope)
                .expect("slot complete");
            let ctx = self.device_ctx();
            let db = self.db.clone();
            let clock = self.clock.clone();
            let hook = self.hook.clone();
            let descriptor = descriptor.clone();
            db.write_blocking(move |conn| {
                relay::slots::put(
                    conn,
                    clock.as_ref(),
                    hook.as_ref(),
                    &ctx,
                    &relay::slots::SlotPutRequest {
                        slot_id,
                        epoch: to_epoch,
                        kind,
                        descriptor,
                        signed_message: b"slot-proof".to_vec(),
                        signature: b"slot-sig".to_vec(),
                    },
                )
            })
            .expect("slot put");
            ids.push(slot_id);
        }
        (ids[0], ids[1])
    }

    // ---------------- GC / 断言辅助 ----------------

    /// 持久 GC 计划 `(epoch, cut_seq, eligible_at)`。
    pub fn gc_plans(&self) -> Vec<(u64, u64, i64)> {
        self.db
            .read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT epoch, cut_seq, eligible_at FROM gc_plans
                     WHERE vault_id = ?1 ORDER BY cut_seq ASC",
                )?;
                let rows: Vec<(u64, u64, i64)> = stmt
                    .query_map([conn.scope().as_slice()], |row| {
                        Ok((
                            u64::try_from(row.get::<_, i64>(0)?).expect("epoch"),
                            u64::try_from(row.get::<_, i64>(1)?).expect("cut"),
                            row.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<std::result::Result<_, _>>()?;
                Ok(rows)
            })
            .expect("gc plans")
    }

    pub fn log_seqs(&self) -> Vec<u64> {
        self.db
            .read(|conn| {
                let mut stmt = conn
                    .prepare("SELECT relay_seq FROM log WHERE vault_id = ?1 ORDER BY relay_seq")?;
                let rows: Vec<u64> = stmt
                    .query_map([conn.scope().as_slice()], |row| {
                        Ok(u64::try_from(row.get::<_, i64>(0)?).expect("seq"))
                    })?
                    .collect::<std::result::Result<_, _>>()?;
                Ok(rows)
            })
            .expect("log seqs")
    }

    pub fn object_row(&self, object_id: &Hash32) -> objects::ObjectRow {
        self.db
            .read(|conn| objects::load(conn, object_id))
            .expect("object row")
    }

    pub fn checkpoint_row(&self, checkpoint_id: &Id16) -> checkpoints::CheckpointRow {
        self.db
            .read(|conn| checkpoints::get(conn, checkpoint_id))
            .expect("checkpoint row")
    }

    /// 原始 SQL 写（只用于"模拟崩溃点/损坏"的测试，业务路径不走这里）。
    pub fn raw_write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&ScopeConn<'_>) -> Result<T> + Send + 'static,
    {
        self.db.write_blocking(f)
    }

    /// 插入一台已批准设备（模拟管理设备 approve 之后的状态）。
    pub fn add_device(&self, device_id: Id16) {
        self.add_device_with(device_id, &[0x00], "active");
    }

    /// 插入一台设备行（指定证书与状态；pending 用空证书，模拟 redeem 后的占位）。
    pub fn add_device_with(&self, device_id: Id16, certificate: &[u8], status: &str) {
        let now = self.clock_now();
        let certificate = certificate.to_vec();
        let status = status.to_owned();
        self.db
            .write_blocking(move |conn| {
                conn.execute(
                    "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate,
                                        status, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    (
                        conn.scope().as_slice(),
                        device_id.as_slice(),
                        [4u8; 32].as_slice(),
                        [5u8; 32].as_slice(),
                        certificate,
                        status,
                        now,
                    ),
                )?;
                Ok(())
            })
            .expect("add device");
    }

    /// 设备行快照（吊销用例要断言"只改 status/revoked_at，证书/公钥保留"）。
    pub fn device_snapshot(&self, device_id: &Id16) -> Option<DeviceSnapshot> {
        let device_id = *device_id;
        self.db
            .read(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT status, revoked_at, certificate, sign_pk, hpke_pk,
                                certificate_signature
                         FROM devices WHERE device_id = ?1",
                        [device_id.as_slice()],
                        |row| {
                            Ok(DeviceSnapshot {
                                status: row.get(0)?,
                                revoked_at: row.get(1)?,
                                certificate: row.get(2)?,
                                sign_pk: row.get(3)?,
                                hpke_pk: row.get(4)?,
                                certificate_signature: row.get(5)?,
                            })
                        },
                    )
                    .optional()?;
                Ok(row)
            })
            .expect("device snapshot")
    }

    /// 当前 ACTIVE 设备集合（排序，等价于成员规划集合）。
    pub fn active_device_ids(&self) -> Vec<Id16> {
        self.db
            .read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT device_id FROM devices
                     WHERE status = 'active' AND vault_id = ?1 ORDER BY device_id",
                )?;
                let rows: Vec<Vec<u8>> = stmt
                    .query_map([conn.scope().as_slice()], |row| row.get::<_, Vec<u8>>(0))?
                    .collect::<std::result::Result<_, _>>()?;
                rows.into_iter()
                    .map(|bytes| {
                        bytes
                            .as_slice()
                            .try_into()
                            .map_err(|_| Error::Invariant("device_id"))
                    })
                    .collect()
            })
            .expect("active devices")
    }

    /// 控制链记录条数（断言"吊销不追加控制记录"）。
    pub fn control_count(&self) -> i64 {
        self.db
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM control_commits WHERE vault_id = ?1",
                    [conn.scope().as_slice()],
                    |row| row.get(0),
                )?)
            })
            .expect("control count")
    }

    pub fn bootstrap(&self) -> Result<checkpoints::BootstrapOutcome> {
        let ctx = self.device_ctx();
        let db = self.db.clone();
        let clock = self.clock.clone();
        db.write_blocking(move |conn| checkpoints::bootstrap(conn, clock.as_ref(), &ctx, false))
    }

    pub fn renew_lease(&self, lease_id: &Id16) -> Result<checkpoints::LeaseRow> {
        let ctx = self.device_ctx();
        let lease_id = *lease_id;
        let db = self.db.clone();
        let clock = self.clock.clone();
        db.write_blocking(move |conn| {
            checkpoints::renew_lease(conn, clock.as_ref(), &ctx, &lease_id)
        })
    }

    pub fn abort_rotation(&self, rotation: &Id16) -> Result<rotations::RotationView> {
        let ctx = self.manager_ctx();
        let rotation = *rotation;
        let control_rev = self.vault().control_rev;
        let db = self.db.clone();
        let clock = self.clock.clone();
        let hook = self.hook.clone();
        db.write_blocking(move |conn| {
            rotations::abort(
                conn,
                clock.as_ref(),
                hook.as_ref(),
                &ctx,
                &rotation,
                &rotations::AbortRequest {
                    request_id: [7u8; 16],
                    path: "/v1/vaults/1/rotations/1/abort",
                    body_sha256: sha256(b"abort"),
                    expected_control_rev: control_rev,
                    signed: b"abort-record",
                    signature: b"signature",
                },
            )
        })
    }

    /// 管理写：用**真实管理私钥**对 canonical body 做管理签名（§4/§26）。
    ///
    /// 签名输入与 relay `verify.rs` 完全同构：
    /// `MN/v1/manage\0 || instance || vault || request_id || control_rev(BE64) || SHA256(CDE{method,path,body})`。
    pub fn sign_management(
        &self,
        method: proto::HttpMethod,
        path: &str,
        request_id: Id16,
        expected_control_rev: u64,
        body: &[u8],
    ) -> Vec<u8> {
        let key = self
            .manager_signing_key
            .as_ref()
            .expect("本用例需要 Fixture::with_real_manager 的管理私钥");
        let vault = self.vault();
        let request_cde =
            proto::management_request_cde(method, path, body).expect("management request cde");
        let body_digest = sha256(&request_cde);
        let input = proto::management_signing_input(
            &vault.instance_id,
            &self.vault_id,
            &request_id,
            expected_control_rev,
            &body_digest,
        )
        .expect("management signing input");
        key.sign(&input).to_bytes().to_vec()
    }

    /// 管理签名包装 `{0: body, 1: signature}`（`wire::Signed`）。
    pub fn signed_wrapper(body: Vec<u8>, signature: Vec<u8>) -> wire::CdeValue {
        wire::map(vec![(0, wire::bytes(body)), (1, wire::bytes(signature))])
    }

    pub fn rotation_view(&self, rotation: &Id16) -> rotations::RotationView {
        self.db
            .read(|conn| rotations::view(conn, rotation))
            .expect("rotation view")
    }

    pub fn latest_grant(&self, device: &Id16) -> Option<rotations::GrantRow> {
        self.db
            .read(|conn| rotations::latest_grant(conn, device))
            .expect("latest grant")
    }
}

// ---------------- 纯构造辅助 ----------------

/// 随机 object id（测试用真实 CSPRNG 语义；每次调用都不同）。
pub fn random_object_id() -> Hash32 {
    ids::random_hash32().expect("csprng")
}

/// 单片合法信封（明文 = `plain_len` 个 0xAA，密文 = 明文 + 16B tag）。
pub fn single_part_envelope(
    object_type: ObjectType,
    vault_id: Id16,
    object_id: Hash32,
    generation: u64,
    sender_seq: u64,
    plain_len: u64,
) -> Vec<u8> {
    single_part_envelope_at(
        object_type,
        vault_id,
        object_id,
        generation,
        sender_seq,
        plain_len,
        1,
    )
}

/// 指定 key_epoch 的单片信封。
pub fn single_part_envelope_at(
    object_type: ObjectType,
    vault_id: Id16,
    object_id: Hash32,
    generation: u64,
    sender_seq: u64,
    plain_len: u64,
    key_epoch: u64,
) -> Vec<u8> {
    assert!((1..=envelope::NON_ASSET_CAP_PLAIN).contains(&plain_len));
    let header = Header {
        protocol_version: envelope::PROTOCOL_VERSION,
        object_type,
        cipher_suite: envelope::CIPHER_SUITE_XCHACHA20_POLY1305,
        flags: 0,
        vault_id,
        object_id,
        message_id: message_id_for(&object_id),
        producer_device_id: [2u8; 16],
        generation,
        key_epoch,
        sender_seq,
        part_index: 0,
        part_count: 1,
        total_plaintext_len: plain_len,
        this_plaintext_len: u32::try_from(plain_len).expect("fits"),
        ciphertext_len: u32::try_from(plain_len + 16).expect("fits"),
        context_hash: if object_type == ObjectType::KeySlot {
            [3u8; 32]
        } else {
            [0u8; 32]
        },
        nonce: [5u8; 24],
    };
    header.encode(&vec![0xAAu8; plain_len as usize + 16])
}

/// 对象级唯一 `message_id`（schema 有 `UNIQUE(epoch, message_id)`；同一对象的所有片必须相同）。
pub fn message_id_for(object_id: &Hash32) -> Id16 {
    let digest = sha256(object_id);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// 用 relay 自己的 object_digest 口径算摘要。
pub fn digest_of(parts: &[Vec<u8>]) -> Result<Hash32> {
    let hashes: Vec<(u32, Hash32)> = parts
        .iter()
        .enumerate()
        .map(|(index, part)| (index as u32, sha256(part)))
        .collect();
    envelope::object_digest(parts.len() as u32, &hashes)
}

/// 测试 commit：前 32B = 摘要（`TestHook` 据此校验）。
pub fn commit_for(digest: &Hash32) -> Vec<u8> {
    let mut commit = digest.to_vec();
    commit.extend_from_slice(b"test-commit");
    commit
}

/// 控制面 CBOR 请求体。
pub fn cbor_body(fields: wire::Fields) -> Vec<u8> {
    wire::encode(fields).expect("cbor encode")
}

/// 解析控制面 CBOR 响应/请求。
pub fn cbor_fields(bytes: &[u8]) -> wire::Fields {
    wire::decode(bytes).expect("cbor decode")
}

/// 构造 slot 描述符（CDE，§15 Slot）。
pub fn slot_descriptor(
    slot_id: Hash32,
    vault_id: Id16,
    epoch: u64,
    kind: SlotKind,
    created_at: u64,
) -> Vec<u8> {
    use relay::proto::{Kdf, SlotDescriptorV1, SlotKind as ProtoKind};
    let descriptor = SlotDescriptorV1 {
        protocol_version: 1,
        slot_id,
        vault_id,
        key_epoch: epoch,
        kind: match kind {
            SlotKind::Password => ProtoKind::Password,
            SlotKind::Recovery => ProtoKind::Recovery,
        },
        wrap_suite: 1,
        kdf: Kdf::RecoveryHkdf,
        created_at,
        manager_rev: 1,
    };
    descriptor.encode().expect("slot descriptor")
}

/// KEY_SLOT 信封（context_hash = SHA256(CDE(descriptor))，§2）。
pub fn key_slot_envelope(
    vault_id: Id16,
    slot_id: Hash32,
    epoch: u64,
    descriptor: &[u8],
    plain_len: u64,
) -> Vec<u8> {
    let header = Header {
        protocol_version: envelope::PROTOCOL_VERSION,
        object_type: ObjectType::KeySlot,
        cipher_suite: envelope::CIPHER_SUITE_XCHACHA20_POLY1305,
        flags: 0,
        vault_id,
        object_id: slot_id,
        message_id: message_id_for(&slot_id),
        producer_device_id: [2u8; 16],
        generation: 0,
        key_epoch: epoch,
        sender_seq: 0,
        part_index: 0,
        part_count: 1,
        total_plaintext_len: plain_len,
        this_plaintext_len: u32::try_from(plain_len).expect("fits"),
        ciphertext_len: u32::try_from(plain_len + 16).expect("fits"),
        context_hash: sha256(descriptor),
        nonce: [5u8; 24],
    };
    header.encode(&vec![0xAAu8; plain_len as usize + 16])
}

/// 系统时钟路径的最小烟测（保证 `SystemClock` 在本机可用）。
pub fn system_now() -> i64 {
    SystemClock.now_secs()
}

/// 便于断言 `Id16`/`Hash32` 的十六进制预览。
pub fn preview(bytes: &[u8]) -> String {
    ids::hex_lower(&bytes[..bytes.len().min(8)])
}
