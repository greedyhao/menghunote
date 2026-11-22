//! ADR 0014 **S1**：邀请链接 onboarding（粘贴 → PENDING → 批准 → bootstrap → Main）。
//!
//! 覆盖：
//! 1. A（真实 Create + 笔记 + 发布基线）生成邀请链接 → B 在 Join 表单粘贴 → PENDING
//!    （不写配置/身份）→ 「重启」（新 `AppRoot`）粘贴同一链接**不重复兑换**（resume）→
//!    A 批准 → B「检查状态」→ Main（配置 active 指向 B、A 的笔记可见）→ 再重启直接 Main；
//! 2. 恶意/非法链接：内联错误、不联网、不建目录；
//! 3. 已用/过期链接：明确提示（INVITE_USED/EXPIRED），不建库、回表单可重试。
//!
//! ⚠️ **非生产**：dev-relay 的 hook 放行验签；本文件验证向导/库级邀请流程，不是鉴权验收。
//! 纪律：临时目录在 `CARGO_TARGET_TMPDIR`；**不 sleep**，收敛点是
//! `wait_for_invite_terminal`/`wait_for_working_terminal`（阻塞在 worker channel，有超时）。
//!
//! feature 关闭时整个文件编译为空。

#![cfg(feature = "dev-sync")]

use std::path::Path;
use std::time::{Duration, Instant};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
use menghunote_app::onboarding::{
    AppRoot, AppRootOptions, ProfileLayout, ProfileStore, RootStateLabel, CREATE_PUMP_INTERVAL,
    INVITE_AUTO_CHECK_INTERVAL,
};
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, list_tree, load_body_doc, local_sync_intent, StructureWriter,
    SyncContext, UpdateRecord, Vault,
};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::invite_link::{approve_pending_device, create_invite_link, InviteLinkTarget};
use menghunote_sync::relay_context::RelayContext;
use tempfile::{Builder as TempDirBuilder, TempDir};

const DEADLINE: Duration = Duration::from_secs(60);

fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("onboarding-invite-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

fn temp_layout(prefix: &str) -> (TempDir, ProfileLayout) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("onboarding-invite-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败");
    let layout = ProfileLayout::new(
        dir.path().join("config/dev-sync.json"),
        dir.path().join("data/instances"),
    );
    (dir, layout)
}

fn open_root(
    cx: &mut TestAppContext,
    options: AppRootOptions,
) -> (Entity<AppRoot>, &mut VisualTestContext) {
    cx.add_window_view(move |window, cx| {
        let root = AppRoot::new(options, cx);
        AppRoot::install_close_handler(window, cx.entity(), cx);
        root
    })
}

fn state_of(root: &Entity<AppRoot>, cx: &mut VisualTestContext) -> RootStateLabel {
    root.update(cx, |root, _| root.state_label())
}

/// A：真实 Create + 一篇笔记 + 发布基线 + 生成邀请链接。
struct Creator {
    relay: DevRelay,
    /// 临时根目录（持有到测试结束，删除磁盘上的 A 实例）。
    _dir: TempDir,
    vault: Vault,
    identity: menghunote_sync::identity::LocalIdentityV1,
    vault_id: [u8; 16],
    note_text: String,
}

