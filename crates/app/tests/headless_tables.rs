//! ADR 0019 S3a：app headless 表格 Object Block 集成测试。
//!
//! 真实 `AppShell` + 临时 vault（`CARGO_TARGET_TMPDIR`、不 sleep）+ 真实 `#[gpui::test]`
//! 窗口：断言
//!
//! 1. 表格投影成对象（display 只有一个占位符，不出现 `|` 源码），表格之后的正文样式
//!    不被吞掉；
//! 2. 点击表格任意位置只选中整块（描边/工具条），caret 落在整块之后、绝不进单元格；
//! 3. 工具条只有「编辑源码」（无放大）；点击后显示整张表源码，`Esc` 恢复表格对象；
//! 4. 长表格按行裁剪；滚出视口后不绘制；滚动回来重新出现；
//! 5. 窄窗口不 panic（最小列宽 + 内容遮罩裁剪）；
//! 6. 删除表格是**整段源码一次删除**，并落库。
//!
//! 像素级外观（DPI、真实窗口）仍必须真机验证；这里证明“解析 → 投影 → 几何 → 命中 →
//! 工具条 → 源码切换 → 编辑事务”链路。

use std::path::Path;

use gpui::{
    div, point, px, size, AppContext as _, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, ScrollDelta, ScrollEvent, ScrollSource, Styled as _, TestAppContext,
    TouchPhase, VisualTestContext,
};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
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
        .join(format!("headless-tables-{}", std::process::id()));
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
    writer.commit("headless-tables-seed").expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-tables", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-tables".to_owned(), cx));
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

fn caret_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> (usize, usize) {
    cx.update(|_window, app| {
        let selection = view.read(app).selection();
        (selection.anchor().get(), selection.head().get())
    })
}

