//! ADR 0013 **S3b**：远端 `remote_tls` Create / Join 在**真实 TLS + 真实 axum relay** 上的回归。
//!
//! 不连任何真实远端 relay：`menghunote-dev-relay` 的 `start_tls()` 现场生成自签 CA +
//! `localhost` 叶子证书、起 rustls 监听；客户端用 `RelayContext::with_test_extra_roots`
//! 显式注入该 CA（仍校验证书链/SAN/有效期，**没有** accept-invalid）。
//!
//! 覆盖：
//!
//! 1. A（NewGenesis）→ rev1 强 pin → 上传初始恢复槽 → 发布基线；
//! 2. B（FreshRecovery）→ 真实 TLS 上的自批 + 认证 bootstrap → record/bootstrap 同视图 pin；
//! 3. 失败负例：错 setup secret / 未信任 TLS / 错恢复码 → 不留本地库/pending；
//! 4. 崩溃点：请求已发 + 身份已写 + pin 未提交 → `reconcile` 在真实 TLS 上补齐并清 pending；
//! 5. `local_dev` context 不能进入远端入口（不静默降级）。

use std::path::{Path, PathBuf};

use menghunote_dev_relay::DevRelay;
use menghunote_storage::{list_tree, StructureWriter, SyncContext, Vault};
use menghunote_sync::control_pin::load_control_pin;
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::genesis::build_materials;
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::init_journal::{
    begin_pending, load_pending, InitKind, InitPendingV1, InitPhase,
};
use menghunote_sync::init_session::{reconcile_with_extra_trust_roots, InitReconcileOutcome};
use menghunote_sync::random::{array, OsRandom};
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::remote_init::{create_remote, join_remote};
use menghunote_sync::SyncError;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 在 `target/tmp` 下建独立测试根目录（AGENTS.md §6：不用 %TEMP%）。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("remote-init-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 生产 TLS context + 显式注入本地 harness 的自签 CA（只加信任根，不改校验策略）。
fn remote_context(relay: &DevRelay) -> RelayContext {
    let ca = relay.tls_ca_der().expect("必须是 TLS relay");
    let context = RelayContext::remote_tls(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("remote_tls context")
        .with_test_extra_roots(vec![ca.to_vec()])
        .expect("注入自签根");
    assert_eq!(
        context.extra_trust_root_count(),
        1,
        "只应注入 harness 的自签 CA（不关闭任何校验）"
    );
    context
}

/// 测试用"身份落盘"回调：只记录已调用，不写任何真实受保护文件。
fn ok_identity(_identity: &LocalIdentityV1) -> Result<(), SyncError> {
    Ok(())
}

/// 断言目录里没有任何库/边车文件（失败路径"不建库"）。
fn assert_no_vault_files(vault_path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut path = vault_path.as_os_str().to_os_string();
        path.push(suffix);
        let path = PathBuf::from(path);
        assert!(!path.exists(), "失败路径不得留下库文件：{}", path.display());
    }
    assert!(
        load_pending_of(vault_path).is_none(),
        "失败路径不得留下可读的 pending（库文件都不应存在）"
    );
}

/// 尽力读 pending：库文件不存在时返回 `None`（不把"打不开"当成"有 pending"）。
fn load_pending_of(vault_path: &Path) -> Option<InitPhase> {
    if !vault_path.exists() {
        return None;
    }
    let vault = Vault::open(vault_path).ok()?;
    load_pending(&vault, None)
        .ok()
        .flatten()
        .map(|pending| pending.phase)
}

/// 在 A 的库里建一篇笔记（结构 CRDT；足以验证基线导入）。
fn seed_note(vault: &Vault, identity: &LocalIdentityV1, title: &str) {
    let peer = identity.loro_peer_id();
    let epoch = identity.current_epoch;
    let mut writer =
        StructureWriter::open_with_sync(vault, peer, Some(SyncContext::new(peer, epoch)))
            .expect("打开结构写入口");
    writer.doc().create_note(title, 1).expect("新建笔记");
    writer.commit("remote-init-seed").expect("结构提交");
}

/// A（NewGenesis）→ rev1 强 pin 的真实 TLS 全链路。
#[test]
fn remote_create_commits_a_rev1_pin_over_real_tls_and_clears_pending() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("create");
    let vault_path = dir.path().join("a/vault.sqlite3");
    let ctx = remote_context(&relay);

    let created = create_remote(
        ctx,
        relay.setup_secret(),
        &vault_path,
        "remote-init 测试实例",
        ok_identity,
    )
    .expect("远端 Create 失败");

    assert!(created.vault_id != [0u8; 16], "vault_id 必须非零");
    assert!(
        created.recovery_code.starts_with("mnrec1"),
        "恢复码格式不对"
    );
    assert_eq!(created.warning, None, "正常路径不应有告警");
    assert_eq!(
        load_pending(&created.vault, None).expect("读 pending"),
        None,
        "Done 之后 pending 必须清理"
    );

    let pin = load_control_pin(&created.vault, created.vault_id)
        .expect("读 pin")
        .expect("必须有 rev1 强 pin");
    assert!(pin.instance_id != [0u8; 16], "instance 非零");
    assert_eq!(pin.record.rev, 1, "新库第一条控制记录是 rev1");
    assert!(
        pin.record.hash.is_some(),
        "remote Create 必须提交带 record hash 的强 pin"
    );
    assert!(pin.bootstrap.is_none(), "genesis 还没有历史 bootstrap 锚");
    // 本地事实：结构行可读（空树），身份 pin 与 genesis 回执一致。
    assert!(list_tree(&created.vault).expect("读结构投影").is_empty());
    assert_eq!(created.identity.pinned_instance, pin.instance_id);
    assert_eq!(created.identity.control_rev, 1);
    assert!(created.identity.pinned_manager != [0u8; 32]);
}

