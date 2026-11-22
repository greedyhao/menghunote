//! 普通 Markdown 链接（`[text](dest)`）的 scheme 策略与平台打开入口。
//!
//! 本模块是**纯逻辑**（分类 / 显示脱敏 / 标题提取），可单测；真正的平台打开只有
//! [`open_external_url`] 一个入口，见下面的安全约定。
//!
//! # scheme 策略（backlog「外部链接」的结论）
//!
//! | 目标 | 行为 |
//! | --- | --- |
//! | `http` / `https` / `mailto` / `zotero` | 直接交给系统（Zotero 原样传递，不解析/改写） |
//! | `file:` / UNC / Windows 盘符路径 | 打开前**确认**（每次都要确认） |
//! | 未知自定义 scheme | 首次**确认**，可勾选「记住此 scheme」（本机偏好，不同步） |
//! | `javascript` / `data` / `vbscript` 等 | **拒绝**；错误只显示 scheme，不回显路径 |
//! | 无 scheme（`#heading` / `Note` / `Folder/Note`） | 内部链接：滚动到标题 / 打开对应笔记 |
//!
//! # 安全约定
//!
//! - **URL 不写日志**：URL 可能含 token / 本地路径 / 查询串；本模块与调用方都不得
//!   把 `dest` 打进 stderr。
//! - 确认框/错误提示只显示 `scheme[:host]`（[`display_target`]），**不回显 path/query**。
//! - 含控制字符的目标直接按危险链接拒绝（防 `java\nscript:` 之类的绕过）。
//!
//! # 平台打开
//!
//! [`open_external_url`] 交给 GPUI 的平台层：Windows `ShellExecuteW`、macOS
//! `NSWorkspace`、Linux `xdg-open`/portal。它们都是**参数数组**调用（不经
//! `sh -c`/`cmd /c`），URL 只作为单个参数传递，不会被 shell 解释。

use gpui::{App, Modifiers};

/// 普通 Markdown 链接的目标分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// 允许直接交给系统的 scheme（`http`/`https`/`mailto`/`zotero`）。
    External {
        /// 小写 scheme（不含 `:`）。
        scheme: String,
    },
    /// 本地文件（`file:` / UNC / Windows 盘符路径）：打开前必须确认。
    File {
        /// 小写 scheme（`file`）。
        scheme: String,
    },
    /// 未知自定义 scheme：首次确认，可记住（本机偏好）。
    Custom {
        /// 小写 scheme（不含 `:`）。
        scheme: String,
    },
    /// 危险 scheme：拒绝打开。
    Dangerous {
        /// 小写 scheme；控制字符用 [`CONTROL_SCHEME`] 表示。
        scheme: String,
    },
    /// 内部链接：`#heading` 或相对笔记路径。
    Internal {
        /// 相对笔记路径（`#heading` 时为空）。
        note: Option<String>,
        /// `#` 后的标题（不含 `#`；没有时为空）。
        heading: Option<String>,
    },
}

/// 含控制字符的目标在 [`LinkTarget::Dangerous`] 里的 scheme 占位。
pub const CONTROL_SCHEME: &str = "control";

/// 允许直接交给系统的 scheme（小写）。
const ALLOWED_SCHEMES: [&str; 4] = ["http", "https", "mailto", "zotero"];

/// 明确拒绝的 scheme（小写）。
const DANGEROUS_SCHEMES: [&str; 3] = ["javascript", "data", "vbscript"];

/// 把 `[text](dest)` 的 destination 分类。
///
/// 规则见模块文档；不解析/改写目标本身（Zotero 等原样保留）。
#[must_use]
pub fn classify(dest: &str) -> LinkTarget {
    let trimmed = dest.trim();
    if trimmed.is_empty() {
        return LinkTarget::Internal {
            note: None,
            heading: None,
        };
    }
    // 控制字符可能把 `javascript:` 拆成看起来无害的文本；直接拒绝。
    if trimmed.chars().any(|ch| ch.is_control()) {
        return LinkTarget::Dangerous {
            scheme: CONTROL_SCHEME.to_owned(),
        };
    }
    // `#heading`：同笔记内跳转。
    if let Some(heading) = trimmed.strip_prefix('#') {
        return LinkTarget::Internal {
            note: None,
            heading: Some(heading.to_owned()),
        };
    }
    // Windows 盘符路径（`C:\...` / `C:/...`）按本地文件处理：`C` 不是 scheme。
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return LinkTarget::File {
            scheme: "file".to_owned(),
        };
    }
    // UNC（`\\server\share` / `//server/share`）按本地文件处理。
    if trimmed.starts_with("\\\\") || trimmed.starts_with("//") {
        return LinkTarget::File {
            scheme: "file".to_owned(),
        };
    }
    if let Some(scheme) = scheme_of(trimmed) {
        let lower = scheme.to_ascii_lowercase();
        if ALLOWED_SCHEMES.contains(&lower.as_str()) {
            return LinkTarget::External { scheme: lower };
        }
        if lower == "file" {
            return LinkTarget::File { scheme: lower };
        }
        if DANGEROUS_SCHEMES.contains(&lower.as_str()) {
            return LinkTarget::Dangerous { scheme: lower };
        }
        return LinkTarget::Custom { scheme: lower };
    }
    // 无 scheme：相对笔记路径（可带 `#heading`）。
    match trimmed.split_once('#') {
        Some((note, heading)) => LinkTarget::Internal {
            note: (!note.is_empty()).then(|| note.to_owned()),
            heading: (!heading.is_empty()).then(|| heading.to_owned()),
        },
        None => LinkTarget::Internal {
            note: Some(trimmed.to_owned()),
            heading: None,
        },
    }
}