/// 当前可见 Object（图片/表格）的窗口矩形；没有该行时直接失败（避免“点空”假通过）。
fn object_rect(
    line: usize,
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> gpui::Bounds<gpui::Pixels> {
    cx.update(|_window, app| {
        view.read(app)
            .object_rects()
            .into_iter()
            .find(|(object_line, _)| *object_line == line)
            .map(|(_, rect)| rect)
            .unwrap_or_else(|| panic!("对象行 {line} 必须可见"))
    })
}

/// 编辑器正文区中心点（滚动事件必须落在元素 bounds 内）。
fn pane_center(cx: &mut VisualTestContext) -> gpui::Point<gpui::Pixels> {
    cx.debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来")
        .center()
}

/// 一次真实滚轮事件（正 delta = 滚轮向上，与 `scroll_by` 的符号约定一致）。
fn scroll(cx: &mut VisualTestContext, delta_y: f32) {
    let position = pane_center(cx);
    cx.simulate_event(ScrollEvent {
        position,
        delta: ScrollDelta::Pixels(point(px(0.0), px(delta_y))),
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
        source: ScrollSource::Pointer,
    });
    cx.run_until_parked();
}

/// 3 列 / 2 行的表格（含对齐分隔行）+ 前后正文。
const TABLE_SOURCE: &str = "# 表格\n\n| 名称 | 数量 | 备注 |\n|:--|:-:|--:|\n| 苹果 | 3 | 红色 |\n| 香蕉 | 5 | 黄色 |\n\n后文\n";
/// 表格表头行号（0 起）。
const TABLE_LINE: usize = 2;

#[gpui::test]
async fn table_object_renders_selects_and_toggles_source(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "表格笔记", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let placeholder = menghunote_editor::OBJECT_PLACEHOLDER;

    // 1) 投影：整表是一个占位符；表头/分隔行/数据行源码都不可见；表后正文原样可读。
    let display = display_of(&view, cx);
    assert!(
        display.contains(placeholder),
        "整表必须投影成对象占位符：{display:?}"
    );
    assert!(
        !display.contains("|---") && !display.contains("| 苹果 |"),
        "对象行不得显示表格源码：{display:?}"
    );
    assert!(
        display.contains("# 表格") && display.contains("后文"),
        "表格前后的正文必须原样可见：{display:?}"
    );
    let snapshots = cx.update(|_window, app| view.read(app).table_snapshots());
    assert_eq!(snapshots.len(), 1, "应有一张整行独占表格：{snapshots:?}");
    assert_eq!(snapshots[0].line, TABLE_LINE);
    assert_eq!(
        snapshots[0].end_line, 5,
        "表格跨 2..5 行（含表头/分隔/两行数据）"
    );
    assert_eq!(snapshots[0].columns, 3);
    assert_eq!(snapshots[0].rows, 2);
    assert!(snapshots[0].display.is_some(), "必须已投影成对象");
    assert!(!snapshots[0].selected && !snapshots[0].editing_source);

    // 2) 几何：表格矩形 = 表头 28 + 2 行 × 26 = 80（上下内边距属于布局行高，不在矩形里，
    //    与图片对象的 rect 语义一致）；宽度 = 内容宽度。
    let rect = object_rect(TABLE_LINE, &view, cx);
    let expected_height = theme::TABLE_HEADER_HEIGHT + 2.0 * theme::TABLE_ROW_HEIGHT;
    assert!(
        (rect.size.height.as_f32() - expected_height).abs() < 0.5,
        "表格矩形高度必须由固定行高算出：{rect:?}"
    );
    assert!(
        rect.size.width.as_f32() > theme::TABLE_MIN_COLUMN_WIDTH * 3.0,
        "默认窗口下三列应超过最小列宽：{rect:?}"
    );

    // 3) 点击**数据单元格**：选中该格（S3b2），表格仍是 Object、不展开源码，
    //    正文 caret **一动不动**（单元格选中不是文本 caret）。
    let cell_click = point(
        rect.origin.x + px(theme::TABLE_CELL_PAD_X + 4.0),
        rect.origin.y + px(theme::TABLE_HEADER_HEIGHT + theme::TABLE_ROW_HEIGHT * 0.5),
    );
    cx.simulate_click(cell_click, Modifiers::default());
    cx.run_until_parked();
    let display = display_of(&view, cx);
    assert!(
        display.contains(placeholder) && !display.contains("| 苹果 |"),
        "点击单元格不得显示表格源码：{display:?}"
    );
    let snapshots = cx.update(|_window, app| view.read(app).table_snapshots());
    assert!(snapshots[0].selected, "单击后表格必须处于选中态");
    assert_eq!(
        snapshots[0].selected_cell,
        Some((1, 0)),
        "单击命中的是第一条数据行第一格"
    );
    assert_eq!(snapshots[0].editing_cell, None, "单击只选中，不进入编辑");
    assert_eq!(
        caret_of(&view, cx),
        (0, 0),
        "单击单元格不得移动正文 caret（也不把 caret 放进格子里）"
    );
    assert!(
        cx.debug_bounds("object-toolbar").is_some(),
        "选中表格必须显示悬浮工具条"
    );
    assert!(
        cx.debug_bounds("object-toolbar-edit").is_some(),
        "表格工具条必须有「编辑源码」"
    );
    assert!(
        cx.debug_bounds("object-toolbar-zoom").is_none(),
        "表格不需要「放大」按钮（ADR 0019 S3a）"
    );

    // 4) 工具条「编辑源码」→ 显示整张表源码；Esc → 恢复表格对象。
    let edit = cx
        .debug_bounds("object-toolbar-edit")
        .expect("「编辑源码」必须可见");
    cx.simulate_click(edit.center(), Modifiers::default());
    cx.run_until_parked();
    let display = display_of(&view, cx);
    assert!(
        display.contains("| 名称 | 数量 | 备注 |")
            && display.contains("|:--|:-:|--:|")
            && display.contains("| 香蕉 | 5 | 黄色 |"),
        "源码模式必须显示完整表格：{display:?}"
    );
    assert!(
        !display.contains(placeholder),
        "源码模式下不再有对象占位符：{display:?}"
    );
    let snapshots = cx.update(|_window, app| view.read(app).table_snapshots());
    assert!(snapshots[0].editing_source && snapshots[0].source_visible);
    assert!(snapshots[0].display.is_none());

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let display = display_of(&view, cx);
    assert!(
        display.contains(placeholder) && !display.contains("| 名称 |"),
        "Esc 必须退出源码模式并恢复表格：{display:?}"
    );

    // 5) 表格对象上 F2 同样进入源码模式；点击块外（标题行）退出。
    //    注意：有**单元格选中态**时 F2 的语义是“进入单元格编辑”（S3b2），
    //    所以先 Esc 清掉单元格选中态再按 F2。
    let rect = object_rect(TABLE_LINE, &view, cx);
    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();
    let snapshots = cx.update(|_window, app| view.read(app).table_snapshots());
    assert_eq!(snapshots[0].selected_cell, Some((1, 1)), "中心点落在第二列");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let snapshots = cx.update(|_window, app| view.read(app).table_snapshots());
    assert_eq!(
        snapshots[0].selected_cell, None,
        "Esc 退出单元格选中态（整表仍选中）"
    );
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    let display = display_of(&view, cx);
    assert!(
        display.contains("| 苹果 | 3 | 红色 |"),
        "F2 在表格对象上必须进入源码模式：{display:?}"
    );
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    cx.simulate_click(
        point(
            pane.origin.x + px(theme::PAD_X + 8.0),
            pane.origin.y + px(theme::PAD_Y + 6.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    let display = display_of(&view, cx);
    assert!(
        display.contains(placeholder) && !display.contains("| 苹果 |"),
        "点击块外必须退出源码模式：{display:?}"
    );

    // 6) 全程只读：点选/工具条/源码切换都不改正文。
    assert_eq!(
        source_of(&view, cx),
        TABLE_SOURCE,
        "读表格/编辑源码切换不得改写正文"
    );
    close_shell(&host, cx);
}

#[gpui::test]
async fn long_table_is_clipped_by_rows_and_hidden_when_scrolled_out(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    // 表格在前、后面 80 行正文：滚动到下面时表格整体离开视口。
    let mut text = String::from("| 行 | 值 |\n|---|---|\n");
    for index in 0..60 {
        text.push_str(&format!("| r{index} | v{index} |\n"));
    }
    text.push('\n');
    for index in 0..80 {
        text.push_str(&format!("正文 {index}\n"));
    }
    seed_note(&vault, "长表格", &text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 1) 视口内只 shaping 与视口相交的行（远小于 60 行）。
    let painted = cx.update(|_window, app| view.read(app).table_paint_rows());
    assert_eq!(painted.len(), 1, "表格对象必须可见：{painted:?}");
    let (line, header, rows) = painted[0];
    assert_eq!(line, 0);
    assert!(header, "表头在视口内时必须绘制");
    assert!(rows > 0, "至少应绘制一行数据：{painted:?}");
    assert!(rows < 60, "长表格必须按视口裁剪：绘制了 {rows} 行");

    // 1b) 滚动到长表格**中间**：仍按视口裁剪，但必须继续绘制可见行（不能整表变空白）。
    // 这一条钉住“元素局部坐标 vs 内容坐标”的坐标系错误（rect 已经减过 scroll）。
    scroll(cx, -900.0);
    let painted = cx.update(|_window, app| view.read(app).table_paint_rows());
    assert_eq!(painted.len(), 1, "滚到表格中间仍必须绘制对象：{painted:?}");
    assert!(
        painted[0].2 > 0,
        "滚到表格中间必须绘制可见数据行：{painted:?}"
    );
    assert!(painted[0].2 < 60, "仍必须按视口裁剪：{painted:?}");

    // 2) 向下滚动到表格完全离开视口：对象不再绘制（不 shape 屏外行，也不命中）。
    scroll(cx, -4000.0);
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    assert!(
        rects.iter().all(|(line, _)| *line != 0),
        "表格滚出视口后不得再绘制：{rects:?}"
    );
    assert!(
        cx.update(|_window, app| view.read(app).table_paint_rows())
            .is_empty(),
        "滚出视口后不得 shaping 表格单元格"
    );

    // 3) 滚回顶部：表格重新出现，几何与绘制都恢复。
    scroll(cx, 8000.0);
    let rect = object_rect(0, &view, cx);
    assert!(rect.size.height.as_f32() > 0.0);
    let painted = cx.update(|_window, app| view.read(app).table_paint_rows());
    assert_eq!(painted.len(), 1, "滚回后表格必须重新绘制：{painted:?}");
    close_shell(&host, cx);
}

#[gpui::test]
async fn narrow_window_keeps_table_geometry_and_does_not_panic(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "窄窗口表格", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 把窗口压窄到侧栏 + 很窄的编辑区：列宽会退化到最小列宽，绘制由内容遮罩裁剪。
    cx.simulate_resize(size(px(300.0), px(600.0)));
    cx.run_until_parked();
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("窄窗口下编辑器面板仍必须画出来");
    assert!(
        pane.size.width.as_f32() > 0.0,
        "编辑区宽度必须为正：{pane:?}"
    );

    // 表格对象仍在：矩形宽度 == 内容宽度（可能小于 3 × 最小列宽），没有 panic。
    let rect = object_rect(TABLE_LINE, &view, cx);
    assert!(rect.size.width.as_f32() > 0.0);
    assert!(
        rect.size.width.as_f32() <= pane.size.width.as_f32(),
        "表格不得画到编辑区外：rect={rect:?} pane={pane:?}"
    );
    let painted = cx.update(|_window, app| view.read(app).table_paint_rows());
    assert_eq!(painted.len(), 1, "窄窗口仍必须绘制表格：{painted:?}");
    assert!(painted[0].1, "表头必须绘制");
    assert_eq!(painted[0].2, 2, "两行数据都在视口内");

    // 极窄（贴边）也不能 panic；表格对象仍然带着正尺寸矩形。
    cx.simulate_resize(size(px(240.0), px(600.0)));
    cx.run_until_parked();
    let rect = object_rect(TABLE_LINE, &view, cx);
    assert!(
        rect.size.width.as_f32() > 0.0 && rect.size.width.as_f32().is_finite(),
        "极窄窗口不能产生退化矩形：{rect:?}"
    );
    close_shell(&host, cx);
}

#[gpui::test]
async fn deleting_a_table_removes_the_whole_source_span(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let doc_id = seed_note(&vault, "删除表格", TABLE_SOURCE);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let rect = object_rect(TABLE_LINE, &view, cx);

    // 单击选中整块 → Backspace 直接删**完整源码跨度**（对象已被点选，不再是
    // “先选块”那一步；与图片对象的行为一致，见 headless_images）。
    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();
    let snapshots = cx.update(|_window, app| view.read(app).table_snapshots());
    assert!(snapshots[0].selected);
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    let source = source_of(&view, cx);
    assert!(
        !source.contains("| 苹果 |") && !source.contains("|---"),
        "Backspace 必须删掉整张表：{source:?}"
    );
    assert_eq!(source, "# 表格\n\n\n\n后文\n", "只留空行，不留残片");
    assert!(
        cx.update(|_window, app| view.read(app).table_snapshots())
            .is_empty(),
        "删除后不应再解析出表格"
    );

    close_shell(&host, cx);
    // 落库校验：删除只去掉表格那几行源码。
    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (after, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    assert_eq!(after.text(), "# 表格\n\n\n\n后文\n");
}

#[gpui::test]
async fn blocks_after_a_table_keep_their_own_metrics(cx: &mut TestAppContext) {
    // 回归：表格把 3 行源码压成 1 个 display 行。若布局按“display 行号 == 源码行号”
    // 取度量，表格之后的标题/列表会拿到错行的字号/缩进（表格越高错得越多）。
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let text = "| a | b |\n|---|---|\n| 1 | 2 |\n\n# 表格之后的一级标题\n\n- 一级\n  - 列表项\n";
    seed_note(&vault, "表格之后", text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let display = display_of(&view, cx);
    let rows = cx.update(|_window, app| view.read(app).visual_row_metrics());
    assert!(!rows.is_empty(), "必须有可见视觉行");

    // 标题行：display 里 marker 被隐藏，只剩“表格之后的一级标题”，字号必须是 h1。
    let heading = rows
        .iter()
        .find(|metric| {
            let (start, end) = metric.bytes;
            &display[start..end] == "表格之后的一级标题"
        })
        .unwrap_or_else(|| panic!("找不到标题行：display={display:?} rows={rows:?}"));
    assert!(
        heading.font_size >= theme::heading_font_size(1),
        "表格之后的标题必须按 h1 字号布局：{heading:?}"
    );
    // 列表行：bullet 被 Replace，但缩进必须来自**它自己的**源码行（不是表格行）。
    let list_row = rows
        .iter()
        .find(|metric| {
            let (start, end) = metric.bytes;
            display[start..end].contains("列表项")
        })
        .unwrap_or_else(|| panic!("找不到列表行：display={display:?} rows={rows:?}"));
    assert!(
        (list_row.indent - theme::LIST_INDENT_PER_LEVEL).abs() < 0.5,
        "表格之后的嵌套列表项必须有 depth=1 的缩进：{list_row:?}"
    );
    assert_eq!(
        list_row.font_size,
        theme::FONT_SIZE,
        "列表正文行不能拿到标题字号：{list_row:?}"
    );
    close_shell(&host, cx);
}
