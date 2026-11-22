//! dev-sync 服务层集成测试（ADR 0009 Phase 1 / 切片 1）。
//!
//! 覆盖：门禁默认关闭、Busy 拒绝、Progress 合并、Completed 保留、Stop→Stopped、
//! 关闭票据 Pending→Ready/Blocked、worker 回收、未实现命令的错误可见、操作 deadline。
//!
//! ## 纪律（AGENTS.md §4/§6）
//!
//! - **不 sleep**：收敛点只有两类——状态机的同步返回，以及 [`SyncService::reap_worker`]
//!   （join 建立 happens-before：worker 已发出的全部事件，join 之后 `poll()` 必然能收到）；
//! - 阻塞/放行用 `Condvar` + `Mutex`（测试自己控制的假 runner），不用时钟；
//! - 不用 `tempfile`/`%TEMP%`：本文件**不碰真实库/网络**（relay 只做 URL 校验）。
//!
//! feature 关闭时整个文件编译为空（0 个测试），默认 `cargo test -p menghunote-app`
//! 完全不受影响——这就是"默认构建零变化"的机械化证据。

#![cfg(feature = "dev-sync")]

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use menghunote_app::dev_sync::gate::{self, Gate as RuntimeGate, GateError};
use menghunote_app::dev_sync::service::{
    OpControl, ServiceConfig, SyncRunner, SyncService, AUTOSYNC_TRANSPORT_TIMEOUTS,
    DEFAULT_OPERATION_DEADLINE, DEFAULT_TRANSPORT_TIMEOUTS,
};
use menghunote_app::dev_sync::state::{
    BusyReason, CloseState, Command, DevSyncState, Event, HeldGate, OpId, OpOutcome, OpResult,
    Phase,
};

/// 测试用可控执行体：按测试配置阻塞在 `Condvar` 上，放行后返回预设结果。
struct ControlledRunner {
    gate: Arc<TestGate>,
}

impl ControlledRunner {
    /// 模拟 runner：发进度 → 停在"外部集成边界"上 → 放行后返回预设结果。
    ///
    /// `control` 只有真实 worker 路径才有（`run_gated_controlled`）；测试用它证明
    /// `Cancel` 的取消标志确实到了执行体手里（ADR 0012 S1）。
    fn run_inner(
        &mut self,
        control: Option<&OpControl>,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        let (progress, block) = {
            let state = self.gate.state.lock().expect("gate 锁");
            (state.progress.clone(), state.block)
        };
        // runner 只上报 Progress（op_id 由 worker 盖章，这里用占位值）。
        for (phase, detail) in progress {
            events(Event::Progress {
                op_id: OpId::UNASSIGNED,
                phase,
                detail,
            });
        }
        {
            let mut state = self.gate.state.lock().expect("gate 锁");
            state.entered += 1;
            state.progress_sent += 1;
            if control.is_some_and(OpControl::is_cancelled) {
                state.observed_cancel = true;
            }
            // 通知所有等待者：runner 已在 "进度发完、还没返回" 的位置上。
            self.gate.condvar.notify_all();
            while block && !state.released {
                state = self.gate.condvar.wait(state).expect("gate wait");
                if control.is_some_and(OpControl::is_cancelled) && !state.observed_cancel {
                    state.observed_cancel = true;
                    // 让测试的 wait_until 立刻看到这条事实（不是等它自己超时醒）。
                    self.gate.condvar.notify_all();
                }
            }
        }
        let state = self.gate.state.lock().expect("gate 锁");
        let fail = state.fail.clone();
        let heads = state.heads.clone();
        let summary = state.summary.clone();
        drop(state);
        match fail {
            Some(message) => Err(message),
            None => Ok(OpOutcome {
                changed_heads: heads,
                summary,
            }),
        }
    }
}

impl SyncRunner for ControlledRunner {
    fn run(&mut self, _op: Command, events: &mut dyn FnMut(Event)) -> Result<OpOutcome, String> {
        self.run_inner(None, events)
    }

    fn run_gated_controlled(
        &mut self,
        _op: Command,
        _gate: Option<&HeldGate>,
        control: &OpControl,
        events: &mut dyn FnMut(Event),
    ) -> Result<OpOutcome, String> {
        self.run_inner(Some(control), events)
    }
}

