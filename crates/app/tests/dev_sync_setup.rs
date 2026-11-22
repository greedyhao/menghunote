//! dev-sync **启动生命周期**（ADR 0009 Phase 1 / 切片 4c-1）的集成测试。
//!
//! 覆盖三件不可分割的事：
//!
//! 1. `prepare(Create)`：真实 genesis → 受保护身份落盘（0600）→ 库可打开、树为空；
//! 2. A 真实建一篇笔记 → 发布初始基线 → B 只凭 `vault_id + 恢复码` `prepare(Join)`，
//!    打开后能读到同一篇正文（B 不读 A 的目录/身份/root）；
//! 3. 普通模式（Existing）缺身份 / 错 setup secret / 错恢复码：明确错误，且**不创建**
//!    库/身份（不留下半初始化状态）；已有身份时只 load、不改写文件。
//!
//! 切片 4c-3 追加（本文件后半）：Create 的一次性交接块能直接喂给 Join、
//! `--dev-sync-info` 只读公开标识（relay 不可达也能用）、`--dev-sync-publish` 缺码/错码
//! 明确失败且库不变、正确码发布后 B 可见。
//!
//! ## 纪律（AGENTS.md §4/§6）
//!
//! - 临时目录一律在 `CARGO_TARGET_TMPDIR`（`target/tmp`），不用 `%TEMP%`；
//! - Linux 用户主键的 `XDG_DATA_HOME` 也指到 `target/tmp`，不碰 `~/.local/share`；
//! - **不 sleep**：收敛点只有 `prepare` / `publish_initial_pair` / `bootstrap` 的显式返回值；
//! - relay 用 `menghunote-dev-relay`（进程内真实 axum + 放行验签 hook）。⚠️ 非生产：
//!   本文件**不能**作为鉴权/验签验收，那些负例必须走 relay 生产路径的 `Ed25519Hook`。
//!
//! feature 关闭时整个文件编译为空（0 个测试），默认 `cargo test -p menghunote-app`
//! 完全不受影响。

#![cfg(feature = "dev-sync")]

use std::path::Path;

use menghunote_app::dev_sync::cli;
use menghunote_app::dev_sync::gate::{DevSyncConfig, CREATE_FLAG, JOIN_FLAG};
use menghunote_app::dev_sync::service::ServiceConfig;
use menghunote_app::dev_sync::setup::{self, SetupMode, SetupSecrets};
use menghunote_app::dev_sync::DevIdentityStore;
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, list_tree, load_body_doc, local_sync_intent, BodyDoc,
    HeadState, StructureWriter, SyncContext, UpdateRecord, Vault,
};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::ids;
use menghunote_sync::random::OsRandom;
use menghunote_sync::recovery::RecoverySeed;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
///
/// `Once` 保证只设一次：`cargo test` 多线程下并发 `set_var` 既不安全也没必要。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dev-sync-setup-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dev-sync-setup-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

fn config_for(relay: &DevRelay, vault_path: &Path) -> DevSyncConfig {
    DevSyncConfig {
        vault: vault_path.to_path_buf(),
        service: ServiceConfig::new(relay.base_url()),
    }
}

fn create_secrets(relay: &DevRelay) -> SetupSecrets {
    SetupSecrets {
        setup_secret: Some(relay.setup_secret().to_owned()),
        ..SetupSecrets::default()
    }
}

/// `SetupOutcome` 故意不实现 `Debug`（里面有身份秘密），所以测试不能直接
/// `expect_err`；这里把 `Err` 取出来，`Ok` 就 panic（语义与 `expect_err` 相同）。
fn setup_prepare_err(
    result: Result<setup::SetupOutcome, setup::SetupError>,
    message: &str,
) -> setup::SetupError {
    match result {
        Ok(_) => panic!("{message}"),
        Err(error) => error,
    }
}

// ---------------------------------------------------------------------------
// 真实笔记的写入助手（与 crates/sync/tests/dev_local.rs 同一套生产路径）
// ---------------------------------------------------------------------------

struct Note {
    doc_id: String,
    title: String,
    text: String,
}

