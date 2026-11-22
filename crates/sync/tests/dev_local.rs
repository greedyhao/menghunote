//! ADR 0009 切片 4b：`DevLocal`（Create / 发布初始基线 / Join / SyncOnce）的集成测试。
//!
//! 用 `menghunote-dev-relay`：进程内真实 axum relay + 临时 SQLite + `FakeClock` +
//! `127.0.0.1:0` 端口，客户端走真实 HTTP（`StdTcpTransport::new_local_dev`），不是 FakeRelay。
//!
//! ⚠️ **非生产**：dev-relay 的 `DevPermissiveHook` 对任何签名**一律放行**。本文件只验证
//! dev-local 编排与数据面（A/B 两个独立 vault 的真实同步），**不能**作为鉴权/验签验收；
//! 那些负例必须走 relay 生产路径的 `Ed25519Hook`。
//!
//! 纪律（AGENTS.md §6）：临时目录一律在 `CARGO_TARGET_TMPDIR`；收敛点只用显式返回值
//! （`sync_once` 的 durable ACK/integrate 结果、`bootstrap` 返回），**不用 sleep**。

use std::path::Path;

use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    append_updates_with_sync_intents, count_inbox_ready, list_tree, load_body_doc, load_body_state,
    load_head, load_structure, local_sync_intent, sync_cursor_state, BodyDoc, HeadState,
    StructureWriter, SyncContext, UpdateRecord, Vault, STRUCTURE_DOC_ID,
};
use menghunote_sync::dev_local::{
    CreatedVault, DevLocal, JoinedVault, INITIAL_RECOVERY_SLOT_META, INITIAL_SLOT_UPLOADED_META,
    PUBLISHED_BASELINE_META,
};
use menghunote_sync::engine::{IntegrateConfig, RELAY_CURSOR_META};
use menghunote_sync::random::OsRandom;
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::SyncError;
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 在 `target/tmp` 下建一个独立测试根目录（**不用** `%TEMP%`）。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dev-local-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 一篇笔记的稳定标识（结构节点 + 正文文档 + 写入的文字）。
struct Note {
    doc_id: String,
    title: String,
    text: String,
}

/// 走真实 storage 路径建一篇笔记：结构 CRDT（含 sync intent）→ 投影 → 正文 A 事务
/// （含 sync intent）。peer/epoch 必须来自设备身份（ADR 0009 切片 2b）。
fn create_note(vault: &Vault, peer: u64, epoch: u64, title: &str, text: &str) -> Note {
    let ctx = SyncContext::new(peer, epoch);
    let mut writer =
        StructureWriter::open_with_sync(vault, peer, Some(ctx)).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer.commit("dev-local-note").expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, peer, epoch, text);
    Note {
        doc_id: seed.body_doc_id,
        title: title.to_owned(),
        text: text.to_owned(),
    }
}