/// A → 发布真实基线 → B（FreshRecovery）在真实 TLS 上加入，record/bootstrap 同视图。
#[test]
fn remote_join_imports_baseline_over_real_tls_with_same_view_pin() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("join");
    let a_path = dir.path().join("a/vault.sqlite3");
    let ctx = remote_context(&relay);

    let a = create_remote(
        ctx.clone(),
        relay.setup_secret(),
        &a_path,
        "remote-init A",
        ok_identity,
    )
    .expect("A Create 失败");
    seed_note(&a.vault, &a.identity, "A 的远端笔记");
    let published = DevLocal::publish_initial_pair_with_identity(&ctx, &a.vault, &a.identity)
        .expect("发布基线");
    assert!(published.validated, "初始基线必须真实读回/导入通过");

    let b_path = dir.path().join("b/vault.sqlite3");
    let b =
        join_remote(ctx, a.vault_id, &a.recovery_code, &b_path, ok_identity).expect("B Join 失败");

    assert_eq!(
        load_pending(&b.vault, None).expect("读 pending"),
        None,
        "Join 完成后 pending 必须清理"
    );
    let pin = load_control_pin(&b.vault, a.vault_id)
        .expect("读 pin")
        .expect("B 必须有 durable pin");
    assert!(
        pin.record.rev >= 1,
        "B 的 record rev 至少覆盖 genesis（enroll 可能推进控制链）"
    );
    assert!(pin.record.hash.is_some(), "Join 必须提交 record 链尾 hash");
    let bootstrap = pin.bootstrap.expect("Join 必须带 bootstrap 锚");
    assert_eq!(
        bootstrap.rev, pin.record.rev,
        "record pin 与 bootstrap 锚必须是同一验证视图的 rev"
    );
    assert_ne!(bootstrap.log_id, [0u8; 16]);
    assert_ne!(pin.instance_id, [0u8; 16]);

    // B 的树必须来自 A 的真实基线（不播种、不伪造）。
    let tree = list_tree(&b.vault).expect("读 B 的结构投影");
    let titles: Vec<&str> = tree.iter().map(|row| row.title.as_str()).collect();
    assert_eq!(tree.len(), 1, "B 只应有 A 的那一篇：{titles:?}");
    assert_eq!(tree[0].title, "A 的远端笔记");
    assert_ne!(
        b.identity.device_id, a.identity.device_id,
        "B 必须是独立设备"
    );
    assert_eq!(b.identity.pinned_manager, a.identity.pinned_manager);
}

/// 错 setup secret：relay 明确拒绝（初始化单事务，不会创建 vault）→ 本地不留库/pending。
#[test]
fn remote_create_with_wrong_setup_secret_leaves_no_local_state() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("wrong-secret");
    let vault_path = dir.path().join("a/vault.sqlite3");
    let error = match create_remote(
        remote_context(&relay),
        "definitely-not-the-setup-secret",
        &vault_path,
        "remote-init 负例",
        ok_identity,
    ) {
        Ok(_) => panic!("错 setup secret 必须失败"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SyncError::RelayApi { .. } | SyncError::HttpStatus(_)),
        "必须是 relay 明确拒绝：{error:?}"
    );
    assert_no_vault_files(&vault_path);
}

/// 未注入自签根 = 系统根不信任该 CA → TLS 拒绝，且**不建库**（没有发任何业务请求）。
#[test]
fn remote_create_without_injected_root_is_tls_rejected_and_leaves_no_vault() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("untrusted");
    let vault_path = dir.path().join("a/vault.sqlite3");
    // 故意不带 `with_test_extra_roots`：生产系统根不会信任测试自签 CA。
    let ctx = RelayContext::remote_tls(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("remote_tls context");
    let error = match create_remote(
        ctx,
        relay.setup_secret(),
        &vault_path,
        "remote-init 负例",
        ok_identity,
    ) {
        Ok(_) => panic!("未信任的自签 CA 必须失败"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SyncError::Tls(_)),
        "必须是 TLS 错误：{error:?}"
    );
    assert_no_vault_files(&vault_path);
}

