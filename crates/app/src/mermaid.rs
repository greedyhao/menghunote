//! Mermaid 渲染底座（M1）：源码 → SVG 字符串。
//!
//! 本模块只提供**纯渲染**能力，不接编辑器、不做 fenced code 识别/缓存/预览，
//! 也不开后台线程或子进程：M2 的编辑器 Object Block 用 [`render_svg_for_preview`]，
//! 缓存/线程在 `editor::mermaid_preview`；M3a 起预览入口接收当前 [`ThemeId`]。
//!
//! ## 依赖与特性（为什么是这些）
//!
//! [`merman`] `=0.10.0`，`default-features = false` +
//! `features = ["all-diagrams", "complete-svg"]`：
//!
//! - `all-diagrams`：解析并渲染 Mermaid 12.3 的全部内置图族（M1 不做按图族裁剪）；
//! - `complete-svg` = `svg` + `layout-cytoscape`：要 SVG 输出，但**不启用**默认特性里的
//!   `layout-elk`（ELK 是 EPL-2.0 组件，体积也大）。本切片不提供 ELK 布局能力，
//!   显式要求 ELK 的图不保证渲染结果（M1 只保证 Cytoscape/内置布局的图）。
//! - 无 Node / Chromium / WebView / 远程渲染；文本度量用 merman 的确定性测量器
//!   （`SvgEnvironment::deterministic()`），不读系统字体，同一输入在同一版本上输出稳定。
//!
//! ## 资源上限与失败回退
//!
//! | 边界 | 上限 | 超限行为 |
//! | --- | --- | --- |
//! | 源码 | [`MAX_MERMAID_SOURCE_BYTES`]（256 KiB，UTF-8 字节） | 交给渲染器前直接 [`MermaidError::TooLarge`] |
//! | 输出 SVG | [`MAX_MERMAID_SVG_BYTES`]（8 MiB，UTF-8 字节） | 渲染器内部先按同一上限拒绝；返回后再复核一次 |
//!
//! 失败（空输入、非图源码、语法错误、超限、渲染器 panic、非 SVG 输出）一律返回
//! [`MermaidError`]，**不 panic、不返回半成品**。调用方的回退语义固定为“显示源码”：
//! 图形渲染失败不影响 Markdown 真源，也不写回正文。
//!
//! 渲染器本身用 `catch_unwind` 包住（第三方解析/布局代码对恶意输入仍可能 panic）；
//! 这里不安装全局 panic hook，也不把 panic 文案拼进 SVG。M1 不做超时/子进程隔离
//! （见 ADR 0019 §15 的后续切片）；输入/输出上限与 merman 自带的布局工作预算
//! 共同限制最坏情况。
//!
//! ## 输出校验
//!
//! 只接受**看起来是独立 SVG 文档**的输出：去掉 BOM/前导空白后必须以 `<svg` 根元素
//! 开头，且包含 `</svg>`。其它输出按 [`MermaidError::InvalidSvg`] 拒绝，避免把
//! 异常字符串（HTML/错误页/空串）当成图片交给上层。
//!
//! 本模块只做形状校验，不做内容加工：不执行 HTML、不内联脚本、不改写 SVG 内容；
//! SVG 里的脚本/外链由后续的显示层与导出层决定（M3）。
//!
//! ## 预览主题（M3a）
//!
//! [`render_svg_for_preview`] 用 merman 的**内置主题编译器**给整条渲染管线绑定
//! 一个编译好的 [`DiagramTheme`]（`RenderRequest::with_theme`），不再依赖 Mermaid
//! 默认调色板：
//!
//! | app 主题 | merman preset | 作用 |
//! | --- | --- | --- |
//! | [`ThemeId::LIGHT`] | `ThemePreset::EditorLight` | 浅色画布/深色文字，接近原默认外观 |
//! | [`ThemeId::DARK`] | `ThemePreset::EditorDark` | 深色画布/浅色文字 |
//!
//! 主题按 preset 编译一次并缓存（`DiagramTheme` 内部是 `Arc`，clone 只加引用），
//! 编译失败按 [`MermaidError::Render`] 回退（调用方仍显示源码）。
//!
//! ### CJK 字体回退
//!
//! merman 的确定性文本度量不读系统字体，CJK 标签的**排版**是确定性的；真正画字
//! 由显示层（GPUI `SvgRenderer`/`usvg`）按 `font-family` 链解析。内置 preset 的
//! 字体栈是 `Inter, ui-sans-serif, system-ui, sans-serif`，在部分 Windows 字体配置下
//! `Inter` 缺失、`system-ui` 不含 CJK 时会出现方框/缺字。因此编译预览主题前把
//! [`CJK_FONT_FALLBACKS`] 插到通用族 `sans-serif` 之前（**只改 SVG 的字体栈声明，
//! 不引入字体文件/新依赖**）；Windows 真机的字体回退观感仍需人工确认。
//!
//! 浅色输出与 M1/M2 的“无主题”输出不是字节级相同（默认调色板 → EditorLight），
//! 这是有意的：单元测试钉住浅/深色画布与文字对比度，避免暗色不可读的回归。

