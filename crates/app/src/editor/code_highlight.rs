//! 代码块**只读**语法高亮（`pulldown-cmark` 认块 + `syntect` 着色）。
//!
//! 本模块只做一件事：把一段代码文本按语言切成
//! `(UTF-8 字节区间, 语义类别)`，交给绘制层换成主题颜色。它**不**解析 Markdown
//! （块/语言来自 `menghunote-markdown` 的 [`CodeBlockSpan`]）、**不**改布局/编辑/
//! 选区/IME/滚动/撤销，也**不**引入 GPUI 的 TextView 或 tree-sitter。
//!
//! # 为什么用 syntect 的 scope 映射而不是 syntect 主题
//!
//! 颜色必须跟应用主题（ADR 0021）走。syntect 的 TextMate scope 是“语义名”
//! （`keyword.control.rust` / `string.quoted.double`），这里把它映射成应用自己的
//! [`CodeHighlightClass`]（keyword/string/comment/...），颜色由
//! `editor::theme::syntax_*` 从**当前**主题 token 取。
//!
//! # 边界（写死，测试钉住）
//!
//! - **未知/空/`text` 语言返回空**（降级为无色），绝不 panic；
//! - **单块输入上限 256 KiB**：超限直接返回空（不阻塞 UI；大块降级无色）；
//! - 所有 `range` 都是 `code` 内的 UTF-8 字节区间，且**一定落在 char 边界**
//!   （含中文的注释/字符串不会被切成半个字符）；
//! - 相邻同类 span 合并；返回区间有序、互不重叠；
//! - 只做**单块**输入：调用方（`EditorView`）负责块切分与 display 坐标映射。
//!
//! 依赖侧只启用纯 Rust 后端（`parsing` / `default-syntaxes` / `regex-fancy`），
//! 不拉 onig（C），不需要 `.tmTheme`/HTML 渲染。

use std::ops::Range;
use std::sync::LazyLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// 单块代码高亮输入上限（UTF-8 字节）。
///
/// 超过上限直接返回空（降级无色）：语法高亮是**可选的视觉增强**，不能因为一篇
/// 笔记里有一大块代码就阻塞绘制/输入。上限与 app 侧缓存策略无关（缓存按块内容
/// 哈希失效）。
pub const MAX_BLOCK_BYTES: usize = 256 * 1024;

/// 语义类别（颜色由主题决定，不在这里写死）。
///
/// 与 `syntect` 的 TextMate scope 一一映射（见 [`semantic_class`]）；未命中的
/// 文本在绘制时用正文色（`theme::text_color()`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeHighlightClass {
    /// 关键字（`fn` / `let` / `if` / `return` ...）。
    Keyword,
    /// 字符串字面量（含引号与转义；正则字面量也归这一类）。
    String,
    /// 注释（行/块/文档注释统一）。
    Comment,
    /// 数值字面量。
    Number,
    /// 类型/结构体/枚举/trait 等名字。
    Type,
    /// 函数/方法/宏名字。
    Function,
    /// 变量/参数/属性访问链上的变量。
    Variable,
    /// 运算符（`=` / `+` / `->` ...）。
    Operator,
    /// 标点/分隔符（括号、逗号、分号）；只有它自己的 scope 明确是 punctuation 时。
    Punctuation,
    /// 属性/注解/对象键名（`#[derive]`、`@decorator`、JSON/TOML 的 key）。
    Attribute,
}

/// 一段代码高亮结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeHighlightSpan {
    /// `code` 内的 UTF-8 字节区间（非空、在 char 边界上）。
    pub range: Range<usize>,
    /// 语义类别。
    pub class: CodeHighlightClass,
}

