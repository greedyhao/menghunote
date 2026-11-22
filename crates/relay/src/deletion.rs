//! **在线不可逆删除作业**（ADR 0018 §3 / 实施切片 S3）。
//!
//! 语义边界：
//! - 生命周期权威仍是 [`crate::registry`]（`vault_registry`）：`ACTIVE → DELETING → DELETED`，
//!   失败 `DELETE_FAILED`（同一 `delete_job_id` 可重试，**永不**重开 `ACTIVE`）。
//! - 标记事务（[`Deletion::start`]）在同一写事务里完成：`ACTIVE → DELETING` CAS、写
//!   `delete_job_id`/请求时刻（`deleted_at`，见下）、作废票据与临时认证（tokens/challenges）。
//!   **提交之后**才：置 admission fence、关闭该 vault 的 WS、返回 `202`/job —— 管理员断连
//!   不会取消作业（作业是独立 tokio task，不借用 HTTP 连接生命周期）。
//! - 双层 fence：
//!   1. **admission**（进程内 [`AdmissionFence`]）：数据面 middleware + WS 升级/发送先拒绝；
//!   2. **事务级复核**（[`crate::registry::ensure_not_fenced`] 在 `Db::read`/`Db::write` 内）：
//!      每个 scoped 事务执行时再查权威 registry，HTTP 取消/已入队 job 都绕不过去。
//! - purge 分批（[`PurgeLimits`]：行/字节/时间预算 + 批间 WAL 预算，单批一个短事务），行序 =
//!   [`PURGE_ORDER`]（FK 子→父，`control_commits` 最后）。其它租户的读写只与单批事务
//!   竞争，不会被整库长事务拖死。
//! - 终态在**同一个事务**里完成：零残留复核 + `PRAGMA foreign_key_check` + `DELETED` CAS。
//!   失败只写 `DELETE_FAILED` + 短错误码（不含租户内容），批次进度（`delete_progress`）保留。
//! - 崩溃恢复：启动时（绑定 listener 之前）[`Deletion::recover_startup`] 读 registry 删除态：
//!   `DELETING` 重新入队（幂等续跑），`DELETE_FAILED` 只围栏、等显式重试，
//!   `DELETED` 复核零残留；状态矛盾/缺失一律 fail closed。
//!
//! `deleted_at` 的双重口径（schema 13 不新增列，ADR 0018 S2 的列集不变）：
//! - `DELETING`/`DELETE_FAILED`：**删除请求时刻**（作业开始）；
//! - `DELETED`：删除**完成时刻**。
//!
//! 删除是**逻辑清除**，不保证 WAL/备份/客户端副本被擦除（ADR 0018 §3 末段）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Instant;

use rusqlite::Connection;
use tokio::sync::{broadcast, oneshot};

use crate::db::Db;
use crate::error::{Error, ErrorCode, Result};
use crate::ids::{random_id16, Id16};
use crate::registry::{self, LifecycleState, RegistryRow};

/// 删除作业按 FK **子 → 父**顺序清理的表（27 张租户表 + `vaults`）与每行的**字节口径**。
///
/// 顺序约束（schema 13 的复合 FK，**不关闭** `PRAGMA foreign_keys`）：
/// - `checkpoint_refs → checkpoints`、`object_parts/key_slots → objects`、
///   `objects → maintenance_reservations`、`maintenance_reservations → devices`；
/// - `challenges/tokens/invites → management_caps → devices/control_commits`；
/// - `device_credentials/device_grants/bootstrap_leases/device_acks → devices`；
/// - `devices/dedup_v2/vaults → control_commits`（所以 `control_commits` 最后）。
///
/// 第二项是该表**大字段的 SQL 字节表达式**：purge 在删除前先按它估算每行字节，
/// 从而让 [`PurgeLimits::bytes_per_batch`] 是真实字节预算（单行最大 4MiB 的 envelope，
/// 只按行数预算会让单批事务过大）。
///
/// 集合必须等于 [`crate::db::schema::TENANT_TABLE_NAMES`] + `vaults`（单元测试守这条不变量）。
pub const PURGE_TABLES: &[(&str, &str)] = &[
    ("checkpoint_refs", "0"),
    (
        "checkpoints",
        "COALESCE(LENGTH(signed_commit),0) + COALESCE(LENGTH(commit_signature),0)",
    ),
    (
        "object_parts",
        "COALESCE(LENGTH(envelope),0) + LENGTH(digest)",
    ),
    ("key_slots", "LENGTH(descriptor)"),
    (
        "objects",
        "COALESCE(LENGTH(signed_commit),0) + COALESCE(LENGTH(commit_signature),0)",
    ),
    ("maintenance_reservations", "0"),
    ("asset_objects", "0"),
    (
        "log",
        "LENGTH(event) + COALESCE(LENGTH(signature),0)",
    ),
    ("challenges", "LENGTH(nonce)"),
    ("tokens", "0"),
    ("invites", "0"),
    // ADR 0016 收件箱：`approval_signatures` FK 连 `approval_jobs`，
    // `approval_jobs` FK 连 `devices`/`management_caps`，所以必须先于它们删除。
    ("approval_signatures", "LENGTH(sig64)"),
    ("approval_jobs", "COALESCE(LENGTH(op_bytes),0)"),
    ("management_caps", "LENGTH(canonical_bytes)"),
    (
        "device_credentials",
        "LENGTH(certificate) + COALESCE(LENGTH(certificate_signature),0)",
    ),
    (
        "device_grants",
        "LENGTH(hpke_ciphertext) + LENGTH(signed_header) + COALESCE(LENGTH(signature),0)",
    ),
    ("bootstrap_leases", "0"),
    ("device_acks", "0"),
    (
        "devices",
        "LENGTH(certificate) + COALESCE(LENGTH(certificate_signature),0)",
    ),
    (
        "manager_keys",
        "LENGTH(transition) + COALESCE(LENGTH(old_signature),0) + COALESCE(LENGTH(new_signature),0)",
    ),
    ("epochs", "0"),
    ("rotations", "0"),
    ("dedup", "LENGTH(response)"),
    ("dedup_v2", "LENGTH(response)"),
    ("gc_plans", "LENGTH(proof)"),
    ("usage", "0"),
    ("vaults", "0"),
    ("control_commits", "LENGTH(signed_record)"),
];