use std::any::Any;
use std::sync::OnceLock;

use merman::diagram_theme::{
    DiagramTheme, DiagramThemeCompiler, ThemeDesignTokenValueV1, ThemePreset, ThemeRecipeV1,
    ThemeValueV1,
};
use merman::svg::{RenderResourcePolicy, SvgPipeline};
use merman::{OperationControl, RenderRequest, Renderer, SvgEnvironment, SvgRequest};

use crate::theme::ThemeId;

/// Mermaid 源码的字节上限（256 KiB）。
///
/// 单位是 **UTF-8 字节**，不是字符数；超限在进入渲染器之前拒绝。
pub const MAX_MERMAID_SOURCE_BYTES: usize = 256 * 1024;

/// 渲染出的 SVG 的字节上限（8 MiB）。
///
/// 这个上限同时写进 merman 的资源策略（`max_svg_bytes`）与返回值的复核；
/// 正常笔记里的图远小于它，超限只可能是病态输入。
pub const MAX_MERMAID_SVG_BYTES: usize = 8 * 1024 * 1024;

/// merman 里 “序列化后的 SVG 字节数” 上限的稳定 id。
const MAX_SVG_BYTES_LIMIT_ID: &str = "max_svg_bytes";

/// CJK 字体回退族（插到通用族 `sans-serif` 之前）。
///
/// 只写进 SVG 的 `font-family` 栈，**不携带**任何字体文件/依赖：字体是否可用由显示层
/// （GPUI `SvgRenderer`/`usvg` 的系统字体库）决定，缺失时仍回退到系统默认字体。
/// Windows 用微软雅黑；Linux/CI 用 Noto Sans CJK SC（发行版常装）。
pub const CJK_FONT_FALLBACKS: [&str; 2] = ["Microsoft YaHei", "Noto Sans CJK SC"];

/// 浅色预览主题（`ThemePreset::EditorLight`，编译一次后缓存）。
static LIGHT_PREVIEW_THEME: OnceLock<Result<DiagramTheme, String>> = OnceLock::new();

/// 深色预览主题（`ThemePreset::EditorDark`，编译一次后缓存）。
static DARK_PREVIEW_THEME: OnceLock<Result<DiagramTheme, String>> = OnceLock::new();

/// 超限的是哪一侧的预算。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MermaidLimit {
    /// 源码字节数（[`MAX_MERMAID_SOURCE_BYTES`]）。
    Source,
    /// 输出 SVG 字节数（[`MAX_MERMAID_SVG_BYTES`]）。
    Svg,
}

/// [`render_svg`] 的失败原因。
///
/// 所有失败都是**可回退**的：调用方显示原始 fenced code 即可，不修改正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MermaidError {
    /// 源码为空或只有空白。
    Empty,
    /// 源码或输出超过预算。
    TooLarge {
        /// 超限的是源码还是 SVG。
        what: MermaidLimit,
        /// 实际字节数。
        actual: usize,
        /// 上限字节数。
        limit: usize,
    },
    /// 渲染器报告解析/布局/输出错误。
    ///
    /// 文案来自 merman 的结构化错误，**只用于诊断展示**，不参与控制流。
    Render(String),
    /// 渲染器 panic 已被捕获（不让上层进程崩溃）。
    Panic(String),
    /// 渲染器返回的不是独立 SVG 文档。
    InvalidSvg(String),
}

