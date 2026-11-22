//! ADR 0013 **S2**：`RelayContext` / `PinnedClient` 的统一 wiring 与 durable pin 提交时序。
//!
//! 全部用 `menghunote-dev-relay`（进程内真实 axum relay + 临时 SQLite + `127.0.0.1:0`），
//! 客户端走真实 HTTP；**不连任何真实远端 relay**。
//!
//! 本文件的三类断言：
//!
//! 1. **提交时序**：首个消费控制链的操作（`members`/同步）之前，`sync.control_pin` 必须
//!    已经 durable 写入，并且带上 bootstrap 锚（rev/epoch/log 独立）；
//! 2. **fail-closed wiring**：durable pin 损坏时，sync/publish/members/revoke/rotate/apply
//!    **每一个**操作都会在发请求之前以 `PinCorrupt` 失败——任何绕过 `RelayContext::open`
//!    的操作都会给出别的错误（这条同时是"没有裸 transport 散点"的行为级证据）；
//! 3. **anchor 语义**：同 rev 的 bootstrap 摘要被改写（且 durable pin 与 `sync.pinned_*`
//!    一致地改写）→ 控制链刷新按当前视图拒绝（equivocation），不会"选一个信"。
//!
//! ⚠️ 非生产：dev-relay 的放行验签 hook 不校验签名，本文件只验证 wiring/pin 流程。

use std::path::Path;

use menghunote_dev_relay::DevRelay;
use menghunote_storage::Vault;
use menghunote_sync::bootstrap::BootstrapPins;
use menghunote_sync::control_pin::{
    encode_pin_v1, load_control_pin, BootstrapAnchorV1, PinV1, META_CONTROL_PIN,
};
use menghunote_sync::dev_local::{CreatedVault, DevLocal, JoinedVault};
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::init_journal::{load_pending, InitPhase};
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::relay_policy::RelayMode;
use menghunote_sync::SyncError;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 在 `target/tmp` 下建独立测试根目录（AGENTS.md §6：不用 %TEMP%）。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("relay-context-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// A 建库 + 发布两份空基线 + B 用恢复码 Join（**不**跑首轮同步）。
struct Fixture {
    relay: DevRelay,
    _dir: TempDir,
    a: CreatedVault,
    b: JoinedVault,
}

fn establish(prefix: &str) -> Fixture {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A 实例")
        .expect("A create 失败");
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("A 发布初始基线失败");
    let b_path = dir.path().join("b").join("vault.sqlite3");
    let b = DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path)
        .expect("B join 失败");
    Fixture {
        relay,
        _dir: dir,
        a,
        b,
    }
}