/// 清理顺序的表名（[`PURGE_TABLES`] 的投影；常量由单元测试守一致）。
pub const PURGE_ORDER: &[&str] = &[
    "checkpoint_refs",
    "checkpoints",
    "object_parts",
    "key_slots",
    "objects",
    "maintenance_reservations",
    "asset_objects",
    "log",
    "challenges",
    "tokens",
    "invites",
    "approval_signatures",
    "approval_jobs",
    "management_caps",
    "device_credentials",
    "device_grants",
    "bootstrap_leases",
    "device_acks",
    "devices",
    "manager_keys",
    "epochs",
    "rotations",
    "dedup",
    "dedup_v2",
    "gc_plans",
    "usage",
    "vaults",
    "control_commits",
];

/// 每批 purge 的预算（行都是硬上界；字节/WAL 预算按 `wal_budget_bytes` 在批间 checkpoint）。
///
/// 生产默认值保证单批事务短（256 行 / 64 行一步 / 25ms 软预算 / 1MiB WAL）；
/// 测试用小阈值（几行）驱动确定性断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgeLimits {
    /// 每批最多删除的行数（**硬上界**，含票据行）。
    pub rows_per_batch: usize,
    /// 每批里单个「选行 → 删除」步骤的最大行数（决定 WAL/事务步长的粒度）。
    pub chunk_rows: usize,
    /// 单批删除的行**字节**预算（按 [`PURGE_TABLES`] 的字节表达式逐行累计）。
    pub bytes_per_batch: u64,
    /// 单批的软时间预算（毫秒；至少做一步，避免 0 预算死循环）。
    pub millis_per_batch: u64,
    /// WAL 字节预算：批次提交后 WAL 超过它就做一次 `wal_checkpoint(TRUNCATE)`。
    pub wal_budget_bytes: u64,
}

impl Default for PurgeLimits {
    fn default() -> Self {
        Self {
            rows_per_batch: 256,
            chunk_rows: 64,
            bytes_per_batch: 4 * 1024 * 1024,
            millis_per_batch: 25,
            wal_budget_bytes: 1024 * 1024,
        }
    }
}

impl PurgeLimits {
    /// 归一化：行/块至少 1，时间预算至少 1ms（0 会让"至少做一步"失去意义）。
    fn normalized(self) -> Self {
        Self {
            rows_per_batch: self.rows_per_batch.max(1),
            chunk_rows: self.chunk_rows.max(1),
            bytes_per_batch: self.bytes_per_batch,
            millis_per_batch: self.millis_per_batch.max(1),
            wal_budget_bytes: self.wal_budget_bytes,
        }
    }
}

/// 进程内 admission 关闭原因（与 registry 删除三态一一对应；只用于诊断/测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedState {
    /// 删除作业进行中。
    Deleting,
    /// 删除失败，等同一 job 重试。
    DeleteFailed,
    /// 已删除（墓碑）。
    Deleted,
}

impl ClosedState {
    fn of(state: LifecycleState) -> Option<Self> {
        match state {
            LifecycleState::Deleting => Some(Self::Deleting),
            LifecycleState::DeleteFailed => Some(Self::DeleteFailed),
            LifecycleState::Deleted => Some(Self::Deleted),
            _ => None,
        }
    }
}

/// **admission fence**（ADR 0018 §3）：删除态 vault 的数据面入口在内存里直接拒绝。
///
/// 进程启动时从 registry 加载（[`Deletion::recover_startup`]）；删除标记提交后立刻插入
/// （见 [`Deletion::start`]）。它只是**快速拒绝**；权威判定仍由事务级复核做
/// （[`crate::registry::ensure_not_fenced`]），所以内存表与 DB 短暂不一致不会放行写入。
#[derive(Debug, Default)]
pub struct AdmissionFence {
    closed: RwLock<HashMap<Id16, ClosedState>>,
}

impl AdmissionFence {
    /// 空 fence（未加载/无删除作业）。
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::RwLockReadGuard<'_, HashMap<Id16, ClosedState>> {
        self.closed
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_mut(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<Id16, ClosedState>> {
        self.closed
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 关闭一个 vault 的 admission（幂等；不会因为重复 start 而"打开"）。
    pub fn close(&self, vault_id: Id16, state: ClosedState) {
        self.lock_mut().entry(vault_id).or_insert(state);
    }

    /// 该 vault 的关闭原因（`None` = 可服务）。
    pub fn closed_state(&self, vault_id: &Id16) -> Option<ClosedState> {
        self.lock().get(vault_id).copied()
    }

    /// 是否处于删除态。
    pub fn is_closed(&self, vault_id: &Id16) -> bool {
        self.closed_state(vault_id).is_some()
    }

    /// admission 检查：删除态 → `410 VAULT_DELETED`（不可重试）。
    pub fn check(&self, vault_id: &Id16) -> Result<()> {
        match self.closed_state(vault_id) {
            Some(_) => Err(Error::api(ErrorCode::VaultDeleted)),
            None => Ok(()),
        }
    }

    /// 当前围栏数量（诊断/测试）。
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

/// 一个删除作业的 registry 投影（运维/测试；不含租户内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteJob {
    /// 目标 vault。
    pub vault_id: Id16,
    /// 作业 id（schema 允许迁移墓碑没有作业 id；本模块创建的作业一定有）。
    pub job_id: Option<Id16>,
    /// `DELETING` / `DELETE_FAILED` / `DELETED`。
    pub state: LifecycleState,
    /// `registry.deleted_at`：请求时刻（进行中/失败）或完成时刻（已删除）。
    pub at: Option<i64>,
    /// 已彻底清空的表数（`0..=PURGE_ORDER.len()`）。
    pub progress: u64,
    /// 失败原因（短错误码，不含租户内容）。
    pub error: Option<String>,
}

impl DeleteJob {
    fn from_row(row: &RegistryRow) -> Option<Self> {
        if !row.state.is_delete_state() {
            return None;
        }
        Some(Self {
            vault_id: row.vault_id,
            job_id: row.delete_job_id,
            state: row.state,
            at: row.deleted_at,
            progress: row.delete_progress,
            error: row.delete_error.clone(),
        })
    }

    /// 已完成删除（墓碑）。
    pub fn is_deleted(&self) -> bool {
        self.state == LifecycleState::Deleted
    }
}

/// [`Deletion::start`] 的结果：调用方按变体回 `202`/页面（不泄露租户内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// 本次真正开始（`ACTIVE → DELETING`）。
    Started(DeleteJob),
    /// 已在删除中（幂等：返回同一 job）。
    AlreadyDeleting(DeleteJob),
    /// 上次失败，本次重新武装（**同一 job id**）。
    Retried(DeleteJob),
    /// 已是墓碑（幂等：不再有任何动作）。
    AlreadyDeleted(DeleteJob),
}

impl StartOutcome {
    /// 对应作业。
    pub fn job(&self) -> &DeleteJob {
        match self {
            Self::Started(job)
            | Self::AlreadyDeleting(job)
            | Self::Retried(job)
            | Self::AlreadyDeleted(job) => job,
        }
    }

