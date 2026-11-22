//! ADR 0019 §4 / S3：资产 GC 的**只读 dry-run 扫描**。
//!
//! 本模块只回答"哪些 `assets` 行看起来没有任何保护根"，**绝不删除字节、不改
//! `orphan_since`、不改 schema**。它的输出用于将来版本化 lifecycle 协议前的
//! dry-run 报告，不能直接当成删除许可。
//!
//! ## 保护根（任一命中即不是候选）
//!
//! 1. `asset_refs` 的**任意**行（当前/历史 `body_seq` 都算；引用是账本，正文才是
//!    真源）；
//! 2. `versions.asset_hashes`：历史版本恢复时仍可能用到资产；
//! 3. 同步 outbox：
//!    - 当前 schema 的 `sync_outbox` 只有**文档 update 意图**（`FOREIGN KEY(doc_id,seq)`
//!      指向 `loro_updates`），没有单独的 ASSET 意图行；
//!    - 因此这里做两件事：把 32B `object_id` 当作不透明对象名与资产 hash 做**严格
//!      相等**匹配（保守，不解析 HMAC）；并把 outbox 指向的 `loro_updates.payload`
//!      当作**出站明文字节**扫描 `asset://<64 小写 hex>`（正是同步仍可能传的正文
//!      引用）。缺少 payload、`object_id` 为空/长度异常、`state` 无法识别都算
//!      **未知**，按 fail-closed 处理；
//! 4. `assets.data IS NULL`：远端占位/未下载，永远不是候选；
//! 5. `baseline_apply_assets`：已验证基线的分批应用 journal 记录
//!    `object_id -> asset_hash`，属于 checkpoint/bootstrap lease 的本地事实；
//!    只要行还在就保护对应 hash。
//!
//! ## fail-closed
//!
//! 任何**保护根**无法可靠解析时（`versions.asset_hashes` 不是 32B 的整数倍、
//! outbox 行缺 payload / 字段类型异常等），整次扫描进入 fail-closed：候选列表为空，
//! 原本可能成为候选的资产计入 [`AssetGcReport::unknown_protected`]，同时计入
//! [`AssetGcReport::skipped_unknown`]。单个资产行自身的元数据坏掉只跳过该行并计入
//! `skipped_unknown`，不影响其它行。
//!
//! ## 只读与一致性
//!
//! 扫描使用 [`crate::ReadSnapshot`] 的只读连接，并显式开一个 `BEGIN DEFERRED` 读事务，
//! 保证 `assets` / `asset_refs` / `versions` / `sync_outbox` / `loro_updates` 来自同一个
//! 已提交快照；所有 SQL 都是 SELECT。重复调用不会修改数据库。
//!
//! ## 计数口径
//!
//! `protected_by_refs` / `protected_by_versions` / `protected_by_sync` /
//! `protected_by_baseline` / `protected_by_no_bytes` 是**按根独立统计**的，可能重叠
//! （例如一个资产同时被正文和版本引用）。`protected_total` 是"命中任一已知保护根"的
//! **去重**资产数；`candidate_count` 是"有字节且无任何已知/未知根"的资产数。
//! `skipped_unknown` 是"因未知原因没有列为候选"的资产数；其中由整次 fail-closed
//! 抑制的部分是 `unknown_protected` 的子集。因此恒有：
//!
//! `total_assets == protected_total + candidate_count + skipped_unknown`
//!
//! （fail-closed 时 `candidate_count` 为 0，`unknown_protected` 可能非零。）

use std::collections::HashSet;

use rusqlite::types::Value;

use crate::error::StorageError;
use crate::read_snapshot::ReadSnapshot;
use crate::Vault;

/// 单次报告最多列出的候选条数硬上限（防止调用方传入无界 `limit` 造成内存放大）。
pub const ASSET_GC_MAX_CANDIDATES: usize = 4_096;