/// 走真实 storage 路径建一篇笔记：结构 CRDT（含 sync intent）→ 投影 → 正文 A 事务。
fn create_note(vault: &Vault, peer: u64, epoch: u64, title: &str, text: &str) -> Note {
    let ctx = SyncContext::new(peer, epoch);
    let mut writer =
        StructureWriter::open_with_sync(vault, peer, Some(ctx)).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer.commit("dev-sync-setup-note").expect("结构提交失败");

    let (doc, head) =
        load_body_doc(vault, &seed.body_doc_id, peer, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    commit_body(
        vault,
        &seed.body_doc_id,
        peer,
        epoch,
        &doc,
        &head,
        "dev-sync-setup-body",
    );

    Note {
        doc_id: seed.body_doc_id,
        title: title.to_owned(),
        text: text.to_owned(),
    }
}

/// 把内存里的正文增量按 head CAS 落库，并**同事务**写一条出站意图（生产路径）。
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
    let update = doc.pending_update(&base).expect("导出正文增量失败");
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

// ---------------------------------------------------------------------------
// 1) Create：身份先落盘、库可打开、树为空
// ---------------------------------------------------------------------------

#[test]
fn create_persists_identity_and_leaves_an_empty_vault() {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("create");
    let vault_path = dir.path().join("inst-a").join("vault.sqlite3");
    let config = config_for(&relay, &vault_path);

    let outcome = setup::prepare(&config, SetupMode::Create, create_secrets(&relay))
        .expect("Create 必须成功");

    // 身份必须已经**受保护落盘**（不是只在内存里）。
    let identity_path = DevIdentityStore::path_for(&vault_path);
    let meta = std::fs::symlink_metadata(&identity_path).expect("Identity 文件必须已落盘");
    assert!(meta.file_type().is_file(), "身份文件必须是常规文件");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "身份文件必须 0600（只有当前用户可读写）"
        );
        assert_eq!(meta.nlink(), 1, "临时文件名必须被清掉（硬链接数回到 1）");
    }
    let persisted = DevIdentityStore
        .load(&vault_path)
        .expect("回读已落盘的身份");
    assert_eq!(
        persisted.loro_peer_id(),
        outcome.identity.loro_peer_id(),
        "磁盘上的身份必须就是本次 Create 得到的身份（peer 唯一来源）"
    );
    assert_eq!(persisted.device_id, outcome.identity.device_id);
    assert_eq!(persisted.vault_id, outcome.vault_id);

    // 库可以独立重开；Create 出来的库必须是**空树**（真实笔记由 UI/测试自己建）。
    assert_eq!(outcome.vault.path(), vault_path.as_path());
    let reopened = Vault::open(&vault_path).expect("Create 之后库必须可重新打开");
    assert!(
        list_tree(&reopened).expect("读取结构投影失败").is_empty(),
        "Create 不得预置任何笔记（占位身份/演示树都不该出现）"
    );
    // 真实启动路径会用 `open_vault_at_with_peer_opts(seed_demo=false)` 重开窗口；
    // 这一步曾错误地走默认 `open_vault_at_with_peer`，把 3 个 demo 节点播进新库。
    drop(reopened);
    let dev = menghunote_app::dev_vault::open_vault_at_with_peer_opts(
        &vault_path,
        persisted.loro_peer_id(),
        false,
    )
    .expect("Create 后按窗口路径重开库失败");
    assert!(!dev.seeded, "dev-sync 的 Create 重开窗口不得播种演示树");
    assert!(
        list_tree(&dev.vault).expect("读取结构投影失败").is_empty(),
        "走真实窗口重开路径后仍必须是空树（否则 demo 树会混进基线）"
    );
    assert!(
        outcome.recovery_code.is_some(),
        "Create 必须把一次性恢复码交回进程内（不打印，供后续展示）"
    );
}

// ---------------------------------------------------------------------------
// 2) A 建笔记 → 发布基线 → B 只凭 vault_id + 恢复码 Join 并读到正文
// ---------------------------------------------------------------------------

