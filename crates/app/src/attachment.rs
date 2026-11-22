//! 附件/图片导入（ADR 0019 §2/§3，S2b）：把**字节**写进内容寻址的 `assets` 表，并在
//! 正文里留下 `asset://<hex>` 链接。
//!
//! ## 语义边界（与任务书一致）
//!
//! - **只做"读取/校验 + 存字节 + 写引用"**：不弹系统文件对话框（`rfd` 在本仓库被
//!   禁用）、不递归目录、不跟随相对路径；路径只可能来自剪贴板（CF_HDROP）或拖放；
//! - 文件的**绝对路径 / 文件名都不写日志**：日志只写哈希前缀与字节数，文件名只允许
//!   出现在 Markdown 链接文字里（用户数据）；
//! - 资产是内容寻址的：同内容重复导入是幂等的（单资产、多条 `asset_refs` 行），
//!   删除一条引用不会删除资产（无 GC 删除路径）；
//! - Markdown 里 `asset://` 只是**普通链接目标文本**，不改变既有解析/导出语义
//!   （导出仍是源文，链接原样写出）。
//!
//! ## 失败语义
//!
//! 读文件 / 校验（不存在、非普通文件、空文件、超过 20 MiB、非 UTF-8 文件名、批量超限）
//! 都在**任何写入之前**失败；写入顺序是"资产 → 引用 → 编辑器事务"，编辑器事务失败时
//! 由调用方 [`rollback_attachment_ref`]（批量用 [`VaultAssetWriter`] 的 `rollback`）回滚
//! 引用（资产本身保留：内容寻址、无副作用）。
//!
//! ## 引用对账（ADR 0019 §3）
//!
//! [`asset_refs_in_source`] 从正文里**字节扫描**所有合法 `asset://<64 小写 hex>`
//! （内联、引用式链接、`[label]: asset://…` 定义行都能扫到；代码块里的也保留——宁可
//! 多留一条账本，也不要因为解析猜测丢掉真实引用）。对账由 storage 的
//! [`menghunote_storage::replace_asset_refs`] 一个事务整篇替换。

use std::io::Cursor;
use std::path::{Path, PathBuf};

use menghunote_storage::{
    asset_content_hash, asset_meta_by_hash, insert_asset, link_asset_ref, list_asset_refs,
    list_tree, replace_asset_refs, unlink_asset_ref, AssetRefReconcile, Vault,
};

use crate::editor::clipboard::{
    AssetImportWriter, AssetRefSummary, ImportImageFormat, ImportPayload, SharedImportWriter,
    StoredImport, StoredRef,
};

/// 单个附件的硬上限（20 MiB）。与 wire 的 64 MiB ASSET 上限无关：这是 UI 侧的体验边界。
pub const MAX_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// 一次导入的**文件个数**上限（ADR 0019 §2：每批 ≤ 10 项）。
pub const MAX_IMPORT_FILES: usize = 10;

/// 一次导入的**总输入**上限（ADR 0019 §2：≤ 100 MiB）。
pub const MAX_IMPORT_TOTAL_BYTES: u64 = 100 * 1024 * 1024;

/// 剪贴板图片的默认文件名主干（ADR 0019 §2：`文件名或"粘贴图片"`）。
pub const CLIPBOARD_IMAGE_STEM: &str = "粘贴图片";

/// `asset://<hex>` 里十六进制哈希的长度（32B SHA-256 → 64 个字符）。
const ASSET_HASH_HEX_LEN: usize = 64;

/// `asset://` 前缀。
const ASSET_URI_PREFIX: &str = "asset://";

