//! Mermaid 渲染底座（M1）：源码 → SVG 字符串。
//!
//! 本模块只提供**纯渲染**能力，不接编辑器、不做 fenced code 识别/缓存/预览/主题，
//! 也不开后台线程或子进程。M2（编辑器里的 Object Block、源码切换、缓存）后续再接线；
//! 在那之前调用方只有 [`render_svg`] 一个入口。
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

use std::any::Any;

use merman::svg::RenderResourcePolicy;
use merman::{OperationControl, RenderRequest, Renderer, SvgEnvironment, SvgRequest};

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
            ..SvgRequest::default()
        },
    );

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
