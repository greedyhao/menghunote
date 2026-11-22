//! ADR 0009 切片 2b：真实 AppShell（dev-sync）的本地写入 → 同事务 `sync_outbox` 意图。
//!
//! 覆盖三件事（全部读**落库的 outbox 行**，不看内存状态）：
//!
//! 1. 真实键入 + `close_all` → outbox 出现身份 peer / epoch 的行（不是常量 1 / 0）；
//! 2. 结构新建（同一 commit 里再 move 进目录）→ 恰好一条 structure intent；
//! 3. 旧/非空开发库 + 无身份 → 明确拒绝，且不新建身份、不删除旧库；
//!    空库/不存在 → 允许；`persist_protected` 原子替换且失败不破坏旧文件。
//!
//! 纪律（AGENTS.md §6）：临时目录在 `CARGO_TARGET_TMPDIR` 下；Linux 用户主键的
//! `XDG_DATA_HOME` 也指到 `target/tmp`。收敛点只用 `run_until_parked()` 与生产自己的
//! 关窗排空（`close_all`），不 sleep。

#![cfg(feature = "dev-sync")]

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _, Render,
    Styled as _, TestAppContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::identity::ensure_instance_vault_ready;
use menghunote_app::dev_sync::service::ServiceConfig;
use menghunote_app::dev_sync::DevIdentityStore;
use menghunote_app::dev_vault;
use menghunote_app::shell::AppShell;
use menghunote_storage::{
    append_updates, intent_message_id, list_tree, list_tree_sorted, load_head, pending_encryption,
    SyncContext, UpdateRecord, Value, Vault, STRUCTURE_DOC_ID,
};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dev-sync-intent-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录（**不用** `%TEMP%`）。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dev-sync-intent-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 窗口根视图：给 `AppShell` 一个稳定的宿主 selector（与 headless_ui.rs 同构）。
struct ShellHost {
    shell: Entity<AppShell>,
}

impl Render for ShellHost {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .size_full()
            .debug_selector(|| "app-shell-host".to_string())
            .child(self.shell.clone())
    }
}

// ---------------------------------------------------------------------------
// 1) 真实键入 + close_all → outbox 带身份 peer / epoch
// ---------------------------------------------------------------------------