/// 附件插入的明确错误（全部可在 UI 的提示行里展示）。
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
    /// 一批文件超过 [`MAX_IMPORT_FILES`]。
    TooManyFiles { count: usize, limit: usize },
    /// 一批文件的总字节数超过 [`MAX_IMPORT_TOTAL_BYTES`]。
    BatchTooLarge { byte_len: u64, limit: u64 },
    /// 一批里没有任何可导入的普通文件（空列表）。
    EmptyImport,
    /// 文件名不是 UTF-8（无法安全写进 Markdown 链接文本）。
    NonUtf8FileName,
    /// 读文件失败（`std::io::Error` 去掉路径后的分类/文案）。
    ReadFailed(String),
    /// 图片字节损坏 / 与声明的格式不符 / 解码器读不出尺寸。
    CorruptImage(String),
    /// 图片尺寸超出预览预算（边长 ≤ 8192、总像素 ≤ 16MP）。
    ImageTooLarge { width: u32, height: u32 },
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
            Self::TooManyFiles { .. } => "too-many-files",
            Self::BatchTooLarge { .. } => "batch-too-large",
            Self::EmptyImport => "empty-import",
            Self::NonUtf8FileName => "non-utf8-file-name",
            Self::ReadFailed(_) => "read-failed",
            Self::CorruptImage(_) => "corrupt-image",
            Self::ImageTooLarge { .. } => "image-too-large",
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
            Self::TooManyFiles { count, limit } => {
                write!(formatter, "一次最多导入 {limit} 个文件（当前 {count} 个）")
            }
            Self::BatchTooLarge { byte_len, limit } => write!(
                formatter,
                "这批文件共 {byte_len} 字节，超过 {} MiB 上限",
                limit / (1024 * 1024)
            ),
            Self::EmptyImport => formatter.write_str("没有可导入的文件"),
            Self::NonUtf8FileName => {
                formatter.write_str("文件名不是 UTF-8，无法写进 Markdown 链接")
            }
            Self::ReadFailed(reason) => write!(formatter, "读取文件失败：{reason}"),
            Self::CorruptImage(reason) => write!(formatter, "图片字节无法识别：{reason}"),
            Self::ImageTooLarge { width, height } => write!(
                formatter,
                "图片 {width}×{height} 超出预览预算（边长 ≤ 8192、总像素 ≤ 16MP）"
            ),
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
    ///
    /// 剪贴板图片/字节没有来源路径，这里是 `None`。
    pub path: Option<PathBuf>,
    /// 文件名（UTF-8；写进 Markdown 链接文本前已转义）。
    pub file_name: String,
    /// 简单扩展名映射的 MIME；未知扩展名是 `application/octet-stream`。
    pub media_type: &'static str,
    /// 文件字节（≤ [`MAX_ATTACHMENT_BYTES`]）。
    pub data: Vec<u8>,
    /// `SHA-256(data)`：内容寻址的资产哈希。
    pub asset_hash: [u8; 32],
    /// 要插入编辑器的 Markdown（图片是 `![名字](asset://<hex>)`，其它是
    /// `[附件：<名字>](asset://<hex>)`）。
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

/// 路径的只读元信息（读字节之前的全部校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct InspectedPath {
    file_name: String,
    media_type: &'static str,
    byte_len: u64,
}

/// 读取并校验一个待插入的附件（**无副作用**）。
///
/// 错误优先级：空输入 → 相对路径 → 不存在 → 非普通文件 → 空文件 → 超限 → 文件名非
/// UTF-8 → 读失败。
pub fn read_attachment(path: impl AsRef<Path>) -> Result<PendingAttachment, AttachmentError> {
    let path = path.as_ref();
    let inspected = inspect_attachment_path(path)?;
    read_inspected(path, &inspected)
}

/// 只做元数据校验（不读字节）：批量导入先用它把**上限**判掉，再统一读。
fn inspect_attachment_path(path: &Path) -> Result<InspectedPath, AttachmentError> {
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
    let media_type = media_type_for(&file_name);
    Ok(InspectedPath {
        file_name,
        media_type,
        byte_len,
    })
}

/// 读字节并组装 [`PendingAttachment`]（校验已在 [`inspect_attachment_path`] 做过，
/// 但**读完再判一次**长度：元数据与读取之间文件可能被改大/清空）。
fn read_inspected(
    path: &Path,
    inspected: &InspectedPath,
) -> Result<PendingAttachment, AttachmentError> {
    let data =
        std::fs::read(path).map_err(|error| AttachmentError::ReadFailed(io_reason(&error)))?;
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
    let markdown = if is_preview_image(&inspected.file_name) {
        image_markdown(&inspected.file_name, &asset_hash)
    } else {
        attachment_markdown(&inspected.file_name, &asset_hash)
    };
    Ok(PendingAttachment {
        path: Some(path.to_path_buf()),
        file_name: inspected.file_name.clone(),
        media_type: inspected.media_type,
        data,
        asset_hash,
        markdown,
    })
}