#[test]
fn join_with_only_vault_id_and_recovery_code_reads_the_same_body() {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("a-to-b");

    // A：Create（真实身份）→ 真实建一篇笔记。
    let a_path = dir.path().join("inst-a").join("vault.sqlite3");
    let a = setup::prepare(
        &config_for(&relay, &a_path),
        SetupMode::Create,
        create_secrets(&relay),
    )
    .expect("A Create 失败");
    let note = create_note(
        &a.vault,
        a.identity.loro_peer_id(),
        a.identity.current_epoch,
        "A 的笔记",
        "A 写的正文第一行\n",
    );
    let recovery_code = a
        .recovery_code
        .as_deref()
        .expect("Create 必须返回恢复码")
        .to_owned();
    let report =
        DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &recovery_code)
            .expect("A 发布初始基线失败");
    assert!(report.validated, "初始基线必须真实读回/导入通过");

    // B：只给 vault_id + 恢复码 + 新路径（没有 A 的目录/身份/root 输入）。
    let b_path = dir.path().join("inst-b").join("vault.sqlite3");
    let b = setup::prepare(
        &config_for(&relay, &b_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(ids::hex_of(&a.vault_id)),
            recovery_code: Some(recovery_code),
            ..SetupSecrets::default()
        },
    )
    .expect("B Join 失败");

    // B 的身份也必须独立落盘、peer 与 A 不同（ADR 0009 切片 2）。
    let persisted = DevIdentityStore.load(&b_path).expect("B 身份必须已落盘");
    assert_eq!(persisted.vault_id, a.vault_id);
    assert_eq!(persisted.loro_peer_id(), b.identity.loro_peer_id());
    assert_ne!(
        b.identity.loro_peer_id(),
        a.identity.loro_peer_id(),
        "B 必须是独立设备（独立 CRDT peer）"
    );
    assert!(
        b.identity.manager_seed.is_none(),
        "普通设备不得持有管理私钥"
    );

    // 打开 B 的库：结构投影与正文都必须来自基线，不需要再跑 SyncOnce。
    let tree = list_tree(&b.vault).expect("读取 B 的结构投影失败");
    let node = tree
        .iter()
        .find(|entry| entry.body_doc_id.as_deref() == Some(note.doc_id.as_str()))
        .expect("B 的结构投影必须含 A 的笔记节点");
    assert_eq!(node.title, note.title);
    let text = body_text(&b.vault, &note.doc_id, b.identity.loro_peer_id());
    assert!(
        text.contains(note.text.trim_end()),
        "B 打开后必须读到 A 的正文，实际 {text:?}"
    );
}

// ---------------------------------------------------------------------------
// 3) 普通模式：只 load，不创建；已有身份时逐字节不改写
// ---------------------------------------------------------------------------

#[test]
fn existing_mode_without_identity_fails_without_creating_anything() {
    ensure_test_user_key();
    let dir = temp_root("existing-missing");
    let vault_path = dir.path().join("inst-a").join("vault.sqlite3");
    let config = DevSyncConfig {
        vault: vault_path.clone(),
        service: ServiceConfig::new("http://127.0.0.1:18080"),
    };

    let error = setup_prepare_err(
        setup::prepare(&config, SetupMode::Existing, SetupSecrets::default()),
        "普通模式缺身份必须明确失败，不能自动创建",
    );
    let message = format!("{error:#}");
    assert!(
        message.contains(CREATE_FLAG) && message.contains(JOIN_FLAG),
        "错误必须提示 Create/Join 两种显式初始化方式：{message}"
    );
    assert!(!vault_path.exists(), "普通模式缺身份不得创建库");
    assert!(
        !DevIdentityStore::path_for(&vault_path).exists(),
        "普通模式缺身份不得创建身份文件"
    );
    assert!(
        !vault_path.parent().expect("父目录").exists(),
        "普通模式缺身份不得创建实例目录"
    );
}

#[test]
fn existing_mode_reuses_the_persisted_identity_without_rewriting_it() {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("existing-ok");
    let vault_path = dir.path().join("inst-a").join("vault.sqlite3");
    let config = config_for(&relay, &vault_path);
    let created =
        setup::prepare(&config, SetupMode::Create, create_secrets(&relay)).expect("Create 失败");

    let identity_path = DevIdentityStore::path_for(&vault_path);
    let before = std::fs::read(&identity_path).expect("读取身份文件");
    let reopened = setup::prepare(&config, SetupMode::Existing, SetupSecrets::default())
        .expect("普通模式必须复用已有身份");
    assert_eq!(
        reopened.identity.loro_peer_id(),
        created.identity.loro_peer_id(),
        "普通模式必须 load 到同一身份 peer"
    );
    assert_eq!(
        std::fs::read(&identity_path).expect("重开后再读"),
        before,
        "普通模式只 load，不得改写身份文件"
    );
    assert_eq!(reopened.vault_id, created.vault_id);
    assert!(reopened.recovery_code.is_none(), "普通模式没有恢复码可交回");
}

