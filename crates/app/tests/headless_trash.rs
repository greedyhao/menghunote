//! P0 回收站（查看 + 恢复）的 headless 端到端门禁（真实 `AppShell` + 临时 vault）。
//!
//! 覆盖：
//!
//! 1. 删除有正文的笔记 → 打开回收站（行/标题存在，空态不存在）→ 恢复 → 侧栏节点回来、
//!    原父级不变、正文与删除前一致、`notes.deleted = 0` → 再次打开回收站为空；
//! 2. 根级笔记 + 文件夹内笔记各删一个 → 回收站两行；恢复文件夹内笔记后仍在该文件夹下；
//! 3. 空回收站显示 `trash-empty`；
//! 4. 恢复失败（注入结构提交失败）→ 面板保留行 + inline 错误；去掉故障后可重试成功；
//! 5. 打开/关闭（Esc、点遮罩、关闭按钮）与焦点回归；`ribbon-trash` / `sidebar-trash`
//!    兼容 selector 同时存在。
//!
//! 纪律（AGENTS.md §6）：临时目录一律建在 `CARGO_TARGET_TMPDIR` 下；不 `sleep`；
//! 唯一的收敛点是 `run_until_parked()`。像素/DPI/真实窗口仍需 Windows 真机验收。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Entity, Focusable as _, InteractiveElement as _, Modifiers,
    MouseButton, ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::shell::trash::TrashPanel;
use menghunote_app::shell::AppShell;
use menghunote_storage::{
    append_updates, list_tree_sorted, load_body_doc, trashed_notes, StructureWriter, TreeSort,
    UpdateRecord, Value, Vault,
};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 窗口根视图（给 `AppShell` 一个稳定 selector）。
struct ShellHost {
    shell: Entity<AppShell>,
}

impl Render for ShellHost {
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

fn temp_vault(prefix: &str) -> (TempDir, Vault) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-trash-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    (dir, vault)
}

/// 正文 A 事务：把 `text` 写进已存在的 body doc（与其它 headless 测试同一条路径）。
fn write_body(vault: &Vault, body_doc_id: &str, text: &str) {
    let (doc, head) = load_body_doc(vault, body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-trash", update)],
    )
    .expect("A 事务写入正文失败");
}

/// 播种结果（结构 ID + 正文 ID；headless 断言用）。
struct SeededNote {
    node_id: String,
    note_id: String,
    body_doc_id: String,
}

/// 建一篇根级笔记（标题 + 正文）。
fn seed_note(vault: &Vault, title: &str, text: &str) -> SeededNote {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer.commit("headless-trash-seed").expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, text);
    SeededNote {
        node_id: seed.node_id,
        note_id: seed.note_id,
        body_doc_id: seed.body_doc_id,
    }
}

/// 建一个文件夹，并在其中放一篇带正文的笔记；返回笔记与文件夹 node_id。
fn seed_folder_note(
    vault: &Vault,
    folder_title: &str,
    title: &str,
    text: &str,
) -> (SeededNote, String) {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let folder_id = writer
        .doc()
        .create_folder(folder_title, 1)
        .expect("结构新建文件夹失败");
    let seed = writer
        .doc()
        .create_note(title, 2)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .doc()
        .move_node(&seed.node_id, Some(&folder_id), 3)
        .expect("把笔记移入文件夹失败");
    writer
        .commit("headless-trash-seed-folder")
        .expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, text);
    (
        SeededNote {
            node_id: seed.node_id,
            note_id: seed.note_id,
            body_doc_id: seed.body_doc_id,
        },
        folder_id,
    )
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-trash".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    // 先移动鼠标：行点击走 `on_click`，gpui 的 click 监听要求按下时 hitbox 已被
    // 上一次 mouse move 标成 hovered（真实用户一定会先移过去；headless 不会自动移）。
    cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn right_click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_mouse_move(bounds.center(), None, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_mouse_down(bounds.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
}

/// 把运行时生成的 selector 泄漏成 `'static str`（`debug_bounds` 只接受静态串）。
fn leak(selector: String) -> &'static str {
    Box::leak(selector.into_boxed_str())
}

