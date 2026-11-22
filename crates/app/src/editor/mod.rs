//! 编辑器视图（ADR 0003 §7 切片 5a：第一次把编辑器接进 GPUI 窗口）。
//!
//! | 模块 | 职责 |
//! | --- | --- |
//! | [`layout`] | 软折行 + 视觉行几何（纯逻辑，可单测） |
//! | [`geometry`] | 一帧的绘制几何：视觉行 → 字形 x 偏移（命中只读这里） |
//! | [`glyph`] | 行 shaping 的唯一入口（绘制与键盘导航共用） |
//! | [`code_highlight`] | 代码块只读语法高亮（syntect scope → 应用语义类别） |
//! | [`shared`] | 视图 ↔ 元素共享的非观察状态（折行缓存 / 上一帧几何） |
//! | [`input`] | `EntityInputHandler`：IME / 文本输入（虚拟输入文档 + 一次提交） |
//! | [`clipboard`] | 粘贴/拖放的剪贴板分流（纯逻辑）+ 资产导入能力面（S2b） |
//! | [`object_image`] | 图片资产的受控解码 + LRU 缓存（S2a） |
//! | [`mermaid_preview`] | Mermaid 后台渲染/光栅化 + LRU 缓存（M2） |
//! | [`ime_map`] | 虚拟输入文档 ↔ source 的 UTF-16 换算（纯函数，重点单测） |
//! | [`link`] | 普通链接 scheme 策略 / 平台打开入口 / 标题提取（纯逻辑） |
//! | [`link_settings`] | 「未知自定义 scheme 记住」的本机偏好（不同步、不写正文） |
//! | [`element`] | 单 `Element`：绘制 + 鼠标命中 / 拖拽 / 滚轮 |
//! | [`view`] | `EditorView`：控制器 + 会话驱动接线、键盘、状态栏 |
//! | [`theme`] | 字号 / 行高 / 颜色常量 |
//!
//! 本切片只做 **S1 identity 投影**：`source_text()` 原样显示（`#` / `**` 不隐藏），
//! 不隐藏标记、不做 IME、不改写文本。

pub mod clipboard;
pub mod code_highlight;
pub mod display_edit;
pub mod element;
pub mod geometry;
pub mod glyph;
pub mod ime_map;
pub mod input;
pub mod layout;
pub mod link;
pub mod link_settings;
pub mod mermaid_preview;
pub mod object_image;
pub mod shared;
pub mod theme;
pub mod view;
