//! `IndexManager`：索引生命周期与查询正确性（S4-1c）。
//!
//! 本模块在 [`crate::SearchIndex`] 之上负责四件事（ADR 0005 §1/§2/§3/§6）：
//!
//! 1. **代目录 + 发布指针**：索引在 `<vault_dir>/search/generations/<id>/`，
//!    storage meta `search.active_generation` 保存 `id + vault UUID + fingerprint`。
//!    指针缺失/损坏/UUID 或 fingerprint 不匹配时，新建一代→完整构建→重开验证→写指针，
//!    最后才删旧代；
//! 2. **启动对账**：枚举索引里每篇的 `DocVersion`，与 `list_index_manifest` 对比，
//!    补缺、更新版本、删除墓碑/孤儿；
//! 3. **查询终检**：索引候选必须再通过**新开的 ReadSnapshot**确认 `deleted=0` 且存在；
//!    不足时扩大候选数继续取，不能只过滤 top-K；
//! 4. **事件增量**：`StorageEventSubscriber` → dirty 集合 → 200ms debounce → 按 dirty
//!    重读 manifest、upsert/delete、commit；失败保留 dirty 并标记 degraded。
//!
//! 本片不做：app UI、真实性能压测。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use menghunote_storage::{
    list_index_manifest, IndexManifestRow, ReadSnapshot, StorageEvent, StorageEventSubscriber,
    Value, Vault, DEFAULT_EVENT_QUEUE,
};

use crate::error::SearchError;
use crate::fingerprint::Fingerprint;
use crate::index::{DocVersion, Hit, QueryOptions, QueryResult, SearchDoc, SearchIndex};
use crate::markdown_to_plain;

/// 发布指针的 meta key。
pub const ACTIVE_GENERATION_META_KEY: &str = "search.active_generation";
/// 本 vault 的稳定 UUID 的 meta key（首次打开时生成并持久化）。
pub const VAULT_UUID_META_KEY: &str = "search.vault_uuid";
/// 事件增量的 debounce 窗口。
pub const DEBOUNCE: Duration = Duration::from_millis(200);

/// 重建进度阶段。
///
/// search crate 自行只报 [`ProgressPhase::BuildIndex`]；物化正文阶段由 app /
/// storage 的 reconcile 流程负责，search 侧不碰 Loro 物化，避免跨 crate 抢占。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressPhase {
    /// 正文物化（search 不报；预留给 app 的 reconcile 进度）。
    MaterializeBodies,
    /// 全量构建 tantivy 索引。
    BuildIndex,
}

/// 一次重建进度回调。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildProgress {
    /// 当前阶段。
    pub phase: ProgressPhase,
    /// 当前阶段已完成数量。
    pub done: usize,
    /// 当前阶段总数。
    pub total: usize,
}

/// 发布指针内容：三代信息缺一不可。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationPointer {
    /// 代目录名（`search/generations/<id>`）。
    pub id: String,
    /// vault UUID；防止把 A vault 的索引目录用于 B vault。
    pub vault_uuid: String,
    /// 该代使用的索引格式 fingerprint。
    pub fingerprint: Fingerprint,
}

impl GenerationPointer {
    /// 序列化为 meta blob（可读文本，便于故障排查）。
    pub fn encode(&self) -> String {
        format!(
            "id={}\nvault_uuid={}\n{}",
            self.id,
            self.vault_uuid,
            self.fingerprint.encode()
        )
    }

    /// 解析 [`Self::encode`]；损坏返回错误，调用方应按"无有效指针"处理。
    pub fn parse(text: &str) -> Result<Self, SearchError> {
        let mut lines = text.lines();
        let id = lines
            .next()
            .and_then(|line| line.strip_prefix("id="))
            .ok_or_else(|| SearchError::Generation("指针缺少 id".to_owned()))?
            .trim()
            .to_owned();
        let vault_uuid = lines
            .next()
            .and_then(|line| line.strip_prefix("vault_uuid="))
            .ok_or_else(|| SearchError::Generation("指针缺少 vault_uuid".to_owned()))?
            .trim()
            .to_owned();
        if id.is_empty() || vault_uuid.is_empty() {
            return Err(SearchError::Generation(
                "指针 id/vault_uuid 为空".to_owned(),
            ));
        }
        let fingerprint_text = lines.collect::<Vec<_>>().join("\n");
        let fingerprint = Fingerprint::parse(&fingerprint_text)?;
        Ok(Self {
            id,
            vault_uuid,
            fingerprint,
        })
    }
}