// ---------------------------------------------------------------------------
// 4) 错误材料：明确错误、不留下半初始化库/身份
// ---------------------------------------------------------------------------

#[test]
fn invalid_create_secret_fails_without_leftovers() {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("bad-secret");
    let vault_path = dir.path().join("inst-a").join("vault.sqlite3");
    let config = config_for(&relay, &vault_path);
    let wrong_secret = "definitely-not-the-setup-secret";

    let error = setup_prepare_err(
        setup::prepare(
            &config,
            SetupMode::Create,
            SetupSecrets {
                setup_secret: Some(wrong_secret.to_owned()),
                ..SetupSecrets::default()
            },
        ),
        "错的 setup secret 必须被 relay 拒绝",
    );
    let message = format!("{error:#}");
    assert!(
        !message.contains(wrong_secret),
        "错误不得回显 setup secret：{message}"
    );
    assert!(
        !vault_path.exists(),
        "失败不得留下半初始化 vault：{}",
        vault_path.display()
    );
    assert!(
        !DevIdentityStore::path_for(&vault_path).exists(),
        "失败不得留下身份文件"
    );
    assert!(
        !vault_path.parent().expect("父目录").exists(),
        "联网失败前不该创建实例目录"
    );

    // 缺 setup secret：在联网之前就明确失败（不创建任何东西）。
    let error = setup_prepare_err(
        setup::prepare(&config, SetupMode::Create, SetupSecrets::default()),
        "缺 setup secret 必须明确失败",
    );
    let message = format!("{error:#}");
    assert!(
        message.contains(setup::SETUP_SECRET_ENV),
        "错误必须点名环境变量：{message}"
    );
    assert!(!vault_path.exists());
}

#[test]
fn invalid_join_material_fails_without_leftovers() {
    ensure_test_user_key();
    let dir = temp_root("bad-join");
    let vault_path = dir.path().join("inst-b").join("vault.sqlite3");
    let config = DevSyncConfig {
        vault: vault_path.clone(),
        service: ServiceConfig::new("http://127.0.0.1:18080"),
    };
    let bad_code = "mnrec1not-a-real-recovery-code";

    // 格式非法：Bech32m 校验就失败，连网络都不该发、本地文件也不该出现。
    let error = setup_prepare_err(
        setup::prepare(
            &config,
            SetupMode::Join,
            SetupSecrets {
                vault_id: Some("00112233445566778899aabbccddeeff".to_owned()),
                recovery_code: Some(bad_code.to_owned()),
                ..SetupSecrets::default()
            },
        ),
        "非法恢复码必须失败",
    );
    let message = format!("{error:#}");
    assert!(!message.contains(bad_code), "错误不得回显恢复码：{message}");
    assert!(!vault_path.exists(), "失败不得留下半初始化 B 库");
    assert!(!DevIdentityStore::path_for(&vault_path).exists());

    // 缺 vault id / 缺恢复码：分别点名环境变量，不触发网络。
    let error = setup_prepare_err(
        setup::prepare(
            &config,
            SetupMode::Join,
            SetupSecrets {
                recovery_code: Some(bad_code.to_owned()),
                ..SetupSecrets::default()
            },
        ),
        "缺 vault id 必须明确失败",
    );
    assert!(format!("{error:#}").contains(setup::VAULT_ID_ENV));

    let error = setup_prepare_err(
        setup::prepare(
            &config,
            SetupMode::Join,
            SetupSecrets {
                vault_id: Some("00112233445566778899aabbccddeeff".to_owned()),
                ..SetupSecrets::default()
            },
        ),
        "缺恢复码必须明确失败",
    );
    assert!(format!("{error:#}").contains(setup::RECOVERY_CODE_ENV));
}

// ---------------------------------------------------------------------------
// 5) 切片 4c-3：手工演示交付（Create 一次性交接块 / info / publish）
// ---------------------------------------------------------------------------