fn local_context(relay: &DevRelay) -> RelayContext {
    RelayContext::local_dev(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("local_dev context")
}

fn durable_pin(vault: &Vault, identity: &LocalIdentityV1) -> Option<PinV1> {
    load_control_pin(vault, identity.vault_id).expect("读 durable pin")
}

fn corrupt_pin(vault: &Vault) {
    vault
        .set_meta(META_CONTROL_PIN, b"corrupted-control-pin")
        .expect("写坏 pin");
}

fn assert_pin_corrupt<T>(label: &str, result: Result<T, SyncError>) {
    match result {
        Err(SyncError::PinCorrupt(_)) => {}
        Err(error) => panic!("{label} 必须在发请求前以 PinCorrupt 失败，实际 {error:?}"),
        Ok(_) => panic!("{label} 在损坏 pin 下不得成功"),
    }
}

// ---------------------------------------------------------------------------
// 1) 首个消费控制链的操作之前，durable pin 已写入并带 bootstrap 锚
// ---------------------------------------------------------------------------

#[test]
fn initialization_commits_a_durable_pin_with_the_bootstrap_anchor() {
    let fixture = establish("commit-timing");
    let b = &fixture.b;

    // S2b：Join（FreshRecovery 受限初始化）在返回前就提交了 record pin + bootstrap 锚，
    // 且两者必须对应**同一验证视图**（同 rev、同 epoch/log、摘要复算通过）。
    let pins = BootstrapPins::from_vault(&b.vault).expect("读 bootstrap pins");
    let anchor = pins
        .control_anchor()
        .expect("bootstrap 锚解析")
        .expect("Join 后必须有 bootstrap 锚");
    assert_eq!(
        anchor.rev,
        pins.control_rev.expect("bootstrap 锚必须带 rev")
    );
    let joined_pin = durable_pin(&b.vault, &b.identity).expect("Join 必须提交 durable pin");
    assert_eq!(joined_pin.instance_id, b.identity.pinned_instance);
    assert_eq!(
        joined_pin.bootstrap,
        Some(anchor),
        "record pin 必须与 bootstrap 锚同视图（同 rev/epoch/log_id）"
    );
    assert_eq!(joined_pin.record.rev, anchor.rev);
    assert!(joined_pin.record.hash.is_some(), "record 锚必须带链尾 hash");
    assert_eq!(
        load_pending(&b.vault, Some(b.identity.vault_id)).expect("读 pending"),
        None,
        "初始化完成后必须清掉 sync.init_pending（清理只在 Done）"
    );
    assert_eq!(
        load_pending(&b.vault, Some(b.identity.vault_id))
            .expect("再读 pending")
            .map(|pending| pending.phase),
        None::<InitPhase>
    );

    let ctx = local_context(&fixture.relay);
    let mut pinned = ctx.open(&b.vault, &b.identity).expect("local_dev 可打开");
    // 消费授权事实：`members()` 会拉取/验证 `/control`，控制链缓存是唯一授权来源。
    let members = pinned.client_mut().members().expect("读取成员视图");
    assert!(
        members
            .iter()
            .any(|member| member.device_id == b.identity.device_id),
        "B 必须出现在自己的成员视图里"
    );

    // 消费控制链后 pin 只能前进/相等：同 rev 的 record hash 必须逐字节不变。
    let pin = durable_pin(&b.vault, &b.identity).expect("消费控制链后必须有 durable pin");
    assert_eq!(pin.instance_id, b.identity.pinned_instance);
    assert_eq!(pin.record.rev, joined_pin.record.rev);
    assert_eq!(pin.record.hash, joined_pin.record.hash);
    assert_eq!(
        pin.bootstrap,
        Some(anchor),
        "durable pin 必须保存独立的 bootstrap 锚（rev/epoch/log_id 各自携带）"
    );
}

// ---------------------------------------------------------------------------
// 2) 损坏 pin：六个操作全部在发请求前 fail-closed（wiring 的行为级证据）
// ---------------------------------------------------------------------------

#[test]
fn corrupt_durable_pin_stops_every_operation_before_any_request() {
    let Fixture {
        relay,
        _dir: dir,
        mut a,
        b,
    } = establish("corrupt-pin");
    // B 先读一次成员视图（消费控制链 = 触发 durable pin 提交），再把两边的 pin 都写坏。
    let ctx = local_context(&relay);
    DevLocal::members(&ctx, &b.vault, &b.identity).expect("B 读成员视图");
    assert!(
        durable_pin(&b.vault, &b.identity).is_some(),
        "消费控制链后必须已有 durable pin"
    );
    corrupt_pin(&b.vault);
    corrupt_pin(&a.vault);

    let identity_path = dir.path().join("identity.bin");

    // --- B 侧：sync / members / apply ---
    assert_pin_corrupt(
        "sync_once",
        DevLocal::sync_once(&ctx, &b.vault, &b.identity),
    );
    assert_pin_corrupt("members", DevLocal::members(&ctx, &b.vault, &b.identity));
    assert_pin_corrupt(
        "apply_latest_epoch",
        DevLocal::apply_latest_epoch(&ctx, &b.vault, &identity_path, &mut b_copy(&b)),
    );

    // --- A 侧：sync / publish / members / revoke / rotate ---
    assert_pin_corrupt(
        "A sync_once",
        DevLocal::sync_once(&ctx, &a.vault, &a.identity),
    );
    assert_pin_corrupt(
        "publish_initial_pair_with_identity",
        DevLocal::publish_initial_pair_with_identity(&ctx, &a.vault, &a.identity),
    );
    assert_pin_corrupt("A members", DevLocal::members(&ctx, &a.vault, &a.identity));
    assert_pin_corrupt(
        "revoke_device",
        DevLocal::revoke_device(&ctx, &a.vault, &a.identity, b.identity.device_id),
    );
    assert_pin_corrupt(
        "rotate",
        DevLocal::rotate(&ctx, &a.vault, &identity_path, &mut a.identity, &[], None),
    );
}

/// `LocalIdentityV1` 不实现 `Clone`（秘密不复制）：测试里按字段重建一份只读副本，
/// 供"apply 必须 PinCorrupt"这条断言使用（不会写回）。
fn b_copy(b: &JoinedVault) -> LocalIdentityV1 {
    LocalIdentityV1 {
        vault_id: b.identity.vault_id,
        device_id: b.identity.device_id,
        peer_id: b.identity.peer_id,
        ed25519_seed: b.identity.ed25519_seed.clone(),
        x25519_secret: b.identity.x25519_secret.clone(),
        current_epoch: b.identity.current_epoch,
        current_root: b.identity.current_root.clone(),
        manager_seed: b.identity.manager_seed.clone(),
        pinned_instance: b.identity.pinned_instance,
        pinned_manager: b.identity.pinned_manager,
        control_rev: b.identity.control_rev,
        pending_rotation: None,
    }
}

// ---------------------------------------------------------------------------
// 3) 同 rev 的 bootstrap 锚被改写：按当前视图拒绝（equivocation）
// ---------------------------------------------------------------------------

#[test]
fn rewritten_bootstrap_anchor_at_the_same_rev_is_rejected() {
    let fixture = establish("rewritten-anchor");
    let b = &fixture.b;
    DevLocal::members(&local_context(&fixture.relay), &b.vault, &b.identity)
        .expect("B 读成员视图（消费控制链）");
    let mut pin = durable_pin(&b.vault, &b.identity).expect("durable pin");
    let anchor = pin.bootstrap.expect("同步后 pin 带 bootstrap 锚");

    // 改写锚 hash，同时改写 durable pin 与 `sync.pinned_*`：让 pin 决议阶段"两处一致"，
    // 这样失败只能来自控制链刷新时的**视图复算**（不能靠"两处不一致"提前拒绝掩盖）。
    let bad = BootstrapAnchorV1 {
        hash: [0x5a; 32],
        ..anchor
    };
    pin.bootstrap = Some(bad);
    b.vault
        .set_meta(META_CONTROL_PIN, &encode_pin_v1(&pin).expect("编码"))
        .expect("写改写后的 pin");
    b.vault
        .set_meta(
            menghunote_sync::bootstrap::META_BOOTSTRAP_CONTROL_HASH,
            &bad.hash,
        )
        .expect("改写 bootstrap 摘要");

    let ctx = local_context(&fixture.relay);
    let mut pinned = ctx
        .open(&b.vault, &b.identity)
        .expect("pin 决议本身仍可打开");
    let error = match pinned.client_mut().members() {
        Ok(_) => panic!("同 rev 改写 bootstrap 摘要必须被拒绝"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SyncError::Equivocation(message) if message.contains("bootstrap"))
            || matches!(error, SyncError::NeedsTrust(_)),
        "必须是 equivocation/needs-trust 而不是放过：{error:?}"
    );
    // 拒绝路径不得把改写后的锚提升成"新的已提交事实"。
    let after = durable_pin(&b.vault, &b.identity).expect("pin 仍在");
    assert_eq!(
        after.bootstrap,
        Some(bad),
        "失败路径不改写 pin（保留原样，等人工排查）"
    );
}

// ---------------------------------------------------------------------------
// 4) 显式 context 与 URL 兼容入口走同一条路径
// ---------------------------------------------------------------------------

#[test]
fn explicit_context_and_url_entry_points_behave_identically() {
    let fixture = establish("targets");
    let b = &fixture.b;
    let ctx = local_context(&fixture.relay);
    // URL 兼容入口（内部构造 local_dev context）；members 会消费控制链并提交 durable pin。
    let members_from_url = DevLocal::members(fixture.relay.base_url(), &b.vault, &b.identity)
        .expect("URL 入口成员视图");
    let pin_from_url = durable_pin(&b.vault, &b.identity).expect("durable pin");
    // 显式 context 入口：同一库、同一 relay，pin 只能前进/相等。
    let members_from_ctx =
        DevLocal::members(&ctx, &b.vault, &b.identity).expect("context 成员视图");
    let pin_from_ctx = durable_pin(&b.vault, &b.identity).expect("durable pin");
    assert_eq!(
        pin_from_url.record, pin_from_ctx.record,
        "两种入口必须收敛到同一个已验证链尾"
    );
    assert_eq!(members_from_url.len(), members_from_ctx.len());
    assert!(members_from_ctx
        .iter()
        .any(|m| m.device_id == b.identity.device_id));
    // 两条入口的同步也都必须能跑（两者共用同一个 RelayContext::open 路径）。
    DevLocal::sync_once(fixture.relay.base_url(), &b.vault, &b.identity).expect("URL 入口同步");
    DevLocal::sync_once(&ctx, &b.vault, &b.identity).expect("context 入口同步");
    // 预算来自 context（不是 legacy 参数）。
    assert_eq!(ctx.mode, RelayMode::LocalDev);
    assert_eq!(ctx.timeouts, DEFAULT_TRANSPORT_TIMEOUTS);
}