    /// 是否是本次新开始的作业（`202` 的"已持久标记"语义主要看这个）。
    pub fn started(&self) -> bool {
        matches!(self, Self::Started(_))
    }
}

/// 作业事件（诊断/测试的显式收敛点；不含租户数据）。
///
/// 测试用 [`Deletion::subscribe`] 等终态事件（`Deleted`/`Failed`/`Stopped`）收敛，
/// **不用 sleep**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteEvent {
    /// 作业开始/恢复（已持久 `DELETING`）。
    Started {
        /// 目标 vault。
        vault_id: Id16,
        /// 作业 id。
        job_id: Id16,
        /// 起始进度（崩溃恢复续跑时 > 0）。
        progress: u64,
    },
    /// 一个 purge 批次已提交（单批一个短事务）。
    Batch {
        /// 目标 vault。
        vault_id: Id16,
        /// 作业 id。
        job_id: Id16,
        /// 本批删除行数（≤ `PurgeLimits::rows_per_batch`）。
        rows: u64,
        /// 本批删除的行字节量（按 `PURGE_TABLES` 的字节表达式；≤ 字节预算 + 单行）。
        bytes: u64,
        /// 已彻底清空的表数。
        tables_done: u64,
        /// 本批耗时（毫秒，诊断用）。
        elapsed_ms: u64,
        /// 批后 WAL 字节数。
        wal_bytes: u64,
        /// 本批后是否做过 WAL checkpoint（超预算时）。
        wal_checkpointed: bool,
    },
    /// 失败（registry 已写 `DELETE_FAILED`）。
    Failed {
        /// 目标 vault。
        vault_id: Id16,
        /// 作业 id。
        job_id: Id16,
        /// 保留的进度。
        progress: u64,
    },
    /// 终态 `DELETED`（零残留复核通过）。
    Deleted {
        /// 目标 vault。
        vault_id: Id16,
        /// 作业 id。
        job_id: Id16,
        /// 进度（= `PURGE_ORDER.len()`）。
        progress: u64,
    },
    /// 测试 failpoint 让作业在**批次之间**停下（模拟进程崩溃：只留持久 `DELETING`）。
    Stopped {
        /// 目标 vault。
        vault_id: Id16,
        /// 作业 id。
        job_id: Id16,
        /// 已持久化的进度。
        progress: u64,
    },
}

/// 测试注入点（只在测试里设置；产品路径永远 `None`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeFailpoint {
    /// 完成 N 个批次后**静默停止**（不写 `DELETE_FAILED`）——模拟进程崩溃。
    StopAfterBatches(u64),
    /// 完成 N 个批次后写 `DELETE_FAILED`（模拟不可恢复错误）。
    FailAfterBatches(u64),
}

#[derive(Debug, Default)]
struct WsState {
    next_id: u64,
    conns: HashMap<Id16, Vec<(u64, oneshot::Sender<()>)>>,
}

/// 每 vault 的 WS 连接注册表（ADR 0018 §3：删除开始时主动 close，慢发送限时）。
#[derive(Debug, Default)]
pub struct WsConnections {
    inner: Mutex<WsState>,
}

impl WsConnections {
    fn lock(&self) -> MutexGuard<'_, WsState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn register(self: &Arc<Self>, vault_id: Id16) -> (WsGuard, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let mut state = self.lock();
        state.next_id = state.next_id.wrapping_add(1);
        let id = state.next_id;
        state.conns.entry(vault_id).or_default().push((id, tx));
        (
            WsGuard {
                conns: Arc::clone(self),
                vault_id,
                id,
            },
            rx,
        )
    }

    fn unregister(&self, vault_id: Id16, id: u64) {
        let mut state = self.lock();
        let Some(list) = state.conns.get_mut(&vault_id) else {
            return;
        };
        list.retain(|(entry, _)| *entry != id);
        if list.is_empty() {
            state.conns.remove(&vault_id);
        }
    }

    /// 主动关闭该 vault 的全部连接（幂等；新连接由 admission fence 拒绝）。
    pub fn close_vault(&self, vault_id: &Id16) {
        let senders = self.lock().conns.remove(vault_id).unwrap_or_default();
        for (_, tx) in senders {
            let _ = tx.send(());
        }
    }

    /// 当前注册的连接数（测试）。
    pub fn active(&self, vault_id: &Id16) -> usize {
        self.lock().conns.get(vault_id).map(Vec::len).unwrap_or(0)
    }
}

