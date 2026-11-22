//! 附件插入（dev-sync 构建）：把本地文件写进内容寻址的 `assets` 表，并在正文里留下
//! `asset://<hex>` 链接。
//!
//! ## 语义边界（与任务书一致）
//!
//! - **只做"插入链接 + 存字节"**：不渲染内联预览、不做拖放、不弹系统文件对话框
//!   （`rfd` 在本仓库被禁用），路径由调用方（UI 的单行输入框）给出；
//! - 文件的**绝对路径 / 文件名都不写日志**：日志只写哈希前缀与字节数，文件名只允许
//!   出现在 Markdown 链接文本里（用户数据）；
//! - 资产是内容寻址的：同内容重复插入是幂等的（单资产、多条 `asset_refs` 行），
//!   删除一条引用不会删除资产（无 GC 删除路径）；
//! - Markdown 里 `asset://` 只是**普通链接目标文本**，不改变既有解析/导出语义
//!   （导出仍是源文，链接原样写出）。
//!
//! ## 失败语义
//!
//! 读文件 / 校验（不存在、非普通文件、空文件、超过 20 MiB、非 UTF-8 文件名）都在
//! **任何写入之前**失败；写入顺序是"资产 → 引用 → 编辑器事务"，编辑器事务失败时由
//! 调用方 [`unlink_asset_ref`] 回滚引用（资产本身保留：内容寻址、无副作用）。

use std::path::{Path, PathBuf};

use menghunote_storage::{
    asset_content_hash, asset_meta_by_hash, insert_asset, link_asset_ref, unlink_asset_ref, Vault,
};

/// 单个附件的硬上限（20 MiB）。与 wire 的 64 MiB ASSET 上限无关：这是 UI 侧的体验边界。
pub const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// `asset://<hex>` 里十六进制哈希的长度（32B SHA-256 → 64 个字符）。
const ASSET_HASH_HEX_LEN: usize = 64;

/// 附件插入的明确错误（全部可在 UI 的 `attachment-error` 行里展示）。
///
/// 文案**不含完整路径**（约束：绝对路径不入日志/提示）；只给文件名或"该路径"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentError {
    /// 输入为空/纯空白。
    EmptyPath,
    /// 只接受绝对路径（相对路径会随 cwd 漂移）。
    NotAbsolute,
    /// 路径不存在。
    NotFound,
    /// 存在但不是普通文件（目录、设备文件、sockets…）。
    NotRegularFile,
    /// 空文件没有可同步的内容。
    EmptyFile,
    /// 超过 [`MAX_ATTACHMENT_BYTES`]。
    TooLarge { byte_len: u64, limit: u64 },
    /// 文件名不是 UTF-8（无法安全写进 Markdown 链接文本）。
    NonUtf8FileName,
    /// 读文件失败（`std::io::Error` 去掉路径后的分类/文案）。
    ReadFailed(String),
    /// 落库失败（storage）。资产可保留（内容寻址）但仍要报告。
    Storage(String),
    /// 当前笔记没有可用的 `note_id`（结构异常）。
    MissingNoteId,
    /// 没有打开的笔记（UI 层前置条件，不是文件问题）。
    NoActiveNote,
    /// 编辑器拒绝这次插入（同步持闸 / 输入法组合中 / 基准过期）。
    Editor(String),
}

impl AttachmentError {
    /// 稳定的错误类别（测试/文案断言用，不随人类可读文案变化）。
    #[must_use]
    pub fn category(&self) -> &'static str {
        match self {
            Self::EmptyPath => "empty-path",
            Self::NotAbsolute => "not-absolute",
            Self::NotFound => "not-found",
            Self::NotRegularFile => "not-regular-file",
            Self::EmptyFile => "empty-file",
            Self::TooLarge { .. } => "too-large",
            Self::NonUtf8FileName => "non-utf8-file-name",
            Self::ReadFailed(_) => "read-failed",
            Self::Storage(_) => "storage",
            Self::MissingNoteId => "missing-note-id",
            Self::NoActiveNote => "no-active-note",
            Self::Editor(_) => "editor",
        }
    }
}

impl std::fmt::Display for AttachmentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPath => formatter.write_str("请输入文件的绝对路径"),
            Self::NotAbsolute => formatter.write_str(
                "只接受绝对路径（例如 /home/you/report.pdf 或 C:\\Users\\you\\报告.pdf）",
            ),
            Self::NotFound => formatter.write_str("文件不存在（该路径下没有文件）"),
            Self::NotRegularFile => {
                formatter.write_str("该路径不是普通文件（目录/设备文件不支持）")
            }
            Self::EmptyFile => formatter.write_str("空文件没有可插入的内容"),
            Self::TooLarge { byte_len, limit } => write!(
                formatter,
                "文件 {byte_len} 字节，超过 {} MiB 上限",
                limit / (1024 * 1024)
            ),
            Self::NonUtf8FileName => {
                formatter.write_str("文件名不是 UTF-8，无法写进 Markdown 链接")
            }
            Self::ReadFailed(reason) => write!(formatter, "读取文件失败：{reason}"),
            Self::Storage(reason) => write!(formatter, "写入资产失败：{reason}"),
            Self::MissingNoteId => formatter.write_str("当前笔记缺少 note_id，无法建立资产引用"),
            Self::NoActiveNote => formatter.write_str("当前没有打开的笔记，无法插入附件"),
            Self::Editor(reason) => formatter.write_str(reason),
        }
    }
}