/// 测试闸：配置假 runner 的行为，并提供**有界**等待原语（不是 sleep）。
#[derive(Default)]
struct GateState {
    block: bool,
    released: bool,
    progress: Vec<(Phase, String)>,
    fail: Option<String>,
    heads: Vec<String>,
    summary: String,
    /// 已进入 runner 的次数。
    entered: usize,
    /// 已发完进度的次数（测试用它当"进度已在通道里"的收敛点）。
    progress_sent: usize,
    /// worker 是否已经看到取消标志（只有 `run_gated_controlled` 路径能观察到）。
    observed_cancel: bool,
}

struct TestGate {
    state: Mutex<GateState>,
    condvar: Condvar,
}

impl TestGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState::default()),
            condvar: Condvar::new(),
        })
    }

    /// 配置下一次（以及之后每次）run 的行为：卡住直到 `release()`。
    fn expect_blocking(
        &self,
        fail: Option<&str>,
        heads: &[&str],
        summary: &str,
        progress: &[(Phase, &str)],
    ) {
        let mut state = self.state.lock().expect("gate 锁");
        state.block = true;
        state.fail = fail.map(str::to_owned);
        state.heads = heads.iter().map(|head| (*head).to_owned()).collect();
        state.summary = summary.to_owned();
        state.progress = progress
            .iter()
            .map(|(phase, detail)| (*phase, (*detail).to_owned()))
            .collect();
    }

    /// 唤醒阻塞中的 runner（让它在 wait 返回后重新读取取消标志；不是 sleep）。
    fn wake(&self) {
        let _state = self.state.lock().expect("gate 锁");
        self.condvar.notify_all();
    }

    /// 放行（置位后不再阻塞后续 run）。
    fn release(&self) {
        let mut state = self.state.lock().expect("gate 锁");
        state.released = true;
        self.condvar.notify_all();
    }

    /// 有界等待某个条件（唯一用途：等 worker 真的进了 runner / 发完了进度）。
    fn wait_until(&self, description: &str, predicate: impl Fn(&GateState) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut state = self.state.lock().expect("gate 锁");
        while !predicate(&state) {
            let now = Instant::now();
            assert!(now < deadline, "等待超时：{description}");
            let (next, timeout) = self
                .condvar
                .wait_timeout(state, deadline - now)
                .expect("gate wait_timeout");
            state = next;
            assert!(
                !timeout.timed_out() || predicate(&state),
                "等待超时：{description}"
            );
        }
    }
}

fn service_with_gate(gate: Arc<TestGate>) -> Arc<SyncService> {
    let config = ServiceConfig::new("http://127.0.0.1:18080");
    SyncService::with_runner(config, Box::new(ControlledRunner { gate }))
}

fn default_config() -> ServiceConfig {
    ServiceConfig::new("http://127.0.0.1:18080")
}

/// 有界等待真实终态（状态收敛点 = `last_completed` 出现该 op；不 sleep）。
fn poll_until_completed(service: &SyncService, op_id: OpId, label: &str) -> Vec<Event> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut collected = Vec::new();
    loop {
        collected.extend(service.poll());
        if service
            .snapshot()
            .last_completed
            .as_ref()
            .is_some_and(|(completed, _, _)| *completed == op_id)
        {
            return collected;
        }
        assert!(Instant::now() < deadline, "等待 {label} 超时（5s）");
        std::thread::yield_now();
    }
}