/// 结构树里的下标（就是 `tree-row-<i>` 的 `i`）。
fn tree_row_of(host: &Entity<ShellHost>, cx: &mut VisualTestContext, node_id: &str) -> usize {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .tree_snapshot()
            .iter()
            .position(|entry| entry.node_id == node_id)
            .unwrap_or_else(|| panic!("节点 {node_id} 必须出现在侧栏里"))
    })
}

/// 右键笔记 →「删除」→二次确认「确认删除」（真实菜单几何命中路径）。
fn delete_note_via_menu(host: &Entity<ShellHost>, cx: &mut VisualTestContext, node_id: &str) {
    let row = tree_row_of(host, cx, node_id);
    let row_selector = leak(format!("tree-row-{row}"));
    right_click_selector(cx, row_selector);
    assert!(
        cx.debug_bounds("context-menu").is_some(),
        "右键笔记必须弹出菜单"
    );
    click_selector(cx, "menu-item-2");
    assert!(
        cx.debug_bounds("context-menu").is_some(),
        "第一次点「删除」只进入二次确认，菜单不得关闭"
    );
    click_selector(cx, "menu-item-2");
    assert!(
        cx.debug_bounds("context-menu").is_none(),
        "二次确认后菜单必须关闭"
    );
}

fn trash_panel(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Entity<TrashPanel> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .trash_entity()
            .expect("回收站覆盖层必须打开")
    })
}

fn trash_restore_selector(index: usize) -> &'static str {
    leak(format!("trash-restore-{index}"))
}

fn restored_notice(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .notice_text()
            .map(str::to_owned)
    })
}

/// `notes.deleted` 投影值（1 = 仍被删除；恢复后必须是 0）。
fn notes_deleted(vault: &Vault, note_id: &str) -> i64 {
    let rows = vault
        .query(
            "SELECT deleted FROM notes WHERE note_id = ?1",
            &[Value::Text(note_id.to_owned())],
        )
        .expect("查询 notes.deleted 失败");
    match rows.first().and_then(|row| row.first()) {
        Some(Value::Integer(value)) => *value,
        other => panic!("notes.deleted 读回异常：{other:?}"),
    }
}

fn visible_titles(vault: &Vault) -> Vec<String> {
    list_tree_sorted(vault, TreeSort::NameAsc)
        .expect("读结构树失败")
        .into_iter()
        .map(|entry| entry.title)
        .collect()
}