fn creator(prefix: &str) -> Creator {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root(prefix);
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let created = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A 实例")
        .expect("A create 失败");
    let peer = created.identity.loro_peer_id();
    let epoch = created.identity.current_epoch;
    // 真实笔记（结构 + 正文 + intent），随发布基线进入 checkpoint。
    let ctx = SyncContext::new(peer, epoch);
    let mut writer =
        StructureWriter::open_with_sync(&created.vault, peer, Some(ctx)).expect("结构写入口");
    let seed = writer
        .doc()
        .create_note("邀请加入的笔记", 1)
        .expect("新建笔记");
    writer.commit("onboarding-invite-note").expect("结构提交");
    {
        let (doc, head) =
            load_body_doc(&created.vault, &seed.body_doc_id, peer, None).expect("正文");
        let text = "A 写的第一行\n";
        doc.insert_scalars(0, text).expect("写正文");
        let base = head.version_vector().expect("vv");
        let update = doc.pending_update(&base).expect("增量");
        let seq = head.head_seq.saturating_add(1);
        let intent = local_sync_intent(
            &seed.body_doc_id,
            seq,
            "invite-body",
            SyncContext::new(peer, epoch),
        )
        .expect("intent");
        append_updates_with_sync_intents(
            &created.vault,
            &seed.body_doc_id,
            head.head_seq,
            &[UpdateRecord::new(intent.batch_id.clone(), update)],
            &[intent],
        )
        .expect("正文提交");
    }
    DevLocal::publish_initial_pair(
        relay.base_url(),
        &created.vault,
        &created.identity,
        &created.recovery_code,
    )
    .expect("A 发布基线");
    Creator {
        relay,
        _dir: dir,
        vault_id: created.vault_id,
        vault: created.vault,
        identity: created.identity,
        note_text: "A 写的第一行".to_owned(),
    }
}

fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "onboarding-invite-creator-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

