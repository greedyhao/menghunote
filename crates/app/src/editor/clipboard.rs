//! 粘贴/拖放的剪贴板分流与资产导入能力面（ADR 0019 §2 / S2b）。
//!
//! ## 这一层做什么
//!
//! 1. 把 GPUI 剪贴板条目按 **ADR 0019 §2 的优先级**收窄成"这次粘贴到底做什么"：
//!
//!    ```text
//!    本应用 Markdown/文本元数据 → 文件 → 图片 → 普通文本     （每次只选一种）
//!    ```
//!
//!    `force_text`（`Ctrl+Shift+V`）只取纯文本，**不**把 CF_HDROP 降级成绝对路径文字
//!    （那会把路径写进正文，违反"不读相对路径、不写绝对路径到 Markdown"）。
//! 2. 给编辑器一个**不依赖 storage / 文件系统**的能力面 [`AssetImportWriter`]：
//!    宿主（`AppShell`）实现"读字节 + 内容寻址落库 + 写引用"，编辑器只负责
//!    "分类剪贴板 / 一次事务插正文 / 失败回滚 / 正文提交后对账"。
//!
//! ## 纯逻辑
//!
//! 本模块**不读剪贴板、不读文件、不写库**：[`choose_paste`] 只吃 `&[ClipboardEntry]`，
//! 因此优先级、强制文本、格式分流都能用普通单测钉住（见本文件 `tests`）。

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{ClipboardEntry, ImageFormat};

/// 本应用写剪贴板时带的元数据标记（粘贴时优先按"本应用 Markdown/文本"处理）。
///
/// 用**纯文本前缀**而不是 JSON：`serde_json` 只在 `dev-sync` 构建里，剪贴板分流不该
/// 为它加依赖；也只有前缀完全匹配才算"本应用的文本"（不信任任意剪贴板元数据）。
pub const MARKDOWN_METADATA_PREFIX: &str = "menghunote/1 markdown";

/// 这个元数据是不是本应用写的 Markdown/文本标记。
#[must_use]
pub fn is_markdown_metadata(metadata: &str) -> bool {
    metadata.trim_start().starts_with(MARKDOWN_METADATA_PREFIX)
}

/// 首版支持的**可预览**位图容器格式（与 S2a 的受控解码一致；SVG 不算图片）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportImageFormat {
    /// PNG
    Png,
    /// JPEG
    Jpeg,
    /// WebP
    WebP,
    /// GIF（动画只取首帧，字节原样保留）
    Gif,
}

impl ImportImageFormat {
    /// GPUI 剪贴板格式 → 首版支持的位图格式；SVG/BMP/TIFF/ICO 返回 `None`
    /// （它们按普通附件处理）。
    #[must_use]
    pub fn from_clipboard(format: ImageFormat) -> Option<Self> {
        match format {
            ImageFormat::Png => Some(Self::Png),
            ImageFormat::Jpeg => Some(Self::Jpeg),
            ImageFormat::Webp => Some(Self::WebP),
            ImageFormat::Gif => Some(Self::Gif),
            ImageFormat::Svg | ImageFormat::Bmp | ImageFormat::Tiff | ImageFormat::Ico => None,
            // `ImageFormat` 是外部枚举（非本仓），新增变体时按"不支持"处理而不是 panic。
            _ => None,
        }
    }

    /// 从字节魔数嗅探（剪贴板声明的格式可能挂错，字节才是真源）。
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
            Some(Self::Png)
        } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Some(Self::WebP)
        } else {
            None
        }
    }

    /// 文件扩展名（不带点）。
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
            Self::Gif => "gif",
        }
    }

    /// MIME。
    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::WebP => "image/webp",
            Self::Gif => "image/gif",
        }
    }
}

