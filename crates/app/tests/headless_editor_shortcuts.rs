//! 编辑器常用快捷键的 app 输入集成测试（真实 `AppShell` + 临时 vault）。
//!
//! 覆盖（全部走 `EditorView::on_key_down` 的 Ctrl 分支 / Esc 分支）：
//!
//! 1. `Ctrl+←/→`：在 **display** 文本上按词移动（隐藏的 `**` 不参与切词）；
//! 2. `Ctrl+Shift+←`：保持 anchor 按词扩选（不是逐字），连续两次扩到前一个词；
//! 3. `Ctrl+L`：选中 caret 所在的**源码行**（含行尾换行），`Ctrl+C` 能整行复制；
//! 4. `Ctrl+Shift+Home/End`：从 anchor 扩选到文件开头/结尾；`Ctrl+Home/End` 不扩选；
//! 5. `Esc`：有选区时折叠到 head，且**不删除文本**；
//! 6. `Ctrl+←` 跳过隐藏 Markdown 标记（`**hello** world` 不会被当成多个词）。
//!
//! 纪律（AGENTS.md §6）：临时目录一律 `CARGO_TARGET_TMPDIR`；收敛点只用
//! `run_until_parked()`，不 sleep。真实键盘/IME/DPI 仍必须 Windows 真机验收。

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-editor-shortcuts-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

fn seed_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-editor-shortcuts-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-editor-shortcuts", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
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

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-editor-shortcuts".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn active_view(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Entity<EditorView> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_editor_view()
            .expect("打开笔记后必须有活动编辑器视图")
    })
}

fn focus_editor(cx: &mut VisualTestContext) {
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    cx.simulate_click(
        gpui::point(
            pane.origin.x + gpui::px(theme::PAD_X + 2.0),
            pane.origin.y + gpui::px(theme::PAD_Y + 10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
}

fn close_shell(host: &Entity<ShellHost>, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn display_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).display_text())
}

fn selected_text(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_window, app| view.read(app).selected_source_text())
}

fn caret_byte(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> usize {
    let source = source_of(view, cx);
    cx.update(|_window, app| {
        view.read(app)
            .selection()
            .head()
            .to_source_byte(&source)
            .expect("caret 必须落在 source 边界")
            .get()
    })
}

fn anchor_byte(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> usize {
    let source = source_of(view, cx);
    cx.update(|_window, app| {
        view.read(app)
            .selection()
            .anchor()
            .to_source_byte(&source)
            .expect("anchor 必须落在 source 边界")
            .get()
    })
}

fn start_byte(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> usize {
    let source = source_of(view, cx);
    cx.update(|_window, app| {
        view.read(app)
            .selection()
            .start()
            .to_source_byte(&source)
            .expect("选区起点必须落在 source 边界")
            .get()
    })
}

fn end_byte(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> usize {
    let source = source_of(view, cx);
    cx.update(|_window, app| {
        view.read(app)
            .selection()
            .end()
            .to_source_byte(&source)
            .expect("选区终点必须落在 source 边界")
            .get()
    })
}

fn selection_is_empty(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_window, app| view.read(app).selection().is_empty())
}

/// 打开一篇单行文本笔记并聚焦正文，返回 (host, view, cx)。
fn open_note<'a>(
    cx: &'a mut TestAppContext,
    title: &str,
    text: &str,
) -> (
    Entity<ShellHost>,
    Entity<EditorView>,
    &'a mut VisualTestContext,
) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, title, text);
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    focus_editor(cx);
    (host, view, cx)
}

// ---- 1. Ctrl+←/→：display 上的按词移动 ----

#[gpui::test]
async fn ctrl_left_and_right_move_between_display_word_starts(cx: &mut TestAppContext) {
    let (host, view, cx) = open_note(cx, "按词移动", "hello world\n");
    assert_eq!(
        display_of(&view, cx),
        "hello world\n",
        "初始 display 是纯文本"
    );

    // 现有 Home/End 行为把 caret 放到首行末尾（"world" 之后）。
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    assert_eq!(caret_byte(&view, cx), 11, "`end` 落在首行末尾");

    cx.simulate_keystrokes("ctrl-left");
    cx.run_until_parked();
    assert_eq!(caret_byte(&view, cx), 6, "Ctrl+← 到 world 开头");

    cx.simulate_keystrokes("ctrl-left");
    cx.run_until_parked();
    assert_eq!(caret_byte(&view, cx), 0, "再 Ctrl+← 到 hello 开头");

    cx.simulate_keystrokes("ctrl-right");
    cx.run_until_parked();
    assert_eq!(caret_byte(&view, cx), 6, "Ctrl+→ 回到 world 开头");

    // 词内任意位置：一次左移回到当前词开头（不是逐字退一格）。
    cx.simulate_keystrokes("ctrl-right");
    cx.run_until_parked();
    assert_eq!(caret_byte(&view, cx), 12, "world 之后越过分隔符到文件尾");
    assert_eq!(source_of(&view, cx), "hello world\n", "按词移动不产生编辑");

    close_shell(&host, cx);
}