/// 取 RFC 3986 的 scheme（`^[A-Za-z][A-Za-z0-9+.-]*:` 的前缀，不含 `:`）。
///
/// 只认**开头**的 scheme；`Note: 标题` 这种带冒号的相对路径也会被当成 scheme，
/// 由 [`classify`] 落进「未知自定义 scheme」——这是有意的保守行为（先确认再打开）。
#[must_use]
fn scheme_of(dest: &str) -> Option<&str> {
    let mut chars = dest.char_indices();
    let (_, first) = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    for (index, ch) in chars {
        if ch == ':' {
            return Some(&dest[..index]);
        }
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.')) {
            return None;
        }
    }
    None
}

/// 确认框/错误提示里显示的目标：只保留 `scheme[:host]`，**绝不回显 path/query/fragment**。
///
/// `mailto:` 没有 `//`，因此只显示 `mailto:`（不显示收件人地址）；`file:` 只显示
/// `file:`（不显示本地路径）。
#[must_use]
pub fn display_target(dest: &str) -> String {
    match classify(dest) {
        LinkTarget::External { scheme }
        | LinkTarget::File { scheme }
        | LinkTarget::Custom { scheme }
        | LinkTarget::Dangerous { scheme } => {
            let host = host_of(dest);
            if host.is_empty() {
                format!("{scheme}:")
            } else {
                format!("{scheme}://{host}")
            }
        }
        LinkTarget::Internal { .. } => "内部链接".to_owned(),
    }
}

/// 危险 scheme 的提示文案（只含 scheme，不含路径）。
#[must_use]
pub fn dangerous_notice(scheme: &str) -> String {
    if scheme == CONTROL_SCHEME {
        "已拒绝不安全的链接（含控制字符）".to_owned()
    } else {
        format!("已拒绝不安全的链接（{scheme}:）")
    }
}

/// `scheme://host` 里的 host（去掉 userinfo/path/query/fragment）；没有 `//` 时返回空串。
#[must_use]
fn host_of(dest: &str) -> String {
    let Some(after_scheme) = dest.split_once(':').map(|(_, rest)| rest) else {
        return String::new();
    };
    let Some(rest) = after_scheme.strip_prefix("//") else {
        return String::new();
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();
    // userinfo（`user:pass@host`）属于敏感信息：只保留最后一个 `@` 之后。
    let host = authority.rsplit('@').next().unwrap_or_default();
    host.to_owned()
}

/// Ctrl（macOS Cmd）+ 单击才打开链接。
///
/// 普通单击只移动/落 caret；这里只判断修饰键，不改变命中规则。
#[must_use]
pub fn is_open_link_modifier(modifiers: &Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        // macOS 的「Command」在 GPUI 里是 `platform`；Ctrl 留给系统右键。
        modifiers.platform
    } else {
        modifiers.control
    }
}

/// 标题文本归一化（去掉首尾空白与开头的 `#`）。
#[must_use]
pub fn normalize_heading(heading: &str) -> String {
    heading.trim().trim_start_matches('#').trim().to_owned()
}

/// 从标题行的 source 区间里提取可比较的标题文本。
///
/// 输入是 [`menghunote_editor::BlockRun::source`] 的区间（**含行尾换行**）：
/// 去掉行尾、开头的 `#` 与空白、结尾的闭合 `#`，得到 `# 标题` → `标题`。
#[must_use]
pub fn heading_text(source: &str, range: menghunote_editor::SourceRange) -> String {
    let raw = source
        .get(range.start().get()..range.end().get())
        .unwrap_or_default();
    let line = raw.trim_end_matches(['\n', '\r']);
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
    let rest = &trimmed[hashes..];
    rest.trim_end_matches(|ch: char| ch == '#' || ch.is_whitespace())
        .trim()
        .to_owned()
}

