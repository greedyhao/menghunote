//! Wiki 链接重命名联动改写（L2 剩余功能）。
//!
//! 笔记标题从 `旧名` 改成 `新名` 后，所有**当前确实解析到该笔记**的 wiki 链接
//! 都要把 target 里的标题部分一起改掉（见 `docs/notes/backlog-wiki-links-graph.md`
//! 的「重命名联动改写（已决）」）：
//!
//! - `[[旧名]]` → `[[新名]]`
//! - `[[旧名|别名]]` → `[[新名|别名]]`
//! - `[[旧名#标题]]` → `[[新名#标题]]`
//! - `[[文件夹/旧名]]` → `[[文件夹/新名]]`（只改 basename，保留路径前缀）
//!
//! # 分工
//!
//! 1. [`link_sources_for_note`]：从派生表 `links` 里取“来源笔记 + 解析到目标笔记的
//!    target 原文”。`to_note_id` 已经表达了解析结果，因此未解析/同名歧义（NULL）
//!    天然不会进入候选；
//! 2. [`plan_wiki_link_renames`]：**纯函数**。对一篇正文重新解析，只在 target 子区间
//!    产出替换；`[[`/`]]`/alias/heading/代码/转义都不在替换范围内；
//! 3. [`rewrite_link_sources`]：逐篇打开正常 [`SessionDriver`] 会话，构造一个
//!    `EditRequest` → `apply` → `close`（有界排空 durable + B 物化）。
//!
//! 每篇正文只提交一次；跨笔记**不做**原子事务，失败不回滚其它篇（报告失败数量）。
//! `links` 不手工写：正文 durable 之后由 B 事务按新正文重建。
//!
//! # 活动笔记
//!
//! 当前打开的笔记不能在这里另开一个会话改写（会和活动 driver 的在途编辑一起
//! 覆盖正文）。调用方必须把它从 `sources` 里排除，走 `EditorView` 的活动 driver
//! 路径；本模块只负责非活动来源。

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use menghunote_text::UnicodeScalar;
use rusqlite::types::Value;

use crate::error::StorageError;
use crate::session::{DriverOptions, EditOutcome, EditRequest, ScalarEdit, SessionDriver};
use crate::sync_bridge::SyncContext;
use crate::Vault;

/// 默认排空上限：与切换/关闭文档的 1500ms 保持同一口径。
pub const DEFAULT_REWRITE_TIMEOUT: Duration = Duration::from_millis(1500);

/// 一条 wiki 链接的目标改写（source UTF-8 字节区间）。
///
/// `start..end` **只覆盖 target 文本本身**（不含 `[[`/`]]`/`#heading`/`|alias`）；
/// 区间两端即使有空白也原样保留（只替换 trim 后的 target 内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLinkRename {
    /// target 内容的起始字节（含）。
    pub start: usize,
    /// target 内容的结束字节（不含）。
    pub end: usize,
    /// 被替换的 target 原文（trim 后）。
    pub old_target: String,
    /// 替换后的 target 原文。
    pub new_target: String,
}

/// 一篇需要联动改写的来源笔记（来自 `links` 派生表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameLinkSource {
    /// 来源笔记 ID。
    pub note_id: String,
    /// 来源笔记标题（结构树当前值，失败报告用）。
    pub title: String,
    /// 来源正文文档 ID。
    pub doc_id: String,
    /// `links` 里解析到目标笔记的 target 原文集合（决定“哪些链接该改”）。
    pub targets: BTreeSet<String>,
}

/// 逐篇改写配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenameLinkOptions {
    /// 本机 Loro peer id。
    pub peer_id: u64,
    /// 同步上下文（`Some` 时 flush 在同一 A 事务里写 `sync_outbox`）。
    pub sync: Option<SyncContext>,
    /// 单篇排空上限（durable + B 物化）。
    pub timeout: Duration,
}

impl Default for RenameLinkOptions {
    fn default() -> Self {
        Self {
            peer_id: 1,
            sync: None,
            timeout: DEFAULT_REWRITE_TIMEOUT,
        }
    }
}

/// 一篇来源笔记的失败记录（其它篇继续，不回滚）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameLinkFailure {
    /// 来源笔记 ID。
    pub note_id: String,
    /// 来源笔记标题。
    pub title: String,
    /// 失败信息。
    pub message: String,
}

/// 逐篇改写汇总。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenameLinkReport {
    /// 参与处理的来源篇数。
    pub sources: usize,
    /// 真正改写了正文的篇数。
    pub notes_updated: usize,
    /// 改写的链接条数。
    pub links_rewritten: usize,
    /// 计划为空（例如 links 已过期）的篇数。
    pub notes_unchanged: usize,
    /// 失败的篇数。
    pub notes_failed: usize,
    /// 失败明细。
    pub failures: Vec<RenameLinkFailure>,
}