/// 模拟操作者从 Create 的 `[DEV ONLY]` 块里复制值：取出 `key=value` 的 value。
///
/// 这里**只解析块的结构**（行前缀 + key），不比对字节级排版；块本身由
/// `dev_sync::cli` 的单测固定。
fn field_from_block<'a>(block: &'a str, key: &str) -> &'a str {
    for line in block.lines() {
        if let Some(rest) = line
            .strip_prefix("[DEV ONLY] ")
            .and_then(|rest| rest.strip_prefix(key))
            .and_then(|rest| rest.strip_prefix('='))
        {
            return rest;
        }
    }
    panic!("Create 块里缺 {key}：{block}");
}

/// 实例身份文件的字节数（用于断言"失败不修改身份/库"）。
fn identity_bytes(vault_path: &Path) -> Vec<u8> {
    std::fs::read(DevIdentityStore::path_for(vault_path)).expect("读取已落盘身份失败")
}

/// A is created (and has a note), then the CLI publish helper is exercised with the
/// operator-visible handoff block: parse it back, join B with those exact values.
#[test]
fn handoff_block_from_create_can_be_pasted_into_join_after_cli_publish() {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("handoff");
    let a_path = dir.path().join("inst-a").join("vault.sqlite3");
    let a_config = config_for(&relay, &a_path);

    let a = setup::prepare(&a_config, SetupMode::Create, create_secrets(&relay))
        .expect("A Create 失败");
    let note = create_note(
        &a.vault,
        a.identity.loro_peer_id(),
        a.identity.current_epoch,
        "A 的笔记",
        "切片 4c-3 的手工演示正文\n",
    );
    let recovery_code = a
        .recovery_code
        .as_deref()
        .expect("Create 必须返回恢复码")
        .to_owned();

    // 操作者看到的块：SPDX 3 行，vault_id + 恢复码都在里面（且不再复述 setup secret）。
    let block = cli::create_handoff_block(&a.vault_id, &recovery_code);
    assert!(block.contains(&ids::hex_of(&a.vault_id)));
    assert!(block.contains(&recovery_code));
    assert!(
        !block.contains(relay.setup_secret()),
        "块里不得出现 setup secret"
    );

    // 文档要求：发布基线时 app 已关闭 → 先丢掉 A 的库句柄再跑 CLI helper。
    drop(a.vault);

    let summary = cli::publish_baseline(&a_config, Some(recovery_code.clone()))
        .expect("CLI 发布初始基线必须成功");
    assert!(summary.notes >= 1, "发布时至少要看到 A 建的那篇笔记");
    assert!(summary.published_at > 0, "checkpoint 必须有发布时间");
    assert!(!cli::publish_block(&summary).contains(&recovery_code));

    // 只凭 Create 块里"复制出来的两个值"join B（不读 A 目录/身份/root）。
    let b_path = dir.path().join("inst-b").join("vault.sqlite3");
    let b = setup::prepare(
        &config_for(&relay, &b_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(field_from_block(&block, "vault_id").to_owned()),
            recovery_code: Some(field_from_block(&block, "recovery_code").to_owned()),
            ..SetupSecrets::default()
        },
    )
    .expect("B 用 Create 块里的值 Join 失败");

    let text = body_text(&b.vault, &note.doc_id, b.identity.loro_peer_id());
    assert!(
        text.contains(note.text.trim_end()),
        "B 必须读到 A 的正文，实际 {text:?}"
    );
}

/// `--dev-sync-info` 不联网：把 relay 指到一个不可达端口，仍然必须打印公开标识。
#[test]
fn info_reads_identity_and_vault_without_network() {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("info");
    let vault_path = dir.path().join("inst-a").join("vault.sqlite3");
    let config = config_for(&relay, &vault_path);
    let created =
        setup::prepare(&config, SetupMode::Create, create_secrets(&relay)).expect("Create 失败");
    let identity = DevIdentityStore.load(&vault_path).expect("回读身份");

    // info 的语义是"只读已有实例"：relay 地址根本不参与（这里故意不可达）。
    let offline_config = DevSyncConfig {
        vault: vault_path.clone(),
        service: ServiceConfig::new("http://127.0.0.1:1"),
    };
    let info = cli::inspect(&offline_config).expect("info 不得依赖 relay");
    assert_eq!(info.vault_id, created.vault_id);
    assert_eq!(info.device_id, identity.device_id);
    assert_eq!(info.peer, identity.loro_peer_id());

    let block = cli::info_block(&info);
    assert!(block.contains(&ids::hex_of(&created.vault_id)));
    assert!(block.contains(&ids::hex_of(&identity.device_id)));
    assert!(block.contains(&format!("peer={}", identity.loro_peer_id())));
    // info 不打印恢复码/root/秘密。
    if let Some(recovery_code) = created.recovery_code.as_deref() {
        assert!(
            !block.contains(recovery_code),
            "info 不得回显恢复码：{block}"
        );
    }
    assert!(
        !block.contains("manager"),
        "info 不得打印管理私钥字段：{block}"
    );
}