/// 对账报告。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    /// 新增/更新篇数。
    pub upserted: usize,
    /// 删除篇数。
    pub deleted: usize,
    /// 版本一致、未动篇数。
    pub unchanged: usize,
}

/// 一次 flush 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushOutcome {
    /// 没有 dirty / 没有待处理标记。
    Idle,
    /// 未到 debounce 窗口（显式 flush 不会返回这个）。
    Deferred {
        /// 预计可 flush 的时间点。
        until: Instant,
    },
    /// 已处理完成。
    Applied {
        /// 检查的 dirty 数（全量对账时为 manifest 行数）。
        examined: usize,
        /// 新增/更新篇数。
        upserted: usize,
        /// 删除篇数。
        deleted: usize,
        /// 是否走了全量对账。
        full_reconcile: bool,
    },
}

/// 索引生命周期管理器。
///
/// 单线程持有：内部含 `SearchIndex`（writer/reader）与 `ReadSnapshot` 使用，
/// 不实现 `Sync`。调用方把 [`Self::poll_events`] / [`Self::flush_if_due`] 放在
/// 自己的后台循环或事件回调里。
pub struct IndexManager {
    vault: Vault,
    index: SearchIndex,
    generation: GenerationPointer,
    generation_dir: PathBuf,
    subscriber: StorageEventSubscriber,
    dirty: BTreeSet<String>,
    dirty_since: Option<Instant>,
    needs_full: bool,
    indexed_versions: BTreeMap<String, DocVersion>,
    degraded: Option<String>,
}

impl IndexManager {
    /// 打开（必要时创建）当前 vault 的检索索引；默认不报进度。
    ///
    /// 启动流程：校验/新建代 → 全量对账 → 清理非活动代。任何一步失败都不会
    /// 覆盖旧指针；旧代只有在指针切换成功后才删除。
    pub fn open(vault: &Vault) -> Result<Self, SearchError> {
        let mut ignore = |_progress: BuildProgress| {};
        Self::open_inner(vault, &mut ignore)
    }

    /// 带重建进度的打开方式。
    ///
    /// 只有**新建代/全量构建索引**时才回调；复用已有代时不会回调。search 只报
    /// [`ProgressPhase::BuildIndex`]（按每 50 篇 + 首尾），物化正文由 app 自己报。
    pub fn open_with_progress(
        vault: &Vault,
        mut sink: impl FnMut(BuildProgress) + Send,
    ) -> Result<Self, SearchError> {
        Self::open_inner(vault, &mut sink)
    }

    fn open_inner(
        vault: &Vault,
        progress: &mut dyn FnMut(BuildProgress),
    ) -> Result<Self, SearchError> {
        let vault = vault.clone();
        let vault_uuid = load_or_create_vault_uuid(&vault)?;
        let generations = generations_root(&vault);
        fs::create_dir_all(&generations)?;

        let subscriber = vault.subscribe_events(DEFAULT_EVENT_QUEUE);
        let active = load_active(&vault, &vault_uuid, &generations)?;
        let (index, generation, generation_dir) = match active {
            Some((index, pointer)) => {
                let dir = generations.join(&pointer.id);
                (index, pointer, dir)
            }
            None => build_generation(&vault, &vault_uuid, &generations, progress)?,
        };

        let mut manager = Self {
            vault,
            index,
            generation,
            generation_dir,
            subscriber,
            dirty: BTreeSet::new(),
            dirty_since: None,
            needs_full: false,
            indexed_versions: BTreeMap::new(),
            degraded: None,
        };
        // 启动必须全量对账；reconcile_required 只是"额外强制"，这里统一走全量。
        manager.full_reconcile()?;
        let _ = manager.vault.take_reconcile_required();

        // 指针已经指向新代、且新代已完成对账，旧代才允许删。
        cleanup_inactive_generations(&generations, &manager.generation.id);
        Ok(manager)
    }

    /// 当前活动代的指针（诊断/测试用）。
    pub fn generation(&self) -> &GenerationPointer {
        &self.generation
    }

