//! ADR 0013 **S2b**：受限初始化能力、rev1 强 pin、pending/journal 与启动对账。
//!
//! 全部用 `menghunote-dev-relay`（进程内真实 axum relay + 真实 HTTP + loopback），
//! **不连任何真实远端 relay**，也不依赖 TLS 服务端（TLS 负例见 `tests/tls_transport.rs`）。
//!
//! 覆盖：
//!
//! 1. NewGenesis：本地复算 rev1 `record_hash` → 与 relay 链尾**逐字节**一致 → 提交强 pin；
//! 2. 负例：错 rev1、伪造回执（换实例）、身份下界回滚、前缀不符 → 全部拒绝且不写 pin；
//! 3. FreshRecovery：record pin 与 bootstrap 锚**同视图**；
//! 4. VerifiedMigration：旧下界 pin → canonical pin；条件不足 → `NeedsTrust`/`Unsupported`；
//! 5. pending/journal 的崩溃阶段矩阵 + 启动对账（不重复 genesis/enroll）；
//! 6. 普通 `RelayContext::open` 在 pending 未完成时 fail-closed（初始化只能走 `InitSession`）。

use std::path::Path;

use menghunote_dev_relay::DevRelay;
use menghunote_storage::Vault;
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::bootstrap::BootstrapPins;
use menghunote_sync::control_pin::{
    encode_pin_v1, load_control_pin, BootstrapAnchorV1, PinAnchorV1, PinV1, META_CONTROL_PIN,
};
use menghunote_sync::genesis::{build_materials, rev1_record_hash, GenesisReceipt};
use menghunote_sync::http_relay::{
    Clock, ControlPin, HttpRelayClient, HttpRelayConfig, SystemClock,
};
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::init_journal::{
    begin_pending, load_pending, InitKind, InitPendingV1, InitPhase, META_INIT_PENDING,
};
use menghunote_sync::init_session::{reconcile, InitReconcileOutcome, InitSession};
use menghunote_sync::random::{array, OsRandom};
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::SyncError;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 在 `target/tmp` 下建独立测试根目录（AGENTS.md §6：不用 %TEMP%）。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("init-session-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

fn local_context(relay: &DevRelay) -> RelayContext {
    RelayContext::local_dev(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("local_dev context")
}

fn now() -> u64 {
    SystemClock.now_secs()
}

/// 初始化报告有意不实现 `Debug`（内含身份秘密），因此错误断言不能用 `expect_err`。
fn expect_init_err<T>(result: Result<T, SyncError>, label: &str) -> SyncError {
    match result {
        Ok(_) => panic!("{label} 必须失败"),
        Err(error) => error,
    }
}

struct Materials {
    vault_id: [u8; 16],
    device_id: [u8; 16],
    materials: menghunote_sync::genesis::GenesisMaterials,
}

fn new_materials() -> Materials {
    let vault_id: [u8; 16] = array(&mut OsRandom).expect("vault id");
    let device_id: [u8; 16] = array(&mut OsRandom).expect("device id");
    let materials = build_materials(
        vault_id,
        device_id,
        "s2b-test-password",
        now(),
        &mut OsRandom,
    )
    .expect("genesis 材料");
    Materials {
        vault_id,
        device_id,
        materials,
    }
}

// ---------------------------------------------------------------------------
// 1) NewGenesis：rev1 强 pin 与 relay 链尾逐字节一致
// ---------------------------------------------------------------------------

#[test]
fn new_genesis_commits_a_rev1_strong_pin_matching_the_relay_head() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("genesis");
    let ctx = local_context(&relay);
    let m = new_materials();
    let session = InitSession::new_genesis(ctx.clone(), &m.materials).expect("会话");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");

    session.prepare_pending(&vault, now()).expect("pending");
    let receipt = session
        .request_genesis(&vault, &m.materials, relay.setup_secret())
        .expect("genesis");
    assert_eq!(receipt.control_rev, 1, "新库的第一条控制记录必为 rev1");
    let report = session
        .finalize_genesis(&vault, &m.materials, &receipt)
        .expect("finalize");
    assert_eq!(report.pin.vault_id, m.vault_id);
    assert_eq!(report.pin.instance_id, receipt.instance_id);
    assert_eq!(report.pin.record.rev, 1);
    let expected = rev1_record_hash(m.materials.certificate()).expect("本地复算");
    assert_eq!(
        report.pin.record.hash,
        Some(expected),
        "强 pin 必须用本地提交证书复算的 rev1 摘要"
    );
    assert_eq!(
        report.pin.bootstrap, None,
        "首次 genesis 没有 bootstrap 历史锚"
    );
    // 回执里的身份与 pin 绑定一致。
    assert_eq!(report.identity.pinned_instance, receipt.instance_id);
    assert_eq!(report.identity.pinned_manager, m.materials.manager_pub());

    // 初始化收尾：pending 只在 Done 后清理。
    session.finish(&vault).expect("finish");
    assert!(
        load_pending(&vault, Some(m.vault_id))
            .expect("读 pending")
            .is_none(),
        "初始化完成后必须清掉 pending"
    );
    let reloaded = load_control_pin(&vault, m.vault_id)
        .expect("读 pin")
        .expect("pin 必须 durable");
    assert_eq!(reloaded, report.pin);

    // 与 relay 的实现对齐：直接从 relay 读 rev1 链尾，必须与本地复算逐字节一致。
    let config = HttpRelayConfig {
        base_url: relay.base_url().to_owned(),
        api_version: RelayApiVersion::V1,
        vault_id: m.vault_id,
        device_id: m.device_id,
        manager_pub: Some(m.materials.manager_pub()),
    };
    let transport = ctx.transport().expect("transport");
    let mut client = HttpRelayClient::new(transport, config, m.materials.signing_key.clone())
        .with_control_pin(ControlPin::new(receipt.instance_id, 1));
    let (rev, head) = client.control_head().expect("relay 链尾");
    assert_eq!(
        (rev, head),
        (1, expected),
        "relay vault::init 的 rev1 record_hash 必须等于客户端本地复算值"
    );
}

/// 错 rev1（本地复算被替换）→ 拒绝且不写 pin、不回退 pending。
#[test]
fn wrong_rev1_hash_is_rejected_and_never_becomes_a_pin() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("wrong-rev1");
    let ctx = local_context(&relay);
    let m = new_materials();
    let session = InitSession::new_genesis(ctx, &m.materials)
        .expect("会话")
        .with_test_rev1_override([0xA5; 32]);
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
    session.prepare_pending(&vault, now()).expect("pending");
    let receipt = session
        .request_genesis(&vault, &m.materials, relay.setup_secret())
        .expect("genesis");
    let error = expect_init_err(
        session.finalize_genesis(&vault, &m.materials, &receipt),
        "错 rev1",
    );
    assert!(
        matches!(
            error,
            SyncError::Equivocation(_) | SyncError::NeedsTrust(_) | SyncError::RelayWire(_)
        ),
        "必须是链/前缀校验失败，实际 {error:?}"
    );
    assert!(
        session.current_pin(&vault).expect("读 pin").is_none(),
        "拒绝路径不得写入 pin"
    );
    assert_eq!(
        load_pending(&vault, Some(m.vault_id))
            .expect("读 pending")
            .expect("pending 仍在")
            .phase,
        InitPhase::GenesisRequested,
        "失败必须保留 pending 供对账，且阶段只进不退"
    );
    // 未提交 pin 时不允许清理 pending（清理只在 Done）。
    assert!(session.finish(&vault).is_err());
}