/// 一批文件：先校验个数/单文件/总量，再逐个读取；任何一项失败都不产生写入。
///
/// 只接受**普通文件**：目录、设备文件、相对路径一律整批失败（不递归、不跟随目录）。
pub fn stage_paths(paths: &[PathBuf]) -> Result<Vec<PendingAttachment>, AttachmentError> {
    if paths.is_empty() {
        return Err(AttachmentError::EmptyImport);
    }
    if paths.len() > MAX_IMPORT_FILES {
        return Err(AttachmentError::TooManyFiles {
            count: paths.len(),
            limit: MAX_IMPORT_FILES,
        });
    }
    let mut total: u64 = 0;
    let mut inspected = Vec::with_capacity(paths.len());
    for path in paths {
        let info = inspect_attachment_path(path)?;
        total = total.saturating_add(info.byte_len);
        if total > MAX_IMPORT_TOTAL_BYTES {
            return Err(AttachmentError::BatchTooLarge {
                byte_len: total,
                limit: MAX_IMPORT_TOTAL_BYTES,
            });
        }
        inspected.push(info);
    }
    inspected
        .iter()
        .zip(paths)
        .map(|(info, path)| read_inspected(path, info))
        .collect()
}

/// 一批导入负载 → 已校验、尚未落库的附件列表（**任何写入之前**完成全部失败判定）。
pub fn stage_payload(payload: &ImportPayload) -> Result<Vec<PendingAttachment>, AttachmentError> {
    match payload {
        ImportPayload::Files(paths) => stage_paths(paths),
        ImportPayload::Image { format, bytes } => Ok(vec![stage_image(*format, bytes)?]),
        ImportPayload::Binary { extension, bytes } => Ok(vec![stage_binary(extension, bytes)?]),
    }
}

/// 剪贴板图片字节 → 附件（先校验格式/尺寸，损坏就不写正文）。
///
/// 校验顺序：非空 → ≤ 20 MiB → 魔数与声明格式一致（不一致以**真实字节**为准）→
/// 解码器读得出尺寸且不超预览预算。只有全部通过才组装 Markdown。
pub fn stage_image(
    declared: ImportImageFormat,
    bytes: &[u8],
) -> Result<PendingAttachment, AttachmentError> {
    if bytes.is_empty() {
        return Err(AttachmentError::EmptyFile);
    }
    let byte_len = bytes.len() as u64;
    if byte_len > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge {
            byte_len,
            limit: MAX_ATTACHMENT_BYTES,
        });
    }
    // 声明格式与实际字节不一致时以字节为准（剪贴板可能挂错格式名）。
    let detected = ImportImageFormat::sniff(bytes).ok_or_else(|| {
        AttachmentError::CorruptImage("不是 PNG/JPEG/WebP/GIF/BMP 的字节头".to_owned())
    })?;
    let _ = declared;
    // Windows 剪贴板 CF_DIB 经 GPUI 转成 BMP；资产管线只预览 PNG/JPEG/WebP/GIF，
    // 所以**入库前**统一转 PNG（保留像素，不把 BMP 变成普通附件）。
    let (format, data) = if detected == ImportImageFormat::Bmp {
        (ImportImageFormat::Png, bmp_to_png(bytes)?)
    } else {
        (detected, bytes.to_vec())
    };
    // 只校验预算（尺寸本身不写进正文/资产列：沿用现有 schema，不加列）。
    image_dimensions(&data, format)?;
    let file_name = format!("{CLIPBOARD_IMAGE_STEM}.{}", format.extension());
    let asset_hash = asset_content_hash(&data);
    Ok(PendingAttachment {
        path: None,
        media_type: format.media_type(),
        markdown: image_markdown(&file_name, &asset_hash),
        file_name,
        data,
        asset_hash,
    })
}

/// 剪贴板 BMP → PNG（Windows CF_DIB 的落库前转换）。
fn bmp_to_png(bytes: &[u8]) -> Result<Vec<u8>, AttachmentError> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Bmp)
        .map_err(|error| AttachmentError::CorruptImage(format!("BMP 解码失败：{error}")))?;
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|error| AttachmentError::CorruptImage(format!("BMP 转 PNG 失败：{error}")))?;
    Ok(output.into_inner())
}

