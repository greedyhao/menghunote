//! ADR 0009 切片 3b：`SessionDriver` 对**外部提交闸门 / adopt** 的转发。
//!
//! 覆盖一条完整的驱动器级链路（存储层语义在 3a，这里只验"驱动不吞错、事件到位"）：
//!
//! 1. 本地编辑 → `flush` → durable；
//! 2. `begin` → `poll` 到 `Held`；Held 期间 `apply` / `undo` 一律 `Rejected{NotReady}`
//!    （驱动原样透传，不额外吞错），文本不变；
//! 3. 外部 writer 直接 `append_updates`（模拟 `engine.integrate`）→ `prepare_committed_delta`
//!    → `adopt`：**不重开文档**就能看到远端文字；`pump` 按序返回 `Remote` → `Durable`；
//! 4. adopt **不**让驱动走会 journal 的 `apply_remote`：`sync_outbox` 0 新行、
//!    `loro_updates` 不多写；
//! 5. `end` 之后本地继续编辑 → flush 成功（rebase 生效，无 `HeadConflict`），
//!    新 outbox 行数 = 本地 update 数（adopt 那条不算）。
//!
//! 纪律（AGENTS.md §6）：临时目录由 `support` 放在 `CARGO_TARGET_TMPDIR` 下；等待的判据
//! 是 storage 自己的水位 / `Held` 状态（`yield_now` 只让出 CPU），**不用 sleep 当收敛判据**。

mod support;

use std::time::{Duration, Instant};

use menghunote_storage::session::{
    prepare_committed_delta, AppliedHeads, ChangeSource, DriverEvent, DriverOptions, EditOutcome,
    EditRequest, ExternalCommitLease, ExternalCommitTicket, FlushPolicy, FlushReason, GateState,
    LiveRev, ManualClock, RejectReason, ScalarEdit, SessionDriver, SessionEpoch, SessionOptions,
};
use menghunote_storage::{
    append_updates, load_body_doc, load_head, SyncContext, UpdateRecord, Vault,
};
use support::{count, fresh_vault, seed_note, PEER_A};

/// 模拟"另一个 writer"的 Loro peer（必须与本地 peer 区分开）。
const PEER_EXTERNAL: u64 = 3;

/// 启用同步上下文，才能证明 adopt 不产生**出站意图**。
const CTX: SyncContext = SyncContext {
    peer_id: 0xA1CE_003B,
    key_epoch: 1,
};

fn edits(start: usize, end: usize, replacement: &str) -> ScalarEdit {
    ScalarEdit {
        start: menghunote_text::UnicodeScalar::new(start),
        end: menghunote_text::UnicodeScalar::new(end),
        replacement: replacement.to_owned(),
    }
}

fn request(
    session: &menghunote_storage::session::DocumentSession,
    command: &str,
    base: u64,
    edits: Vec<ScalarEdit>,
) -> EditRequest {
    EditRequest {
        doc_id: session.doc_id().to_owned(),
        generation: session.generation(),
        session_epoch: session.session_epoch(),
        command_id: command.to_owned(),
        base_live_rev: LiveRev(base),
        edits,
        origin: "local".to_owned(),
        undo_group: None,
    }
}

/// 打开驱动器：手动时钟（不让 250 ms 窗口自己到期，flush 全部显式触发）。
fn open_driver(vault: &Vault) -> SessionDriver {
    SessionDriver::open_with_clock(
        vault,
        "doc-1",
        DriverOptions {
            session: SessionOptions {
                peer_id: CTX.peer_id,
                session_epoch: SessionEpoch(31),
                sync: Some(CTX),
                ..SessionOptions::default()
            },
            policy: FlushPolicy::default(),
            auto_tick: false,
            tick_interval: None,
        },
        Box::new(ManualClock::new()),
    )
    .expect("打开驱动")
}

fn outbox_rows(vault: &Vault) -> i64 {
    count(vault, "SELECT count(*) FROM sync_outbox", &[])
}

fn update_rows(vault: &Vault) -> i64 {
    count(
        vault,
        "SELECT count(*) FROM loro_updates WHERE doc_id='doc-1'",
        &[],
    )
}