/// 伪造回执（换实例）：即使 genesis 本身成功，也不能把 pin 挂到另一个实例上。
#[test]
fn tampered_receipt_instance_is_rejected() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("tampered-receipt");
    let ctx = local_context(&relay);
    let m = new_materials();
    let session = InitSession::new_genesis(ctx, &m.materials).expect("会话");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
    session.prepare_pending(&vault, now()).expect("pending");
    let receipt = session
        .request_genesis(&vault, &m.materials, relay.setup_secret())
        .expect("genesis");
    let mut tampered: GenesisReceipt = receipt.clone();
    tampered.instance_id = [0x99; 16];
    let error = expect_init_err(
        session.finalize_genesis(&vault, &m.materials, &tampered),
        "换实例",
    );
    assert!(
        matches!(error, SyncError::RelayWire(_) | SyncError::InstanceMismatch),
        "必须拒绝实例不符：{error:?}"
    );
    assert!(session.current_pin(&vault).expect("读").is_none());
}

// ---------------------------------------------------------------------------
// 2) 客户端 pin 校验原语（初始化 finalize 与普通 open 共用）
// ---------------------------------------------------------------------------

/// 换实例 / 回退 / 前缀不符：客户端在**发请求/消费控制链**之前就拒绝（fail-closed）。
#[test]
fn control_pin_checks_reject_instance_rollback_and_wrong_prefix() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("pin-checks");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = menghunote_sync::dev_local::DevLocal::create(
        relay.base_url(),
        relay.setup_secret(),
        &a_path,
        "A",
    )
    .expect("A create");
    let pin = load_control_pin(&a.vault, a.vault_id)
        .expect("读 pin")
        .expect("create 已提交 rev1 pin");
    let real_hash = pin.record.hash.expect("rev1 hash");

    let build = |control_pin: ControlPin| {
        let ctx = local_context(&relay);
        let config = HttpRelayConfig {
            base_url: relay.base_url().to_owned(),
            api_version: RelayApiVersion::V1,
            vault_id: a.identity.vault_id,
            device_id: a.identity.device_id,
            manager_pub: Some(a.identity.pinned_manager),
        };
        let signing =
            menghunote_sync::device::DeviceSigningKey::from_seed(a.identity.ed25519_seed.clone());
        HttpRelayClient::new(ctx.transport().expect("transport"), config, signing)
            .with_control_pin(control_pin)
    };

    // (a) 换实例。
    let error = build(ControlPin::new([0x99; 16], 1).with_record_hash(real_hash))
        .verified_head()
        .expect_err("换实例必须拒绝");
    assert!(matches!(error, SyncError::RelayWire(_)), "{error:?}");

    // (b) 回退：本地 pin 比 relay head 更"新"。
    let error = build(ControlPin::new(pin.instance_id, 2).with_record_hash(real_hash))
        .verified_head()
        .expect_err("回退必须拒绝");
    assert!(matches!(error, SyncError::RelayWire(_)), "{error:?}");

    // (c) 同 rev 换 hash（换 genesis/重写链）。
    let error = build(ControlPin::new(pin.instance_id, 1).with_record_hash([0xAB; 32]))
        .verified_head()
        .expect_err("同 rev 换 hash 必须拒绝");
    assert!(matches!(error, SyncError::Equivocation(_)), "{error:?}");

    // (d) 正确的前缀可以被证明（同 rev 逐字节相等）。
    let mut good = build(ControlPin::new(pin.instance_id, 1).with_record_hash(real_hash));
    let head = good.verified_head().expect("正确前缀必须放行");
    assert_eq!(head.record_rev(), 1);
    assert_eq!(head.record_hash(), real_hash);
}