/// 内置语法集：解析默认语法 dump 只做一次（`LazyLock`），所有块共享。
///
/// `load_defaults_newlines` 版本要求按行（含行尾 `\n`）喂给 `ParseState`，
/// 对“最后一行没有换行”的输入也安全（见 [`highlight`]）。
static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// code fence / info 字符串里的**语言 token**（info 第一个词，小写）。
///
/// - `rust,no_run` → `rust`（逗号后的属性丢弃）；
/// - `{.rust}` → `rust`（Pandoc 风格属性）；
/// - 空/只有空白 → 空串（调用方据此跳过）。
#[must_use]
pub fn language_from_info(info: &str) -> String {
    let first = info.split_whitespace().next().unwrap_or("");
    let first = first.trim_start_matches('{').trim_end_matches('}');
    let first = first.split(',').next().unwrap_or("");
    let first = first.trim_start_matches('.');
    first.to_ascii_lowercase()
}

/// 按语言/内容产出语法高亮 span。
///
/// 未知/空/`text` 语言、空内容、超过 [`MAX_BLOCK_BYTES`] 的块都返回空 vec
/// （降级无色，不报错）。返回区间有序、互不重叠、相邻同类已合并。
#[must_use]
pub fn highlight(language: &str, code: &str) -> Vec<CodeHighlightSpan> {
    if code.is_empty() || code.len() > MAX_BLOCK_BYTES {
        return Vec::new();
    }
    let Some(syntax) = syntax_for(language) else {
        return Vec::new();
    };
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut spans: Vec<CodeHighlightSpan> = Vec::new();
    let mut offset = 0usize;
    for line in LinesWithEndings::from(code) {
        // 解析失败的单行保持进入时的 scope：不整块放弃，也不 panic。
        if let Ok(operations) = parser.parse_line(line, &SYNTAX_SET) {
            let mut cursor = 0usize;
            for (index, operation) in operations {
                push_span(&mut spans, code, offset + cursor..offset + index, &scopes);
                if scopes.apply(&operation).is_err() {
                    // scope 栈损坏（语法定义异常）：用进入该行的栈继续，宁可少颜色。
                    continue;
                }
                cursor = index;
            }
            push_span(
                &mut spans,
                code,
                offset + cursor..offset + line.len(),
                &scopes,
            );
        }
        offset += line.len();
    }
    merge_adjacent(spans)
}

/// 语言 token → syntect 语法；`None` 表示“不认这门语言，保持无色”。
///
/// syntect 的 `find_syntax_by_token` 先按扩展名、再按大小写不敏感的名字找，
/// 因此 `rust`/`python`/`json`/`js`/`sh` 这些 fence 常见写法都能命中；少数
/// 惯用别名在 [`canonical_token`] 里收口。
fn syntax_for(language: &str) -> Option<&'static SyntaxReference> {
    let token = language_from_info(language);
    if token.is_empty()
        || matches!(
            token.as_str(),
            "text" | "txt" | "plain" | "plaintext" | "none" | "no-highlight"
        )
    {
        return None;
    }
    let canonical = canonical_token(&token);
    let found = [token.as_str(), canonical]
        .into_iter()
        .find_map(|candidate| SYNTAX_SET.find_syntax_by_token(candidate))
        .filter(|syntax| syntax.name != "Plain Text");
    found
}

/// fence info 里常见的别名（syntect 自带的扩展名/名字之外）。
fn canonical_token(token: &str) -> &str {
    match token {
        "c#" | "csharp" => "cs",
        "c++" | "cxx" => "cpp",
        "objc" | "objective-c" => "m",
        "shell" | "console" | "zsh" | "bash" => "sh",
        "yml" => "yaml",
        "golang" => "go",
        "jsx" => "js",
        other => other,
    }
}