// ---- 2. Ctrl+Shift+←：保持 anchor 按词扩选 ----

#[gpui::test]
async fn ctrl_shift_left_extends_by_word_from_fixed_anchor(cx: &mut TestAppContext) {
    let (host, view, cx) = open_note(cx, "按词扩选", "hello world\n");
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    let anchor = anchor_byte(&view, cx);
    assert_eq!(anchor, 11, "扩选前 anchor 在行尾");

    cx.simulate_keystrokes("ctrl-shift-left");
    cx.run_until_parked();
    assert!(!selection_is_empty(&view, cx), "Shift 扩选必须产生非空选区");
    assert_eq!(anchor_byte(&view, cx), anchor, "anchor 不能移动");
    assert_eq!(
        caret_byte(&view, cx),
        6,
        "head 按词到 world 开头（不是 10）"
    );
    assert_eq!(selected_text(&view, cx).as_deref(), Some("world"));

    cx.simulate_keystrokes("ctrl-shift-left");
    cx.run_until_parked();
    assert_eq!(anchor_byte(&view, cx), anchor, "连续扩选 anchor 仍不动");
    assert_eq!(caret_byte(&view, cx), 0, "第二次扩到 hello 开头");
    assert_eq!(start_byte(&view, cx), 0);
    assert_eq!(end_byte(&view, cx), anchor);
    assert_eq!(selected_text(&view, cx).as_deref(), Some("hello world"));
    assert_eq!(source_of(&view, cx), "hello world\n", "扩选不产生编辑");

    // 反方向：折叠到 0 后 Ctrl+Shift+→ 按词向右扩（anchor 固定在 0）。
    cx.simulate_keystrokes("ctrl-home");
    cx.run_until_parked();
    assert!(selection_is_empty(&view, cx));
    cx.simulate_keystrokes("ctrl-shift-right");
    cx.run_until_parked();
    assert_eq!(anchor_byte(&view, cx), 0, "向右扩选 anchor 也不动");
    assert_eq!(caret_byte(&view, cx), 6, "head 按词到 world 开头");
    cx.simulate_keystrokes("ctrl-shift-right");
    cx.run_until_parked();
    assert_eq!(anchor_byte(&view, cx), 0);
    assert_eq!(caret_byte(&view, cx), 12, "第二次扩到行尾/文件尾");
    assert_eq!(selected_text(&view, cx).as_deref(), Some("hello world\n"));
    assert_eq!(source_of(&view, cx), "hello world\n", "扩选不产生编辑");

    close_shell(&host, cx);
}

// ---- 3. Ctrl+L：选中当前源码行 ----

#[gpui::test]
async fn ctrl_l_selects_the_current_source_line(cx: &mut TestAppContext) {
    let (host, view, cx) = open_note(cx, "整行选择", "hello world\nsecond line\n");
    let empty = selection_is_empty(&view, cx);
    assert!(empty, "初始 caret 是折叠的");

    cx.simulate_keystrokes("ctrl-l");
    cx.run_until_parked();
    assert_eq!(
        (start_byte(&view, cx), end_byte(&view, cx)),
        (0, 12),
        "第一行（含行尾换行）被整行选中"
    );
    assert_eq!(
        selected_text(&view, cx).as_deref(),
        Some("hello world\n"),
        "Ctrl+C 会复制到整行内容 + 换行"
    );
    assert_eq!(anchor_byte(&view, cx), 0, "anchor 在行首");
    assert_eq!(caret_byte(&view, cx), 12, "head 在行尾后（下一行开头）");

    // head 已经在下一行开头：再按一次选中第二行。
    cx.simulate_keystrokes("ctrl-l");
    cx.run_until_parked();
    assert_eq!((start_byte(&view, cx), end_byte(&view, cx)), (12, 24));
    assert_eq!(
        selected_text(&view, cx).as_deref(),
        Some("second line\n"),
        "第二行同样整行选中"
    );

    assert_eq!(
        source_of(&view, cx),
        "hello world\nsecond line\n",
        "不产生编辑"
    );
    close_shell(&host, cx);
}

