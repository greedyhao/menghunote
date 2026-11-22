//! 反引号自动补全（纯函数，**不含 GPUI / 输入法**）。
//!
//! 编辑器把平台送来的普通文本输入（`EntityInputHandler::replace_text_in_range`）
//! 交给本模块判定“这一次反引号该怎么落”：
//!
//! | 场景 | 动作 |
//! | --- | --- |
//! | 普通位置输入 `` ` `` | 插入成对 `` `` ``，caret 落在两个反引号之间 |
//! | caret 在成对反引号中间（或行内代码的闭合位置）再输入 `` ` `` | 跳过自动补的那个（caret 右移），不产生编辑 |
//! | 行首连续输入到 ```` ``` ```` | 补全 fenced code block：插入开围栏 + 空行 + 闭围栏，caret 落在**开围栏之后**（便于输入 info string / 语言） |
//!
//! # 为什么是“纯函数 + app 执行”
//!
//! 本模块只回答**动作**，不碰 `EditorController`、不产生 `EditRequest`、不依赖
//! GPUI；app 据此走既有的 `compose_replace_selection`（一次按键最多一个请求、一个
//! 撤销组）。这样规则可以在无窗口环境逐条单测，输入法/表格/代码块等上下文由 app
//! 在调用前用行语义过滤（见 `EditorView::try_backtick_autocomplete`）。
//!
//! # 规则（写死，测试钉住）
//!
//! - 只处理**折叠 caret**（app 在非空选区时直接走普通替换）；
//! - caret 必须落在 UTF-8 char 边界，越界返回 [`BacktickAction::Plain`]；
//! - “成对中间”包括空对（`|` 两侧都是反引号）与行内代码闭合位置（caret 前一行内
//!   有奇数个反引号、caret 处正好是自动补的闭引号）；
//! - 行首的“连续反引号”只看**当前行 caret 之前**是否全为反引号；行尾（caret 之后）
//!   只允许空白才补全 fenced block（避免吞掉已有正文/表格）；
//! - 行首已有 1 个反引号时继续输入单个（先凑成 2 个），第 3 个才补全 fenced block；
//!   已有 3+ 个时继续单个（不嵌套围栏）；
//! - 其它情况（普通文字/行首空行）插入成对反引号。
//!
//! 明确不做：不解析 Markdown、不判断代码块/表格（app 用行语义过滤）、不处理 IME
//! 组合文本（组合上屏走 `commit_composition`，不经过本模块）。

/// 一次反引号输入的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BacktickAction {
    /// 普通插入一个反引号（不配对）。
    Plain,
    /// caret 在成对反引号中间（或行内代码闭合位置）：跳过自动补的那个。
    SkipPaired,
    /// 插入一对反引号，caret 落在中间。
    InsertPair,
    /// 行首连续反引号：只插入一个（继续凑围栏）。
    InsertSingle,
    /// 行首 ```` ``` ````：补全 fenced code block。
    CompleteFence,
}

/// 成对反引号的插入文本。
pub const PAIR_INSERT: &str = "``";
/// 成对插入后 caret 相对原 caret 的字节偏移（落在两个反引号之间）。
pub const PAIR_CARET_OFFSET: usize = 1;
/// 单个反引号的插入文本。
pub const SINGLE_INSERT: &str = "`";
/// 单个插入后 caret 相对原 caret 的字节偏移。
pub const SINGLE_CARET_OFFSET: usize = 1;
/// 行首 ```` ``` ```` 补全时插入的文本（第三个反引号 + 空内容行 + 闭围栏）。
pub const FENCE_INSERT: &str = "`\n\n```";
/// 补全后 caret 相对原 caret 的字节偏移：落在**开围栏之后**（第三个反引号之后），
/// 这样紧接着输入 `mermaid` 会写成 ```` ```mermaid ```` 而不是内容行。
pub const FENCE_CARET_OFFSET: usize = 1;