/// 一条**候选**资产（有本地字节、没有任何保护根命中）。
///
/// 只是 dry-run 报告项，不代表允许删除；lifecycle 协议还需要因果水位、设备 ACK 等
/// 额外证明（ADR 0019 §4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetGcCandidate {
    /// 内容寻址的 SHA-256。
    pub hash: [u8; 32],
    /// 本地明文长度（有字节的资产必然有值）。
    pub byte_len: u64,
    /// 资产 MIME；未知时为 `None`。
    pub media_type: Option<String>,
}

/// 一次只读资产 GC 扫描的结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AssetGcReport {
    /// `assets` 总行数（包括无字节占位与解析失败的行）。
    pub total_assets: u64,
    /// `assets.data IS NOT NULL` 且行可解析的数量。
    pub assets_with_data: u64,
    /// `assets.data IS NULL` 且行可解析的数量。
    pub assets_without_data: u64,
    /// 命中 `asset_refs`（当前或历史 `body_seq`）的资产数。
    pub protected_by_refs: u64,
    /// 命中 `versions.asset_hashes` 的资产数。
    pub protected_by_versions: u64,
    /// 被同步 outbox（对象名精确匹配或 payload 中的 `asset://` 引用）保护的资产数。
    pub protected_by_sync: u64,
    /// 被 `baseline_apply_assets`（验证基线 journal）保护的资产数。
    pub protected_by_baseline: u64,
    /// `data IS NULL`（远端占位/未下载）的资产数。
    pub protected_by_no_bytes: u64,
    /// 命中**任一**已知保护根的资产数（去重）。
    pub protected_total: u64,
    /// 因未知保护根无法解析而**被跳过**的、原本可能是候选的资产数（fail-closed）。
    ///
    /// 是 [`Self::skipped_unknown`] 的子集：`skipped_unknown` 还包括单行元数据损坏。
    pub unknown_protected: u64,
    /// 因未知原因（保护根 fail-closed 或自身行损坏）没有列为候选的资产数。
    pub skipped_unknown: u64,
    /// 无保护根且有本地字节的候选总数（不受 `limit` 截断影响）。
    pub candidate_count: u64,
    /// 候选资产的总字节数（不受 `limit` 截断影响）。
    pub candidate_bytes: u64,
    /// 候选列表，按 `hash` 升序；最多 `limit` 条。
    pub candidates: Vec<AssetGcCandidate>,
    /// 因 `limit` 没有进入 `candidates` 的候选条数。
    pub candidates_truncated: u64,
    /// 本次实际生效的候选条数上限（不超过 [`ASSET_GC_MAX_CANDIDATES`]）。
    pub limit: usize,
    /// 调用方传入的 `limit` 是否被硬上限截断。
    pub limit_capped: bool,

    /// `versions` 表行数。
    pub version_rows: u64,
    /// `versions.asset_hashes` 解析出的**去重** hash 数。
    pub version_hashes: u64,
    /// `versions.asset_hashes` 无法解析的行数。
    pub version_parse_errors: u64,
    /// `sync_outbox` 表行数。
    pub sync_outbox_rows: u64,
    /// `sync_outbox` 行/关联 payload 无法解析的数量。
    pub sync_parse_errors: u64,
    /// `baseline_apply_assets.asset_hash` 无法解析的行数。
    pub baseline_parse_errors: u64,
    /// `assets` 行自身无法解析的数量。
    pub asset_row_parse_errors: u64,
    /// 上述所有解析错误总数。
    pub parse_errors: u64,

    /// 是否有未知保护根导致本次候选被整体跳过（fail-closed）。
    pub fail_closed: bool,
}

impl AssetGcReport {
    /// 是否没有任何候选（正常完成或 fail-closed 都可能为 true；看 `fail_closed` 区分）。
    #[must_use]
    pub fn has_no_candidates(&self) -> bool {
        self.candidate_count == 0 && self.candidates.is_empty()
    }
}

