//! ADR 0014 **S3b**：创建链接（`mn-pair://create` + 一次性票据）/ 多实例 / v2 邀请加入。
//!
//! 真实 dev relay（进程内 axum + 真实 HTTP；**放行验签 hook**，非生产）：
//!
//! 1. 同一 relay 上两张票据各建一个 vault（同一 `AppRoot` 连续创建）：两个 profile、
//!    `api_version=2`、身份/pin 独立、目录独立；active 指向新实例；两库各自建笔记/
//!    发布基线，互不可见；重启分别进 Main；Welcome 多实例列表可分别打开。
//! 2. v2 邀请链接：A（票据创建的 v2 vault）生成带 `api=2` 的链接 → B 粘贴 → PENDING
//!    （不写配置/身份）→ A 批准 → B「检查状态」→ Main（配置 `api_version=2`）→ B 用
//!    `/v2` 数据面同步（拉取 A 的新笔记）；v1 链接（`api` 缺省）仍由既有
//!    `onboarding_invite.rs` 覆盖。
//! 3. 失败矩阵：未知票据 → 403，错 vault → 403（且不消费票据），过期 → 403，
//!    已用 → 403，同 vault 重复（vault 已存在 + 新票据）→ 409，超过 v2 创建限流 → 429；
//!    全部明确分类、不建库/身份/配置。
//! 4. 票据不出现在配置/实例目录/错误文案里；链接输入框在应用后立刻清空。
//!
//! 纪律：临时目录一律 `CARGO_TARGET_TMPDIR`；**不 sleep**，收敛点是
//! `wait_for_create_terminal`/`wait_for_invite_terminal`（阻塞在 worker channel，有超时）。

#![cfg(feature = "dev-sync")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{Entity, Modifiers, TestAppContext, VisualTestContext};
use menghunote_app::dev_sync::DevIdentityStore;
use menghunote_app::onboarding::{
    AppRoot, AppRootOptions, ProfileLayout, ProfileStore, RelayApiVersion, RelayMode,
    RootStateLabel, DEFAULT_OPERATION_BUDGET,
};
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, list_tree, load_body_doc, local_sync_intent, StructureWriter,
    SyncContext, UpdateRecord, Vault,
};
use menghunote_sync::create_link::{CreateLink, CreateTicket};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::invite_link::{approve_pending_device, create_invite_link, InviteLinkTarget};
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::remote_init::create_vault_with_ticket;
use tempfile::{Builder as TempDirBuilder, TempDir};

fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("onboarding-create-link-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

fn temp_layout(prefix: &str) -> (TempDir, ProfileLayout) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "onboarding-create-link-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败");
    let layout = ProfileLayout::new(
        dir.path().join("config/sync.json"),
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

fn options_for(layout: &ProfileLayout, relay: &DevRelay) -> AppRootOptions {
    AppRootOptions {
        layout: layout.clone(),
        default_relay: relay.base_url().to_owned(),
        instance_id: None,
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    }
}

fn expect_bounds(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
    if cx.debug_bounds(selector).is_none() {
        // 同步面板现在收在统一设置页里：Main 场景下先点「设置」，再断言面板内 selector。
        if let Some(gear) = cx.debug_bounds("app-root-settings") {
            cx.simulate_click(gear.center(), Modifiers::default());
            cx.run_until_parked();
        }
        // 设置页是「左侧分类导航 + 右侧只渲染当前分类」：按 selector 前缀切到对应分类。
        let section = match selector {
            // 公开信息（API 面）已随「实例」分类移除，挪到「同步」分类底部。
            "settings-api-version" | "settings-public-info" => Some("sync"),
            "settings-fingerprint" => Some("security"),
            "settings-relay" => Some("relay"),
            _ => None,
        };
        if let Some(section) = section {
            let nav: &'static str = Box::leak(format!("settings-nav-{section}").into_boxed_str());
            if let Some(bounds) = cx.debug_bounds(nav) {
                cx.simulate_click(bounds.center(), Modifiers::default());
                cx.run_until_parked();
            }
        }
        if selector.starts_with("dev-sync-") {
            // 同步面板在「同步」分类里；进入分类就直接渲染（没有展开/收起按钮）。
            if let Some(bounds) = cx.debug_bounds("settings-nav-sync") {
                cx.simulate_click(bounds.center(), Modifiers::default());
                cx.run_until_parked();
            }
        }
    }
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("必须画出 selector={selector}"));
    assert!(
        bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0,
        "{selector} 必须有非零尺寸：{bounds:?}"
    );
    bounds
}

fn hex16(bytes: &[u8; 16]) -> String {
    menghunote_sync::ids::hex_of(bytes)
}

fn decode_hex16(text: &str) -> [u8; 16] {
    assert_eq!(text.len(), 32, "32 位 hex：{text}");
    let mut out = [0u8; 16];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let value = |byte: u8| -> u8 {
            match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => panic!("非法 hex：{text}"),
            }
        };
        out[index] = value(chunk[0]) << 4 | value(chunk[1]);
    }
    out
}

fn link_from_ticket(relay: &DevRelay, vault_id: [u8; 16], raw: &[u8; 32]) -> String {
    let ticket = CreateTicket::from_slice(raw).expect("票据必须是 32B");
    CreateLink::encode(
        RelayMode::LocalDev,
        relay.base_url(),
        vault_id,
        ticket,
        None,
    )
    .expect("编码创建链接失败")
    .url
}

/// 为全新 vault 预留并发行票据，再编码成创建链接（ADR 0018 S4b：App 只消费链接）。
fn issue_link(relay: &DevRelay, vault_id: [u8; 16], ttl_secs: u64) -> String {
    let raw = relay
        .issue_create_ticket(vault_id, Duration::from_secs(ttl_secs))
        .expect("发行创建票据失败");
    link_from_ticket(relay, vault_id, &raw)
}

fn v2_context(relay: &DevRelay) -> RelayContext {
    RelayContext::local_dev(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("local context")
        .with_api_version(RelayApiVersion::V2)
}

/// 库级票据创建（测试夹具：A 的 v2 vault；身份受保护落盘）。
fn ticket_vault(relay: &DevRelay, path: &Path, vault_id: [u8; 16], label: &str) -> Vault {
    let raw = relay
        .issue_create_ticket(vault_id, Duration::from_secs(900))
        .expect("发行创建票据失败");
    let ticket = CreateTicket::from_slice(&raw).expect("票据必须是 32B");
    let store = DevIdentityStore;
    let created = create_vault_with_ticket(
        v2_context(relay),
        vault_id,
        &ticket,
        None,
        path,
        label,
        |identity| {
            store
                .persist_protected(path, identity)
                .map_err(|error| menghunote_sync::SyncError::Io(format!("夹具身份落盘：{error:#}")))
        },
    )
    .expect("票据创建失败");
    created.vault
}

/// 建一篇笔记（结构 + 正文 + intent）。
fn seed_note(vault: &Vault, peer: u64, epoch: u64, title: &str, text: &str) -> String {
    let ctx = SyncContext::new(peer, epoch);
    let mut writer = StructureWriter::open_with_sync(vault, peer, Some(ctx)).expect("结构写入口");
    let seed = writer.doc().create_note(title, 1).expect("新建笔记失败");
    writer.commit("create-link-note").expect("结构提交");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, peer, None).expect("正文");
    doc.insert_scalars(0, text).expect("写正文");
    let base = head.version_vector().expect("vv");
    let update = doc.pending_update(&base).expect("增量");
    let seq = head.head_seq.saturating_add(1);
    let intent = local_sync_intent(
        &seed.body_doc_id,
        seq,
        "create-link-body",
        SyncContext::new(peer, epoch),
    )
    .expect("intent");
    append_updates_with_sync_intents(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new(intent.batch_id.clone(), update)],
        &[intent],
    )
    .expect("正文提交");
    seed.body_doc_id
}