impl std::fmt::Display for MermaidError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("Mermaid 源码为空"),
            Self::TooLarge {
                what,
                actual,
                limit,
            } => match what {
                MermaidLimit::Source => write!(
                    formatter,
                    "Mermaid 源码 {actual} 字节，超过 {} KiB 上限",
                    limit / 1024
                ),
                MermaidLimit::Svg => write!(
                    formatter,
                    "Mermaid 渲染出的 SVG {actual} 字节，超过 {} MiB 上限",
                    limit / (1024 * 1024)
                ),
            },
            Self::Render(reason) => write!(formatter, "Mermaid 渲染失败：{reason}"),
            Self::Panic(reason) => write!(formatter, "Mermaid 渲染器 panic（已捕获）：{reason}"),
            Self::InvalidSvg(reason) => {
                write!(formatter, "Mermaid 渲染器返回的不是 SVG：{reason}")
            }
        }
    }
}

impl std::error::Error for MermaidError {}

/// 把 Mermaid 源码渲染成独立 SVG 字符串。
///
/// 同步、无全局状态：每次调用都新建渲染器，同一输入在同一 crate 版本上输出稳定，
/// 可以在当前线程或后台线程调用（M1 不替调用方选择线程）。
///
/// # 错误
///
/// 空/纯空白（[`MermaidError::Empty`]）、源码或输出超限（[`MermaidError::TooLarge`]）、
/// 不是图或语法/布局错误（[`MermaidError::Render`]）、
/// 渲染器 panic（[`MermaidError::Panic`]）、非 SVG 输出（[`MermaidError::InvalidSvg`]）。
/// 这些失败都不 panic，也不返回部分结果。
///
/// # 示例
///
/// ```text
/// let svg = menghunote_app::mermaid::render_svg("flowchart TD\nA-->B")
///     .expect("简单流程图必须能渲染");
/// assert!(svg.starts_with("<svg"));
/// ```
///
/// 上面这段不编成 doctest：本机 app crate 的 doctest 链接缺 `zed_font_kit` rlib
/// （环境限制），同一路径已由模块内 `flowchart_renders_svg` 单测覆盖。
pub fn render_svg(source: &str) -> Result<String, MermaidError> {
    render_svg_with_pipeline(source, None, None)
}

/// 预览用渲染：在 [`render_svg`] 之上启用 merman 的 `ResvgSafe` SVG 管线并绑定主题。
///
/// Mermaid 12 默认把标签放在 `<foreignObject>` 里的 HTML（浏览器能画，`usvg`/`resvg`
/// 会直接忽略），所以 **M2 的编辑器预览**不能直接拿 Parity SVG 去光栅化。`ResvgSafe`
/// 会把 HTML 标签投影成等价的 SVG `<text>`，并清掉已知的 rasterization hazard；
/// 其余行为与 [`render_svg`] 完全一致（同样的输入/输出上限、`catch_unwind`、SVG
/// 形状校验与确定性环境）。
///
/// `theme` 是**当前 app 主题**（ADR 0021）：浅色 → [`ThemePreset::EditorLight`]，
/// 深色 → [`ThemePreset::EditorDark`]。主题走 `RenderRequest::with_theme`，对整条
/// 解析→布局→SVG 管线生效；主题编译/绑定失败按 [`MermaidError::Render`] 回退。
/// 编译好的预设会按主题缓存（模块内 `LIGHT_PREVIEW_THEME` / `DARK_PREVIEW_THEME`），
/// 字体栈额外插入 [`CJK_FONT_FALLBACKS`] 以改善 CJK 标签的显示层回退。
///
/// [`ThemePreset::EditorLight`]: merman::diagram_theme::ThemePreset::EditorLight
/// [`ThemePreset::EditorDark`]: merman::diagram_theme::ThemePreset::EditorDark
pub fn render_svg_for_preview(source: &str, theme: ThemeId) -> Result<String, MermaidError> {
    render_svg_with_pipeline(source, Some(SvgPipeline::resvg_safe()), Some(theme))
}