/// 首次写入正文（`loro_docs` 行由结构投影建好，因此走 `load_body_doc` 而不是裸 INSERT）。
fn write_body(vault: &Vault, doc_id: &str, peer: u64, epoch: u64, text: &str) {
    let (doc, head) = load_body_doc(vault, doc_id, peer, None).expect("加载刚投影出的正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    commit_body(vault, doc_id, peer, epoch, &doc, &head, "dev-local-body");
}

/// 在正文末尾追加一段（同一 A 事务 + intent），模拟 A/B 的继续编辑。
fn append_body(vault: &Vault, doc_id: &str, peer: u64, epoch: u64, suffix: &str) {
    let (doc, head) = load_body_doc(vault, doc_id, peer, None).expect("加载正文失败");
    let pos = doc.scalar_len();
    doc.insert_scalars(pos, suffix).expect("追加正文失败");
    commit_body(vault, doc_id, peer, epoch, &doc, &head, "dev-local-append");
}

/// 把内存里的文档增量按 head CAS 落库，并**同事务**写一条出站意图。
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

/// 读正文（`peer` 只影响本地 seq 分配的 peer，不影响已 durable 的内容）。
fn body_text(vault: &Vault, doc_id: &str, peer: u64) -> String {
    load_body_doc(vault, doc_id, peer, None)
        .expect("加载正文失败")
        .0
        .text()
}

/// A 发布初始基线后、用 `vault_id + 恢复码 + 新路径` 加入的 B。
struct Fixture {
    relay: DevRelay,
    _dir: TempDir,
    a: CreatedVault,
    b: JoinedVault,
    note: Note,
}

/// 建 A（含一篇笔记）→ 发布初始基线 → B 用恢复码 join → B `sync_once`。
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
        "A 的笔记",
        "A 写的第一行\n",
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
// 1) A create → 真实笔记 → 发布基线 → B 只凭 vault_id + 恢复码看到笔记
// ---------------------------------------------------------------------------

#[test]
fn a_create_publish_b_join_sees_note() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("create-publish-join");
    let a_path = dir.path().join("a").join("vault.sqlite3");

    // Create：只给 relay + setup secret + 新路径 + 标签。
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A 实例")
        .expect("A create 失败");
    assert_eq!(a.vault_id, a.identity.vault_id);
    assert_eq!(a.setup_secret, relay.setup_secret());
    assert!(
        a.recovery_code.starts_with("mnrec1"),
        "恢复码必须是 Bech32m mnrec1...，实际前缀 {:?}",
        a.recovery_code.get(..6)
    );
    assert_eq!(
        DevLocal::vault_label(&a.vault)
            .expect("读标签失败")
            .as_deref(),
        Some("A 实例")
    );
    // create 必须把 relay 游标与日志代落成 durable 事实（否则发布/同步无从起步）。
    assert!(
        a.vault
            .get_meta(RELAY_CURSOR_META)
            .expect("读游标失败")
            .is_some(),
        "create 后必须已有 relay 游标"
    );
    assert!(
        a.vault
            .get_meta(INITIAL_RECOVERY_SLOT_META)
            .expect("读初始槽记录失败")
            .is_some(),
        "create 必须留下 genesis recovery 槽材料（供发布基线复用）"
    );

    // 真实写入：结构 CRDT（含 intent）+ 正文 A 事务（含 intent）。
    let note = create_note(
        &a.vault,
        a.identity.loro_peer_id(),
        a.identity.current_epoch,
        "来自 A 的笔记",
        "A 写的正文第一行\n",
    );

    // 发布初始基线（内部先跑一轮完整 SyncOnce，再发两份真实 checkpoint）。
    let report =
        DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
            .expect("A 发布初始基线失败");
    assert!(
        report.validated,
        "checkpoint 必须真实读回/导入通过 validate"
    );
    assert!(report.published_at.is_some(), "promote 后必须已发布");
    assert!(
        report.docs.iter().any(|doc| doc.doc_id == note.doc_id),
        "checkpoint 的文档快照必须覆盖真实笔记正文，实际文档 {:?}",
        report
            .docs
            .iter()
            .map(|doc| &doc.doc_id)
            .collect::<Vec<_>>()
    );
    assert!(
        report.docs.iter().any(|doc| doc.kind == 0),
        "checkpoint 必须含 structure 文档"
    );
    // relay 侧事实：返回的这份 checkpoint 必须真的 published（不是本地自报）。
    let row = relay
        .checkpoint(report.checkpoint_id)
        .expect("读 relay checkpoint 行失败");
    assert_eq!(row.state.as_str(), "published");
    assert!(row.published_at.is_some());
    assert_eq!(row.covered_seq, report.covered_seq);
    assert!(
        relay
            .checkpoint_ref_count(report.checkpoint_id)
            .expect("读 relay checkpoint refs 失败")
            > 0,
        "published 的 checkpoint 必须有 refs（另外一份 pair 同理）"
    );

    // "checkpoint/cursor 事实"必须落成 durable meta。
    let baseline = DevLocal::published_baseline(&a.vault)
        .expect("读已发布基线失败")
        .expect("发布后必须留下 durable 基线事实");
    assert_eq!(baseline.checkpoint_id, report.checkpoint_id);
    assert_eq!(baseline.epoch, report.epoch);
    assert_eq!(baseline.covered_seq, report.covered_seq);
    assert_eq!(baseline.published_at, report.published_at.unwrap_or(0));
    assert!(a
        .vault
        .get_meta(PUBLISHED_BASELINE_META)
        .expect("读基线 meta 失败")
        .is_some());

    // Join：B 只拿到 vault_id + 恢复码 + 新路径（没有任何 A 的目录/身份输入）。
    // `/bootstrap` 走的是**非降级**路径（至少两份 healthy published 才成功），所以
    // 这一步成功同时证明发布的是真正的一对 checkpoint。
    let b_path = dir.path().join("b").join("vault.sqlite3");
    let b = DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path)
        .expect("B join 失败");
    assert_eq!(b.vault_id, a.vault_id);
    assert_eq!(b.vault.path(), b_path.as_path());
    assert_ne!(
        b.identity.device_id, a.identity.device_id,
        "B 必须是独立设备"
    );
    assert_ne!(
        b.identity.loro_peer_id(),
        a.identity.loro_peer_id(),
        "B 的 Loro peer 必须与 A 不同（ADR 0009 切片 2）"
    );
    assert!(
        b.identity.manager_seed.is_none(),
        "普通设备不得持有管理私钥"
    );
    // 信任锚来自恢复码本身，不是读 A 的目录。
    assert_eq!(b.identity.pinned_manager, a.identity.pinned_manager);
    assert_eq!(b.identity.pinned_instance, a.identity.pinned_instance);

    // B 首轮同步（游标由 bootstrap 落库，pull 从基线 S 之后继续）。
    let sync =
        DevLocal::sync_once(relay.base_url(), &b.vault, &b.identity).expect("B sync_once 失败");
    assert_eq!(sync.push.failed, 0, "B 没有任何待推送项，不该有失败");
    assert_eq!(sync.integrate.deferred, 0);

    // B 的库/正文必须含 A 的笔记（按 doc id + 标题 + 正文，不只看某一列）。
    let tree = list_tree(&b.vault).expect("读 B 的结构投影失败");
    let node = tree
        .iter()
        .find(|entry| entry.body_doc_id.as_deref() == Some(note.doc_id.as_str()))
        .expect("B 的结构投影必须含 A 的笔记节点");
    assert_eq!(node.title, note.title);
    let text = body_text(&b.vault, &note.doc_id, b.identity.loro_peer_id());
    assert!(
        text.contains(note.text.trim_end()),
        "B 的正文必须含 A 的原文，实际 {text:?}"
    );
}

// ---------------------------------------------------------------------------
// 2) 双向同步：B 编辑 → A 看到；A 再编辑 → B 看到
// ---------------------------------------------------------------------------