/// 计算在 `source` 的 `caret_byte` 处输入一个反引号的动作。
///
/// `caret_byte` 是 **UTF-8 字节**（与 `SourceByte` 一致），必须落在 char 边界。
#[must_use]
pub fn action(source: &str, caret_byte: usize) -> BacktickAction {
    if caret_byte > source.len() || !source.is_char_boundary(caret_byte) {
        return BacktickAction::Plain;
    }
    let bytes = source.as_bytes();
    let line_start = source[..caret_byte]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let line_end = source[caret_byte..]
        .find('\n')
        .map_or(source.len(), |offset| caret_byte + offset);
    let prefix = &source[line_start..caret_byte];
    let rest = &source[caret_byte..line_end];

    // 1) caret 两侧都是反引号：空对中间 / 连续反引号中间 → 跳过右边那个。
    if caret_byte > 0
        && caret_byte < source.len()
        && bytes[caret_byte - 1] == b'`'
        && bytes[caret_byte] == b'`'
    {
        return BacktickAction::SkipPaired;
    }

    // 2) 行内代码的闭合位置：caret 处是自动补的闭引号，且本行 caret 之前有奇数个
    //    反引号（一个未闭合的开引号）。跳过它，避免再插一对。
    if caret_byte < source.len() && bytes[caret_byte] == b'`' {
        let ticks_before = prefix.bytes().filter(|byte| *byte == b'`').count();
        if ticks_before % 2 == 1 {
            return BacktickAction::SkipPaired;
        }
    }

    // 3) 行首连续反引号：只在“caret 之前全是反引号”时进入围栏规则。
    let prefix_all_ticks = prefix.bytes().all(|byte| byte == b'`');
    if prefix_all_ticks {
        let ticks = prefix.len();
        let rest_blank = rest.bytes().all(|byte| byte == b' ' || byte == b'\t');
        if ticks == 2 && rest_blank {
            return BacktickAction::CompleteFence;
        }
        // 已有 1 个：先补成 2 个；已有 3+ 个：继续单个（不嵌套围栏）。
        // 行尾还有正文时也继续单个，避免把已有文本吞进围栏。
        if ticks >= 1 {
            return BacktickAction::InsertSingle;
        }
    }

    // 4) 普通位置：成对插入。
    BacktickAction::InsertPair
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_position_inserts_a_pair() {
        assert_eq!(action("", 0), BacktickAction::InsertPair);
        assert_eq!(action("abc", 3), BacktickAction::InsertPair);
        assert_eq!(action("a b", 2), BacktickAction::InsertPair);
        assert_eq!(action("标题", "标题".len()), BacktickAction::InsertPair);
    }

    #[test]
    fn caret_between_a_pair_skips_the_auto_inserted_tick() {
        assert_eq!(action("``", 1), BacktickAction::SkipPaired, "空对中间");
        assert_eq!(
            action("```", 1),
            BacktickAction::SkipPaired,
            "连续反引号中间"
        );
        assert_eq!(action("```", 2), BacktickAction::SkipPaired);
    }

    #[test]
    fn closing_an_inline_code_skips_the_paired_tick() {
        // 自动补全后的行内代码：`x` 且 caret 在闭引号之前。
        assert_eq!(action("`x`", 2), BacktickAction::SkipPaired);
        assert_eq!(action("`中文`", 7), BacktickAction::SkipPaired);
        // 没有未闭合开引号时（偶数个）不跳过，正常配对。
        assert_eq!(action("`x``", 4), BacktickAction::InsertPair);
    }

    #[test]
    fn line_start_three_ticks_completes_a_fenced_block() {
        // 自动配对 → 跳过 → 第三个反引号：行首两个反引号 + 行尾空白。
        assert_eq!(action("``", 2), BacktickAction::CompleteFence);
        assert_eq!(action("``\n后文", 2), BacktickAction::CompleteFence);
        assert_eq!(action("``\n", 2), BacktickAction::CompleteFence);
        assert_eq!(action("a\n``", 4), BacktickAction::CompleteFence);
        assert_eq!(action("``   ", 2), BacktickAction::CompleteFence);
    }

    #[test]
    fn line_start_builds_one_tick_at_a_time_before_the_fence() {
        assert_eq!(action("`", 1), BacktickAction::InsertSingle);
        assert_eq!(action("`\n", 1), BacktickAction::InsertSingle);
        assert_eq!(
            action("```", 3),
            BacktickAction::InsertSingle,
            "已有围栏不再嵌套"
        );
        assert_eq!(action("````", 4), BacktickAction::InsertSingle);
    }

    #[test]
    fn line_start_with_existing_text_never_swallows_it() {
        // 行首已有正文：继续单个反引号，不补全围栏（不破坏已有文本/表格）。
        assert_eq!(action("``abc", 2), BacktickAction::InsertSingle);
        assert_eq!(action("``| a |", 2), BacktickAction::InsertSingle);
        assert_eq!(action("``\t后文", 2), BacktickAction::InsertSingle);
    }

    #[test]
    fn indented_or_mid_line_ticks_are_not_fence_completions() {
        // 行首缩进：不是“行首连续反引号” → 普通配对（app 还会按行语义过滤）。
        assert_eq!(action("  ``", 4), BacktickAction::InsertPair);
        // 行中反引号后：普通配对。
        assert_eq!(action("a``", 3), BacktickAction::InsertPair);
    }

    #[test]
    fn invalid_caret_falls_back_to_plain() {
        assert_eq!(action("abc", 4), BacktickAction::Plain);
        assert_eq!(action("中", 1), BacktickAction::Plain, "非 char 边界");
    }

    #[test]
    fn insertion_constants_match_the_documented_caret_offsets() {
        assert_eq!(PAIR_INSERT.len(), 2);
        assert_eq!(PAIR_CARET_OFFSET, 1);
        assert_eq!(SINGLE_INSERT.len(), 1);
        assert_eq!(SINGLE_CARET_OFFSET, 1);
        assert_eq!(FENCE_INSERT, "`\n\n```");
        assert_eq!(
            FENCE_CARET_OFFSET, 1,
            "caret 落在开围栏之后，便于输入 info string"
        );
    }
}
