//! ADR 0010 切片 1b：设备访问吊销的两设备真实 e2e（真实 HTTP + 进程内 relay）。
//!
//! 覆盖：
//! 1. A 用管理端 helper 吊销 B → B 的**旧客户端**立即 push/pull 失败（REVOKED），
//!    新 challenge 也被拒；A 继续同步正常；B 本地身份/笔记/恢复材料不被 helper 改写；
//! 2. `auth_rev` 缓存失效：吊销只推进 `auth_rev`（`control_rev` 不变），持有旧 token /
//!    控制成员缓存的客户端必须刷新——A 的旧 token 直接被 relay 拒绝（REVOKED → 清缓存 →
//!    一次重认证）以及在 token 仍"看起来有效"时靠 `/state.auth_rev` 比对清掉成员缓存，
//!    刷新后 active 集合排除 B；控制链链尾仍精确等于 `control_rev`（无占位记录），
//!    后续 grant/rotation 规划不含 B；
//! 3. 重复吊销 `changed=false` 且 `auth_rev` 不二次推进；同 id/B 重放回放原响应；
//!    同 id 不同 target → ID_CONFLICT；target 不存在 → NOT_FOUND 且无副作用；
//! 4. helper 本地守卫：self-remove / 错管理 key 直接拒绝，**不发任何 HTTP 请求**；
//! 5. 切片 2a-1 的只读成员视图：create/join 后 A/B 均 Active 且有指纹；吊销后 B 变
//!    Revoked（历史证书保留 → 指纹不变）、A 仍 Active，`control_rev`/链尾/授权集合/
//!    grant 规划都不受视图影响；排序与内容稳定，Debug 不含完整公钥。
//!
//! ⚠️ **非生产**：`menghunote-dev-relay` 的 `DevPermissiveHook` 对任何签名**一律放行**；
//! 本文件验证的是**访问状态与缓存语义**（relay 生产验签/守卫由 relay 的单元测试覆盖），
//! 不能把 dev-relay 的结果当作鉴权/验签验收。
//!
//! 纪律（AGENTS.md §6）：临时目录一律在 `CARGO_TARGET_TMPDIR`；**不用 sleep**，收敛点只用
//! 显式返回值（`sync_once` 的 durable 结果 / `state()` / helper 的返回值）。

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, list_tree, load_body_doc, local_sync_intent, BodyDoc,
    HeadState, StructureWriter, SyncContext, UpdateRecord, Vault,
};
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::dev_local::{CreatedVault, DevLocal, JoinedVault};
use menghunote_sync::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use menghunote_sync::device_management::{revoke_device, RevokeContext, RevokeOutcome};
use menghunote_sync::engine::{EngineIdentity, SyncEngine};
use menghunote_sync::genesis::GENESIS_MANAGER_REV;
use menghunote_sync::http_relay::{
    Clock, ControlPin, HttpRelayClient, HttpRelayConfig, HttpRequest, HttpResponse, HttpTransport,
    MemberStatus, MemberView, StdTcpTransport,
};
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::keys::{RootSecretV1, Secret32};
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::relay::{RelayClient as _, RelayState};
use menghunote_sync::rotation_slots::plan_all_active_grants;
use menghunote_sync::{DeviceId, SyncError};
use tempfile::{Builder as TempDirBuilder, TempDir};

// ---------------------------------------------------------------------------
// 基础设施：临时目录 / 笔记 / 设备客户端 / 假时钟
// ---------------------------------------------------------------------------

/// 在 `target/tmp` 下建一个独立测试根目录（**不用** `%TEMP%`）。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("device-revocation-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 一篇笔记的稳定标识（结构节点 + 正文文档 + 写入的文字）。
struct Note {
    doc_id: String,
    text: String,
}

fn create_note(vault: &Vault, peer: u64, epoch: u64, title: &str, text: &str) -> Note {
    let ctx = SyncContext::new(peer, epoch);
    let mut writer =
        StructureWriter::open_with_sync(vault, peer, Some(ctx)).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("device-revocation-note")
        .expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, peer, epoch, text);
    Note {
        doc_id: seed.body_doc_id,
        text: text.to_owned(),
    }
}

fn write_body(vault: &Vault, doc_id: &str, peer: u64, epoch: u64, text: &str) {
    let (doc, head) = load_body_doc(vault, doc_id, peer, None).expect("加载刚投影出的正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    commit_body(
        vault,
        doc_id,
        peer,
        epoch,
        &doc,
        &head,
        "device-revocation-body",
    );
}

/// 在正文末尾追加一段（同一 A 事务 + 出站意图），模拟 B 的本地编辑。
fn append_body(vault: &Vault, doc_id: &str, peer: u64, epoch: u64, suffix: &str) {
    let (doc, head) = load_body_doc(vault, doc_id, peer, None).expect("加载正文失败");
    let pos = doc.scalar_len();
    doc.insert_scalars(pos, suffix).expect("追加正文失败");
    commit_body(
        vault,
        doc_id,
        peer,
        epoch,
        &doc,
        &head,
        "device-revocation-append",
    );
}

fn commit_body(
    vault: &Vault,
    doc_id: &str,
    peer: u64,
    epoch: u64,
    doc: &BodyDoc,
    head: &HeadState,
    batch: &str,
) {
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    let seq = head.head_seq.saturating_add(1);
    let intent = local_sync_intent(doc_id, seq, batch, SyncContext::new(peer, epoch))
        .expect("派生 sync intent 失败");
    append_updates_with_sync_intents(
        vault,
        doc_id,
        head.head_seq,
        &[UpdateRecord::new(intent.batch_id.clone(), update)],
        &[intent],
    )
    .expect("A 事务写入正文 + intent 失败");
}