/// 运行时门禁：默认关闭；请求启用时缺 vault / 非 loopback relay 必须明确拒绝。
#[test]
fn runtime_gate_is_off_by_default_and_refuses_incomplete_config() {
    // 1) 什么都没设置 → Disabled（带 feature 的普通启动也走这条路，零行为变化）。
    let disabled = gate::resolve(Vec::<String>::new(), |_| None).expect("未启用不是错误");
    assert_eq!(disabled, RuntimeGate::Disabled);
    assert!(disabled.config().is_none());

    // 2) 请求启用但没显式 vault → 明确报错（不回退默认 dev vault）。
    let missing =
        gate::resolve(["--dev-sync".to_owned()], |_| None).expect_err("缺 vault 必须拒绝");
    assert_eq!(missing, GateError::MissingVault);
    assert!(missing.to_string().contains(gate::VAULT_ENV));

    // 3) relay 非 loopback（含 https）→ 明确报错。
    let vault = "/tmp/menghunote-dev-sync-integration/vault.sqlite3";
    let bad = gate::resolve(["--dev-sync".to_owned()], |key| match key {
        gate::VAULT_ENV => Some(vault.to_owned()),
        gate::RELAY_ENV => Some("https://localhost:18080".to_owned()),
        _ => None,
    })
    .expect_err("https relay 必须拒绝");
    assert!(matches!(bad, GateError::RelayNotLoopback(_)));

    // 4) env 开关 + loopback 默认 relay → 启用。
    let enabled = gate::resolve(["menghunote-app".to_owned()], |key| match key {
        gate::ENABLE_ENV => Some("1".to_owned()),
        gate::VAULT_ENV => Some(vault.to_owned()),
        _ => None,
    })
    .expect("loopback 默认 relay 必须启用");
    assert!(matches!(enabled, RuntimeGate::Enabled(_)));
    let config = enabled.config().expect("配置");
    assert_eq!(config.relay(), gate::DEFAULT_RELAY);
    assert_eq!(
        config.service.operation_deadline,
        Some(DEFAULT_OPERATION_DEADLINE)
    );
}

/// 单操作在途：第二条操作命令必须被 `Busy` 拒绝，并带上在途 op。
#[test]
fn second_operation_is_rejected_while_one_is_in_flight() {
    let gate = TestGate::new();
    gate.expect_blocking(None, &["head-1"], "已同步", &[]);
    let service = service_with_gate(gate.clone());

    let first = service.try_command(Command::Create).expect("第一条命令");
    let busy = service
        .try_command(Command::SyncOnce)
        .expect_err("在途时必须拒绝");
    assert_eq!(busy.reason, BusyReason::InFlight);
    assert_eq!(busy.current, Some(first));
    assert!(
        busy.to_string().contains("在途"),
        "拒绝原因必须可读：{busy}"
    );
    assert_eq!(
        service.snapshot().in_flight,
        Some((first, Command::Create.kind())),
        "快照必须显示在途 op"
    );

    // 收尾：放行 → Stop → join（确定性收敛点）→ 事件必须完整。
    gate.release();
    let _ticket = service.request_stop();
    assert!(service.reap_worker(), "worker 必须在 Stop 后退出");
    let events = service.poll();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Completed { op_id, result: OpResult::Ok { .. }, .. } if *op_id == first
        )),
        "放行后必须拿到 Completed(Ok)：{events:?}"
    );
    assert!(events
        .iter()
        .any(|event| matches!(event, Event::Stopped { .. })));
}

/// Progress 可合并（同一 op 只留最新一条），Completed 不丢。
#[test]
fn progress_is_coalesced_and_completed_is_delivered() {
    let gate = TestGate::new();
    gate.expect_blocking(
        None,
        &["head-7"],
        "已同步 1 个头",
        &[
            (Phase::Preparing, "准备"),
            (Phase::Pushing, "推送 1 条"),
            (Phase::Durability, "落盘完成"),
        ],
    );
    let service = service_with_gate(gate.clone());

    let op_id = service.try_command(Command::SyncOnce).expect("同步命令");
    // 收敛点：runner 已把 3 条进度发进通道，自己还卡在 Condvar 上。
    gate.wait_until("进度已发出", |state| state.progress_sent >= 1);

    let events = service.poll();
    assert_eq!(events.len(), 1, "3 条进度必须合并成 1 条：{events:?}");
    match &events[0] {
        Event::Progress {
            op_id: event_op,
            phase,
            detail,
        } => {
            assert_eq!(*event_op, op_id, "op_id 必须由 worker 盖章");
            assert_eq!(*phase, Phase::Durability);
            assert_eq!(detail, "落盘完成");
        }
        other => panic!("必须是 Progress：{other:?}"),
    }
    assert_eq!(
        service.snapshot().progress,
        Some((Phase::Durability, "落盘完成".to_owned()))
    );
    assert!(
        matches!(service.snapshot().state, DevSyncState::Busy { .. }),
        "还在等 runner 返回，状态必须是 Busy"
    );

    gate.release();
    let _ticket = service.request_stop();
    assert!(service.reap_worker(), "worker 必须退出");
    let events = service.poll();
    let completed: Vec<&Event> = events
        .iter()
        .filter(|event| matches!(event, Event::Completed { .. }))
        .collect();
    assert_eq!(completed.len(), 1, "Completed 必须恰好一条：{events:?}");
    assert_eq!(
        service.snapshot().last_completed,
        Some((
            op_id,
            vec!["head-7".to_owned()],
            OpResult::Ok {
                summary: "已同步 1 个头".to_owned()
            }
        ))
    );
}