/// 把一段 scope 映射到应用类别；`None` = 该段不单独着色（继承外层/正文色）。
///
/// 规则参考 gpui-component 的 syntect backend（`semantic_name`），但落到本应用的
/// 十个类别上；**容器/定界符**（`source` / `meta` / `punctuation.definition.*`）
/// 一律返回 `None`，让外层语义决定颜色（例如字符串引号跟着字符串色走）。
fn semantic_class(scope: Scope) -> Option<CodeHighlightClass> {
    let scope = scope.build_string();
    let is = |prefix: &str| {
        scope == prefix
            || scope
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
    };

    if is("punctuation.definition.string")
        || is("punctuation.definition.comment")
        || is("invalid")
        || is("meta")
        || is("source")
        || is("text")
    {
        None
    } else if is("comment") {
        Some(CodeHighlightClass::Comment)
    } else if is("string") || is("constant.character.escape") {
        // `string.regexp` 也被 `is("string")` 覆盖；转义字符跟着字符串色走。
        Some(CodeHighlightClass::String)
    } else if is("constant.numeric") {
        Some(CodeHighlightClass::Number)
    } else if is("constant.language") {
        // `true` / `false` / `None` / `null` 更像关键字而不是数字。
        Some(CodeHighlightClass::Keyword)
    } else if is("constant.other") {
        // 字符串里的 `{}` 占位符等：继承外层（字符串）颜色。
        None
    } else if is("constant") || is("support.constant") || is("entity.name.constant") {
        Some(CodeHighlightClass::Number)
    } else if is("keyword.operator") {
        Some(CodeHighlightClass::Operator)
    } else if is("keyword") || is("storage.modifier") || is("meta.preprocessor") {
        Some(CodeHighlightClass::Keyword)
    } else if is("storage.type") {
        // `fn`/`class`/`struct` 是声明关键字，`i32`/`int` 是类型名。
        //
        // 注意不能把 "type" 放进关键字词表：`storage.type.rust`（i32/let 共用）
        // 本身就含 "type"，会让所有类型名都变成关键字色。
        if [
            "class",
            "enum",
            "function",
            "impl",
            "interface",
            "module",
            "namespace",
            "struct",
            "trait",
            "let",
            "var",
            "const",
        ]
        .iter()
        .any(|word| scope.contains(word))
        {
            Some(CodeHighlightClass::Keyword)
        } else {
            Some(CodeHighlightClass::Type)
        }
    } else if is("entity.name.function")
        || is("support.function")
        || is("variable.function")
        || is("meta.function-call")
        || is("support.macro")
        || is("entity.name.macro")
    {
        Some(CodeHighlightClass::Function)
    } else if is("entity.name.type")
        || is("entity.name.class")
        || is("entity.name.struct")
        || is("entity.name.enum")
        || is("entity.name.trait")
        || is("entity.name.interface")
        || is("entity.other.inherited-class")
        || is("support.type")
        || is("support.class")
        || is("entity.name.tag")
    {
        Some(CodeHighlightClass::Type)
    } else if is("entity.other.attribute-name")
        || is("support.type.property-name")
        || is("meta.mapping.key")
    {
        Some(CodeHighlightClass::Attribute)
    } else if is("entity.name.label") {
        Some(CodeHighlightClass::Variable)
    } else if is("entity.name.section") || is("markup.heading") {
        Some(CodeHighlightClass::Keyword)
    } else if is("variable") {
        Some(CodeHighlightClass::Variable)
    } else if is("markup.raw") || is("markup.underline.link") {
        Some(CodeHighlightClass::String)
    } else if is("punctuation") || is("markup.list") {
        Some(CodeHighlightClass::Punctuation)
    } else {
        None
    }
}

/// 取 scope 栈里**最内层**有语义的类别（字符串引号跟着字符串色走）。
fn push_span(
    spans: &mut Vec<CodeHighlightSpan>,
    code: &str,
    range: Range<usize>,
    scopes: &ScopeStack,
) {
    let Some(class) = scopes
        .scopes
        .iter()
        .rev()
        .find_map(|scope| semantic_class(*scope))
    else {
        return;
    };
    let Some(range) = clamp_to_char_boundaries(code, range) else {
        return;
    };
    match spans.last_mut() {
        Some(last) if last.class == class && last.range.end == range.start => {
            last.range.end = range.end;
        }
        _ => spans.push(CodeHighlightSpan { range, class }),
    }
}

