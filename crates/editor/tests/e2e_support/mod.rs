//! 切片 4 无头端到端脚手架：临时 vault、真实会话 / 驱动、等待 ACK。
//!
//! - 临时目录按 AGENTS.md 放 `CARGO_TARGET_TMPDIR`：本机构建环境里 cargo 启动的
//!   测试进程不能写 `%TEMP%`；
//! - 不引入 `tempfile`（避免给 editor 增加 dev-dependency），手写带 `Drop` 清理的
//!   [`TestDir`]；
//! - 所有时间推进都用 [`ManualClock`]，不靠真睡 250 ms 碰运气；只有后台 flush
//!   完成等待需要短轮询（`wait_durable`）。

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use menghunote_editor::EditorController;
use menghunote_storage::session::{
    Applied, DocumentSession, DriverEvent, DriverOptions, DurableAck, EditOutcome, FlushPolicy,
    LiveRev, ManualClock, Rejected, SessionDriver, SessionEpoch, SessionOptions,
};
use menghunote_storage::{create_body_doc, create_note_slot, BodyDoc, Vault};

/// 夹具笔记 ID。
pub const NOTE_ID: &str = "note-1";
/// 夹具正文文档 ID。
pub const DOC_ID: &str = "doc-1";
/// 本机 peer。
pub const PEER_A: u64 = 1;
/// 远端 peer。
pub const PEER_B: u64 = 2;
/// ADR 0002 §11.1 冻结夹具（`é` 为分解形式 e + U+0301：11/5/6/4）。
pub const NFD: &str = "A中😀e\u{301}";

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// `target/tmp` 下的一次性目录，`Drop` 时清理。
pub struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new() -> Self {
        let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
        fs::create_dir_all(base).expect("创建 target/tmp");
        let unique = format!(
            "editor-e2e-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        );
        let path = base.join(unique);
        fs::create_dir_all(&path).expect("创建测试目录");
        Self { path }
    }

    /// vault 文件路径（父目录故意不存在，由 `Vault::open` 创建）。
    pub fn vault_path(&self) -> PathBuf {
        self.path.join("vaults").join("primary.sqlite3")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// 新建临时目录 + vault。
pub fn fresh_vault() -> (TestDir, Vault) {
    let dir = TestDir::new();
    let vault = Vault::open(dir.vault_path()).expect("打开 vault");
    (dir, vault)
}

/// 重开同一个 vault（模拟重启）。
pub fn open_vault(dir: &TestDir) -> Vault {
    Vault::open(dir.vault_path()).expect("重开 vault")
}

/// 建 `loro_docs` + `notes` 行（B 物化需要 notes 行）。
pub fn seed_note(vault: &Vault) -> BodyDoc {
    create_body_doc(vault, DOC_ID).expect("插入 loro_docs 行");
    create_note_slot(vault, NOTE_ID, DOC_ID, "测试笔记", 1).expect("插入 notes 行");
    BodyDoc::create(DOC_ID, PEER_A).expect("创建 BodyDoc")
}

fn session_options(capacity: usize, flush_delay: Option<Duration>) -> SessionOptions {
    SessionOptions {
        peer_id: PEER_A,
        session_epoch: SessionEpoch(21),
        journal_capacity: capacity,
        flush_batch: 8,
        flush_delay,
    }
}

/// 直接打开会话核（用 [`DocumentSession`] 直接调 undo/redo/apply_remote 的场景）。
pub fn open_session(
    vault: &Vault,
    capacity: usize,
    flush_delay: Option<Duration>,
) -> DocumentSession {
    DocumentSession::open(vault, DOC_ID, session_options(capacity, flush_delay)).expect("打开会话")
}

/// 打开会话驱动（固定窗口 flush + B 物化 + close 排空）。
pub fn open_driver(
    vault: &Vault,
    clock: ManualClock,
    capacity: usize,
    flush_delay: Option<Duration>,
) -> SessionDriver {
    SessionDriver::open_with_clock(
        vault,
        DOC_ID,
        DriverOptions {
            session: session_options(capacity, flush_delay),
            policy: FlushPolicy::default(),
            auto_tick: false,
            tick_interval: None,
        },
        Box::new(clock),
    )
    .expect("打开驱动")
}

/// 用会话快照初始化编辑器纯模型。
pub fn open_editor(session: &DocumentSession) -> EditorController {
    EditorController::open(session.snapshot()).expect("打开编辑器模型")
}

/// 断言并取出 `Applied`。
pub fn applied(outcome: EditOutcome) -> Applied {
    match outcome {
        EditOutcome::Applied(applied) => applied,
        other => panic!("期望 Applied，实际 {other:?}"),
    }
}

/// 断言并取出 `Rejected`。
pub fn rejected(outcome: EditOutcome) -> Rejected {
    match outcome {
        EditOutcome::Rejected(rejected) => rejected,
        other => panic!("期望 Rejected，实际 {other:?}"),
    }
}

/// 轮询驱动事件，直到出现覆盖 `target` 的 `DurableAck`。
pub fn wait_durable(
    driver: &mut SessionDriver,
    target: LiveRev,
    timeout: Duration,
) -> Option<DurableAck> {
    let deadline = Instant::now() + timeout;
    loop {
        for event in driver.pump() {
            if let DriverEvent::Durable(ack) = event {
                if ack.through_live_rev >= target {
                    return Some(ack);
                }
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(2));
    }
}

/// 由另一个 peer 产生一段可导入的远端 update（同一 `doc_id`）。
pub fn remote_update(insert_at: usize, text: &str) -> Vec<u8> {
    let remote = BodyDoc::create(DOC_ID, PEER_B).expect("创建远端副本");
    remote.insert_scalars(insert_at, text).expect("远端编辑");
    remote.export_all_updates().expect("导出远端 update")
}