/// 高 rev 前进必须包含旧 pin 的前缀：B join 后 relay 前进到 rev2，rev1 的旧 hash 仍被证明。
#[test]
fn advanced_head_must_contain_the_pinned_prefix() {
    use menghunote_sync::dev_local::DevLocal;
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("prefix");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A").expect("A");
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("发布基线");
    let a_pin = load_control_pin(&a.vault, a.vault_id)
        .expect("读 pin")
        .expect("rev1 pin");
    let b_path = dir.path().join("b").join("vault.sqlite3");
    let _b = DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path)
        .expect("B join（推进控制链）");

    let ctx = local_context(&relay);
    let config = HttpRelayConfig {
        base_url: relay.base_url().to_owned(),
        api_version: RelayApiVersion::V1,
        vault_id: a.identity.vault_id,
        device_id: a.identity.device_id,
        manager_pub: Some(a.identity.pinned_manager),
    };
    let signing =
        menghunote_sync::device::DeviceSigningKey::from_seed(a.identity.ed25519_seed.clone());
    // 正确的 rev1 前缀：允许前进到 rev2。
    let mut client =
        HttpRelayClient::new(ctx.transport().expect("t"), config.clone(), signing.clone())
            .with_control_pin(
                ControlPin::new(a_pin.instance_id, a_pin.record.rev)
                    .with_record_hash(a_pin.record.hash.expect("hash")),
            );
    let head = client.verified_head().expect("rev1 前缀必须被包含");
    assert!(head.record_rev() >= 1);
    // 错误的旧 hash：前缀证明失败。
    let error = HttpRelayClient::new(ctx.transport().expect("t"), config, signing)
        .with_control_pin(
            ControlPin::new(a_pin.instance_id, a_pin.record.rev).with_record_hash([0xCD; 32]),
        )
        .verified_head()
        .expect_err("旧 hash 不在新链上必须拒绝");
    assert!(matches!(error, SyncError::Equivocation(_)), "{error:?}");
}

// ---------------------------------------------------------------------------
// 3) FreshRecovery：record pin 与 bootstrap 锚同视图
// ---------------------------------------------------------------------------