/// WS 连接的注册守卫：连接任务结束时注销（避免注册表泄漏）。
#[derive(Debug)]
pub struct WsGuard {
    conns: Arc<WsConnections>,
    vault_id: Id16,
    id: u64,
}

impl Drop for WsGuard {
    fn drop(&mut self) {
        self.conns.unregister(self.vault_id, self.id);
    }
}

/// 启动恢复报告（诊断/测试；不含租户内容）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// 被围栏的 vault（删除三态）。
    pub fenced: Vec<Id16>,
    /// 重新入队的 `DELETING` 作业。
    pub resumed: Vec<Id16>,
    /// 保持 `DELETE_FAILED`（等显式重试）的数量。
    pub failed: usize,
    /// 已删除墓碑数量。
    pub deleted: usize,
}

impl RecoveryReport {
    /// 是否需要打印/断言（没有任何删除态时为空）。
    pub fn is_empty(&self) -> bool {
        self.fenced.is_empty()
    }
}

/// 删除作业运行时（数据面 `AppState` 与 admin listener **共享同一个实例**）。
#[derive(Debug)]
pub struct Deletion {
    db: Db,
    admission: Arc<AdmissionFence>,
    ws: Arc<WsConnections>,
    limits: PurgeLimits,
    failpoint: Mutex<Option<PurgeFailpoint>>,
    events: broadcast::Sender<DeleteEvent>,
    jobs: Mutex<HashMap<Id16, tokio::task::JoinHandle<()>>>,
}

impl Deletion {
    /// 生产默认预算。
    pub fn new(db: Db) -> Self {
        Self::with_limits(db, PurgeLimits::default())
    }

    /// 指定预算（运维/测试）。
    pub fn with_limits(db: Db, limits: PurgeLimits) -> Self {
        let (events, _) = broadcast::channel(1024);
        Self {
            db,
            admission: Arc::new(AdmissionFence::new()),
            ws: Arc::new(WsConnections::default()),
            limits: limits.normalized(),
            failpoint: Mutex::new(None),
            events,
            jobs: Mutex::new(HashMap::new()),
        }
    }

    /// admission fence（数据面 middleware / WS 升级用）。
    pub fn admission(&self) -> &AdmissionFence {
        &self.admission
    }

    /// WS 连接注册表。
    pub fn ws(&self) -> &Arc<WsConnections> {
        &self.ws
    }

    /// 生效预算。
    pub fn limits(&self) -> PurgeLimits {
        self.limits
    }

    /// 订阅作业事件（测试/诊断的显式收敛点）。
    pub fn subscribe(&self) -> broadcast::Receiver<DeleteEvent> {
        self.events.subscribe()
    }

    /// 设置测试 failpoint（`None` = 关闭）。
    pub fn set_failpoint(&self, failpoint: Option<PurgeFailpoint>) {
        *self
            .failpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = failpoint;
    }