fn titles(vault: &Vault) -> Vec<String> {
    list_tree(vault)
        .expect("结构树")
        .into_iter()
        .map(|entry| entry.title)
        .collect()
}

fn profile_entries(store: &ProfileStore) -> Vec<(String, String, RelayApiVersion, String)> {
    store
        .load()
        .expect("读配置")
        .map(|profile| {
            profile
                .instances
                .into_iter()
                .map(|entry| {
                    (
                        entry.instance_id,
                        entry.vault_id,
                        entry.api_version,
                        entry.vault_path,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn active_instance(store: &ProfileStore) -> Option<String> {
    store
        .load()
        .expect("读配置")
        .and_then(|profile| profile.active_instance)
}

fn set_active_instance(store: &ProfileStore, instance_id: Option<&str>) {
    let mut profile = store.load().expect("读配置").expect("配置存在");
    profile.active_instance = instance_id.map(str::to_owned);
    store.save(&profile).expect("写配置");
}

/// 进入 Create 表单、粘贴并应用创建链接。
fn apply_create_link(root: &Entity<AppRoot>, cx: &mut VisualTestContext, link: &str) {
    root.update(cx, |root, cx| {
        root.enter_create_link_form(cx);
        root.form_set_create_link(link, cx);
        root.form_apply_create_link(cx);
    });
    cx.run_until_parked();
}

/// 用创建链接建一个实例并走到 Main；返回恢复码（备份页捕获）。
fn create_via_link(
    root: &Entity<AppRoot>,
    cx: &mut VisualTestContext,
    link: &str,
    dir: &Path,
) -> String {
    apply_create_link(root, cx, link);
    assert_eq!(state_of(root, cx), RootStateLabel::CreateForm);
    assert!(
        root.update(cx, |root, _| root.form_has_create_link()),
        "应用链接后必须进入票据模式"
    );
    root.update(cx, |root, cx| {
        root.form_set_dir(&dir.display().to_string(), cx);
        root.form_set_consent(true, cx);
        root.submit_create(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(root, cx), RootStateLabel::Working);
    assert!(
        root.update(cx, |root, cx| root
            .wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "票据 Create 必须在预算内收敛"
    );
    cx.run_until_parked();
    assert_eq!(
        state_of(root, cx),
        RootStateLabel::BackupRequired,
        "票据 Create 成功后必须停在备份页（与 v1 同一条路径）"
    );
    let code = root
        .update(cx, |root, _| root.recovery_code().map(str::to_owned))
        .expect("备份页必须有恢复码");
    root.update(cx, |root, cx| {
        root.set_backup_confirmed(true, cx);
        root.confirm_backup(cx);
    });
    cx.run_until_parked();
    assert_eq!(
        state_of(root, cx),
        RootStateLabel::Main,
        "确认备份后进 Main"
    );
    code
}

/// 用创建链接失败的一次尝试（断言错误页 + 没有落盘）。
fn failed_create_via_link(
    root: &Entity<AppRoot>,
    cx: &mut VisualTestContext,
    link: &str,
    dir: &Path,
) {
    apply_create_link(root, cx, link);
    assert!(
        root.update(cx, |root, _| root.form_has_create_link()),
        "本地解析必须通过（失败应来自 relay）"
    );
    root.update(cx, |root, cx| {
        root.form_set_dir(&dir.display().to_string(), cx);
        root.form_set_consent(true, cx);
        root.submit_create(cx);
    });
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, cx| root
            .wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "失败也必须在预算内收敛"
    );
    cx.run_until_parked();
    assert_eq!(
        state_of(root, cx),
        RootStateLabel::Error,
        "票据失败必须进 Error（可回表单重试），不能半进 Main"
    );
    assert!(!dir.join("vault.sqlite3").exists(), "失败不得创建库");
    assert!(
        !DevIdentityStore::path_for(&dir.join("vault.sqlite3")).exists(),
        "失败不得写身份"
    );
}

fn error_message(root: &Entity<AppRoot>, cx: &mut VisualTestContext) -> String {
    root.update(cx, |root, _| {
        root.error_message().map(str::to_owned).unwrap_or_default()
    })
}

// ---------------------------------------------------------------------------
// 1) 同一 relay 两个票据 vault：配置/身份/目录独立 + 笔记互不可见 + 重启进 Main
// ---------------------------------------------------------------------------

#[gpui::test]
async fn two_ticket_vaults_on_one_relay_are_isolated_and_restart_into_main(
    cx: &mut TestAppContext,
) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("multi");
    let store = layout.store();
    let vault_a = [0xA1u8; 16];
    let vault_b = [0xB2u8; 16];
    let link_a = issue_link(&relay, vault_a, 900);
    let link_b = issue_link(&relay, vault_b, 900);
    let dir_a = layout.instances_dir.join("inst-a");
    let dir_b = layout.instances_dir.join("inst-b");

    let (root, cx) = open_root(cx, options_for(&layout, &relay));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Welcome);
    // 欢迎页已改为 Obsidian 式仓库选择页：旧的「粘贴创建链接」入口换成
    // 「同步远程仓库」卡（同一条链接、统一的 create/join 识别）。
    expect_bounds(cx, "welcome-sync-remote");

    // ---- A：粘贴链接 → 票据模式（setup secret 隐藏）→ 真实创建 ----
    apply_create_link(&root, cx, &link_a);
    assert_eq!(state_of(&root, cx), RootStateLabel::CreateForm);
    for selector in [
        "app-root-create-link-input",
        "app-root-create-link-apply",
        "app-root-create-link-warning",
        "app-root-create-link-facts",
        "app-root-dir-input",
        "app-root-consent",
        "app-root-submit",
    ] {
        expect_bounds(cx, selector);
    }
    for selector in [
        "app-root-secret-input",
        "app-root-admin-secret",
        "app-root-admin-notice",
        "app-root-relay-input",
        "app-root-create-mode-admin",
        "app-root-create-mode-legacy",
    ] {
        assert!(
            cx.debug_bounds(selector).is_none(),
            "ADR 0018 S4b：App 不得再显示 {selector}"
        );
    }
    assert_eq!(
        root.update(cx, |root, cx| root.form_create_link_is_empty(cx)),
        Some(true),
        "应用链接后必须立刻清空链接输入框（票据只在内存）"
    );
    assert_eq!(
        root.update(cx, |root, _| root.form_mode()),
        Some(RelayMode::LocalDev),
        "http 创建链接必须固定 local_dev"
    );
    assert_eq!(
        root.update(cx, |root, _| root
            .form_create_link_vault_id()
            .map(str::to_owned)),
        Some(hex16(&vault_a)),
        "链接预填 vault id"
    );
    root.update(cx, |root, cx| {
        root.form_set_dir(&dir_a.display().to_string(), cx);
        root.form_set_consent(true, cx);
        root.submit_create(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Working);
    assert!(
        root.update(cx, |root, cx| root
            .wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "A 的票据 Create 必须收敛"
    );
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::BackupRequired);
    let code_a = root
        .update(cx, |root, _| root.recovery_code().map(str::to_owned))
        .expect("A 必须有恢复码");
    root.update(cx, |root, cx| {
        root.set_backup_confirmed(true, cx);
        root.confirm_backup(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert!(root.update(cx, |root, _| root.has_main_shell()));

    // 备份页 → Main 之后配置才写入：一条 v2 profile。
    let entries = profile_entries(&store);
    assert_eq!(entries.len(), 1, "A 必须先写入一份 profile");
    let (inst_a, entry_vault_a, api_a, path_a) = entries[0].clone();
    assert_eq!(entry_vault_a, hex16(&vault_a));
    assert_eq!(api_a, RelayApiVersion::V2, "票据创建必须写 api_version=2");
    assert_eq!(path_a, dir_a.join("vault.sqlite3").display().to_string());
    assert_eq!(active_instance(&store).as_deref(), Some(inst_a.as_str()));
    let identity_a = DevIdentityStore
        .load(&dir_a.join("vault.sqlite3"))
        .expect("A 身份");
    assert_eq!(identity_a.vault_id, vault_a);
    assert!(identity_a.manager_seed.is_some(), "票据 genesis 有管理钥");

    // ---- 同一 relay 创建第二个 vault（不覆盖已有 profile）----
    root.update(cx, |root, cx| {
        root.back_to_welcome(cx);
        assert!(root.should_close(cx));
    });
    let code_b = create_via_link(&root, cx, &link_b, &dir_b);
    let entries = profile_entries(&store);
    assert_eq!(
        entries.len(),
        2,
        "第二个 vault 必须追加，不覆盖 A 的 profile"
    );
    let (inst_b, entry_vault_b, api_b, path_b) = entries
        .iter()
        .find(|(_, vault, _, _)| vault == &hex16(&vault_b))
        .cloned()
        .expect("B 的 profile");
    assert_eq!(api_b, RelayApiVersion::V2);
    assert_eq!(path_b, dir_b.join("vault.sqlite3").display().to_string());
    assert_ne!(inst_a, inst_b, "两个实例 id 必须不同");
    assert_ne!(entry_vault_a, entry_vault_b, "两个 vault 必须不同");
    assert_eq!(
        active_instance(&store).as_deref(),
        Some(inst_b.as_str()),
        "active 必须指向新实例（重启语义）"
    );
    let listed = root.update(cx, |root, _| {
        root.entries()
            .into_iter()
            .map(|entry| entry.instance_id)
            .collect::<Vec<_>>()
    });
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0], inst_b, "active 必须排在第一行");
    let identity_b = DevIdentityStore
        .load(&dir_b.join("vault.sqlite3"))
        .expect("B 身份");
    assert_eq!(identity_b.vault_id, vault_b);
    assert_ne!(
        identity_a.device_id, identity_b.device_id,
        "两个实例的设备身份必须独立"
    );
    // 每个库各自 durable 提交 pin（同一 relay 同一 instance，但分别落在各自库里）。
    let pin_a = {
        let vault = Vault::open(dir_a.join("vault.sqlite3")).expect("A 库");
        menghunote_sync::control_pin::load_control_pin(&vault, vault_a).expect("A pin 读取")
    };
    let pin_b = {
        let vault = Vault::open(dir_b.join("vault.sqlite3")).expect("B 库");
        menghunote_sync::control_pin::load_control_pin(&vault, vault_b).expect("B pin 读取")
    };
    assert!(
        pin_a.is_some() && pin_b.is_some(),
        "两个库各自提交 durable pin"
    );
    assert_eq!(
        pin_a.expect("A pin").instance_id,
        pin_b.expect("B pin").instance_id,
        "同一 relay 的 instance 相同（pin 内容分别提交）"
    );

    // ---- 两库各自建笔记 + 发布基线；另一 vault 不可见 ----
    root.update(cx, |root, cx| {
        root.back_to_welcome(cx);
        assert!(root.should_close(cx));
    });
    let context = v2_context(&relay);
    for (vault_path, vault_id, note_title, note_text) in [
        (
            dir_a.join("vault.sqlite3"),
            vault_a,
            "A 独有笔记",
            "A 写的第一行\n",
        ),
        (
            dir_b.join("vault.sqlite3"),
            vault_b,
            "B 独有笔记",
            "B 写的第一行\n",
        ),
    ] {
        let vault = Vault::open(&vault_path).expect("库");
        let identity = DevIdentityStore.load(&vault_path).expect("身份");
        assert_eq!(identity.vault_id, vault_id);
        seed_note(
            &vault,
            identity.loro_peer_id(),
            identity.current_epoch,
            note_title,
            note_text,
        );
        DevLocal::publish_initial_pair_with_identity(&context, &vault, &identity)
            .expect("v2 发布初始基线（含真实 sync）");
        drop(vault);
        drop(identity);
    }
    let titles_a = {
        let vault = Vault::open(dir_a.join("vault.sqlite3")).expect("A 库");
        titles(&vault)
    };
    let titles_b = {
        let vault = Vault::open(dir_b.join("vault.sqlite3")).expect("B 库");
        titles(&vault)
    };
    assert!(titles_a.iter().any(|title| title == "A 独有笔记"));
    assert!(!titles_a.iter().any(|title| title == "B 独有笔记"));
    assert!(titles_b.iter().any(|title| title == "B 独有笔记"));
    assert!(!titles_b.iter().any(|title| title == "A 独有笔记"));
    let _ = (code_a, code_b);

    // ---- 重启分别进 Main（active 决定自动打开哪一个）----
    set_active_instance(&store, Some(&inst_a));
    let (root_a, cx_a) = open_root(cx, options_for(&layout, &relay));
    cx_a.run_until_parked();
    assert_eq!(state_of(&root_a, cx_a), RootStateLabel::Main);
    assert_eq!(
        root_a.update(cx_a, |root, _| root
            .active_entry()
            .map(|entry| entry.instance_id.clone())),
        Some(inst_a.clone())
    );
    assert!(
        root_a.update(cx_a, |root, _| root.main_shell().is_some()),
        "A 的 Main 必须是真实 AppShell"
    );
    root_a.update(cx_a, |root, cx| {
        assert!(root.should_close(cx));
    });

    set_active_instance(&store, Some(&inst_b));
    let (root_b, cx_b) = open_root(cx, options_for(&layout, &relay));
    cx_b.run_until_parked();
    assert_eq!(state_of(&root_b, cx_b), RootStateLabel::Main);
    assert_eq!(
        root_b.update(cx_b, |root, _| root
            .active_entry()
            .map(|entry| entry.instance_id.clone())),
        Some(inst_b.clone())
    );
    assert!(root_b.update(cx_b, |root, _| root.main_shell().is_some()));
    root_b.update(cx_b, |root, cx| {
        assert!(root.should_close(cx));
    });

    // ---- Welcome 多实例列表可打开不同 vault（不热切换：只从选择页打开）----
    set_active_instance(&store, None);
    let (root_w, cx_w) = open_root(cx, options_for(&layout, &relay));
    cx_w.run_until_parked();
    assert_eq!(state_of(&root_w, cx_w), RootStateLabel::Welcome);
    let listed = root_w.update(cx_w, |root, _| {
        root.entries()
            .into_iter()
            .map(|entry| entry.instance_id)
            .collect::<Vec<_>>()
    });
    assert_eq!(listed, vec![inst_a.clone(), inst_b.clone()]);
    expect_bounds(cx_w, "welcome-vault-0");
    expect_bounds(cx_w, "welcome-vault-1");
    let first = expect_bounds(cx_w, "welcome-vault-0");
    cx_w.simulate_click(first.center(), Modifiers::default());
    cx_w.run_until_parked();
    assert_eq!(state_of(&root_w, cx_w), RootStateLabel::Main);
    assert_eq!(
        root_w.update(cx_w, |root, _| root
            .active_entry()
            .map(|entry| entry.instance_id.clone())),
        Some(inst_a)
    );
    root_w.update(cx_w, |root, cx| {
        root.back_to_welcome(cx);
        assert!(root.should_close(cx));
    });
    let second = expect_bounds(cx_w, "welcome-vault-1");
    cx_w.simulate_click(second.center(), Modifiers::default());
    cx_w.run_until_parked();
    assert_eq!(state_of(&root_w, cx_w), RootStateLabel::Main);
    assert_eq!(
        root_w.update(cx_w, |root, _| root
            .active_entry()
            .map(|entry| entry.instance_id.clone())),
        Some(inst_b)
    );
    root_w.update(cx_w, |root, cx| {
        assert!(root.should_close(cx));
    });
}

// ---------------------------------------------------------------------------
// 2) v2 邀请链接：api=2 → PENDING → 批准 → Main → v2 数据面同步
// ---------------------------------------------------------------------------

#[gpui::test]
async fn v2_invite_link_carries_api_and_joins_through_v2_data_plane(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("v2-invite");
    let store = layout.store();
    let creator_dir = layout.instances_dir.join("creator-a");
    let creator_vault_id = [0xC1u8; 16];
    let creator_path = creator_dir.join("vault.sqlite3");
    let creator_vault = ticket_vault(&relay, &creator_path, creator_vault_id, "A v2 实例");
    let creator_identity = DevIdentityStore.load(&creator_path).expect("A 身份");
    assert!(creator_identity.manager_seed.is_some());
    let note_doc = seed_note(
        &creator_vault,
        creator_identity.loro_peer_id(),
        creator_identity.current_epoch,
        "A 的 v2 笔记",
        "A 写的第一行\n",
    );
    let context = v2_context(&relay);
    DevLocal::publish_initial_pair_with_identity(&context, &creator_vault, &creator_identity)
        .expect("A 发布基线");

    // A 生成邀请链接：v2 vault 必须带 api=2。
    let link =
        create_invite_link(&context, &creator_vault, &creator_identity).expect("生成邀请失败");
    assert!(
        link.url.contains("&api=2#token="),
        "v2 邀请链接必须带 api=2：{}",
        link.url
    );
    assert!(link.url.starts_with("mn-pair://join?v=1&relay="));

    // B：粘贴链接 → PENDING（不写配置/身份）。
    let b_dir = layout.instances_dir.join("invitee-b");
    let (root, cx) = open_root(cx, options_for(&layout, &relay));
    cx.run_until_parked();
    root.update(cx, |root, cx| {
        root.enter_join_form(cx);
        root.join_form_set_dir(&b_dir.display().to_string(), cx);
        root.join_form_set_invite(&link.url, cx);
        root.submit_join(cx);
    });
    assert!(
        root.update(cx, |root, cx| root
            .wait_for_invite_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "v2 邀请兑换必须收敛"
    );
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::InvitePending);
    let facts = root
        .update(cx, |root, _| root.invite_pending_facts().cloned())
        .expect("PENDING 公开事实");
    assert_eq!(facts.api_version, RelayApiVersion::V2);
    assert_eq!(facts.vault_id, hex16(&creator_vault_id));
    assert!(profile_entries(&store).is_empty(), "PENDING 期间不得写配置");
    assert!(
        !DevIdentityStore::path_for(&b_dir.join("vault.sqlite3")).exists(),
        "PENDING 期间不得写身份"
    );

    // A 批准（v2 管理签名面）。
    let device = decode_hex16(&facts.device_id);
    let now = u64::try_from(relay.now()).expect("now");
    let approved = approve_pending_device(&context, &creator_vault, &creator_identity, device, now)
        .expect("A 批准失败");
    assert!(approved.changed);

    // B 检查 → bootstrap → Main；配置 api_version=2。
    root.update(cx, |root, cx| root.check_invite(cx));
    assert!(
        root.update(cx, |root, cx| root
            .wait_for_invite_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "v2 检查/bootstrap 必须收敛"
    );
    cx.run_until_parked();
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Main,
        "批准后必须进入 Main（否则 bootstrap/config 写回失败）"
    );
    let entries = profile_entries(&store);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].2,
        RelayApiVersion::V2,
        "邀请加入必须写 api_version=2"
    );
    assert_eq!(entries[0].1, hex16(&creator_vault_id));
    assert_eq!(
        entries[0].3,
        b_dir.join("vault.sqlite3").display().to_string()
    );

    let b_identity = DevIdentityStore
        .load(&b_dir.join("vault.sqlite3"))
        .expect("B 身份");
    assert!(b_identity.manager_seed.is_none(), "邀请加入没有管理私钥");
    {
        let b_vault = Vault::open(b_dir.join("vault.sqlite3")).expect("B 库");
        assert_eq!(
            titles(&b_vault),
            vec!["A 的 v2 笔记".to_owned()],
            "B 必须看到 A 的笔记"
        );
        let text = load_body_doc(&b_vault, &note_doc, b_identity.loro_peer_id(), None)
            .expect("B 正文")
            .0
            .text();
        assert!(text.contains("A 写的第一行"), "B 必须看到 A 的正文");
    }

    // B 用 v2 数据面同步：A 再写一篇并推送，B sync_once 拉到。
    root.update(cx, |root, cx| {
        root.back_to_welcome(cx);
        assert!(root.should_close(cx));
    });
    let new_doc = seed_note(
        &creator_vault,
        creator_identity.loro_peer_id(),
        creator_identity.current_epoch,
        "A 的第二篇",
        "第二篇正文\n",
    );
    DevLocal::sync_once(&context, &creator_vault, &creator_identity).expect("A 推送");
    let b_vault = Vault::open(b_dir.join("vault.sqlite3")).expect("B 库");
    DevLocal::sync_once(&context, &b_vault, &b_identity).expect("B 在 v2 面同步");
    assert!(
        titles(&b_vault).iter().any(|title| title == "A 的第二篇"),
        "B 必须通过 v2 数据面拉到 A 的新笔记"
    );
    let text = load_body_doc(&b_vault, &new_doc, b_identity.loro_peer_id(), None)
        .expect("B 正文")
        .0
        .text();
    assert!(text.contains("第二篇正文"));
}

// ---------------------------------------------------------------------------
// 3) 票据失败矩阵：403 / 409 / 429 明确分类，不建库/身份/配置
// ---------------------------------------------------------------------------

#[gpui::test]
async fn ticket_failures_are_classified_and_persist_nothing(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    // 第二个 relay 单独跑「已用/重复」；第三个 relay 单独跑限流（默认 6 次/分钟，
    // 前面每个失败请求都会计数，所以限流用例必须独立一片时间窗）。
    let relay_used = DevRelay::start().expect("起第二个 dev relay 失败");
    let relay_rate = DevRelay::start().expect("起限流 dev relay 失败");
    let (_dir, layout) = temp_layout("failures");
    let store = layout.store();
    let (root, cx) = open_root(cx, options_for(&layout, &relay));
    cx.run_until_parked();

    // (a) 未知票据 + 任意 vault → 403 FORBIDDEN（relay 统一不区分原因）。
    let unknown_ticket = [0x77u8; 32];
    let bogus = link_from_ticket(&relay, [0x11u8; 16], &unknown_ticket);
    let dir_403 = layout.instances_dir.join("dir-403");
    failed_create_via_link(&root, cx, &bogus, &dir_403);
    let message = error_message(&root, cx);
    assert!(
        message.contains("403") && message.contains("FORBIDDEN"),
        "未知票据必须明确 403：{message}"
    );
    assert!(
        !message.contains("mn-pair://"),
        "错误不得回显链接：{message}"
    );
    assert!(
        !message.contains(&menghunote_sync::base64url::encode(&unknown_ticket)),
        "错误不得回显票据：{message}"
    );
    assert!(profile_entries(&store).is_empty());
    root.update(cx, |root, cx| root.back_to_welcome(cx));

    // (b) 票据与链接 vault 不一致 → 403（不消费票据：随后用票据自己的 vault 仍可创建）。
    let bound_vault = [0x21u8; 16];
    let wrong_vault = [0x22u8; 16];
    let raw_ticket = relay
        .issue_create_ticket(bound_vault, Duration::from_secs(900))
        .expect("发行票据");
    let wrong_vault_link = link_from_ticket(&relay, wrong_vault, &raw_ticket);
    let dir_wrong = layout.instances_dir.join("dir-wrong-vault");
    failed_create_via_link(&root, cx, &wrong_vault_link, &dir_wrong);
    assert!(
        error_message(&root, cx).contains("403"),
        "错 vault 必须 403：{}",
        error_message(&root, cx)
    );
    root.update(cx, |root, cx| root.back_to_welcome(cx));
    let right_link = link_from_ticket(&relay, bound_vault, &raw_ticket);
    let dir_right = layout.instances_dir.join("dir-right-vault");
    create_via_link(&root, cx, &right_link, &dir_right);
    assert_eq!(profile_entries(&store).len(), 1, "正确链接必须创建成功");
    root.update(cx, |root, cx| {
        root.back_to_welcome(cx);
        assert!(root.should_close(cx));
    });

    // (c) 过期票据 → 403（relay 假时钟推进）。
    let expired_vault = [0x31u8; 16];
    let expired_link = issue_link(&relay, expired_vault, 60);
    relay.advance_secs(120);
    let dir_expired = layout.instances_dir.join("dir-expired");
    failed_create_via_link(&root, cx, &expired_link, &dir_expired);
    assert!(
        error_message(&root, cx).contains("403"),
        "过期票据必须 403：{}",
        error_message(&root, cx)
    );
    root.update(cx, |root, cx| root.back_to_welcome(cx));

    // (d) 已用票据（quota=1）→ 403（同票第二次创建）。
    let used_vault = [0x41u8; 16];
    let used_link = issue_link(&relay_used, used_vault, 900);
    let dir_used_first = layout.instances_dir.join("dir-used-first");
    create_via_link(&root, cx, &used_link, &dir_used_first);
    root.update(cx, |root, cx| {
        root.back_to_welcome(cx);
        assert!(root.should_close(cx));
    });
    let dir_used_second = layout.instances_dir.join("dir-used-second");
    failed_create_via_link(&root, cx, &used_link, &dir_used_second);
    assert!(
        error_message(&root, cx).contains("403"),
        "已用票据必须 403：{}",
        error_message(&root, cx)
    );
    root.update(cx, |root, cx| root.back_to_welcome(cx));

    // (e) 同 vault 重复创建：测试夹具直接种一张绑定已存在 vault 的新票据（ADR 0018 S4b
    //     起没有对 ACTIVE vault 的发行入口），该票有效但 registry 已是 ACTIVE →
    //     409 ALREADY_INITIALIZED，且票据不被消费。
    let dup_vault = [0x51u8; 16];
    let dup_link = issue_link(&relay_used, dup_vault, 900);
    let dir_dup_first = layout.instances_dir.join("dir-dup-first");
    create_via_link(&root, cx, &dup_link, &dir_dup_first);
    root.update(cx, |root, cx| {
        root.back_to_welcome(cx);
        assert!(root.should_close(cx));
    });
    let dup_raw = relay_used
        .seed_ticket_for_existing_vault(dup_vault, Duration::from_secs(900))
        .expect("种入已存在 vault 的新票据");
    let dup_link_again = link_from_ticket(&relay_used, dup_vault, &dup_raw);
    let dir_dup_second = layout.instances_dir.join("dir-dup-second");
    failed_create_via_link(&root, cx, &dup_link_again, &dir_dup_second);
    let message = error_message(&root, cx);
    assert!(
        message.contains("409") && message.contains("ALREADY_INITIALIZED"),
        "同 vault 重复必须 409：{message}"
    );
    root.update(cx, |root, cx| root.back_to_welcome(cx));

    // (f) v2 创建入口限流（默认 6 次/分钟）：连续失败请求第 7 次 → 429 RATE_LIMITED。
    let bogus_rate = link_from_ticket(&relay_rate, [0x61u8; 16], &[0x66u8; 32]);
    for index in 0..6 {
        let dir = layout.instances_dir.join(format!("dir-rate-{index}"));
        failed_create_via_link(&root, cx, &bogus_rate, &dir);
        assert!(
            error_message(&root, cx).contains("403"),
            "第 {index} 次应是 403：{}",
            error_message(&root, cx)
        );
        root.update(cx, |root, cx| root.back_to_welcome(cx));
    }
    let dir_rate = layout.instances_dir.join("dir-rate-limited");
    failed_create_via_link(&root, cx, &bogus_rate, &dir_rate);
    let message = error_message(&root, cx);
    assert!(
        message.contains("429") && message.contains("RATE_LIMITED") && message.contains("限流"),
        "第 7 次必须 429 且文案说明限流：{message}"
    );
    // 只有成功的 3 个实例写入配置；所有失败尝试都没有落盘。
    assert_eq!(
        profile_entries(&store).len(),
        3,
        "成功 3 个（错 vault 后重试 1 + 已用票首次 + 重复票首次）；失败不得写配置"
    );
}

// ---------------------------------------------------------------------------
// 4) 票据/秘密不落盘：配置 JSON、实例目录文件、错误文案
// ---------------------------------------------------------------------------

#[gpui::test]
async fn ticket_is_never_persisted_or_echoed(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("secrets");
    let store = layout.store();
    let vault_id = [0x71u8; 16];
    let raw = relay
        .issue_create_ticket(vault_id, Duration::from_secs(900))
        .expect("发行票据");
    let ticket_b64 = menghunote_sync::base64url::encode(&raw);
    let ticket_hex = menghunote_sync::ids::hex_of(&raw);
    let link = link_from_ticket(&relay, vault_id, &raw);
    assert!(link.contains(&ticket_b64), "链接 fragment 必须带票据");

    let (root, cx) = open_root(cx, options_for(&layout, &relay));
    cx.run_until_parked();
    let dir = layout.instances_dir.join("inst-secret");
    let _ = create_via_link(&root, cx, &link, &dir);

    // 配置里没有票据（也没有链接）。
    let config = std::fs::read_to_string(store.path()).expect("读配置");
    assert!(!config.contains(&ticket_b64), "配置不得含票据：{config}");
    assert!(!config.contains(&ticket_hex));
    assert!(!config.contains("mn-pair://"));
    assert!(!config.contains("ticket"));

    // 实例目录里没有票据/链接文件，文件内容也没有票据明文。
    let mut files = Vec::new();
    collect_files(&dir, &mut files);
    assert!(!files.is_empty());
    for file in &files {
        let name = file.file_name().map_or_else(String::new, |name| {
            name.to_string_lossy().to_ascii_lowercase()
        });
        assert!(
            !name.contains("ticket") && !name.contains("create-link"),
            "实例目录不得出现票据相关文件：{}",
            file.display()
        );
        let bytes = std::fs::read(file).unwrap_or_default();
        let as_text = String::from_utf8_lossy(&bytes);
        assert!(
            !as_text.contains(&ticket_b64),
            "文件内容含票据：{}",
            file.display()
        );
        assert!(
            !as_text.contains(&ticket_hex),
            "文件内容含票据 hex：{}",
            file.display()
        );
    }
    // 票据已移交给 worker（不在 UI 状态里残留）：链接输入框为空、票据模式已退出。
    assert_eq!(
        root.update(cx, |root, cx| root.form_create_link_is_empty(cx)),
        Some(true),
        "链接输入框必须已清空"
    );
    assert!(
        !root.update(cx, |root, _| root.form_has_create_link()),
        "票据不得留在 UI 状态里"
    );
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// 5) 配置写失败只重试保存：不重跑票据创建（票据不会被二次消费）
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[gpui::test]
async fn ticket_profile_save_failure_retries_without_recreating(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt as _;

    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("save-fail");
    let store = layout.store();
    let vault_id = [0x81u8; 16];
    let link = issue_link(&relay, vault_id, 900);
    let dir = layout.instances_dir.join("inst-save-fail");
    let (root, cx) = open_root(cx, options_for(&layout, &relay));
    cx.run_until_parked();

    apply_create_link(&root, cx, &link);
    root.update(cx, |root, cx| {
        root.form_set_dir(&dir.display().to_string(), cx);
        root.form_set_consent(true, cx);
        root.submit_create(cx);
    });
    assert!(
        root.update(cx, |root, cx| root
            .wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "票据 Create 必须收敛"
    );
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::BackupRequired);
    let vault_id_before = root
        .update(cx, |root, _| root.vault_id().map(str::to_owned))
        .expect("备份页 vault_id");
    assert!(root.update(cx, |root, _| root.recovery_code().is_some()));

    // 让配置写失败：父目录只读（Unix 权限；Windows 用 ACL，本测试只在 Unix 跑）。
    let config_dir = layout
        .config_path
        .parent()
        .expect("配置父目录")
        .to_path_buf();
    std::fs::create_dir_all(&config_dir).expect("创建配置目录失败");
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o500))
        .expect("设置只读失败");
    root.update(cx, |root, cx| {
        root.set_backup_confirmed(true, cx);
        root.confirm_backup(cx);
    });
    cx.run_until_parked();
    std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700))
        .expect("恢复权限失败");
    assert_eq!(
        state_of(&root, cx),
        RootStateLabel::Error,
        "配置写失败必须停在 Error（不能假装进 Main）"
    );
    let message = error_message(&root, cx);
    assert!(
        message.contains("配置保存失败") && message.contains("重试保存"),
        "错误必须说清「重试保存、不重跑 Create」：{message}"
    );
    assert!(!layout.config_path.exists(), "写失败不得留下半份配置");
    // 恢复码仍在内存里等重试（票据创建已经真实成功，不能丢）。
    assert!(
        root.update(cx, |root, _| root.recovery_code().is_some()),
        "配置写失败时恢复码必须保留在备份页状态里"
    );

    // 重试保存 → Main；vault_id 不变 = 没有重跑票据创建。
    root.update(cx, |root, cx| root.retry(cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    let entries = profile_entries(&store);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, vault_id_before);
    assert_eq!(
        entries[0].2,
        RelayApiVersion::V2,
        "重试保存也必须写 api_version=2"
    );
    assert_eq!(entries[0].1, hex16(&vault_id));
    assert_eq!(
        entries[0].3,
        dir.join("vault.sqlite3").display().to_string()
    );
}

// ---------------------------------------------------------------------------
// 6) 设置页：显示 API 面（v1/v2）与 relay instance/record pin 公开指纹
// ---------------------------------------------------------------------------

#[gpui::test]
async fn settings_show_api_version_and_public_pin(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("settings");
    let vault_id = [0x91u8; 16];
    let link = issue_link(&relay, vault_id, 900);
    let dir = layout.instances_dir.join("inst-settings");
    let (root, cx) = open_root(cx, options_for(&layout, &relay));
    cx.run_until_parked();
    let _ = create_via_link(&root, cx, &link, &dir);

    root.update(cx, |root, cx| root.open_settings(cx));
    cx.run_until_parked();
    expect_bounds(cx, "dev-sync-settings");
    expect_bounds(cx, "settings-api-version");
    expect_bounds(cx, "settings-fingerprint");
    let entry = root
        .update(cx, |root, _| root.settings_active_entry())
        .expect("设置里有 active 实例");
    assert_eq!(entry.api_version, RelayApiVersion::V2, "设置显示 v2");
    assert_eq!(entry.relay_mode, RelayMode::LocalDev);
    assert_eq!(entry.vault_id, hex16(&vault_id));
    let fingerprint = root
        .update(cx, |root, _| root.settings_fingerprint_state())
        .expect("设置必须读取 durable pin 指纹");
    assert!(
        fingerprint.has_pin(),
        "票据创建后必须有 durable pin 指纹：{fingerprint:?}"
    );
    // 关闭设置不写配置、不改实例。
    root.update(cx, |root, cx| root.close_settings(cx));
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    assert_eq!(profile_entries(&layout.store()).len(), 1);
}

// ---------------------------------------------------------------------------
// 7) Create 自动发布初始基线：备份页可直接复制邀请；B 加入不再 NO_VALID_BASELINE
// ---------------------------------------------------------------------------

/// Create 提交后等到备份页，返回恢复码（**不**确认备份、不进 Main）。
fn create_to_backup_page(
    root: &Entity<AppRoot>,
    cx: &mut VisualTestContext,
    link: &str,
    dir: &Path,
) -> String {
    apply_create_link(root, cx, link);
    root.update(cx, |root, cx| {
        root.form_set_dir(&dir.display().to_string(), cx);
        root.form_set_consent(true, cx);
        root.submit_create(cx);
    });
    cx.run_until_parked();
    assert!(
        root.update(cx, |root, cx| root
            .wait_for_create_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "票据 Create 必须在预算内收敛"
    );
    cx.run_until_parked();
    assert_eq!(state_of(root, cx), RootStateLabel::BackupRequired);
    root.update(cx, |root, _| root.recovery_code().map(str::to_owned))
        .expect("备份页必须有恢复码")
}

/// 推进测试时钟 + 宿主泵，直到 `ready(shell)` 成立（显式收敛，不 sleep）。
fn pump_shell_until(
    cx: &mut VisualTestContext,
    root: &Entity<AppRoot>,
    what: &str,
    mut ready: impl FnMut(&menghunote_app::shell::AppShell, &gpui::App) -> bool,
) {
    let deadline = Instant::now() + DEFAULT_OPERATION_BUDGET;
    loop {
        cx.executor()
            .advance_clock(menghunote_app::dev_sync::SYNC_PUMP_INTERVAL);
        root.update(cx, |root, cx| {
            if let Some(shell) = root.main_shell().cloned() {
                shell.update(cx, |shell, cx| shell.dev_sync_pump(cx));
            }
        });
        cx.run_until_parked();
        let done = root.update(cx, |root, app| {
            root.main_shell()
                .is_some_and(|shell| ready(shell.read(app), app))
        });
        if done {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{}s 内没有收敛（{what}）",
            DEFAULT_OPERATION_BUDGET.as_secs()
        );
        std::thread::yield_now();
    }
}

#[gpui::test]
async fn create_auto_publishes_baseline_and_backup_invite_joins_immediately(
    cx: &mut TestAppContext,
) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("auto-baseline");
    let vault_id = [0xD1u8; 16];
    let link = issue_link(&relay, vault_id, 900);
    let dir = layout.instances_dir.join("inst-auto-a");
    let vault_path = dir.join("vault.sqlite3");
    let (root, cx) = open_root(cx, options_for(&layout, &relay));
    cx.run_until_parked();

    let recovery_code = create_to_backup_page(&root, cx, &link, &dir);
    assert!(recovery_code.starts_with("mnrec1"), "恢复码仍必须展示");
    assert_eq!(
        root.update(cx, |root, _| root.backup_baseline_published()),
        Some(true),
        "Create 成功必须已自动发布初始基线"
    );
    expect_bounds(cx, "app-root-baseline-status");
    {
        let vault = Vault::open(&vault_path).expect("打开 A 库");
        let baseline = DevLocal::published_baseline(&vault)
            .expect("读 durable 基线事实")
            .expect("Create 必须自动发布初始基线（两份 checkpoint）");
        assert_ne!(baseline.checkpoint_id, [0u8; 16]);
    }

    // 备份页直接复制邀请：成功、token 只进剪贴板、UI 无明文。
    root.update(cx, |root, cx| root.copy_invite(cx));
    assert!(
        root.update(cx, |root, cx| root
            .wait_for_invite_copy_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "复制邀请必须在预算内收敛"
    );
    cx.run_until_parked();
    assert_eq!(
        root.update(cx, |root, _| root.backup_invite_error().map(str::to_owned)),
        None,
        "基线已发布时复制邀请不得失败"
    );
    let invite_id = root
        .update(cx, |root, _| root.backup_invite_id())
        .expect("必须生成邀请 id");
    assert_eq!(invite_id.len(), 32);
    assert!(
        cx.debug_bounds("app-root-invite-copied").is_some(),
        "复制成功必须显示折叠/已复制提示"
    );
    let clipboard = cx
        .read_from_clipboard()
        .and_then(|item| item.text())
        .expect("复制邀请必须写剪贴板");
    assert!(clipboard.starts_with("mn-pair://join?v=1&relay="));
    assert!(
        clipboard.contains("&api=2#token="),
        "v2 邀请链接必须带 api=2"
    );
    let target = InviteLinkTarget::parse(&clipboard).expect("剪贴板链接必须可解析");
    assert_eq!(target.vault_id(), vault_id);
    assert_eq!(menghunote_sync::ids::hex_of(&target.invite_id()), invite_id);

    // B：粘贴刚复制的邀请链接 → PENDING（v2 面；不写配置/身份）。
    let b_dir = layout.instances_dir.join("inst-auto-b");
    let b_options = AppRootOptions::for_test(layout.clone(), relay.base_url(), "inst-auto-b");
    drop(recovery_code);
    let (root_b, cx_b) = open_root(cx, b_options);
    cx_b.run_until_parked();
    root_b.update(cx_b, |root, cx| {
        root.enter_join_form(cx);
        root.join_form_set_dir(&b_dir.display().to_string(), cx);
        root.join_form_set_invite(&clipboard, cx);
        root.submit_join(cx);
    });
    assert!(
        root_b.update(cx_b, |root, cx| root
            .wait_for_invite_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "B 兑换邀请必须在预算内收敛"
    );
    cx_b.run_until_parked();
    assert_eq!(state_of(&root_b, cx_b), RootStateLabel::InvitePending);
    let facts = root_b
        .update(cx_b, |root, _| root.invite_pending_facts().cloned())
        .expect("B 的 PENDING 公开事实");
    assert_eq!(facts.api_version, RelayApiVersion::V2);

    // A 批准（备份页仍持有库/身份事实；用同一身份从 disk 打开一个只读管理连接）。
    let a_identity = DevIdentityStore.load(&vault_path).expect("读 A 身份");
    let a_vault = Vault::open(&vault_path).expect("打开 A 库");
    let approved = approve_pending_device(
        &v2_context(&relay),
        &a_vault,
        &a_identity,
        decode_hex16(&facts.device_id),
        u64::try_from(relay.now()).expect("now"),
    )
    .expect("A 批准 B");
    assert!(approved.changed);
    drop(a_vault);

    // B「检查状态」→ bootstrap：auto 基线必须让它进 Main（否则就是 NO_VALID_BASELINE）。
    root_b.update(cx_b, |root, cx| root.check_invite(cx));
    assert!(
        root_b.update(cx_b, |root, cx| root
            .wait_for_invite_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "B 检查/ bootstrap 必须在预算内收敛"
    );
    cx_b.run_until_parked();
    assert_eq!(
        state_of(&root_b, cx_b),
        RootStateLabel::Main,
        "B 必须进入 Main（缺基线时会停在等待页并报 NO_VALID_BASELINE）"
    );
    let b_vault = Vault::open(b_dir.join("vault.sqlite3")).expect("打开 B 库");
    assert!(
        list_tree(&b_vault).expect("读 B 结构树").is_empty(),
        "空库基线导入后 B 的树应为空"
    );
    root_b.update(cx_b, |root, cx| {
        assert!(root.should_close(cx));
    });
}

#[gpui::test]
async fn failed_auto_publish_keeps_instance_blocks_invite_then_retry_repairs(
    cx: &mut TestAppContext,
) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let (_dir, layout) = temp_layout("auto-baseline-fail");
    let vault_id = [0xE2u8; 16];
    let link = issue_link(&relay, vault_id, 900);
    let dir = layout.instances_dir.join("inst-fail-a");
    let vault_path = dir.join("vault.sqlite3");
    let (root, cx) = open_root(cx, options_for(&layout, &relay));
    cx.run_until_parked();
    // 只让"自动发布基线"这一步走不可达 relay（真实 transport 失败，不改 Create）。
    root.update(cx, |root, _| root.set_test_baseline_unreachable(true));

    let recovery_code = create_to_backup_page(&root, cx, &link, &dir);
    // Create 必须成功：库/身份/恢复码全在，只把基线降级为"未发布"。
    assert_eq!(
        root.update(cx, |root, _| root.backup_baseline_published()),
        Some(false),
        "发布失败不得被吞成成功"
    );
    assert!(vault_path.is_file(), "发布失败不得删除本地库");
    assert!(
        DevIdentityStore::path_for(&vault_path).exists(),
        "发布失败不得删除身份"
    );
    assert!(recovery_code.starts_with("mnrec1"), "恢复码仍必须展示");
    expect_bounds(cx, "app-root-create-warning");
    {
        let vault = Vault::open(&vault_path).expect("打开库");
        assert!(
            DevLocal::published_baseline(&vault)
                .expect("读 durable 基线事实")
                .is_none(),
            "注入失败后不得留下假基线事实"
        );
    }

    // 备份页复制邀请：本地前置检查明确挡住（不联网、不生成链接）。
    root.update(cx, |root, cx| root.copy_invite(cx));
    assert!(
        root.update(cx, |root, cx| root
            .wait_for_invite_copy_terminal(cx, DEFAULT_OPERATION_BUDGET)),
        "前置检查必须在预算内收敛"
    );
    cx.run_until_parked();
    let error = root
        .update(cx, |root, _| root.backup_invite_error().map(str::to_owned))
        .expect("缺基线时复制邀请必须明确失败");
    assert!(
        error.contains("尚未发布初始基线") && error.contains("发布"),
        "失败文案必须指向发布基线：{error}"
    );
    assert!(!error.contains("mn-pair://"), "失败文案不得含链接：{error}");
    assert!(
        root.update(cx, |root, _| root.backup_invite_id()).is_none(),
        "前置检查失败不得生成邀请"
    );

    // 进 Main：面板看到 durable 事实缺失 → 渲染「重试发布基线」。
    root.update(cx, |root, cx| {
        root.set_backup_confirmed(true, cx);
        root.confirm_backup(cx);
    });
    cx.run_until_parked();
    assert_eq!(state_of(&root, cx), RootStateLabel::Main);
    pump_shell_until(cx, &root, "面板读到基线缺失", |shell, app| {
        shell.dev_sync_panel().is_some_and(|panel| {
            let snapshot = panel.read(app).service().snapshot();
            snapshot.baseline_known && snapshot.published_baseline.is_none()
        })
    });
    expect_bounds(cx, "dev-sync-publish");
    let invite_reason = root.update(cx, |root, app| {
        root.main_shell().and_then(|shell| {
            shell
                .read(app)
                .dev_sync_panel()
                .and_then(|panel| panel.read(app).invite_block_reason())
        })
    });
    assert!(
        invite_reason.is_some_and(|reason| reason.contains("初始基线")),
        "缺基线时邀请必须先被挡住：{invite_reason:?}"
    );

    // 重试发布：先关闭设置遮罩，真实建一篇笔记（发布需要活动文档持闸），再重新打开设置点按钮。
    let close = expect_bounds(cx, "settings-close");
    cx.simulate_click(close.center(), Modifiers::default());
    cx.run_until_parked();
    let new_note = expect_bounds(cx, "sidebar-new-note");
    cx.simulate_click(new_note.center(), Modifiers::default());
    cx.run_until_parked();
    let retry = expect_bounds(cx, "dev-sync-publish");
    cx.simulate_click(retry.center(), Modifiers::default());
    pump_shell_until(cx, &root, "重试发布基线完成", |shell, _app| {
        shell
            .dev_sync_last_publish()
            .is_some_and(|report| report.is_ok())
            && !shell.dev_sync_publish_in_progress()
    });
    pump_shell_until(cx, &root, "面板读到基线已发布", |shell, app| {
        shell.dev_sync_panel().is_some_and(|panel| {
            let snapshot = panel.read(app).service().snapshot();
            snapshot.baseline_known && snapshot.published_baseline.is_some()
        })
    });
    assert!(
        cx.debug_bounds("dev-sync-publish").is_none(),
        "重试成功后必须隐藏发布按钮"
    );
    {
        let vault = Vault::open(&vault_path).expect("打开库");
        assert!(
            DevLocal::published_baseline(&vault)
                .expect("读 durable 基线事实")
                .is_some(),
            "重试成功后必须有 durable 基线事实"
        );
    }

    // 邀请现在可用（面板生成本地链接；基线下 B 能加入）。
    // 先等面板真正空闲：自动同步可能占着单操作槽，忙碌时点击只会得到"有操作在途"。
    pump_shell_until(cx, &root, "面板空闲可生成邀请", |shell, app| {
        shell
            .dev_sync_panel()
            .is_some_and(|panel| panel.read(app).invite_block_reason().is_none())
    });
    let invite = expect_bounds(cx, "dev-sync-invite");
    cx.simulate_click(invite.center(), Modifiers::default());
    pump_shell_until(cx, &root, "重试后生成邀请", |shell, app| {
        shell.dev_sync_panel().is_some_and(|panel| {
            panel.read(app).invite_link().is_some()
                && panel.read(app).service().snapshot().in_flight.is_none()
        })
    });
    drop(recovery_code);
    let invite_url = root.update(cx, |root, app| {
        root.main_shell().and_then(|shell| {
            shell
                .read(app)
                .dev_sync_panel()
                .and_then(|panel| panel.read(app).invite_link().map(|link| link.url.clone()))
        })
    });
    let invite_url = invite_url.expect("重试发布后必须能生成邀请链接");
    let target = InviteLinkTarget::parse(&invite_url).expect("重试后生成的链接必须可解析");
    assert_eq!(target.vault_id(), vault_id);
    assert_eq!(target.api_version(), RelayApiVersion::V2);
}