#[test]
fn fresh_recovery_commits_record_pin_in_the_same_view_as_the_bootstrap_anchor() {
    use menghunote_sync::dev_local::DevLocal;
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("recovery");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A").expect("A");
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("发布基线");

    let ctx = local_context(&relay);
    let seed = RecoverySeed::from_code(&a.recovery_code).expect("恢复码");
    let session = InitSession::new_fresh_recovery(ctx.clone(), a.vault_id, &seed).expect("会话");
    let vault = Vault::open(dir.path().join("b").join("vault.sqlite3")).expect("B vault");
    session.prepare_pending(&vault, now()).expect("pending");
    let recovered = session.recover_identity(&vault, now()).expect("recover");
    assert_eq!(recovered.identity.vault_id, a.vault_id);
    // 尚未提交 pin：pending 仍停在 genesis_requested。
    assert_eq!(
        load_pending(&vault, Some(a.vault_id))
            .expect("读")
            .expect("在")
            .phase,
        InitPhase::GenesisRequested
    );
    let pin = session
        .commit_recovery_pin(&vault, &recovered.identity)
        .expect("提交 pin");
    let pins = BootstrapPins::from_vault(&vault).expect("bootstrap pins");
    let anchor: BootstrapAnchorV1 = pins
        .control_anchor()
        .expect("锚解析")
        .expect("bootstrap 后必须有锚");
    assert_eq!(pin.bootstrap, Some(anchor), "record pin 必须带同一视图的锚");
    assert_eq!(pin.record.rev, anchor.rev);
    assert!(pin.record.hash.is_some());
    assert_eq!(pin.instance_id, recovered.identity.pinned_instance);
    session.finish(&vault).expect("finish");
    assert!(load_pending(&vault, Some(a.vault_id))
        .expect("读")
        .is_none());

    // 初始化完成后普通同步可以正常打开（pending 已清）。
    let mut pinned = ctx.open(&vault, &recovered.identity).expect("open");
    let members = pinned.client_mut().members().expect("成员视图");
    assert!(members
        .iter()
        .any(|member| member.device_id == recovered.identity.device_id));
}

// ---------------------------------------------------------------------------
// 4) VerifiedMigration
// ---------------------------------------------------------------------------

fn lower_bound_pin(vault_id: [u8; 16], pin: &PinV1) -> PinV1 {
    PinV1 {
        vault_id,
        instance_id: pin.instance_id,
        record: PinAnchorV1 {
            rev: pin.record.rev,
            hash: None,
        },
        bootstrap: pin.bootstrap,
    }
}

#[test]
fn verified_migration_upgrades_a_lower_bound_pin() {
    use menghunote_sync::dev_local::DevLocal;
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("migration");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A").expect("A");
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("发布基线");
    let b_path = dir.path().join("b").join("vault.sqlite3");
    let b = DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path).expect("B");

    // 已有完整 pin 时明确"无需迁移"（不重复写）。
    let ctx = local_context(&relay);
    let error = InitSession::new_verified_migration(ctx.clone(), &b.vault, &b.identity)
        .expect_err("完整 pin 不需要迁移");
    assert!(matches!(error, SyncError::Unsupported(_)), "{error:?}");

    // 模拟"旧实例"：record 锚只有下界（没有 hash），但保留 bootstrap 锚。
    let full = load_control_pin(&b.vault, a.vault_id)
        .expect("读")
        .expect("pin");
    let old = lower_bound_pin(a.vault_id, &full);
    b.vault
        .set_meta(META_CONTROL_PIN, &encode_pin_v1(&old).expect("编码"))
        .expect("降级 pin");

    // 降级后再迁移：重新验证控制链并写入带 record.hash 的 canonical pin。
    let session =
        InitSession::new_verified_migration(ctx, &b.vault, &b.identity).expect("迁移会话");
    let report = session
        .run_migration(&b.vault, &b.identity, now())
        .expect("迁移");
    assert!(report.pin.record.hash.is_some(), "迁移后必须带链尾 hash");
    assert_eq!(report.pin.bootstrap, old.bootstrap, "bootstrap 锚不得丢");
    assert_eq!(
        load_pending(&b.vault, Some(a.vault_id))
            .expect("读 pending")
            .expect("phase")
            .phase,
        InitPhase::PinCommitted,
        "run_migration 只推进到 pin_committed（清理要显式 finish）"
    );
    session.finish(&b.vault).expect("finish");
    assert!(load_pending(&b.vault, Some(a.vault_id))
        .expect("读")
        .is_none());
}