fn body_text(vault: &Vault, doc_id: &str, peer: u64) -> String {
    load_body_doc(vault, doc_id, peer, None)
        .expect("加载正文失败")
        .0
        .text()
}

/// 可推进的测试时钟（既是客户端 token 时间源，也与 relay 的 `FakeClock` 对齐）。
#[derive(Clone)]
struct SharedClock(Arc<AtomicU64>);

impl SharedClock {
    fn new(now: u64) -> Self {
        Self(Arc::new(AtomicU64::new(now)))
    }

    fn set(&self, now: u64) {
        self.0.store(now, Ordering::Relaxed);
    }
}

impl Clock for SharedClock {
    fn now_secs(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

fn relay_now(relay: &DevRelay) -> u64 {
    u64::try_from(relay.now()).expect("relay 假时钟必须非负")
}

/// 设备 realm 的真实 HTTP 客户端（真实控制头 pin + 注入时钟）。
fn device_client(
    relay_url: &str,
    identity: &LocalIdentityV1,
    clock: SharedClock,
) -> HttpRelayClient<StdTcpTransport, SharedClock> {
    let transport = StdTcpTransport::new_local_dev(relay_url).expect("构造本地传输失败");
    let config = HttpRelayConfig {
        base_url: relay_url.to_owned(),
        api_version: RelayApiVersion::V1,
        vault_id: identity.vault_id,
        device_id: identity.device_id,
        manager_pub: Some(identity.pinned_manager),
    };
    HttpRelayClient::with_clock(
        transport,
        config,
        DeviceSigningKey::from_seed(identity.ed25519_seed.clone()),
        clock,
    )
    .with_root(root_of(identity))
    .with_control_pin(ControlPin::from_identity(identity))
}

fn root_of(identity: &LocalIdentityV1) -> RootSecretV1 {
    RootSecretV1::new(
        identity.vault_id,
        identity.current_epoch,
        identity.current_root.clone(),
    )
}

fn engine_identity(identity: &LocalIdentityV1) -> EngineIdentity {
    EngineIdentity {
        device_id: identity.device_id,
        signing_key: DeviceSigningKey::from_seed(identity.ed25519_seed.clone()),
        peer_id: identity.loro_peer_id(),
    }
}

/// 管理私钥：只从恢复码派生（与 `DevLocal::join` 的信任锚同源）。
fn manager_of(recovery_code: &str, vault_id: &menghunote_sync::VaultId) -> ManagementKey {
    let seed = RecoverySeed::from_code(recovery_code).expect("解析恢复码失败");
    ManagementKey::from_recovery(&seed, vault_id).expect("派生管理私钥失败")
}

/// 新的设备 realm 客户端读一次 `/state`（拿 current control_rev / auth_rev）。
fn fresh_state(relay: &DevRelay, identity: &LocalIdentityV1) -> RelayState {
    let mut client = device_client(
        relay.base_url(),
        identity,
        SharedClock::new(relay_now(relay)),
    );
    client.state().expect("读 relay /state 失败")
}

/// 一次吊销调用（每次用独立 transport，模拟真实调用点）。
fn revoke(
    relay: &DevRelay,
    context_owner: &LocalIdentityV1,
    manager: &ManagementKey,
    expected_control_rev: u64,
    target: DeviceId,
    request_id: [u8; 16],
) -> Result<RevokeOutcome, SyncError> {
    let ctx = RevokeContext::from_identity(
        relay.base_url(),
        context_owner,
        expected_control_rev,
        relay_now(relay),
    );
    let mut transport = StdTcpTransport::new_local_dev(relay.base_url()).expect("构造传输失败");
    revoke_device(&mut transport, &ctx, manager, target, request_id)
}

/// 断言错误是 relay 的 `REVOKED`（访问已撤销）。
fn assert_revoked(error: &SyncError, what: &str) {
    match error {
        SyncError::RelayApi { code, .. } => assert_eq!(
            code, "REVOKED",
            "{what} 必须以 REVOKED 失败（旧 token / 新 challenge 都被拒），实际 {error}"
        ),
        other => panic!("{what} 必须是 relay 明确拒绝（REVOKED），实际 {other}"),
    }
}

/// A/B 两设备夹具：A create + 一篇笔记 + 发布基线 → B 用恢复码 join + 首轮同步。
struct Fixture {
    relay: DevRelay,
    _dir: TempDir,
    a: CreatedVault,
    b: JoinedVault,
    note: Note,
}

fn establish(prefix: &str) -> Fixture {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A 实例")
        .expect("A create 失败");
    let note = create_note(
        &a.vault,
        a.identity.loro_peer_id(),
        a.identity.current_epoch,
        "吊销测试笔记",
        "A 写的原始正文\n",
    );
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("A 发布初始基线失败");
    let b_path = dir.path().join("b").join("vault.sqlite3");
    let b = DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path)
        .expect("B join 失败");
    DevLocal::sync_once(relay.base_url(), &b.vault, &b.identity).expect("B 首轮 sync_once 失败");
    Fixture {
        relay,
        _dir: dir,
        a,
        b,
        note,
    }
}

// ---------------------------------------------------------------------------
// 1) A 吊销 B → B 旧客户端立刻 REVOKED；A 继续同步；helper 不动 B 本地数据
// ---------------------------------------------------------------------------

#[test]
fn revoke_blocks_old_client_immediately_and_a_keeps_working() {
    let fixture = establish("immediate");
    let relay_url = fixture.relay.base_url().to_owned();
    let b_device = fixture.b.identity.device_id;
    let b_peer = fixture.b.identity.loro_peer_id();
    let b_epoch = fixture.b.identity.current_epoch;
    let doc_id = fixture.note.doc_id.clone();

    // B 的**旧客户端**：吊销前完成一次真实同步并缓存 token/control（后续复用同一个实例）。
    let mut b_client = device_client(
        &relay_url,
        &fixture.b.identity,
        SharedClock::new(relay_now(&fixture.relay)),
    );
    {
        let mut engine = SyncEngine::new(
            &fixture.b.vault,
            &mut b_client,
            engine_identity(&fixture.b.identity),
            root_of(&fixture.b.identity),
        );
        engine.sync_once().expect("吊销前 B 的旧客户端同步应成功");
    }
    b_client.control_head().expect("吊销前缓存控制链失败");
    assert!(b_client.cached_auth_rev().is_some());

    let before = fresh_state(&fixture.relay, &fixture.a.identity);
    let manager = manager_of(&fixture.a.recovery_code, &fixture.a.vault_id);

    // A 用管理端 helper 吊销 B（独立 transport，只发管理端请求）。
    let outcome = revoke(
        &fixture.relay,
        &fixture.a.identity,
        &manager,
        before.control_rev,
        b_device,
        [0x11; 16],
    )
    .expect("A 吊销 B 失败");
    assert!(outcome.changed, "首次吊销必须真的改变状态");
    assert_eq!(outcome.target, b_device);
    assert_eq!(
        outcome.control_rev, before.control_rev,
        "独立吊销不得推进 control_rev"
    );
    assert!(
        outcome.auth_rev > before.auth_rev,
        "真吊销必须推进 auth_rev：{} → {}",
        before.auth_rev,
        outcome.auth_rev
    );

    // B 旧客户端：缓存 token 立刻被 relay 拒绝（REVOKED），并清掉 token/control 缓存。
    let error = b_client
        .state()
        .expect_err("吊销后 B 的旧 token 必须立即失效");
    assert_revoked(&error, "B 旧客户端的 /state");
    assert_eq!(
        b_client.cached_control_rev(),
        None,
        "REVOKED 必须清掉成员/控制缓存"
    );
    assert_eq!(b_client.cached_auth_rev(), None);

    // B 同步：第一步 push 没有待推送项（不联网），pull 的 /state 必须 REVOKED。
    let error = SyncEngine::new(
        &fixture.b.vault,
        &mut b_client,
        engine_identity(&fixture.b.identity),
        root_of(&fixture.b.identity),
    )
    .sync_once()
    .expect_err("吊销后 B 的 pull 必须失败");
    assert_revoked(&error, "B 的 pull");

    // B 本地继续编辑（本地写入不联网）→ push 必须 REVOKED，且不能把 outbox 当已 ACK。
    append_body(
        &fixture.b.vault,
        &doc_id,
        b_peer,
        b_epoch,
        "B 被吊销后的本地编辑\n",
    );
    let error = SyncEngine::new(
        &fixture.b.vault,
        &mut b_client,
        engine_identity(&fixture.b.identity),
        root_of(&fixture.b.identity),
    )
    .sync_once()
    .expect_err("吊销后 B 的 push 必须失败");
    assert_revoked(&error, "B 的 push");

    // B 的成员集合也不能拿旧缓存假成功：缓存已被清，刷新必须先认证 → REVOKED。
    let error = b_client
        .active_member_hpke_keys()
        .expect_err("被吊销设备不得再从旧缓存返回 active 成员");
    assert_revoked(&error, "B 的成员集合刷新");

    // B 新 challenge 也被拒（全新的客户端，无任何缓存 token）。
    let mut fresh_b = device_client(
        &relay_url,
        &fixture.b.identity,
        SharedClock::new(relay_now(&fixture.relay)),
    );
    let error = fresh_b
        .state()
        .expect_err("被吊销设备的新 challenge 必须被拒绝");
    assert_revoked(&error, "B 的新 challenge");

    // A 继续同步正常（新 transport 重新认证；A 是 ACTIVE 设备）。
    DevLocal::sync_once(&relay_url, &fixture.a.vault, &fixture.a.identity)
        .expect("A 在 B 被吊销后必须仍能同步");

    // A 侧控制链 active 集合：B 已被排除，A 仍在。
    let mut a_client = device_client(
        &relay_url,
        &fixture.a.identity,
        SharedClock::new(relay_now(&fixture.relay)),
    );
    let a_state = a_client.state().expect("A 读 /state 失败");
    assert_eq!(a_state.control_rev, before.control_rev);
    assert_eq!(a_state.auth_rev, outcome.auth_rev);
    let active = a_client
        .active_member_hpke_keys()
        .expect("读 active 集合失败");
    assert!(
        active.iter().all(|(device, _)| *device != b_device),
        "active 集合必须排除被吊销的 B"
    );
    assert!(
        active
            .iter()
            .any(|(device, _)| *device == fixture.a.identity.device_id),
        "active 集合必须仍含 A"
    );

    // helper 只有网络请求：B 的本地身份/笔记/恢复材料不被改写。
    assert!(
        fixture.b.identity.manager_seed.is_none(),
        "B 是普通设备，吊销不得凭空生成/删除管理私钥"
    );
    assert_eq!(fixture.b.identity.device_id, b_device);
    assert!(
        body_text(&fixture.b.vault, &doc_id, b_peer).contains(fixture.note.text.trim_end()),
        "被吊销设备的本地笔记必须仍可读"
    );
    assert!(
        body_text(&fixture.b.vault, &doc_id, b_peer).contains("B 被吊销后的本地编辑"),
        "被吊销设备后续的本地编辑也仍然落在本地"
    );
}

// ---------------------------------------------------------------------------
// 2) 缓存路径：吊销只推进 auth_rev（control_rev 不变）→ 有缓存也必须刷新
// ---------------------------------------------------------------------------

#[test]
fn auth_rev_bump_refreshes_cached_members_without_control_rev_change() {
    let fixture = establish("cache-refresh");
    let relay_url = fixture.relay.base_url().to_owned();
    let b_device = fixture.b.identity.device_id;

    // A 的客户端：正常认证 + 缓存控制链/active 集合（模拟长期在线设备）。
    let mut a_client = device_client(
        &relay_url,
        &fixture.a.identity,
        SharedClock::new(relay_now(&fixture.relay)),
    );
    let before = a_client.state().expect("A 读 /state 失败");
    let (head_rev, _head_hash) = a_client.control_head().expect("A 缓存控制链失败");
    assert_eq!(head_rev, before.control_rev);
    let members_before = a_client
        .active_member_hpke_keys()
        .expect("读 active 集合失败");
    assert!(
        members_before.iter().any(|(device, _)| *device == b_device),
        "吊销前 active 集合必须含 B"
    );
    let digest_before = a_client
        .active_member_cert_digest()
        .expect("读成员摘要失败");
    let kinds_before = a_client.control_kinds();

    let manager = manager_of(&fixture.a.recovery_code, &fixture.a.vault_id);
    let outcome = revoke(
        &fixture.relay,
        &fixture.a.identity,
        &manager,
        before.control_rev,
        b_device,
        [0x12; 16],
    )
    .expect("A 吊销 B 失败");

    // A 自己的旧 token 也被 auth_rev 推进作废：`state()` 在 REVOKED 后清缓存并重认证一次。
    let after = a_client.state().expect("A 重认证后 /state 必须成功");
    assert_eq!(
        after.control_rev, before.control_rev,
        "独立吊销只推进 auth_rev，control_rev 必须不变"
    );
    assert_eq!(after.auth_rev, outcome.auth_rev);
    assert!(
        after.auth_rev > before.auth_rev,
        "吊销后 /state.auth_rev 必须前进"
    );
    assert_eq!(
        a_client.cached_control_rev(),
        None,
        "旧 token 被拒后控制成员缓存必须已作废（不能拿旧缓存当事实）"
    );

    // 重新拉控制链：active 集合排除 B、成员摘要变化（缓存真的刷新了）。
    let members_after = a_client
        .active_member_hpke_keys()
        .expect("刷新 active 集合失败");
    assert!(
        members_after.iter().all(|(device, _)| *device != b_device),
        "刷新后的 active 集合必须排除 B"
    );
    assert!(members_after
        .iter()
        .any(|(device, _)| *device == fixture.a.identity.device_id));
    assert_eq!(
        a_client.cached_control_rev(),
        Some(after.control_rev),
        "刷新后的控制缓存 rev 不变"
    );
    assert_eq!(
        a_client.cached_auth_rev(),
        Some(after.auth_rev),
        "刷新后的控制缓存必须锚在新 auth_rev 上"
    );
    let digest_after = a_client
        .active_member_cert_digest()
        .expect("读刷新后成员摘要失败");
    assert_ne!(
        digest_before, digest_after,
        "成员集合变了，active 成员摘要必须变化"
    );

    // 控制链链尾仍精确等于 control_rev（不追加占位记录），吊销不是控制事实。
    let (tail_rev, _tail_hash) = a_client.control_head().expect("读控制链链尾失败");
    assert_eq!(tail_rev, after.control_rev, "链尾必须精确等于 control_rev");
    assert_eq!(
        a_client.control_kinds(),
        kinds_before,
        "独立吊销不得追加控制记录"
    );

    // 后续 grant/rotation 规划：只封给 active（A），不含 B。
    let manager_for_plan = manager_of(&fixture.a.recovery_code, &fixture.a.vault_id);
    let leaves = plan_all_active_grants(
        &mut a_client,
        &root_of(&fixture.a.identity),
        &manager_for_plan,
        GENESIS_MANAGER_REV,
    )
    .expect("规划全 active grants 失败");
    assert!(
        leaves.iter().all(|leaf| leaf.device_id != b_device),
        "grant 规划不得包含被吊销的 B"
    );
    assert!(
        leaves
            .iter()
            .any(|leaf| leaf.device_id == fixture.a.identity.device_id),
        "grant 规划必须仍包含 A"
    );
}

// ---------------------------------------------------------------------------
// 3) token 还"看起来有效"时，`/state.auth_rev` 比对也必须让成员缓存失效
// ---------------------------------------------------------------------------

#[test]
fn state_auth_rev_change_invalidates_control_cache_even_with_valid_token() {
    let fixture = establish("auth-rev-compare");
    let relay_url = fixture.relay.base_url().to_owned();
    let b_device = fixture.b.identity.device_id;
    let t0 = relay_now(&fixture.relay);
    let clock = SharedClock::new(t0);
    let mut a_client = device_client(&relay_url, &fixture.a.identity, clock.clone());
    let before = a_client.state().expect("A 读 /state 失败");
    a_client.control_head().expect("A 缓存控制链失败");
    assert_eq!(a_client.cached_auth_rev(), Some(before.auth_rev));

    let manager = manager_of(&fixture.a.recovery_code, &fixture.a.vault_id);
    let outcome = revoke(
        &fixture.relay,
        &fixture.a.identity,
        &manager,
        before.control_rev,
        b_device,
        [0x13; 16],
    )
    .expect("A 吊销 B 失败");

    // 让客户端 token 自然过期（客户端与 relay 假时钟一起推进）：`state()` 会先重认证，
    // 于是 /state 成功返回新 auth_rev，而不是走 REVOKED 重试路径——此时**只能**靠
    // `state.auth_rev != cached` 让旧的成员缓存失效。
    fixture.relay.advance_secs(700);
    clock.set(t0 + 700);

    let after = a_client.state().expect("token 过期后 /state 必须成功");
    assert_eq!(after.control_rev, before.control_rev, "control_rev 不变");
    assert_eq!(after.auth_rev, outcome.auth_rev, "auth_rev 必须观察到");
    assert_eq!(
        a_client.cached_control_rev(),
        None,
        "即使 token 仍可用，auth_rev 变化也必须清成员缓存"
    );
    assert_eq!(a_client.cached_auth_rev(), None);

    let members = a_client
        .active_member_hpke_keys()
        .expect("刷新 active 集合失败");
    assert!(
        members.iter().all(|(device, _)| *device != b_device),
        "刷新后的 active 集合必须排除 B"
    );
    assert_eq!(a_client.cached_auth_rev(), Some(after.auth_rev));
}

// ---------------------------------------------------------------------------
// 4) helper 本地守卫：self-remove / 错管理 key 直接拒绝，不发请求
// ---------------------------------------------------------------------------

/// 传输层探针：任何请求都算失败（本地守卫必须先于网络拒绝）。
#[derive(Default)]
struct CountingTransport {
    calls: usize,
}

impl HttpTransport for CountingTransport {
    fn send(&mut self, _request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        self.calls += 1;
        Err(SyncError::Transport("本地守卫必须先于任何 HTTP 请求"))
    }
}

#[test]
fn helper_refuses_self_remove_and_wrong_manager_before_any_request() {
    let fixture = establish("guard");
    let manager = manager_of(&fixture.a.recovery_code, &fixture.a.vault_id);
    let control_rev = fresh_state(&fixture.relay, &fixture.a.identity).control_rev;
    let context = RevokeContext::from_identity(
        fixture.relay.base_url(),
        &fixture.a.identity,
        control_rev,
        relay_now(&fixture.relay),
    );
    let mut transport = CountingTransport::default();

    // ① 禁止移除本设备：本地拒绝，不发任何请求。
    let error = revoke_device(
        &mut transport,
        &context,
        &manager,
        fixture.a.identity.device_id,
        [0x14; 16],
    )
    .expect_err("self-remove 必须被本地拒绝");
    match &error {
        SyncError::Unsupported(message) => assert!(
            message.contains("本设备"),
            "self-remove 的错误必须说明禁止本设备，实际 {message}"
        ),
        other => panic!("self-remove 必须是本地 Unsupported 拒绝，实际 {other}"),
    }
    assert_eq!(transport.calls, 0, "self-remove 不得发出任何 HTTP 请求");

    // ② 错管理 key（不是该身份对应的管理代）：本地拒绝，不发任何请求。
    //    "缺管理 key" 由类型保证（`&ManagementKey`），标准入口拿不到 key 时不应构造调用。
    let wrong_manager = ManagementKey::from_seed(Secret32::from_array([0x5b; 32]));
    assert_ne!(wrong_manager.public_key(), context.manager_pub);
    let error = revoke_device(
        &mut transport,
        &context,
        &wrong_manager,
        fixture.b.identity.device_id,
        [0x15; 16],
    )
    .expect_err("错管理 key 必须被本地拒绝");
    match &error {
        SyncError::Unsupported(message) => assert!(
            message.contains("管理"),
            "错管理 key 的错误必须说明管理公钥不符，实际 {message}"
        ),
        other => panic!("错管理 key 必须是本地 Unsupported 拒绝，实际 {other}"),
    }
    assert_eq!(transport.calls, 0, "错管理 key 不得发出任何 HTTP 请求");
}

// ---------------------------------------------------------------------------
// 5) 幂等 / 重放 / 负例无副作用
// ---------------------------------------------------------------------------

#[test]
fn revoke_is_idempotent_and_failures_have_no_side_effects() {
    let fixture = establish("idempotent");
    let b_device = fixture.b.identity.device_id;
    let manager = manager_of(&fixture.a.recovery_code, &fixture.a.vault_id);
    let before = fresh_state(&fixture.relay, &fixture.a.identity);

    // ① 目标不存在 → 404 NOT_FOUND 映射；失败不得推进 auth_rev。
    let error = revoke(
        &fixture.relay,
        &fixture.a.identity,
        &manager,
        before.control_rev,
        [0xee; 16],
        [0x20; 16],
    )
    .expect_err("不存在的 target 必须被 relay 拒绝");
    match &error {
        SyncError::RelayApi { code, .. } => assert_eq!(code, "NOT_FOUND"),
        other => panic!("不存在的 target 必须是 RelayApi NOT_FOUND，实际 {other}"),
    }
    let after_missing = fresh_state(&fixture.relay, &fixture.a.identity);
    assert_eq!(
        after_missing.auth_rev, before.auth_rev,
        "失败请求不得推进 auth_rev"
    );
    assert_eq!(after_missing.control_rev, before.control_rev);

    // ② 首次真吊销。
    let first = revoke(
        &fixture.relay,
        &fixture.a.identity,
        &manager,
        before.control_rev,
        b_device,
        [0x21; 16],
    )
    .expect("首次吊销失败");
    assert!(first.changed);
    assert!(first.auth_rev > before.auth_rev);

    // ③ 响应丢失后用**同 id 同 B** 重试：回放原响应，auth_rev 不二次推进。
    let replayed = revoke(
        &fixture.relay,
        &fixture.a.identity,
        &manager,
        before.control_rev,
        b_device,
        [0x21; 16],
    )
    .expect("同 id/B 重试必须回放原响应");
    assert_eq!(replayed, first, "重放必须逐字段等于首次结果");

    // ④ 换新 request_id 的重复吊销：changed=false，auth_rev/control_rev 都不再推进。
    let again = revoke(
        &fixture.relay,
        &fixture.a.identity,
        &manager,
        before.control_rev,
        b_device,
        [0x22; 16],
    )
    .expect("重复吊销必须幂等成功");
    assert!(!again.changed, "已 revoked 的目标必须返回 changed=false");
    assert_eq!(again.auth_rev, first.auth_rev, "auth_rev 不得二次推进");
    assert_eq!(again.control_rev, first.control_rev);

    // ⑤ 同 request_id 不同 canonical body（不同 target）→ ID_CONFLICT（防跨设备碰撞）。
    let error = revoke(
        &fixture.relay,
        &fixture.a.identity,
        &manager,
        before.control_rev,
        [0xef; 16],
        [0x21; 16],
    )
    .expect_err("同 id 不同 body 必须被拒绝");
    match &error {
        SyncError::RelayApi { code, .. } => assert_eq!(code, "ID_CONFLICT"),
        other => panic!("同 id 不同 body 必须是 ID_CONFLICT，实际 {other}"),
    }

    // ⑥ relay 侧事实：auth_rev 只在首次真吊销推进一次；control_rev 始终不变。
    let after = fresh_state(&fixture.relay, &fixture.a.identity);
    assert_eq!(after.auth_rev, first.auth_rev, "auth_rev 只推进一次");
    assert_eq!(
        after.control_rev, before.control_rev,
        "control_rev 始终不变"
    );

    // A 仍能在吊销 B 之后正常同步（负例/幂等路径没有破坏活动设备）。
    DevLocal::sync_once(
        fixture.relay.base_url(),
        &fixture.a.vault,
        &fixture.a.identity,
    )
    .expect("幂等负例之后 A 必须仍能同步");
    assert!(!list_tree(&fixture.a.vault)
        .expect("读 A 结构投影失败")
        .is_empty());
}

// ---------------------------------------------------------------------------
// 6) ADR 0010 切片 2a-1：只读成员视图（Active/Revoked + 指纹 + 稳定排序）
// ---------------------------------------------------------------------------

/// 成员视图里按设备 ID 取一条（缺失即测试失败）。
fn view_for(members: &[MemberView], device: DeviceId) -> &MemberView {
    members
        .iter()
        .find(|view| view.device_id == device)
        .expect("成员视图缺少设备")
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn member_view_reports_active_then_revoked_and_keeps_grants_unchanged() {
    let fixture = establish("member-view");
    let relay_url = fixture.relay.base_url().to_owned();
    let a_device = fixture.a.identity.device_id;
    let b_device = fixture.b.identity.device_id;

    // A 的一个长期客户端：create + join 之后，A/B 都应是 Active 且有指纹。
    let mut client = device_client(
        &relay_url,
        &fixture.a.identity,
        SharedClock::new(relay_now(&fixture.relay)),
    );
    let before = client.state().expect("A 读 /state 失败");
    let members_before = client.members().expect("读成员视图失败");
    assert_eq!(
        members_before
            .iter()
            .map(|view| view.device_id)
            .collect::<Vec<_>>(),
        {
            let mut sorted = vec![a_device, b_device];
            sorted.sort();
            sorted
        },
        "成员视图必须按 device_id 稳定升序且同时包含 A/B"
    );
    // 当前 create/join 流程会把 B 直接批准，不留 pending 成员；pending 投影
    // （空证书 → Pending + fingerprint=None）由 `http_relay` 的单元测试构造覆盖。
    assert_eq!(
        members_before.len(),
        2,
        "create/join 之后不应残留 pending 成员"
    );
    for view in &members_before {
        assert_eq!(
            view.status,
            MemberStatus::Active,
            "create/join 后成员必须 Active"
        );
        let fingerprint = view.fingerprint.clone().expect("active 成员必须有指纹");
        assert_eq!(fingerprint.len(), 32, "指纹是 32 位小写 hex");
        assert!(
            fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "指纹必须是 32 位小写 hex，实际 {fingerprint}"
        );
        assert_eq!(
            view.issued_manager_rev,
            Some(GENESIS_MANAGER_REV),
            "genesis 证书的签发管理版本必须可读出"
        );
    }
    assert_ne!(
        members_before[0].fingerprint, members_before[1].fingerprint,
        "两台设备的指纹必须不同"
    );
    // 同一客户端再读一次：内容与排序稳定（不随 HashMap 遍历序抖动）。
    assert_eq!(
        client.members().expect("重复读成员视图失败"),
        members_before,
        "成员视图必须稳定（内容 + device_id 升序）"
    );
    assert_eq!(client.cached_control_rev(), Some(before.control_rev));

    // A 吊销 B：控制链/成员证书都不变（只推进 auth_rev 与 relay member.status）。
    let manager = manager_of(&fixture.a.recovery_code, &fixture.a.vault_id);
    let outcome = revoke(
        &fixture.relay,
        &fixture.a.identity,
        &manager,
        before.control_rev,
        b_device,
        [0x31; 16],
    )
    .expect("A 吊销 B 失败");
    assert!(outcome.changed, "首次吊销必须真的改变状态");
    assert_eq!(
        outcome.control_rev, before.control_rev,
        "独立吊销不得推进 control_rev"
    );

    // 同一个客户端：members() 必须先比对 /state.auth_rev 丢掉旧缓存，再刷新控制链。
    let members_after = client.members().expect("吊销后刷新成员视图失败");
    assert_eq!(
        client.cached_control_rev(),
        Some(before.control_rev),
        "刷新后的控制缓存 control_rev 不变"
    );
    assert_eq!(
        client.cached_auth_rev(),
        Some(outcome.auth_rev),
        "刷新后的控制缓存必须锚在新 auth_rev 上"
    );

    // A 仍 Active；B 变 Revoked，但历史证书保留 → 指纹仍可解析且与吊销前一致。
    assert_eq!(
        view_for(&members_after, a_device).status,
        MemberStatus::Active
    );
    let b_after = view_for(&members_after, b_device);
    assert_eq!(
        b_after.status,
        MemberStatus::Revoked,
        "吊销后 B 在成员视图里必须显示 Revoked"
    );
    assert_eq!(
        b_after.fingerprint,
        view_for(&members_before, b_device).fingerprint,
        "被吊销设备的历史证书保留 → 指纹不变"
    );
    assert!(b_after.fingerprint.is_some(), "Revoked 成员仍可解析指纹");
    assert_eq!(
        b_after.issued_manager_rev,
        Some(GENESIS_MANAGER_REV),
        "历史证书的签发管理版本仍可读出"
    );

    // 成员视图不改授权：active 集合排除 B、仍含 A；grant 规划同样不含 B。
    let active = client
        .active_member_hpke_keys()
        .expect("读 active 集合失败");
    assert!(
        active.iter().all(|(device, _)| *device != b_device),
        "active 集合必须排除被吊销的 B"
    );
    assert!(
        active.iter().any(|(device, _)| *device == a_device),
        "active 集合必须仍含 A"
    );
    let leaves = plan_all_active_grants(
        &mut client,
        &root_of(&fixture.a.identity),
        &manager,
        GENESIS_MANAGER_REV,
    )
    .expect("规划全 active grants 失败");
    assert!(
        leaves.iter().all(|leaf| leaf.device_id != b_device),
        "grant 规划不得包含被吊销的 B"
    );
    assert!(
        leaves.iter().any(|leaf| leaf.device_id == a_device),
        "grant 规划必须仍包含 A"
    );

    // 链尾校验不炸：控制链链尾仍精确等于 control_rev（吊销不是控制事实）。
    let (tail_rev, _) = client.control_head().expect("读控制链链尾失败");
    assert_eq!(tail_rev, before.control_rev, "链尾必须精确等于 control_rev");

    // 视图只暴露公开密钥字段；Debug 不含完整 sign_pk/hpke_pk。
    let b_sign = DeviceSigningKey::from_seed(fixture.b.identity.ed25519_seed.clone()).public_key();
    let b_hpke = DeviceHpkeKey::from_secret(fixture.b.identity.x25519_secret.clone()).public_key();
    assert_eq!(b_after.sign_pk, b_sign, "sign_pk 必须是设备真实公钥");
    assert_eq!(b_after.hpke_pk, b_hpke, "hpke_pk 必须是设备真实公钥");
    let debug = format!("{b_after:?}");
    assert!(
        debug.contains("MemberView") && debug.contains("Revoked"),
        "Debug 必须带类型与状态，实际 {debug}"
    );
    assert!(
        debug.contains(b_after.fingerprint.as_deref().expect("指纹")),
        "Debug 必须带指纹，实际 {debug}"
    );
    assert!(
        !debug.contains(&hex_lower(&b_sign)) && !debug.contains(&hex_lower(&b_hpke)),
        "Debug 不得泄漏完整 sign_pk/hpke_pk，实际 {debug}"
    );
}

// ---------------------------------------------------------------------------
// 7) ADR 0010 切片 2a-2：DevLocal 的 GPUI-free 数据入口（成员列表 / 移除设备）
// ---------------------------------------------------------------------------

/// 从 `DevLocal::members` 的返回里按状态统计（缺失即测试失败）。
fn statuses(members: &[MemberView], device: DeviceId) -> MemberStatus {
    view_for(members, device).status
}

#[test]
fn dev_local_members_lists_devices_and_revoke_marks_target_revoked() {
    let fixture = establish("dev-local-members");
    let relay_url = fixture.relay.base_url().to_owned();
    let a_device = fixture.a.identity.device_id;
    let b_device = fixture.b.identity.device_id;

    // ① 列表入口：A 侧两台设备都 Active、按 device_id 稳定升序、指纹可读。
    let members = DevLocal::members(&relay_url, &fixture.a.vault, &fixture.a.identity)
        .expect("A 读设备列表失败");
    assert_eq!(members.len(), 2, "create/join 后必须恰好两台设备");
    let mut expected = vec![a_device, b_device];
    expected.sort();
    assert_eq!(
        members
            .iter()
            .map(|view| view.device_id)
            .collect::<Vec<_>>(),
        expected,
        "列表必须按 device_id 稳定升序"
    );
    for view in &members {
        assert_eq!(view.status, MemberStatus::Active, "加入后两台设备都 Active");
        let fingerprint = view.fingerprint.clone().expect("active 成员必须有指纹");
        assert_eq!(fingerprint.len(), 32, "指纹是 32 位小写 hex");
    }
    // 只读入口不泄漏密钥材料：Debug 不带完整 sign_pk/hpke_pk。
    let debug = format!("{members:?}");
    let a_sign = DeviceSigningKey::from_seed(fixture.a.identity.ed25519_seed.clone()).public_key();
    assert!(
        !debug.contains(&hex_lower(&a_sign)),
        "设备列表 Debug 不得带完整 sign_pk"
    );

    // ② Join 设备（没有管理密钥）也能读列表：列表只需设备 realm 认证。
    let members_b = DevLocal::members(&relay_url, &fixture.b.vault, &fixture.b.identity)
        .expect("B（Join 设备）读设备列表失败");
    assert_eq!(members_b.len(), 2, "B 必须看到同样的两台设备");
    assert!(members_b
        .iter()
        .all(|view| view.status == MemberStatus::Active));

    // ③ 移除 B：`DevLocal::revoke_device` 返回 relay 事实 + 刷新后的列表。
    let before = fresh_state(&fixture.relay, &fixture.a.identity);
    let revoked =
        DevLocal::revoke_device(&relay_url, &fixture.a.vault, &fixture.a.identity, b_device)
            .expect("A 移除 B 失败");
    assert!(revoked.outcome.changed, "首次移除必须真的改变状态");
    assert_eq!(revoked.outcome.target, b_device);
    assert_eq!(
        revoked.outcome.control_rev, before.control_rev,
        "独立移除不得推进 control_rev"
    );
    assert!(
        revoked.outcome.auth_rev > before.auth_rev,
        "移除必须推进 auth_rev：{} → {}",
        before.auth_rev,
        revoked.outcome.auth_rev
    );
    // 返回的列表已经是刷新后的：B=Revoked、A 仍 Active（且指纹不变）。
    assert_eq!(statuses(&revoked.members, b_device), MemberStatus::Revoked);
    assert_eq!(statuses(&revoked.members, a_device), MemberStatus::Active);
    assert_eq!(
        view_for(&revoked.members, b_device).fingerprint,
        view_for(&members, b_device).fingerprint,
        "历史证书保留 → 被移除设备的指纹不变"
    );
    // 与单独再拉一次的结果一致（入口不做任何缓存假象）。
    assert_eq!(
        DevLocal::members(&relay_url, &fixture.a.vault, &fixture.a.identity)
            .expect("移除后重读列表失败"),
        revoked.members,
        "revoke 返回的列表必须就是吊销后的真实视图"
    );

    // ④ 安全边界：被移除的 B 连列表（设备 realm 认证）都读不到；A 仍能同步。
    let error = DevLocal::members(&relay_url, &fixture.b.vault, &fixture.b.identity)
        .expect_err("被移除设备的列表读取必须 REVOKED");
    assert_revoked(&error, "B 的成员列表");
    DevLocal::sync_once(&relay_url, &fixture.a.vault, &fixture.a.identity)
        .expect("A 在移除 B 之后必须仍能同步");
    // B 的本地身份/笔记不被 helper 改写（只断访问）。
    assert!(
        fixture.b.identity.manager_seed.is_none(),
        "普通设备的身份不得因为被移除而获得/失去管理密钥"
    );
    assert!(
        !list_tree(&fixture.b.vault)
            .expect("读 B 结构投影失败")
            .is_empty(),
        "被移除设备的本地笔记必须原样保留"
    );
}

#[test]
fn dev_local_revoke_refuses_self_and_missing_manager_key_without_side_effects() {
    let fixture = establish("dev-local-guards");
    let relay_url = fixture.relay.base_url().to_owned();
    let a_device = fixture.a.identity.device_id;
    let b_device = fixture.b.identity.device_id;

    // ① 本机禁止移除：本地拒绝，且**不发任何请求**（B 仍 Active）。
    let error =
        DevLocal::revoke_device(&relay_url, &fixture.a.vault, &fixture.a.identity, a_device)
            .expect_err("移除本机必须被拒绝");
    match &error {
        SyncError::Unsupported(message) => assert!(
            message.contains("本机"),
            "错误必须点明不能移除本机：{message}"
        ),
        other => panic!("本机移除必须是本地拒绝（Unsupported），实际 {other}"),
    }
    let members = DevLocal::members(&relay_url, &fixture.a.vault, &fixture.a.identity)
        .expect("本机拒绝后仍应能读列表");
    assert_eq!(
        statuses(&members, b_device),
        MemberStatus::Active,
        "本机移除被拒后 B 必须仍是 Active（无副作用）"
    );

    // ② 缺管理密钥（Join 设备）：明确失败，也不发请求（A 仍 Active）。
    let error =
        DevLocal::revoke_device(&relay_url, &fixture.b.vault, &fixture.b.identity, a_device)
            .expect_err("Join 设备移除设备必须被拒绝");
    match &error {
        SyncError::Unsupported(message) => assert!(
            message.contains("管理密钥"),
            "错误必须点明缺管理密钥：{message}"
        ),
        other => panic!("缺管理密钥必须是本地拒绝（Unsupported），实际 {other}"),
    }
    let members = DevLocal::members(&relay_url, &fixture.a.vault, &fixture.a.identity)
        .expect("拒绝后仍应能读列表");
    assert_eq!(
        statuses(&members, a_device),
        MemberStatus::Active,
        "缺管理密钥被拒后 A 必须仍是 Active（无副作用）"
    );
    assert_eq!(statuses(&members, b_device), MemberStatus::Active);
}