#[test]
fn bidirectional_sync() {
    let fixture = establish("bidirectional");
    let a_peer = fixture.a.identity.loro_peer_id();
    let b_peer = fixture.b.identity.loro_peer_id();
    let a_epoch = fixture.a.identity.current_epoch;
    let b_epoch = fixture.b.identity.current_epoch;
    let doc_id = fixture.note.doc_id.clone();
    let relay_url = fixture.relay.base_url().to_owned();

    // B 编辑同一篇笔记（同一 A 事务 + intent，epoch 来自 B 自己的身份）。
    let b_line = "B 追加的一行（双向同步）\n";
    append_body(&fixture.b.vault, &doc_id, b_peer, b_epoch, b_line);
    let b_push = DevLocal::sync_once(&relay_url, &fixture.b.vault, &fixture.b.identity)
        .expect("B 推送编辑失败");
    assert_eq!(b_push.push.failed, 0);
    assert!(b_push.push.acked >= 1, "B 的编辑必须拿到 durable ACK");

    let a_pull = DevLocal::sync_once(&relay_url, &fixture.a.vault, &fixture.a.identity)
        .expect("A 拉取 B 的编辑失败");
    assert!(
        a_pull.integrate.integrated >= 1,
        "A 必须真的 integrate 了 B 的更新"
    );
    let a_text = body_text(&fixture.a.vault, &doc_id, a_peer);
    assert!(
        a_text.contains(b_line.trim_end()),
        "A 必须看到 B 的文字，实际 {a_text:?}"
    );

    // 反向：A 再编辑，B 同步后必须看到。
    let a_line = "A 再追加的一行（反向）\n";
    append_body(&fixture.a.vault, &doc_id, a_peer, a_epoch, a_line);
    let a_push = DevLocal::sync_once(&relay_url, &fixture.a.vault, &fixture.a.identity)
        .expect("A 推送编辑失败");
    assert!(a_push.push.acked >= 1);
    let b_pull = DevLocal::sync_once(&relay_url, &fixture.b.vault, &fixture.b.identity)
        .expect("B 拉取 A 的编辑失败");
    assert!(b_pull.integrate.integrated >= 1);
    let b_text = body_text(&fixture.b.vault, &doc_id, b_peer);
    assert!(
        b_text.contains(a_line.trim_end()),
        "B 必须看到 A 的文字，实际 {b_text:?}"
    );
    // 两边的文字仍是同一份（A 的第一行 + 两段追加）。
    assert!(b_text.contains(fixture.note.text.trim_end()));
    assert!(b_text.contains(b_line.trim_end()));
}

// ---------------------------------------------------------------------------
// 3) 错误路径：错 setup secret / 错恢复码 → 明确错误且不留下半初始化 vault
// ---------------------------------------------------------------------------

#[test]
fn create_rejects_bad_setup_secret() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("bad-setup-secret");
    let path = dir.path().join("a").join("vault.sqlite3");

    let error = match DevLocal::create(relay.base_url(), "definitely-not-the-secret", &path, "A") {
        Ok(_) => panic!("错的 setup secret 必须被 relay 拒绝"),
        Err(error) => error,
    };
    match &error {
        // dev relay 对错 secret 返回 403 + CBOR 错误体（REVOKED）：必须映射成可读的
        // RelayApi，而不是无信息的 HttpStatus。
        SyncError::RelayApi { code, .. } => assert_eq!(code, "REVOKED"),
        other => panic!("期望 relay 明确拒绝，实际 {other}"),
    }
    assert!(!path.exists(), "失败不得留下半初始化 vault");
    let mut wal = path.clone().into_os_string();
    wal.push("-wal");
    assert!(
        !Path::new(&wal).exists(),
        "失败不得留下 WAL 边车：{}",
        Path::new(&wal).display()
    );
}

#[test]
fn join_rejects_bad_recovery() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("bad-recovery");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A 实例")
        .expect("A create 失败");
    let note = create_note(
        &a.vault,
        a.identity.loro_peer_id(),
        a.identity.current_epoch,
        "A 的笔记",
        "A 的正文\n",
    );
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("A 发布初始基线失败");
    let b_path = dir.path().join("b").join("vault.sqlite3");

    // (a) 格式非法：Bech32m 校验就失败，连网络都不该发。
    let error = match DevLocal::join(relay.base_url(), &a.vault_id, "mnrec1not-a-code", &b_path) {
        Ok(_) => panic!("非法恢复码必须失败"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SyncError::RecoveryCode(_)),
        "非法恢复码必须报 RecoveryCode，实际 {error}"
    );
    assert!(!b_path.exists(), "非法恢复码不得创建本地库");

    // (b) 格式合法但内容错误：管理私钥对不上，relay 必须拒绝（在任何本地落库之前）。
    let wrong_seed = RecoverySeed::generate(&mut OsRandom).expect("生成假恢复码种子失败");
    let wrong_code = wrong_seed.to_code().expect("编码假恢复码失败");
    assert_ne!(wrong_code.as_str(), a.recovery_code);
    let error = match DevLocal::join(relay.base_url(), &a.vault_id, wrong_code.as_str(), &b_path) {
        Ok(_) => panic!("错恢复码必须失败"),
        Err(error) => error,
    };
    assert!(
        !matches!(error, SyncError::RecoveryCode(_)),
        "格式合法但内容错误的恢复码应由 relay 认证拒绝，实际 {error}"
    );
    assert!(!b_path.exists(), "失败不得留下半初始化 B 库");

    // A 侧事实没有被破坏：A 仍能正常同步，笔记仍在。
    DevLocal::sync_once(relay.base_url(), &a.vault, &a.identity).expect("A 在负例后仍能同步");
    assert!(body_text(&a.vault, &note.doc_id, a.identity.loro_peer_id()).contains("A 的正文"));
}