/// 纯函数：为一篇正文规划 wiki 链接 target 的替换。
///
/// `linked_targets` 是 `links` 表里**解析到被重命名笔记**的 target 原文集合：
/// 只有 trim 后与之逐字相等的链接才会被改写。这样未解析/歧义链接不会被误改，
/// 同一篇里指向别处但 basename 相同的链接也不会被连带。
///
/// 返回按 `start` 升序、互不重叠的替换列表；没有要改的返回空。
#[must_use]
pub fn plan_wiki_link_renames(
    source: &str,
    old_title: &str,
    new_title: &str,
    linked_targets: &BTreeSet<String>,
) -> Vec<WikiLinkRename> {
    if old_title.is_empty() || new_title.is_empty() || old_title == new_title {
        return Vec::new();
    }
    let parsed = menghunote_markdown::parse(source);
    let mut renames = Vec::new();
    for link in &parsed.wiki_links {
        let Some(raw) = source.get(link.target.start..link.target.end) else {
            continue;
        };
        let target = raw.trim();
        if target.is_empty() || !linked_targets.contains(target) {
            continue;
        }
        let Some(new_target) = rename_target(old_title, new_title, target) else {
            continue;
        };
        // 只替换 target 里 trim 后的内容：前导空白按字节原样保留。
        let leading = raw.len() - raw.trim_start().len();
        let start = link.target.start.saturating_add(leading);
        let end = start.saturating_add(target.len());
        if source.get(start..end) != Some(target) {
            continue;
        }
        renames.push(WikiLinkRename {
            start,
            end,
            old_target: target.to_owned(),
            new_target,
        });
    }
    renames.sort_by_key(|rename| (rename.start, rename.end));
    renames.dedup_by(|left, right| left.start == right.start && left.end == right.end);
    renames
}

/// 只按 title 的 basename 改写，路径前缀 / 末尾 `/` / `.md` 后缀逐字保留。
fn rename_target(old_title: &str, new_title: &str, target: &str) -> Option<String> {
    // 解析器把路径首尾 `/` 当分隔符（`normalize_target` 会 trim 掉），这里全部保留，
    // 只替换最后一个非空路径段的标题部分。
    let trimmed = target.trim_end_matches('/');
    let trailing = &target[trimmed.len()..];
    if trimmed.is_empty() {
        return None;
    }
    let (prefix, basename) = match trimmed.rsplit_once('/') {
        Some((prefix, basename)) => (format!("{prefix}/"), basename),
        None => (String::new(), trimmed),
    };
    if basename.is_empty() {
        return None;
    }
    let replaced = if same_title(basename, old_title) {
        Some(new_title.to_owned())
    } else if let Some(stem) = basename.strip_suffix(".md") {
        if !stem.is_empty() && same_title(stem, old_title) {
            Some(format!("{new_title}.md"))
        } else {
            None
        }
    } else {
        None
    };
    replaced.map(|name| format!("{prefix}{name}{trailing}"))
}

/// 与 `links.rs` 的解析规则一致：标题比较大小写不敏感。
fn same_title(left: &str, right: &str) -> bool {
    left.to_lowercase() == right.to_lowercase()
}

/// 把字节替换清单换算成会话协议使用的 Unicode scalar 编辑。
///
/// 校验通过才返回：区间必须有序、不重叠、落在 char 边界，且 source 切片仍等于
/// `old_target`（防止计划与正文不一致时按过期偏移误改）。
pub fn scalar_edits_for_renames(
    source: &str,
    renames: &[WikiLinkRename],
) -> Result<Vec<ScalarEdit>, String> {
    let mut edits = Vec::with_capacity(renames.len());
    let mut previous_end = 0usize;
    for rename in renames {
        if rename.start >= rename.end || rename.end > source.len() {
            return Err(format!(
                "非法改写区间 {}..{}（正文长度 {}）",
                rename.start,
                rename.end,
                source.len()
            ));
        }
        if rename.start < previous_end {
            return Err("改写区间重叠或未按起点排序".to_owned());
        }
        if !source.is_char_boundary(rename.start) || !source.is_char_boundary(rename.end) {
            return Err("改写区间不在 UTF-8 字符边界".to_owned());
        }
        if source.get(rename.start..rename.end) != Some(rename.old_target.as_str()) {
            return Err("正文与链接计划不一致（target 已变化）".to_owned());
        }
        let start = source[..rename.start].chars().count();
        let end = source[..rename.end].chars().count();
        edits.push(ScalarEdit {
            start: UnicodeScalar::new(start),
            end: UnicodeScalar::new(end),
            replacement: rename.new_target.clone(),
        });
        previous_end = rename.end;
    }
    Ok(edits)
}