    fn failpoint(&self) -> Option<PurgeFailpoint> {
        *self
            .failpoint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn emit(&self, event: DeleteEvent) {
        // 无订阅者/落后都是正常情况（事件只是诊断）。
        let _ = self.events.send(event);
    }

    fn jobs_lock(&self) -> MutexGuard<'_, HashMap<Id16, tokio::task::JoinHandle<()>>> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 读该 vault 的删除作业（非删除态 = `None`）。
    pub fn status(&self, vault_id: Id16) -> Result<Option<DeleteJob>> {
        self.db.read_instance(move |conn| {
            Ok(registry::load_raw(conn, &vault_id)?.and_then(|row| DeleteJob::from_row(&row)))
        })
    }

    /// 启动/幂等重发删除请求（**提交标记后才返回**；ADR 0018 §3）。
    ///
    /// - `ACTIVE` → 标记 `DELETING` + 作废票据/临时认证 → 围栏 + 关 WS + 起作业；
    /// - `DELETING`/`DELETE_FAILED` → 返回同一 job（后者重新武装同一 job）；
    /// - `DELETED` → 幂等返回墓碑；
    /// - `RESERVED`/`EXPIRED`/`CANCELLED` → 409（预留请走 cancel；终态没有可删租户数据）。
    pub async fn start(self: &Arc<Self>, vault_id: Id16) -> Result<StartOutcome> {
        let clock = self.db.clock_handle();
        let candidate = random_id16()?;
        let outcome = self
            .db
            .write_instance(move |conn| mark_deleting(conn, &vault_id, clock.now_secs(), candidate))
            .await?;
        let (outcome, job_id) = match (outcome, candidate) {
            (
                MarkOutcome::Marked {
                    job_id,
                    requested_at,
                },
                _,
            ) => (
                StartOutcome::Started(DeleteJob {
                    vault_id,
                    job_id: Some(job_id),
                    state: LifecycleState::Deleting,
                    at: Some(requested_at),
                    progress: 0,
                    error: None,
                }),
                job_id,
            ),
            (
                MarkOutcome::Resume {
                    job_id,
                    requested_at,
                    progress,
                },
                _,
            ) => (
                StartOutcome::AlreadyDeleting(DeleteJob {
                    vault_id,
                    job_id: Some(job_id),
                    state: LifecycleState::Deleting,
                    at: Some(requested_at),
                    progress,
                    error: None,
                }),
                job_id,
            ),
            (
                MarkOutcome::Retry {
                    job_id,
                    requested_at,
                    progress,
                },
                _,
            ) => (
                StartOutcome::Retried(DeleteJob {
                    vault_id,
                    job_id: Some(job_id),
                    state: LifecycleState::Deleting,
                    at: Some(requested_at),
                    progress,
                    error: None,
                }),
                job_id,
            ),
            (MarkOutcome::Deleted(job), _) => {
                self.admission.close(vault_id, ClosedState::Deleted);
                return Ok(StartOutcome::AlreadyDeleted(job));
            }
        };
        // 标记事务已提交：现在才关闭数据面 + 通知 WS（作业不依赖本调用者的连接）。
        self.admission.close(
            vault_id,
            ClosedState::of(outcome.job().state).unwrap_or(ClosedState::Deleting),
        );
        self.ws.close_vault(&vault_id);
        // `AlreadyDeleting` 只表示持久状态；若上一个作业已因测试 failpoint 结束，
        // 不要因为重复 POST 再起第二个作业（会重复发 Stopped/进度事件）。真正的
        // 崩溃恢复走 `recover_startup`；`Started`/`Retried` 才允许替换已结束的句柄。
        let replace_finished =
            matches!(outcome, StartOutcome::Started(_) | StartOutcome::Retried(_));
        self.spawn_job(vault_id, job_id, replace_finished);
        Ok(outcome)
    }

    /// 启动时（**绑定 listener 之前**）加载 fence 并恢复 `DELETING` 作业（ADR 0018 §3）。
    ///
    /// 状态矛盾/缺失 fail closed：`DELETING`/`DELETE_FAILED` 缺 `delete_job_id`、`DELETED`
    /// 仍有租户行（或 `vaults` 行）都会返回错误让启动中止——宁可起不来，也不对外服务一个
    /// "说删了但其实还在"的实例。
    pub async fn recover_startup(self: &Arc<Self>) -> Result<RecoveryReport> {
        let rows = self.db.read_instance(registry::load_delete_rows)?;
        let mut report = RecoveryReport::default();
        for row in rows {
            match row.state {
                LifecycleState::Deleting => {
                    let job_id = row
                        .delete_job_id
                        .ok_or(Error::Invariant("recover: DELETING without delete_job_id"))?;
                    self.admission.close(row.vault_id, ClosedState::Deleting);
                    report.fenced.push(row.vault_id);
                    report.resumed.push(row.vault_id);
                    self.spawn_job(row.vault_id, job_id, true);
                }
                LifecycleState::DeleteFailed => {
                    row.delete_job_id.ok_or(Error::Invariant(
                        "recover: DELETE_FAILED without delete_job_id",
                    ))?;
                    self.admission
                        .close(row.vault_id, ClosedState::DeleteFailed);
                    report.fenced.push(row.vault_id);
                    report.failed += 1;
                }
                LifecycleState::Deleted => {
                    let vault_id = row.vault_id;
                    let residue = self
                        .db
                        .read_instance(move |conn| tenant_residue(conn, &vault_id))?;
                    if residue != 0 {
                        return Err(Error::Invariant(
                            "recover: DELETED tombstone still has tenant rows",
                        ));
                    }
                    self.admission.close(row.vault_id, ClosedState::Deleted);
                    report.fenced.push(row.vault_id);
                    report.deleted += 1;
                }
                _ => {
                    return Err(Error::Invariant(
                        "recover: registry returned non-delete state",
                    ))
                }
            }
        }
        Ok(report)
    }

    /// 注册一个 WS 连接：已围栏 → `None`（调用方必须拒绝/立即关闭）。
    ///
    /// 注册与围栏有竞态：注册后**再查一次** fence，若已关闭就立刻发关闭信号（连接不会漏关）。
    pub fn register_ws(&self, vault_id: Id16) -> Option<(WsGuard, oneshot::Receiver<()>)> {
        let out = self.ws.register(vault_id);
        if self.admission.is_closed(&vault_id) {
            self.ws.close_vault(&vault_id);
        }
        Some(out)
    }

    /// 主动关闭该 vault 的全部 WS（删除开始时调用；幂等）。
    pub fn close_ws(&self, vault_id: &Id16) {
        self.ws.close_vault(vault_id);
    }

    fn spawn_job(self: &Arc<Self>, vault_id: Id16, job_id: Id16, replace_finished: bool) {
        let mut jobs = self.jobs_lock();
        if let Some(existing) = jobs.get(&vault_id) {
            if !existing.is_finished() {
                return;
            }
            if !replace_finished {
                return;
            }
        }
        jobs.retain(|id, handle| *id == vault_id || !handle.is_finished());
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move { this.run_job(vault_id, job_id).await });
        jobs.insert(vault_id, handle);
    }

    /// 收尾（测试/进程退出）：中止并等待所有作业任务（批次之间可中止；进行中的事务会提交，
    /// 进度只增不减，重启后从持久进度续跑）。
    pub async fn shutdown(&self) {
        let handles: Vec<_> = self.jobs_lock().drain().map(|(_, handle)| handle).collect();
        for handle in handles {
            handle.abort();
            let _ = handle.await;
        }
    }

    async fn run_job(self: Arc<Self>, vault_id: Id16, job_id: Id16) {
        let limits = self.limits;
        let mut batches: u64 = 0;
        let progress = self.progress(vault_id).await;
        self.emit(DeleteEvent::Started {
            vault_id,
            job_id,
            progress,
        });
        loop {
            match self.failpoint() {
                Some(PurgeFailpoint::StopAfterBatches(n)) if batches >= n => {
                    let progress = self.progress(vault_id).await;
                    self.emit(DeleteEvent::Stopped {
                        vault_id,
                        job_id,
                        progress,
                    });
                    return;
                }
                Some(PurgeFailpoint::FailAfterBatches(n)) if batches >= n => {
                    self.record_failure(vault_id, "injected").await;
                    self.failed_event(vault_id, job_id).await;
                    return;
                }
                _ => {}
            }
            let clock = self.db.clock_handle();
            let id = vault_id;
            let batch = self
                .db
                .write_maintenance(move |conn| purge_batch(conn, &id, clock.now_secs(), limits))
                .await;
            match batch {
                Ok(batch) => {
                    batches += 1;
                    let (wal_bytes, wal_checkpointed) = self.wal_guard(limits).await;
                    self.emit(DeleteEvent::Batch {
                        vault_id,
                        job_id,
                        rows: batch.rows,
                        bytes: batch.bytes,
                        tables_done: batch.tables_done,
                        elapsed_ms: batch.elapsed_ms,
                        wal_bytes,
                        wal_checkpointed,
                    });
                    if batch.deleted {
                        let progress = self.progress(vault_id).await;
                        self.emit(DeleteEvent::Deleted {
                            vault_id,
                            job_id,
                            progress,
                        });
                        return;
                    }
                    if batch.rows == 0 {
                        // 无可删行但未终态 = 状态矛盾（fail closed，不空转）。
                        self.record_failure(vault_id, "no progress").await;
                        self.failed_event(vault_id, job_id).await;
                        return;
                    }
                }
                Err(err) => {
                    self.record_failure(vault_id, failure_kind(&err)).await;
                    self.failed_event(vault_id, job_id).await;
                    return;
                }
            }
            // 批次之间让出执行权：其它租户请求/GC/WS 事件不会被一个长任务饿死。
            tokio::task::yield_now().await;
        }
    }