/// [`render_svg`] / [`render_svg_for_preview`] 的公共实现。
fn render_svg_with_pipeline(
    source: &str,
    pipeline: Option<SvgPipeline>,
    theme: Option<ThemeId>,
) -> Result<String, MermaidError> {
    if source.trim().is_empty() {
        return Err(MermaidError::Empty);
    }
    if source.len() > MAX_MERMAID_SOURCE_BYTES {
        return Err(MermaidError::TooLarge {
            what: MermaidLimit::Source,
            actual: source.len(),
            limit: MAX_MERMAID_SOURCE_BYTES,
        });
    }

    let environment = svg_environment()?;
    let request = RenderRequest::svg(
        source,
        OperationControl::new(),
        SvgRequest {
            environment,
            pipeline,
            ..SvgRequest::default()
        },
    );
    let request = match theme {
        Some(theme) => request.with_theme(preview_diagram_theme(theme)?),
        None => request,
    };

    // 解析/布局/序列化都是第三方代码；恶意或病态输入仍可能触发 panic，
    // 这里统一兜住，避免把编辑器进程带走。
    let rendered = catch_render_panic(|| Renderer::new().render_svg_data(request))?;
    match rendered {
        Ok(Some(svg)) => validate_svg(svg),
        Ok(None) => Err(MermaidError::Render(
            "渲染器没有产生图形输出（源码里没有可识别的 Mermaid 图）".to_owned(),
        )),
        Err(error) => Err(MermaidError::Render(error.to_string())),
    }
}

/// 取（并缓存）预览主题：浅色 → `EditorLight`，深色 → `EditorDark`。
///
/// 缓存的是 `Result`：主题编译是确定性的，失败后不必每次重试。
fn preview_diagram_theme(theme: ThemeId) -> Result<DiagramTheme, MermaidError> {
    let (slot, preset) = if theme == ThemeId::DARK {
        (&DARK_PREVIEW_THEME, ThemePreset::EditorDark)
    } else {
        (&LIGHT_PREVIEW_THEME, ThemePreset::EditorLight)
    };
    match slot.get_or_init(|| compile_preview_theme(preset)) {
        Ok(theme) => Ok(theme.clone()),
        Err(reason) => Err(MermaidError::Render(format!(
            "无法编译 Mermaid 预览主题 {}：{reason}",
            theme.as_str()
        ))),
    }
}

/// 编译内置 preset，并把 CJK 回退族插进字体栈。
fn compile_preview_theme(preset: ThemePreset) -> Result<DiagramTheme, String> {
    let compiler = DiagramThemeCompiler::new();
    let recipe = compiler
        .export_preset(preset)
        .map_err(|error| error.to_string())?;
    let recipe = with_cjk_font_fallback(recipe);
    compiler
        .compile_recipe(recipe)
        .map_err(|error| error.to_string())
}

/// 在 preset 的默认字体栈里补上 [`CJK_FONT_FALLBACKS`]（幂等）。
///
/// 内置 `EditorLight`/`EditorDark` 用共享 token `body-font` 保存默认字体栈，
/// `typography.default.font_stack` 只是引用它；所以改 token 就覆盖了整张图的
/// `font-family` 声明。其它形状（完整规格 / 无关 token）原样返回——不猜、不改。
fn with_cjk_font_fallback(mut recipe: ThemeRecipeV1) -> ThemeRecipeV1 {
    let ThemeRecipeV1::Design { design } = &mut recipe else {
        return recipe;
    };
    let Some(token) = design.tokens.get_mut("body-font") else {
        return recipe;
    };
    let ThemeDesignTokenValueV1::FontStack(ThemeValueV1::Literal(families)) = &mut token.value
    else {
        return recipe;
    };
    // 通用族保持在最后：先插入到 `sans-serif` 之前，没有通用族时追加。
    let insert_at = families
        .iter()
        .position(|family| family.eq_ignore_ascii_case("sans-serif"))
        .unwrap_or(families.len());
    let mut inserted = 0usize;
    for family in CJK_FONT_FALLBACKS {
        if families
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(family))
        {
            continue;
        }
        families.insert(insert_at + inserted, family.to_owned());
        inserted += 1;
    }
    recipe
}

/// 构造带输出上限的确定性 SVG 环境。
///
/// 文本度量固定为 `deterministic()`（不读系统字体、不依赖宿主环境）；
/// 输出字节上限与 [`MAX_MERMAID_SVG_BYTES`] 对齐，让渲染器在序列化阶段就拒绝。
fn svg_environment() -> Result<SvgEnvironment, MermaidError> {
    let mut resources = RenderResourcePolicy::default();
    resources
        .apply_override(MAX_SVG_BYTES_LIMIT_ID, MAX_MERMAID_SVG_BYTES)
        .map_err(|error| {
            MermaidError::Render(format!(
                "无法把 SVG 输出上限设为 {MAX_MERMAID_SVG_BYTES} 字节：{error}"
            ))
        })?;
    Ok(SvgEnvironment::deterministic().with_resource_policy(resources))
}

