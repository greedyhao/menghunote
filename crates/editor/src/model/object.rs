//! 原子 **Object Block** 抽象（ADR 0019 S2a 图片 / S3a 表格）。
//!
//! 图片和表格在编辑器里是同一类东西：**整块源码被一个 display 占位符替换**
//! （`SegmentKind::Object`），块内部没有文本 caret，只有“块前/块后”两个落点；
//! 单击只选中整块，显式「编辑源码」才显示完整 Markdown。差异只在
//! **画什么**（图片像素 / GFM 表格）和**数据来自哪里**（图片资产 / 表格块树），
//! 投影、命中、选中、源码切换、删除必须共用同一条路径。
//!
//! 因此本模块只定义“块的公共身份”：
//!
//! - [`ObjectKind`]：图片 / 表格；
//! - [`ObjectBlock`]：整块 source 区间 + 起始/结束源码行 + 在同类集合里的下标。
//!
//! 具体数据（图片的 `dest`/`alt`、表格的行列/对齐）仍然留在
//! [`crate::model::markdown::MarkdownImage`] / [`crate::model::markdown::MarkdownTable`]
//! 里，通过 [`crate::model::markdown::MarkdownDocument::object_image`] /
//! [`crate::model::markdown::MarkdownDocument::object_table`] 取回 —— 不复制一套
//! 平行逻辑，也不把两种载荷塞进同一个大 enum 里让所有调用方写 `match`。
//!
//! # “对象行”与多行对象
//!
//! [`ObjectBlock::line`] 是**对象行**（object key）：图片恒等于它所在的源码行；
//! 表格是表头行。投影的“显式源码模式”与 app 的选中/工具条都按这一个行号寻址，
//! 所以一张跨 10 行的表格也只占一个对象行。`end_line` 只用于文档/诊断
//! （例如日志里说明“这是 3..5 行的表格”）。

use menghunote_text::SourceRange;

/// 原子 Object 块的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectKind {
    /// 整行独占的图片（`![alt](dest)`，ADR 0019 S2a）。
    Image,
    /// GFM 表格块（ADR 0019 S3a）。
    Table,
}

impl ObjectKind {
    /// 是否图片对象。
    #[must_use]
    pub const fn is_image(self) -> bool {
        matches!(self, Self::Image)
    }

    /// 是否表格对象。
    #[must_use]
    pub const fn is_table(self) -> bool {
        matches!(self, Self::Table)
    }

    /// 日志/诊断用的中文名（不带尺寸等渲染信息）。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Image => "图片",
            Self::Table => "表格",
        }
    }
}

/// 一个可投影为原子 Object 的块（整块源码 ↔ 一个 display 占位符）。
///
/// **只表示“块存在 + 它占哪段源码”**：是否真的投影成对象由
/// [`crate::model::markdown::MarkdownDocument`] 的 block 判定决定；
/// 当前是否被“编辑源码”展开由 [`crate::model::controller::EditorController`] 决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectBlock {
    kind: ObjectKind,
    span: SourceRange,
    line: usize,
    end_line: usize,
    index: usize,
}

impl ObjectBlock {
    /// 由 [`crate::model::markdown::MarkdownDocument`] 在解析后构造。
    pub(crate) fn new(
        kind: ObjectKind,
        span: SourceRange,
        line: usize,
        end_line: usize,
        index: usize,
    ) -> Self {
        Self {
            kind,
            span,
            line,
            end_line: end_line.max(line),
            index,
        }
    }

    /// 对象种类。
    #[must_use]
    pub const fn kind(&self) -> ObjectKind {
        self.kind
    }

    /// 整块源码区间（图片是 `![alt](dest)`；表格是整表，不含末尾换行）。
    #[must_use]
    pub const fn span(&self) -> SourceRange {
        self.span
    }

    /// 对象行（object key）：图片所在行 / 表格表头行（0 起）。
    ///
    /// 投影的“显式源码模式”列表、app 的选中与工具条都用它寻址。
    #[must_use]
    pub const fn line(&self) -> usize {
        self.line
    }

    /// 块结束源码行（0 起，含）；图片与 `line` 相同，表格是最后一条数据行。
    #[must_use]
    pub const fn end_line(&self) -> usize {
        self.end_line
    }

    /// 在**同类**集合里的下标：图片对应 `MarkdownDocument::images()`，
    /// 表格对应 `MarkdownDocument::tables()`。取载荷时用它，不要自己再扫一遍。
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }
}