// ---------------------------------------------------------------------------
// 4) 空 vault 也能发布 pair（0 资产/空 structure），并能用于 Join
// ---------------------------------------------------------------------------

#[test]
fn publish_without_data() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("empty-publish");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "空实例")
        .expect("A create 失败");

    let report =
        DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
            .expect("空 vault 也必须能发布初始基线");
    assert!(report.validated);
    assert_eq!(report.covered_seq, 0, "空库的 S 必须是 0");
    assert!(report.assets.is_empty(), "空库不得凭空造资产");
    assert!(
        report.docs.iter().all(|doc| doc.kind == 0),
        "空库只应有 structure 文档快照"
    );
    assert_eq!(
        DevLocal::published_baseline(&a.vault)
            .expect("读基线事实失败")
            .map(|baseline| baseline.covered_seq),
        Some(0)
    );

    // 空基线也要能 Join：structure 文档被导入，笔记树为空。
    let b_path = dir.path().join("b").join("vault.sqlite3");
    let b = DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path)
        .expect("空基线也必须能 Join");
    assert!(list_tree(&b.vault).expect("读 B 结构投影失败").is_empty());
    let (structure, _head) =
        load_structure(&b.vault, b.identity.loro_peer_id()).expect("B 必须能打开结构文档");
    assert!(
        !structure
            .export_snapshot()
            .expect("导出结构快照失败")
            .is_empty(),
        "B 的结构文档必须可读（基线导入成功）"
    );
    let sync = DevLocal::sync_once(relay.base_url(), &b.vault, &b.identity)
        .expect("空基线之后的第一轮同步失败");
    assert_eq!(sync.pull.events, 0, "空库里没有可拉的日志事件");
    assert_eq!(sync.integrate.integrated, 0);
}

// ---------------------------------------------------------------------------
// 附属断言：create/join 只接受新路径（不覆盖已有库）
// ---------------------------------------------------------------------------

#[test]
fn create_and_join_require_fresh_paths() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("fresh-paths");
    let path = dir.path().join("a").join("vault.sqlite3");
    let created = DevLocal::create(relay.base_url(), relay.setup_secret(), &path, "A")
        .expect("A create 失败");

    let error = match DevLocal::create(relay.base_url(), relay.setup_secret(), &path, "A again") {
        Ok(_) => panic!("已存在的路径不得被 create 覆盖"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SyncError::LocalCorrupt(_)),
        "已存在路径必须是明确的本地错误，实际 {error}"
    );
    // 原库与标签没有被破坏。
    assert_eq!(
        DevLocal::vault_label(&created.vault)
            .expect("读标签失败")
            .as_deref(),
        Some("A")
    );
    assert!(list_tree(&created.vault)
        .expect("读结构投影失败")
        .is_empty());

    let error = match DevLocal::join(
        relay.base_url(),
        &created.vault_id,
        &created.recovery_code,
        &path,
    ) {
        Ok(_) => panic!("已存在的路径不得被 join 覆盖"),
        Err(error) => error,
    };
    assert!(matches!(error, SyncError::LocalCorrupt(_)));
}

// ---------------------------------------------------------------------------
// 5) 重复发布 / 错恢复码发布：可重跑且错码不污染 KEY_SLOT
// ---------------------------------------------------------------------------

#[test]
fn publish_is_repeatable() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("repeat-publish");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A 实例")
        .expect("A create 失败");
    let note = create_note(
        &a.vault,
        a.identity.loro_peer_id(),
        a.identity.current_epoch,
        "A 的笔记",
        "A 的正文\n",
    );

    let first =
        DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
            .expect("第一次发布失败");
    // 第二次必须成功：KEY_SLOT 对象用确定性 message id 派生，重传逐字节相同 →
    // relay 幂等接受（随机 id 会在这里 409 ID_CONFLICT）。
    let second =
        DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
            .expect("重跑发布基线必须可重试");
    assert!(second.validated);
    assert_ne!(first.checkpoint_id, second.checkpoint_id);
    assert_eq!(
        DevLocal::published_baseline(&a.vault)
            .expect("读基线事实失败")
            .map(|baseline| baseline.checkpoint_id),
        Some(second.checkpoint_id),
        "durable 基线事实必须指向较新的那份"
    );
    // 两次发布之后 Join 仍然可用（最新的两份 published 仍是可用基线）。
    let b_path = dir.path().join("b").join("vault.sqlite3");
    let b = DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path)
        .expect("重复发布之后 join 仍须成功");
    assert!(body_text(&b.vault, &note.doc_id, b.identity.loro_peer_id()).contains("A 的正文"));
}

#[test]
fn publish_rejects_mismatched_recovery_code() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("publish-bad-code");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A 实例")
        .expect("A create 失败");
    create_note(
        &a.vault,
        a.identity.loro_peer_id(),
        a.identity.current_epoch,
        "A 的笔记",
        "A 的正文\n",
    );

    // 格式合法但内容错误的恢复码：必须在**任何上传之前**被拦下（否则会用错的
    // K_slot_mne 加密 KEY_SLOT 对象，把初始槽写坏且无法补救）。
    let wrong_seed = RecoverySeed::generate(&mut OsRandom).expect("生成假恢复码种子失败");
    let wrong_code = wrong_seed.to_code().expect("编码假恢复码失败");
    let error = match DevLocal::publish_initial_pair(
        relay.base_url(),
        &a.vault,
        &a.identity,
        wrong_code.as_str(),
    ) {
        Ok(_) => panic!("错恢复码不得发布基线"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SyncError::RecoveryCode(_)),
        "错恢复码必须在本地一致性校验处失败，实际 {error}"
    );

    // 正确恢复码随后仍能成功发布，且 Join 可用（错码没有污染初始槽）。
    let report =
        DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
            .expect("错码之后真码发布必须仍成功");
    assert!(report.validated);
    let b_path = dir.path().join("b").join("vault.sqlite3");
    DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path)
        .expect("错码尝试之后 join 必须仍成功");
}