fn invite_url(creator: &Creator) -> String {
    let context = RelayContext::local_dev(creator.relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("context");
    create_invite_link(&context, &creator.vault, &creator.identity)
        .expect("生成邀请")
        .url
}

fn relay_context(creator: &Creator) -> RelayContext {
    RelayContext::local_dev(creator.relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS).expect("context")
}

fn pending_member_count(creator: &Creator) -> usize {
    DevLocal::members(&relay_context(creator), &creator.vault, &creator.identity)
        .expect("成员列表")
        .len()
}

fn config_active(store: &ProfileStore) -> Option<String> {
    let profile = store.load().expect("读配置")?;
    profile.active().expect("active").cloned().map(|entry| {
        format!(
            "{}|{}|{}",
            entry.instance_id, entry.vault_id, entry.vault_path
        )
    })
}

// ---------------------------------------------------------------------------
// 1) 粘贴链接 → PENDING → 重启 resume → 批准 → Main
// ---------------------------------------------------------------------------

#[gpui::test]
async fn invite_pending_resumes_then_main_after_approval(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let creator = creator("flow");
    let link = invite_url(&creator);
    let (dir_root, layout) = temp_layout("flow");
    let b_dir = layout.instances_dir.join("inst-b");
    let store = layout.store();

    // ---- 第一次：粘贴邀请链接 → PENDING（不写配置/身份）----
    let (root1, cx1) = open_root(
        cx,
        AppRootOptions::for_test(layout.clone(), creator.relay.base_url(), "inst-b"),
    );
    root1.update(cx1, |root, cx| {
        root.enter_join_form(cx);
        root.join_form_set_dir(&b_dir.display().to_string(), cx);
        root.join_form_set_invite(&link, cx);
        root.submit_join(cx);
    });
    assert!(
        root1.update(cx1, |root, cx| root.wait_for_invite_terminal(cx, DEADLINE)),
        "邀请兑换必须在 60s 内收敛"
    );
    assert_eq!(state_of(&root1, cx1), RootStateLabel::InvitePending);
    let facts = root1
        .update(cx1, |root, _| root.invite_pending_facts().cloned())
        .expect("PENDING 公开事实");
    assert_eq!(
        facts.vault_id,
        menghunote_sync::ids::hex_of(&creator.vault_id)
    );
    assert!(!facts.resumed, "首次兑换不是 resume");
    assert_eq!(facts.fingerprint.len(), 32);
    assert_eq!(facts.sign_short.len(), 16);
    assert_eq!(facts.hpke_short.len(), 16);
    assert_eq!(facts.device_id.len(), 32, "设备 id 是 32 位 hex（公开）");
    assert!(
        config_active(&store).is_none(),
        "PENDING 期间不得写公开配置"
    );
    assert!(
        !menghunote_app::dev_sync::DevIdentityStore::path_for(&b_dir.join("vault.sqlite3"))
            .exists(),
        "PENDING 期间不得写身份文件"
    );
    let pending_device = facts.device_id;
    let before_restart = pending_member_count(&creator);
    assert_eq!(before_restart, 2, "relay 上应只有 A + 一个 pending 设备");

    // ---- 「重启」：新 AppRoot（同一配置目录/实例目录）粘贴同一链接 ----
    // 先让第一个 root 真正放手（等价于关窗释放实例锁）；durable pending 留在库里。
    root1.update(cx1, |root, cx| root.back_from_invite(cx));
    assert_eq!(state_of(&root1, cx1), RootStateLabel::Welcome);
    drop(link.clone());
    let (root2, cx2) = open_root(
        cx,
        AppRootOptions::for_test(layout.clone(), creator.relay.base_url(), "inst-b"),
    );
    root2.update(cx2, |root, cx| {
        root.enter_join_form(cx);
        root.join_form_set_dir(&b_dir.display().to_string(), cx);
        root.join_form_set_invite(&link, cx);
        root.submit_join(cx);
    });
    assert!(
        root2.update(cx2, |root, cx| root.wait_for_invite_terminal(cx, DEADLINE)),
        "resume 必须在 60s 内收敛"
    );
    assert_eq!(state_of(&root2, cx2), RootStateLabel::InvitePending);
    let resumed = root2
        .update(cx2, |root, _| root.invite_pending_facts().cloned())
        .expect("PENDING 事实");
    assert!(resumed.resumed, "重贴同一链接必须走 resume（不重复兑换）");
    assert_eq!(resumed.device_id, pending_device, "设备 id 必须不变");
    assert_eq!(
        pending_member_count(&creator),
        2,
        "resume 不得在 relay 上再造一个 pending 设备"
    );

    // ---- A 批准 → B「检查状态」→ bootstrap → Main ----
    let target_device: [u8; 16] = {
        let mut out = [0u8; 16];
        for (index, chunk) in resumed.device_id.as_bytes().chunks(2).enumerate() {
            let hex = |byte: u8| -> u8 {
                match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    _ => panic!("非法 hex"),
                }
            };
            out[index] = hex(chunk[0]) << 4 | hex(chunk[1]);
        }
        out
    };
    let approved = approve_pending_device(
        &relay_context(&creator),
        &creator.vault,
        &creator.identity,
        target_device,
        u64::try_from(creator.relay.now()).expect("now"),
    )
    .expect("A 批准");
    assert!(approved.changed);

    root2.update(cx2, |root, cx| root.check_invite(cx));
    assert!(
        root2.update(cx2, |root, cx| root.wait_for_invite_terminal(cx, DEADLINE)),
        "检查/ bootstrap 必须在 60s 内收敛"
    );
    assert_eq!(
        state_of(&root2, cx2),
        RootStateLabel::Main,
        "批准后必须进入 Main；若停在等待页说明 bootstrap/配置写回失败"
    );
    // 公开配置 active 指向 B，且库/身份都在。
    let active = config_active(&store).expect("配置必须写入 active");
    assert!(active.contains(&b_dir.display().to_string()), "{active}");
    assert!(active.contains(&menghunote_sync::ids::hex_of(&creator.vault_id)));
    assert!(
        menghunote_app::dev_sync::DevIdentityStore::path_for(&b_dir.join("vault.sqlite3")).exists(),
        "Main 之后身份文件必须已落盘"
    );
    // B 的库里有 A 的笔记正文（bootstrap 真的导入了基线）。
    let b_vault = Vault::open(b_dir.join("vault.sqlite3")).expect("B 库");
    let tree = list_tree(&b_vault).expect("B 结构树");
    let doc_id = tree
        .iter()
        .find(|entry| entry.title.as_str() == "邀请加入的笔记")
        .and_then(|entry| entry.body_doc_id.clone())
        .expect("B 必须看到 A 的笔记");
    let identity = menghunote_app::dev_sync::DevIdentityStore
        .load(&b_dir.join("vault.sqlite3"))
        .expect("B 身份文件可解封");
    assert!(
        identity.manager_seed.is_none(),
        "邀请加入的设备没有管理私钥"
    );
    let text = load_body_doc(&b_vault, &doc_id, identity.loro_peer_id(), None)
        .expect("B 正文")
        .0
        .text();
    assert!(text.contains(&creator.note_text), "B 必须看到 A 的正文");

    // ---- 再重启：配置 active + 身份都在 → 直接 Main（不再需要链接）----
    root2.update(cx2, |root, cx| {
        assert!(root.should_close(cx), "Main 关窗必须放行");
    });
    let (root3, cx3) = open_root(
        cx,
        AppRootOptions::for_test(layout.clone(), creator.relay.base_url(), "inst-b"),
    );
    assert_eq!(
        state_of(&root3, cx3),
        RootStateLabel::Main,
        "重启后必须自动打开已加入实例"
    );
    drop(dir_root);
}

