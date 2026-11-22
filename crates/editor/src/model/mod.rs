//! 编辑器纯模型（ADR 0003 §7 切片 3）。
//!
//! 本模块**不含 GPUI / 布局 / 平台接入**，因此可以在无窗口环境完成全部测试：
//!
//! - [`controller`]：Rope 镜像、source selection、确认 delta 的单次应用、
//!   本地编辑请求生成、grapheme 交互导航 / 删除、IME 组合；
//! - [`markdown`]：Markdown 语义扫描 + 可隐藏区间（S2 第一层，纯模型）；
//! - [`projection`]：identity + Markdown 省略投影（S2 真实 display↔source 双向映射）；
//! - [`style`]：display 坐标上的语义样式区间（Bold/Emphasis/InlineCode/Strike/Link）；
//! - [`block`]：source 坐标上的块级语义（每行一个 run：标题级别/引用/列表/代码/段落）；
//! - [`input`]：临时输入文档 + preedit overlay（UTF-16 接口的相对坐标系）；
//! - [`selection`]：anchor / head 均为 `UnicodeScalar`；
//! - [`error`]：纯模型的错误类型。
//!
//! 依赖方向（ADR 0003 §3）：`editor → storage / text`，且不依赖 GPUI。

pub mod block;
pub mod controller;
pub mod error;
pub mod input;
pub mod markdown;
pub mod projection;
pub mod selection;
pub mod style;