/// 包住渲染调用：panic 转成 [`MermaidError::Panic`]，不让进程终止。
///
/// 断言 `UnwindSafe` 是安全的：渲染器与请求都在闭包里创建，panic 后不再使用。
fn catch_render_panic<T>(operation: impl FnOnce() -> T) -> Result<T, MermaidError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
        .map_err(|payload| MermaidError::Panic(panic_message(payload.as_ref())))
}

/// 提取 panic 载荷的人类可读文案（`panic!("...")` / `&String` / 其它）。
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "非字符串 panic 载荷".to_owned()
    }
}

/// 复核输出：字节上限 + 独立 SVG 根元素。
///
/// 上限在渲染器内部已经设过一次，这里再查一次是**防御性**的：即使上游策略失效，
/// 也不把超大/非 SVG 字符串交给 UI。
fn validate_svg(svg: String) -> Result<String, MermaidError> {
    if svg.len() > MAX_MERMAID_SVG_BYTES {
        return Err(MermaidError::TooLarge {
            what: MermaidLimit::Svg,
            actual: svg.len(),
            limit: MAX_MERMAID_SVG_BYTES,
        });
    }
    if !looks_like_svg(&svg) {
        return Err(MermaidError::InvalidSvg(format!(
            "期望以 `<svg` 根元素开头且包含 `</svg>`，实际前缀是 {:?}",
            svg_prefix(&svg)
        )));
    }
    Ok(svg)
}

/// 判断字符串是否像一个独立的 SVG 文档。
///
/// 允许 UTF-8 BOM 与前导空白；`<svg` 之后必须是空白或 `>`（排除 `<svgfoo>`）。
/// 只做形状校验，不解析 XML，也不执行任何内容。
fn looks_like_svg(svg: &str) -> bool {
    let trimmed = svg.trim_start_matches('\u{feff}').trim_start();
    let Some(rest) = trimmed.strip_prefix("<svg") else {
        return false;
    };
    let root_delimited = match rest.chars().next() {
        Some('>') => true,
        Some(character) => character.is_whitespace(),
        None => false,
    };
    root_delimited && trimmed.contains("</svg>")
}

