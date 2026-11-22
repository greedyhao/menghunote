//! Markdown → 纯文本（检索用）。
//!
//! 只负责"把可见文字拿出来"：去掉标题/强调/行内代码/链接的 Markdown 标记，
//! 保留代码块内容、链接文本、引用与列表的可见文字。
//!
//! 这是检索的派生数据，**不是** Markdown 渲染器：
//! 不处理 HTML、表格对齐、脚注等边缘语法；遇到未知标记时宁可保留字符，
//! 也不要吞掉用户可见内容。
//!
//! 算法版本 [`MARKDOWN_TO_PLAIN_VERSION`] 进 fingerprint；改行为必须升版本，
//! 否则旧索引不会触发重建。

/// 当前正文转换算法版本。修改转换语义时 **必须** +1。
pub const MARKDOWN_TO_PLAIN_VERSION: u32 = 1;

/// 把 Markdown 源文转换为检索用纯文本。
///
/// 行为约定：
/// - 围栏代码块（``` / ~~~）内部**逐字保留**（含 `#`、`**` 等），只去掉围栏行；
/// - 标题去掉前导 `#`；引用去掉 `>`；列表去掉 `-` / `*` / `+` / `1.`；
/// - 链接 `[文本](url)` 保留 `文本`，丢弃 `url`；
/// - 行内代码去掉反引号、保留内容；`**粗体**` / `*斜体*` / `~~删除~~` 去掉标记；
/// - 空输入返回空串，不 panic。
pub fn markdown_to_plain(markdown: &str) -> String {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for raw_line in markdown.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

        if let Some((marker, len)) = fence {
            if is_fence_close(line, marker, len) {
                fence = None;
            } else {
                // 代码块内容逐字保留（包括缩进与 Markdown 标记）。
                out.push(line.to_string());
            }
            continue;
        }

        if let Some(open) = parse_fence_open(line) {
            fence = Some(open);
            continue;
        }

        let rest = strip_block_prefixes(line);
        out.push(inline_to_plain(rest));
    }

    out.join("\n")
}

/// 识别围栏开始行；返回 (标记字符, 标记长度)。
fn parse_fence_open(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|ch| *ch == marker).count();
    if len < 3 {
        return None;
    }
    // 反引号围栏的 info string 里不能再出现反引号（CommonMark）。
    if marker == '`' && trimmed[len..].contains('`') {
        return None;
    }
    Some((marker, len))
}

/// 识别围栏结束行。
fn is_fence_close(line: &str, marker: char, len: usize) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }
    let run = trimmed.chars().take_while(|ch| *ch == marker).count();
    if run < len {
        return false;
    }
    // 结束围栏只允许空白尾巴。
    trimmed[run * marker.len_utf8()..].trim().is_empty()
}

/// 去掉引用前缀与块级标记（标题/列表），返回待 inline 处理的行。
fn strip_block_prefixes(line: &str) -> &str {
    let mut s = line;

    // 引用可嵌套：">", "> >", "> > >" ...
    loop {
        let trimmed = s.trim_start_matches(' ');
        if s.len() - trimmed.len() > 3 {
            break;
        }
        let Some(after) = trimmed.strip_prefix('>') else {
            break;
        };
        s = after.strip_prefix(' ').unwrap_or(after);
    }

    let trimmed = s.trim_start_matches(' ');
    let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&hashes) {
        let after = &trimmed[hashes..];
        if after.is_empty() {
            return "";
        }
        if let Some(after_space) = after.strip_prefix(' ') {
            return strip_list_marker(after_space);
        }
    }

    strip_list_marker(s)
}

/// 去掉列表项标记；不是列表项时原样返回。
fn strip_list_marker(line: &str) -> &str {
    let trimmed = line.trim_start_matches(' ');
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'-' | b'*' | b'+') && bytes[1] == b' ' {
        return &trimmed[2..];
    }

    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits > 0
        && digits <= 9
        && bytes.get(digits) == Some(&b'.')
        && bytes.get(digits + 1) == Some(&b' ')
    {
        return &trimmed[digits + 2..];
    }

    line
}