// ---------------------------------------------------------------------------
// 6) ADR 0009 切片 4c-4：只凭受保护身份发布（面板「发布基线」的库入口）
// ---------------------------------------------------------------------------

/// `create` 必须在上传初始 recovery 槽之后落 durable 标记，且只凭身份就能发布。
///
/// 这是"面板发布不需要恢复码"的两个前提：
/// 1. 槽在 Create（恢复码还在内存里）就已上传；
/// 2. 标记让只凭身份发布能 fail-closed 地判断"槽真的传过"。
#[test]
fn create_uploads_the_initial_slot_and_identity_can_publish_without_a_recovery_code() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("publish-identity");
    let a_path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &a_path, "A 实例")
        .expect("A create 失败");

    // Create 之后：槽材料 + 上传标记都必须是 durable 事实（否则面板发布会 fail-closed）。
    assert!(
        a.vault
            .get_meta(INITIAL_RECOVERY_SLOT_META)
            .expect("读初始槽记录失败")
            .is_some(),
        "create 必须留下 genesis recovery 槽材料"
    );
    assert!(
        a.vault
            .get_meta(INITIAL_SLOT_UPLOADED_META)
            .expect("读初始槽上传记录失败")
            .is_some(),
        "create 必须在恢复码还在内存里时上传初始槽并落标记"
    );

    let note = create_note(
        &a.vault,
        a.identity.loro_peer_id(),
        a.identity.current_epoch,
        "面板发布的笔记",
        "面板发布路径的正文\n",
    );

    // 关键：**不传恢复码**，只给身份 + 库。
    let report =
        DevLocal::publish_initial_pair_with_identity(relay.base_url(), &a.vault, &a.identity)
            .expect("只凭身份发布必须成功（面板路径不允许再要恢复码）");
    assert!(
        report.validated,
        "checkpoint 必须真实读回/导入通过 validate"
    );
    assert!(report.published_at.is_some(), "promote 后必须已发布");
    assert!(
        report.docs.iter().any(|doc| doc.doc_id == note.doc_id),
        "checkpoint 必须覆盖真实笔记正文"
    );
    let baseline = DevLocal::published_baseline(&a.vault)
        .expect("读已发布基线失败")
        .expect("发布后必须留下 durable 基线事实");
    assert_eq!(baseline.checkpoint_id, report.checkpoint_id);
    assert_eq!(baseline.covered_seq, report.covered_seq);

    // B 只凭 vault_id + 恢复码 Join：证明"没有恢复码的发布"对 B 完全等价。
    let b_path = dir.path().join("b").join("vault.sqlite3");
    let b = DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &b_path)
        .expect("只凭身份发布之后 B join 必须成功");
    assert!(
        body_text(&b.vault, &note.doc_id, b.identity.loro_peer_id()).contains("面板发布路径的正文"),
        "B 必须读到 A 的正文"
    );

    // 重跑：沿用 4b 幂等语义（多一对 checkpoint，不破坏已有基线；Join 仍可用）。
    let second =
        DevLocal::publish_initial_pair_with_identity(relay.base_url(), &a.vault, &a.identity)
            .expect("重跑只凭身份发布必须可重试");
    assert_ne!(
        report.checkpoint_id, second.checkpoint_id,
        "两次发布必须是不同 checkpoint"
    );
    assert_eq!(
        DevLocal::published_baseline(&a.vault)
            .expect("读基线事实失败")
            .map(|fact| fact.checkpoint_id),
        Some(second.checkpoint_id),
        "durable 基线事实必须指向较新的那份"
    );
    let c_path = dir.path().join("c").join("vault.sqlite3");
    let c = DevLocal::join(relay.base_url(), &a.vault_id, &a.recovery_code, &c_path)
        .expect("重跑发布之后 join 仍须成功");
    assert!(
        body_text(&c.vault, &note.doc_id, c.identity.loro_peer_id()).contains("面板发布路径的正文")
    );
}

/// Join 设备（没有管理私钥）不能只凭身份发布：明确拒绝，且不产生任何本地/relay 改动。
#[test]
fn publish_with_identity_refuses_a_join_device_without_a_manager_key() {
    let fixture = establish("publish-identity-refuses-join-device");
    assert!(
        fixture.b.identity.manager_seed.is_none(),
        "Join 设备必须没有管理私钥"
    );

    let error = match DevLocal::publish_initial_pair_with_identity(
        fixture.relay.base_url(),
        &fixture.b.vault,
        &fixture.b.identity,
    ) {
        Ok(_) => panic!("Join 设备不得发布初始基线"),
        Err(error) => error,
    };
    assert!(
        matches!(error, SyncError::Unsupported(_)),
        "必须是明确的 Unsupported（不是网络/损坏错误），实际 {error}"
    );
    assert!(
        error.to_string().contains("管理密钥"),
        "错误必须说清缺管理密钥：{error}"
    );
    // 失败不产生本地写入：B 的库仍没有"已发布基线"事实。
    assert!(
        DevLocal::published_baseline(&fixture.b.vault)
            .expect("读基线事实失败")
            .is_none(),
        "失败的发布不得留下 durable 基线事实"
    );
}