/// 查“哪些来源笔记里有哪些已解析到 `to_note_id` 的链接”。
///
/// 只读派生表 `links`（`kind='wiki'` + `to_note_id` 非空），不扫正文。
pub fn link_sources_for_note(
    vault: &Vault,
    to_note_id: &str,
) -> Result<Vec<RenameLinkSource>, StorageError> {
    let rows = vault.query(
        "SELECT l.from_note_id, t.title, n.body_doc_id, l.target \
         FROM links l \
         JOIN tree t ON t.note_id = l.from_note_id AND t.deleted = 0 \
         JOIN notes n ON n.note_id = l.from_note_id \
         WHERE l.kind = 'wiki' AND l.to_note_id = ?1 \
         ORDER BY l.from_note_id, l.target",
        &[Value::Text(to_note_id.to_owned())],
    )?;
    let mut sources: BTreeMap<String, RenameLinkSource> = BTreeMap::new();
    for row in rows {
        let note_id = text_of(&row, 0, "links.from_note_id")?;
        let title = text_of(&row, 1, "tree.title")?;
        let doc_id = text_of(&row, 2, "notes.body_doc_id")?;
        let target = text_of(&row, 3, "links.target")?;
        sources
            .entry(note_id.clone())
            .or_insert_with(|| RenameLinkSource {
                note_id,
                title,
                doc_id,
                targets: BTreeSet::new(),
            })
            .targets
            .insert(target);
    }
    Ok(sources.into_values().collect())
}

/// 逐篇执行：打开正常会话、一次 `EditRequest`、有界 `close` 排空 durable + B 物化。
///
/// 调用方必须把**当前活动笔记**从 `sources` 里排除（活动 driver 才拥有那篇正文的
/// live 状态；另开会话会用旧快照覆盖在途编辑）。失败只计入报告，不回滚其它篇。
#[must_use]
pub fn rewrite_link_sources(
    vault: &Vault,
    sources: &[RenameLinkSource],
    old_title: &str,
    new_title: &str,
    options: RenameLinkOptions,
) -> RenameLinkReport {
    let mut report = RenameLinkReport {
        sources: sources.len(),
        ..RenameLinkReport::default()
    };
    for source in sources {
        match rewrite_link_source(vault, source, old_title, new_title, options) {
            Ok(Some(links)) => {
                report.notes_updated += 1;
                report.links_rewritten += links;
            }
            Ok(None) => report.notes_unchanged += 1,
            Err(message) => {
                eprintln!(
                    "[menghunote] 重命名联动改写失败 note={} title={:?}：{message}",
                    source.note_id, source.title
                );
                report.notes_failed += 1;
                report.failures.push(RenameLinkFailure {
                    note_id: source.note_id.clone(),
                    title: source.title.clone(),
                    message,
                });
            }
        }
    }
    report
}

/// 改写一篇来源；没有计划返回 `Ok(None)`，写了返回改写的链接数。
fn rewrite_link_source(
    vault: &Vault,
    source: &RenameLinkSource,
    old_title: &str,
    new_title: &str,
    options: RenameLinkOptions,
) -> Result<Option<usize>, String> {
    let mut driver_options = DriverOptions::default();
    driver_options.session.peer_id = options.peer_id;
    driver_options.session.sync = options.sync;
    let mut driver = SessionDriver::open(vault, &source.doc_id, driver_options)
        .map_err(|error| format!("打开正文会话失败：{error:?}"))?;

    let source_text = driver.markdown();
    let renames = plan_wiki_link_renames(&source_text, old_title, new_title, &source.targets);
    if renames.is_empty() {
        // 计划为空也要关闭会话（停掉 ticker / 物化线程，保持资源有界）。
        let _ = driver.close(options.timeout);
        return Ok(None);
    }
    let edits = scalar_edits_for_renames(&source_text, &renames)?;
    let link_count = edits.len();
    let snapshot = driver.session().snapshot();
    let request = EditRequest {
        doc_id: snapshot.doc_id.clone(),
        generation: snapshot.generation,
        session_epoch: snapshot.session_epoch,
        command_id: format!("rename-links:{}:{}", source.note_id, snapshot.live_rev.0),
        base_live_rev: snapshot.live_rev,
        edits,
        origin: "rename-links".to_owned(),
        undo_group: None,
    };
    match driver.apply(request) {
        EditOutcome::Applied(_) => {}
        EditOutcome::Rejected(rejected) => {
            let _ = driver.close(options.timeout);
            return Err(format!("会话拒绝改写：{:?}", rejected.reason));
        }
        EditOutcome::Stale(stale) => {
            let _ = driver.close(options.timeout);
            return Err(format!(
                "会话基准过期（current_live_rev={}）",
                stale.current_live_rev.0
            ));
        }
    }

    let close = driver.close(options.timeout);
    if close.timed_out || close.pending_edits > 0 || close.pending_materialize > 0 {
        return Err(format!(
            "正文未排空：pending_edits={} pending_materialize={} timed_out={}",
            close.pending_edits, close.pending_materialize, close.timed_out
        ));
    }
    if close.materialized == 0 {
        // 正文可能已经 durable，但 links 没刷新；按失败报告（不假装成功）。
        return Err("正文已写入，但 links 物化未完成".to_owned());
    }
    Ok(Some(link_count))
}

fn text_of(row: &[Value], index: usize, field: &str) -> Result<String, StorageError> {
    match row.get(index) {
        Some(Value::Text(text)) => Ok(text.clone()),
        other => Err(StorageError::Corrupt {
            context: format!("{field} 读回异常：{other:?}"),
        }),
    }
}
