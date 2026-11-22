//! 代码块只读语法高亮的 headless 集成测试（真实 `AppShell` + 临时 vault）。
//!
//! 覆盖：
//!
//! 1. ` ```rust ` 代码块打开后 `code_highlight_span_count() > 0`，Rust 的
//!    `fn`/字符串/注释/数字在 **display 坐标**上都有 span，且类别不只 Plain；
//! 2. 纯文本 / 未知语言 / 无 info 的围栏块不着色（span 为 0），且不 panic；
//! 3. 浅色/深色切换后 span 仍存在，但主题取色函数给出不同颜色（缓存失效并重绘）；
//! 4. 点击进入代码块后围栏显现、语法 span 重新映射（不失效成 0），继续输入可编辑，
//!    caret/选区/布局不回归。
//!
//! 大块超过 256 KiB 的上限降级在 `editor::code_highlight` 的单测里锁住（这里不塞
//! 256 KiB 的临时库，避免拖慢整套 headless 测试）。
//!
//! 纪律（AGENTS.md §6）：临时目录一律在 `CARGO_TARGET_TMPDIR` 下；收敛点只用
//! `run_until_parked()`，**不 sleep**。

use std::ops::Range;
use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Styled as _, TestAppContext, VisualTestContext, WindowAppearance,
};
use menghunote_app::editor::code_highlight::CodeHighlightClass;
use menghunote_app::editor::theme;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_app::theme as app_theme;
use menghunote_app::theme::ThemeMode;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_vault_dir(name: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-code-highlight-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 在空 vault 里建一篇正文（结构 CRDT → 投影 → 正文 A 事务）。
fn seed_note(vault: &Vault, title: &str, text: &str) {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-code-highlight-seed")
        .expect("结构提交失败");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-code-highlight", update)],
    )
    .expect("A 事务写入正文失败");
}

struct ShellHost {
    shell: Entity<AppShell>,
}

impl gpui::Render for ShellHost {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .size_full()
            .debug_selector(|| "app-shell-host".to_string())
            .child(self.shell.clone())
    }
}

fn open_shell<'a>(
    cx: &'a mut TestAppContext,
    vault: Vault,
    name: &str,
) -> (Entity<ShellHost>, &'a mut VisualTestContext) {
    let name = format!("headless-code-highlight-{name}");
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, name.clone(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn active_view(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Entity<EditorView> {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell
            .read(app)
            .active_editor_view()
            .expect("打开笔记后必须有活动编辑器视图")
    })
}

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn display_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).display_text())
}

fn span_snapshot(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> Vec<(Range<usize>, CodeHighlightClass)> {
    cx.update(|_window, app| view.update(app, |view, _| view.syntax_runs_display()))
}

fn span_count(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> usize {
    cx.update(|_window, app| view.update(app, |view, _| view.code_highlight_span_count()))
}

fn languages(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> Vec<String> {
    cx.update(|_window, app| view.update(app, |view, _| view.code_highlight_languages()))
}

fn selection_is_collapsed(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_window, app| view.read(app).selection().is_empty())
}

fn switch_theme(mode: ThemeMode, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        app_theme::set_mode(mode, WindowAppearance::Light, app);
    });
    cx.run_until_parked();
}

/// 打开一篇含 Rust 代码块的笔记（浅色主题 + 主题锁由调用方决定）。
fn rust_note() -> &'static str {
    "前言\n\n```rust\nfn main() {\n    let s = \"hello\"; // 中文注释\n    let n = 42;\n}\n```\n\n后文\n"
}