// ---------------------------------------------------------------------------
// 2) 非法链接：内联错误、不建目录
// ---------------------------------------------------------------------------

#[gpui::test]
async fn malicious_or_broken_invite_links_are_rejected_locally(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let creator = creator("bad-links");
    let (dir_root, layout) = temp_layout("bad-links");
    let b_dir = layout.instances_dir.join("inst-b");
    let (root, cx) = open_root(
        cx,
        AppRootOptions::for_test(layout.clone(), creator.relay.base_url(), "inst-b"),
    );
    for (label, link) in [
        ("scheme", "https://join?v=1&relay=http%3A%2F%2F127.0.0.1%3A1&vault="),
        (
            "恶意跳转",
            "mn-pair://join?v=1&relay=http%3A%2F%2F127.0.0.1%3A1&next=https%3A%2F%2Fevil&vault=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&iid=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&pin=cccccccccccccccccccccccccccccccc#token=AAAA",
        ),
        (
            "token",
            "mn-pair://join?v=1&relay=http%3A%2F%2F127.0.0.1%3A1&vault=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&iid=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&pin=cccccccccccccccccccccccccccccccc#token=short",
        ),
    ] {
        root.update(cx, |root, cx| {
            root.enter_join_form(cx);
            root.join_form_set_dir(&b_dir.display().to_string(), cx);
            if !link.is_empty() {
                root.join_form_set_invite(link, cx);
            }
            root.submit_join(cx);
        });
        assert_eq!(
            state_of(&root, cx),
            RootStateLabel::JoinForm,
            "{label}：非法链接必须留在表单"
        );
        let error = root
            .update(cx, |root, _| root.join_form_error().map(str::to_owned))
            .expect("必须有内联错误");
        assert!(
            error.contains("邀请链接") || error.contains("邀请"),
            "{label}：错误必须点明邀请链接：{error}"
        );
        assert!(
            !b_dir.join("vault.sqlite3").exists(),
            "{label}：不得创建库"
        );
    }
    drop(dir_root);
}

// ---------------------------------------------------------------------------
// 3) 已用链接：明确 INVITE_USED，不建库、可重试
// ---------------------------------------------------------------------------

#[gpui::test]
async fn used_invite_link_reports_explicit_failure(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let creator = creator("used");
    let link = invite_url(&creator);
    let target = InviteLinkTarget::parse(&link).expect("解析");
    let (dir_root, layout) = temp_layout("used");
    // 先用库级入口消费掉 token（另一个目录），向导随后再兑换必须明确失败。
    let other_dir = layout.instances_dir.join("inst-x");
    menghunote_sync::invite_link::InviteJoin::redeem(
        &relay_context(&creator),
        &target,
        &other_dir.join("vault.sqlite3"),
        u64::try_from(creator.relay.now()).expect("now"),
    )
    .expect("首次兑换");
    let b_dir = layout.instances_dir.join("inst-b");
    let (root, cx) = open_root(
        cx,
        AppRootOptions::for_test(layout.clone(), creator.relay.base_url(), "inst-b"),
    );
    root.update(cx, |root, cx| {
        root.enter_join_form(cx);
        root.join_form_set_dir(&b_dir.display().to_string(), cx);
        root.join_form_set_invite(&link, cx);
        root.submit_join(cx);
    });
    assert!(
        root.update(cx, |root, cx| root.wait_for_invite_terminal(cx, DEADLINE)),
        "失败也必须在 60s 内收敛"
    );
    assert_eq!(state_of(&root, cx), RootStateLabel::JoinForm);
    let error = root
        .update(cx, |root, _| root.join_form_error().map(str::to_owned))
        .expect("必须有错误文案");
    assert!(
        error.contains("已被使用") || error.contains("INVITE_USED"),
        "已用 token 必须明确：{error}"
    );
    assert!(
        !b_dir.join("vault.sqlite3").exists(),
        "失败不得留下半初始化库"
    );
    assert!(
        !menghunote_app::dev_sync::DevIdentityStore::path_for(&b_dir.join("vault.sqlite3"))
            .exists(),
        "失败不得留下身份文件"
    );
    drop(dir_root);
}

