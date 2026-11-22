//! menghunote 文本原语：坐标、单位与边界校验的**唯一实现**。
//!
//! 依据 [ADR 0003](../../docs/adr/0003-module-boundaries.md) §4：`SourceByte` /
//! `UnicodeScalar` / `InputUtf16` / `DisplayByte`、`SourceRange` 与**唯一一份**
//! `CoordError` 都归属本 crate；`crates/editor`（以及后续的 `crates/storage`）
//! 只能 `pub use` 再导出或调用，**不得有第二份定义、不得有第二套通用换算**
//! （ADR 0003 §6 第 4 条：旧路径只能别名）。
//!
//! 本 crate 不依赖 GPUI、Loro、SQL 或任何协议类型，因此可以在无窗口环境中完成
//! 全部单位夹具与属性测试。依赖只有 `unicode-segmentation`（见 [`units`] 的
//! grapheme 说明）。
//!
//! # 本切片的坐标契约
//!
//! 四种坐标单位互不混用，均为字段私有的 Newtype：
//!
//! | 类型 | 单位 | 用途 |
//! | --- | --- | --- |
//! | [`SourceByte`] | UTF-8 bytes | 持久化真源 / 区间 op 的切分单位 |
//! | [`UnicodeScalar`] | Unicode scalar 序号 | CRDT 索引候选（实际单位由 ADR 0002 §11.1 实测） |
//! | [`InputUtf16`] | UTF-16 code units | Windows IME / 剪贴板的原生单位 |
//! | [`DisplayByte`] | 逻辑显示串的 UTF-8 bytes | 投影后的显示坐标（**不接 `ProjectionMap`**） |
//!
//! ## 不变量（本切片已由测试锁定）
//!
//! 1. **没有 `impl From`**：四者之间只能经依赖 `&str` 的显式方法换算
//!    （如 [`SourceByte::to_scalar`]），单位混用是编译错误；
//! 2. **错误优先**：越界、非 scalar 边界、代理对中间、孤立半代理一律返回
//!    [`CoordError`]，不截断、不钳制、不四舍五入；
//! 3. **grapheme 只是交互边界**：`unicode-segmentation` 的切分用于光标 / 删除粒度，
//!    **不是 CRDT / 区间 op 的索引**（见 [`units`] 模块文档）；
//! 4. **本切片不引入 ropey**：所有换算都是 `&str` 上的线性参考实现，后续 Rope
//!    前缀索引版本必须与之逐项等价。
//!
//! # 不在本 crate
//!
//! - `DisplayByte ↔ SourceByte` 的映射（属于 `ProjectionMap` 的多对一语义，见 ADR 0003 §4）；
//! - grapheme 的移动 / 删除策略（属于 editor 的交互策略）；
//! - Loro、SQL、协议类型、解析器、GPUI。
//!
//! # 示例
//!
//! ```rust
//! use menghunote_text::{units, SourceByte, UnicodeScalar};
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

pub mod coords;
pub mod error;
pub mod range;
pub mod units;

pub use coords::{DisplayByte, InputUtf16, SourceByte, UnicodeScalar};
pub use error::CoordError;
pub use range::SourceRange;

#[cfg(test)]
mod tests;