/// 扫描 `assets`，返回只读的 GC 候选报告。
///
/// - `limit` 是候选**列表**条数上限（总条数/总字节仍会完整统计）；超过
///   [`ASSET_GC_MAX_CANDIDATES`] 时会被截断，并在 [`AssetGcReport::limit_capped`] 标记。
/// - 只读：使用独立只读连接 + `BEGIN DEFERRED` 读事务；不写任何表，不改
///   `orphan_since`。
/// - 任何保护根格式不确定时 fail-closed：`fail_closed == true`、
///   `candidates` 为空、`unknown_protected` 记录被抑制的候选数。
///
/// # Errors
///
/// SQLite/快照打开失败等无法形成报告的情况返回 [`StorageError`]；保护根内容的
/// 格式问题不返回错误，而是写进报告并 fail-closed。
pub fn scan_asset_gc(vault: &Vault, limit: usize) -> Result<AssetGcReport, StorageError> {
    let snapshot = vault.open_read_snapshot()?;
    begin_read_transaction(&snapshot)?;
    let result = scan_in_snapshot(&snapshot, limit);
    match result {
        Ok(report) => {
            commit_read_transaction(&snapshot)?;
            Ok(report)
        }
        Err(error) => {
            rollback_read_transaction(&snapshot);
            Err(error)
        }
    }
}

/// 扫描过程中从某个保护根读出的 hash 集合与解析错误数。
#[derive(Debug, Default)]
struct SourceScan {
    rows: u64,
    hashes: HashSet<[u8; 32]>,
    parse_errors: u64,
}

/// 从 `assets` 行解析出的只读视图。
#[derive(Debug)]
struct ScannedAsset {
    hash: [u8; 32],
    byte_len: Option<u64>,
    media_type: Option<String>,
    has_data: bool,
    referenced: bool,
}

fn scan_in_snapshot(
    snapshot: &ReadSnapshot,
    requested_limit: usize,
) -> Result<AssetGcReport, StorageError> {
    let effective_limit = requested_limit.min(ASSET_GC_MAX_CANDIDATES);
    let mut report = AssetGcReport {
        limit: effective_limit,
        limit_capped: requested_limit > effective_limit,
        ..AssetGcReport::default()
    };

    let versions = scan_versions(snapshot)?;
    report.version_rows = versions.rows;
    report.version_hashes = count_u64(versions.hashes.len());
    report.version_parse_errors = versions.parse_errors;

    let sync = scan_sync_outbox(snapshot)?;
    report.sync_outbox_rows = sync.rows;
    report.sync_parse_errors = sync.parse_errors;

    let baseline = scan_baseline_assets(snapshot)?;
    report.baseline_parse_errors = baseline.parse_errors;

    // 保护根解析失败 => 不知道谁被保护，整次扫描不能给出候选（fail-closed）。
    report.fail_closed = report.version_parse_errors > 0
        || report.sync_parse_errors > 0
        || report.baseline_parse_errors > 0;

    scan_assets(
        snapshot,
        &versions.hashes,
        &sync.hashes,
        &baseline.hashes,
        &mut report,
    )?;

    report.parse_errors = report
        .version_parse_errors
        .saturating_add(report.sync_parse_errors)
        .saturating_add(report.baseline_parse_errors)
        .saturating_add(report.asset_row_parse_errors);
    report.candidates_truncated = report
        .candidate_count
        .saturating_sub(count_u64(report.candidates.len()));
    Ok(report)
}

/// `versions.asset_hashes` 的解析口径。
///
/// 当前仓库**没有** versions 的写入方（该列只有 schema），因此这里采用唯一不需要
/// 额外依赖、且能由固定长度 hash 无歧义还原的保守布局：若干 32B SHA-256 **直接拼接**，
/// 空 BLOB 表示该历史版本没有资产引用。长度不是 32 的整数倍时返回 `None`，调用方
/// 必须 fail-closed（不能猜 JSON/长度前缀等其它布局）。
fn decode_version_asset_hashes(bytes: &[u8]) -> Option<Vec<[u8; 32]>> {
    if !bytes.len().is_multiple_of(32) {
        return None;
    }
    let (chunks, _) = bytes.as_chunks::<32>();
    let mut hashes = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        hashes.push(*chunk);
    }
    Some(hashes)
}

