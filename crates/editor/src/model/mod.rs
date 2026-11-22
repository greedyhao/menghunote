//! 编辑器纯模型（ADR 0003 §7 切片 3）。
//!
//! 本模块**不含 GPUI / 布局 / 平台接入**，因此可以在无窗口环境完成全部测试：
//!
//! - [`controller`]：Rope 镜像、source selection、确认 delta 的单次应用、
//!   本地编辑请求生成、grapheme 交互导航 / 删除、IME 组合；
//! - [`markdown`]：Markdown 语义扫描 + 可隐藏区间（S2 第一层，纯模型）；
//! - [`object`]：图片/表格共用的原子 Object Block 身份（ADR 0019 S2a/S3a）；
//! - [`table_edit`]：表格「加行 / 加列 / 删除行 / 删除列 / 拖拽重排行列」的源码构造
//!   （ADR 0019 S3b / S3b5）；
//! - [`table_cell_edit`]：表格单元格就地编辑的 GFM 转义 / 单元格导航 / 删除与重排后的
//!   选中格修正纯函数（S3b2 / S3b5）；
//! - [`projection`]：identity + Markdown 省略投影（S2 真实 display↔source 双向映射）；
//! - [`style`]：display 坐标上的语义样式区间（Bold/Emphasis/InlineCode/Strike/Link）；
//! - [`block`]：source 坐标上的块级语义（每行一个 run：标题级别/引用/列表/代码/段落）；
//! - [`input`]：临时输入文档 + preedit overlay（UTF-16 接口的相对坐标系）；
//! - [`selection`]：anchor / head 均为 `UnicodeScalar`；
//! - [`error`]：纯模型的错误类型。
//!
//! 依赖方向（ADR 0003 §3）：`editor → storage / text`，且不依赖 GPUI。

pub mod backtick;
pub mod block;
pub mod controller;
pub mod error;
pub mod input;
pub mod markdown;
pub mod object;
pub mod projection;
pub mod selection;
pub mod style;
pub mod table_cell_edit;
pub mod table_edit;