/// Stop → 在途操作跑完 → Stopped；关闭票据 Pending → Ready；worker 被回收。
#[test]
fn stop_yields_pending_then_ready_and_reaps_the_worker() {
    let gate = TestGate::new();
    gate.expect_blocking(None, &["head-1"], "已确认 durable", &[]);
    let service = service_with_gate(gate.clone());

    let op_id = service.try_command(Command::SyncOnce).expect("同步命令");
    let ticket = service.request_stop();
    assert_eq!(
        service.poll_close(&ticket),
        CloseState::Pending,
        "在途操作还没确认 durable，关闭必须 Pending"
    );
    // 重复请求幂等：拿到同一张票。
    assert_eq!(service.request_stop(), ticket);
    assert_eq!(service.poll_close(&ticket), CloseState::Pending);
    assert!(
        service.try_command(Command::SyncOnce).is_err(),
        "关闭流程里不再接新命令"
    );

    gate.release();
    assert!(service.reap_worker(), "worker 必须在 Stopped 之后退出");
    let events = service.poll();
    assert_eq!(events.len(), 2, "Completed + Stopped：{events:?}");
    assert!(matches!(
        events[0],
        Event::Completed {
            op_id: completed,
            result: OpResult::Ok { .. },
            ..
        } if completed == op_id
    ));
    assert!(matches!(events[1], Event::Stopped { .. }));

    assert_eq!(
        service.poll_close(&ticket),
        CloseState::Ready,
        "目标 durable 已确认 → Ready"
    );
    assert!(service.snapshot().worker_stopped);
    assert_eq!(service.snapshot().state, DevSyncState::Closing);
    // worker 已停：不再接命令，也不会重复回收。
    assert_eq!(
        service
            .try_command(Command::Create)
            .expect_err("worker 已停")
            .reason,
        BusyReason::WorkerStopped
    );
    assert!(!service.reap_worker(), "已回收过的 worker 不能重复 join");
}

/// 在途操作失败 → 关闭票据必须 Blocked（不假装 Ready）。
#[test]
fn close_is_blocked_when_the_in_flight_operation_fails() {
    let gate = TestGate::new();
    gate.expect_blocking(Some("模拟 durable 未确认"), &[], "", &[]);
    let service = service_with_gate(gate.clone());

    let op_id = service.try_command(Command::Join).expect("加入命令");
    let ticket = service.request_stop();
    assert_eq!(service.poll_close(&ticket), CloseState::Pending);

    gate.release();
    assert!(service.reap_worker(), "worker 必须退出");
    let events = service.poll();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Completed {
                op_id: completed,
                result: OpResult::Failed { .. },
                ..
            } if *completed == op_id
        )),
        "必须有失败终态：{events:?}"
    );
    let close = service.poll_close(&ticket);
    match close {
        CloseState::Blocked(reason) => {
            assert!(
                reason.contains("durable 未确认"),
                "阻断原因必须保留原文：{reason}"
            );
        }
        other => panic!("失败后关闭必须 Blocked，实际 {other:?}"),
    }
    assert!(
        service
            .snapshot()
            .last_error
            .is_some_and(|error| error.contains("模拟 durable 未确认")),
        "面板必须能看到最近错误"
    );
}

