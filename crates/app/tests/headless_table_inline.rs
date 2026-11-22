//! ADR 0019 S3b7：app headless「表格单元格行内 Markdown」集成测试。
//!
//! 真实 `AppShell` + 临时 vault（`CARGO_TARGET_TMPDIR`、不 sleep）+ 真实 `#[gpui::test]`
//! 窗口。断言的是**绘制同源**的数据（`table_cell_render` 直接读本帧 `TableCellPaint`）：
//!
//! 1. 渲染：`**粗**` / `*斜*` / `` `代码` `` / `~~删~~` 的标记被隐藏、各自成 run
//!    （不同字体 id），整表仍是 Object（display 里没有表格源码）；
//! 2. 编辑：单击 cell 的浮层显示**源码原文**（`**`、`[..](..)` 都在），渲染文本隐藏；
//! 3. 提交：无改动提交逐字节不变；改一格不影响其它格的行内 Markdown；
//! 4. 链接：点 cell 内的链接**打开**（`cx.opened_url()`），不进入单元格编辑；
//!    非 http/https/mailto 明确拒绝；
//! 5. 结构：`\|` 在行内代码里仍是字面 `|`（列数不变）；加行/加列后行内 Markdown 仍渲染；
//!    `Ctrl+Z` 回到原表（一次编辑一个撤销组）。
//!
//! 像素级外观（粗体字重、代码底色、链接下划线）仍必须真机验证；这里证明
//! 「解析 → shaping → 命中 → 提交」整条链路与结构不回退。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::view::{EditorView, TableCellRenderSnapshot};
use menghunote_app::shell::AppShell;
use menghunote_editor::StyleKind;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 窗口根视图（给 `AppShell` 一个稳定 selector）。
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
        .join(format!("headless-table-inline-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 在空 vault 里建一篇正文（结构 CRDT → 投影 → 正文 A 事务）。
fn seed_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-table-inline-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-table-inline", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-table-inline".to_owned(), cx));
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

fn display_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).display_text())
}

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn is_editing_cell(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_window, app| view.read(app).is_editing_table_cell())
}

fn selected_cell(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> Option<(usize, usize, usize)> {
    cx.update(|_window, app| view.read(app).selected_table_cell())
}

fn draft_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_window, app| view.read(app).table_cell_edit_draft().map(str::to_owned))
}

/// 某个单元格的**本帧绘制快照**（display 文本 + 逐 run 字体 + 可见链接）。
fn render(
    view: &Entity<EditorView>,
    row: usize,
    column: usize,
    cx: &mut VisualTestContext,
) -> TableCellRenderSnapshot {
    cx.update(|_window, app| {
        view.read(app)
            .table_cell_render(TABLE_LINE, row, column)
            .unwrap_or_else(|| panic!("单元格（row={row} col={column}）必须在本帧绘制里"))
    })
}