#[test]
fn verified_migration_requires_a_trust_anchor_for_remote_and_valid_identity() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("migration-negatives");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = menghunote_sync::dev_local::DevLocal::create(
        relay.base_url(),
        relay.setup_secret(),
        &a_path,
        "A",
    )
    .expect("A");
    // A 的 vault 没有 `sync.pinned_*`：把完整 pin 降级成只有下界的形态。
    let full = load_control_pin(&a.vault, a.vault_id)
        .expect("读")
        .expect("pin");
    let old = lower_bound_pin(a.vault_id, &full);
    a.vault
        .set_meta(META_CONTROL_PIN, &encode_pin_v1(&old).expect("编码"))
        .expect("写旧 pin");

    // remote_tls 且没有同 rev 锚 → NeedsTrust（identity 的 (instance, rev) 只是下界）。
    let remote = RelayContext::remote_tls("https://relay.example", DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("remote context");
    let error = InitSession::new_verified_migration(remote, &a.vault, &a.identity)
        .expect_err("缺同 rev 锚必须拒绝");
    assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");

    // 空身份（没有管理锚/实例下界）→ NeedsTrust。
    let empty = LocalIdentityV1::generate_dev(a.vault_id, [0x77; 16]).expect("dev 身份");
    let local = local_context(&relay);
    let error = InitSession::new_verified_migration(local.clone(), &a.vault, &empty)
        .expect_err("空身份必须拒绝");
    assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");

    // 身份下界比 relay 更新（回退）→ 迁移失败且不写 pin。
    let mut ahead = identity_copy(&a.identity);
    ahead.control_rev = 9;
    let session = InitSession::new_verified_migration(local, &a.vault, &a.identity)
        .expect("local_dev 允许身份下界");
    let error = expect_init_err(
        session.run_migration(&a.vault, &ahead, now()),
        "下界高于 relay",
    );
    assert!(
        matches!(
            error,
            SyncError::RelayWire(_) | SyncError::ControlRollback { .. }
        ),
        "{error:?}"
    );
    assert!(
        load_control_pin(&a.vault, a.vault_id)
            .expect("读")
            .expect("pin")
            .record
            .hash
            .is_none(),
        "失败路径不得写新 pin"
    );
}

fn identity_copy(identity: &LocalIdentityV1) -> LocalIdentityV1 {
    LocalIdentityV1 {
        vault_id: identity.vault_id,
        device_id: identity.device_id,
        peer_id: identity.peer_id,
        ed25519_seed: identity.ed25519_seed.clone(),
        x25519_secret: identity.x25519_secret.clone(),
        current_epoch: identity.current_epoch,
        current_root: identity.current_root.clone(),
        manager_seed: identity.manager_seed.clone(),
        pinned_instance: identity.pinned_instance,
        pinned_manager: identity.pinned_manager,
        control_rev: identity.control_rev,
        pending_rotation: None,
    }
}

// ---------------------------------------------------------------------------
// 5) pending / journal 崩溃矩阵与启动对账
// ---------------------------------------------------------------------------

fn pending_for(
    kind: InitKind,
    ctx: &RelayContext,
    vault_id: [u8; 16],
    device: [u8; 16],
) -> InitPendingV1 {
    InitPendingV1::prepared(kind, ctx.mode, ctx.origin_str(), vault_id, device, now())
}

#[test]
fn reconcile_matrix_over_crash_phases() {
    use menghunote_sync::dev_local::DevLocal;
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("reconcile-matrix");
    let ctx = local_context(&relay);

    // (0) 无 pending → NoPending（普通启动）。
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A").expect("A");
    assert!(
        reconcile(&a.vault, Some(&a.identity))
            .expect("对账")
            .is_none(),
        "没有 pending 时对账是 no-op"
    );

    // (1) Prepared（请求从未发出）→ 安全放弃并清理；不碰库数据/身份。
    begin_pending(
        &a.vault,
        &pending_for(InitKind::NewGenesis, &ctx, a.vault_id, a.identity.device_id),
    )
    .expect("写 pending");
    let report = reconcile(&a.vault, Some(&a.identity))
        .expect("对账")
        .expect("有 pending");
    assert_eq!(report.outcome, InitReconcileOutcome::AbortedBeforeRequest);
    assert!(load_pending(&a.vault, Some(a.vault_id))
        .expect("读")
        .is_none());

    // (2) Done → 清理 pending，不动 pin。
    let mut done = pending_for(InitKind::NewGenesis, &ctx, a.vault_id, a.identity.device_id);
    done.phase = InitPhase::Done;
    a.vault
        .set_meta(META_INIT_PENDING, &encode_pending(&done))
        .expect("写 done pending");
    let before = load_control_pin(&a.vault, a.vault_id)
        .expect("读")
        .expect("pin");
    let report = reconcile(&a.vault, None)
        .expect("对账")
        .expect("有 pending");
    assert_eq!(report.outcome, InitReconcileOutcome::ClearedDone);
    assert_eq!(report.pin, Some(before), "清理不得改 pin");
    assert!(load_pending(&a.vault, Some(a.vault_id))
        .expect("读")
        .is_none());

    // (3) 损坏 pending → ConfigInvalid（停止，不重建、不删数据）。
    a.vault
        .set_meta(META_INIT_PENDING, b"not-a-pending")
        .expect("写坏 pending");
    let error = reconcile(&a.vault, Some(&a.identity)).expect_err("损坏必须停止");
    assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
    assert!(
        a.vault.get_meta(META_INIT_PENDING).expect("读").is_some(),
        "拒绝路径不得删 pending"
    );
    a.vault.set_meta(META_INIT_PENDING, b"").expect("清位");
    // 空字节也是损坏（不是"没有 pending"）。
    assert!(reconcile(&a.vault, Some(&a.identity)).is_err());
    // 用真实删除语义恢复：直接删 meta 由 storage 提供 set_meta 覆盖，这里用一条正常 pending。
    let mut fresh = pending_for(InitKind::NewGenesis, &ctx, a.vault_id, a.identity.device_id);
    fresh.phase = InitPhase::GenesisRequested;
    a.vault
        .set_meta(META_INIT_PENDING, &encode_pending(&fresh))
        .expect("写 pending");
    let report = reconcile(&a.vault, Some(&a.identity))
        .expect("用已有身份完成 finalize")
        .expect("有 pending");
    assert_eq!(report.outcome, InitReconcileOutcome::Completed);
    assert!(report.pin.is_some());
    assert!(load_pending(&a.vault, Some(a.vault_id))
        .expect("读")
        .is_none());
}