    async fn wal_guard(&self, limits: PurgeLimits) -> (u64, bool) {
        let bytes = self.db.wal_size_bytes();
        if bytes <= limits.wal_budget_bytes {
            return (bytes, false);
        }
        let _ = self.db.checkpoint_wal(true).await;
        (self.db.wal_size_bytes(), true)
    }

    async fn progress(&self, vault_id: Id16) -> u64 {
        self.status(vault_id)
            .ok()
            .flatten()
            .map(|job| job.progress)
            .unwrap_or(0)
    }

    async fn failed_event(&self, vault_id: Id16, job_id: Id16) {
        let progress = self.progress(vault_id).await;
        self.emit(DeleteEvent::Failed {
            vault_id,
            job_id,
            progress,
        });
    }

    /// 失败只写 `DELETE_FAILED` + 短错误码（不改 `deleted_at`：保留删除请求时刻）。
    async fn record_failure(&self, vault_id: Id16, kind: &'static str) {
        let _ = self
            .db
            .write_instance(move |conn| {
                conn.execute(
                    "UPDATE vault_registry SET state = 'DELETE_FAILED', delete_error = ?1
                      WHERE vault_id = ?2 AND state = 'DELETING'",
                    (format!("purge failed: {kind}"), vault_id.as_slice()),
                )?;
                Ok(())
            })
            .await;
    }
}

/// 标记事务的结果（[`mark_deleting`]）。
enum MarkOutcome {
    Marked {
        job_id: Id16,
        requested_at: i64,
    },
    Resume {
        job_id: Id16,
        requested_at: i64,
        progress: u64,
    },
    Retry {
        job_id: Id16,
        requested_at: i64,
        progress: u64,
    },
    Deleted(DeleteJob),
}

/// `ACTIVE → DELETING` 标记（**必须在写事务内**运行；ADR 0018 §3 步 2）。
fn mark_deleting(
    conn: &Connection,
    vault_id: &Id16,
    now: i64,
    candidate: Id16,
) -> Result<MarkOutcome> {
    let Some(row) = registry::load_raw(conn, vault_id)? else {
        return Err(Error::api(ErrorCode::NotFound));
    };
    match row.state {
        LifecycleState::Active => {
            let changed = conn.execute(
                "UPDATE vault_registry
                    SET state = 'DELETING', delete_job_id = ?1, deleted_at = ?2,
                        delete_progress = 0, delete_error = NULL
                  WHERE vault_id = ?3 AND state = 'ACTIVE'",
                (candidate.as_slice(), now, vault_id.as_slice()),
            )?;
            if changed != 1 {
                return Err(Error::api(ErrorCode::CasConflict));
            }
            // 作废该 vault 的全部创建票据（未用/过期/兄弟；已消费的历史行留给 purge 清）。
            conn.execute(
                "UPDATE create_tickets SET quota = 0, consumed_at = ?1
                  WHERE vault_id = ?2 AND consumed_at IS NULL",
                (now, vault_id.as_slice()),
            )?;
            // 作废临时认证：challenge 与 token 全删（realm 1/2/3/4 都算）。
            conn.execute(
                "DELETE FROM tokens WHERE vault_id = ?1",
                [vault_id.as_slice()],
            )?;
            conn.execute(
                "DELETE FROM challenges WHERE vault_id = ?1",
                [vault_id.as_slice()],
            )?;
            Ok(MarkOutcome::Marked {
                job_id: candidate,
                requested_at: now,
            })
        }
        LifecycleState::Deleting => {
            let job_id = row
                .delete_job_id
                .ok_or(Error::Invariant("start: DELETING without delete_job_id"))?;
            Ok(MarkOutcome::Resume {
                job_id,
                requested_at: row.deleted_at.unwrap_or(now),
                progress: row.delete_progress,
            })
        }
        LifecycleState::DeleteFailed => {
            let job_id = row.delete_job_id.ok_or(Error::Invariant(
                "start: DELETE_FAILED without delete_job_id",
            ))?;
            let changed = conn.execute(
                "UPDATE vault_registry SET state = 'DELETING', delete_error = NULL
                  WHERE vault_id = ?1 AND state = 'DELETE_FAILED'",
                [vault_id.as_slice()],
            )?;
            if changed != 1 {
                return Err(Error::api(ErrorCode::CasConflict));
            }
            Ok(MarkOutcome::Retry {
                job_id,
                requested_at: row.deleted_at.unwrap_or(now),
                progress: row.delete_progress,
            })
        }
        LifecycleState::Deleted => Ok(MarkOutcome::Deleted(
            DeleteJob::from_row(&row).ok_or(Error::Invariant("start: DELETED row"))?,
        )),
        // 预留要先 cancel（不能把一张票据直接变成墓碑）；EXPIRED/CANCELLED 没有租户数据，
        // 也没有可重开/可再删的东西。
        LifecycleState::Reserved => Err(Error::api(ErrorCode::CasConflict)),
        LifecycleState::Expired | LifecycleState::Cancelled => {
            Err(Error::api(ErrorCode::InvalidState))
        }
    }
}