/// 模拟 `engine.integrate`：从库中当前 head 重放一份独立副本 → 改 → 一个 A 事务追加。
fn external_append(vault: &Vault, at: usize, text: &str) -> u64 {
    let current = load_head(vault, "doc-1").expect("库中 head");
    let base = current.version_vector().expect("解码 head_vv");
    let (doc, _) = load_body_doc(vault, "doc-1", PEER_EXTERNAL, None).expect("重放外部副本");
    doc.insert_scalars(at, text).expect("外部编辑");
    let update = doc.pending_update(&base).expect("外部增量");
    append_updates(
        vault,
        "doc-1",
        current.head_seq,
        &[UpdateRecord::new("engine-integrate", update)],
    )
    .expect("外部 A 事务")
    .head_seq
}

/// 等本地编辑落 durable（收敛判据是驱动自己推的 `DurableAck`）。
fn wait_durable(
    driver: &mut SessionDriver,
    target: LiveRev,
) -> menghunote_storage::session::DurableAck {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for event in driver.pump() {
            if let DriverEvent::Durable(ack) = event {
                if ack.through_live_rev >= target {
                    return ack;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "本地 flush 10s 内没有 durable（pending={}）",
            driver.pending_edits()
        );
        // 只让出 CPU 等 flush 线程；不用 sleep 当收敛判据。
        std::thread::yield_now();
    }
}

/// 把闸门驱到 `Held`：只等**明确状态**，时间只是安全上限。
fn drive_to_held(driver: &mut SessionDriver, ticket: &ExternalCommitTicket) -> ExternalCommitLease {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match driver.poll_external_commit(ticket) {
            GateState::Held(lease) => return lease,
            GateState::Flushing => std::thread::yield_now(),
            GateState::WaitingIME => panic!("没有 composition 时不应 WaitingIME"),
            other => panic!("闸门未收敛到 Held：{other:?}"),
        }
        assert!(Instant::now() < deadline, "闸门 10s 内没有进入 Held");
    }
}

fn applied_heads(vault: &Vault) -> AppliedHeads {
    AppliedHeads::load(vault, "doc-1").expect("读库中 heads")
}