/// 一次粘贴/拖放里需要**落库**的负载（每次只选一种）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportPayload {
    /// 文件/目录路径（剪贴板 CF_HDROP 或拖放）。宿主只读**普通文件**、不递归目录、
    /// 不接受相对路径；每批 ≤ 10 项 / 总输入 ≤ 100 MiB / 单文件 ≤ 20 MiB。
    Files(Vec<PathBuf>),
    /// 剪贴板位图（PNG/JPEG/WebP/GIF）：插入 `![名字](asset://hash)`，可进图片对象预览。
    Image {
        /// 剪贴板声明的格式（落库前以字节嗅探为准）。
        format: ImportImageFormat,
        /// 原始编码字节（PNG/JPEG/WebP/GIF 容器，**不转码**）。
        bytes: Vec<u8>,
    },
    /// 其它剪贴板字节（SVG / BMP / TIFF / ICO）：按**普通附件**存，不执行、不预览。
    Binary {
        /// 文件扩展名（不带点；落库前会清洗）。
        extension: String,
        /// 原始字节。
        bytes: Vec<u8>,
    },
}

impl ImportPayload {
    /// 日志/提示用的类别（不含文件名/路径）。
    #[must_use]
    pub fn category(&self) -> &'static str {
        match self {
            Self::Files(_) => "files",
            Self::Image { .. } => "image",
            Self::Binary { .. } => "binary",
        }
    }
}

/// 这次粘贴要做什么（每次只选一种）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteAction {
    /// 纯文本：编辑器自己插入（不落库）。
    Text(String),
    /// 需要落库的资产导入（`AppShell` 注入 [`AssetImportWriter`] 才能完成）。
    Import(ImportPayload),
    /// 剪贴板里没有可用内容（或强制纯文本但只有非文本条目）。
    None,
}

/// 本应用写剪贴板时优先命中的"自己的 Markdown/文本"（`metadata` 命中前缀且文本非空）。
#[must_use]
pub fn own_markdown(entries: &[ClipboardEntry]) -> Option<String> {
    entries.iter().find_map(|entry| match entry {
        ClipboardEntry::String(string) => {
            let metadata = string.metadata.as_deref()?;
            if !is_markdown_metadata(metadata) || string.text.is_empty() {
                return None;
            }
            Some(string.text.clone())
        }
        ClipboardEntry::Image(_) | ClipboardEntry::ExternalPaths(_) => None,
    })
}

/// 所有字符串条目的拼接（与 `ClipboardItem::text()` 的字符串部分一致）。
///
/// **刻意不包含** `ExternalPaths`：`ClipboardItem::text()` 会把 CF_HDROP 变成一串绝对
/// 路径，粘贴成文本就是把本机路径写进正文（ADR 0019 §2 明令禁止）。
#[must_use]
pub fn joined_text(entries: &[ClipboardEntry]) -> Option<String> {
    let mut text = String::new();
    for entry in entries {
        if let ClipboardEntry::String(string) = entry {
            text.push_str(&string.text);
        }
    }
    (!text.is_empty()).then_some(text)
}

/// 剪贴板第一个文件列表条目（CF_HDROP / 拖放）。
#[must_use]
pub fn first_paths(entries: &[ClipboardEntry]) -> Option<Vec<PathBuf>> {
    entries.iter().find_map(|entry| match entry {
        ClipboardEntry::ExternalPaths(paths) => {
            let paths = paths.paths();
            (!paths.is_empty()).then(|| paths.to_vec())
        }
        ClipboardEntry::String(_) | ClipboardEntry::Image(_) => None,
    })
}

/// 剪贴板图片条目 → 导入负载（首版位图 → `Image`，其它 → 普通附件 `Binary`）。
#[must_use]
pub fn first_image_payload(entries: &[ClipboardEntry]) -> Option<ImportPayload> {
    entries.iter().find_map(|entry| {
        let ClipboardEntry::Image(image) = entry else {
            return None;
        };
        if image.bytes.is_empty() {
            return None;
        }
        Some(match ImportImageFormat::from_clipboard(image.format) {
            Some(format) => ImportPayload::Image {
                format,
                bytes: image.bytes.clone(),
            },
            None => ImportPayload::Binary {
                extension: image.format.extension().to_owned(),
                bytes: image.bytes.clone(),
            },
        })
    })
}