fn scan_versions(snapshot: &ReadSnapshot) -> Result<SourceScan, StorageError> {
    let rows = snapshot.query(
        "SELECT version_id, asset_hashes FROM versions ORDER BY version_id",
        &[],
    )?;
    let mut scan = SourceScan {
        rows: count_u64(rows.len()),
        ..SourceScan::default()
    };
    for row in &rows {
        match row.get(1) {
            Some(Value::Blob(bytes)) => match decode_version_asset_hashes(bytes) {
                Some(hashes) => scan.hashes.extend(hashes),
                None => scan.parse_errors = scan.parse_errors.saturating_add(1),
            },
            // `asset_hashes` 是 NOT NULL BLOB；其它类型只可能来自外部损坏。
            _ => scan.parse_errors = scan.parse_errors.saturating_add(1),
        }
    }
    Ok(scan)
}

/// 同步 outbox 的保护来源。
///
/// 当前 `sync_outbox` 的每一行都是文档 update 意图（schema 有 `(doc_id,seq)` 外键），
/// 不存在 ASSET 专用列；因此这里**不发明 ASSET 二进制格式**：
///
/// - `object_id` 是 32B 对象名；与资产 hash 相等时保守保护（ASSET 对象名在本仓库实际是
///   HMAC，不能从 storage 侧反推，因此不做猜测）；
/// - 关联的 `loro_updates.payload` 是出站明文（Loro update 字节），按严格
///   `asset://<64 小写 hex>` 扫描；命中的 hash 属于"同步可能仍要传的正文引用"；
/// - `object_id` 缺失/长度异常、payload 缺失/类型异常、state 无法识别 →
///   `parse_errors`，整次扫描 fail-closed。
fn scan_sync_outbox(snapshot: &ReadSnapshot) -> Result<SourceScan, StorageError> {
    let rows = snapshot.query(
        "SELECT o.object_id, o.state, o.seq, u.payload \
         FROM sync_outbox o \
         LEFT JOIN loro_updates u ON u.doc_id=o.doc_id AND u.seq=o.seq \
         ORDER BY o.message_id",
        &[],
    )?;
    let mut scan = SourceScan {
        rows: count_u64(rows.len()),
        ..SourceScan::default()
    };
    for row in &rows {
        let state = match row.get(1) {
            Some(Value::Text(state)) => state.as_str(),
            _ => {
                scan.parse_errors = scan.parse_errors.saturating_add(1);
                continue;
            }
        };
        // 已 ACK 的行是同步终点，不再需要发送；坏行也不应阻塞其它行。
        if state == "acked" {
            continue;
        }
        if !matches!(state, "pending" | "encrypted" | "sent") {
            scan.parse_errors = scan.parse_errors.saturating_add(1);
            continue;
        }
        // 文档 update 的 seq 一定从 1 开始；seq=0 只可能是外部损坏或未来的
        // ASSET 意图（ASSET 的 generation/sender_seq 为 0）。无法确认时 fail-closed。
        match row.get(2) {
            Some(Value::Integer(seq)) if *seq > 0 => {}
            _ => {
                scan.parse_errors = scan.parse_errors.saturating_add(1);
                continue;
            }
        }

        match row.first() {
            Some(Value::Blob(object_id)) if object_id.len() == 32 => {
                match <[u8; 32]>::try_from(object_id.as_slice()) {
                    Ok(hash) => {
                        scan.hashes.insert(hash);
                    }
                    Err(_) => scan.parse_errors = scan.parse_errors.saturating_add(1),
                }
            }
            // 旧 schema 可留 NULL；当前写入路径都会写 object_id。NULL 无法判断它是不是
            // 一条未来/异常的 ASSET 意图，按未知处理（fail-closed）。
            Some(Value::Null) | None => {
                scan.parse_errors = scan.parse_errors.saturating_add(1);
            }
            Some(_) => scan.parse_errors = scan.parse_errors.saturating_add(1),
        }

        match row.get(3) {
            Some(Value::Blob(payload)) => {
                for hash in asset_hashes_in_bytes(payload) {
                    scan.hashes.insert(hash);
                }
            }
            // pending/encrypted/sent 的 outbox 行按外键必须有对应 update；缺了就无法
            // 判断它引用了哪些资产，不能猜。
            _ => scan.parse_errors = scan.parse_errors.saturating_add(1),
        }
    }
    Ok(scan)
}