/// 行内标记转换。
fn inline_to_plain(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < input.len() {
        let rest = &input[i..];
        let ch = rest.chars().next().expect("i 在边界上");

        match ch {
            '\\' => {
                i += 1;
                if let Some(escaped) = input[i..].chars().next() {
                    out.push(escaped);
                    i += escaped.len_utf8();
                }
            }
            '`' => {
                let run = run_len(rest, '`');
                if let Some(close) = find_marker(input, i + run, '`', run) {
                    out.push_str(&input[i + run..close]);
                    i = close + run;
                } else {
                    out.push('`');
                    i += run;
                }
            }
            '*' => {
                let run = if run_len(rest, '*') >= 2 { 2 } else { 1 };
                if let Some(close) = find_marker(input, i + run, '*', run) {
                    out.push_str(&inline_to_plain(&input[i + run..close]));
                    i = close + run;
                } else {
                    out.push('*');
                    i += 1;
                }
            }
            '_' => {
                if !starts_underscore_emphasis(input, i) {
                    out.push('_');
                    i += 1;
                    continue;
                }
                if let Some(close) = find_marker(input, i + 1, '_', 1) {
                    if ends_underscore_emphasis(input, close) {
                        out.push_str(&inline_to_plain(&input[i + 1..close]));
                        i = close + 1;
                        continue;
                    }
                }
                out.push('_');
                i += 1;
            }
            '~' if rest.starts_with("~~") => {
                if let Some(close) = find_marker(input, i + 2, '~', 2) {
                    out.push_str(&inline_to_plain(&input[i + 2..close]));
                    i = close + 2;
                } else {
                    out.push('~');
                    i += 1;
                }
            }
            '!' if rest.starts_with("![") => {
                // 图片：保留 alt 文本，丢弃 URL 与标记（alt 是最接近可见信息的文本）。
                if let Some((alt, end)) = parse_link(input, i + 1) {
                    out.push_str(&inline_to_plain(alt));
                    i = end;
                } else {
                    out.push('!');
                    i += 1;
                }
            }
            '[' => {
                if let Some((text, end)) = parse_link(input, i) {
                    out.push_str(&inline_to_plain(text));
                    i = end;
                } else {
                    out.push('[');
                    i += 1;
                }
            }
            _ => {
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }

    out
}

/// 从 `open` 处的 `[` 解析 `[text](url)`，返回 (text, 结束位置)。
fn parse_link(input: &str, open: usize) -> Option<(&str, usize)> {
    debug_assert!(input[open..].starts_with('['));
    let close = find_closing_bracket(input, open + 1)?;
    if input[close + 1..].starts_with('(') {
        let end = find_closing_paren(input, close + 2)?;
        return Some((&input[open + 1..close], end + 1));
    }
    None
}

/// 找 `]`（忽略转义）。
fn find_closing_bracket(input: &str, from: usize) -> Option<usize> {
    let mut i = from;
    while i < input.len() {
        let ch = input[i..].chars().next()?;
        if ch == '\\' {
            i += 1 + input[i + 1..].chars().next()?.len_utf8();
            continue;
        }
        if ch == ']' {
            return Some(i);
        }
        i += ch.len_utf8();
    }
    None
}

/// 找 `)`（忽略转义；URL 里的括号按第一层处理）。
fn find_closing_paren(input: &str, from: usize) -> Option<usize> {
    let mut i = from;
    while i < input.len() {
        let ch = input[i..].chars().next()?;
        if ch == '\\' {
            i += 1 + input[i + 1..].chars().next()?.len_utf8();
            continue;
        }
        if ch == ')' {
            return Some(i);
        }
        i += ch.len_utf8();
    }
    None
}

/// 找下一个指定长度的标记（忽略反斜杠转义）。
fn find_marker(input: &str, from: usize, marker: char, len: usize) -> Option<usize> {
    let needle = marker.to_string().repeat(len);
    let mut i = from;
    while i < input.len() {
        if input[i..].starts_with('\\') {
            i += 1;
            if let Some(next) = input[i..].chars().next() {
                i += next.len_utf8();
            }
            continue;
        }
        if input[i..].starts_with(&needle) {
            return Some(i);
        }
        i += input[i..].chars().next()?.len_utf8();
    }
    None
}

fn run_len(input: &str, marker: char) -> usize {
    input.chars().take_while(|ch| *ch == marker).count()
}

/// `_` 强调的左边界：前面不是字母数字，后面不是空白。
fn starts_underscore_emphasis(input: &str, i: usize) -> bool {
    let prev = input[..i].chars().next_back();
    let next = input[i + 1..].chars().next();
    !prev.is_some_and(|ch| ch.is_alphanumeric()) && next.is_some_and(|ch| !ch.is_whitespace())
}

/// `_` 强调的右边界：后面不是字母数字。
fn ends_underscore_emphasis(input: &str, close: usize) -> bool {
    input[close + 1..]
        .chars()
        .next()
        .is_none_or(|ch| !ch.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings_and_inline_markers() {
        assert_eq!(
            markdown_to_plain("# 标题\n\n**粗体** 与 `代码`"),
            "标题\n\n粗体 与 代码"
        );
    }

    #[test]
    fn links_keep_text_and_drop_url() {
        assert_eq!(
            markdown_to_plain("前 [链接](https://example.com/a?b=1) 后"),
            "前 链接 后"
        );
    }

    #[test]
    fn fenced_code_content_is_preserved() {
        let input = "```rust\n# not heading\n**not bold**\nfn main() {}\n```";
        assert_eq!(
            markdown_to_plain(input),
            "# not heading\n**not bold**\nfn main() {}"
        );
    }

    #[test]
    fn blockquotes_and_lists() {
        assert_eq!(
            markdown_to_plain("> 引用\n> > 嵌套\n- 一\n1. 二"),
            "引用\n嵌套\n一\n二"
        );
    }

    #[test]
    fn empty_input_is_empty() {
        assert_eq!(markdown_to_plain(""), "");
        assert_eq!(markdown_to_plain("\n"), "\n");
    }

    #[test]
    fn unclosed_fence_keeps_rest() {
        assert_eq!(markdown_to_plain("```\n# 原样\n"), "# 原样\n");
    }

    #[test]
    fn escaped_markers_stay_visible() {
        assert_eq!(markdown_to_plain(r"\*不是斜体\*"), "*不是斜体*");
    }

    #[test]
    fn underscores_in_words_are_not_emphasis() {
        assert_eq!(markdown_to_plain("snake_case 名字"), "snake_case 名字");
    }

    #[test]
    fn image_alt_is_kept() {
        assert_eq!(markdown_to_plain("![图注](x.png)"), "图注");
    }
}