/// 一批 purge 的可观测量（单批 = 一个短事务）。
struct BatchOutcome {
    rows: u64,
    bytes: u64,
    tables_done: u64,
    deleted: bool,
    elapsed_ms: u64,
}

/// 分批清理一批行；预算耗尽就返回（下一批继续）。**必须在写事务内运行**。
///
/// 顺序：先清该 vault 的创建票据（量小、FK 指向 registry 墓碑），再按 [`PURGE_ORDER`]
/// 从第一张非空表开始，每步一条 `DELETE ... LIMIT`。所有表都空且票据清零 → 同一事务内
/// 做零残留/FK 复核并写 `DELETED` 终态。
fn purge_batch(
    conn: &Connection,
    vault_id: &Id16,
    now: i64,
    limits: PurgeLimits,
) -> Result<BatchOutcome> {
    let limits = limits.normalized();
    let started = Instant::now();
    let mut rows: u64 = 0;
    let mut bytes: u64 = 0;
    rows += u64::try_from(conn.execute(
        "DELETE FROM create_tickets WHERE vault_id = ?1",
        [vault_id.as_slice()],
    )?)
    .unwrap_or(0);
    let mut rows_left = limits
        .rows_per_batch
        .saturating_sub(usize::try_from(rows).unwrap_or(usize::MAX));
    let mut bytes_left = limits.bytes_per_batch;

    'tables: for (table, size_expr) in PURGE_TABLES {
        loop {
            // 至少做一步：0 行/0 字节/0ms 预算也不至于空转（normalized 已保证行/时间 ≥1）。
            if rows_left == 0
                || bytes_left == 0
                || (rows > 0 && started.elapsed().as_millis() as u64 >= limits.millis_per_batch)
            {
                break 'tables;
            }
            let step = limits.chunk_rows.min(rows_left).max(1);
            // 先按字节表达式选行：严格按"剩余字节预算"截断（首行总是允许，保证有进展）。
            let mut selected: Vec<(i64, u64)> = Vec::new();
            let mut selected_bytes: u64 = 0;
            let mut byte_cut = false;
            {
                let sql =
                    format!("SELECT rowid, {size_expr} FROM {table} WHERE vault_id = ?1 LIMIT ?2");
                let mut stmt = conn.prepare(&sql)?;
                let mut selected_rows =
                    stmt.query((vault_id.as_slice(), i64::try_from(step).unwrap_or(i64::MAX)))?;
                while let Some(row) = selected_rows.next()? {
                    let rowid: i64 = row.get(0)?;
                    let size: i64 = row.get(1)?;
                    let size = u64::try_from(size.max(0)).unwrap_or(u64::MAX);
                    if !selected.is_empty() && size > bytes_left.saturating_sub(selected_bytes) {
                        byte_cut = true;
                        break;
                    }
                    selected_bytes = selected_bytes.saturating_add(size);
                    selected.push((rowid, size));
                }
            }
            if selected.is_empty() {
                // 该表已空 → 下一张。
                break;
            }
            let placeholders = (0..selected.len())
                .map(|index| format!("?{}", index + 2))
                .collect::<Vec<_>>()
                .join(",");
            let sql =
                format!("DELETE FROM {table} WHERE vault_id = ?1 AND rowid IN ({placeholders})");
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(selected.len() + 1);
            params.push(vault_id);
            let rowids: Vec<i64> = selected.iter().map(|(rowid, _)| *rowid).collect();
            for rowid in &rowids {
                params.push(rowid);
            }
            let deleted = conn.execute(&sql, params.as_slice())?;
            let deleted = u64::try_from(deleted).unwrap_or(0);
            rows += deleted;
            bytes = bytes.saturating_add(selected_bytes);
            rows_left = rows_left.saturating_sub(usize::try_from(deleted).unwrap_or(usize::MAX));
            bytes_left = bytes_left.saturating_sub(selected_bytes);
            if deleted < u64::try_from(selected.len()).unwrap_or(u64::MAX) {
                return Err(Error::Invariant("delete: concurrent row removal"));
            }
            // 字节预算被截断（含"单行就超预算"）：本批到此为止，**不能**跳到后面的表
            // （后面的表是父表，子表没删干净时删父行会撞 FK）。
            if byte_cut || selected_bytes >= limits.bytes_per_batch || bytes_left == 0 {
                break 'tables;
            }
            // 选中数少于步长 = 表已空；否则本批预算允许再删一步。
            if selected.len() < step {
                break;
            }
        }
    }

    let tables_done = leading_empty_tables(conn, vault_id)?;
    let tickets_left: i64 = conn.query_row(
        "SELECT COUNT(*) FROM create_tickets WHERE vault_id = ?1",
        [vault_id.as_slice()],
        |row| row.get(0),
    )?;
    // 进度随批次持久化（崩溃续跑靠 registry 的 `delete_progress`，不额外建游标表）。
    conn.execute(
        "UPDATE vault_registry SET delete_progress = ?1
          WHERE vault_id = ?2 AND state = 'DELETING'",
        (
            i64::try_from(tables_done).unwrap_or(i64::MAX),
            vault_id.as_slice(),
        ),
    )?;
    let mut deleted = false;
    if tables_done as usize == PURGE_ORDER.len() && tickets_left == 0 {
        finalize(conn, vault_id, now)?;
        deleted = true;
    }
    Ok(BatchOutcome {
        rows,
        bytes,
        tables_done,
        deleted,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// 从 [`PURGE_ORDER`] 队首起连续清空的表数（= 持久化的 `delete_progress`）。
///
/// 崩溃续跑不需要额外游标：每次从头扫描，第一张非空表就是下一批的起点。
fn leading_empty_tables(conn: &Connection, vault_id: &Id16) -> Result<u64> {
    for (index, table) in PURGE_ORDER.iter().enumerate() {
        let exists: i64 = conn.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE vault_id = ?1)"),
            [vault_id.as_slice()],
            |row| row.get(0),
        )?;
        if exists != 0 {
            return Ok(index as u64);
        }
    }
    Ok(PURGE_ORDER.len() as u64)
}

