//! Wiki L2：笔记重命名联动改写（headless，真实 `AppShell` + 临时 vault）。
//!
//! 覆盖两条最容易出 stale 覆盖的路径：
//! 1. 当前活动笔记本身就是来源：必须走活动 `EditorView` 的 driver，不能另开会话；
//! 2. 当前活动笔记是目标：非活动来源走 storage 的逐篇 `SessionDriver`，之后切换正常。

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, MouseButton, ParentElement as _,
    Styled as _, TestAppContext,
};
use menghunote_app::shell::AppShell;
use menghunote_storage::{
    append_updates, list_tree_sorted, load_body_doc, materialize_body, StructureWriter, TreeSort,
    UpdateRecord, Vault,
};
use tempfile::{Builder as TempDirBuilder, TempDir};

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

struct Seeded {
    note_id: String,
    doc_id: String,
}

fn temp_vault() -> (TempDir, Vault) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-wiki-rename-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
    let dir = TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("创建临时 vault 失败");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开 vault 失败");
    (dir, vault)
}

/// 建一篇笔记 + 写正文 + 物化 links（重命名联动依赖 `links` 派生表）。
fn seed_note(vault: &Vault, title: &str, text: &str) -> Seeded {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer.doc().create_note(title, 1).expect("创建笔记失败");
    writer
        .commit("headless-wiki-rename-seed")
        .expect("结构提交失败");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    let head = append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-wiki-rename", update)],
    )
    .expect("写入正文失败");
    materialize_body(vault, &seed.note_id, &seed.body_doc_id, head.head_seq, 1)
        .expect("物化正文失败");
    Seeded {
        note_id: seed.note_id,
        doc_id: seed.body_doc_id,
    }
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut gpui::VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-wiki-rename".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn active_view(
    cx: &mut gpui::VisualTestContext,
    host: &Entity<ShellHost>,
) -> Entity<menghunote_app::editor::view::EditorView> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_editor_view()
            .expect("活动视图")
    })
}

fn view_text(
    cx: &mut gpui::VisualTestContext,
    view: &Entity<menghunote_app::editor::view::EditorView>,
) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn shell_notice(cx: &mut gpui::VisualTestContext, host: &Entity<ShellHost>) -> Option<String> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .notice_text()
            .map(str::to_owned)
    })
}

fn tree_row(vault: &Vault, title: &str) -> usize {
    list_tree_sorted(vault, TreeSort::NameAsc)
        .expect("读取结构树失败")
        .iter()
        .position(|entry| entry.title == title)
        .unwrap_or_else(|| panic!("结构树里找不到「{title}」"))
}

fn click_tree_row(cx: &mut gpui::VisualTestContext, index: usize) {
    let selector: &'static str = Box::leak(format!("tree-row-{index}").into_boxed_str());
    let row = cx.debug_bounds(selector).expect("树行必须有 selector");
    cx.simulate_click(row.center(), gpui::Modifiers::default());
    cx.run_until_parked();
}

