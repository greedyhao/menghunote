//! relay 集成测试公共夹具。
//!
//! 约定：
//! - 每个用例一个**独立 SQLite 文件**（`CARGO_TARGET_TMPDIR/<pid>-<counter>/`，同进程并行也不冲突）；
//! - 时间一律 `FakeClock`，**不 sleep**（ADR 0006 §9）；
//! - 测试用 `TestHook` 只做"摘要相等"的弱校验（真实验签在 `relay::verify` 的单元测试里）；
//! - 全部走公开 API（`Db::write_blocking` + 领域模块），除"模拟崩溃点"外不直接改表。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub use menghunote_relay as relay;

use relay::auth::{self, AuthContext, Realm};
use relay::checkpoints;
use relay::config::RelayConfig;
use relay::db::Db;
use relay::envelope::{self, Header, ObjectType};
use relay::error::{Error, Result};
use relay::ids::{self, sha256, Hash32, Id16};
use relay::objects;
use relay::quota;
use relay::refs::RefRole;
use relay::rotations;
use relay::sig::{Expectation, SignDomain, SignatureCheck, SignatureHook};
use relay::slots::SlotKind;
use relay::time::{Clock, FakeClock, SystemClock};
use relay::vault::{self, Vault};
use relay::wire;

/// 固定基准时刻（2027-01-15），所有 TTL 都用 FakeClock 推进。
pub const BASE_TIME: i64 = 1_800_000_000;

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
pub fn temp_db_path(label: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "relay-it-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir.join(format!("{label}.db"))
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

/// 测试夹具：一个已初始化的单 vault + 单设备。
pub struct Fixture {
    pub db: Db,
    pub clock: Arc<FakeClock>,
    pub hook: Arc<TestHook>,
    pub cfg: Arc<RelayConfig>,
    pub vault_id: Id16,
    pub device: Id16,
    pub manager_pub: [u8; 32],
}

impl Fixture {
    /// 默认配额：`Q = 1MiB`，峰值 `Q+R = 2MiB`。
    pub fn new(label: &str) -> Self {
        Self::with_limits(label, 1024 * 1024, 2 * 1024 * 1024)
    }

    /// 指定配额的夹具。
    pub fn with_limits(label: &str, normal_limit: i64, maintenance_limit: i64) -> Self {
        let clock = Arc::new(FakeClock::new(BASE_TIME));
        let hook = Arc::new(TestHook::new());
        let cfg = Arc::new(
            RelayConfig::new(temp_db_path(label))
                .with_setup_secret("setup-secret")
                .with_limits(normal_limit, maintenance_limit),
        );
        let db = Db::open(cfg.db.clone(), clock.clone()).expect("open db");
        let vault_id = [1u8; 16];
        let device = [2u8; 16];
        let manager_pub = [7u8; 32];
        // §17.1：首台设备证书必须是管理 key 签发的 CDE(DeviceCertV1)。
        let certificate = certificate_bytes(&vault_id, &device, &[3u8; 32], &[4u8; 32], 1);
        let init = vault::InitRequest {
            vault_id,
            device_id: device,
            sign_pk: [3u8; 32],
            hpke_pk: [4u8; 32],
            manager_pub,
            setup_secret: "setup-secret".to_string(),
            certificate,
            certificate_signature: b"certificate-sig".to_vec(),
        };
        let db_for_init = db.clone();
        let clock_for_init = clock.clone();
        let hook_for_init = hook.clone();
        let cfg_for_init = cfg.clone();
        db_for_init
            .write_blocking(move |conn| {
                vault::initialize(
                    conn,
                    clock_for_init.as_ref(),
                    &cfg_for_init,
                    hook_for_init.as_ref(),
                    &init,
                )
            })
            .expect("initialize vault");
        Self {
            db,
            clock,
            hook,
            cfg,
            vault_id,
            device,
            manager_pub,
        }
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
        }
    }

    pub fn manager_ctx(&self) -> AuthContext {
        AuthContext {
            realm: Realm::Management,
            subject: Realm::management_subject(&self.manager_pub),
            device_id: None,
            auth_rev: self.vault().auth_rev,
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
        let subject = self.device;
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
            .write_blocking(move |conn| checkpoints::create(conn, clock.as_ref(), &ctx, 4096, None))
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

    // ---------------- 轮换 ----------------

    pub fn begin_rotation(&self, revoke: &[Id16]) -> rotations::RotationView {
        let ctx = self.manager_ctx();
        let control_rev = self.vault().control_rev;
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
                    expected_epoch: 1,
                    expected_control_rev: control_rev,
                    request_id: [5u8; 16],
                    path: "/v1/vaults/1/rotations",
                    body_sha256: sha256(b"body"),
                    signed: b"rotation-record",
                    signature: b"signature",
                },
            )
        })
        .expect("begin rotation")
    }

    /// 为候选 epoch 建两份 published checkpoint（轮换材料）。
    ///
    /// 候选 epoch 的对象必须用该 checkpoint 的维护 reservation 覆盖（ADR §13.2）。
    pub fn publish_candidate_baselines(&self, to_epoch: u64) -> (Id16, Id16) {
        let ctx = self.device_ctx();
        let mut ids = Vec::new();
        for _ in 0..2 {
            let db = self.db.clone();
            let clock = self.clock.clone();
            let outcome = db
                .write_blocking(move |conn| {
                    checkpoints::create(conn, clock.as_ref(), &ctx, 4096, Some(to_epoch))
                })
                .expect("create candidate checkpoint");
            let cid = outcome.checkpoint_id;
            let rid = outcome.reservation_id;
            let snapshot = self.create_candidate_object(ObjectType::Snapshot, 1, to_epoch, &rid);
            let manifest = self.create_candidate_object(ObjectType::Manifest, 0, to_epoch, &rid);
            let db = self.db.clone();
            db.write_blocking(move |conn| {
                checkpoints::add_refs(
                    conn,
                    1024,
                    &cid,
                    &[(snapshot, RefRole::Data), (manifest, RefRole::Manifest)],
                )
            })
            .expect("candidate refs");
            let covered = outcome.covered_seq;
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
                        refs_digest: [8u8; 32],
                        signed_commit: b"candidate-commit",
                        signature: b"signature",
                    },
                )
            })
            .expect("publish candidate");
            ids.push(cid);
        }
        (ids[0], ids[1])
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
                    manager_rev: 1,
                },
                clock.as_ref(),
            )
        })
        .expect("stage grant");
    }

    pub fn activate_rotation(&self, rotation: &Id16) -> Result<rotations::ActivateOutcome> {
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
                    new_manager: None,
                },
            )
        })
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
                    "SELECT reservation_id FROM maintenance_reservations WHERE resource_id = ?1",
                    [checkpoint.as_slice()],
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
                     WHERE purpose = 'rotation' AND resource_id = ?1 AND state = 'active'",
                    [rotation.as_slice()],
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
                    "SELECT epoch, cut_seq, eligible_at FROM gc_plans ORDER BY cut_seq ASC",
                )?;
                let rows: Vec<(u64, u64, i64)> = stmt
                    .query_map([], |row| {
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
                let mut stmt = conn.prepare("SELECT relay_seq FROM log ORDER BY relay_seq")?;
                let rows: Vec<u64> = stmt
                    .query_map([], |row| {
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
        F: FnOnce(&rusqlite::Connection) -> Result<T> + Send + 'static,
    {
        self.db.write_blocking(f)
    }

    /// 插入一台已批准设备（模拟管理设备 approve 之后的状态）。
    pub fn add_device(&self, device_id: Id16) {
        let now = self.clock_now();
        self.db
            .write_blocking(move |conn| {
                conn.execute(
                    "INSERT INTO devices(device_id, sign_pk, hpke_pk, certificate, status, created_at)
                     VALUES (?1, ?2, ?3, X'00', 'active', ?4)",
                    (
                        device_id.as_slice(),
                        [4u8; 32].as_slice(),
                        [5u8; 32].as_slice(),
                        now,
                    ),
                )?;
                Ok(())
            })
            .expect("add device");
    }

    pub fn bootstrap(&self) -> Result<checkpoints::BootstrapOutcome> {
        let ctx = self.device_ctx();
        let db = self.db.clone();
        let clock = self.clock.clone();
        db.write_blocking(move |conn| checkpoints::bootstrap(conn, clock.as_ref(), &ctx))
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