/// 把区间裁剪到 `code` 的合法 char 边界；空区间返回 `None`。
///
/// syntect 的索引理论上都在 char 边界（它按 `&str` 匹配），这里仍显式兜底：
/// 高亮区间只要切坏一个多字节字符，GPUI 的 shaping 就会 panic 或画出乱码。
fn clamp_to_char_boundaries(code: &str, range: Range<usize>) -> Option<Range<usize>> {
    let mut start = range.start.min(code.len());
    let mut end = range.end.min(code.len());
    while start < end && !code.is_char_boundary(start) {
        start += 1;
    }
    while end > start && !code.is_char_boundary(end) {
        end -= 1;
    }
    (start < end).then_some(start..end)
}

/// 合并相邻同类（[`push_span`] 已按顺序合并；这里兜底处理跨行/被 `None` 打断后
/// 仍相邻的同类段），并保证有序、互不重叠。
fn merge_adjacent(mut spans: Vec<CodeHighlightSpan>) -> Vec<CodeHighlightSpan> {
    let mut merged: Vec<CodeHighlightSpan> = Vec::with_capacity(spans.len());
    for span in spans.drain(..) {
        match merged.last_mut() {
            Some(last) if last.class == span.class && last.range.end == span.range.start => {
                last.range.end = span.range.end;
            }
            _ => merged.push(span),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::{highlight, language_from_info, CodeHighlightClass as Class, MAX_BLOCK_BYTES};

    fn classes(code: &str, language: &str) -> Vec<Class> {
        highlight(language, code)
            .into_iter()
            .map(|span| span.class)
            .collect()
    }

    fn slice<'a>(code: &'a str, span: &super::CodeHighlightSpan) -> &'a str {
        &code[span.range.clone()]
    }

    #[test]
    fn rust_hits_keyword_string_comment_number_and_function() {
        let code = "fn main() {\n    let x: i32 = 42; // 注释\n    let s = \"hello\";\n}\n";
        let spans = highlight("rust", code);
        let found = classes(code, "rust");
        for expected in [
            Class::Keyword,
            Class::String,
            Class::Comment,
            Class::Number,
            Class::Function,
            Class::Type,
        ] {
            assert!(found.contains(&expected), "Rust 缺 {expected:?}：{found:?}");
        }
        // `fn` 是关键字、`main` 是函数名、`42` 是数字、`"hello"` 是字符串。
        assert!(spans
            .iter()
            .any(|span| span.range == (0..2) && span.class == Class::Keyword));
        assert!(spans
            .iter()
            .any(|span| slice(code, span) == "main" && span.class == Class::Function));
        assert!(spans
            .iter()
            .any(|span| slice(code, span) == "42" && span.class == Class::Number));
        assert!(spans
            .iter()
            .any(|span| { slice(code, span) == "\"hello\"" && span.class == Class::String }));
        assert!(spans
            .iter()
            .any(|span| slice(code, span).contains("注释") && span.class == Class::Comment));
    }

    #[test]
    fn python_hits_keyword_string_comment_number() {
        let code = "def f(x):\n    # 注释\n    return \"hi\" + str(42)\n";
        let found = classes(code, "python");
        for expected in [Class::Keyword, Class::String, Class::Comment, Class::Number] {
            assert!(
                found.contains(&expected),
                "Python 缺 {expected:?}：{found:?}"
            );
        }
    }

    #[test]
    fn json_hits_string_number_and_boolean() {
        let code = "{\n  \"name\": \"value\",\n  \"n\": 42,\n  \"ok\": true\n}\n";
        let spans = highlight("json", code);
        let found = classes(code, "json");
        assert!(found.contains(&Class::String), "JSON 缺 String：{found:?}");
        assert!(found.contains(&Class::Number), "JSON 缺 Number：{found:?}");
        assert!(
            found.contains(&Class::Keyword),
            "true 归关键字类：{found:?}"
        );
        assert!(spans
            .iter()
            .any(|span| slice(code, span) == "42" && span.class == Class::Number));
    }

    #[test]
    fn unknown_empty_and_text_languages_have_no_spans() {
        for language in [
            "",
            "   ",
            "text",
            "plaintext",
            "none",
            "definitely-not-a-language",
        ] {
            assert!(
                highlight(language, "fn main() {}\n").is_empty(),
                "未知语言必须无色：{language:?}"
            );
        }
        assert!(highlight("rust", "").is_empty(), "空内容返回空");
        assert!(highlight("rust", "   \n").is_empty(), "只有空白也返回空");
    }

    #[test]
    fn oversize_block_returns_empty_instead_of_blocking() {
        let code = "x".repeat(MAX_BLOCK_BYTES + 1);
        assert!(
            highlight("rust", &code).is_empty(),
            "超过 {MAX_BLOCK_BYTES} 字节必须直接降级无色"
        );
        // 上限本身仍可着色（不是 >= 就拒绝）：恰好等于上限的有效 Rust 仍有 span。
        let mut code = String::from("fn main() {}\n");
        code.push_str(&"x".repeat(MAX_BLOCK_BYTES - code.len()));
        assert_eq!(code.len(), MAX_BLOCK_BYTES, "测试输入必须恰好等于上限");
        assert!(!highlight("rust", &code).is_empty(), "上限内仍要着色");
        // 超过上限 1 字节即拒绝。
        code.push('x');
        assert!(highlight("rust", &code).is_empty());
    }

    #[test]
    fn ranges_never_split_utf8_chars_and_stay_in_bounds() {
        let code = "fn 中文函数() {\n    let s = \"汉字字符串\"; // 中文注释\n}\n";
        let spans = highlight("rust", code);
        assert!(!spans.is_empty(), "中文内容仍应有 token 命中");
        for span in &spans {
            assert!(
                code.is_char_boundary(span.range.start) && code.is_char_boundary(span.range.end),
                "span {:?} 切坏了 UTF-8：{code:?}",
                span.range
            );
            assert!(span.range.start < span.range.end);
            assert!(span.range.end <= code.len());
        }
        // 有序、不重叠。
        for pair in spans.windows(2) {
            assert!(
                pair[0].range.end <= pair[1].range.start,
                "span 必须有序不重叠：{spans:?}"
            );
        }
    }

    #[test]
    fn language_token_handles_fence_info_variants() {
        assert_eq!(language_from_info("rust"), "rust");
        assert_eq!(language_from_info("Rust"), "rust");
        assert_eq!(language_from_info("rust,no_run"), "rust");
        assert_eq!(language_from_info("{.python}"), "python");
        assert_eq!(language_from_info("  js  extra"), "js");
        assert_eq!(language_from_info(""), "");
        assert_eq!(language_from_info("   "), "");
        // info 里只有属性时得到空 token。
        assert_eq!(language_from_info("{.}"), "");
    }

    #[test]
    fn adjacent_same_class_spans_are_merged() {
        let code = "fn main() { let x = 1; }\n";
        let spans = highlight("rust", code);
        for pair in spans.windows(2) {
            assert!(
                !(pair[0].class == pair[1].class && pair[0].range.end == pair[1].range.start),
                "相邻同类必须合并：{spans:?}"
            );
        }
    }

    #[test]
    fn operators_and_punctuation_are_colored_distinctly() {
        let code = "let x = 1 + 2;\n";
        let found = classes(code, "rust");
        assert!(found.contains(&Class::Operator), "`=`/`+` 归运算符");
        assert!(found.contains(&Class::Punctuation), "`;` 归标点");
    }
}