/// 右键 → 菜单「重命名」→ 输入 → Enter（与用户路径一致）。
fn rename_via_sidebar(cx: &mut gpui::VisualTestContext, index: usize, title: &str) {
    let selector: &'static str = Box::leak(format!("tree-row-{index}").into_boxed_str());
    let row = cx.debug_bounds(selector).expect("树行必须有 selector");
    cx.simulate_mouse_down(row.center(), MouseButton::Right, gpui::Modifiers::default());
    cx.run_until_parked();
    let rename = cx
        .debug_bounds("menu-item-0")
        .expect("笔记菜单第一项是重命名");
    cx.simulate_click(rename.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.simulate_input(title);
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
}

fn body_text(vault: &Vault, doc_id: &str) -> String {
    load_body_doc(vault, doc_id, 1, None)
        .expect("重放正文失败")
        .0
        .text()
}

fn link_rows(vault: &Vault, from_note_id: &str) -> Vec<(String, Option<String>)> {
    let rows = vault
        .query(
            "SELECT target, to_note_id FROM links WHERE from_note_id = ?1 ORDER BY start_byte",
            &[menghunote_storage::Value::Text(from_note_id.to_owned())],
        )
        .expect("查 links 失败");
    rows.into_iter()
        .map(|row| {
            let target = match &row[0] {
                menghunote_storage::Value::Text(text) => text.clone(),
                other => panic!("target 读回异常：{other:?}"),
            };
            let to_note_id = match &row[1] {
                menghunote_storage::Value::Text(text) => Some(text.clone()),
                menghunote_storage::Value::Null => None,
                other => panic!("to_note_id 读回异常：{other:?}"),
            };
            (target, to_note_id)
        })
        .collect()
}

#[gpui::test]
async fn renaming_target_rewrites_active_source_without_stale_overwrite(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    // 先建目标（来源物化 links 时它必须已经在结构树里，否则 to_note_id 为 NULL），
    // 再建来源并切过去：活动笔记 = 来源。
    let target = seed_note(&vault, "目标", "目标正文");
    let source = seed_note(&vault, "来源", "[[目标]] 尾");
    let (host, cx) = open_shell(cx, vault.clone());

    click_tree_row(cx, tree_row(&vault, "来源"));
    let view = active_view(cx, &host);
    assert_eq!(view_text(cx, &view), "[[目标]] 尾");

    // 先在活动笔记里产生一次**尚未 flush** 的本地编辑：重命名联动如果另开会话
    // 拿旧快照写正文，就会把这个字符覆盖掉。
    let pane = cx.debug_bounds("editor-pane").expect("编辑器面板");
    cx.simulate_click(pane.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    cx.simulate_keystrokes("a");
    cx.run_until_parked();
    let typed = active_view(cx, &host);
    assert!(
        view_text(cx, &typed).ends_with("a"),
        "前置态：活动来源应已有未落库的 'a'：{:?}",
        view_text(cx, &typed)
    );

    let target_row = tree_row(&vault, "目标");
    rename_via_sidebar(cx, target_row, "新目标");

    let after = active_view(cx, &host);
    assert_eq!(
        view_text(cx, &after),
        "[[新目标]] 尾a",
        "活动来源必须经同一个 driver 改写，且在途编辑不能被 stale 快照覆盖"
    );
    assert_eq!(
        body_text(&vault, &source.doc_id),
        "[[新目标]] 尾a",
        "正文必须 durable"
    );
    assert_eq!(
        link_rows(&vault, &source.note_id),
        vec![("新目标".to_owned(), Some(target.note_id.clone()))],
        "links 必须按新正文物化，to_note_id 仍指向被重命名笔记"
    );

    let notice = shell_notice(cx, &host).expect("重命名后必须有提示");
    assert!(
        notice.contains("更新 1 篇 / 1 条链接"),
        "提示必须显示更新篇数/链接数：{notice:?}"
    );
    assert!(
        notice.contains("失败 0 篇"),
        "提示必须显示失败篇数：{notice:?}"
    );
}

#[gpui::test]
async fn renaming_active_target_rewrites_sources_and_switching_still_works(
    cx: &mut TestAppContext,
) {
    let (_dir, vault) = temp_vault();
    // 先建目标 → 启动时打开目标；来源是非活动笔记。
    let target = seed_note(&vault, "目标", "目标正文");
    let source = seed_note(&vault, "来源", "[[目标|别名]] 与 [[目标#标题]]");
    let (host, cx) = open_shell(cx, vault.clone());

    // 启动时打开的是排序后的第一篇笔记（「来源」<「目标」），先把活动文档切到目标。
    click_tree_row(cx, tree_row(&vault, "目标"));
    let target_view = active_view(cx, &host);
    assert_eq!(view_text(cx, &target_view), "目标正文");

    // 用 F2 重命名选中行（走产品里的真实入口；避免右键菜单 150ms 手势余波）。
    let pane = cx.debug_bounds("editor-pane").expect("编辑器面板");
    cx.simulate_click(pane.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    let notice = shell_notice(cx, &host).expect("重命名输入前必须提示联动数量");
    assert!(
        notice.contains("将同时更新 2 条链接"),
        "重命名前必须显示预计更新条数：{notice:?}"
    );
    cx.simulate_input("新目标");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    let notice = shell_notice(cx, &host).expect("重命名后必须有提示");
    assert!(
        notice.contains("更新 1 篇 / 2 条链接"),
        "非活动来源必须逐篇改写：{notice:?}"
    );
    assert!(
        notice.contains("失败 0 篇"),
        "提示必须显示失败篇数：{notice:?}"
    );

    // 切到来源：正文已改写，alias/heading 逐字保留。
    click_tree_row(cx, tree_row(&vault, "来源"));
    let source_view = active_view(cx, &host);
    assert_eq!(
        view_text(cx, &source_view),
        "[[新目标|别名]] 与 [[新目标#标题]]"
    );
    assert_eq!(
        body_text(&vault, &source.doc_id),
        "[[新目标|别名]] 与 [[新目标#标题]]"
    );
    let rows = link_rows(&vault, &source.note_id);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|row| row.0 == "新目标"));
    assert!(rows
        .iter()
        .all(|row| row.1.as_deref() == Some(target.note_id.as_str())));

    // 再切回目标：目标正文不变，重命名后仍可正常打开。
    click_tree_row(cx, tree_row(&vault, "新目标"));
    let target_view = active_view(cx, &host);
    assert_eq!(view_text(cx, &target_view), "目标正文");
}