/// **粘贴分流的唯一决策点**（ADR 0019 §2 的优先级）。
///
/// 1. 本应用 Markdown/文本元数据（没有就跳过）；
/// 2. 文件路径（`ExternalPaths`）；
/// 3. 图片（位图 → `![](…)`；SVG/BMP 等 → 普通附件）；
/// 4. 普通文本。
///
/// `force_text`（`Ctrl+Shift+V`）只看纯文本条目：既不看文件，也不看图片。
#[must_use]
pub fn choose_paste(entries: &[ClipboardEntry], force_text: bool) -> PasteAction {
    if let Some(text) = own_markdown(entries) {
        return PasteAction::Text(text);
    }
    if force_text {
        return joined_text(entries).map_or(PasteAction::None, PasteAction::Text);
    }
    if let Some(paths) = first_paths(entries) {
        return PasteAction::Import(ImportPayload::Files(paths));
    }
    if let Some(payload) = first_image_payload(entries) {
        return PasteAction::Import(payload);
    }
    joined_text(entries).map_or(PasteAction::None, PasteAction::Text)
}

/// 粘贴/拖放的来源（只用于日志与提示文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSource {
    /// 剪贴板粘贴（`Ctrl+V`）。
    Clipboard,
    /// 文件拖放（`ExternalPaths`）。
    Drop,
}

impl ImportSource {
    /// 中文短标签（日志/提示行用）。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Clipboard => "粘贴",
            Self::Drop => "拖放",
        }
    }
}

/// 一次导入落在正文里的引用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredRef {
    /// `asset://` 前缀在正文 source 坐标里的起点（与引用对账的扫描结果同一约定）。
    pub start_byte: u64,
    /// 内容寻址的资产哈希。
    pub asset_hash: [u8; 32],
}

/// 宿主已完成的"资产 + 引用"落库结果（正文**还没**插入）。
///
/// 编辑器拿到它以后只做两件事：把 `markdown` 作为**一个**编辑事务插入；
/// 成功 → `commit`（按正文整篇对账），失败 → `rollback`（回滚本批新增引用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredImport {
    /// 导入目标笔记（宿主按 `body_doc_id` 反查得到）。
    pub note_id: String,
    /// 写引用时用的正文 head 序号（账本字段，不参与同步裁决）。
    pub body_seq: u64,
    /// 本次插入的基址（正文 source 字节）。
    pub base_start: u64,
    /// 要插入的 Markdown（多项之间用 `\n` 分隔）。
    pub markdown: String,
    /// 资产个数。
    pub items: usize,
    /// 资产总字节数。
    pub byte_len: u64,
    /// 已写入的引用（回滚用）。
    pub refs: Vec<StoredRef>,
}

/// 引用对账的统计（[`AssetImportWriter::commit`] / [`AssetImportWriter::reconcile`]）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssetRefSummary {
    /// 对账后该笔记的引用条数。
    pub refs: usize,
    /// 为缺失元数据的引用新建的占位行数。
    pub placeholders: usize,
}

/// 一次导入的结果（`EditorView` 通过 [`super::view::EditorEvent::ImportReported`] 报给宿主）。
///
/// 失败时**正文与引用都没有改变**（宿主只写提示行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// 来源（粘贴 / 拖放）。
    pub source: ImportSource,
    /// 成功摘要或人类可读原因（不含路径/文件名）。
    pub result: Result<ImportedSummary, String>,
}

/// 一次成功导入的摘要。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportedSummary {
    /// 资产个数。
    pub items: usize,
    /// 资产总字节数。
    pub byte_len: u64,
    /// 对账后的引用条数。
    pub refs: usize,
}