/// 单个可见表格快照（不存在就失败，避免假通过）。
fn table_of(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> menghunote_app::editor::view::TableSnapshot {
    let snapshots = cx.update(|_window, app| view.read(app).table_snapshots());
    snapshots
        .into_iter()
        .next()
        .expect("必须解析出整块表格对象")
}

/// 第 `row` 行（0 = 表头）第 `column` 列单元格的窗口矩形（不可见直接失败）。
fn cell_rect(
    line: usize,
    row: usize,
    column: usize,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> gpui::Bounds<gpui::Pixels> {
    cx.update(|_window, app| {
        view.read(app)
            .table_cell_rects()
            .into_iter()
            .find(|(object_line, r, c, _)| *object_line == line && *r == row && *c == column)
            .map(|(_, _, _, rect)| rect)
            .unwrap_or_else(|| panic!("单元格（行 {line} 的 {row},{column}）必须可见"))
    })
}

/// 点击一个单元格（单击 = 选中并**直接进入原地编辑**）。
fn click_cell(
    line: usize,
    row: usize,
    column: usize,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) {
    let rect = cell_rect(line, row, column, view, cx);
    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 点击一个 debug selector 目标（找不到就失败：按钮必须真实画出来）。
fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 悬浮到表格对象中心（不点选，只让浮层出现）。
fn hover_table(line: usize, view: &Entity<EditorView>, cx: &mut VisualTestContext) {
    let rect = cx
        .update(|_window, app| {
            view.read(app)
                .object_rects()
                .into_iter()
                .find(|(object_line, _)| *object_line == line)
                .map(|(_, rect)| rect)
        })
        .unwrap_or_else(|| panic!("对象行 {line} 必须可见"));
    cx.simulate_mouse_move(rect.center(), None, Modifiers::default());
    cx.run_until_parked();
}

/// 表格表头行号（0 起）。
const TABLE_LINE: usize = 2;

/// 2 列 / 2 条数据行，每格都带行内 Markdown。
const TABLE_SOURCE: &str = "# 表格\n\n| 名称 | 说明 |\n|:--|--:|\n| **粗** *斜* | `代码` ~~删~~ |\n| [文档](https://example.com/a) | 平 |\n\n后文\n";

#[gpui::test]
async fn cells_render_inline_markdown_with_styles_and_hidden_markers(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "行内 Markdown 渲染", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 整表仍是 Object：display 里没有表格源码（更没有 `**` 标记）。
    let display = display_of(&view, cx);
    assert!(!display.contains("**粗**"), "标记必须隐藏：{display:?}");
    assert!(!display.contains("~~删~~"), "标记必须隐藏：{display:?}");
    assert!(
        !display.contains("](https://"),
        "链接目标不得出现在正文：{display:?}"
    );

    // `**粗** *斜*`：标记隐藏，样式段精确覆盖内容（粗体 / 空白 / 斜体）。
    let bold = render(&view, 1, 0, cx);
    assert_eq!(bold.display, "粗 斜", "只画内容，不画标记");
    assert_eq!(
        bold.spans,
        vec![
            (0..3, StyleKind::Bold),
            (3..4, StyleKind::Plain),
            (4..7, StyleKind::Emphasis),
        ],
        "粗体/斜体的语义段必须与绘制同源"
    );
    assert!(bold.links.is_empty(), "粗体/斜体不是链接");

    // `` `代码` ~~删~~ ``：行内代码 + 删除线。
    let code = render(&view, 1, 1, cx);
    assert_eq!(code.display, "代码 删");
    assert_eq!(
        code.spans,
        vec![
            (0..6, StyleKind::InlineCode),
            (6..7, StyleKind::Plain),
            (7..10, StyleKind::Strike),
        ]
    );
    assert!(code.links.is_empty());

    // 链接格：display 只有可见文本，链接区间指向它。
    let link = render(&view, 2, 0, cx);
    assert_eq!(link.display, "文档");
    assert_eq!(link.links.len(), 1);
    assert_eq!(link.links[0].display, 0.."文档".len());
    assert_eq!(link.links[0].dest, "https://example.com/a");

    // 普通文本格：单段 Plain、无链接。
    let plain = render(&view, 2, 1, cx);
    assert_eq!(plain.display, "平");
    assert_eq!(plain.spans, vec![(0..3, StyleKind::Plain)]);
    assert!(plain.links.is_empty());

    // 表格结构不变。
    let table = table_of(&view, cx);
    assert_eq!((table.columns, table.rows), (2, 2));

    close_shell(&host, cx);
}

#[gpui::test]
async fn clicking_a_cell_link_opens_it_instead_of_editing_the_cell(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "单元格链接点击", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    let link = render(&view, 2, 0, cx).links[0].clone();
    cx.simulate_click(link.rect.center(), Modifiers::default());
    cx.run_until_parked();

    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/a"),
        "单击单元格内的链接必须打开它"
    );
    assert!(
        !is_editing_cell(&view, cx),
        "点链接不能进入单元格编辑（否则链接永远点不开）"
    );
    assert_eq!(
        selected_cell(&view, cx),
        None,
        "点链接不选中单元格（与正文普通链接同一套语义）"
    );
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "打开链接不改正文");

    // 链接**末尾边界**不算命中（与正文链接一致）：点链接右侧的空白仍是单元格。
    let cell = cell_rect(TABLE_LINE, 2, 0, &view, cx);
    cx.simulate_click(
        point(
            link.rect.origin.x + link.rect.size.width + px(2.0),
            cell.center().y,
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(
        is_editing_cell(&view, cx),
        "链接之后（同一格内）的点击仍然进入单元格编辑"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn file_links_require_confirmation_in_table_cells(cx: &mut TestAppContext) {
    const SOURCE: &str =
        "# 表格\n\n| a | b |\n|---|---|\n| [文件](file:///etc/passwd) | 平 |\n\n后文\n";
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "非 Web 链接", SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    let link = cx
        .update(|_window, app| {
            view.read(app)
                .table_cell_render(TABLE_LINE, 1, 0)
                .expect("链接格必须可见")
        })
        .links
        .into_iter()
        .next()
        .expect("必须解析出链接");
    assert_eq!(link.dest, "file:///etc/passwd");
    cx.simulate_click(link.rect.center(), Modifiers::default());
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("editor-link-confirm").is_some(),
        "file: 打开前必须弹确认框"
    );
    assert_eq!(cx.opened_url(), None, "确认前不得打开");
    click_selector(cx, "editor-link-confirm-cancel");
    assert_eq!(cx.opened_url(), None, "取消后不得打开");
    assert!(
        cx.debug_bounds("editor-link-confirm").is_none(),
        "取消必须关闭确认框"
    );
    assert!(!is_editing_cell(&view, cx), "取消打开也不进入编辑");

    close_shell(&host, cx);
}

#[gpui::test]
async fn editing_a_cell_shows_raw_source_and_commits_byte_exact(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "编辑源码", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 单击 → 浮层显示**源码原文**（`**`、`*` 都在），渲染文本隐藏（不重影）。
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert!(is_editing_cell(&view, cx));
    assert_eq!(
        draft_of(&view, cx).as_deref(),
        Some("**粗** *斜*"),
        "浮层显示原始 cell 源码（不是渲染后的文本）"
    );
    assert_eq!(
        render(&view, 1, 0, cx).display,
        "",
        "编辑期间该格不画文字（输入框覆盖在上面）"
    );
    assert!(
        render(&view, 1, 1, cx).display == "代码 删",
        "其它格照常渲染行内 Markdown"
    );

    // Enter 提交（没改字）→ 源码逐字节不变（不会把标记重写/转义）。
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!is_editing_cell(&view, cx));
    assert_eq!(source_of(&view, cx), TABLE_SOURCE);
    assert_eq!(
        render(&view, 1, 0, cx).display,
        "粗 斜",
        "提交后仍渲染行内 Markdown"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn editing_one_cell_keeps_other_cells_inline_markdown(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "改一格不动其它格", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    click_cell(TABLE_LINE, 2, 1, &view, cx);
    assert_eq!(draft_of(&view, cx).as_deref(), Some("平"));
    cx.simulate_input("改");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "草稿不写正文");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "# 表格\n\n| 名称 | 说明 |\n|:--|--:|\n| **粗** *斜* | `代码` ~~删~~ |\n| [文档](https://example.com/a) | 平改 |\n\n后文\n",
        "只改被编辑的那一格，其它格的 Markdown 逐字节不变"
    );

    // 其它格的行内 Markdown 仍然渲染；被改的格变成普通文本。
    assert_eq!(render(&view, 1, 0, cx).display, "粗 斜");
    assert_eq!(render(&view, 1, 1, cx).display, "代码 删");
    assert_eq!(render(&view, 2, 0, cx).display, "文档");
    assert_eq!(render(&view, 2, 1, cx).display, "平改");
    assert_eq!(
        render(&view, 2, 1, cx).spans,
        vec![(0.."平改".len(), StyleKind::Plain)],
        "被改的格变成普通文本"
    );
    let table = table_of(&view, cx);
    assert_eq!((table.columns, table.rows), (2, 2), "表格结构不变");

    // 一次编辑 = 一个撤销组：Ctrl+Z 回到原表（行内 Markdown 原样）。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), TABLE_SOURCE, "撤销回到原表");
    assert_eq!(render(&view, 2, 1, cx).display, "平");

    close_shell(&host, cx);
}

#[gpui::test]
async fn escaped_pipe_inside_inline_code_stays_one_cell(cx: &mut TestAppContext) {
    // GFM：cell 里的字面 `|` 必须写 `\|`（**包括行内代码内部**），列数不变。
    const SOURCE: &str = "# 表格\n\n| a | b |\n|---|---|\n| `x\\|y` | 平 |\n\n后文\n";
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "行内代码里的管道", SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    let table = table_of(&view, cx);
    assert_eq!(table.columns, 2, "`\\|` 不是列边界");
    let cell = render(&view, 1, 0, cx);
    assert_eq!(cell.display, "x|y", "行内代码里的 `\\|` 还原成字面 `|`");
    assert_eq!(
        cell.spans,
        vec![(0..3, StyleKind::InlineCode)],
        "整格都是行内代码"
    );
    assert_eq!(render(&view, 1, 1, cx).display, "平");

    // 编辑该格：浮层显示源码（`\|` 原样），提交不改字节。
    click_cell(TABLE_LINE, 1, 0, &view, cx);
    assert_eq!(draft_of(&view, cx).as_deref(), Some("`x|y`"));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), SOURCE, "无改动提交逐字节不变");

    close_shell(&host, cx);
}

#[gpui::test]
async fn adding_rows_and_columns_keeps_inline_markdown_rendering(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "加行加列", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 悬浮表格 → 「+ 行」：新行 cell 为空，原格的行内 Markdown 仍渲染。
    hover_table(TABLE_LINE, &view, cx);
    click_selector(cx, "table-insert-row");
    let table = table_of(&view, cx);
    assert_eq!((table.columns, table.rows), (2, 3), "加一行");
    assert_eq!(render(&view, 1, 0, cx).display, "粗 斜");
    assert_eq!(render(&view, 3, 0, cx).display, "", "新行是空 cell");

    // 「+ 列」：表头/分隔行/所有数据行一起扩列，行内 Markdown 不受影响。
    hover_table(TABLE_LINE, &view, cx);
    click_selector(cx, "table-insert-column");
    let table = table_of(&view, cx);
    assert_eq!((table.columns, table.rows), (3, 3), "加一列");
    assert_eq!(render(&view, 1, 0, cx).display, "粗 斜");
    assert_eq!(render(&view, 1, 1, cx).display, "代码 删");
    assert_eq!(render(&view, 2, 0, cx).display, "文档");
    assert_eq!(render(&view, 1, 2, cx).display, "", "新列是空 cell");

    close_shell(&host, cx);
}