/// 只保留开头的一小段用于错误文案（不把整段 SVG 塞进错误里）。
fn svg_prefix(svg: &str) -> String {
    const PREFIX_CHARS: usize = 32;
    let mut prefix: String = svg.chars().take(PREFIX_CHARS).collect();
    if svg.chars().count() > PREFIX_CHARS {
        prefix.push('…');
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str) -> Result<String, MermaidError> {
        render_svg(source)
    }

    /// 取 `needle` 之后第一个 `fill="#rrggbb"` 的 RGB（主题回归测试辅助）。
    fn fill_after(svg: &str, needle: &str) -> (u8, u8, u8) {
        let rest = &svg[svg.find(needle).expect("定位标记") + needle.len()..];
        let start = rest.find("fill=\"#").expect("fill 颜色") + "fill=\"#".len();
        let hex = &rest[start..start + 6];
        let channel =
            |offset: usize| u8::from_str_radix(&hex[offset..offset + 2], 16).expect("十六进制颜色");
        (channel(0), channel(2), channel(4))
    }

    /// WCAG 2.x 相对亮度（sRGB）。
    fn relative_luminance((red, green, blue): (u8, u8, u8)) -> f32 {
        fn channel(value: u8) -> f32 {
            let value = f32::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
    }

    /// WCAG 对比度（1.0–21.0）。
    fn contrast_ratio(foreground: (u8, u8, u8), background: (u8, u8, u8)) -> f32 {
        let (lighter, darker) = {
            let a = relative_luminance(foreground);
            let b = relative_luminance(background);
            if a >= b {
                (a, b)
            } else {
                (b, a)
            }
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    /// SVG 里根 `font-family` 声明的完整值（到 `;` 为止）。
    fn root_font_stack(svg: &str) -> &str {
        let root = svg.find("#merman{font-family:").expect("#merman 根样式");
        let value = &svg[root + "#merman{font-family:".len()..];
        &value[..value.find(';').expect("font-family 以分号结束")]
    }

    /// 浅/深色主题必须是不同的画布与文字色，且文本对比度可读。
    #[test]
    fn preview_light_and_dark_palettes_differ_with_readable_contrast() {
        let source = "flowchart TD\n  开始[开始] --> 结束[结束]\n";
        let light = render_svg_for_preview(source, ThemeId::LIGHT).expect("浅色 SVG");
        let dark = render_svg_for_preview(source, ThemeId::DARK).expect("深色 SVG");
        assert_ne!(light, dark, "同一源码的浅/深色 SVG 必须不同");

        let light_canvas = fill_after(&light, "data-merman-theme-canvas=\"base\"");
        let dark_canvas = fill_after(&dark, "data-merman-theme-canvas=\"base\"");
        let light_text = fill_after(&light, "<text");
        let dark_text = fill_after(&dark, "<text");
        assert_ne!(light_canvas, dark_canvas, "画布颜色必须随主题变化");
        assert_ne!(light_text, dark_text, "文字颜色必须随主题变化");
        assert!(
            relative_luminance(dark_canvas) < relative_luminance(light_canvas),
            "深色画布必须比浅色画布暗：{dark_canvas:?} vs {light_canvas:?}"
        );
        assert!(
            relative_luminance(light_text) < relative_luminance(light_canvas),
            "浅色主题必须是深色文字/浅色画布"
        );
        assert!(
            relative_luminance(dark_text) > relative_luminance(dark_canvas),
            "深色主题必须是浅色文字/深色画布"
        );
        assert!(
            relative_luminance(dark_canvas) < 0.05,
            "深色画布亮度应很低：{dark_canvas:?}"
        );
        assert!(
            relative_luminance(light_canvas) > 0.9,
            "浅色画布亮度应很高：{light_canvas:?}"
        );
        // 浅色外观的回归锚点：EditorLight 的画布必须与应用浅色编辑器底色一致
        // （避免升级 merman 预设后白色画布变成别的浅色而“无故改变现有外观”）。
        let editor_bg = crate::theme::theme_for(ThemeId::LIGHT).editor_bg;
        let expected_editor_bg = (
            (editor_bg.r * 255.0).round() as u8,
            (editor_bg.g * 255.0).round() as u8,
            (editor_bg.b * 255.0).round() as u8,
        );
        assert_eq!(
            light_canvas, expected_editor_bg,
            "浅色 SVG 画布必须与应用浅色编辑器底色一致"
        );

        let light_ratio = contrast_ratio(light_text, light_canvas);
        let dark_ratio = contrast_ratio(dark_text, dark_canvas);
        assert!(
            light_ratio >= 4.5,
            "浅色文本/画布对比度 {light_ratio:.2} 低于 WCAG AA 4.5"
        );
        assert!(
            dark_ratio >= 4.5,
            "深色文本/画布对比度 {dark_ratio:.2} 低于 WCAG AA 4.5"
        );
    }

    /// 字号栈必须包含 CJK 回退族，并把通用族留在最后。
    #[test]
    fn preview_font_stack_includes_cjk_fallbacks() {
        for theme in [ThemeId::LIGHT, ThemeId::DARK] {
            let svg = render_svg_for_preview("flowchart TD\nA-->B\n", theme).expect("SVG");
            let stack = root_font_stack(&svg);
            for family in [
                "system-ui",
                "Microsoft YaHei",
                "Noto Sans CJK SC",
                "sans-serif",
            ] {
                assert!(stack.contains(family), "字体栈缺少 {family}：{stack}");
            }
            let generic = stack.rfind("sans-serif").expect("通用族");
            let noto = stack.find("Noto Sans CJK SC").expect("Noto 回退");
            let yahei = stack.find("Microsoft YaHei").expect("雅黑回退");
            assert!(
                noto < generic && yahei < generic,
                "通用族必须在最后：{stack}"
            );
        }
    }

    /// 真实中文标签经 ResvgSafe 管线后仍是 SVG `<text>`（不是 foreignObject）。
    #[test]
    fn preview_renders_cjk_labels_as_svg_text() {
        let source = "flowchart TD\n  开始[开始] --> 结束[结束]\n";
        for theme in [ThemeId::LIGHT, ThemeId::DARK] {
            let svg = render_svg_for_preview(source, theme).expect("中文标签必须渲染");
            assert!(svg.starts_with("<svg"));
            assert!(svg.contains("</svg>"));
            assert!(
                !svg.contains("<foreignObject"),
                "ResvgSafe 不得保留 foreignObject"
            );
            let text_nodes = svg.matches("<text").count();
            assert!(text_nodes >= 2, "两个中文标签都应是 SVG text：{text_nodes}");
            assert!(svg.contains("开始"), "缺少标签「开始」");
            assert!(svg.contains("结束"), "缺少标签「结束」");
        }
        // 同一主题重复渲染必须字节级稳定（缓存 key 只用主题/源码/宽度）。
        let first = render_svg_for_preview(source, ThemeId::DARK).expect("第一次");
        let second = render_svg_for_preview(source, ThemeId::DARK).expect("第二次");
        assert_eq!(first, second);
    }

    #[test]
    fn flowchart_renders_svg() {
        let svg = render("flowchart TD\nA-->B").expect("简单流程图必须渲染成功");
        assert!(svg.starts_with("<svg"), "SVG 根元素：{svg:.80}");
        assert!(svg.contains("</svg>"), "SVG 必须闭合");
        assert!(svg.len() > 200, "不应是空壳 SVG：{} 字节", svg.len());
    }

    #[test]
    fn sequence_diagram_renders_svg() {
        let svg = render("sequenceDiagram\nAlice->>Bob: Hi").expect("时序图必须渲染成功");
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("</svg>"));
        // 参与者名字应出现在文本节点里（宽松断言，避免绑定 merman 内部 id 方案）。
        assert!(svg.contains("Alice"), "时序图缺少参与者文本");
        assert!(svg.contains("Bob"), "时序图缺少参与者文本");
    }

    #[test]
    fn preview_pipeline_projects_html_labels_to_svg_text() {
        let source = "flowchart TD\nA[开始] --> B[结束]\n";
        let parity = render_svg(source).expect("Parity SVG");
        let preview = render_svg_for_preview(source, ThemeId::LIGHT).expect("ResvgSafe SVG");
        // Parity 保持 Mermaid 默认的 HTML 标签（浏览器能画，usvg 会忽略）。
        assert!(
            parity.contains("<foreignObject"),
            "Mermaid 默认标签是 foreignObject（前置条件）"
        );
        // 预览管线把它们投影成 SVG <text>，否则光栅化后没有文字。
        assert!(
            !preview.contains("<foreignObject"),
            "ResvgSafe 必须去掉 foreignObject"
        );
        assert!(preview.contains("<text"), "标签必须变成 SVG <text>");
        // 形状校验/确定性仍成立。
        assert!(preview.starts_with("<svg"));
        assert!(preview.contains("</svg>"));
        assert_eq!(
            preview,
            render_svg_for_preview(source, ThemeId::LIGHT).expect("第二次渲染"),
            "同一输入必须字节级一致"
        );
    }

    #[test]
    fn preview_pipeline_reports_the_same_failures() {
        assert_eq!(
            render_svg_for_preview("", ThemeId::LIGHT),
            Err(MermaidError::Empty)
        );
        assert!(matches!(
            render_svg_for_preview("这不是 Mermaid", ThemeId::LIGHT),
            Err(MermaidError::Render(_))
        ));
        assert!(matches!(
            render_svg_for_preview("flowchart TD\nA[未闭合", ThemeId::LIGHT),
            Err(MermaidError::Render(_))
        ));
    }

    #[test]
    fn empty_or_blank_source_is_rejected() {
        for source in ["", " ", "\n\t \r\n"] {
            assert_eq!(render(source), Err(MermaidError::Empty), "源码：{source:?}");
        }
    }

    #[test]
    fn non_diagram_source_is_rejected() {
        let error = render("这不是 Mermaid，只是一段普通文本").expect_err("非图源码必须拒绝");
        assert!(
            matches!(error, MermaidError::Render(_)),
            "非图源码应报告 Render（merman 把“没有图”报成解析错误），实际：{error:?}"
        );
    }

    #[test]
    fn invalid_syntax_is_rejected_without_panic() {
        // 截断的 flowchart：有图类型前缀，但节点/连线不完整。
        let error = render("flowchart TD\nA[未闭合").expect_err("截断的语法必须拒绝");
        assert!(
            matches!(error, MermaidError::Render(_)),
            "非法语法应走 Render，实际：{error:?}"
        );
        // 另一类：时序图声明了 loop 却没有 end。
        let error =
            render("sequenceDiagram\nAlice->>Bob: hi\nloop 每秒").expect_err("截断的块必须拒绝");
        assert!(
            matches!(error, MermaidError::Render(_)),
            "截断的块应走 Render，实际：{error:?}"
        );
    }

    #[test]
    fn oversized_source_is_rejected_before_rendering() {
        // 只比上限多 1 字节：必须在进渲染器之前拒绝，否则这类测试会非常慢。
        let mut source = String::with_capacity(MAX_MERMAID_SOURCE_BYTES + 1);
        source.push_str("flowchart TD\nA-->B\n");
        while source.len() < MAX_MERMAID_SOURCE_BYTES {
            source.push('%');
        }
        source.push('%');
        assert_eq!(source.len(), MAX_MERMAID_SOURCE_BYTES + 1);
        assert_eq!(
            render(&source),
            Err(MermaidError::TooLarge {
                what: MermaidLimit::Source,
                actual: MAX_MERMAID_SOURCE_BYTES + 1,
                limit: MAX_MERMAID_SOURCE_BYTES,
            })
        );
    }

    #[test]
    fn oversized_svg_output_is_rejected() {
        // 直接测复核层：8 MiB 输出不经过渲染器，避免测试依赖病态图。
        let mut svg = String::with_capacity(MAX_MERMAID_SVG_BYTES + 64);
        svg.push_str("<svg xmlns=\"http://www.w3.org/2000/svg\">");
        while svg.len() <= MAX_MERMAID_SVG_BYTES {
            svg.push('x');
        }
        svg.push_str("</svg>");
        let actual = svg.len();
        assert!(actual > MAX_MERMAID_SVG_BYTES);
        assert_eq!(
            validate_svg(svg),
            Err(MermaidError::TooLarge {
                what: MermaidLimit::Svg,
                actual,
                limit: MAX_MERMAID_SVG_BYTES,
            })
        );
    }

    #[test]
    fn svg_shape_validation_rejects_non_svg() {
        assert!(looks_like_svg(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"
        ));
        assert!(looks_like_svg("\u{feff}\n  <svg></svg>"));
        assert!(!looks_like_svg("<svg>"), "只有开标签不算完整文档");
        assert!(!looks_like_svg(""));
        assert!(!looks_like_svg("<html><body>error</body></html>"));
        assert!(!looks_like_svg("<svgfoo></svgfoo>"), "根元素名必须精确");
        assert!(!looks_like_svg("<svg xmlns=\"…\"></svg"), "必须闭合");
        // 错误文案只截一小段，不把整个非法输出带出去。
        let error = validate_svg("<html>".repeat(100)).expect_err("非 SVG 必须拒绝");
        match error {
            MermaidError::InvalidSvg(reason) => assert!(reason.chars().count() < 120),
            other => panic!("预期 InvalidSvg，实际 {other:?}"),
        }
    }

    #[test]
    fn repeated_renders_are_deterministic_and_stateless() {
        let source = "flowchart LR\nA[开始] --> B[结束]";
        let first = render(source).expect("第一次渲染");
        let second = render(source).expect("第二次渲染");
        assert_eq!(first, second, "同一输入必须字节级一致");

        // 中间渲染别的图，不应污染后续结果（无跨调用全局状态）。
        render("sequenceDiagram\nAlice->>Bob: Hi").expect("中间渲染");
        let third = render(source).expect("第三次渲染");
        assert_eq!(first, third);
    }

    #[test]
    fn renders_on_another_thread() {
        // M1 不替调用方开线程，但接口必须能在后台线程调用（M2 会这么做）。
        let source = "flowchart TD\nA-->B";
        let expected = render(source).expect("当前线程渲染");
        let handle = std::thread::spawn(move || render_svg(source));
        let rendered = handle
            .join()
            .expect("渲染线程不应 panic")
            .expect("后台线程渲染成功");
        assert_eq!(rendered, expected, "跨线程输出必须一致");
    }

    #[test]
    fn panics_are_caught_and_reported() {
        let error =
            catch_render_panic(|| panic!("注入的渲染器 panic")).expect_err("panic 必须转成错误");
        assert_eq!(error, MermaidError::Panic("注入的渲染器 panic".to_owned()));

        let error = catch_render_panic(|| std::panic::panic_any(42u32))
            .expect_err("非字符串 panic 也要兜住");
        assert!(
            matches!(error, MermaidError::Panic(ref message) if !message.is_empty()),
            "非字符串载荷也要有可读文案：{error:?}"
        );
    }
}