// ---------------------------------------------------------------------------
// 4) PENDING 等待页自动刷新批准状态（不需要点「检查状态」）
// ---------------------------------------------------------------------------

/// 推进测试时钟（40ms 一拍，不 sleep），直到等待页累计自动发出 `expected` 次 CHECK。
fn advance_until_auto_checks(root: &Entity<AppRoot>, cx: &mut VisualTestContext, expected: u32) {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if root.update(cx, |root, _| root.invite_auto_check_count()) >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "等待页在 {INVITE_AUTO_CHECK_INTERVAL:?} 后仍未自动发出第 {expected} 次 CHECK"
        );
        cx.executor().advance_clock(CREATE_PUMP_INTERVAL);
        cx.run_until_parked();
    }
}

/// 用户反馈：邀请等待批准时状态不会自动更新，要手动点「检查状态」。
///
/// 本用例全程**不点**按钮：进入 PENDING 后推进测试时钟（不 sleep），等待页的泵应在
/// ~[`INVITE_AUTO_CHECK_INTERVAL`] 后自动发出一次 CHECK；创建端批准后自动 bootstrap
/// 并进入 Main。
#[gpui::test]
async fn pending_page_auto_checks_and_enters_main_without_manual_check(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let creator = creator("auto-check");
    let link = invite_url(&creator);
    let (_dir_root, layout) = temp_layout("auto-check");
    let b_dir = layout.instances_dir.join("inst-b");
    let store = layout.store();

    let (root, cx) = open_root(
        cx,
        AppRootOptions::for_test(layout.clone(), creator.relay.base_url(), "inst-b"),
    );
    root.update(cx, |root, cx| {
        root.enter_join_form(cx);
        root.join_form_set_dir(&b_dir.display().to_string(), cx);
        root.join_form_set_invite(&link, cx);
        root.submit_join(cx);
    });
    assert!(
        root.update(cx, |root, cx| root.wait_for_invite_terminal(cx, DEADLINE)),
        "邀请兑换必须在 60s 内收敛"
    );
    assert_eq!(state_of(&root, cx), RootStateLabel::InvitePending);
    let facts = root
        .update(cx, |root, _| root.invite_pending_facts().cloned())
        .expect("PENDING 公开事实");
    assert_eq!(
        root.update(cx, |root, _| root.invite_auto_check_count()),
        0,
        "刚进入等待页不应立刻发起自动检查（间隔 ~5s）"
    );

    // 第一步：还没批准时，自动 CHECK 必须发出；结果是 Waiting，页面保留并继续轮询。
    advance_until_auto_checks(&root, cx, 1);
    assert!(
        root.update(cx, |root, cx| root.wait_for_invite_terminal(cx, DEADLINE)),
        "未批准时的自动检查也必须在 60s 内收敛（Waiting）"
    );
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::InvitePending,
        "未批准不得离开等待页（下次自动检查继续）"
    );
    let waiting = root
        .update(cx, |root, _| root.invite_message().map(str::to_owned))
        .expect("Waiting 必须给出状态文案");
    assert!(
        waiting.contains("尚未批准") || waiting.contains("未批准"),
        "未批准文案必须明确：{waiting}"
    );
    assert!(
        !root.update(cx, |root, _| root.invite_busy()),
        "Waiting 后必须允许下一次自动检查"
    );

    // 创建端批准（不碰 B 的等待页/按钮）。
    let target_device =
        menghunote_sync::ids::parse_hex::<16>(&facts.device_id).expect("设备 id 必须是 32 位 hex");
    let approved = approve_pending_device(
        &relay_context(&creator),
        &creator.vault,
        &creator.identity,
        target_device,
        u64::try_from(creator.relay.now()).expect("now"),
    )
    .expect("A 批准");
    assert!(approved.changed);

    // 第二步：再推进 ~5s，自动 CHECK 看到已批准 → bootstrap 并进 Main。
    advance_until_auto_checks(&root, cx, 2);

    // 自动 CHECK 结果：bootstrap + 写配置 + 进 Main（没有手动点按钮）。
    assert!(
        root.update(cx, |root, cx| root.wait_for_invite_terminal(cx, DEADLINE)),
        "自动检查必须在 60s 内收敛"
    );
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "批准后等待页必须自动进入 Main"
    );
    assert!(
        config_active(&store).is_some(),
        "自动进入 Main 也必须写入公开配置 active"
    );
    drop(_dir_root);
}