#[test]
fn driver_adopts_external_commit_and_keeps_local_flush_working() {
    let (_dir, vault) = fresh_vault();
    let _seed = seed_note(&vault, "note-1", "doc-1");
    assert_ne!(PEER_A, CTX.peer_id);
    let mut driver = open_driver(&vault);

    // 1) 本地编辑 → 显式 flush → durable
    assert!(matches!(
        driver.apply(request(
            driver.session(),
            "c1",
            0,
            vec![edits(0, 0, "本地")]
        )),
        EditOutcome::Applied(_)
    ));
    driver.flush(FlushReason::Manual);
    let ack = wait_durable(&mut driver, LiveRev(1));
    assert_eq!(ack.through_live_rev, LiveRev(1));
    assert_eq!(ack.head_seq, 1);
    assert_eq!(outbox_rows(&vault), 1, "本地 update 必须产生 1 条意图");
    assert_eq!(update_rows(&vault), 1);

    // 2) begin → Held
    let ticket = driver.begin_external_commit().expect("begin");
    assert_eq!(ticket.target_live_rev(), LiveRev(1));
    assert_eq!(ticket.baseline_head_seq(), 1);
    assert!(
        !driver.is_external_commit_held(),
        "begin 不阻塞、也不直接 Held"
    );
    let lease = drive_to_held(&mut driver, &ticket);
    assert!(driver.is_external_commit_held());
    assert_eq!(driver.session().external_commit_lease(), Some(&lease));

    // 3) Held 期间：驱动**原样透传**拒绝，不吞错、不改文本
    match driver.apply(request(driver.session(), "c2", 1, vec![edits(2, 2, "禁")])) {
        EditOutcome::Rejected(rejected) => {
            assert_eq!(rejected.reason, RejectReason::NotReady);
            assert_eq!(rejected.command_id, "c2");
        }
        other => panic!("Held 期间 apply 必须被拒，实际 {other:?}"),
    }
    match driver.undo().expect("undo 不 panic") {
        Some(EditOutcome::Rejected(rejected)) => {
            assert_eq!(rejected.reason, RejectReason::NotReady)
        }
        other => panic!("Held 期间 undo 必须被拒，实际 {other:?}"),
    }
    assert_eq!(driver.markdown(), "本地");
    assert_eq!(driver.live_rev(), LiveRev(1));

    // 4) 外部提交（模拟 engine.integrate）：head 1 → 2
    assert_eq!(external_append(&vault, 2, "远端"), 2);
    let prepared = prepare_committed_delta(&vault, "doc-1", lease.durable_head_seq())
        .expect("prepare 外部增量");
    assert_eq!(prepared.base_seq, lease.durable_head_seq());
    assert_eq!(prepared.target_head_seq, 2);
    assert_eq!(prepared.updates.len(), 1);

    let outcome = driver
        .adopt_committed_delta(&lease, prepared)
        .expect("adopt 已 durable 的增量");
    assert_eq!(outcome.delta.source, ChangeSource::Remote);
    assert_eq!(outcome.delta.cause, None);
    assert_eq!(outcome.delta.from_rev, LiveRev(1));
    assert_eq!(outcome.delta.to_rev, LiveRev(2));
    assert_eq!(outcome.ack.head_seq, 2);
    assert_eq!(outcome.ack.through_live_rev, LiveRev(2));

    // 不重开文档就能看到远端文字
    assert_eq!(driver.markdown(), "本地远端");
    assert_eq!(driver.live_rev(), LiveRev(2));

    // 5) pump 取到 Remote（镜像）与 Durable（水位），且顺序固定：先 Remote 后 Durable
    let events = driver.pump();
    let remote = events
        .iter()
        .find_map(|event| match event {
            DriverEvent::Remote(delta) => Some(delta.clone()),
            _ => None,
        })
        .expect("pump 必须返回 DriverEvent::Remote");
    assert_eq!(remote.doc_id, "doc-1");
    assert_eq!(remote.to_rev, LiveRev(2));
    assert_eq!(remote.source, ChangeSource::Remote);
    let durable = events
        .iter()
        .find_map(|event| match event {
            DriverEvent::Durable(ack) => Some(ack.clone()),
            _ => None,
        })
        .expect("pump 必须返回 DriverEvent::Durable");
    assert_eq!(durable.head_seq, 2);
    assert_eq!(durable.through_live_rev, LiveRev(2));
    let remote_at = events
        .iter()
        .position(|event| matches!(event, DriverEvent::Remote(_)))
        .expect("Remote 位置");
    let durable_at = events
        .iter()
        .position(|event| matches!(event, DriverEvent::Durable(_)))
        .expect("Durable 位置");
    assert!(
        remote_at < durable_at,
        "必须先推 Remote 再推 Durable（否则镜像会落后于水位）：{events:?}"
    );

    // 6) adopt 没有走 apply_remote：0 新 outbox 行、没有重复写库
    assert_eq!(outbox_rows(&vault), 1, "adopt 不得产生新的出站意图");
    assert_eq!(update_rows(&vault), 2, "adopt 只导入已 durable 的行");

    // 7) end → 继续本地编辑 → flush 成功（rebase 生效，没有 HeadConflict）
    driver
        .end_external_commit(lease, applied_heads(&vault))
        .expect("end");
    assert!(!driver.is_external_commit_held());
    assert!(matches!(
        driver.apply(request(driver.session(), "c3", 2, vec![edits(4, 4, "续")])),
        EditOutcome::Applied(_)
    ));
    driver.flush(FlushReason::Manual);
    let ack = wait_durable(&mut driver, LiveRev(3));
    assert_eq!(ack.head_seq, 3, "本地 flush 必须接在外部 head 之后");
    assert_eq!(driver.markdown(), "本地远端续");
    assert_eq!(update_rows(&vault), 3, "新 update 行数 = 本地 update 数");
    assert_eq!(
        outbox_rows(&vault),
        2,
        "outbox 行数 = 本地 update 数（2 条本地 + adopt 0 条）"
    );
    assert_eq!(driver.pending_edits(), 0);
}