// ---------------------------------------------------------------------------
// 4) ADR 0012 S2b：两阶段 Integrate 的结构→正文依赖收敛 / 批次预算 / 水位 / 对账
// ---------------------------------------------------------------------------

/// B 的活动文档（先序第一篇）与它的 durable head：两阶段 Integrate 的授权基准。
fn b_watch_doc(fixture: &Fixture) -> (String, u64) {
    let doc_id = list_tree(&fixture.b.vault)
        .expect("读 B 结构树失败")
        .into_iter()
        .find(|entry| entry.is_note())
        .and_then(|entry| entry.body_doc_id)
        .expect("B 基线必须有笔记");
    let head = load_head(&fixture.b.vault, &doc_id)
        .expect("读 B head 失败")
        .head_seq;
    (doc_id, head)
}

/// S2b 用例 1：一次 Integrate 内收敛「结构 → 只投影 → 正文」，不需要第二轮网络。
#[test]
fn two_phase_integrate_converges_structure_before_body_in_one_round() {
    let fixture = establish("s2b-converge");
    let relay_url = fixture.relay.base_url().to_owned();
    let b_peer = fixture.b.identity.loro_peer_id();
    let (watch_doc, watch_head) = b_watch_doc(&fixture);

    // A：新建一篇（结构 + 正文都走真实 storage 路径，各自带出站意图），再推 relay。
    let note = create_note(
        &fixture.a.vault,
        fixture.a.identity.loro_peer_id(),
        fixture.a.identity.current_epoch,
        "S2b 新笔记",
        "来自 A 的新正文\n",
    );
    let a_push = DevLocal::sync_once(&relay_url, &fixture.a.vault, &fixture.a.identity)
        .expect("A 推送新笔记失败");
    assert!(
        a_push.push.acked >= 2,
        "结构与正文都必须推到 relay：{a_push:?}"
    );

    // B：Prepare 只拉取（不 integrate/投影），inbox 必须 durable。
    let prepare = DevLocal::sync_prepare(&relay_url, &fixture.b.vault, &fixture.b.identity, None)
        .expect("B Prepare 失败");
    assert!(prepare.pull.stored >= 2, "Prepare 必须拉到两条更新");
    assert_eq!(
        count_inbox_ready(&fixture.b.vault).expect("读 ready 失败"),
        prepare.pull.stored as u64,
        "Prepare 之后所有更新必须 durable 待应用"
    );
    assert!(
        list_tree(&fixture.b.vault)
            .expect("读 B 树失败")
            .iter()
            .all(|entry| entry.title != note.title),
        "Prepare 不得投影结构（网络阶段不持闸）"
    );

    // B：一次 Integrate：结构先集成 → 只投影建立 body 行 → 正文在同一次内集成。
    let integrate = DevLocal::sync_integrate(
        &relay_url,
        &fixture.b.vault,
        &fixture.b.identity,
        None,
        Some((watch_doc.as_str(), watch_head)),
        &watch_doc,
    )
    .expect("B Integrate 失败");
    assert!(
        integrate.integrate.projected,
        "结构 head 变了必须只做一次投影：{integrate:?}"
    );
    assert!(
        integrate.integrate.structure_integrated >= 1,
        "结构必须先行集成：{integrate:?}"
    );
    assert!(
        integrate.integrate.body_integrated >= 1,
        "正文必须在同一次 Integrate 内集成（不能 deferred 到下一轮网络）：{integrate:?}"
    );
    assert_eq!(
        integrate.integrate.deferred, 0,
        "依赖已在同一次 Integrate 内收敛：{integrate:?}"
    );
    assert!(
        integrate
            .changed_heads
            .iter()
            .any(|head| head == STRUCTURE_DOC_ID),
        "结构 head 真变必须出现在 changed_heads：{integrate:?}"
    );
    assert!(
        integrate
            .changed_heads
            .iter()
            .any(|head| head == &note.doc_id),
        "正文 head 真变必须出现在 changed_heads：{integrate:?}"
    );
    assert!(
        integrate.materialized >= 1,
        "changed heads 必须触发既有物化接口：{integrate:?}"
    );
    assert!(!integrate.watch_doc_changed, "活动文档本身没有远端更新");

    // 树、正文 CRDT、物化三处都必须出现；inbox 清空且水位追平。
    let tree = list_tree(&fixture.b.vault).expect("同步后读 B 树失败");
    let entry = tree
        .iter()
        .find(|entry| entry.title == note.title)
        .expect("B 的树必须出现 A 的新笔记");
    assert_eq!(entry.body_doc_id.as_deref(), Some(note.doc_id.as_str()));
    let text = body_text(&fixture.b.vault, &note.doc_id, b_peer);
    assert!(text.contains("来自 A 的新正文"), "正文必须集成：{text:?}");
    let note_id = entry.note_id.clone().expect("笔记必须有 note_id");
    let body_state = load_body_state(&fixture.b.vault, &note_id).expect("读物化状态失败");
    assert!(
        body_state
            .body_md
            .as_deref()
            .is_some_and(|body| body.contains("来自 A 的新正文")),
        "changed heads 必须触发 notes.body_md 物化：{body_state:?}"
    );
    assert_eq!(
        count_inbox_ready(&fixture.b.vault).expect("读 ready 失败"),
        0,
        "收敛后不得残留 verified 行"
    );
    let cursor = sync_cursor_state(&fixture.b.vault).expect("读水位失败");
    assert_eq!(
        cursor.integrated_seq, cursor.received_seq,
        "收敛后 integrated 必须追平 received"
    );

    // Finish（ACK）只覆盖已 integrated 的前缀。
    let finish = DevLocal::sync_finish(&relay_url, &fixture.b.vault, &fixture.b.identity, None)
        .expect("B Finish 失败");
    assert_eq!(finish.ack.integrated_seq, cursor.integrated_seq);
    assert_eq!(finish.ack.received_seq, cursor.received_seq);
}