/// 在任意字节里扫描严格 `asset://<64 小写 hex>`，返回命中的 hash。
///
/// 这是**字节级**扫描（不解析 Markdown/Loro），只用于保护出站 update 里可能出现的
/// 资产引用；误命中只会多保护、不会少保护，符合 fail-closed 方向。
fn asset_hashes_in_bytes(bytes: &[u8]) -> Vec<[u8; 32]> {
    const PREFIX: &[u8] = b"asset://";
    let mut hashes = Vec::new();
    let mut index = 0;
    while index + PREFIX.len() + 64 <= bytes.len() {
        if bytes[index..].starts_with(PREFIX) {
            let hex_start = index + PREFIX.len();
            let hex_end = hex_start + 64;
            if let Some(hash) = decode_lower_hex_32(&bytes[hex_start..hex_end]) {
                // 与 app 的引用扫描同口径：后一个字节仍是小写 hex 时说明这是更长的
                // 十六进制串，不算一次完整引用。
                let next_is_hex = bytes
                    .get(hex_end)
                    .is_some_and(|byte| is_lower_hex_byte(*byte));
                if !next_is_hex {
                    hashes.push(hash);
                }
            }
            index = hex_start;
            continue;
        }
        index += 1;
    }
    hashes
}

/// 解码恰好 64 个小写十六进制字符为 32B hash。
fn decode_lower_hex_32(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.len() != 64 {
        return None;
    }
    let mut hash = [0u8; 32];
    for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        hash[index] = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
    }
    Some(hash)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn is_lower_hex_byte(byte: u8) -> bool {
    matches!(byte, b'0'..=b'9' | b'a'..=b'f')
}

fn scan_baseline_assets(snapshot: &ReadSnapshot) -> Result<SourceScan, StorageError> {
    let rows = snapshot.query("SELECT asset_hash FROM baseline_apply_assets", &[])?;
    let mut scan = SourceScan {
        rows: count_u64(rows.len()),
        ..SourceScan::default()
    };
    for row in &rows {
        match row.first() {
            Some(Value::Blob(bytes)) if bytes.len() == 32 => {
                match <[u8; 32]>::try_from(bytes.as_slice()) {
                    Ok(hash) => {
                        scan.hashes.insert(hash);
                    }
                    Err(_) => scan.parse_errors = scan.parse_errors.saturating_add(1),
                }
            }
            _ => scan.parse_errors = scan.parse_errors.saturating_add(1),
        }
    }
    Ok(scan)
}

fn scan_assets(
    snapshot: &ReadSnapshot,
    version_hashes: &HashSet<[u8; 32]>,
    sync_hashes: &HashSet<[u8; 32]>,
    baseline_hashes: &HashSet<[u8; 32]>,
    report: &mut AssetGcReport,
) -> Result<(), StorageError> {
    let rows = snapshot.query(
        "SELECT a.asset_hash, a.byte_len, a.media_type, a.data IS NOT NULL, \
                EXISTS(SELECT 1 FROM asset_refs r WHERE r.asset_hash=a.asset_hash) \
         FROM assets a ORDER BY a.asset_hash ASC",
        &[],
    )?;
    report.total_assets = count_u64(rows.len());
    for row in &rows {
        let Some(asset) = parse_asset_row(row) else {
            report.asset_row_parse_errors = report.asset_row_parse_errors.saturating_add(1);
            report.skipped_unknown = report.skipped_unknown.saturating_add(1);
            continue;
        };
        record_asset(asset, version_hashes, sync_hashes, baseline_hashes, report);
    }
    Ok(())
}

