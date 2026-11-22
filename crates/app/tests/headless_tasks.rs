//! ADR 0019 S3：Markdown 任务列表复选框的 app 集成测试（headless）。
//!
//! 真实 `AppShell` + 临时 vault（收敛点只有 `run_until_parked`，**不 sleep**）。
//! 覆盖：
//! 1. 点击未选中 → 源码 `[x]`；再点 → `[ ]`，且 caret 不被复选框点击移动；
//! 2. 嵌套 / 有序 / 大写 `[X]` 各自独立切换；
//! 3. 一次点击一个撤销组：`Ctrl+Z` 恢复 `[ ]`，`Ctrl+Shift+Z` 重做；
//! 4. durable 后重开 vault / 重新打开 shell，勾选状态仍是新的。
//!
//! 像素外观仍要真机验证；这里证明“命中 → 一次 EditRequest → source/undo/持久化”链路。

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _, Point,
    Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 窗口根视图：给 `AppShell` 一个稳定 selector，避免直接拿 `AppShell` 查 bounds。
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

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-tasks-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 在空 vault 里用真实 storage 路径建一篇笔记（结构 CRDT → 投影 → 正文 A 事务）。
fn seed_task_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer.commit("headless-tasks-seed").expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-tasks", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

/// 打开 AppShell（它会自动打开树里第一篇笔记）并跑完首帧。
fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-tasks".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn close_shell(host: &Entity<ShellHost>, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
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

fn caret_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> (usize, usize) {
    cx.update(|_window, app| {
        let selection = view.read(app).selection();
        (selection.anchor().get(), selection.head().get())
    })
}

/// 任务复选框矩形（窗口坐标；缺少某个 item 时直接失败，避免“点空”假通过）。
fn checkbox_rect(
    view: &Entity<EditorView>,
    item: usize,
    cx: &mut VisualTestContext,
) -> gpui::Bounds<gpui::Pixels> {
    cx.update(|_window, app| {
        view.read(app)
            .task_checkbox_rects()
            .into_iter()
            .find(|(index, _, _)| *index == item)
            .map(|(_, _, rect)| rect)
            .unwrap_or_else(|| panic!("item {item} 的任务复选框必须可见"))
    })
}

fn center(rect: gpui::Bounds<gpui::Pixels>) -> Point<gpui::Pixels> {
    rect.center()
}