#[gpui::test]
async fn rust_code_block_gets_display_syntax_spans(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("rust");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "语法高亮", rust_note());
    let (host, cx) = open_shell(cx, vault, "rust");
    let view = active_view(&host, cx);

    // 1) 解析器认出了 fenced rust 块，app 跑出了 token。
    assert_eq!(languages(&view, cx), vec!["rust".to_owned()]);
    assert!(
        span_count(&view, cx) > 0,
        "```rust 代码块必须有语法 span（否则只读到纯文本）"
    );
    let spans = span_snapshot(&view, cx);
    let display = display_of(&view, cx);
    let classes: Vec<CodeHighlightClass> = spans.iter().map(|(_, class)| *class).collect();
    for expected in [
        CodeHighlightClass::Keyword,
        CodeHighlightClass::String,
        CodeHighlightClass::Comment,
        CodeHighlightClass::Number,
        CodeHighlightClass::Function,
    ] {
        assert!(
            classes.contains(&expected),
            "Rust 高亮缺 {expected:?}：{classes:?}"
        );
    }
    assert!(
        classes
            .iter()
            .any(|class| *class != CodeHighlightClass::Keyword),
        "颜色类别不能只有一种"
    );

    // 2) display 切片上确实能读到标记（说明 source→display 映射正确）。
    let slice = |range: &Range<usize>| -> &str {
        display
            .get(range.clone())
            .unwrap_or_else(|| panic!("span {range:?} 越界（display len {}）", display.len()))
    };
    assert!(
        spans.iter().any(|(range, class)| {
            slice(range) == "fn" && *class == CodeHighlightClass::Keyword
        }),
        "`fn` 必须是关键字 span：{spans:?} / display={display:?}"
    );
    assert!(
        spans
            .iter()
            .any(|(range, class)| *class == CodeHighlightClass::String
                && slice(range).contains("hello")),
        "字符串内容必须着色：{spans:?}"
    );
    assert!(
        spans
            .iter()
            .any(|(range, class)| *class == CodeHighlightClass::Comment
                && slice(range).contains("中文注释")),
        "注释必须着色（含中文）：{spans:?}"
    );
    assert!(
        spans
            .iter()
            .any(|(range, class)| { slice(range) == "42" && *class == CodeHighlightClass::Number }),
        "数字必须是 Number span：{spans:?}"
    );

    // 3) span 落在 display 的 char 边界内，且都在代码块区间里。
    for (range, _) in &spans {
        assert!(
            display.is_char_boundary(range.start) && display.is_char_boundary(range.end),
            "span {range:?} 切坏 UTF-8"
        );
        assert!(range.start < range.end && range.end <= display.len());
    }
    let blocks =
        cx.update(|_window, app| view.update(app, |view, _| view.code_block_display_ranges()));
    assert!(!blocks.is_empty(), "代码块必须有 display 区间");
    for (range, _) in &spans {
        assert!(
            blocks
                .iter()
                .any(|block| block.start <= range.start && range.end <= block.end),
            "语法 span {range:?} 必须在代码块 display 区间 {blocks:?} 内"
        );
    }
    // 布局仍是代码块的等宽行（没有因为着色改变度量）。
    let metrics = cx.update(|_window, app| view.read(app).visual_row_metrics());
    assert!(!metrics.is_empty(), "着色后仍必须有布局几何");
}

#[gpui::test]
async fn plain_and_unknown_fenced_blocks_stay_plain(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("plain");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let text = "```text\n纯文本块\n```\n\n```not-a-language\nfoo bar\n```\n\n```\n无 info\n```\n";
    seed_note(&vault, "无色代码块", text);
    let (host, cx) = open_shell(cx, vault, "plain");
    let view = active_view(&host, cx);

    assert_eq!(
        span_count(&view, cx),
        0,
        "text / 未知语言 / 无 info 的围栏块都不能着色（降级无色）"
    );
    assert_eq!(
        languages(&view, cx),
        vec![
            "text".to_owned(),
            "not-a-language".to_owned(),
            String::new()
        ],
        "块顺序/语言 token 必须与源码一致"
    );
}