#[gpui::test]
async fn delete_open_restore_round_trip_keeps_body_and_parent(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("round-trip");
    let seeded = seed_note(&vault, "回收站笔记", "回收站正文\n");
    let (host, cx) = open_shell(cx, vault.clone());
    assert_eq!(
        cx.update(|_window, app| host
            .read(app)
            .shell
            .read(app)
            .active_note_id()
            .map(str::to_owned)),
        Some(seeded.note_id.clone()),
        "启动后必须先打开这篇笔记"
    );

    // 1) 删除（真实右键菜单 + 二次确认）。
    delete_note_via_menu(&host, cx, &seeded.node_id);
    assert!(
        visible_titles(&vault).is_empty(),
        "删除后侧栏不得再有该笔记"
    );
    assert_eq!(notes_deleted(&vault, &seeded.note_id), 1);
    assert_eq!(
        trashed_notes(&vault).expect("读回收站失败").len(),
        1,
        "删除后回收站必须有一行"
    );

    // 2) 打开回收站：行/标题/恢复按钮存在，空态不存在。
    click_selector(cx, "ribbon-trash");
    assert!(cx.debug_bounds("trash-panel").is_some());
    assert!(
        cx.debug_bounds("trash-empty").is_none(),
        "有删除笔记时不得显示空态"
    );
    assert!(cx.debug_bounds("trash-row-0").is_some());
    assert!(cx.debug_bounds("trash-restore-0").is_some());
    assert!(cx.debug_bounds("trash-close").is_some());
    let panel = trash_panel(&host, cx);
    assert_eq!(cx.update(|_window, app| panel.read(app).row_count()), 1);
    assert_eq!(
        cx.update(|_window, app| panel.read(app).row_title(0).map(str::to_owned)),
        Some("回收站笔记".to_owned())
    );
    // 点击行本身不打开（被删除的笔记没有正文可开），也不关闭覆盖层。
    click_selector(cx, "trash-row-0");
    assert!(
        cx.debug_bounds("trash-panel").is_some(),
        "点行本身不得关闭回收站"
    );
    assert_eq!(
        cx.update(|_window, app| host
            .read(app)
            .shell
            .read(app)
            .active_note_id()
            .map(str::to_owned)),
        None,
        "点行本身不得打开已删除的笔记"
    );

    // 3) 恢复：关闭覆盖层、侧栏出现、选中恢复节点、不自动打开正文。
    click_selector(cx, "trash-restore-0");
    assert!(
        cx.debug_bounds("trash-panel").is_none(),
        "恢复成功后必须关闭回收站"
    );
    assert_eq!(
        restored_notice(&host, cx).as_deref(),
        Some("已恢复"),
        "恢复成功必须提示「已恢复」"
    );
    assert_eq!(visible_titles(&vault), vec!["回收站笔记".to_owned()]);
    assert_eq!(
        cx.update(|_window, app| host
            .read(app)
            .shell
            .read(app)
            .selected_node()
            .map(str::to_owned)),
        Some(seeded.node_id.clone()),
        "恢复后必须选中恢复的节点"
    );
    assert_eq!(
        cx.update(|_window, app| host
            .read(app)
            .shell
            .read(app)
            .active_note_id()
            .map(str::to_owned)),
        None,
        "恢复不得自动打开正文"
    );
    assert_eq!(
        notes_deleted(&vault, &seeded.note_id),
        0,
        "恢复必须把 notes.deleted 置回 0"
    );

    // 4) 重新打开正文：内容与删除前一致（正文从未被删除）。
    let tree = list_tree_sorted(&vault, TreeSort::NameAsc).expect("读结构树失败");
    let entry = tree
        .iter()
        .find(|entry| entry.note_id.as_deref() == Some(seeded.note_id.as_str()))
        .expect("恢复的笔记必须回到树里");
    assert!(
        entry.parent_id.is_none(),
        "原父级是根级，恢复后必须仍是根级"
    );
    click_selector(cx, "tree-row-0");
    let source = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_editor_view()
            .expect("点击侧栏后必须打开正文")
            .read(app)
            .source_text()
    });
    assert_eq!(source, "回收站正文\n", "恢复后正文必须与删除前一致");
    let (body, _) = load_body_doc(&vault, &seeded.body_doc_id, 1, None).expect("重放正文失败");
    assert_eq!(
        body.text(),
        "回收站正文\n",
        "恢复不碰正文：body doc 内容必须与删除前一致"
    );

    // 5) 再次打开回收站：应为空。
    click_selector(cx, "ribbon-trash");
    let panel = trash_panel(&host, cx);
    assert_eq!(cx.update(|_window, app| panel.read(app).row_count()), 0);
    assert!(
        cx.debug_bounds("trash-empty").is_some(),
        "没有删除笔记时必须显示空态"
    );
}