    /// 当前活动代目录。
    pub fn generation_dir(&self) -> &Path {
        &self.generation_dir
    }

    /// 当前已索引的版本水位（按 note_id 排序）。
    pub fn indexed_versions(&self) -> &BTreeMap<String, DocVersion> {
        &self.indexed_versions
    }

    /// 单篇笔记当前 searchable 的版本水位；未索引/已删除返回 `None`。
    pub fn searchable_version(&self, note_id: &str) -> Option<&DocVersion> {
        self.indexed_versions.get(note_id)
    }

    /// 最近一次失败是否让管理器处于 degraded 状态。
    pub fn degraded(&self) -> Option<&str> {
        self.degraded.as_deref()
    }

    /// 查询；默认 [`QueryOptions`]（大小写不敏感、整词）。
    pub fn query(&self, q: &str, limit: usize) -> Result<Vec<Hit>, SearchError> {
        Ok(self
            .query_with_options(q, limit, QueryOptions::default())?
            .hits)
    }

    /// 带开关查询；`total` 是索引侧命中数（墓碑终检前），`hits` 已终检。
    pub fn query_with_options(
        &self,
        q: &str,
        limit: usize,
        options: QueryOptions,
    ) -> Result<QueryResult, SearchError> {
        if q.trim().is_empty() {
            return Ok(QueryResult {
                total: 0,
                hits: Vec::new(),
            });
        }
        let snapshot = self.vault.open_read_snapshot()?;
        let docs = self.index.num_docs();
        if docs == 0 {
            return Ok(QueryResult {
                total: 0,
                hits: Vec::new(),
            });
        }
        let docs = usize::try_from(docs).unwrap_or(usize::MAX);
        let mut fetch = limit.max(1).clamp(1, docs);

        loop {
            // 候选先不做 snippet：墓碑终检会剔除一部分，不能为被剔除的候选白算。
            let candidates = self.index.query_candidates(q, fetch, options)?;
            let total = candidates.total;
            let ids: Vec<String> = candidates
                .hits
                .iter()
                .map(|hit| hit.note_id.clone())
                .collect();
            let live = live_ids(&snapshot, &ids)?;
            let mut valid = Vec::new();
            for hit in candidates.hits {
                if live.contains(&hit.note_id) {
                    valid.push(hit);
                    if valid.len() >= limit {
                        valid.truncate(limit);
                        self.index.attach_snippets(q, options, &mut valid)?;
                        return Ok(QueryResult { total, hits: valid });
                    }
                }
            }
            if ids.len() < fetch || fetch >= docs {
                self.index.attach_snippets(q, options, &mut valid)?;
                return Ok(QueryResult { total, hits: valid });
            }
            fetch = fetch.saturating_mul(2).min(docs);
        }
    }

    /// 标记一篇笔记需要增量处理（事件适配/测试用）。
    pub fn mark_dirty(&mut self, note_id: impl Into<String>) {
        self.mark_dirty_at(note_id, Instant::now());
    }

    /// 带显式时间的 [`Self::mark_dirty`]，供可控时钟测试。
    pub fn mark_dirty_at(&mut self, note_id: impl Into<String>, now: Instant) {
        self.dirty.insert(note_id.into());
        self.dirty_since.get_or_insert(now);
    }

    /// 从 storage 订阅队列非阻塞取事件，合并进 dirty。
    ///
    /// `StructureProjected { note_id: None }` 表示整棵树可能变了，直接升级为全量对账；
    /// `take_reconcile_required()` 为 true（事件丢过）也升级为全量。
    pub fn poll_events(&mut self, now: Instant) -> Result<usize, SearchError> {
        let mut count = 0usize;
        while let Some(event) = self.subscriber.try_recv() {
            self.ingest_event(event);
            count += 1;
        }
        if self.vault.take_reconcile_required() {
            self.needs_full = true;
        }
        if count > 0 {
            self.dirty_since.get_or_insert(now);
        }
        Ok(count)
    }