// ---------------------------------------------------------------------------
// 5) 统一「同步远程仓库」：粘贴 mn-pair://join 链接 → PENDING（不再显示手动字段）
// ---------------------------------------------------------------------------

#[gpui::test]
async fn welcome_remote_form_join_link_enters_pending(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let creator = creator("welcome-join");
    let link = invite_url(&creator);
    let (_dir_root, layout) = temp_layout("welcome-join");
    let b_dir = layout.instances_dir.join("inst-b");
    let vault_path = b_dir.join("vault.sqlite3");
    let store = layout.store();

    let (root, cx) = open_root(
        cx,
        AppRootOptions::for_test(layout.clone(), creator.relay.base_url(), "inst-b"),
    );
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);

    // 展开统一同步卡：只有一个链接字段 + 实例目录（+ dev 同意），没有手动 relay/
    // vault_id/恢复码/邀请链接多字段。
    let Some(card) = cx.debug_bounds("welcome-sync-remote") else {
        panic!("Welcome 必须有「同步远程仓库」卡");
    };
    cx.simulate_click(card.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("welcome-sync-link").is_some());
    assert!(cx.debug_bounds("welcome-sync-dir").is_some());
    assert!(cx.debug_bounds("welcome-sync-confirm").is_some());
    assert!(cx.debug_bounds("welcome-sync-consent").is_some());
    for absent in [
        "app-root-join-form",
        "app-root-join-relay-input",
        "app-root-vault-id-input",
        "app-root-recovery-input",
        "app-root-invite-input",
    ] {
        assert!(
            cx.debug_bounds(absent).is_none(),
            "统一表单不得显示手动字段：{absent}"
        );
    }

    root.update(cx, |root, cx| {
        root.welcome_set_sync_link(&link, cx);
        root.welcome_set_sync_dir(&b_dir.display().to_string(), cx);
        root.welcome_set_sync_consent(true, cx);
        root.welcome_confirm_sync(cx);
    });
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Working,
        "合法 join 链接必须直接进入后台邀请兑换（不是 JoinForm）"
    );
    assert!(
        root.update(cx, |root, cx| root.wait_for_invite_terminal(cx, DEADLINE)),
        "邀请兑换必须在 60s 内收敛"
    );
    assert_eq!(state_of(&root, cx), RootStateLabel::InvitePending);
    let facts = root
        .update(cx, |root, _| root.invite_pending_facts().cloned())
        .expect("PENDING 公开事实");
    assert_eq!(
        facts.vault_id,
        menghunote_sync::ids::hex_of(&creator.vault_id)
    );
    assert!(
        config_active(&store).is_none(),
        "PENDING 期间不得写公开配置"
    );
    assert!(
        !menghunote_app::dev_sync::DevIdentityStore::path_for(&vault_path).exists(),
        "PENDING 期间不得写身份文件"
    );
}

// ---------------------------------------------------------------------------
// 6) 统一表单「仓库名称」：邀请加入（join）成功后写入 display_name
// ---------------------------------------------------------------------------