/// 剪贴板里的非可预览字节（SVG / TIFF / ICO；BMP 已在上游转 PNG）→ 普通附件。
pub fn stage_binary(extension: &str, bytes: &[u8]) -> Result<PendingAttachment, AttachmentError> {
    if bytes.is_empty() {
        return Err(AttachmentError::EmptyFile);
    }
    let byte_len = bytes.len() as u64;
    if byte_len > MAX_ATTACHMENT_BYTES {
        return Err(AttachmentError::TooLarge {
            byte_len,
            limit: MAX_ATTACHMENT_BYTES,
        });
    }
    let extension = sanitize_extension(extension);
    let file_name = format!("{CLIPBOARD_IMAGE_STEM}.{extension}");
    let asset_hash = asset_content_hash(bytes);
    Ok(PendingAttachment {
        path: None,
        file_name: file_name.clone(),
        media_type: media_type_for(&file_name),
        markdown: attachment_markdown(&file_name, &asset_hash),
        data: bytes.to_vec(),
        asset_hash,
    })
}

/// 一批附件的 Markdown：单项就是它自己那一段；多项之间用 `\n` 分隔（各自独占一行，
/// 不追加任何其它内容）。
///
/// 返回 `(markdown, 每项的引用定位点)`。**定位点是 `asset://` 前缀在批内的字节偏移**，
/// 与 [`asset_refs_in_source`] 的扫描结果同一约定（引用账本只记"哪一段正文引用了哪个
/// 资产"，两种坐标都能定位；统一成一种才能让"预写引用"和"正文对账"逐字节一致）。
#[must_use]
pub fn batch_markdown(items: &[PendingAttachment]) -> (String, Vec<u64>) {
    let mut markdown = String::new();
    let mut starts = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            markdown.push('\n');
        }
        starts.push(
            markdown.len() as u64
                + u64::try_from(item.markdown.find(ASSET_URI_PREFIX).unwrap_or(0)).unwrap_or(0),
        );
        markdown.push_str(&item.markdown);
    }
    (markdown, starts)
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
    let mut out = String::with_capacity(ASSET_URI_PREFIX.len() + ASSET_HASH_HEX_LEN);
    out.push_str(ASSET_URI_PREFIX);
    for byte in hash {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// 生成插入正文的附件 Markdown：链接文字是文件名（转义 `\` `[` `]` `(` `)`），目标是
/// `asset://<hex>`。重复文件名允许（内容寻址才是身份）。
#[must_use]
pub fn attachment_markdown(file_name: &str, hash: &[u8; 32]) -> String {
    format!(
        "[附件：{}]({})",
        escape_link_text(file_name),
        asset_uri(hash)
    )
}

/// 生成插入正文的图片 Markdown：`![<名字>](asset://<hex>)`。
///
/// 不补换行、不写尺寸 HTML（ADR 0019 §1）：能否投影成 Object Block 由所在行决定。
#[must_use]
pub fn image_markdown(file_name: &str, hash: &[u8; 32]) -> String {
    format!("![{}]({})", escape_link_text(file_name), asset_uri(hash))
}

/// 这个文件名是不是"可预览位图"（只认 ADR 0019 首版四种格式；SVG 不算）。
#[must_use]
pub fn is_preview_image(file_name: &str) -> bool {
    matches!(
        Path::new(file_name)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

/// 转义 Markdown 链接/alt 文字里的定界符：`\` `[` `]` `(` `)`，并把换行压成空格
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
        Some("bmp") => "image/bmp",
        Some("tif" | "tiff") => "image/tiff",
        Some("ico") => "image/ico",
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

/// 从正文里扫出所有合法 `asset://<64 小写 hex>`（字节偏移 = 链接起点）。
///
/// - **严格**：必须正好 64 个小写 hex，且后一个字节不是小写 hex（`asset://<65 hex>`
///   不算一条引用）；
/// - 覆盖内联/引用式图片/链接与 `[label]: asset://…` 定义行；行内代码/围栏代码里的
///   出现也计入（fail-open：宁可多留一条账本，也不误删真实引用）；
/// - 结果按出现顺序返回，`start_byte` 是 **`asset://` 前缀的 UTF-8 字节偏移**
///   （与 `asset_refs.start_byte` 同一坐标：引用账本只关心"哪一段正文引用了哪个资产"）。
#[must_use]
pub fn asset_refs_in_source(source: &str) -> Vec<([u8; 32], u64)> {
    let bytes = source.as_bytes();
    let mut refs = Vec::new();
    let mut index = 0usize;
    while let Some(found) = find_from(bytes, ASSET_URI_PREFIX.as_bytes(), index) {
        let hash_start = found + ASSET_URI_PREFIX.len();
        let hash_end = hash_start + ASSET_HASH_HEX_LEN;
        let valid = hash_end <= bytes.len()
            && bytes[hash_start..hash_end]
                .iter()
                .all(|byte| is_lower_hex(*byte))
            && bytes.get(hash_end).is_none_or(|byte| !is_lower_hex(*byte));
        let hash = valid
            .then(|| decode_hash(&bytes[hash_start..hash_end]))
            .flatten();
        if let Some(hash) = hash {
            refs.push((hash, found as u64));
            index = hash_end;
        } else {
            index = hash_start;
        }
    }
    refs
}

/// 把正文按当前引用集合**整篇对账**（打开笔记/正文 durable 后由 `EditorView` 调）。
///
/// 返回 `None` 表示没有对应 note（正常跳过）；同一集合不重复写库（幂等、不刷事务）。
pub fn reconcile_note_refs(
    vault: &Vault,
    note_id: &str,
    body_seq: u64,
    source: &str,
) -> Result<AssetRefReconcile, AttachmentError> {
    let refs = asset_refs_in_source(source);
    let current = list_asset_refs(vault, note_id)?;
    if refs_match(&current, &refs) {
        return Ok(AssetRefReconcile {
            refs: refs.len(),
            placeholders: 0,
            duplicates: 0,
        });
    }
    Ok(replace_asset_refs(vault, note_id, body_seq, &refs)?)
}

/// 结构投影里按 `body_doc_id` 找 `note_id`（导入前**重验** note 归属；切笔记/删笔记
/// 之后这里就找不到）。
#[must_use]
pub fn resolve_note_for_doc(vault: &Vault, doc_id: &str) -> Option<String> {
    list_tree(vault).ok()?.into_iter().find_map(|entry| {
        if entry.body_doc_id.as_deref() == Some(doc_id) {
            entry.note_id
        } else {
            None
        }
    })
}

/// `AppShell` 注入给 `EditorView` 的导入实现：**唯一**写 `assets`/`asset_refs` 的地方。
///
/// 编辑器只认 [`AssetImportWriter`] 这个能力面（不依赖 storage / 不碰文件系统）。
#[derive(Debug, Clone)]
pub struct VaultAssetWriter {
    vault: Vault,
}

impl VaultAssetWriter {
    /// 用同一个 vault 句柄构造（`Vault` 是 `Clone + Send + Sync`）。
    #[must_use]
    pub fn new(vault: Vault) -> Self {
        Self { vault }
    }

    /// 共享句柄（`EditorView::new` 的注入参数）。
    #[must_use]
    pub fn shared(vault: &Vault) -> SharedImportWriter {
        std::sync::Arc::new(Self::new(vault.clone()))
    }
}

impl AssetImportWriter for VaultAssetWriter {
    fn stage_and_store(
        &self,
        doc_id: &str,
        body_seq: u64,
        base_start: u64,
        payload: &ImportPayload,
    ) -> Result<StoredImport, String> {
        // 1) 重验 note 归属：按**当前**结构投影从 doc_id 反查 note_id。
        let note_id = resolve_note_for_doc(&self.vault, doc_id)
            .ok_or_else(|| "当前文档没有对应的笔记（可能已删除或切走）".to_owned())?;
        // 2) 全部校验 + 读取（任何失败都发生在写入之前）。
        let items = stage_payload(payload).map_err(|error| error.to_string())?;
        let (markdown, starts) = batch_markdown(&items);
        // 3) 资产 + 引用；中途失败要把本批已写的引用回滚掉。
        let mut refs: Vec<StoredRef> = Vec::with_capacity(items.len());
        let mut byte_len = 0u64;
        for (item, relative) in items.iter().zip(starts) {
            let start_byte = base_start.saturating_add(relative);
            if let Err(error) = store_attachment(&self.vault, &note_id, body_seq, start_byte, item)
            {
                rollback_refs(&self.vault, &note_id, &refs);
                return Err(error.to_string());
            }
            byte_len = byte_len.saturating_add(item.byte_len());
            refs.push(StoredRef {
                start_byte,
                asset_hash: item.asset_hash,
            });
        }
        Ok(StoredImport {
            note_id,
            body_seq,
            base_start,
            markdown,
            items: items.len(),
            byte_len,
            refs,
        })
    }

    fn commit(&self, stored: &StoredImport, source: &str) -> Result<AssetRefSummary, String> {
        let report = reconcile_note_refs(&self.vault, &stored.note_id, stored.body_seq, source)
            .map_err(|error| error.to_string())?;
        Ok(AssetRefSummary {
            refs: report.refs,
            placeholders: report.placeholders,
        })
    }

    fn rollback(&self, stored: &StoredImport) {
        rollback_refs(&self.vault, &stored.note_id, &stored.refs);
    }

    fn reconcile(
        &self,
        doc_id: &str,
        body_seq: u64,
        source: &str,
    ) -> Result<Option<AssetRefSummary>, String> {
        let Some(note_id) = resolve_note_for_doc(&self.vault, doc_id) else {
            return Ok(None);
        };
        let report = reconcile_note_refs(&self.vault, &note_id, body_seq, source)
            .map_err(|error| error.to_string())?;
        Ok(Some(AssetRefSummary {
            refs: report.refs,
            placeholders: report.placeholders,
        }))
    }
}

/// 回滚一批**本批新增**的引用（资产保留；失败只记日志，不打断导入路径）。
fn rollback_refs(vault: &Vault, note_id: &str, refs: &[StoredRef]) {
    for entry in refs {
        match unlink_asset_ref(vault, note_id, &entry.asset_hash, entry.start_byte) {
            Ok(true) => {}
            Ok(false) => eprintln!(
                "[menghunote] 回滚引用：没找到 hash-{}/offset={}（可能已被对账覆盖）",
                short_hash(&entry.asset_hash),
                entry.start_byte
            ),
            Err(error) => eprintln!("[menghunote] 回滚引用失败：{error}"),
        }
    }
}

/// 日志用的短哈希（前 4 字节 = 8 个十六进制字符；够区分，又不会把整串刷进日志）。
#[must_use]
pub fn short_hash(hash: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(8);
    for byte in &hash[..4] {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// 当前引用集合是否与扫描结果一致（只比 `(hash, start_byte)`，`body_seq` 是提示字段）。
fn refs_match(current: &[menghunote_storage::AssetRef], scanned: &[([u8; 32], u64)]) -> bool {
    if current.len() != scanned.len() {
        return false;
    }
    let mut left: Vec<([u8; 32], u64)> = current
        .iter()
        .map(|entry| (entry.asset_hash, entry.start_byte))
        .collect();
    let mut right: Vec<([u8; 32], u64)> = scanned.to_vec();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

/// 剪贴板图片的尺寸探测（**只读头部**，不解码像素、不分配大内存）。
fn image_dimensions(
    bytes: &[u8],
    format: ImportImageFormat,
) -> Result<(u32, u32), AttachmentError> {
    use crate::editor::object_image::{MAX_IMAGE_EDGE, MAX_IMAGE_PIXELS};
    let image_format = match format {
        ImportImageFormat::Png => image::ImageFormat::Png,
        ImportImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImportImageFormat::Gif => image::ImageFormat::Gif,
        ImportImageFormat::WebP => image::ImageFormat::WebP,
        ImportImageFormat::Bmp => image::ImageFormat::Bmp,
    };
    let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), image_format)
        .into_dimensions()
        .map_err(|error| AttachmentError::CorruptImage(error.to_string()))?;
    if width == 0 || height == 0 {
        return Err(AttachmentError::CorruptImage("尺寸为 0".to_owned()));
    }
    if width > MAX_IMAGE_EDGE
        || height > MAX_IMAGE_EDGE
        || u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
    {
        return Err(AttachmentError::ImageTooLarge { width, height });
    }
    Ok((width, height))
}

/// 扩展名清洗：只保留 ASCII 字母/数字（防止 `../x` 之类的注入进文件名）。
fn sanitize_extension(extension: &str) -> String {
    let cleaned: String = extension
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(16)
        .collect();
    if cleaned.is_empty() {
        "bin".to_owned()
    } else {
        cleaned.to_ascii_lowercase()
    }
}

/// `needle` 在 `haystack` 里从 `from` 起的下一个位置。
fn find_from(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// 小写十六进制字符。
fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

/// 64 个已验证的小写 hex → 32 字节哈希。
fn decode_hash(text: &[u8]) -> Option<[u8; 32]> {
    if text.len() != ASSET_HASH_HEX_LEN {
        return None;
    }
    let mut hash = [0u8; 32];
    let (pairs, remainder) = text.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    for (index, pair) in pairs.iter().enumerate() {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        hash[index] = u8::try_from(high * 16 + low).ok()?;
    }
    Some(hash)
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

    fn hex(hash: &[u8; 32]) -> String {
        hash.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn hash_of(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

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
        // 换行压成空格（文件名是单行，换行会破坏链接结构）。
        assert_eq!(
            image_markdown("a\nb.png", &hash),
            format!("![a b.png]({})", asset_uri(&hash))
        );
    }

    #[test]
    fn unknown_extension_is_octet_stream() {
        assert_eq!(media_type_for("x.bin"), "application/octet-stream");
        assert_eq!(media_type_for("x.PNG"), "image/png");
        assert!(is_preview_image("x.PNG"));
        assert!(is_preview_image("a/b/图.webp"));
        assert!(!is_preview_image("x.svg"), "SVG 按普通附件处理");
        assert!(!is_preview_image("x.pdf"));
    }

    #[test]
    fn relative_path_is_rejected_before_touching_disk() {
        let err = read_attachment("relative.txt").expect_err("相对路径必须拒绝");
        assert_eq!(err, AttachmentError::NotAbsolute);
        assert_eq!(err.category(), "not-absolute");
    }

    #[test]
    fn asset_refs_scan_covers_inline_reference_and_ignores_bad_hex() {
        let first = hash_of(0x11);
        let second = hash_of(0x22);
        let first_hex = hex(&first);
        let second_hex = hex(&second);
        let source = format!(
            "前 ![图](asset://{first_hex}) 后\n\n[附件：x](asset://{second_hex})\n\n[label]: asset://{first_hex}\n"
        );
        let refs = asset_refs_in_source(&source);
        assert_eq!(
            refs.len(),
            3,
            "内联图片 + 链接 + 引用式定义都要扫到：{refs:?}"
        );
        assert_eq!(refs[0], (first, "前 ![图](".len() as u64));
        assert_eq!(refs[1].0, second);
        assert_eq!(refs[2].0, first);
        let definition_start = source.find("[label]:").expect("定义行") as u64;
        assert_eq!(refs[2].1, definition_start + "[label]: ".len() as u64);
        // 多字节前缀不改变字节偏移语义。
        let source = format!("中文😀![x](asset://{first_hex})");
        let refs = asset_refs_in_source(&source);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].1, "中文😀![x](".len() as u64);
    }

    #[test]
    fn asset_refs_scan_is_strict_about_length_and_case() {
        let lower = "a".repeat(64);
        assert_eq!(asset_refs_in_source(&format!("asset://{lower}")).len(), 1);
        // 大写 hex 不是合法资产链接。
        assert!(asset_refs_in_source(&format!("asset://{}", "A".repeat(64))).is_empty());
        // 63 / 65 位都不算（65 位后面还有 hex）。
        assert!(asset_refs_in_source(&format!("asset://{}", "a".repeat(63))).is_empty());
        assert!(asset_refs_in_source(&format!("asset://{}", "a".repeat(65))).is_empty());
        // 紧跟着非法字符（非 hex）时仍然成立。
        assert_eq!(asset_refs_in_source(&format!("asset://{lower})")).len(), 1);
        // 没有 asset:// 前缀、或者不是 hex 的字符。
        assert!(asset_refs_in_source(&lower).is_empty());
        assert!(asset_refs_in_source("asset://zzzz").is_empty());
        // 空文档。
        assert!(asset_refs_in_source("").is_empty());
    }

    #[test]
    fn batch_markdown_keeps_order_and_tracks_offsets() {
        let one = PendingAttachment {
            path: None,
            file_name: "a.txt".to_owned(),
            media_type: media_type_for("a.txt"),
            markdown: attachment_markdown("a.txt", &hash_of(0x01)),
            data: b"a".to_vec(),
            asset_hash: hash_of(0x01),
        };
        let two = PendingAttachment {
            path: None,
            file_name: "b.png".to_owned(),
            media_type: media_type_for("b.png"),
            markdown: image_markdown("b.png", &hash_of(0x02)),
            data: b"b".to_vec(),
            asset_hash: hash_of(0x02),
        };
        let single = batch_markdown(std::slice::from_ref(&one));
        assert_eq!(single.0, one.markdown, "单项不得补换行");
        assert_eq!(
            single.1,
            vec![one.markdown.find("asset://").expect("含 asset://") as u64],
            "定位点是 asset:// 前缀的偏移（不是链接起点）"
        );

        let (markdown, starts) = batch_markdown(&[one.clone(), two.clone()]);
        assert_eq!(markdown, format!("{}\n{}", one.markdown, two.markdown));
        // 定位点是 `asset://` 前缀在批内的偏移（单项/多项同一约定）。
        let first_uri = one.markdown.find("asset://").expect("含 asset://"); // 附件：`[附件：`
        let second_base = one.markdown.len() + 1;
        let second_uri = two.markdown.find("asset://").expect("含 asset://");
        assert_eq!(
            starts,
            vec![first_uri as u64, (second_base + second_uri) as u64]
        );
        for (index, start) in starts.iter().enumerate() {
            let tail = &markdown[*start as usize..];
            assert!(
                tail.starts_with("asset://"),
                "第 {index} 项的定位点必须落在 asset:// 上：{tail:?}"
            );
        }
        assert_eq!(
            &markdown[second_base..],
            two.markdown,
            "偏移必须能在 batch markdown 里切出第二项"
        );
    }

    #[test]
    fn image_sniffing_and_dimensions_reject_corrupt_bytes() {
        // 1×1 PNG（真实字节，走解码器只读头部）。
        let png = {
            let buffer = image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 255]));
            let mut out = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(buffer)
                .write_to(&mut out, image::ImageFormat::Png)
                .expect("编码测试 PNG");
            out.into_inner()
        };
        assert_eq!(ImportImageFormat::sniff(&png), Some(ImportImageFormat::Png));
        let staged = stage_image(ImportImageFormat::Png, &png).expect("合法 PNG 必须通过");
        assert_eq!(staged.file_name, "粘贴图片.png");
        assert_eq!(staged.media_type, "image/png");
        assert_eq!(staged.data, png);
        assert!(staged.markdown.starts_with("![粘贴图片.png](asset://"));

        // 声明成 JPEG 但字节是 PNG：以字节为准（不误判损坏）。
        let staged = stage_image(ImportImageFormat::Jpeg, &png).expect("以真实字节为准");
        assert_eq!(staged.file_name, "粘贴图片.png");

        // 损坏字节 / 空字节：都在写正文之前失败。
        assert!(matches!(
            stage_image(ImportImageFormat::Png, b"not a png"),
            Err(AttachmentError::CorruptImage(_))
        ));
        assert_eq!(
            stage_image(ImportImageFormat::Png, b"").unwrap_err(),
            AttachmentError::EmptyFile
        );
        // 截断的 PNG 头（有魔数、尺寸读不出来）。
        assert!(matches!(
            stage_image(ImportImageFormat::Png, &png[..20]),
            Err(AttachmentError::CorruptImage(_))
        ));
    }

    #[test]
    fn svg_bytes_are_stored_as_a_plain_attachment() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
        let staged = stage_binary("svg", svg).expect("SVG 按普通附件");
        assert_eq!(staged.file_name, "粘贴图片.svg");
        assert_eq!(staged.media_type, "image/svg+xml");
        assert!(
            staged.markdown.starts_with("[附件：粘贴图片.svg](asset://"),
            "SVG 不得写成图片语法：{}",
            staged.markdown
        );
        // 扩展名清洗：路径注入进不了文件名。
        let staged = stage_binary("../x", b"x").unwrap();
        assert_eq!(staged.file_name, "粘贴图片.x");
        let staged = stage_binary("", b"x").unwrap();
        assert_eq!(staged.file_name, "粘贴图片.bin");
    }

    #[test]
    fn windows_clipboard_bmp_is_converted_to_png_image_asset() {
        let image = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        let mut bmp = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bmp, image::ImageFormat::Bmp)
            .expect("生成 BMP 失败");
        let staged = stage_image(ImportImageFormat::Bmp, &bmp.into_inner())
            .expect("BMP 剪贴板必须转成图片资产");
        assert_eq!(staged.file_name, "粘贴图片.png");
        assert_eq!(staged.media_type, "image/png");
        assert!(
            staged.markdown.starts_with("![粘贴图片.png](asset://"),
            "必须写成图片语法：{}",
            staged.markdown
        );
        assert_eq!(
            ImportImageFormat::sniff(&staged.data),
            Some(ImportImageFormat::Png),
            "落库字节必须是 PNG（否则预览仍不认）"
        );
    }
}