impl std::error::Error for AttachmentError {}

impl From<menghunote_storage::StorageError> for AttachmentError {
    fn from(error: menghunote_storage::StorageError) -> Self {
        Self::Storage(error.to_string())
    }
}

/// 一次"读取 + 校验"的结果（尚未写任何东西）。
///
/// 调用方先拿到它，再决定光标位置 → 才能落 `asset_refs.start_byte`；这样"读取失败"
/// 一定发生在编辑器事务之前（失败不插入 Markdown、不留半链接）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAttachment {
    /// 原始绝对路径（**不要写日志**；只用于再次读文件/诊断）。
    pub path: PathBuf,
    /// 文件名（UTF-8；写进 Markdown 链接文本前已转义）。
    pub file_name: String,
    /// 简单扩展名映射的 MIME；未知扩展名是 `application/octet-stream`。
    pub media_type: &'static str,
    /// 文件字节（≤ [`MAX_ATTACHMENT_BYTES`]）。
    pub data: Vec<u8>,
    /// `SHA-256(data)`：内容寻址的资产哈希。
    pub asset_hash: [u8; 32],
    /// 要插入编辑器光标处的 Markdown：`[附件：<文件名>](asset://<hex>)`。
    pub markdown: String,
}

impl PendingAttachment {
    /// 资产字节数。
    #[must_use]
    pub fn byte_len(&self) -> u64 {
        self.data.len() as u64
    }
}

/// 一次成功插入的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentOutcome {
    /// 目标笔记。
    pub note_id: String,
    /// 资产哈希（内容寻址）。
    pub asset_hash: [u8; 32],
    /// 资产字节数。
    pub byte_len: u64,
    /// 文件名（未转义，展示/测试用；写进 Markdown 的是转义后的文本）。
    pub file_name: String,
    /// 实际插入的 Markdown。
    pub markdown: String,
    /// 链接在正文 source 坐标里的起始字节。
    pub start_byte: u64,
    /// 插入前该资产是否**已经存在**（重复插入 = 单资产 + 多条引用）。
    pub asset_preexisting: bool,
}

/// 读取并校验一个待插入的附件（**无副作用**）。
///
/// 错误优先级：空输入 → 相对路径 → 不存在 → 非普通文件 → 空文件 → 超限 → 文件名非
/// UTF-8 → 读失败。
pub fn read_attachment(path: impl AsRef<Path>) -> Result<PendingAttachment, AttachmentError> {
    let path = path.as_ref();
    let display = path.to_string_lossy();
    if display.trim().is_empty() {
        return Err(AttachmentError::EmptyPath);
    }
    if !path.is_absolute() {
        return Err(AttachmentError::NotAbsolute);
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => AttachmentError::NotFound,
        _ => AttachmentError::ReadFailed(io_reason(&error)),
    })?;
    // 符号链接指向文件也允许（`metadata()` 会跟随）；这里用 `metadata` 判"普通文件"。
    let metadata = if metadata.file_type().is_symlink() {
        std::fs::metadata(path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => AttachmentError::NotFound,
            _ => AttachmentError::ReadFailed(io_reason(&error)),
        })?
    } else {
        metadata
    };
    if !metadata.is_file() {
        return Err(AttachmentError::NotRegularFile);
    }
    let byte_len = metadata.len();
    if byte_len == 0 {
        return Err(AttachmentError::EmptyFile);
    }
    if byte_len > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge {
            byte_len,
            limit: MAX_ATTACHMENT_BYTES,
        });
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(AttachmentError::NonUtf8FileName)?
        .to_owned();
    let data =
        std::fs::read(path).map_err(|error| AttachmentError::ReadFailed(io_reason(&error)))?;
    // 元数据与读取之间文件被改大：按真实读到的长度再判一次（上限/空）。
    let byte_len = data.len() as u64;
    if byte_len == 0 {
        return Err(AttachmentError::EmptyFile);
    }
    if byte_len > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge {
            byte_len,
            limit: MAX_ATTACHMENT_BYTES,
        });
    }
    let asset_hash = asset_content_hash(&data);
    let media_type = media_type_for(&file_name);
    Ok(PendingAttachment {
        path: path.to_path_buf(),
        file_name: file_name.clone(),
        media_type,
        markdown: attachment_markdown(&file_name, &asset_hash),
        data,
        asset_hash,
    })
}