/// 生产默认 runner：所有真实操作都明确"切片 4 未实现"，面板可见，绝不谎报成功。
#[test]
fn unimplemented_commands_surface_an_explicit_error() {
    let service = SyncService::new(default_config());
    let op_id = service.try_command(Command::Create).expect("创建命令");
    let ticket = service.request_stop();
    assert!(service.reap_worker(), "worker 必须退出");
    let events = service.poll();

    let completed: Vec<&Event> = events
        .iter()
        .filter(|event| matches!(event, Event::Completed { .. }))
        .collect();
    assert_eq!(completed.len(), 1, "必须有且只有一条 Completed：{events:?}");
    match completed[0] {
        Event::Completed {
            op_id: completed_op,
            changed_heads,
            result,
        } => {
            assert_eq!(*completed_op, op_id);
            assert!(changed_heads.is_empty(), "未实现的操作不能声称改了 head");
            let message = result.error().expect("必须是失败终态");
            assert!(
                message.contains("切片 4 未实现"),
                "错误必须说清未实现：{message}"
            );
            assert!(!result.is_ok(), "绝不能谎报成功");
        }
        other => unreachable!("已过滤：{other:?}"),
    }
    assert!(
        service
            .snapshot()
            .last_error
            .is_some_and(|error| error.contains("切片 4 未实现")),
        "面板的最近错误必须显示未实现"
    );
    // 失败的操作**不能**被当作"已 durable"放行关闭：票据必须 Blocked 且带原因。
    match service.poll_close(&ticket) {
        CloseState::Blocked(reason) => assert!(
            reason.contains("切片 4 未实现"),
            "阻断原因必须保留操作错误：{reason}"
        ),
        other => panic!("失败后关闭必须 Blocked，实际 {other:?}"),
    }
}

/// 操作 deadline（ADR 0012 S1）：只产生 `Cancelling`，**绝不**合成可解闸的终态。
///
/// 场景 ①：worker 仍卡在外部集成**之前**。超时后必须保持单操作占位、拒绝第二条命令、
/// 不放行任何 `Completed`；barrier 放行后真实结果才到达并收尾（迟到回执不能丢）。
#[test]
fn deadline_keeps_the_operation_slot_until_the_real_receipt_arrives() {
    let gate = TestGate::new();
    gate.expect_blocking(
        None,
        &["head-late"],
        "已同步 1 个头",
        &[(Phase::Transport, "等待 relay 响应")],
    );
    let mut config = default_config();
    config.operation_deadline = Some(Duration::ZERO);
    let service =
        SyncService::with_runner(config, Box::new(ControlledRunner { gate: gate.clone() }));

    let op_id = service.try_command(Command::SyncOnce).expect("同步命令");
    gate.wait_until("runner 已进入", |state| state.entered >= 1);

    // 第一次 poll：deadline 到期 → 只有 DeadlineExceeded（不是 Completed）。
    let events = service.poll();
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::DeadlineExceeded { op_id: timed_out, message }
                if *timed_out == op_id && message.contains("操作超时")
        )),
        "deadline 到期必须产生 DeadlineExceeded：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Completed { .. })),
        "看门狗绝不能伪造可解闸的终态：{events:?}"
    );
    let snapshot = service.snapshot();
    assert!(
        matches!(snapshot.state, DevSyncState::Cancelling { op_id: cancelled } if cancelled == op_id),
        "状态必须是 Cancelling：{:?}",
        snapshot.state
    );
    assert_eq!(
        snapshot.in_flight,
        Some((op_id, Command::SyncOnce.kind())),
        "单操作位必须仍被占着（worker 还在跑）"
    );
    assert_eq!(snapshot.deadline_exceeded, Some(op_id));
    assert!(
        snapshot.last_completed.is_none(),
        "超时不能生成 Completed（UI 会据此解闸）"
    );

    // 第二条操作命令必须明确 Busy/Cancelling，并且不能真的派发。
    let busy = service
        .try_command(Command::Create)
        .expect_err("超时后不得派发第二操作");
    assert_eq!(busy.reason, BusyReason::Cancelling);
    assert_eq!(busy.current, Some(op_id));
    assert!(
        busy.to_string().contains("真实收尾"),
        "拒绝原因必须说清在等真实收尾：{busy}"
    );
    assert_eq!(
        gate.state.lock().expect("gate 锁").entered,
        1,
        "第二条命令不能到达 runner"
    );

    // 放行 barrier → 真实结果到达（迟到回执被记录并注明）。
    gate.release();
    let events = poll_until_completed(&service, op_id, "迟到真实终态");
    let completed: Vec<&Event> = events
        .iter()
        .filter(|event| matches!(event, Event::Completed { .. }))
        .collect();
    assert_eq!(completed.len(), 1, "迟到终态必须被交付（不丢）：{events:?}");
    match completed[0] {
        Event::Completed {
            op_id: completed_op,
            changed_heads,
            result,
        } => {
            assert_eq!(*completed_op, op_id);
            assert_eq!(changed_heads, &vec!["head-late".to_owned()]);
            match result {
                OpResult::Ok { summary } => assert!(
                    summary.contains("超时后真实收尾"),
                    "迟到回执必须注明它是在超时之后到的：{summary}"
                ),
                other => panic!("真实结果是成功：{other:?}"),
            }
        }
        other => unreachable!("已过滤：{other:?}"),
    }
    let snapshot = service.snapshot();
    assert_eq!(snapshot.deadline_exceeded, None);
    assert_eq!(snapshot.state, DevSyncState::Ready);
    assert!(snapshot.last_error.is_none(), "真实成功收尾后错误必须清空");

    let _ticket = service.request_stop();
    assert!(service.reap_worker(), "worker 必须退出");
}