#[gpui::test]
async fn root_and_folder_notes_are_listed_and_folder_parent_is_kept(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("folder");
    let (child, folder_id) = seed_folder_note(&vault, "项目", "子笔记", "子正文\n");
    let root = seed_note(&vault, "根笔记", "根正文\n");
    let (host, cx) = open_shell(cx, vault.clone());

    // 两个都删（文件夹内笔记恢复后必须仍在原文件夹下）。
    delete_note_via_menu(&host, cx, &child.node_id);
    delete_note_via_menu(&host, cx, &root.node_id);
    assert_eq!(trashed_notes(&vault).expect("读回收站失败").len(), 2);

    click_selector(cx, "ribbon-trash");
    let panel = trash_panel(&host, cx);
    assert_eq!(cx.update(|_window, app| panel.read(app).row_count()), 2);
    assert!(cx.debug_bounds("trash-row-0").is_some());
    assert!(cx.debug_bounds("trash-row-1").is_some());
    let child_row = cx.update(|_window, app| {
        (0..panel.read(app).row_count())
            .find(|index| panel.read(app).row_title(*index) == Some("子笔记"))
            .expect("回收站必须列出文件夹内笔记")
    });
    assert_eq!(
        cx.update(|_window, app| panel
            .read(app)
            .row_parent_label(child_row)
            .map(str::to_owned)),
        Some("项目".to_owned()),
        "行的父级路径必须显示所在文件夹"
    );

    // 恢复文件夹内笔记：面板关闭，节点仍在原文件夹下，根笔记仍留在回收站。
    click_selector(cx, trash_restore_selector(child_row));
    assert!(cx.debug_bounds("trash-panel").is_none());
    let tree = list_tree_sorted(&vault, TreeSort::NameAsc).expect("读结构树失败");
    let entry = tree
        .iter()
        .find(|entry| entry.node_id == child.node_id)
        .expect("恢复的子笔记必须回到树里");
    assert_eq!(
        entry.parent_id.as_deref(),
        Some(folder_id.as_str()),
        "恢复只 ack 删除事件，父级必须不变"
    );
    assert_eq!(
        visible_titles(&vault),
        vec!["项目".to_owned(), "子笔记".to_owned()]
    );
    let remaining = trashed_notes(&vault).expect("读回收站失败");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].title, "根笔记");
    assert_eq!(
        cx.update(|_window, app| host
            .read(app)
            .shell
            .read(app)
            .active_note_id()
            .map(str::to_owned)),
        None,
        "恢复不自动打开正文"
    );

    // 再恢复根级笔记：父级必须是根。
    click_selector(cx, "ribbon-trash");
    click_selector(cx, "trash-restore-0");
    assert!(cx.debug_bounds("trash-panel").is_none());
    let tree = list_tree_sorted(&vault, TreeSort::NameAsc).expect("读结构树失败");
    let entry = tree
        .iter()
        .find(|entry| entry.node_id == root.node_id)
        .expect("恢复的根笔记必须回到树里");
    assert!(entry.parent_id.is_none(), "根级笔记恢复后必须仍在根级");
    assert!(trashed_notes(&vault).expect("读回收站失败").is_empty());
}

#[gpui::test]
async fn empty_trash_shows_empty_state(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("empty");
    let (host, cx) = open_shell(cx, vault);

    click_selector(cx, "ribbon-trash");
    let panel = trash_panel(&host, cx);
    assert_eq!(cx.update(|_window, app| panel.read(app).row_count()), 0);
    assert!(
        cx.debug_bounds("trash-empty").is_some(),
        "空回收站必须显示 `trash-empty`"
    );
    assert!(cx.debug_bounds("trash-list").is_some());
    assert!(cx.debug_bounds("trash-close").is_some());

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("trash-panel").is_none());
}