/// 平台打开外部 URL 的**唯一入口**。
///
/// - 参数数组、不经 shell：交给 GPUI 平台层（Windows `ShellExecuteW`、macOS
///   `NSWorkspace`、Linux `xdg-open`/portal），URL 不会被 shell 解释；
/// - 只去掉首尾空白（Markdown destination 可能带空白），其余**原样**传递
///   （Zotero 等不解析/不改写）；
/// - **URL 不写日志**：这里不打任何日志，失败也不回显目标（可能含 token/本地路径）。
pub fn open_external_url(url: &str, cx: &mut App) {
    let url = url.trim();
    if url.is_empty() {
        return;
    }
    cx.open_url(url);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_schemes_are_external() {
        for dest in [
            "http://example.com/a?b=1#c",
            "https://example.com",
            "mailto:user@example.com",
            "zotero://select/library/items/ABCD1234",
            "zotero://open-pdf/library/items/ABCD1234?page=3",
            "HTTPS://EXAMPLE.COM",
        ] {
            assert!(
                matches!(classify(dest), LinkTarget::External { .. }),
                "{dest} 应直接打开"
            );
        }
    }

    #[test]
    fn zotero_is_passed_through_verbatim() {
        let dest = "zotero://select/groups/123/items/KEY?foo=bar#frag";
        assert_eq!(
            classify(dest),
            LinkTarget::External {
                scheme: "zotero".to_owned()
            }
        );
        // 分类不改写目标：调用方拿到的仍是原串。
        assert_eq!(display_target(dest), "zotero://select");
    }

    #[test]
    fn file_and_unc_need_confirmation() {
        for dest in [
            "file:///home/user/secret.txt",
            "file:relative.txt",
            r"\\server\share\file.txt",
            "//server/share/file.txt",
            r"C:\Users\alice\secret.txt",
            "C:/Users/alice/secret.txt",
        ] {
            assert!(
                matches!(classify(dest), LinkTarget::File { .. }),
                "{dest} 必须确认"
            );
        }
        assert_eq!(
            classify("file:///home/user/secret.txt"),
            LinkTarget::File {
                scheme: "file".to_owned()
            }
        );
    }

    #[test]
    fn unknown_scheme_is_custom() {
        assert_eq!(
            classify("obsidian://open?vault=x&file=y"),
            LinkTarget::Custom {
                scheme: "obsidian".to_owned()
            }
        );
        assert_eq!(
            classify("my-app+v1.2://x"),
            LinkTarget::Custom {
                scheme: "my-app+v1.2".to_owned()
            }
        );
    }

    #[test]
    fn dangerous_schemes_are_rejected() {
        for dest in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "vbscript:msgbox(1)",
            "java\nscript:alert(1)",
            "\u{0}javascript:alert(1)",
        ] {
            assert!(
                matches!(classify(dest), LinkTarget::Dangerous { .. }),
                "{dest:?} 必须拒绝"
            );
        }
        assert_eq!(
            dangerous_notice("javascript"),
            "已拒绝不安全的链接（javascript:）"
        );
        assert_eq!(
            dangerous_notice(CONTROL_SCHEME),
            "已拒绝不安全的链接（含控制字符）"
        );
    }

    #[test]
    fn display_target_never_echoes_path_or_query() {
        assert_eq!(
            display_target("https://example.com/secret/path?token=abc#frag"),
            "https://example.com"
        );
        assert_eq!(
            display_target("https://user:pass@example.com/secret"),
            "https://example.com"
        );
        assert_eq!(display_target("mailto:user@example.com"), "mailto:");
        assert_eq!(display_target("file:///home/user/secret.txt"), "file:");
        assert_eq!(display_target("obsidian://open?vault=x"), "obsidian://open");
        assert_eq!(display_target("javascript:alert(1)"), "javascript:");
        assert_eq!(display_target("#标题"), "内部链接");
        assert_eq!(display_target("Note"), "内部链接");
    }

    #[test]
    fn internal_links_split_note_and_heading() {
        assert_eq!(
            classify("#标题"),
            LinkTarget::Internal {
                note: None,
                heading: Some("标题".to_owned())
            }
        );
        assert_eq!(
            classify("Note"),
            LinkTarget::Internal {
                note: Some("Note".to_owned()),
                heading: None
            }
        );
        assert_eq!(
            classify("Folder/Note#标题"),
            LinkTarget::Internal {
                note: Some("Folder/Note".to_owned()),
                heading: Some("标题".to_owned())
            }
        );
        assert_eq!(
            classify("Note#"),
            LinkTarget::Internal {
                note: Some("Note".to_owned()),
                heading: None
            }
        );
        assert_eq!(
            classify(""),
            LinkTarget::Internal {
                note: None,
                heading: None
            }
        );
    }

    #[test]
    fn heading_text_strips_markers_and_newline() {
        let source = "# 标题\n## 二级 ##\n正文\n";
        let runs = menghunote_editor::MarkdownDocument::parse(source)
            .expect("解析失败")
            .block_runs()
            .expect("块级语义失败");
        let headings: Vec<String> = runs
            .iter()
            .filter(|run| run.kind() == menghunote_editor::BlockKind::Heading)
            .map(|run| heading_text(source, run.source()))
            .collect();
        assert_eq!(headings, vec!["标题".to_owned(), "二级".to_owned()]);
        assert_eq!(normalize_heading("  ## 标题  "), "标题");
    }

    #[test]
    fn open_modifier_is_platform_specific() {
        let control = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        let platform = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        if cfg!(target_os = "macos") {
            assert!(is_open_link_modifier(&platform));
            assert!(!is_open_link_modifier(&control));
        } else {
            assert!(is_open_link_modifier(&control));
            assert!(!is_open_link_modifier(&platform));
        }
        assert!(!is_open_link_modifier(&Modifiers::default()));
    }
}