/// 崩溃在 `GenesisRequested`：没有身份材料 → 停住（**不**重建、**不**删库）。
#[test]
fn reconcile_without_identity_stops_after_the_request_was_sent() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("reconcile-noid");
    let ctx = local_context(&relay);
    let m = new_materials();
    let session = InitSession::new_genesis(ctx, &m.materials).expect("会话");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
    session.prepare_pending(&vault, now()).expect("pending");
    let _receipt = session
        .request_genesis(&vault, &m.materials, relay.setup_secret())
        .expect("genesis");
    // 模拟"进程在 genesis 之后崩溃、材料丢失"：只剩 pending。
    let error = reconcile(&vault, None).expect_err("没有身份材料必须停住");
    assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
    assert!(
        load_pending(&vault, Some(m.vault_id))
            .expect("读")
            .expect("pending 必须保留")
            .phase
            .request_sent(),
        "远端可能已创建时不得把 pending 当成没发生"
    );
    // 库数据仍在（不删用户数据）。
    assert!(vault.path().is_file());
}

/// 崩溃在 `GenesisRequested` 但身份材料在：用已有材料完成 finalize（不重复 genesis）。
#[test]
fn reconcile_completes_the_finalize_with_existing_materials() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("reconcile-complete");
    let ctx = local_context(&relay);
    let m = new_materials();
    let session = InitSession::new_genesis(ctx, &m.materials).expect("会话");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
    session.prepare_pending(&vault, now()).expect("pending");
    let receipt = session
        .request_genesis(&vault, &m.materials, relay.setup_secret())
        .expect("genesis");
    // app 侧已把身份原子落盘（这里用返回的身份对象模拟），但 pin 还没提交。
    let identity = m
        .materials
        .identity(receipt.instance_id, receipt.control_rev)
        .expect("身份");
    let report = reconcile(&vault, Some(&identity))
        .expect("对账")
        .expect("有 pending");
    assert_eq!(report.outcome, InitReconcileOutcome::Completed);
    let pin = report.pin.expect("必须提交 pin");
    assert_eq!(
        pin.record.hash,
        Some(rev1_record_hash(m.materials.certificate()).expect("摘要"))
    );
    assert!(load_pending(&vault, Some(m.vault_id))
        .expect("读")
        .is_none());
    // 不重复 genesis：再对账是 no-op（没有第二次创建）。
    assert!(
        reconcile(&vault, Some(&identity))
            .expect("再对账")
            .is_none(),
        "pending 已清理后不得再做初始化"
    );
}