    /// 到 debounce 窗口才处理；未到窗口返回 [`FlushOutcome::Deferred`]。
    ///
    /// 显式调用 [`Self::flush_now`] 可跳过 debounce（调用方主动刷新）。
    pub fn flush_if_due(&mut self, now: Instant) -> Result<FlushOutcome, SearchError> {
        if self.needs_full || self.vault.take_reconcile_required() {
            self.needs_full = true;
            return self.apply_pending();
        }
        if self.dirty.is_empty() {
            return Ok(FlushOutcome::Idle);
        }
        let since = self.dirty_since.unwrap_or(now);
        if now.saturating_duration_since(since) < DEBOUNCE {
            return Ok(FlushOutcome::Deferred {
                until: since + DEBOUNCE,
            });
        }
        self.apply_pending()
    }

    /// 立即处理所有 pending（跳过 debounce）。
    pub fn flush_now(&mut self) -> Result<FlushOutcome, SearchError> {
        if self.vault.take_reconcile_required() {
            self.needs_full = true;
        }
        self.apply_pending()
    }

    /// 显式全量对账；成功后清 dirty、解除 degraded。
    pub fn reconcile_now(&mut self) -> Result<ReconcileReport, SearchError> {
        match self.full_reconcile() {
            Ok(report) => {
                self.dirty.clear();
                self.dirty_since = None;
                self.needs_full = false;
                self.degraded = None;
                Ok(report)
            }
            Err(error) => {
                self.needs_full = true;
                self.degraded = Some(error.to_string());
                Err(error)
            }
        }
    }

    fn ingest_event(&mut self, event: StorageEvent) {
        match event {
            StorageEvent::BodyMaterialized { note_id } => {
                self.dirty.insert(note_id);
            }
            StorageEvent::StructureProjected {
                note_id: Some(note_id),
            } => {
                self.dirty.insert(note_id);
            }
            StorageEvent::StructureProjected { note_id: None } => {
                self.needs_full = true;
            }
        }
    }