/// 把已读取的附件写进 `assets` / `asset_refs`（**不碰编辑器**）。
///
/// `start_byte` 是链接在正文 source 坐标里的起点（调用方按光标计算），`body_seq` 是
/// 写入时的正文 head 序号（引用只是账本，不参与同步裁决）。
pub fn store_attachment(
    vault: &Vault,
    note_id: &str,
    body_seq: u64,
    start_byte: u64,
    pending: &PendingAttachment,
) -> Result<AttachmentOutcome, AttachmentError> {
    if note_id.trim().is_empty() {
        return Err(AttachmentError::MissingNoteId);
    }
    let existing = asset_meta_by_hash(vault, &pending.asset_hash)?;
    insert_asset(
        vault,
        &pending.asset_hash,
        pending.byte_len(),
        Some(pending.media_type),
        &pending.data,
    )?;
    link_asset_ref(vault, note_id, &pending.asset_hash, start_byte, body_seq)?;
    Ok(AttachmentOutcome {
        note_id: note_id.to_owned(),
        asset_hash: pending.asset_hash,
        byte_len: pending.byte_len(),
        file_name: pending.file_name.clone(),
        markdown: pending.markdown.clone(),
        start_byte,
        asset_preexisting: existing.is_some(),
    })
}

/// 回滚一条引用（编辑器事务失败时用；资产保留，内容寻址无副作用）。
pub fn rollback_attachment_ref(
    vault: &Vault,
    note_id: &str,
    asset_hash: &[u8; 32],
    start_byte: u64,
) -> Result<bool, AttachmentError> {
    Ok(unlink_asset_ref(vault, note_id, asset_hash, start_byte)?)
}

/// 一步到位：读取 + 落库（编辑器无关的测试/工具入口）。
pub fn insert_attachment(
    vault: &Vault,
    note_id: &str,
    body_seq: u64,
    start_byte: u64,
    path: impl AsRef<Path>,
) -> Result<AttachmentOutcome, AttachmentError> {
    let pending = read_attachment(path)?;
    store_attachment(vault, note_id, body_seq, start_byte, &pending)
}

/// 把哈希编码成 `asset://` 目标（小写十六进制，定长 64）。
#[must_use]
pub fn asset_uri(hash: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity("asset://".len() + ASSET_HASH_HEX_LEN);
    out.push_str("asset://");
    for byte in hash {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// 生成插入正文的 Markdown：链接文本是文件名（转义 `\` `[` `]` `(` `)`），目标是
/// `asset://<hex>`。重复文件名允许（内容寻址才是身份）。
#[must_use]
pub fn attachment_markdown(file_name: &str, hash: &[u8; 32]) -> String {
    format!(
        "[附件：{}]({})",
        escape_link_text(file_name),
        asset_uri(hash)
    )
}

/// 转义 Markdown 链接文本里的定界符：`\` `[` `]` `(` `)`，并把换行压成空格
/// （文件名在 UI 里是单行；换行会破坏链接结构）。
fn escape_link_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' | '[' | ']' | '(' | ')' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' | '\r' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

/// 简单扩展名 → MIME 映射；未知扩展名一律 `application/octet-stream`。
#[must_use]
pub fn media_type_for(file_name: &str) -> &'static str {
    let extension = Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt" | "log") => "text/plain",
        Some("md" | "markdown") => "text/markdown",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("zip") => "application/zip",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// `std::io::Error` → 不含路径的短文案（日志/错误行都不带绝对路径）。
fn io_reason(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => "文件不存在".to_owned(),
        std::io::ErrorKind::PermissionDenied => "没有读取权限".to_owned(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_escapes_link_delimiters() {
        let hash = [0xab; 32];
        let markdown = attachment_markdown("a[b](c)\\d.txt", &hash);
        assert_eq!(
            markdown,
            format!("[附件：a\\[b\\]\\(c\\)\\\\d.txt]({})", asset_uri(&hash))
        );
        assert_eq!(
            markdown.matches("ab").count(),
            ASSET_HASH_HEX_LEN / 2,
            "哈希必须编码成 64 位小写十六进制：{markdown}"
        );
    }

    #[test]
    fn unknown_extension_is_octet_stream() {
        assert_eq!(media_type_for("x.bin"), "application/octet-stream");
        assert_eq!(media_type_for("x.PNG"), "image/png");
    }

    #[test]
    fn relative_path_is_rejected_before_touching_disk() {
        let err = read_attachment("relative.txt").expect_err("相对路径必须拒绝");
        assert_eq!(err, AttachmentError::NotAbsolute);
        assert_eq!(err.category(), "not-absolute");
    }
}