// ---- 4. Ctrl+Shift+Home/End 与 Ctrl+Home/End ----

#[gpui::test]
async fn ctrl_shift_home_end_and_ctrl_home_end_jump_to_document_edges(cx: &mut TestAppContext) {
    let (host, view, cx) = open_note(cx, "文件首尾", "hello world\n");
    let len = source_of(&view, cx).len();

    // caret 放到 world 开头（anchor = 6）。
    cx.simulate_keystrokes("end ctrl-left");
    cx.run_until_parked();
    assert_eq!(caret_byte(&view, cx), 6);

    cx.simulate_keystrokes("ctrl-shift-home");
    cx.run_until_parked();
    assert_eq!(start_byte(&view, cx), 0, "Shift+Home 覆盖到文件开头");
    assert_eq!(caret_byte(&view, cx), 0, "head 到文件开头");
    assert_eq!(anchor_byte(&view, cx), 6, "anchor 保持不动");

    cx.simulate_keystrokes("ctrl-shift-end");
    cx.run_until_parked();
    assert_eq!(start_byte(&view, cx), 6, "anchor 仍是 6，选区从 6 开始");
    assert_eq!(end_byte(&view, cx), len, "Shift+End 覆盖到文件结尾");
    assert_eq!(caret_byte(&view, cx), len, "head 到文件结尾");
    assert_eq!(selected_text(&view, cx).as_deref(), Some("world\n"));

    // 不带 Shift：折叠移动到文件首/尾，不产生选区。
    cx.simulate_keystrokes("ctrl-home");
    cx.run_until_parked();
    assert!(selection_is_empty(&view, cx), "Ctrl+Home 不扩选");
    assert_eq!(caret_byte(&view, cx), 0);

    cx.simulate_keystrokes("ctrl-end");
    cx.run_until_parked();
    assert!(selection_is_empty(&view, cx), "Ctrl+End 不扩选");
    assert_eq!(caret_byte(&view, cx), len);

    assert_eq!(source_of(&view, cx), "hello world\n", "首尾移动不产生编辑");
    close_shell(&host, cx);
}

// ---- 5. Esc 折叠选区，不删除文本 ----

#[gpui::test]
async fn escape_collapses_selection_without_deleting_text(cx: &mut TestAppContext) {
    let (host, view, cx) = open_note(cx, "Esc 折叠", "hello world\n");
    let original = source_of(&view, cx);

    cx.simulate_keystrokes("end ctrl-shift-home");
    cx.run_until_parked();
    assert!(!selection_is_empty(&view, cx), "先造一个非空选区");
    let head = caret_byte(&view, cx);
    assert_eq!(head, 0, "Shift+Home 后 head 在文件开头");

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(selection_is_empty(&view, cx), "Esc 把选区折叠成 caret");
    assert_eq!(
        caret_byte(&view, cx),
        head,
        "收起后停在原 head（不是 anchor）"
    );
    assert_eq!(source_of(&view, cx), original, "Esc 不删除、不修改正文");

    // 无选区时 Esc 保持原样（不 panic、不产生编辑）。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(selection_is_empty(&view, cx));
    assert_eq!(source_of(&view, cx), original, "折叠态的 Esc 仍不产生编辑");

    close_shell(&host, cx);
}

// ---- 6. 隐藏 Markdown 标记不参与按词切分 ----

#[gpui::test]
async fn ctrl_left_skips_hidden_markdown_markers(cx: &mut TestAppContext) {
    let (host, view, cx) = open_note(cx, "隐藏标记", "**hello** world\n");
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    assert_eq!(caret_byte(&view, cx), 15, "`end` 落在一行末尾");
    let display = display_of(&view, cx);
    assert!(
        !display.contains("**"),
        "caret 在构造外时隐藏标记不参与 display，实际 {display:?}"
    );

    // display "hello world\n"：第一次左移到 world 开头 → source 10。
    cx.simulate_keystrokes("ctrl-left");
    cx.run_until_parked();
    assert_eq!(caret_byte(&view, cx), 10, "Ctrl+← 到 world 开头");

    // 第二次左移到 hello 开头 → source 2（`**` 之后），不是 0 也不是某个 `*`。
    cx.simulate_keystrokes("ctrl-left");
    cx.run_until_parked();
    assert_eq!(caret_byte(&view, cx), 2, "隐藏的 `**` 不被当成单词/落点");
    assert_eq!(
        source_of(&view, cx),
        "**hello** world\n",
        "按词移动只改选区，不产生编辑"
    );

    close_shell(&host, cx);
}