/// 粘贴 `mn-pair://join` + 填「仓库名称」→ PENDING（不写配置）→ 创建端批准 →
/// 检查/bootstrap → Main：`display_name` 落盘（trim），三处显示同名，目标路径不变。
#[gpui::test]
async fn welcome_join_link_with_display_name_writes_it_on_bootstrap(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let creator = creator("welcome-name");
    let link = invite_url(&creator);
    let (_dir_root, layout) = temp_layout("welcome-name");
    let b_dir = layout.instances_dir.join("inst-b");
    let store = layout.store();

    let (root, cx) = open_root(
        cx,
        AppRootOptions::for_test(layout.clone(), creator.relay.base_url(), "inst-b"),
    );
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);

    // 展开统一同步卡：链接 / 实例目录 / 可选的「仓库名称」。
    let card = cx
        .debug_bounds("welcome-sync-remote")
        .expect("Welcome 必须有「同步远程仓库」卡");
    cx.simulate_click(card.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("welcome-sync-link").is_some());
    assert!(cx.debug_bounds("welcome-sync-dir").is_some());
    assert!(
        cx.debug_bounds("welcome-sync-name").is_some(),
        "统一表单必须有可选的「仓库名称」字段"
    );

    root.update(cx, |root, cx| {
        root.welcome_set_sync_link(&link, cx);
        root.welcome_set_sync_dir(&b_dir.display().to_string(), cx);
        root.welcome_set_sync_name("  邀请 来的库  ", cx);
        root.welcome_set_sync_consent(true, cx);
        root.welcome_confirm_sync(cx);
    });
    assert!(
        root.update(cx, |root, cx| root.wait_for_invite_terminal(cx, DEADLINE)),
        "邀请兑换必须在 60s 内收敛"
    );
    assert_eq!(state_of(&root, cx), RootStateLabel::InvitePending);
    assert!(
        config_active(&store).is_none(),
        "PENDING 期间不得写公开配置（名称也不落盘）"
    );

    // 创建端批准 → B「检查状态」→ bootstrap → Main。
    let facts = root
        .update(cx, |root, _| root.invite_pending_facts().cloned())
        .expect("PENDING 公开事实");
    let target_device =
        menghunote_sync::ids::parse_hex::<16>(&facts.device_id).expect("设备 id 必须是 32 位 hex");
    approve_pending_device(
        &relay_context(&creator),
        &creator.vault,
        &creator.identity,
        target_device,
        u64::try_from(creator.relay.now()).expect("now"),
    )
    .expect("A 批准");
    root.update(cx, |root, cx| root.check_invite(cx));
    assert!(
        root.update(cx, |root, cx| root.wait_for_invite_terminal(cx, DEADLINE)),
        "检查/bootstrap 必须在 60s 内收敛"
    );
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "批准后必须进入 Main（配置写回失败会停在等待页）"
    );

    let profile = store.load().expect("读配置").expect("配置存在");
    let entry = profile.find("inst-b").expect("B 已登记");
    assert_eq!(
        entry.display_name.as_deref(),
        Some("邀请 来的库"),
        "邀请加入成功后必须写 display_name（trim）"
    );
    assert_eq!(
        std::path::PathBuf::from(&entry.vault_path),
        b_dir.join("vault.sqlite3"),
        "名称只影响显示：目标路径不变"
    );
    assert_eq!(profile.active_instance.as_deref(), Some("inst-b"));

    // 左下角 + 切换菜单同一名字。
    assert_eq!(
        root.update(cx, |root, cx| root.current_vault_name(cx)),
        "邀请 来的库"
    );
    let switcher = cx
        .debug_bounds("app-root-vault-switcher")
        .expect("Main 左下角必须有切换器");
    cx.simulate_click(switcher.center(), Modifiers::default());
    cx.run_until_parked();
    let rows = root.update(cx, |root, cx| root.vault_menu_rows(cx));
    assert_eq!(rows[0].label, "✓ 邀请 来的库");
    drop(_dir_root);
}
