//! ADR 0014 **S1**：邀请链接（创建 → 兑换 PENDING → 管理端批准 → bootstrap）的真实 e2e。
//!
//! 用 `menghunote-dev-relay`：进程内真实 axum relay + 假时钟 + `127.0.0.1:0` 端口，客户端走
//! 真实 HTTP（`StdTcpTransport` 的开发策略）。**不 sleep**：收敛点全部是显式返回值。
//!
//! 覆盖：
//! 1. A 生成邀请链接（15 分钟 TTL，token 只在 fragment）→ B 解析/redeem → PENDING
//!    （自己查：`WaitingForApproval`）→ A 批准（幂等）→ B `check` = Approved →
//!    `finish` bootstrap 出身份（`manager_seed = None`）→ B 同步后能看到 A 的笔记，
//!    双向编辑再同步都可见；
//! 2. durable PENDING 跨重启：丢回应/关闭后 `InviteJoin::load` 仍可读，重新解析同一链接
//!    **不重复兑换**（用一个不可达的 context 也成功 → 证明没有联网）；
//! 3. 失败矩阵：重复兑换 `INVITE_USED`、TTL 过期 `EXPIRED`、错 vault `NOT_FOUND`、
//!    pin 不符 `InstanceMismatch`（且不自动换 pin）；
//! 4. 非管理设备不能批准（`Unsupported`，不发管理请求）；邀请链接不含恢复码。
//!
//! ⚠️ **非生产**：dev-relay 的 `DevPermissiveHook` 对任何签名一律放行；本文件验证的是
//! 邀请流程/状态机/数据面，不是 relay 鉴权验收。

use std::path::{Path, PathBuf};

use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, list_tree, load_body_doc, local_sync_intent, BodyDoc,
    HeadState, StructureWriter, SyncContext, UpdateRecord, Vault,
};
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::create_link::CreateTicket;
use menghunote_sync::dev_local::{CreatedVault, DevLocal};
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::invite_link::{
    approve_pending_device, create_invite_link, create_invite_link_with, InviteCheck, InviteFinish,
    InviteJoin, InviteJoinPhase, InviteLinkTarget, MAX_INVITE_TTL_SECS,
};
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::relay_policy::RelayMode;
use menghunote_sync::remote_init::create_vault_with_ticket;
use menghunote_sync::SyncError;
use tempfile::{Builder as TempDirBuilder, TempDir};

// ---------------------------------------------------------------------------
// 基础设施
// ---------------------------------------------------------------------------

/// Linux 上 pending 的本地封装需要每用户主钥（Windows DPAPI 是 no-op）。
/// 同一测试二进制内并发跑：`OnceLock` 保证只初始化一次，并把 `XDG_DATA_HOME` 指到
/// `target/tmp`（不污染真实用户目录）。
fn init_identity_wrap() {
    static INIT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    let result = INIT.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("invite-links-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key().map_err(|error| error.to_string())
    });
    if let Err(error) = result {
        panic!("初始化本地身份封装主键失败：{error}");
    }
}

/// 在 `target/tmp` 下建独立测试根目录（**不用** `%TEMP%`）。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("invite-links-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

fn local_context(relay: &DevRelay) -> RelayContext {
    RelayContext::local_dev(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS).expect("local context")
}

fn now_of(relay: &DevRelay) -> u64 {
    u64::try_from(relay.now()).expect("relay now")
}

struct Note {
    doc_id: String,
}

fn create_note(vault: &Vault, peer: u64, epoch: u64, title: &str, text: &str) -> Note {
    let ctx = SyncContext::new(peer, epoch);
    let mut writer =
        StructureWriter::open_with_sync(vault, peer, Some(ctx)).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer.commit("invite-note").expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, peer, epoch, text);
    Note {
        doc_id: seed.body_doc_id,
    }
}