/// FreshRecovery 崩溃在 bootstrap 之后、pin 之前：对账用已有身份提交 pin（不重复 enroll）。
#[test]
fn reconcile_completes_a_fresh_recovery_after_bootstrap() {
    use menghunote_sync::dev_local::DevLocal;
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("reconcile-recovery");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A").expect("A");
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("发布基线");
    let seed = RecoverySeed::from_code(&a.recovery_code).expect("恢复码");
    let ctx = local_context(&relay);
    let session = InitSession::new_fresh_recovery(ctx, a.vault_id, &seed).expect("会话");
    let vault = Vault::open(dir.path().join("b").join("vault.sqlite3")).expect("B vault");
    session.prepare_pending(&vault, now()).expect("pending");
    let recovered = session.recover_identity(&vault, now()).expect("recover");
    // 模拟"bootstrap 完成、pin 未提交"的崩溃。
    assert!(load_control_pin(&vault, a.vault_id).expect("读").is_none());
    let report = reconcile(&vault, Some(&recovered.identity))
        .expect("对账")
        .expect("有 pending");
    assert_eq!(report.outcome, InitReconcileOutcome::Completed);
    let pin = report.pin.expect("pin");
    let anchor = BootstrapPins::from_vault(&vault)
        .expect("pins")
        .control_anchor()
        .expect("锚")
        .expect("必须有锚");
    assert_eq!(pin.bootstrap, Some(anchor));
    assert!(load_pending(&vault, Some(a.vault_id))
        .expect("读")
        .is_none());
}

// ---------------------------------------------------------------------------
// 6) 普通 open 必须被未完成的 pending 挡住
// ---------------------------------------------------------------------------

#[test]
fn ordinary_open_is_blocked_while_initialization_is_unfinished() {
    use menghunote_sync::dev_local::DevLocal;
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("pending-guard");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A").expect("A");
    let ctx = local_context(&relay);
    // 正常情况：能打开。
    ctx.open(&a.vault, &a.identity).expect("无 pending 可打开");
    // 写一条未完成的 pending（模拟崩溃残留）：普通 open 必须在发请求前失败。
    let pending = pending_for(InitKind::NewGenesis, &ctx, a.vault_id, a.identity.device_id);
    begin_pending(&a.vault, &pending).expect("pending");
    let error = ctx
        .open(&a.vault, &a.identity)
        .err()
        .expect("未完成初始化必须挡住普通同步");
    assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
    // 对账完成后恢复。
    let report = reconcile(&a.vault, Some(&a.identity))
        .expect("对账")
        .expect("有 pending");
    assert_eq!(report.outcome, InitReconcileOutcome::AbortedBeforeRequest);
    ctx.open(&a.vault, &a.identity).expect("对账后恢复");
}

/// 重复 Join（不同路径）：各自提交自己的 pin，互不覆盖（幂等语义在 relay 侧）。
#[test]
fn repeated_join_on_new_paths_keeps_each_device_pin() {
    use menghunote_sync::dev_local::DevLocal;
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("repeated-join");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A").expect("A");
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("发布基线");
    let mut pins = Vec::new();
    for index in 0..2 {
        let path = dir.path().join(format!("b{index}")).join("vault.sqlite3");
        let b =
            DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &path).expect("join");
        let pin = load_control_pin(&b.vault, a.vault_id)
            .expect("读")
            .expect("每次 join 都必须提交 pin");
        assert_eq!(pin.instance_id, b.identity.pinned_instance);
        assert_eq!(
            pin.bootstrap.map(|anchor| anchor.rev),
            Some(pin.record.rev),
            "record 与 bootstrap 锚必须同 rev"
        );
        pins.push((pin.record.rev, pin.record.hash, pin.instance_id));
    }
    // 每次 join 都会在控制链上追加 approve：rev 只进不退，实例不变，且都有链尾 hash。
    assert!(pins[1].0 >= pins[0].0, "控制链 rev 不得回退");
    assert_eq!(pins[0].2, pins[1].2, "同一 relay 实例");
    assert!(
        pins.iter().all(|(_, hash, _)| hash.is_some()),
        "每次都必须带链尾 hash"
    );
}

fn encode_pending(pending: &InitPendingV1) -> Vec<u8> {
    menghunote_sync::init_journal::encode_pending_v1(pending).expect("编码 pending")
}

// ---------------------------------------------------------------------------
// 7) S3a：app 在身份原子落盘后推进 `IdentityWritten → Done`
// ---------------------------------------------------------------------------