#[gpui::test]
async fn restore_failure_keeps_rows_and_shows_inline_error(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("failure");
    let seeded = seed_note(&vault, "失败测试", "正文\n");
    let (host, cx) = open_shell(cx, vault.clone());
    delete_note_via_menu(&host, cx, &seeded.node_id);
    click_selector(cx, "ribbon-trash");
    let panel = trash_panel(&host, cx);
    assert_eq!(cx.update(|_window, app| panel.read(app).row_count()), 1);

    // 注入真实的提交失败：结构 update 写不进 `loro_updates`（A 事务回滚）。
    vault
        .exec_batch(
            "CREATE TRIGGER headless_trash_restore_fail \
             BEFORE INSERT ON loro_updates WHEN NEW.doc_id = 'structure' \
             BEGIN SELECT RAISE(ABORT, 'headless trash restore failure'); END;",
        )
        .expect("创建注入触发器失败");

    click_selector(cx, "trash-restore-0");
    assert!(
        cx.debug_bounds("trash-panel").is_some(),
        "恢复失败不得关闭覆盖层"
    );
    assert_eq!(
        cx.update(|_window, app| panel.read(app).row_count()),
        1,
        "恢复失败不得丢行"
    );
    assert!(
        cx.debug_bounds("trash-row-0").is_some(),
        "恢复失败后行必须仍然可见"
    );
    assert!(
        cx.debug_bounds("trash-error").is_some(),
        "恢复失败必须显示 inline 错误"
    );
    let error = cx
        .update(|_window, app| panel.read(app).error_text().map(str::to_owned))
        .expect("面板必须记录错误详情");
    assert!(!error.is_empty(), "inline 错误不得为空：{error:?}");
    assert!(
        visible_titles(&vault).is_empty(),
        "失败的恢复不得把笔记投影出来"
    );
    assert_eq!(notes_deleted(&vault, &seeded.note_id), 1);

    // 去掉故障后重试：同一条列表还能成功恢复（证明失败没有破坏面板状态）。
    vault
        .exec_batch("DROP TRIGGER headless_trash_restore_fail")
        .expect("删除注入触发器失败");
    click_selector(cx, "trash-restore-0");
    assert!(cx.debug_bounds("trash-panel").is_none());
    assert_eq!(visible_titles(&vault), vec!["失败测试".to_owned()]);
    assert_eq!(notes_deleted(&vault, &seeded.note_id), 0);
}

#[gpui::test]
async fn close_paths_return_focus_and_selectors_exist(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("close");
    seed_note(&vault, "焦点测试", "原始正文\n");
    let (host, cx) = open_shell(cx, vault);

    let editor = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_editor_view()
            .expect("启动后必须有编辑器")
    });
    let editor_focused = |cx: &mut VisualTestContext| {
        cx.update(|window, app| window.focused(app) == Some(editor.read(app).focus_handle(app)))
    };
    assert!(editor_focused(cx), "启动后焦点应在编辑器");

    // selector 存在性：ribbon 按钮 + 兼容别名 + 关闭按钮。
    click_selector(cx, "ribbon-trash");
    assert!(cx.debug_bounds("ribbon-trash").is_some());
    assert!(
        cx.debug_bounds("sidebar-trash").is_some(),
        "兼容别名 `sidebar-trash` 必须同时存在"
    );
    assert!(cx.debug_bounds("trash-panel").is_some());
    assert!(cx.debug_bounds("trash-close").is_some());
    assert!(!editor_focused(cx), "打开回收站必须抢走编辑器焦点");

    // 打开时输入不得落进正文。
    let before = cx.update(|_window, app| editor.read(app).source_text());
    cx.simulate_input("不应写入正文");
    cx.run_until_parked();
    assert_eq!(
        cx.update(|_window, app| editor.read(app).source_text()),
        before,
        "覆盖层打开时正文不得被键入修改"
    );

    // Esc 关闭 → 焦点还给编辑器。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("trash-panel").is_none(),
        "Esc 必须关闭回收站"
    );
    assert!(editor_focused(cx), "Esc 关闭后焦点必须回到编辑器");
    cx.simulate_input("回到正文");
    cx.run_until_parked();
    assert!(
        cx.update(|_window, app| editor.read(app).source_text())
            .contains("回到正文"),
        "关闭后键入必须进正文（焦点已回归）"
    );

    // 点遮罩关闭。
    click_selector(cx, "ribbon-trash");
    assert!(cx.debug_bounds("trash-panel").is_some());
    cx.simulate_click(point(px(4.0), px(4.0)), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("trash-panel").is_none(),
        "点遮罩必须关闭回收站"
    );
    assert!(editor_focused(cx), "点遮罩关闭后焦点必须回到编辑器");

    // 关闭按钮关闭。
    click_selector(cx, "ribbon-trash");
    assert!(cx.debug_bounds("trash-panel").is_some());
    click_selector(cx, "trash-close");
    assert!(
        cx.debug_bounds("trash-panel").is_none(),
        "关闭按钮必须关闭回收站"
    );
    assert!(editor_focused(cx), "关闭按钮关闭后焦点必须回到编辑器");
}