/// 格式合法但错误的恢复码：自批被 relay 拒绝 → 本地不留库/pending/身份。
#[test]
fn remote_join_with_wrong_recovery_code_leaves_no_local_state() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("wrong-code");
    let a_path = dir.path().join("a/vault.sqlite3");
    let ctx = remote_context(&relay);
    let a = create_remote(
        ctx.clone(),
        relay.setup_secret(),
        &a_path,
        "remote-init A",
        ok_identity,
    )
    .expect("A Create 失败");

    // 另一个种子生成的**格式合法**恢复码（不是 A 的）。
    let wrong = RecoverySeed::generate(&mut OsRandom)
        .expect("生成错误种子")
        .to_code()
        .expect("编码错误恢复码");
    let b_path = dir.path().join("b/vault.sqlite3");
    let error = match join_remote(ctx, a.vault_id, wrong.as_str(), &b_path, ok_identity) {
        Ok(_) => panic!("错恢复码必须失败"),
        Err(error) => error,
    };
    assert!(
        matches!(
            error,
            SyncError::RelayApi { .. } | SyncError::HttpStatus(_) | SyncError::RecoveryCode(_)
        ),
        "必须是恢复码/relay 拒绝：{error:?}"
    );
    assert_no_vault_files(&b_path);
}

/// 崩溃点：`GenesisRequested → IdentityWritten` 之后（pin 未提交）→ `reconcile` 在真实 TLS
/// 上补齐 finalize、提交强 pin 并清 pending（不重复 genesis）。
#[test]
fn remote_create_crash_after_identity_is_reconciled_over_tls() {
    let relay = DevRelay::start_tls().expect("起进程内 TLS relay 失败");
    let dir = temp_root("reconcile");
    let vault_path = dir.path().join("a/vault.sqlite3");
    let ctx = remote_context(&relay);

    // 手工复刻 `create_remote` 的前半程，然后在 IdentityWritten 处"崩溃"（drop 库句柄）。
    let vault_id: [u8; 16] = array(&mut OsRandom).expect("vault id");
    let device_id: [u8; 16] = array(&mut OsRandom).expect("device id");
    let materials = build_materials(
        vault_id,
        device_id,
        "remote-init-test-password",
        1,
        &mut OsRandom,
    )
    .expect("genesis 材料");
    let session = menghunote_sync::init_session::InitSession::new_genesis(ctx, &materials)
        .expect("genesis 会话");
    let vault = Vault::open(&vault_path).expect("新库");
    session.prepare_pending(&vault, 1).expect("写前 pending");
    let receipt = session
        .request_genesis(&vault, &materials, relay.setup_secret())
        .expect("genesis 请求");
    let identity = materials
        .identity(receipt.instance_id, receipt.control_rev)
        .expect("身份");
    menghunote_sync::init_session::mark_identity_written(&vault).expect("身份已落盘");
    assert_eq!(
        load_pending(&vault, None)
            .expect("读 pending")
            .expect("pending 仍在")
            .phase,
        InitPhase::IdentityWritten
    );
    // 崩溃：不提交 pin、不做 finish，直接关库。
    drop(vault);

    // 重启对账（用身份材料）：必须补齐 pin → Done。
    let vault = Vault::open(&vault_path).expect("重开库");
    let ca = relay.tls_ca_der().expect("TLS relay").to_vec();
    let report =
        reconcile_with_extra_trust_roots(&vault, Some(&identity), std::slice::from_ref(&ca))
            .expect("对账失败")
            .expect("必须有 pending 报告");
    assert_eq!(report.phase_before, InitPhase::IdentityWritten);
    assert_eq!(report.outcome, InitReconcileOutcome::Completed);
    assert_eq!(load_pending(&vault, None).expect("读 pending"), None);
    let pin = load_control_pin(&vault, vault_id)
        .expect("读 pin")
        .expect("对账必须提交强 pin");
    assert_eq!(pin.record.rev, 1);
    assert!(pin.record.hash.is_some());
}

/// `local_dev` context 不能进入远端入口（不静默降级成明文/开发语义）。
#[test]
fn remote_entry_requires_remote_tls_context() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("mode-gate");
    let vault_path = dir.path().join("a/vault.sqlite3");
    let ctx = RelayContext::local_dev(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("local_dev context");
    let error = match create_remote(
        ctx,
        relay.setup_secret(),
        &vault_path,
        "remote-init 负例",
        ok_identity,
    ) {
        Ok(_) => panic!("local_dev 必须被远端入口拒绝"),
        Err(error) => error,
    };
    assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
    assert!(!vault_path.exists(), "拒绝路径不得建库");
}

/// 一条 pending 夹具（覆盖 remote 模式/HTTPS origin 的 CDE 编码）。
#[test]
fn remote_pending_roundtrips_mode_and_https_origin() {
    let dir = temp_root("pending-cde");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
    let pending = InitPendingV1::prepared(
        InitKind::NewGenesis,
        menghunote_sync::relay_policy::RelayMode::RemoteTls,
        "https://relay.example:8443",
        [0x11; 16],
        [0x22; 16],
        7,
    );
    begin_pending(&vault, &pending).expect("写 pending");
    let loaded = load_pending(&vault, Some([0x11; 16]))
        .expect("读 pending")
        .expect("pending 存在");
    assert_eq!(loaded, pending);
    assert_eq!(
        loaded.mode,
        menghunote_sync::relay_policy::RelayMode::RemoteTls
    );
    assert_eq!(loaded.origin, "https://relay.example:8443");
}