/// S2b 用例 2a：批次上限（32 对象 / 1MiB / 50ms 软预算）真实生效，并且**有进展才继续**。
///
/// 用 1 对象/批、最多 2 批的配置：两批只能把结构集成完，正文仍 durable deferred；
/// 下一次 Integrate（**不发网络 Prepare**）继续把正文收敛完。
#[test]
fn integrate_batches_are_bounded_and_progress_continues() {
    let fixture = establish("s2b-batches");
    let relay_url = fixture.relay.base_url().to_owned();
    let (watch_doc, watch_head) = b_watch_doc(&fixture);

    // A 新建两篇（每篇一次结构提交 + 一次正文提交）。
    let first = create_note(
        &fixture.a.vault,
        fixture.a.identity.loro_peer_id(),
        fixture.a.identity.current_epoch,
        "批量第一篇",
        "批量正文一\n",
    );
    let second = create_note(
        &fixture.a.vault,
        fixture.a.identity.loro_peer_id(),
        fixture.a.identity.current_epoch,
        "批量第二篇",
        "批量正文二\n",
    );
    DevLocal::sync_once(&relay_url, &fixture.a.vault, &fixture.a.identity).expect("A 推送失败");
    DevLocal::sync_prepare(&relay_url, &fixture.b.vault, &fixture.b.identity, None)
        .expect("B Prepare 失败");

    let tiny = IntegrateConfig {
        batch_objects: 1,
        max_batches: 2,
        ..IntegrateConfig::default()
    };
    let partial = DevLocal::sync_integrate_with_config(
        &relay_url,
        &fixture.b.vault,
        &fixture.b.identity,
        None,
        Some((watch_doc.as_str(), watch_head)),
        &watch_doc,
        tiny,
    )
    .expect("B 部分 Integrate 失败");
    assert_eq!(
        partial.integrate.batches, 2,
        "批次上限必须硬生效（结构两批就停）：{partial:?}"
    );
    assert!(
        partial.integrate.projected,
        "两批结构集成后必须投影：{partial:?}"
    );
    assert!(
        partial.integrate.deferred > 0,
        "被批数上限留下的正文必须如实上报 deferred：{partial:?}"
    );
    assert_eq!(
        count_inbox_ready(&fixture.b.vault).expect("读 ready 失败"),
        partial.integrate.deferred as u64,
        "deferred 必须等于 durable verified 剩余行"
    );
    assert!(
        body_text(
            &fixture.b.vault,
            &first.doc_id,
            fixture.b.identity.loro_peer_id()
        )
        .is_empty(),
        "预置检查：结构已投影但正文还没集成"
    );
    assert!(
        body_text(
            &fixture.b.vault,
            &second.doc_id,
            fixture.b.identity.loro_peer_id()
        )
        .is_empty(),
        "预置检查：两篇正文都还没集成"
    );

    // 不发网络 Prepare，再 Integrate 一次就继续收敛（结构投影已建立 body 行）。
    let full = DevLocal::sync_integrate(
        &relay_url,
        &fixture.b.vault,
        &fixture.b.identity,
        None,
        Some((watch_doc.as_str(), watch_head)),
        &watch_doc,
    )
    .expect("B 续批 Integrate 失败");
    assert_eq!(full.integrate.deferred, 0, "续批必须收敛：{full:?}");
    assert!(
        full.integrate.body_integrated >= 2,
        "两篇正文都必须在续批内集成：{full:?}"
    );
    let tree = list_tree(&fixture.b.vault).expect("读 B 树失败");
    for note in [&first, &second] {
        let entry = tree
            .iter()
            .find(|entry| entry.title == note.title)
            .unwrap_or_else(|| panic!("树里缺少「{}」", note.title));
        assert_eq!(entry.body_doc_id.as_deref(), Some(note.doc_id.as_str()));
        assert!(
            body_text(
                &fixture.b.vault,
                &note.doc_id,
                fixture.b.identity.loro_peer_id()
            )
            .contains(note.text.trim_end()),
            "「{}」的正文必须集成",
            note.title
        );
    }
}