/// 编辑器需要的**唯一**落库能力面（`AppShell` 用真实 `Vault` 实现）。
///
/// 实现必须是 `Send + Sync`（与 S2a 的 `AssetBytesProvider` 一致；编辑器只持有 `Arc`）。
/// 语义约定：
///
/// 1. [`stage_and_store`](AssetImportWriter::stage_and_store) 先做**全部**校验与读取，
///    再把资产写进内容寻址的 `assets`、按 `base + 相对偏移` 写 `asset_refs`；
///    **不碰正文**。整批失败不留正文/引用（资产按内容寻址可保留）；
/// 2. 调用方随后把 [`StoredImport::markdown`] 作为**一个**编辑事务插入；
/// 3. 正文插入失败 → [`rollback`](AssetImportWriter::rollback) 回滚本批新增引用；
///    正文插入成功 → [`commit`](AssetImportWriter::commit) 按**当前正文**整篇重建引用；
/// 4. [`reconcile`](AssetImportWriter::reconcile) 只对账（打开笔记、正文 durable 后）。
pub trait AssetImportWriter: Send + Sync + 'static {
    /// 读取 + 校验 + 落资产 + 写引用（**不碰正文**）。
    fn stage_and_store(
        &self,
        doc_id: &str,
        body_seq: u64,
        base_start: u64,
        payload: &ImportPayload,
    ) -> Result<StoredImport, String>;

    /// 正文插入成功：按 `source` 整篇重建该笔记的引用（对账）。
    fn commit(&self, stored: &StoredImport, source: &str) -> Result<AssetRefSummary, String>;

    /// 正文插入失败：回滚本批新增引用（资产保留）。
    fn rollback(&self, stored: &StoredImport);

    /// 只对账：`None` = 该文档没有对应笔记（正常跳过）。
    fn reconcile(
        &self,
        doc_id: &str,
        body_seq: u64,
        source: &str,
    ) -> Result<Option<AssetRefSummary>, String>;
}

/// 共享的导入实现（注入 `EditorView`）。
pub type SharedImportWriter = Arc<dyn AssetImportWriter>;

/// 恒失败的实现：没有 vault 的测试 / 默认构建占位（粘贴文件/图片时给出明确原因）。
#[derive(Debug, Clone, Copy, Default)]
pub struct NoImportWriter;

impl AssetImportWriter for NoImportWriter {
    fn stage_and_store(
        &self,
        _doc_id: &str,
        _body_seq: u64,
        _base_start: u64,
        _payload: &ImportPayload,
    ) -> Result<StoredImport, String> {
        Err("当前会话没有可用的资产库，无法导入附件".to_owned())
    }

    fn commit(&self, _stored: &StoredImport, _source: &str) -> Result<AssetRefSummary, String> {
        Ok(AssetRefSummary::default())
    }

    fn rollback(&self, _stored: &StoredImport) {}

    fn reconcile(
        &self,
        _doc_id: &str,
        _body_seq: u64,
        _source: &str,
    ) -> Result<Option<AssetRefSummary>, String> {
        Ok(None)
    }
}

