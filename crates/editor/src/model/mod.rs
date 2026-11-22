//! 编辑器纯模型（ADR 0003 §7 切片 3）。
//!
//! 本模块**不含 GPUI / 布局 / 平台接入**，因此可以在无窗口环境完成全部测试：
//!
//! - [`controller`]：Rope 镜像、source selection、确认 delta 的单次应用、
//!   本地编辑请求生成、grapheme 交互导航 / 删除、IME 组合；
//! - [`projection`]：S1 identity 投影，但结构按 ADR 0002 §4 预留 `Vec<Segment>`；
//! - [`input`]：临时输入文档 + preedit overlay（UTF-16 接口的相对坐标系）；
//! - [`selection`]：anchor / head 均为 `UnicodeScalar`；
//! - [`error`]：纯模型的错误类型。
//!
//! 依赖方向（ADR 0003 §3）：`editor → storage / text`，且不依赖 GPUI。

pub mod controller;
pub mod error;
pub mod input;
pub mod projection;
pub mod selection;