/// 场景 ②：worker **已经进入 integrate** 之后才超时 —— 仍不得提前解闸/派发第二条；
/// 真实事务结束后，已 durable 的变化（changed_heads）必须完整交付。
#[test]
fn deadline_after_integrate_started_waits_for_the_real_transaction() {
    let gate = TestGate::new();
    gate.expect_blocking(
        None,
        &["head-integrated"],
        "推送 1 / 拉取 2 / 整合 2 / 延后 0",
        &[
            (Phase::Transport, "pull 完成"),
            (Phase::Integrating, "进入 integrate（真实事务已开始）"),
        ],
    );
    let mut config = default_config();
    config.operation_deadline = Some(Duration::ZERO);
    let service =
        SyncService::with_runner(config, Box::new(ControlledRunner { gate: gate.clone() }));

    let op_id = service.try_command(Command::SyncOnce).expect("同步命令");
    gate.wait_until("runner 已进入", |state| state.entered >= 1);

    // 一次 poll 同时收进度与超时（worker 已经进入 integrate 相位后才被看门狗标记）。
    let events = service.poll();
    assert_eq!(
        service
            .snapshot()
            .progress
            .as_ref()
            .map(|(phase, _)| *phase),
        Some(Phase::Integrating),
        "runner 必须已经报出 integrate 相位"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::DeadlineExceeded { .. })),
        "必须记录超时：{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Completed { .. })),
        "integrate 中也不能伪造终态"
    );
    assert_eq!(
        service
            .try_command(Command::SyncOnce)
            .expect_err("单操作位不得复用")
            .reason,
        BusyReason::Cancelling
    );

    // 真实事务结束：已 durable 的变化必须完整交付，不能被"超时"吞掉。
    gate.release();
    let events = poll_until_completed(&service, op_id, "integrate 的真实终态");
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::Completed {
                op_id: completed,
                changed_heads,
                result: OpResult::Ok { .. },
            } if *completed == op_id && changed_heads == &vec!["head-integrated".to_owned()]
        )),
        "integrate 已开始的结果必须等真实事务结束后完整交付：{events:?}"
    );
    assert_eq!(
        service
            .snapshot()
            .last_completed
            .map(|(op, heads, _)| (op, heads)),
        Some((op_id, vec!["head-integrated".to_owned()]))
    );
    let _ticket = service.request_stop();
    assert!(service.reap_worker());
}

/// 取消通道（ADR 0012 S1）：`Cancel` 必须真的送达 worker（integration 前的复核点），
/// 且不能绕过单操作（在途时第二条操作仍被拒）。
#[test]
fn cancel_reaches_the_worker_and_never_allows_a_second_operation() {
    let gate = TestGate::new();
    gate.expect_blocking(None, &[], "跑完了", &[(Phase::Transport, "等待响应")]);
    let service = service_with_gate(gate.clone());

    let op_id = service.try_command(Command::SyncOnce).expect("同步");
    gate.wait_until("runner 已进入", |state| state.entered >= 1);
    assert_eq!(
        service
            .try_command(Command::SyncOnce)
            .expect_err("在途时必须拒绝")
            .reason,
        BusyReason::InFlight
    );
    let cancelled = service.try_command(Command::Cancel).expect("取消在途 op");
    assert_eq!(cancelled, op_id, "Cancel 返回在途 op");
    gate.wake();
    gate.wait_until("worker 必须看到取消标志", |state| {
        state.observed_cancel
    });

    gate.release();
    let events = poll_until_completed(&service, op_id, "取消后的真实终态");
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Completed { op_id: completed, result: OpResult::Ok { .. }, .. }
            if *completed == op_id
    )));
    // 取消过的操作不能画成 Ready（既有语义：执行体不可中断，如实显示 Paused）。
    assert!(matches!(
        service.snapshot().state,
        DevSyncState::Paused { .. }
    ));
    let _ticket = service.request_stop();
    assert!(service.reap_worker());
}