/// app 路径：Create 停在 `PinCommitted`；身份落盘后 mark → finish 才清理。
/// 顺序错误（未 mark 就 finish）必须明确拒绝，不能提前清理。
#[test]
fn deferred_create_pending_advances_identity_written_then_done() {
    use menghunote_sync::dev_local::DevLocal;
    use menghunote_sync::init_session::{finish_identity_written, mark_identity_written};

    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("deferred-create");
    let path = dir.path().join("a").join("vault.sqlite3");
    let created = DevLocal::create_deferring_identity(
        relay.base_url(),
        relay.setup_secret(),
        &path,
        "A 实例",
    )
    .expect("create_deferring_identity");
    let pending = load_pending(&created.vault, Some(created.vault_id))
        .expect("读 pending")
        .expect("pending 必须仍在");
    assert_eq!(
        pending.phase,
        InitPhase::PinCommitted,
        "app 路径在身份落盘前必须停在 pin_committed"
    );
    // local_dev 的 Create 在提交 pin 之后才落盘身份：阶段已在 pin_committed
    // （阶段序晚于 identity_written），mark 幂等接受、不回退。
    mark_identity_written(&created.vault).expect("身份落盘后 mark");
    assert_eq!(
        load_pending(&created.vault, Some(created.vault_id))
            .expect("读")
            .expect("仍在")
            .phase,
        InitPhase::PinCommitted,
        "先提交 pin 的本地流程不得为了 mark 回退阶段"
    );
    // mark 幂等（崩溃后重启重试安全）。
    mark_identity_written(&created.vault).expect("mark 幂等");
    finish_identity_written(&created.vault).expect("finish");
    assert!(
        load_pending(&created.vault, Some(created.vault_id))
            .expect("读")
            .is_none(),
        "Done 之后必须清理 pending"
    );
    // 普通 open 现在可以工作（pending 不再挡住同步）。
    let ctx = local_context(&relay);
    ctx.open(&created.vault, &created.identity)
        .expect("pending 清理后普通 open 必须可用");
}

/// Join 的 app 路径同样停在 `PinCommitted`，身份落盘后推进并清理。
#[test]
fn deferred_join_pending_advances_identity_written_then_done() {
    use menghunote_sync::dev_local::DevLocal;
    use menghunote_sync::init_session::{finish_identity_written, mark_identity_written};

    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("deferred-join");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A").expect("A");
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("发布基线");

    let b_path = dir.path().join("b").join("vault.sqlite3");
    let b =
        DevLocal::join_deferring_identity(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path)
            .expect("join_deferring_identity");
    assert_eq!(
        load_pending(&b.vault, Some(a.vault_id))
            .expect("读")
            .expect("pending 仍在")
            .phase,
        InitPhase::PinCommitted
    );
    mark_identity_written(&b.vault).expect("mark");
    finish_identity_written(&b.vault).expect("finish");
    assert!(load_pending(&b.vault, Some(a.vault_id))
        .expect("读")
        .is_none());
    let ctx = local_context(&relay);
    ctx.open(&b.vault, &b.identity)
        .expect("pending 清理后可 open");
}

/// 没有 pending 时 `finish_identity_written` 是幂等 no-op；`mark` 则明确报错
/// （没有初始化事务却声称身份已落盘 = 绑定矛盾）。
#[test]
fn mark_requires_pending_but_finish_is_idempotent_when_already_done() {
    use menghunote_sync::init_session::{finish_identity_written, mark_identity_written};
    let dir = temp_root("no-pending");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
    assert!(
        mark_identity_written(&vault).is_err(),
        "没有 pending 不得声称 identity_written"
    );
    finish_identity_written(&vault).expect("没有待清理的 pending 是幂等成功");
}

/// 远端 S3b 的规范顺序（先落盘身份、再提交 pin）：`GenesisRequested` 上的 mark
/// 必须真正推进到 `IdentityWritten`，之后才允许 Done/清理。
#[test]
fn mark_advances_genesis_requested_to_identity_written_in_the_canonical_order() {
    use menghunote_sync::init_journal::{advance_phase, begin_pending};
    use menghunote_sync::init_session::{finish_identity_written, mark_identity_written};
    use menghunote_sync::relay_policy::RelayMode;

    let dir = temp_root("canonical-identity");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
    let vault_id: [u8; 16] = array(&mut OsRandom).expect("vault id");
    let device_id: [u8; 16] = array(&mut OsRandom).expect("device id");
    let pending = InitPendingV1::prepared(
        InitKind::NewGenesis,
        RelayMode::LocalDev,
        "http://127.0.0.1:1",
        vault_id,
        device_id,
        1,
    );
    begin_pending(&vault, &pending).expect("begin");
    advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested).expect("request sent");
    // 身份还没落盘（仍是 GenesisRequested）：不得提前 Done/清理。
    assert!(
        finish_identity_written(&vault).is_err(),
        "未推进 identity_written 时不得清理 pending"
    );
    mark_identity_written(&vault).expect("mark");
    assert_eq!(
        load_pending(&vault, Some(vault_id))
            .expect("读")
            .expect("仍在")
            .phase,
        InitPhase::IdentityWritten,
        "规范顺序必须先推进 identity_written"
    );
    finish_identity_written(&vault).expect("finish");
    assert!(load_pending(&vault, Some(vault_id)).expect("读").is_none());
}