/// 终态（**与最后一批删除同一个事务**）：零残留 + FK 复核 → `DELETING → DELETED` CAS。
fn finalize(conn: &Connection, vault_id: &Id16, now: i64) -> Result<()> {
    for table in PURGE_ORDER {
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE vault_id = ?1"),
            [vault_id.as_slice()],
            |row| row.get(0),
        )?;
        if count != 0 {
            return Err(Error::Invariant("delete: tenant rows remain"));
        }
    }
    let tickets: i64 = conn.query_row(
        "SELECT COUNT(*) FROM create_tickets WHERE vault_id = ?1",
        [vault_id.as_slice()],
        |row| row.get(0),
    )?;
    if tickets != 0 {
        return Err(Error::Invariant("delete: tickets remain"));
    }
    // 不关 FK：终态前显式复核一次（跨租户/拓扑错误必须 fail closed）。
    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let mut violations = stmt.query([])?;
    if violations.next()?.is_some() {
        return Err(Error::Invariant("delete: foreign key violation"));
    }
    drop(violations);
    drop(stmt);
    let changed = conn.execute(
        "UPDATE vault_registry
            SET state = 'DELETED', deleted_at = ?1, delete_progress = ?2, delete_error = NULL
          WHERE vault_id = ?3 AND state = 'DELETING'",
        (
            now,
            i64::try_from(PURGE_ORDER.len()).unwrap_or(i64::MAX),
            vault_id.as_slice(),
        ),
    )?;
    if changed != 1 {
        return Err(Error::api(ErrorCode::CasConflict));
    }
    Ok(())
}

/// 该 vault 在 26 张表 + 票据表里的残留行数（启动恢复/测试的零残留复核）。
pub(crate) fn tenant_residue(conn: &Connection, vault_id: &Id16) -> Result<u64> {
    let mut total: u64 = 0;
    for table in PURGE_ORDER {
        let count: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE vault_id = ?1"),
            [vault_id.as_slice()],
            |row| row.get(0),
        )?;
        total += u64::try_from(count).unwrap_or(0);
    }
    let tickets: i64 = conn.query_row(
        "SELECT COUNT(*) FROM create_tickets WHERE vault_id = ?1",
        [vault_id.as_slice()],
        |row| row.get(0),
    )?;
    total += u64::try_from(tickets).unwrap_or(0);
    Ok(total)
}

/// 失败原因 → 短错误码（进 `delete_error`；**不含**租户内容/密文）。
fn failure_kind(err: &Error) -> &'static str {
    match err {
        Error::Api(api) => match api.code {
            ErrorCode::CasConflict => "cas",
            ErrorCode::VaultDeleted => "fenced",
            ErrorCode::NotFound => "missing",
            _ => "api",
        },
        Error::Sqlite(_) => "sqlite",
        Error::WriterClosed => "writer",
        Error::Invariant(_) => "invariant",
        _ => "internal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn purge_order_covers_every_tenant_table_and_vaults_exactly_once() {
        use crate::db::schema;
        let purge: BTreeSet<&str> = PURGE_ORDER.iter().copied().collect();
        assert_eq!(purge.len(), PURGE_ORDER.len(), "PURGE_ORDER 不得有重复表");
        let mut expected: BTreeSet<&str> = schema::TENANT_TABLE_NAMES.iter().copied().collect();
        assert_eq!(expected.len(), 27, "schema 14 应有 27 张租户表");
        expected.insert("vaults");
        assert_eq!(
            purge, expected,
            "purge 清单必须与 schema 租户表清单一致（新增表时同步 FK 顺序）"
        );
        let tables: Vec<&str> = PURGE_TABLES.iter().map(|(table, _)| *table).collect();
        assert_eq!(
            tables, PURGE_ORDER,
            "PURGE_TABLES/PURGE_ORDER 必须同序同集合"
        );
        assert_eq!(
            PURGE_ORDER.last().copied(),
            Some("control_commits"),
            "control_commits 必须最后删（devices/dedup_v2/vaults 都 FK 到它）"
        );
        assert_eq!(
            PURGE_ORDER[PURGE_ORDER.len() - 2],
            "vaults",
            "vaults 行必须在 control_commits 之前删"
        );
    }

    #[test]
    fn limits_are_normalized_to_avoid_empty_loops() {
        let limits = PurgeLimits {
            rows_per_batch: 0,
            chunk_rows: 0,
            bytes_per_batch: 0,
            millis_per_batch: 0,
            wal_budget_bytes: 0,
        }
        .normalized();
        assert_eq!(limits.rows_per_batch, 1);
        assert_eq!(limits.chunk_rows, 1);
        assert_eq!(limits.bytes_per_batch, 0);
        assert_eq!(limits.millis_per_batch, 1);
        assert_eq!(limits.wal_budget_bytes, 0);
    }

    #[test]
    fn admission_fence_is_monotonic_and_reports_closed_state() {
        let fence = AdmissionFence::new();
        let id = [7u8; 16];
        assert!(fence.check(&id).is_ok());
        fence.close(id, ClosedState::Deleting);
        assert_eq!(fence.closed_state(&id), Some(ClosedState::Deleting));
        assert_eq!(
            fence.check(&id).expect_err("closed").code(),
            Some(ErrorCode::VaultDeleted)
        );
        // 重复 close 不改变既有原因，也不会重新打开。
        fence.close(id, ClosedState::Deleted);
        assert_eq!(fence.closed_state(&id), Some(ClosedState::Deleting));
        assert_eq!(fence.len(), 1);
    }

    #[test]
    fn failure_kind_never_leaks_contents() {
        assert_eq!(
            failure_kind(&Error::Sqlite(rusqlite::Error::QueryReturnedNoRows)),
            "sqlite"
        );
        assert_eq!(failure_kind(&Error::Invariant("x")), "invariant");
        assert_eq!(failure_kind(&Error::api(ErrorCode::CasConflict)), "cas");
    }
}