/// 手动同步回归：正常完成后单操作位释放，下一条命令可以再次派发。
#[test]
fn a_completed_manual_operation_frees_the_slot_for_the_next_one() {
    let gate = TestGate::new();
    gate.expect_blocking(None, &["head-1"], "已同步", &[]);
    let service = service_with_gate(gate.clone());

    let first = service.try_command(Command::SyncOnce).expect("第一条");
    gate.wait_until("第一条已进入", |state| state.entered >= 1);
    gate.release();
    poll_until_completed(&service, first, "第一条真实终态");
    assert_eq!(service.snapshot().state, DevSyncState::Ready);

    // 单操作位必须已经释放：第二条命令可以被接受（不是 InFlight/Cancelling）。
    let second = service
        .try_command(Command::SyncOnce)
        .expect("正常完成后必须能派发下一条");
    poll_until_completed(&service, second, "第二条真实终态");
    assert_eq!(service.snapshot().state, DevSyncState::Ready);

    let _ticket = service.request_stop();
    assert!(service.reap_worker(), "worker 必须退出");
    // join 建立 happens-before：把已发出的 `Stopped` 事件收进状态机。
    let _ = service.poll();
    // worker 已停止：新命令是 WorkerStopped（不是 Cancelling/InFlight）——证明超时标记
    // 没有残留到正常路径上。
    assert_eq!(
        service
            .try_command(Command::SyncOnce)
            .expect_err("worker 已停")
            .reason,
        BusyReason::WorkerStopped
    );
}

/// 传输预算接通（ADR 0012 S1）：`ServiceConfig` 的预算真的到 transport/runner 上。
#[test]
fn transport_budget_is_plumbed_from_config_to_the_runner_and_transport() {
    // 手动默认：与历史行为逐项一致（30/30s + 无整次请求预算）。
    let manual = ServiceConfig::new("http://127.0.0.1:18080");
    assert_eq!(manual.transport, DEFAULT_TRANSPORT_TIMEOUTS);
    assert_eq!(
        manual.build_transport().expect("loopback").timeouts(),
        DEFAULT_TRANSPORT_TIMEOUTS
    );

    // 自动同步建议预算：5/10/30s（S1 只定义、不启用）。
    let autosync = ServiceConfig::for_autosync("http://127.0.0.1:18080");
    assert_eq!(autosync.transport, AUTOSYNC_TRANSPORT_TIMEOUTS);
    assert_eq!(autosync.transport.connect, Duration::from_secs(5));
    assert_eq!(autosync.transport.io, Duration::from_secs(10));
    assert_eq!(
        autosync.transport.request_deadline,
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        autosync.build_transport().expect("loopback").timeouts(),
        AUTOSYNC_TRANSPORT_TIMEOUTS,
        "配置里的预算必须真的接上 transport 构造（不是只在结构体里躺着）"
    );

    // worker 的 runner 也拿到同一份预算。
    let runner = menghunote_app::dev_sync::DevLocalRunner::new_with_transport(
        "http://127.0.0.1:18080",
        PathBuf::from("/nonexistent-mn-dev-sync-runner/vault.sqlite3"),
        autosync.transport,
    );
    assert_eq!(runner.transport_timeouts(), AUTOSYNC_TRANSPORT_TIMEOUTS);
    // 向后兼容入口仍用历史默认。
    let legacy = menghunote_app::dev_sync::DevLocalRunner::new(
        "http://127.0.0.1:18080",
        PathBuf::from("/nonexistent-mn-dev-sync-runner/vault.sqlite3"),
    );
    assert_eq!(legacy.transport_timeouts(), DEFAULT_TRANSPORT_TIMEOUTS);
}