/// S2b 用例 2b：ACK 只覆盖 durable integrated 前缀，**不越过 deferred**。
#[test]
fn ack_never_leads_the_durable_integrated_prefix() {
    let fixture = establish("s2b-ack");
    let relay_url = fixture.relay.base_url().to_owned();
    let (watch_doc, watch_head) = b_watch_doc(&fixture);

    let note = create_note(
        &fixture.a.vault,
        fixture.a.identity.loro_peer_id(),
        fixture.a.identity.current_epoch,
        "水位笔记",
        "水位正文\n",
    );
    DevLocal::sync_once(&relay_url, &fixture.a.vault, &fixture.a.identity).expect("A 推送失败");
    DevLocal::sync_prepare(&relay_url, &fixture.b.vault, &fixture.b.identity, None)
        .expect("B Prepare 失败");

    // 只跑一批（1 个结构对象）：正文必然 deferred。
    let only_structure = IntegrateConfig {
        batch_objects: 1,
        max_batches: 1,
        ..IntegrateConfig::default()
    };
    let partial = DevLocal::sync_integrate_with_config(
        &relay_url,
        &fixture.b.vault,
        &fixture.b.identity,
        None,
        Some((watch_doc.as_str(), watch_head)),
        &watch_doc,
        only_structure,
    )
    .expect("B 部分 Integrate 失败");
    assert!(partial.integrate.deferred > 0, "正文必须还在 deferred");

    let cursor = sync_cursor_state(&fixture.b.vault).expect("读水位失败");
    assert!(
        cursor.integrated_seq < cursor.received_seq,
        "integrated 水位不能越过 deferred（收到 ≠ 已应用）：{cursor:?}"
    );
    let finish = DevLocal::sync_finish(&relay_url, &fixture.b.vault, &fixture.b.identity, None)
        .expect("B Finish 失败");
    assert_eq!(
        finish.ack.integrated_seq, cursor.integrated_seq,
        "ACK 只能上报真实 integrated 前缀"
    );
    assert!(
        finish.ack.integrated_seq < finish.ack.received_seq,
        "ACK 不得领先 deferred：{finish:?}"
    );

    // 继续集成后水位追平，ACK 才覆盖全部已接收对象。
    let full = DevLocal::sync_integrate(
        &relay_url,
        &fixture.b.vault,
        &fixture.b.identity,
        None,
        Some((watch_doc.as_str(), watch_head)),
        &watch_doc,
    )
    .expect("B 续批 Integrate 失败");
    assert_eq!(full.integrate.deferred, 0);
    let finish = DevLocal::sync_finish(&relay_url, &fixture.b.vault, &fixture.b.identity, None)
        .expect("B 第二次 Finish 失败");
    assert_eq!(
        finish.ack.integrated_seq, finish.ack.received_seq,
        "收敛后 ACK 才能覆盖全部：{finish:?}"
    );
    assert!(body_text(
        &fixture.b.vault,
        &note.doc_id,
        fixture.b.identity.loro_peer_id()
    )
    .contains("水位正文"));
}

/// S2b 用例 4：Integrate 前本地 head 被推进（授权失效）→ 不 append、不丢 durable，
/// 按新授权重试就能交付。
#[test]
fn stale_authorization_keeps_inbox_and_retry_delivers_without_data_loss() {
    let fixture = establish("s2b-head-conflict");
    let relay_url = fixture.relay.base_url().to_owned();
    let doc_id = fixture.note.doc_id.clone();
    let b_peer = fixture.b.identity.loro_peer_id();
    let b_epoch = fixture.b.identity.current_epoch;
    let old_head = load_head(&fixture.b.vault, &doc_id)
        .expect("读 B head 失败")
        .head_seq;

    // A 追加一段并推到 relay；B Prepare 只拉到 durable inbox。
    append_body(
        &fixture.a.vault,
        &doc_id,
        fixture.a.identity.loro_peer_id(),
        fixture.a.identity.current_epoch,
        "来自 A 的冲突前正文\n",
    );
    DevLocal::sync_once(&relay_url, &fixture.a.vault, &fixture.a.identity).expect("A 推送失败");
    let prepare = DevLocal::sync_prepare(&relay_url, &fixture.b.vault, &fixture.b.identity, None)
        .expect("B Prepare 失败");
    assert!(prepare.pull.stored >= 1);

    // 集成前 B 本地写入：head 被推进，持闸时的授权基准已过期。
    append_body(&fixture.b.vault, &doc_id, b_peer, b_epoch, "B 本地写入\n");
    let error = DevLocal::sync_integrate(
        &relay_url,
        &fixture.b.vault,
        &fixture.b.identity,
        None,
        Some((doc_id.as_str(), old_head)),
        &doc_id,
    )
    .expect_err("旧授权必须被拒绝");
    assert!(
        matches!(error, SyncError::CancelledBeforeIntegrate),
        "必须走授权失效路径而不是数据丢失：{error:?}"
    );

    // 不 append、不丢 durable：head 仍是本地写入后的值，inbox 行原样保留。
    let head_after_failure = load_head(&fixture.b.vault, &doc_id)
        .expect("读 B head 失败")
        .head_seq;
    assert_eq!(
        head_after_failure,
        old_head + 1,
        "拒绝集成不得改动 head（只有本地写入那一条）"
    );
    assert!(
        count_inbox_ready(&fixture.b.vault).expect("读 ready 失败") >= 1,
        "durable inbox 不得丢失"
    );
    let text = body_text(&fixture.b.vault, &doc_id, b_peer);
    assert!(
        !text.contains("来自 A 的冲突前正文"),
        "授权失效时不得 append：{text:?}"
    );
    assert!(text.contains("B 本地写入"), "{text:?}");

    // 按新授权（当前 head）重试：交付两条更新，正文合并。
    let current_head = load_head(&fixture.b.vault, &doc_id)
        .expect("读 B head 失败")
        .head_seq;
    let retry = DevLocal::sync_integrate(
        &relay_url,
        &fixture.b.vault,
        &fixture.b.identity,
        None,
        Some((doc_id.as_str(), current_head)),
        &doc_id,
    )
    .expect("新授权重试必须成功");
    assert!(retry.integrate.integrated >= 1, "{retry:?}");
    assert_eq!(retry.integrate.deferred, 0, "{retry:?}");
    assert!(retry.watch_doc_changed, "活动文档 head 必须前进");
    let text = body_text(&fixture.b.vault, &doc_id, b_peer);
    assert!(
        text.contains("来自 A 的冲突前正文") && text.contains("B 本地写入"),
        "重试必须交付 durable 更新且不丢本地写入：{text:?}"
    );
}