fn parse_asset_row(row: &crate::Row) -> Option<ScannedAsset> {
    let hash: [u8; 32] = match row.first() {
        Some(Value::Blob(bytes)) if bytes.len() == 32 => bytes.as_slice().try_into().ok()?,
        _ => return None,
    };
    let byte_len = match row.get(1) {
        Some(Value::Null) | None => None,
        Some(Value::Integer(value)) if *value >= 0 => u64::try_from(*value).ok(),
        _ => return None,
    };
    let media_type = match row.get(2) {
        Some(Value::Null) | None => None,
        Some(Value::Text(value)) => Some(value.clone()),
        _ => return None,
    };
    let has_data = match row.get(3) {
        Some(Value::Integer(0)) => false,
        Some(Value::Integer(1)) => true,
        _ => return None,
    };
    let referenced = match row.get(4) {
        Some(Value::Integer(0)) => false,
        Some(Value::Integer(1)) => true,
        _ => return None,
    };
    // `assets` 的 CHECK 保证有字节就一定有 byte_len；外部损坏时跳过该行（fail-closed）。
    if has_data && byte_len.is_none() {
        return None;
    }
    Some(ScannedAsset {
        hash,
        byte_len,
        media_type,
        has_data,
        referenced,
    })
}

fn record_asset(
    asset: ScannedAsset,
    version_hashes: &HashSet<[u8; 32]>,
    sync_hashes: &HashSet<[u8; 32]>,
    baseline_hashes: &HashSet<[u8; 32]>,
    report: &mut AssetGcReport,
) {
    if asset.has_data {
        report.assets_with_data = report.assets_with_data.saturating_add(1);
    } else {
        report.assets_without_data = report.assets_without_data.saturating_add(1);
    }

    let mut protected = false;
    if asset.referenced {
        report.protected_by_refs = report.protected_by_refs.saturating_add(1);
        protected = true;
    }
    if version_hashes.contains(&asset.hash) {
        report.protected_by_versions = report.protected_by_versions.saturating_add(1);
        protected = true;
    }
    if sync_hashes.contains(&asset.hash) {
        report.protected_by_sync = report.protected_by_sync.saturating_add(1);
        protected = true;
    }
    if baseline_hashes.contains(&asset.hash) {
        report.protected_by_baseline = report.protected_by_baseline.saturating_add(1);
        protected = true;
    }
    if !asset.has_data {
        report.protected_by_no_bytes = report.protected_by_no_bytes.saturating_add(1);
        protected = true;
    }

    if protected {
        report.protected_total = report.protected_total.saturating_add(1);
        return;
    }
    if report.fail_closed {
        // 有未知保护根：无法证明它不被保护，不能列为候选。
        report.unknown_protected = report.unknown_protected.saturating_add(1);
        report.skipped_unknown = report.skipped_unknown.saturating_add(1);
        return;
    }

    // `has_data` 为 true 时 `parse_asset_row` 已保证 `byte_len` 存在；这里再做一次
    // 防御，避免未来改 CHECK 后静默把长度未知的资产当候选。
    let Some(byte_len) = asset.byte_len else {
        report.asset_row_parse_errors = report.asset_row_parse_errors.saturating_add(1);
        report.skipped_unknown = report.skipped_unknown.saturating_add(1);
        return;
    };

    report.candidate_count = report.candidate_count.saturating_add(1);
    report.candidate_bytes = report.candidate_bytes.saturating_add(byte_len);
    if report.candidates.len() < report.limit {
        report.candidates.push(AssetGcCandidate {
            hash: asset.hash,
            byte_len,
            media_type: asset.media_type,
        });
    }
}

fn count_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn begin_read_transaction(snapshot: &ReadSnapshot) -> Result<(), StorageError> {
    let mut statement = snapshot.prepare("BEGIN DEFERRED")?;
    statement.execute([])?;
    Ok(())
}

fn commit_read_transaction(snapshot: &ReadSnapshot) -> Result<(), StorageError> {
    let mut statement = snapshot.prepare("COMMIT")?;
    statement.execute([])?;
    Ok(())
}

fn rollback_read_transaction(snapshot: &ReadSnapshot) {
    if let Ok(mut statement) = snapshot.prepare("ROLLBACK") {
        let _ = statement.execute([]);
    }
}