#[gpui::test]
async fn app_shell_typing_flushes_intents_with_identity_peer_and_epoch(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("typing");
    let vault_path = dir.path().join("vault.sqlite3");

    let identity = DevIdentityStore::load_or_create(&vault_path).expect("dev identity");
    let peer = identity.loro_peer_id();
    assert_ne!(peer, dev_vault::DEV_PEER_ID, "身份 peer 不得等于常量 1");
    let sync = SyncContext::new(peer, identity.current_epoch);

    let dev = dev_vault::open_vault_at_with_peer(&vault_path, peer).expect("打开实例库");
    // `AppShell` 打开的是**侧栏展示顺序**（目录优先 + A-Z）的第一篇笔记。
    let first_doc = list_tree_sorted(&dev.vault, menghunote_storage::TreeSort::NameAsc)
        .expect("读结构树")
        .into_iter()
        .find(|entry| entry.is_note())
        .and_then(|entry| entry.body_doc_id)
        .expect("第一篇笔记必须有 body_doc_id");
    assert!(
        pending_encryption(&dev.vault, 10)
            .expect("读 pending")
            .is_empty(),
        "播种不得产生 sync intent（否则下面的断言没有判别力）"
    );

    let vault = dev.vault.clone();
    let config = DevSyncConfig {
        relay_mode: menghunote_app::onboarding::RelayMode::LocalDev,
        vault: vault_path.clone(),
        service: ServiceConfig::new("http://127.0.0.1:18080"),
    };
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync(vault, "dev-sync-intent".to_owned(), config, sync, cx)
        });
        shell.update(cx, |shell, cx| shell.open_settings_headless(cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("dev-sync-panel").is_some(),
        "dev-sync 必须真的启用（面板已挂载）"
    );

    // 点正文 → 键入（真实输入通道 → 会话 journal → flush）。
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出 selector=editor-pane");
    cx.simulate_click(pane.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("a");
    cx.run_until_parked();

    // 生产关窗路径做有界排空（不 sleep）。
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();

    // 落库断言：outbox 里必须有本次键入的行，且 peer/epoch 来自身份。
    let rows = pending_encryption(&dev.vault, 100).expect("读 pending");
    assert!(
        !rows.is_empty(),
        "真实键入 + close_all 之后必须出现 sync_outbox 行"
    );
    let mut body_rows = 0usize;
    for row in &rows {
        assert_eq!(row.state, menghunote_storage::SyncOutboxState::Pending);
        assert_eq!(
            row.key_epoch, sync.key_epoch,
            "epoch 必须来自身份（Phase 1 = 1），不是常量 0"
        );
        assert_eq!(
            row.message_id,
            intent_message_id(&row.doc_id, row.seq, sync),
            "message_id 必须由身份 peer + epoch 确定性派生"
        );
        assert!(
            row.object_id.is_some_and(|value| value != [0; 32]),
            "每条意图必须有 32B object_id"
        );
        assert_eq!(
            row.message_id.len(),
            32,
            "engine 只接受 16B hex message id：{:?}",
            row.message_id
        );
        if row.doc_id == first_doc {
            body_rows += 1;
            // 同一 (doc, seq) 换成常量 peer 1 会得到不同 message：证明 peer 真的参与了派生。
            let with_default_peer = intent_message_id(
                &row.doc_id,
                row.seq,
                SyncContext::new(dev_vault::DEV_PEER_ID, sync.key_epoch),
            );
            assert_ne!(
                row.message_id, with_default_peer,
                "意图必须用身份 peer 派生，不能是常量 1"
            );
        }
    }
    assert!(
        body_rows > 0,
        "正文 flush 必须产生 intent（实际 doc={first_doc} 的行数 {body_rows}）"
    );

    // loro_updates 里也必须有对应 seq 的 update（intent 不是空中楼阁）。
    let update_seqs = dev
        .vault
        .query(
            "SELECT seq FROM loro_updates WHERE doc_id=?1 ORDER BY seq",
            &[Value::Text(first_doc.clone())],
        )
        .expect("读 loro_updates");
    assert!(!update_seqs.is_empty(), "键入必须真的落库 update");
    for row in rows.iter().filter(|row| row.doc_id == first_doc) {
        assert!(
            update_seqs
                .iter()
                .any(|seq| matches!(seq.first(), Some(Value::Integer(value)) if *value == i64::try_from(row.seq).expect("seq"))),
            "intent seq={} 必须能在 loro_updates 里找到",
            row.seq
        );
    }
}

// ---------------------------------------------------------------------------
// 2) 结构新建/移动 → structure intent
// ---------------------------------------------------------------------------

#[gpui::test]
async fn structure_writes_produce_structure_intents(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("structure");
    let vault_path = dir.path().join("vault.sqlite3");

    let identity = DevIdentityStore::load_or_create(&vault_path).expect("dev identity");
    let peer = identity.loro_peer_id();
    let sync = SyncContext::new(peer, identity.current_epoch);
    let dev = dev_vault::open_vault_at_with_peer(&vault_path, peer).expect("打开实例库");
    let structure_head_before = load_head(&dev.vault, STRUCTURE_DOC_ID)
        .expect("结构 head")
        .head_seq;
    assert!(
        pending_encryption(&dev.vault, 10)
            .expect("读 pending")
            .is_empty(),
        "播种不得产生 intent"
    );

    let vault = dev.vault.clone();
    let config = DevSyncConfig {
        relay_mode: menghunote_app::onboarding::RelayMode::LocalDev,
        vault: vault_path.clone(),
        service: ServiceConfig::new("http://127.0.0.1:18080"),
    };
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync(vault, "dev-sync-intent".to_owned(), config, sync, cx)
        });
        shell.update(cx, |shell, cx| shell.open_settings_headless(cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    assert_eq!(
        list_tree(&dev.vault).expect("读树").len(),
        3,
        "播种后应有 3 个节点"
    );

    // 右键目录行 →「新建笔记」（create_note + move_node 在同一个 commit 里）。
    // 侧栏是「目录优先 + 文件名 A-Z」：demo 树里的目录在**第一行**。
    let folder = cx
        .debug_bounds("tree-row-0")
        .expect("tree-row-0 必须是目录行");
    cx.simulate_mouse_down(
        folder.center(),
        gpui::MouseButton::Right,
        Modifiers::default(),
    );
    cx.run_until_parked();
    let item = cx
        .debug_bounds("menu-item-1")
        .expect("目录菜单必须画出「新建笔记」");
    cx.simulate_click(item.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        list_tree(&dev.vault).expect("读树").len(),
        4,
        "「新建笔记」必须让结构树从 3 项变 4 项"
    );

    // 关窗排空（结构提交是同步的，这一步只是保证没有残留写入）。
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();

    let rows = pending_encryption(&dev.vault, 100).expect("读 pending");
    let structure_head = load_head(&dev.vault, STRUCTURE_DOC_ID)
        .expect("结构 head")
        .head_seq;
    assert!(
        structure_head > structure_head_before,
        "「新建笔记」必须真的推进结构 durable seq：{structure_head_before} → {structure_head}"
    );
    assert_eq!(rows.len(), 1, "一次结构提交 → 恰好一条 structure intent");
    let row = &rows[0];
    assert_eq!(row.doc_id, STRUCTURE_DOC_ID, "intent 必须属于结构文档");
    assert_eq!(row.seq, structure_head, "intent seq = 本次结构 head");
    assert_eq!(row.key_epoch, sync.key_epoch);
    assert_eq!(
        row.batch_id, "sidebar",
        "结构 intent 沿用 AppShell 的 batch_id"
    );
    assert_eq!(
        row.message_id,
        intent_message_id(STRUCTURE_DOC_ID, structure_head, sync)
    );
}

// ---------------------------------------------------------------------------
// 3) 旧库拒绝 / 空库允许 / persist_protected
// ---------------------------------------------------------------------------

/// 造一个有内容的旧库（`loro_updates` / `notes` 都有行）。
fn seed_old_vault(path: &Path) {
    let vault = Vault::open(path).expect("建旧库");
    menghunote_storage::create_body_doc(&vault, "doc-old").expect("loro_docs 行");
    menghunote_storage::create_note_slot(&vault, "note-old", "doc-old", "旧笔记", 1)
        .expect("notes 行");
    // 只用 storage 的公开 API（app 不依赖 loro，拿不到 VersionVector 类型）。
    let (body, head) =
        menghunote_storage::load_body_doc(&vault, "doc-old", 1, None).expect("重放正文");
    body.insert_scalars(0, "旧内容").expect("编辑");
    let base = head.version_vector().expect("head vv");
    let update = body.pending_update(&base).expect("增量");
    append_updates(
        &vault,
        "doc-old",
        head.head_seq,
        &[UpdateRecord::new("old", update)],
    )
    .expect("A 事务");
    drop(vault);
}

#[test]
fn old_non_empty_vault_without_identity_is_rejected_without_touching_files() {
    ensure_test_user_key();
    let dir = temp_root("old");
    let vault_path = dir.path().join("vault.sqlite3");
    seed_old_vault(&vault_path);
    let identity_path = DevIdentityStore::path_for(&vault_path);
    let before = std::fs::read(&vault_path).expect("读旧库字节");
    assert!(!identity_path.exists(), "前置：旧库没有身份文件");

    let error = ensure_instance_vault_ready(&vault_path).expect_err("非空旧库 + 无身份必须被拒绝");
    let message = format!("{error:#}");
    assert!(
        message.contains("旧") && message.contains("非空"),
        "错误信息必须说明旧/非空库：{message}"
    );
    assert!(
        message.contains("新目录"),
        "错误信息必须给出可执行的出路（换新目录）：{message}"
    );
    assert!(
        !identity_path.exists(),
        "拒绝路径**不得**为新库创建身份（否则旧库会被悄悄接管）"
    );
    assert_eq!(
        std::fs::read(&vault_path).expect("拒绝后再读"),
        before,
        "拒绝路径不得改写/删除旧库"
    );
}

#[test]
fn empty_or_missing_vault_is_allowed_and_identity_is_created() {
    ensure_test_user_key();

    // 不存在的文件 → 允许。
    let dir = temp_root("empty-missing");
    let missing = dir.path().join("vault.sqlite3");
    ensure_instance_vault_ready(&missing).expect("不存在的库必须允许");
    assert!(!missing.exists(), "检查阶段不得建库");
    let identity = DevIdentityStore::load_or_create(&missing).expect("随后可以创建身份");
    assert_ne!(identity.loro_peer_id(), 0);

    // 只有 schema 的空库 → 允许。
    let dir2 = temp_root("empty-schema");
    let empty = dir2.path().join("vault.sqlite3");
    drop(Vault::open(&empty).expect("建空库"));
    ensure_instance_vault_ready(&empty).expect("空库必须允许");
    let identity2 = DevIdentityStore::load_or_create(&empty).expect("空库可以创建身份");
    assert_ne!(identity2.loro_peer_id(), 0);
}

#[test]
fn existing_identity_keeps_2a_semantics_and_load_reports_missing() {
    ensure_test_user_key();
    let dir = temp_root("keep-2a");
    let vault_path = dir.path().join("vault.sqlite3");

    // 没有身份时 `load` 必须明确报错（不创建）。
    let store = DevIdentityStore;
    let error = store.load(&vault_path).expect_err("不存在时必须报错");
    assert!(
        format!("{error:#}").contains("dev identity"),
        "错误信息必须指明身份文件：{error:#}"
    );
    assert!(
        !DevIdentityStore::path_for(&vault_path).exists(),
        "load 不得创建身份文件"
    );

    // 已有身份 + 非空库：沿用 2a（放行，由后续 open/load 校验库）；不重复创建。
    let identity = DevIdentityStore::load_or_create(&vault_path).expect("创建身份");
    let file = DevIdentityStore::path_for(&vault_path);
    let bytes = std::fs::read(&file).expect("读身份文件");
    seed_old_vault(&vault_path);
    ensure_instance_vault_ready(&vault_path).expect("已有身份时沿用 2a 语义（放行）");
    assert_eq!(
        std::fs::read(&file).expect("再读"),
        bytes,
        "放行路径不得改写身份文件"
    );
    let loaded = store.load(&vault_path).expect("读回身份");
    assert_eq!(loaded.loro_peer_id(), identity.loro_peer_id());
}

#[test]
fn persist_protected_replaces_atomically_and_never_breaks_old_file() {
    ensure_test_user_key();
    let dir = temp_root("persist");
    let vault_path = dir.path().join("vault.sqlite3");
    let store = DevIdentityStore;

    // 先落一份身份 A。
    let first = DevIdentityStore::load_or_create(&vault_path).expect("身份 A");
    let file = DevIdentityStore::path_for(&vault_path);
    let first_bytes = std::fs::read(&file).expect("读 A");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::symlink_metadata(&file)
                .expect("lstat")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    // 原子替换成 B（同一 vault/device 的新密钥材料 → peer 变）。
    let replacement =
        menghunote_sync::identity::LocalIdentityV1::generate_dev(first.vault_id, first.device_id)
            .expect("生成替换身份");
    store
        .persist_protected(&vault_path, &replacement)
        .expect("原子替换");
    assert_ne!(
        std::fs::read(&file).expect("读 B"),
        first_bytes,
        "替换后文件必须变化"
    );
    let loaded = store.load(&vault_path).expect("读回");
    assert_eq!(loaded.device_id, first.device_id);
    assert_eq!(loaded.vault_id, first.vault_id);
    assert_eq!(loaded.loro_peer_id(), replacement.loro_peer_id());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let meta = std::fs::symlink_metadata(&file).expect("lstat");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600, "替换后仍 0600");
        assert_eq!(meta.nlink(), 1, "临时文件名必须被清掉");
    }

    // 失败路径：目录不可写 → 替换失败，旧文件逐字节不变。
    let good_bytes = std::fs::read(&file).expect("读替换后文件");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        // 非 root 才能靠目录权限注入失败；root 会直接写成功，这种环境跳过该断言。
        let parent = dir.path();
        let original = std::fs::symlink_metadata(parent)
            .expect("lstat dir")
            .permissions();
        let mut readonly = original.clone();
        readonly.set_mode(0o500);
        std::fs::set_permissions(parent, readonly).expect("chmod 0500");
        let result = store.persist_protected(&vault_path, &replacement);
        std::fs::set_permissions(parent, original).expect("恢复目录权限");
        if result.is_err() {
            assert_eq!(
                std::fs::read(&file).expect("失败后再读"),
                good_bytes,
                "替换失败不得破坏旧身份文件"
            );
            // 不留临时文件。
            let leftovers: Vec<_> = std::fs::read_dir(parent)
                .expect("读目录")
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().contains(".replace."))
                .collect();
            assert!(leftovers.is_empty(), "失败路径必须清掉临时文件");
        }
    }
    // 无论如何都还能读回一份可用的身份（旧或新）。
    let usable = store.load(&vault_path).expect("失败路径后仍可读回");
    assert_ne!(usable.loro_peer_id(), 0);
}
