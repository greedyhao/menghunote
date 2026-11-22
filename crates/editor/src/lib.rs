//! menghunote 编辑器核心（**非 UI 层**）。
//!
//! 本 crate 放纯数据 / 坐标 / 会话模型 / 映射逻辑，**不依赖 GPUI**，因此可以在
//! 无窗口环境中做单位夹具、协议事件夹具与属性测试。真正的 GPUI 视图在后续的
//! `crates/app`（ADR 0003 §7 切片 5）。
//!
//! # 坐标 / 单位：只有别名，没有第二定义
//!
//! 依据 [ADR 0003](../../docs/adr/0003-module-boundaries.md) §4，坐标与单位原语的
//! **唯一实现**在 [`menghunote_text`]；本 crate 只做 `pub use` 再导出：
//!
//! - [`coords`] / [`error`] / [`range`] / [`units`] 四个模块整体再导出；
//! - [`SourceByte`] / [`UnicodeScalar`] / [`InputUtf16`] / [`DisplayByte`]、
//!   [`SourceRange`] 与唯一的 [`CoordError`] 逐个再导出。
//!
//! 因此 `menghunote_editor::SourceByte` 与 `menghunote_text::SourceByte` 是
//! **同一个类型**（不是同名副本），旧路径继续可用；任何在 editor 内重新定义
//! 坐标 / 单位 / 换算的行为都会破坏 ADR 0003 §6 第 4 条（只能别名）。
//!
//! # 纯模型（ADR 0003 §7 切片 3）
//!
//! [`model`] 是本 crate 的实现主体，不含 GPUI / 布局 / 平台：
//!
//! - [`EditorController`]：Rope 镜像 + source selection + 当前 `live_rev`；
//!   **只按确认 delta 更新一次**（`Applied.local_echo = true` 时不再按 `splice`
//!   二次应用，[`LiveDelta`] 必应用）；
//! - [`ProjectionMap`]：identity + Markdown 省略投影（真实 display ↔ source 双向映射）；
//! - [`MarkdownDocument`]：Markdown 语义扫描 + 可隐藏标记区间（投影层的输入）；
//! - [`InputDocument`]：source 在选区处替换成 preedit 的**临时**输入文档；
//!   preedit 永不进入 source，也不产生 `ScalarEdit`；
//! - grapheme 只作为交互边界（`unicode-segmentation` 在 `menghunote-text`），
//!   **不是** CRDT 索引，产生的编辑端点始终是 [`UnicodeScalar`]。
//!
//! 设计依据：[`docs/adr/0002-editor-core.md`](../../docs/adr/0002-editor-core.md)
//! §4（投影层：坐标强类型）与 §11.1 第 1 条（单位夹具）。
//!
//! # 坐标契约（实现见 `menghunote-text`）
//!
//! 四种坐标单位互不混用，均为字段私有的 Newtype：
//!
//! | 类型 | 单位 | 用途 |
//! | --- | --- | --- |
//! | [`SourceByte`] | UTF-8 bytes | 持久化真源 / 区间 op 的切分单位 |
//! | [`UnicodeScalar`] | Unicode scalar 序号 | session 协议（`ScalarEdit`）的单位 |
//! | [`InputUtf16`] | UTF-16 code units | Windows IME / 剪贴板的原生单位 |
//! | [`DisplayByte`] | 逻辑显示串的 UTF-8 bytes | 投影后的显示坐标 |
//!
//! ## 不变量
//!
//! 1. **没有 `impl From`**：四者之间只能经依赖 `&str` 的显式方法换算
//!    （如 [`SourceByte::to_scalar`]），单位混用是编译错误；
//! 2. **错误优先**：越界、非 scalar 边界、代理对中间、孤立半代理一律返回
//!    [`CoordError`]，不截断、不钳制、不四舍五入；
//! 3. **grapheme 只是交互边界**：用于光标 / 删除粒度，**不是 CRDT / 区间 op 的索引**；
//! 4. `menghunote-text` 的换算仍是 `&str` 上的线性参考实现；本 crate 的 Rope 镜像
//!    负责文档存储，两者语义必须逐项一致。
//!
//! # 示例
//!
//! ```rust
//! use menghunote_editor::{units, SourceByte, UnicodeScalar};
//!
//! // ADR 0002 §11.1 第 1 条冻结的夹具（`é` 为分解形式 e + U+0301）。
//! let text = "A\u{4E2D}\u{1F600}e\u{301}";
//! assert_eq!(units::byte_len(text), 11);
//! assert_eq!(units::scalar_len(text), 5);
//! assert_eq!(units::utf16_len(text), 6);
//! assert_eq!(units::grapheme_len(text), 4);
//!
//! // 换算必须显式带上下文；返回 Result 而不是静默纠正。
//! let scalar = SourceByte::new(4).to_scalar(text).expect("4 是 scalar 边界");
//! assert_eq!(scalar, UnicodeScalar::new(2));
//! assert!(SourceByte::new(2).to_scalar(text).is_err(), "2 落在『中』内部");
//! ```

#![forbid(unsafe_code)]

// 四个模块整体再导出：`menghunote_editor::units::scalar_len` 等旧路径继续可用。
pub use menghunote_text::{coords, error, range, units};
// 类型逐个再导出：与 text 中**同一类型**，不存在第二定义。
pub use menghunote_text::{
    CoordError, DisplayByte, InputUtf16, SourceByte, SourceRange, UnicodeScalar,
};

pub mod model;

pub use model::block::{BlockKind, BlockRun};
// ADR 0019 S1：列表/行语义由 menghunote-markdown 定义，editor 原样再导出（同一类型，
// 不是副本）；app 用它们算 gutter / hanging indent，不自己重扫 Markdown。
pub use menghunote_markdown::{
    LineInfo as MarkdownLineInfo, LineKind as MarkdownLineKind, ListBlock as MarkdownListBlock,
    ListItem as MarkdownListItem, ListLineInfo as MarkdownListLineInfo, Span as MarkdownSpan,
};
pub use model::controller::{EditorController, GraphemeStep, OutcomeEffect};
pub use model::error::EditorError;
pub use model::input::InputDocument;
pub use model::markdown::{
    ConstructKind, MarkdownConstruct, MarkdownDocument, MarkdownError, MarkdownImage,
    TASK_BOX_DISPLAY,
};
pub use model::projection::{
    Bias, BoundaryBias, ProjectionError, ProjectionMap, ProjectionPiece, Segment, SegmentKind,
    OBJECT_PLACEHOLDER, OBJECT_PLACEHOLDER_LEN,
};
pub use model::selection::Selection;
pub use model::style::{StyleKind, StyleRun};
