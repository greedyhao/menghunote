//! Wiki 双链 L1：解析 → 点击事件 → 宿主按标题解析并打开目标。

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, ParentElement as _, Styled as _,
    TestAppContext,
};
use menghunote_app::shell::AppShell;
use menghunote_editor::SourceByte;
use menghunote_storage::{
    append_updates, list_tree_sorted, load_body_doc, StructureWriter, TreeSort, UpdateRecord, Vault,
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

fn temp_vault() -> (TempDir, Vault) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-wiki-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
    let dir = TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("创建临时 vault 失败");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开 vault 失败");
    (dir, vault)
}

fn seed_note(vault: &Vault, title: &str, text: &str) {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer.doc().create_note(title, 1).expect("创建笔记失败");
    writer.commit("headless-wiki-seed").expect("结构提交失败");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-wiki", update)],
    )
    .expect("写入正文失败");
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut gpui::VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-wiki".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

/// 侧栏（目录优先 + 文件名 A-Z）里标题对应的先序下标。
fn tree_row(vault: &Vault, title: &str) -> usize {
    list_tree_sorted(vault, TreeSort::NameAsc)
        .expect("读取结构树失败")
        .iter()
        .position(|entry| entry.title == title)
        .unwrap_or_else(|| panic!("结构树里找不到「{title}」"))
}

/// 点侧栏某一行（真实鼠标事件；启动时打开的是排序后的第一篇，不一定是要测的那篇）。
fn click_tree_row(cx: &mut gpui::VisualTestContext, index: usize) {
    let selector: &'static str = Box::leak(format!("tree-row-{index}").into_boxed_str());
    let row = cx.debug_bounds(selector).expect("树行必须有 selector");
    cx.simulate_click(row.center(), gpui::Modifiers::default());
    cx.run_until_parked();
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

#[gpui::test]
async fn wiki_click_opens_resolved_note(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    seed_note(&vault, "来源", "[[目标]]");
    seed_note(&vault, "目标", "目标正文");
    let (host, cx) = open_shell(cx, vault);

    let view = active_view(cx, &host);
    assert_eq!(view_text(cx, &view), "[[目标]]");
    view.update(cx, |view, cx| {
        let (target, heading) = view
            .wiki_link_at_source(SourceByte::new(3))
            .expect("byte 3 应落在链接内");
        assert_eq!(target, "目标");
        assert!(heading.is_none());
        let ranges = view.wiki_display_ranges();
        assert_eq!(ranges.len(), 1);
        let (start, end, _, _) = &ranges[0];
        assert!(view.wiki_link_at_display(*start).is_some());
        assert!(
            view.wiki_link_at_display(*end).is_none(),
            "链接末尾边界不能算命中（点末尾应显示源码/移动 caret）"
        );
        view.request_wiki_link(target, heading, cx);
    });
    cx.run_until_parked();

    let after = active_view(cx, &host);
    assert_eq!(
        view_text(cx, &after),
        "目标正文",
        "点击 wiki 链接必须切到目标笔记"
    );
}

#[gpui::test]
async fn unresolved_wiki_link_does_not_create_or_switch(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    seed_note(&vault, "来源", "[[不存在]]");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    view.update(cx, |view, cx| {
        let (target, heading) = view
            .wiki_link_at_source(SourceByte::new(3))
            .expect("byte 3 应落在链接内");
        view.request_wiki_link(target, heading, cx);
    });
    cx.run_until_parked();

    let after = active_view(cx, &host);
    assert_eq!(
        view_text(cx, &after),
        "[[不存在]]",
        "unresolved 不得自动建笔记/切换"
    );
    let notice = shell_notice(cx, &host);
    assert!(
        notice
            .as_deref()
            .is_some_and(|notice| notice.contains("未找到")),
        "必须给明确提示：{notice:?}"
    );
}

#[gpui::test]
async fn ordinary_link_click_opens_external_url(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    seed_note(&vault, "来源", "[链接](https://example.com/x)");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    cx.update(|_window, app| {
        let view = view.read(app);
        let ranges = view.external_link_display_ranges();
        assert_eq!(ranges.len(), 1);
        let (start, end, url) = ranges[0].clone();
        assert_eq!(url, "https://example.com/x");
        assert!(view.external_link_at_display(start).is_some());
        assert!(
            view.external_link_at_display(end).is_none(),
            "链接末尾边界不能触发跳转"
        );
    });
    view.update(cx, |view, cx| {
        view.request_external_link("https://example.com/x".to_owned(), cx);
    });
    cx.run_until_parked();
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/x"),
        "普通链接单击必须交给系统打开"
    );
}

#[gpui::test]
async fn backlinks_panel_lists_source_and_opens_it(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    seed_note(&vault, "目标", "目标正文");
    seed_note(&vault, "来源", "[[目标]]");
    let rows = vault
        .query("SELECT note_id, title FROM tree WHERE deleted=0", &[])
        .expect("查树");
    let mut ids = std::collections::HashMap::new();
    for row in rows {
        let (menghunote_storage::Value::Text(id), menghunote_storage::Value::Text(title)) =
            (&row[0], &row[1])
        else {
            panic!("tree 列类型异常");
        };
        ids.insert(title.clone(), id.clone());
    }
    vault
        .exec(
            "INSERT INTO links(from_note_id,start_byte,end_byte,to_note_id,target,kind,body_seq) \
             VALUES(?1,0,8,?2,'目标','wiki',1)",
            &[
                menghunote_storage::Value::Text(ids["来源"].clone()),
                menghunote_storage::Value::Text(ids["目标"].clone()),
            ],
        )
        .expect("插入反向链接");

    let (host, cx) = open_shell(cx, vault.clone());
    // 启动时打开的是排序后的第一篇笔记（「来源」U+6765 排在「目标」U+76EE 之前），
    // 所以显式点开「目标」再断言。
    click_tree_row(cx, tree_row(&vault, "目标"));
    let target_view = active_view(cx, &host);
    assert_eq!(view_text(cx, &target_view), "目标正文");
    let toggle = cx
        .debug_bounds("backlinks-toggle")
        .expect("状态栏必须有反向链接入口");
    cx.simulate_click(toggle.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("backlinks-panel").is_some());
    assert!(cx.debug_bounds("backlink-0").is_some());
    let row = cx.debug_bounds("backlink-0").expect("来源行");
    cx.simulate_click(row.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    let source_view = active_view(cx, &host);
    assert_eq!(view_text(cx, &source_view), "[[目标]]");
}