/// `--dev-sync-publish`：缺码/错码 → 明确错误且库/身份不变；正确码 → 发布成功。
#[test]
fn publish_cli_requires_matching_recovery_code_and_keeps_the_vault_unchanged() {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("cli-publish");
    let a_path = dir.path().join("inst-a").join("vault.sqlite3");
    let config = config_for(&relay, &a_path);

    let a =
        setup::prepare(&config, SetupMode::Create, create_secrets(&relay)).expect("A Create 失败");
    let note = create_note(
        &a.vault,
        a.identity.loro_peer_id(),
        a.identity.current_epoch,
        "A 的笔记",
        "发布失败不得改动库\n",
    );
    let before_identity = identity_bytes(&a_path);
    assert!(
        DevLocal::published_baseline(&a.vault)
            .expect("读基线事实失败")
            .is_none(),
        "发布之前不应有基线和事实"
    );
    drop(a.vault);

    // 缺恢复码：点名环境变量，完全不碰库（连打开都不需要）。
    let error = cli::publish_baseline(&config, None).expect_err("缺恢复码必须失败");
    let message = format!("{error:#}");
    assert!(
        message.contains(setup::RECOVERY_CODE_ENV),
        "错误必须点名环境变量：{message}"
    );
    assert_eq!(identity_bytes(&a_path), before_identity, "缺码不得改写身份");

    // 格式合法但内容错误的恢复码：本地一致性校验失败，且不回显恢复码。
    let wrong_code = RecoverySeed::generate(&mut OsRandom)
        .expect("生成假恢复码种子失败")
        .to_code()
        .expect("编码假恢复码失败")
        .as_str()
        .to_owned();
    let error =
        cli::publish_baseline(&config, Some(wrong_code.clone())).expect_err("错恢复码必须失败");
    let message = format!("{error:#}");
    assert!(
        message.contains("不匹配"),
        "错码必须报一致性不匹配：{message}"
    );
    assert!(
        !message.contains(&wrong_code),
        "错误不得回显恢复码：{message}"
    );
    assert_eq!(identity_bytes(&a_path), before_identity, "错码不得改写身份");

    // 库本身也保持"没有基线事实/正文仍在"。
    let unchanged = Vault::open(&a_path).expect("重开库失败");
    assert!(
        DevLocal::published_baseline(&unchanged)
            .expect("读基线事实失败")
            .is_none(),
        "失败的发布不得写入基线事实"
    );
    assert!(
        body_text(&unchanged, &note.doc_id, a.identity.loro_peer_id())
            .contains("发布失败不得改动库")
    );
    drop(unchanged);

    // 正确恢复码：发布成功，返回的都是公开事实。
    let recovery_code = a
        .recovery_code
        .as_deref()
        .expect("Create 必须返回恢复码")
        .to_owned();
    let summary = cli::publish_baseline(&config, Some(recovery_code.clone()))
        .expect("正确恢复码必须发布成功");
    assert!(summary.notes >= 1);
    assert!(summary.covered_seq > 0, "发布必须覆盖真实水位");
    let block = cli::publish_block(&summary);
    assert!(block.contains(&ids::hex_of(&summary.checkpoint_id)));
    assert!(!block.contains(&recovery_code), "发布输出不得回显恢复码");

    // 发布后 B 可以 join 并读到 A 的笔记。
    let b_path = dir.path().join("inst-b").join("vault.sqlite3");
    let b = setup::prepare(
        &config_for(&relay, &b_path),
        SetupMode::Join,
        SetupSecrets {
            vault_id: Some(ids::hex_of(&a.vault_id)),
            recovery_code: Some(recovery_code),
            ..SetupSecrets::default()
        },
    )
    .expect("发布之后 B Join 必须成功");
    assert!(
        body_text(&b.vault, &note.doc_id, b.identity.loro_peer_id()).contains("发布失败不得改动库")
    );
}