/// 把 caret 放进第 0 行正文（默认 caret 在 scalar 0 = 列表 marker 内，会显现源码）。
///
/// 任务框只在“源码隐藏态”绘制，所以 headless 断言前必须先离开 marker；这个点击
/// 同时会触发一次真实 prepaint/paint，让 `task_checkbox_rects()` 有本帧几何。
fn focus_first_line_content(cx: &mut VisualTestContext) {
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    cx.simulate_click(
        gpui::point(
            pane.origin.x + gpui::px(theme::PAD_X + 70.0),
            pane.origin.y + gpui::px(theme::PAD_Y + 10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
}

fn load_text(vault_path: &Path, doc_id: &str) -> String {
    let vault = Vault::open(vault_path).expect("重开 vault");
    let (doc, _head) = load_body_doc(&vault, doc_id, 1, None).expect("重放正文失败");
    doc.text()
}

#[gpui::test]
async fn click_toggles_the_box_without_moving_the_caret(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let text = "- [ ] alpha\n- [x] beta\n- [X] gamma\n";
    seed_task_note(&vault, "任务列表", text);

    let (host, cx) = open_shell(cx, vault);
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    let view = active_view(&host, cx);

    // caret 放进第一个 item 的正文：之后点复选框必须不移动它。
    cx.simulate_click(
        gpui::point(
            pane.origin.x + gpui::px(theme::PAD_X + 70.0),
            pane.origin.y + gpui::px(theme::PAD_Y + 10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    let caret_before = caret_of(&view, cx);

    let first = checkbox_rect(&view, 0, cx);
    cx.simulate_click(center(first), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "- [x] alpha\n- [x] beta\n- [X] gamma\n",
        "点击未选中任务框必须只把 `[ ]` 改成 `[x]`"
    );
    assert_eq!(
        caret_of(&view, cx),
        caret_before,
        "点击复选框不得移动 caret"
    );

    // 第二次点击同一框 → `[x]` → `[ ]`，其它行/字节不动。
    let first_again = checkbox_rect(&view, 0, cx);
    cx.simulate_click(center(first_again), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), text, "再点一次必须逐字恢复原源码");
    assert_eq!(
        caret_of(&view, cx),
        caret_before,
        "第二次点击也不得移动 caret"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn nested_ordered_and_uppercase_boxes_toggle_independently(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let text = "- [ ] parent\n  - [x] child\n\n1. [X] ordered\n";
    let doc_id = seed_task_note(&vault, "嵌套任务", text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    focus_first_line_content(cx);
    let caret_before = caret_of(&view, cx);

    // 父 / 子 / 有序各点一次；每次重新取矩形（几何与最新 source 同源）。
    for (item, expected) in [
        (0usize, "- [x] parent\n  - [x] child\n\n1. [X] ordered\n"),
        (1, "- [x] parent\n  - [ ] child\n\n1. [X] ordered\n"),
        (2, "- [x] parent\n  - [ ] child\n\n1. [ ] ordered\n"),
    ] {
        let rect = checkbox_rect(&view, item, cx);
        cx.simulate_click(center(rect), Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            source_of(&view, cx),
            expected,
            "item {item} 的切换只能改自己的任务框"
        );
        assert_eq!(
            caret_of(&view, cx),
            caret_before,
            "item {item} 的点击不得移动 caret"
        );
    }

    close_shell(&host, cx);
    let persisted = load_text(&vault_path, &doc_id);
    assert_eq!(
        persisted, "- [x] parent\n  - [ ] child\n\n1. [ ] ordered\n",
        "落库后必须保持三次切换的结果"
    );
}

#[gpui::test]
async fn undo_and_redo_restore_the_task_box(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let text = "- [ ] alpha\n";
    let doc_id = seed_task_note(&vault, "撤销任务", text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    focus_first_line_content(cx);
    let rect = checkbox_rect(&view, 0, cx);
    cx.simulate_click(center(rect), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "- [x] alpha\n");

    // 一次点击 = 一个撤销组：一次 Ctrl+Z 就回到 `[ ]`。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), text, "一次撤销必须恢复任务框");
    // 重做恢复同一个 `[x]`（不批量改别的字节）。
    cx.simulate_keystrokes("ctrl-shift-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "- [x] alpha\n");

    close_shell(&host, cx);
    assert_eq!(load_text(&vault_path, &doc_id), "- [x] alpha\n");
}

#[gpui::test]
async fn reopened_vault_keeps_the_toggled_state(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let text = "- [ ] alpha\n- [x] beta\n";
    let doc_id = seed_task_note(&vault, "持久化任务", text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    focus_first_line_content(cx);
    let rect = checkbox_rect(&view, 0, cx);
    cx.simulate_click(center(rect), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "- [x] alpha\n- [x] beta\n");
    close_shell(&host, cx);

    // 1) durable 后正文逐字正确。
    assert_eq!(load_text(&vault_path, &doc_id), "- [x] alpha\n- [x] beta\n");

    // 2) 重新打开 shell（新 Vault / 新视图）：勾选态从 source 重建，仍是新状态。
    let vault = Vault::open(&vault_path).expect("再次打开 vault");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    focus_first_line_content(cx);
    let states: Vec<(usize, bool)> = cx.update(|_window, app| {
        view.read(app)
            .task_checkbox_rects()
            .into_iter()
            .map(|(item, checked, _)| (item, checked))
            .collect()
    });
    assert_eq!(
        states,
        vec![(0, true), (1, true)],
        "重开后两个复选框都必须来自 source 的新勾选态"
    );
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.starts_with(&format!("• {}", menghunote_editor::TASK_BOX_DISPLAY)),
        "重开后仍应投影成占位（不是源码 `[x]`）：{display:?}"
    );
    close_shell(&host, cx);
}