fn write_body(vault: &Vault, doc_id: &str, peer: u64, epoch: u64, text: &str) {
    let (doc, head) = load_body_doc(vault, doc_id, peer, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    commit_body(vault, doc_id, peer, epoch, &doc, &head, "invite-body");
}

fn append_body(vault: &Vault, doc_id: &str, peer: u64, epoch: u64, suffix: &str) {
    let (doc, head) = load_body_doc(vault, doc_id, peer, None).expect("加载正文失败");
    let pos = doc.scalar_len();
    doc.insert_scalars(pos, suffix).expect("追加正文失败");
    commit_body(vault, doc_id, peer, epoch, &doc, &head, "invite-append");
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

/// A（创建端）+ 一篇笔记 + 已发布基线。
struct Creator {
    relay: DevRelay,
    dir: TempDir,
    created: CreatedVault,
    note: Note,
}

fn creator(prefix: &str) -> Creator {
    init_identity_wrap();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);
    let path = dir.path().join("a").join("vault.sqlite3");
    let created = DevLocal::create(relay.base_url(), relay.setup_secret(), &path, "A 实例")
        .expect("A create 失败");
    let note = create_note(
        &created.vault,
        created.identity.loro_peer_id(),
        created.identity.current_epoch,
        "A 的笔记",
        "A 写的第一行\n",
    );
    DevLocal::publish_initial_pair(
        relay.base_url(),
        &created.vault,
        &created.identity,
        &created.recovery_code,
    )
    .expect("A 发布初始基线失败");
    Creator {
        relay,
        dir,
        created,
        note,
    }
}

fn invite_link(creator: &Creator) -> menghunote_sync::invite_link::InviteLink {
    let context = local_context(&creator.relay);
    create_invite_link_with(
        &context,
        &creator.created.vault,
        &creator.created.identity,
        MAX_INVITE_TTL_SECS.min(900),
        now_of(&creator.relay),
    )
    .expect("生成邀请链接失败")
}

fn b_path(creator: &Creator) -> PathBuf {
    creator.dir.path().join("b").join("vault.sqlite3")
}

// ---------------------------------------------------------------------------
// 1) 端到端：链接 → PENDING → 批准 → bootstrap → 双向同步
// ---------------------------------------------------------------------------

#[test]
fn invite_link_flow_reaches_main_and_syncs_both_ways() {
    let a = creator("e2e");
    let link = invite_link(&a);
    assert!(
        link.url.starts_with("mn-pair://join?v=1&relay="),
        "{}",
        link.url
    );
    assert!(link.url.contains("#token="), "token 必须在 fragment");
    assert!(
        !link.url.contains(&a.created.recovery_code),
        "邀请链接绝不能包含恢复码"
    );
    // Debug 不打印 token/URL。
    assert!(!format!("{link:?}").contains("mn-pair"));
    // TTL 文案是公开的（无 token）。
    assert!(link.ttl_text(now_of(&a.relay)).contains("有效"));

    // B 解析 + 兑换（真实 relay）：只到 PENDING。
    let target = InviteLinkTarget::parse(&link.url).expect("解析邀请链接失败");
    assert_eq!(target.mode(), RelayMode::LocalDev);
    assert_eq!(target.vault_id(), a.created.vault_id);
    let context = local_context(&a.relay);
    let b_dir = b_path(&a);
    let started =
        InviteJoin::redeem(&context, &target, &b_dir, now_of(&a.relay)).expect("B 兑换邀请失败");
    assert!(!started.resumed);
    assert_eq!(started.pending.vault_id, a.created.vault_id);
    assert_eq!(started.pending.invite_id, link.invite_id);
    assert_eq!(started.pending.phase, InviteJoinPhase::Redeemed);
    assert_eq!(started.pending.mode, RelayMode::LocalDev);
    // pending 文件里的明文头不含私钥材料。
    let raw = started
        .vault
        .get_meta(menghunote_sync::invite_link::META_INVITE_PENDING)
        .expect("读 pending meta")
        .expect("pending meta 必须已落盘");
    assert_eq!(&raw[..4], b"MNIP");
    drop(started.vault);

    // B 立即检查：仍是 PENDING（relay 对 pending 回 REVOKED，不区分）。
    let vault = Vault::open(&b_dir).expect("重开 B 的库");
    assert_eq!(
        InviteJoin::check(&context, &vault, now_of(&a.relay)).expect("检查状态失败"),
        InviteCheck::WaitingForApproval
    );
    // 未批准时 finish 也明确是"等待"，不是错误，也不写任何身份。
    assert!(matches!(
        InviteJoin::finish(&context, &vault, now_of(&a.relay)).expect("finish 等待"),
        InviteFinish::WaitingForApproval
    ));

    // A 批准（需要管理钥）：成员视图里能看到 B 是 pending。
    let members = DevLocal::members(
        &local_context(&a.relay),
        &a.created.vault,
        &a.created.identity,
    )
    .expect("A 读设备列表失败");
    let pending = members
        .iter()
        .find(|member| member.device_id == started.pending.device_id)
        .expect("B 必须出现在成员列表里");
    assert_eq!(
        pending.status,
        menghunote_sync::http_relay::MemberStatus::Pending
    );
    let approved = approve_pending_device(
        &local_context(&a.relay),
        &a.created.vault,
        &a.created.identity,
        started.pending.device_id,
        now_of(&a.relay),
    )
    .expect("A 批准 B 失败");
    assert!(approved.changed, "首次批准必须真的改变状态");
    assert_eq!(approved.device_id, started.pending.device_id);
    assert_eq!(approved.fingerprint.len(), 32);
    // 幂等：重复批准返回 changed=false，且不再推进 control_rev。
    let again = approve_pending_device(
        &local_context(&a.relay),
        &a.created.vault,
        &a.created.identity,
        started.pending.device_id,
        now_of(&a.relay),
    )
    .expect("重复批准必须幂等成功");
    assert!(!again.changed);

    // B 检查 → Approved → bootstrap 出身份。
    assert_eq!(
        InviteJoin::check(&context, &vault, now_of(&a.relay)).expect("检查状态失败"),
        InviteCheck::Approved
    );
    let InviteFinish::Joined(joined) =
        InviteJoin::finish(&context, &vault, now_of(&a.relay)).expect("B bootstrap 失败")
    else {
        panic!("批准后 finish 必须直接完成 bootstrap");
    };
    assert_eq!(joined.pending.phase, InviteJoinPhase::Bootstrapped);
    let b_identity = joined.identity;
    assert!(b_identity.manager_seed.is_none(), "普通设备没有管理私钥");
    assert_eq!(b_identity.vault_id, a.created.vault_id);
    assert_eq!(b_identity.device_id, started.pending.device_id);
    assert_eq!(b_identity.pinned_instance, started.pending.pin);
    assert_eq!(b_identity.current_epoch, 1);
    // 身份 BLOB 可落盘/解封（app 的 DevIdentityStore 同一路径）。
    let blob = b_identity.protect().expect("身份封装失败");
    let reopened = LocalIdentityV1::unprotect(&blob, &b_identity.vault_id, &b_identity.device_id)
        .expect("身份解封失败");
    assert_eq!(reopened.loro_peer_id(), b_identity.loro_peer_id());

    // B 同步：看到 A 的笔记（bootstrap 已导入基线）。
    DevLocal::sync_once(a.relay.base_url(), &vault, &b_identity).expect("B 同步失败");
    let tree = list_tree(&vault).expect("B 读结构树失败");
    let doc_id = tree
        .iter()
        .find(|entry| entry.title.as_str() == "A 的笔记")
        .and_then(|entry| entry.body_doc_id.clone())
        .expect("B 必须看到 A 的笔记");
    let peer = b_identity.loro_peer_id();
    assert!(body_text(&vault, &doc_id, peer).contains("A 写的第一行"));

    // 双向编辑：B 追加 → 同步 → A 同步后看到。
    append_body(
        &vault,
        &doc_id,
        peer,
        b_identity.current_epoch,
        "B 追加的一行\n",
    );
    DevLocal::sync_once(a.relay.base_url(), &vault, &b_identity).expect("B 推送失败");
    DevLocal::sync_once(a.relay.base_url(), &a.created.vault, &a.created.identity)
        .expect("A 拉取失败");
    assert!(
        body_text(&a.created.vault, &doc_id, a.created.identity.loro_peer_id())
            .contains("B 追加的一行"),
        "A 必须看到 B 的编辑"
    );

    // 完成加入（app 写完配置后）：清 pending。
    InviteJoin::complete(&vault).expect("清理 pending 失败");
    assert!(InviteJoin::load(&vault).expect("读 pending").is_none());
    assert_eq!(a.note.doc_id, doc_id, "B 看到的必须是同一篇正文");
    // 清理后不能再次 finish（没有 pending 时是明确错误，不是静默成功）。
    assert!(InviteJoin::finish(&context, &vault, now_of(&a.relay)).is_err());
}

// ---------------------------------------------------------------------------
// 2) durable PENDING 跨重启：重贴同一链接不重复兑换（不联网）
// ---------------------------------------------------------------------------

#[test]
fn durable_pending_resumes_without_redeeming_again() {
    let a = creator("resume");
    let link = invite_link(&a);
    let target = InviteLinkTarget::parse(&link.url).expect("解析失败");
    let context = local_context(&a.relay);
    let b_dir = b_path(&a);
    let first = InviteJoin::redeem(&context, &target, &b_dir, now_of(&a.relay)).expect("首次兑换");
    let device = first.pending.device_id;
    let peer = first.pending.peer_id;
    drop(first.vault); // 模拟进程关闭

    // 重新打开：pending 仍在（阶段=redeemed），设备 id/peer 不变。
    let vault = Vault::open(&b_dir).expect("重开库");
    let pending = InviteJoin::load(&vault)
        .expect("读 pending")
        .expect("pending 必须存在");
    assert_eq!(pending.phase, InviteJoinPhase::Redeemed);
    assert_eq!(pending.device_id, device);
    assert_eq!(pending.peer_id, peer);

    // 用**不可达**的 context 重贴链接：resume 路径在联网之前就返回（证明不重复兑换）。
    let dead = RelayContext::local_dev("http://127.0.0.1:1", DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("dead context");
    let resumed = InviteJoin::redeem(&dead, &target, &b_dir, now_of(&a.relay))
        .expect("resume 不得联网/不得重复兑换");
    assert!(resumed.resumed, "必须走 resume 而不是再次 redeem");
    assert_eq!(resumed.pending.device_id, device);

    // 同一目录放另一份邀请：明确拒绝（不覆盖已有库）。
    let other = create_invite_link(&context, &a.created.vault, &a.created.identity)
        .expect("再生成一份邀请");
    let other_target = InviteLinkTarget::parse(&other.url).expect("解析失败");
    let error = InviteJoin::redeem(&context, &other_target, &b_dir, now_of(&a.relay)).err();
    assert!(
        matches!(error, Some(SyncError::LocalCorrupt(_))),
        "换成另一份邀请必须明确冲突：{error:?}"
    );
}

// ---------------------------------------------------------------------------
// 3) 失败矩阵：已用 / 过期 / 错 vault / 错 pin
// ---------------------------------------------------------------------------

#[test]
fn redeem_rejects_used_expired_and_wrong_vault_links() {
    let a = creator("errors");
    let context = local_context(&a.relay);
    let link = invite_link(&a);
    let target = InviteLinkTarget::parse(&link.url).expect("解析失败");

    // 第一次成功；第二次（新目录）→ INVITE_USED。
    let first_dir = a.dir.path().join("b1").join("vault.sqlite3");
    InviteJoin::redeem(&context, &target, &first_dir, now_of(&a.relay)).expect("首次兑换");
    let second_dir = a.dir.path().join("b2").join("vault.sqlite3");
    let used = InviteJoin::redeem(&context, &target, &second_dir, now_of(&a.relay)).err();
    assert!(
        matches!(&used, Some(SyncError::RelayApi { code, .. }) if code == "INVITE_USED"),
        "token 已用必须明确 INVITE_USED：{used:?}"
    );
    assert!(!second_dir.exists(), "失败不得留下半初始化库");

    // TTL 过期 → EXPIRED（relay 时钟推进）。
    let expiring = create_invite_link_with(
        &context,
        &a.created.vault,
        &a.created.identity,
        60,
        now_of(&a.relay),
    )
    .expect("生成短 TTL 邀请");
    let expiring_target = InviteLinkTarget::parse(&expiring.url).expect("解析失败");
    a.relay.advance_secs(120);
    let expired_dir = a.dir.path().join("b3").join("vault.sqlite3");
    let expired =
        InviteJoin::redeem(&context, &expiring_target, &expired_dir, now_of(&a.relay)).err();
    assert!(
        matches!(&expired, Some(SyncError::RelayApi { code, .. }) if code == "EXPIRED"),
        "过期邀请必须明确 EXPIRED：{expired:?}"
    );
    assert!(!expired_dir.exists(), "失败不得留下半初始化库");

    // 错 vault：链接里的 vault 换成另一个 32 位 hex → relay 明确拒绝（不建库）。
    let wrong_vault = {
        let hex = link.url.clone();
        let start = hex.find("vault=").expect("vault 字段") + "vault=".len();
        let mut bytes: Vec<u8> = hex.into_bytes();
        for offset in 0..32 {
            bytes[start + offset] = b'a';
        }
        String::from_utf8(bytes).expect("utf8")
    };
    let wrong_target = InviteLinkTarget::parse(&wrong_vault).expect("解析失败");
    let wrong_dir = a.dir.path().join("b4").join("vault.sqlite3");
    let wrong = InviteJoin::redeem(&context, &wrong_target, &wrong_dir, now_of(&a.relay)).err();
    assert!(
        matches!(&wrong, Some(SyncError::RelayApi { .. })),
        "错 vault 必须被 relay 明确拒绝：{wrong:?}"
    );
    assert!(!wrong_dir.exists(), "失败不得留下半初始化库");
}

#[test]
fn wrong_pin_is_rejected_after_approval_without_repinning() {
    let a = creator("pin");
    let context = local_context(&a.relay);
    let link = invite_link(&a);
    let target = InviteLinkTarget::parse(&link.url).expect("解析失败");
    // 换一个**格式合法**但错误的 pin：兑换不看 pin（relay 不返回 instance），
    // 批准后的设备认证必须拒绝，且**不会**自动换 pin。
    let wrong_pin_target = InviteLinkTarget::from_parts(
        target.mode(),
        target.relay(),
        target.vault_id(),
        target.invite_id(),
        [0xEE; 16],
        *target.token(),
        target.api_version(),
    );
    let b_dir = a.dir.path().join("b").join("vault.sqlite3");
    let started = InviteJoin::redeem(&context, &wrong_pin_target, &b_dir, now_of(&a.relay))
        .expect("兑换本身不校验 pin（instance 锚在认证/控制链上）");
    drop(started.vault);
    approve_pending_device(
        &local_context(&a.relay),
        &a.created.vault,
        &a.created.identity,
        started.pending.device_id,
        now_of(&a.relay),
    )
    .expect("A 批准");
    let vault = Vault::open(&b_dir).expect("重开库");
    let error = InviteJoin::check(&context, &vault, now_of(&a.relay)).err();
    assert!(
        matches!(error, Some(SyncError::InstanceMismatch)),
        "pin 不符必须 InstanceMismatch：{error:?}"
    );
    // pending 仍在，pin 没有被改写（不自动换 pin）。
    let pending = InviteJoin::load(&vault)
        .expect("读 pending")
        .expect("pending 仍在");
    assert_eq!(pending.pin, [0xEE; 16]);
    assert_eq!(
        pending.phase,
        InviteJoinPhase::Redeemed,
        "认证失败不得推进阶段"
    );
}

// ---------------------------------------------------------------------------
// 4) 非管理设备不能批准 / 不能生成邀请
// ---------------------------------------------------------------------------

#[test]
fn non_management_device_cannot_invite_or_approve() {
    let a = creator("nomanage");
    let link = invite_link(&a);
    let target = InviteLinkTarget::parse(&link.url).expect("解析失败");
    let context = local_context(&a.relay);
    let b_dir = b_path(&a);
    let started = InviteJoin::redeem(&context, &target, &b_dir, now_of(&a.relay)).expect("兑换");
    let b_device = started.pending.device_id;
    drop(started.vault);

    // 一台没有管理密钥的本地身份（普通设备/伪造者）：两个入口都必须明确拒绝。
    let bystander = LocalIdentityV1::generate_dev(a.created.vault_id, [0x77; 16]).expect("dev");
    let error = approve_pending_device(
        &context,
        &a.created.vault,
        &bystander,
        b_device,
        now_of(&a.relay),
    )
    .err();
    assert!(
        matches!(error, Some(SyncError::Unsupported(_))),
        "非管理设备批准必须 Unsupported：{error:?}"
    );
    let error = create_invite_link(&context, &a.created.vault, &bystander).err();
    assert!(
        matches!(error, Some(SyncError::Unsupported(_))),
        "非管理设备生成邀请必须 Unsupported：{error:?}"
    );
    // B 仍停在 pending（bystander 的失败没有副作用）。
    let vault = Vault::open(&b_dir).expect("重开库");
    assert_eq!(
        InviteJoin::check(&context, &vault, now_of(&a.relay)).expect("检查"),
        InviteCheck::WaitingForApproval
    );
}

// ---------------------------------------------------------------------------
// 5) ADR 0014 S3b：v2 邀请链接（api=2）在 v2 数据面上兑换/批准/bootstrap
// ---------------------------------------------------------------------------

/// 票据创建出来的 v2 vault（库级夹具；身份只需在内存里）。
struct V2Creator {
    relay: DevRelay,
    dir: TempDir,
    vault_id: [u8; 16],
    vault: Vault,
    identity: LocalIdentityV1,
}

fn v2_context(relay: &DevRelay) -> RelayContext {
    local_context(relay).with_api_version(RelayApiVersion::V2)
}

fn v2_creator(prefix: &str) -> V2Creator {
    init_identity_wrap();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);
    let vault_id = [0x9Au8; 16];
    let path = dir.path().join("a").join("vault.sqlite3");
    let raw = relay
        .issue_create_ticket(vault_id, std::time::Duration::from_secs(900))
        .expect("发行票据");
    let ticket = CreateTicket::from_slice(&raw).expect("32B");
    let created = create_vault_with_ticket(
        v2_context(&relay),
        vault_id,
        &ticket,
        None,
        &path,
        "A v2 实例",
        |_identity| Ok(()),
    )
    .expect("票据创建");
    let _ = create_note(
        &created.vault,
        created.identity.loro_peer_id(),
        created.identity.current_epoch,
        "A 的 v2 笔记",
        "A 写的第一行\n",
    );
    DevLocal::publish_initial_pair_with_identity(
        &v2_context(&relay),
        &created.vault,
        &created.identity,
    )
    .expect("A 发布基线");
    V2Creator {
        relay,
        dir,
        vault_id,
        vault: created.vault,
        identity: created.identity,
    }
}

#[test]
fn v2_invite_link_joins_through_v2_plane_and_rejects_api_mismatch() {
    let a = v2_creator("v2");
    let context = v2_context(&a.relay);
    let link = create_invite_link(&context, &a.vault, &a.identity).expect("生成邀请");
    assert!(
        link.url.contains("&api=2#token="),
        "v2 vault 的邀请链接必须带 api=2：{}",
        link.url
    );
    let target = InviteLinkTarget::parse(&link.url).expect("解析");
    assert_eq!(target.api_version(), RelayApiVersion::V2);

    // 面不一致（v1 context）必须在联网之前拒绝。
    let v1_context = local_context(&a.relay);
    let b_path = a.dir.path().join("b").join("vault.sqlite3");
    let error = InviteJoin::redeem(&v1_context, &target, &b_path, now_of(&a.relay)).err();
    assert!(
        matches!(error, Some(SyncError::ConfigInvalid(_))),
        "v1 context 兑换 v2 邀请必须 ConfigInvalid：{error:?}"
    );
    assert!(!b_path.exists(), "面不一致不得留下库文件");

    // v2 context：兑换 PENDING，pending 里持久 api=2。
    let started =
        InviteJoin::redeem(&context, &target, &b_path, now_of(&a.relay)).expect("v2 兑换");
    assert_eq!(started.pending.api_version, RelayApiVersion::V2);
    assert_eq!(
        InviteJoin::check(&context, &started.vault, now_of(&a.relay)).expect("检查"),
        InviteCheck::WaitingForApproval
    );
    drop(started.vault);
    // 重开库：pending 的 API 面也必须从 durable 记录恢复。
    let vault = Vault::open(&b_path).expect("重开 B 库");
    let pending = InviteJoin::load(&vault)
        .expect("读 pending")
        .expect("pending");
    assert_eq!(pending.api_version, RelayApiVersion::V2);
    // v1 context 检查必须拒绝（不能把 v2 pending 降级到 v1）。
    assert!(matches!(
        InviteJoin::check(&v1_context, &vault, now_of(&a.relay)),
        Err(SyncError::ConfigInvalid(_))
    ));

    // A 批准 → B bootstrap（v2 面）。
    let approved = approve_pending_device(
        &context,
        &a.vault,
        &a.identity,
        pending.device_id,
        now_of(&a.relay),
    )
    .expect("A 批准");
    assert!(approved.changed);
    let joined = match InviteJoin::finish(&context, &vault, now_of(&a.relay)).expect("finish") {
        InviteFinish::Joined(joined) => joined,
        InviteFinish::WaitingForApproval => panic!("已批准但 finish 仍报等待"),
    };
    assert!(
        joined.identity.manager_seed.is_none(),
        "邀请加入的设备没有管理私钥"
    );
    assert_eq!(joined.identity.vault_id, a.vault_id);
    assert_eq!(joined.pending.api_version, RelayApiVersion::V2);
    // 普通 Join 设备不能生成邀请（与 v1 同一门禁）。
    let error = create_invite_link(&context, &vault, &joined.identity).err();
    assert!(
        matches!(error, Some(SyncError::Unsupported(_))),
        "{error:?}"
    );

    // 未知 api 值明确拒绝，且错误文案不泄露 token。
    let token = link
        .url
        .split_once("#token=")
        .map(|(_, token)| token.to_owned())
        .expect("token");
    for bad in ["3", "v3"] {
        let broken = link.url.replace("&api=2#", &format!("&api={bad}#"));
        let error = InviteLinkTarget::parse(&broken).expect_err("未知 api 必须拒绝");
        assert!(!format!("{error}").contains(&token), "{error}");
    }
}