#[gpui::test]
async fn theme_switch_keeps_spans_and_changes_syntax_colors(cx: &mut TestAppContext) {
    let guard = app_theme::lock_for_test();
    guard.reset(None);

    let dir = temp_vault_dir("theme");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "深浅色高亮", rust_note());
    let (host, cx) = open_shell(cx, vault, "theme");
    let view = active_view(&host, cx);

    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::LIGHT);
    let light_spans = span_snapshot(&view, cx);
    assert!(!light_spans.is_empty(), "浅色下必须有语法 span");
    let light_keyword = theme::syntax_keyword();
    assert_eq!(
        light_keyword,
        app_theme::light_theme().syntax_keyword,
        "浅色取色必须来自浅色主题 token"
    );
    assert_eq!(
        theme::syntax_color(CodeHighlightClass::Keyword),
        light_keyword,
        "类别取色入口必须与 token 一致"
    );

    switch_theme(ThemeMode::Dark, cx);
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::DARK);
    let dark_spans = span_snapshot(&view, cx);
    assert!(!dark_spans.is_empty(), "深色下语法 span 必须仍然存在");
    assert_eq!(
        light_spans, dark_spans,
        "span 只含坐标/类别；切主题不应改变着色范围（颜色在 shaping 时取）"
    );
    let dark_keyword = theme::syntax_keyword();
    assert_eq!(
        dark_keyword,
        app_theme::dark_theme().syntax_keyword,
        "深色取色必须来自深色主题 token"
    );
    assert_ne!(light_keyword, dark_keyword, "深浅色的关键字 token 必须不同");
    for (name, light, dark) in [
        (
            "syntax_string",
            app_theme::light_theme().syntax_string,
            app_theme::dark_theme().syntax_string,
        ),
        (
            "syntax_comment",
            app_theme::light_theme().syntax_comment,
            app_theme::dark_theme().syntax_comment,
        ),
        (
            "syntax_number",
            app_theme::light_theme().syntax_number,
            app_theme::dark_theme().syntax_number,
        ),
        (
            "syntax_function",
            app_theme::light_theme().syntax_function,
            app_theme::dark_theme().syntax_function,
        ),
    ] {
        assert_ne!(light, dark, "{name} 的深浅色必须不同");
    }
}

#[gpui::test]
async fn editing_inside_code_block_keeps_highlight_and_editor_state(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("edit");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let text = rust_note();
    seed_note(&vault, "代码块编辑", text);
    let (host, cx) = open_shell(cx, vault, "edit");
    let view = active_view(&host, cx);

    // 打开后先确认有高亮，且围栏处于隐藏态（display 不含 ```rust）。
    assert!(span_count(&view, cx) > 0);
    assert!(
        !display_of(&view, cx).contains("```rust"),
        "caret 不在块内时围栏应隐藏"
    );

    // 点击代码内容里的 `let s` 之后（caret 进入代码块 → 围栏显现）。
    let source = source_of(&view, cx);
    let click_byte = source.find("let s").expect("种子正文必须有 `let s`") + 1;
    let point = cx
        .update(|window, app| view.read(app).point_for_source_byte(click_byte, window))
        .expect("代码行必须能换算成窗口坐标（布局存在）");
    cx.simulate_click(point, Modifiers::default());
    cx.run_until_parked();

    assert!(
        selection_is_collapsed(&view, cx),
        "单击代码内容后光标必须是折叠选区"
    );
    let revealed = display_of(&view, cx);
    assert!(
        revealed.contains("```rust"),
        "caret 进入代码块后围栏必须显现（display={revealed:?}）"
    );
    assert!(
        span_count(&view, cx) > 0,
        "围栏显现导致 display 偏移变化后，语法 span 必须重新映射而不是失效"
    );

    // 继续输入：代码块仍可直接编辑，着色缓存随 source 失效重建。
    cx.simulate_input("x");
    cx.run_until_parked();
    let edited = source_of(&view, cx);
    assert_ne!(edited, text, "输入必须真的改了正文");
    assert_eq!(edited.len(), text.len() + 1, "只插入一个字节");
    let x_at = edited.find('x').expect("输入字符必须写入正文");
    let code_start = edited.find("```rust").expect("代码块仍在") + "```rust".len();
    let code_end = edited.rfind("```").expect("闭围栏仍在");
    assert!(
        code_start <= x_at && x_at < code_end,
        "输入必须落在代码块内容里：{edited:?}"
    );
    assert!(selection_is_collapsed(&view, cx), "输入后仍是折叠 caret");
    assert!(
        span_count(&view, cx) > 0,
        "编辑后代码块仍必须被着色（缓存按 source 重建）"
    );
    // 布局/命中仍可用：caret 所在的 source 字节还能换算成窗口坐标。
    let caret_byte = cx.update(|_window, app| {
        let source = view.read(app).source_text();
        view.read(app)
            .selection()
            .head()
            .to_source_byte(&source)
            .expect("caret 必须落在 source 边界")
            .get()
    });
    assert!(
        cx.update(|window, app| view.read(app).point_for_source_byte(caret_byte, window))
            .is_some(),
        "编辑后布局几何必须仍然存在"
    );
}