/// 没有落库能力的共享句柄。
#[must_use]
pub fn no_import_writer() -> SharedImportWriter {
    Arc::new(NoImportWriter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{ClipboardItem, ClipboardString, ExternalPaths, Image};
    use std::path::Path;

    fn paths(paths: &[&str]) -> ExternalPaths {
        ExternalPaths(
            paths
                .iter()
                .map(Path::new)
                .map(|p| p.to_path_buf())
                .collect(),
        )
    }

    fn image(format: ImageFormat, bytes: &[u8]) -> Image {
        Image {
            format,
            bytes: bytes.to_vec(),
            id: 7,
        }
    }

    #[test]
    fn priority_is_files_then_image_then_text() {
        // 同时有文件、图片、文本：**文件优先**（ADR 0019 §2）。
        let entries = vec![
            ClipboardEntry::String(ClipboardString::new("hello".to_owned())),
            ClipboardEntry::Image(image(ImageFormat::Png, b"\x89PNG")),
            ClipboardEntry::ExternalPaths(paths(&["/tmp/a.png"])),
        ];
        assert_eq!(
            choose_paste(&entries, false),
            PasteAction::Import(ImportPayload::Files(vec![PathBuf::from("/tmp/a.png")]))
        );
        // 只有图片和文本：图片优先（浏览器里复制图片会同时给 HTML/文本）。
        let entries = vec![
            ClipboardEntry::String(ClipboardString::new("text".to_owned())),
            ClipboardEntry::Image(image(ImageFormat::Png, b"\x89PNG")),
        ];
        assert!(matches!(
            choose_paste(&entries, false),
            PasteAction::Import(ImportPayload::Image {
                format: ImportImageFormat::Png,
                ..
            })
        ));
        // 只有文本：按文本插入。
        let entries = vec![ClipboardEntry::String(ClipboardString::new(
            "普通文本".to_owned(),
        ))];
        assert_eq!(
            choose_paste(&entries, false),
            PasteAction::Text("普通文本".to_owned())
        );
        // 空剪贴板。
        assert_eq!(choose_paste(&[], false), PasteAction::None);
    }

    #[test]
    fn own_markdown_metadata_wins_over_files_and_images() {
        let entries = vec![
            ClipboardEntry::String(ClipboardString {
                text: "![x](asset://a)".to_owned(),
                metadata: Some(MARKDOWN_METADATA_PREFIX.to_owned()),
            }),
            ClipboardEntry::ExternalPaths(paths(&["/tmp/a.png"])),
        ];
        assert_eq!(
            choose_paste(&entries, false),
            PasteAction::Text("![x](asset://a)".to_owned()),
            "本应用自己的 Markdown 文本优先级最高"
        );
        // 元数据前缀不匹配 → 不算本应用文本（不信任任意元数据）。
        let entries = vec![ClipboardEntry::String(ClipboardString {
            text: "x".to_owned(),
            metadata: Some("other-app".to_owned()),
        })];
        assert_eq!(
            choose_paste(&entries, false),
            PasteAction::Text("x".to_owned())
        );
        // 前缀匹配但文本为空：跳过，按后面的条目处理。
        let entries = vec![
            ClipboardEntry::String(ClipboardString {
                text: String::new(),
                metadata: Some(MARKDOWN_METADATA_PREFIX.to_owned()),
            }),
            ClipboardEntry::String(ClipboardString::new("后来".to_owned())),
        ];
        assert_eq!(
            choose_paste(&entries, false),
            PasteAction::Text("后来".to_owned())
        );
    }

    #[test]
    fn force_text_ignores_files_and_images_and_never_writes_paths() {
        // 只有文件：Ctrl+Shift+V 不得把绝对路径当文本插进正文。
        let entries = vec![ClipboardEntry::ExternalPaths(paths(&[
            "/tmp/机密 文件.txt",
        ]))];
        assert_eq!(choose_paste(&entries, true), PasteAction::None);
        // 图片 + 文本：强制文本只取文本。
        let entries = vec![
            ClipboardEntry::Image(image(ImageFormat::Png, b"\x89PNG")),
            ClipboardEntry::String(ClipboardString::new("纯文本".to_owned())),
        ];
        assert_eq!(
            choose_paste(&entries, true),
            PasteAction::Text("纯文本".to_owned())
        );
        // 多个文本条目按顺序拼接（与 ClipboardItem::text 的字符串部分一致）。
        let entries = vec![
            ClipboardEntry::String(ClipboardString::new("a".to_owned())),
            ClipboardEntry::String(ClipboardString::new("b".to_owned())),
        ];
        assert_eq!(
            choose_paste(&entries, true),
            PasteAction::Text("ab".to_owned())
        );
        // `joined_text` 绝不回退到 ExternalPaths。
        assert_eq!(
            joined_text(&[ClipboardEntry::ExternalPaths(paths(&["/tmp/x"]))]),
            None
        );
    }

    #[test]
    fn clipboard_images_are_split_into_previewable_and_plain_attachments() {
        // 位图 → 图片负载（四种）。
        for (format, expected) in [
            (ImageFormat::Png, ImportImageFormat::Png),
            (ImageFormat::Jpeg, ImportImageFormat::Jpeg),
            (ImageFormat::Webp, ImportImageFormat::WebP),
            (ImageFormat::Gif, ImportImageFormat::Gif),
        ] {
            let entries = vec![ClipboardEntry::Image(image(format, b"bytes"))];
            match choose_paste(&entries, false) {
                PasteAction::Import(ImportPayload::Image { format, bytes }) => {
                    assert_eq!(format, expected);
                    assert_eq!(bytes, b"bytes".to_vec());
                }
                other => panic!("{format:?} 必须走图片负载：{other:?}"),
            }
        }
        // SVG → 普通附件（不执行、不栅格化）。
        let entries = vec![ClipboardEntry::Image(image(ImageFormat::Svg, b"<svg/>"))];
        assert_eq!(
            choose_paste(&entries, false),
            PasteAction::Import(ImportPayload::Binary {
                extension: "svg".to_owned(),
                bytes: b"<svg/>".to_vec(),
            })
        );
        // BMP/TIFF/ICO 同样按附件（保住字节，不做不支持的预览）。
        let entries = vec![ClipboardEntry::Image(image(ImageFormat::Bmp, b"BM"))];
        assert!(matches!(
            choose_paste(&entries, false),
            PasteAction::Import(ImportPayload::Binary { .. })
        ));
        // 空图片字节：当作没有图片，继续看文本。
        let entries = vec![
            ClipboardEntry::Image(image(ImageFormat::Png, b"")),
            ClipboardEntry::String(ClipboardString::new("t".to_owned())),
        ];
        assert_eq!(
            choose_paste(&entries, false),
            PasteAction::Text("t".to_owned())
        );
    }

    #[test]
    fn clipboard_item_entries_feed_choose_paste_directly() {
        // 与 `ClipboardItem` 的真实结构对齐（不做任何字段映射）。
        let item = ClipboardItem::new_image(&image(ImageFormat::Png, b"\x89PNG\r\n\x1a\n"));
        assert!(matches!(
            choose_paste(item.entries(), false),
            PasteAction::Import(ImportPayload::Image { .. })
        ));
        let item = ClipboardItem::new_string("hi".to_owned());
        assert_eq!(
            choose_paste(item.entries(), false),
            PasteAction::Text("hi".to_owned())
        );
    }

    #[test]
    fn image_format_helpers_are_consistent() {
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        assert_eq!(ImportImageFormat::sniff(&png), Some(ImportImageFormat::Png));
        assert_eq!(
            ImportImageFormat::sniff(&[0xff, 0xd8, 0xff, 0xe0]),
            Some(ImportImageFormat::Jpeg)
        );
        assert_eq!(
            ImportImageFormat::sniff(b"GIF89a...."),
            Some(ImportImageFormat::Gif)
        );
        assert_eq!(
            ImportImageFormat::sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some(ImportImageFormat::WebP)
        );
        assert_eq!(
            ImportImageFormat::sniff(b"RIFF\x00\x00\x00\x00WAVEfmt "),
            None
        );
        assert_eq!(ImportImageFormat::sniff(b"<svg/>"), None);
        for format in [
            ImportImageFormat::Png,
            ImportImageFormat::Jpeg,
            ImportImageFormat::WebP,
            ImportImageFormat::Gif,
        ] {
            assert!(!format.extension().is_empty());
            assert!(format.media_type().starts_with("image/"));
            assert_eq!(
                ImportImageFormat::from_clipboard(match format {
                    ImportImageFormat::Png => ImageFormat::Png,
                    ImportImageFormat::Jpeg => ImageFormat::Jpeg,
                    ImportImageFormat::WebP => ImageFormat::Webp,
                    ImportImageFormat::Gif => ImageFormat::Gif,
                }),
                Some(format)
            );
        }
        assert_eq!(ImportImageFormat::from_clipboard(ImageFormat::Svg), None);
        assert_eq!(
            ImportPayload::Files(vec![PathBuf::from("/a")]).category(),
            "files"
        );
        assert_eq!(
            ImportPayload::Image {
                format: ImportImageFormat::Png,
                bytes: Vec::new()
            }
            .category(),
            "image"
        );
        assert_eq!(ImportSource::Clipboard.label(), "粘贴");
        assert_eq!(ImportSource::Drop.label(), "拖放");
        assert_eq!(
            no_import_writer()
                .stage_and_store("doc", 0, 0, &ImportPayload::Files(Vec::new()))
                .unwrap_err(),
            "当前会话没有可用的资产库，无法导入附件"
        );
    }
}