    fn apply_pending(&mut self) -> Result<FlushOutcome, SearchError> {
        let full = self.needs_full || self.vault.take_reconcile_required();
        if !full && self.dirty.is_empty() {
            return Ok(FlushOutcome::Idle);
        }

        let result = if full {
            let examined = self.indexed_versions.len();
            self.full_reconcile().map(|report| (report, true, examined))
        } else {
            let examined = self.dirty.len();
            self.flush_dirty().map(|report| (report, false, examined))
        };

        match result {
            Ok((report, full_reconcile, _examined)) => {
                self.dirty.clear();
                self.dirty_since = None;
                self.needs_full = false;
                self.degraded = None;
                Ok(FlushOutcome::Applied {
                    examined: report.upserted + report.deleted + report.unchanged,
                    upserted: report.upserted,
                    deleted: report.deleted,
                    full_reconcile,
                })
            }
            Err(error) => {
                // 失败保留 dirty / full 标记，不推进 indexed_versions 水位。
                self.needs_full = full;
                self.degraded = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// 全量对账：manifest 是期望值，索引里多出来的文档删除。
    fn full_reconcile(&mut self) -> Result<ReconcileReport, SearchError> {
        let manifest = read_manifest(&self.vault)?;
        let mut indexed =
            self.index
                .all_doc_versions()?
                .into_iter()
                .fold(BTreeMap::new(), |mut map, entry| {
                    map.insert(entry.note_id, entry.version);
                    map
                });
        let manifest_ids: BTreeSet<&str> =
            manifest.iter().map(|row| row.note_id.as_str()).collect();

        let mut report = ReconcileReport::default();
        for row in &manifest {
            if row.deleted {
                if indexed.remove(&row.note_id).is_some() {
                    self.index.delete(&row.note_id)?;
                    report.deleted += 1;
                } else {
                    report.unchanged += 1;
                }
                continue;
            }
            let desired = desired_version(row);
            match indexed.get(&row.note_id) {
                Some(current) if current == &desired => report.unchanged += 1,
                _ => {
                    self.index.upsert(search_doc(row))?;
                    indexed.insert(row.note_id.clone(), desired);
                    report.upserted += 1;
                }
            }
        }

        let stale_ids: Vec<String> = indexed
            .keys()
            .filter(|note_id| !manifest_ids.contains(note_id.as_str()))
            .cloned()
            .collect();
        for note_id in stale_ids {
            self.index.delete(&note_id)?;
            report.deleted += 1;
        }

        if report.upserted + report.deleted > 0 {
            self.index.commit()?;
        }

        self.indexed_versions = manifest
            .iter()
            .filter(|row| !row.deleted)
            .map(|row| (row.note_id.clone(), desired_version(row)))
            .collect();
        Ok(report)
    }

    /// dirty 增量：只重读受影响行的已提交版本，批量 upsert/delete 一次 commit。
    fn flush_dirty(&mut self) -> Result<ReconcileReport, SearchError> {
        let manifest = read_manifest(&self.vault)?;
        let by_id: HashMap<&str, &IndexManifestRow> = manifest
            .iter()
            .map(|row| (row.note_id.as_str(), row))
            .collect();

        let dirty: Vec<String> = self.dirty.iter().cloned().collect();
        let mut next_versions = self.indexed_versions.clone();
        let mut report = ReconcileReport::default();

        for note_id in &dirty {
            let row = by_id.get(note_id.as_str()).copied();
            let wants_delete = row.is_none_or(|row| row.deleted);
            if wants_delete {
                if next_versions.remove(note_id).is_some() {
                    self.index.delete(note_id)?;
                    report.deleted += 1;
                } else {
                    report.unchanged += 1;
                }
                continue;
            }

            let Some(row) = row else {
                // wants_delete 为 false 说明 row 必存在；保留分支避免 panic。
                report.unchanged += 1;
                continue;
            };
            let desired = desired_version(row);
            match next_versions.get(note_id) {
                Some(current) if current == &desired => report.unchanged += 1,
                _ => {
                    self.index.upsert(search_doc(row))?;
                    next_versions.insert(note_id.clone(), desired);
                    report.upserted += 1;
                }
            }
        }

        if report.upserted + report.deleted > 0 {
            self.index.commit()?;
        }
        self.indexed_versions = next_versions;
        Ok(report)
    }
}

fn read_manifest(vault: &Vault) -> Result<Vec<IndexManifestRow>, SearchError> {
    let snapshot = vault.open_read_snapshot()?;
    Ok(list_index_manifest(&snapshot)?)
}

fn desired_version(row: &IndexManifestRow) -> DocVersion {
    DocVersion {
        body_doc_id: row.body_doc_id.clone(),
        body_seq: u64::try_from(row.body_seq).unwrap_or(0),
        body_frontier_hash: DocVersion::hash_frontier(row.body_frontier.as_deref()),
        meta_seq: row.meta_seq,
        meta_hash: meta_hash(row),
    }
}

/// storage manifest 目前没有独立 `meta_hash`，由 search 侧用**稳定 FNV-1a**
/// 对可见 meta 内容计算；storage 以后提供正式 meta hash 时替换此函数即可。
fn meta_hash(row: &IndexManifestRow) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in row.title.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    for byte in row.meta_seq.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    for byte in (row.updated_at as u64).to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash ^= u64::from(row.deleted);
    hash.wrapping_mul(PRIME)
}

fn search_doc(row: &IndexManifestRow) -> SearchDoc {
    SearchDoc {
        note_id: row.note_id.clone(),
        title: row.title.clone(),
        body_text: markdown_to_plain(row.body_md.as_deref().unwrap_or("")),
        version: desired_version(row),
    }
}

/// 查询终检：用**本次查询新开的快照**批量确认文档仍存在且未删。
fn live_ids(snapshot: &ReadSnapshot, note_ids: &[String]) -> Result<HashSet<String>, SearchError> {
    if note_ids.is_empty() {
        return Ok(HashSet::new());
    }
    // SQLite 默认最多 999 个参数；分块校验，避免候选数大时直接报错。
    let mut live = HashSet::with_capacity(note_ids.len());
    for chunk in note_ids.chunks(900) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql =
            format!("SELECT note_id FROM notes WHERE deleted = 0 AND note_id IN ({placeholders})");
        let params: Vec<Value> = chunk
            .iter()
            .map(|note_id| Value::Text(note_id.clone()))
            .collect();
        let rows = snapshot.query(&sql, &params)?;
        for row in rows {
            if let Some(Value::Text(note_id)) = row.into_iter().next() {
                live.insert(note_id);
            }
        }
    }
    Ok(live)
}

fn load_or_create_vault_uuid(vault: &Vault) -> Result<String, SearchError> {
    if let Some(bytes) = vault.get_meta(VAULT_UUID_META_KEY)? {
        if let Ok(uuid) = String::from_utf8(bytes) {
            if !uuid.is_empty() {
                return Ok(uuid);
            }
        }
        // 非 UTF-8/空值视为损坏，按"缺失"处理，重新生成。
    }
    let uuid = unique_id("vault");
    vault.set_meta(VAULT_UUID_META_KEY, uuid.as_bytes())?;
    Ok(uuid)
}

fn load_active(
    vault: &Vault,
    vault_uuid: &str,
    generations: &Path,
) -> Result<Option<(SearchIndex, GenerationPointer)>, SearchError> {
    let Some(pointer) = read_pointer(vault)? else {
        return Ok(None);
    };
    if pointer.vault_uuid != vault_uuid || pointer.fingerprint != Fingerprint::current() {
        return Ok(None);
    }
    let dir = generations.join(&pointer.id);
    match SearchIndex::open(&dir) {
        Ok(index) => Ok(Some((index, pointer))),
        Err(_) => Ok(None),
    }
}

fn read_pointer(vault: &Vault) -> Result<Option<GenerationPointer>, SearchError> {
    let Some(bytes) = vault.get_meta(ACTIVE_GENERATION_META_KEY)? else {
        return Ok(None);
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(None);
    };
    Ok(GenerationPointer::parse(&text).ok())
}

fn build_generation(
    vault: &Vault,
    vault_uuid: &str,
    generations: &Path,
    progress: &mut dyn FnMut(BuildProgress),
) -> Result<(SearchIndex, GenerationPointer, PathBuf), SearchError> {
    let id = unique_id("gen");
    let dir = generations.join(&id);
    let result = build_generation_in_dir(vault, vault_uuid, &dir, &id, progress);
    if result.is_err() {
        // 指针还没写：失败的新代是孤儿，清掉；旧代保持原样。
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

fn build_generation_in_dir(
    vault: &Vault,
    vault_uuid: &str,
    dir: &Path,
    id: &str,
    progress: &mut dyn FnMut(BuildProgress),
) -> Result<(SearchIndex, GenerationPointer, PathBuf), SearchError> {
    fs::create_dir_all(dir)?;
    let mut index = SearchIndex::create(dir, &Fingerprint::current())?;
    let manifest = read_manifest(vault)?;
    let total = manifest.iter().filter(|row| !row.deleted).count();
    progress(BuildProgress {
        phase: ProgressPhase::BuildIndex,
        done: 0,
        total,
    });
    let mut done = 0usize;
    for row in &manifest {
        if row.deleted {
            continue;
        }
        index.upsert(search_doc(row))?;
        done += 1;
        if done.is_multiple_of(50) || done == total {
            progress(BuildProgress {
                phase: ProgressPhase::BuildIndex,
                done,
                total,
            });
        }
    }
    index.commit()?;

    // 必须重新 open 验证，而不是继续用刚写完的 writer 实例。
    drop(index);
    let index = SearchIndex::open(dir)?;
    if let Some(row) = manifest.iter().find(|row| !row.deleted) {
        let sample = if row.title.is_empty() {
            markdown_to_plain(row.body_md.as_deref().unwrap_or(""))
        } else {
            row.title.clone()
        };
        if !sample.trim().is_empty() {
            let _ = index.query(&sample, 1)?;
        }
    }

    let pointer = GenerationPointer {
        id: id.to_owned(),
        vault_uuid: vault_uuid.to_owned(),
        fingerprint: Fingerprint::current(),
    };
    // 指针是唯一发布点：写成功之前旧指针仍然有效。
    vault.set_meta(ACTIVE_GENERATION_META_KEY, pointer.encode().as_bytes())?;
    Ok((index, pointer, dir.to_path_buf()))
}

fn generations_root(vault: &Vault) -> PathBuf {
    let parent = vault
        .path()
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    parent.join("search").join("generations")
}

fn cleanup_inactive_generations(generations: &Path, active_id: &str) {
    let Ok(entries) = fs::read_dir(generations) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy() == active_id {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            // 删不掉（Windows 文件仍被映射等）不影响当前代；下次启动再试。
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn unique_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{counter:04x}")
}
