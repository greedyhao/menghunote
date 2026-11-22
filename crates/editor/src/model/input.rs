//! 临时输入文档（`InputDocument`）与 preedit overlay（ADR 0002 §4 / §7）。
//!
//! 输入文档 = **在选区处把 preedit 插进 source 后的那份文本**，只用于喂平台 IME /
//! 几何查询。它是派生视图，不是真源：
//!
//! - preedit 的字节**永远不进入** [`crate::EditorController`] 的 Rope 镜像；
//! - preedit 本身**不产生** [`menghunote_storage::session::ScalarEdit`]；
//!   只有 IME commit 的文本才会走正常编辑请求；
//! - 所有 UTF-16 序号都是相对**输入文档**的，不能拿回 source 上使用
//!   （source 的 UTF-16 序号在 preedit 之后会整体错位）。

use menghunote_text::{units, InputUtf16, SourceByte, UnicodeScalar};

use crate::model::error::EditorError;
use crate::model::selection::Selection;

/// 一份临时输入文档：source 在 `selection` 处被 `preedit` 替换后的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDocument {
    text: String,
    /// preedit 在本输入文档中的 UTF-8 字节区间（**不是 source 坐标**）。
    preedit_start: usize,
    preedit_end: usize,
    scalar_len: usize,
    utf16_len: usize,
}

impl InputDocument {
    /// 没有组合时，输入文档就是 source 本身。
    #[must_use]
    pub fn from_source(source: &str) -> Self {
        Self {
            text: source.to_owned(),
            preedit_start: 0,
            preedit_end: 0,
            scalar_len: units::scalar_len(source),
            utf16_len: units::utf16_len(source),
        }
    }

    /// 在 source 的 `selection` 处叠上 preedit。
    ///
    /// `selection` 是 source 坐标；转换失败（越界 / 非 scalar 边界）返回错误，
    /// 不做截断。
    pub(crate) fn compose(
        source: &str,
        selection: Selection,
        preedit: &str,
    ) -> Result<Self, EditorError> {
        let start = selection.start().to_source_byte(source)?.get();
        let end = selection.end().to_source_byte(source)?.get();

        let mut text = String::with_capacity(source.len().saturating_add(preedit.len()));
        text.push_str(&source[..start]);
        text.push_str(preedit);
        text.push_str(&source[end..]);

        Ok(Self {
            preedit_start: start,
            preedit_end: start + preedit.len(),
            scalar_len: units::scalar_len(&text),
            utf16_len: units::utf16_len(&text),
            text,
        })
    }

    /// 输入文档全文（含 preedit）。
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// preedit 文本（没有组合时为空串）。
    #[must_use]
    pub fn preedit(&self) -> &str {
        &self.text[self.preedit_start..self.preedit_end]
    }

    /// 输入文档的 UTF-8 字节数。
    #[must_use]
    pub const fn byte_len(&self) -> usize {
        self.text.len()
    }

    /// 输入文档的 scalar 数（**输入文档**的计数，不是 source）。
    #[must_use]
    pub const fn scalar_len(&self) -> usize {
        self.scalar_len
    }

    /// 输入文档的 UTF-16 code unit 数。
    #[must_use]
    pub const fn utf16_len(&self) -> usize {
        self.utf16_len
    }

    /// preedit 在输入文档中的 UTF-8 字节区间（半开）。
    ///
    /// 返回值只是**输入文档**的字节下标，用于切 [`Self::text`]；它不是
    /// `SourceByte`，不能传给任何 source 侧 API。
    #[must_use]
    pub const fn preedit_byte_range(&self) -> (usize, usize) {
        (self.preedit_start, self.preedit_end)
    }

    /// preedit 在输入文档中的 UTF-16 区间（IME 组合范围用）。
    pub fn preedit_utf16_range(
        &self,
    ) -> Result<(InputUtf16, InputUtf16), menghunote_text::CoordError> {
        let start = SourceByte::new(self.preedit_start).to_utf16(&self.text)?;
        let end = start.get() + units::utf16_len(self.preedit());
        Ok((start, InputUtf16::new(end)))
    }

    /// 把输入文档内的 UTF-16 序号换成**输入文档**内的 UTF-8 字节偏移。
    ///
    /// 返回的整数只用于切 [`Self::text`]；不要在 source 上复用（preedit 之后会错位）。
    /// 序号落在代理对中间或越界时返回 [`menghunote_text::CoordError`]。
    pub fn utf16_to_byte_offset(
        &self,
        at: InputUtf16,
    ) -> Result<usize, menghunote_text::CoordError> {
        at.to_source_byte(&self.text).map(SourceByte::get)
    }

    /// 在输入文档内把 UTF-16 序号换成 scalar 序号。
    ///
    /// 返回的是**输入文档**的 scalar 序号，同样不是 source 坐标。
    pub fn utf16_to_scalar_index(
        &self,
        at: InputUtf16,
    ) -> Result<usize, menghunote_text::CoordError> {
        let byte = at.to_source_byte(&self.text)?;
        byte.to_scalar(&self.text).map(UnicodeScalar::get)
    }
}
