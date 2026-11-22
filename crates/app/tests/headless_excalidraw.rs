//! 画布笔记（`.excalidraw`）+ Excalidraw 编辑器覆盖层的 headless 集成测试。
//!
//! 覆盖（真实 `AppShell` + 临时 vault + `#[gpui::test]`，**不 sleep**）：
//!
//! 1. 侧栏「画布」新建 `未命名画布.excalidraw`（重名追加 ` 2`…，后缀仍在最后）
//!    并直接打开覆盖层；
//! 2. 覆盖层里的场景（通过 `load_scene_json` 注入矩形，不模拟画布点击）保存后
//!    **活动 `EditorView` 的源文**变成合法 `.excalidraw` JSON；
//! 3. 「关闭（不保存）」丢弃改动，正文一个字节都不动；
//! 4. 重新打开画布笔记能读回保存的场景；
//! 5. 普通笔记不出现覆盖层。
//!
//! 纪律（AGENTS.md §6）：临时目录一律 `CARGO_TARGET_TMPDIR`；收敛点只用
//! `run_until_parked()`（不 sleep、不依赖系统时间/窗口焦点）。

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    NavigationDirection, ParentElement as _, Render, Styled as _, TestAppContext,
    VisualTestContext,
};
use menghunote_app::drawing::DrawingOverlay;
use menghunote_app::editor::view::ObjectImageState;
use menghunote_app::shell::AppShell;
use menghunote_storage::{
    append_updates, list_tree_sorted, load_body_doc, StructureWriter, TreeSort, UpdateRecord, Vault,
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

fn temp_dir(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-excalidraw-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("创建临时目录失败")
}

fn temp_vault(prefix: &str) -> (TempDir, Vault) {
    let dir = temp_dir(prefix);
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    (dir, vault)
}

/// 正文 A 事务：把 `text` 写进已存在的 body doc（与 `headless_templates` 同一条路径）。
fn write_body(vault: &Vault, body_doc_id: &str, text: &str) {
    let (doc, head) = load_body_doc(vault, body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-excalidraw", update)],
    )
    .expect("A 事务写入正文失败");
}

/// 建一篇普通笔记（标题 + 正文），返回节点 ID。
fn seed_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-excalidraw-seed")
        .expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, text);
    seed.node_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-excalidraw".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn active_view(
    host: &Entity<ShellHost>,
    cx: &mut VisualTestContext,
) -> Entity<menghunote_app::editor::view::EditorView> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_editor_view()
            .expect("打开笔记后必须有活动编辑器视图")
    })
}

fn source_of(
    view: &Entity<menghunote_app::editor::view::EditorView>,
    cx: &mut VisualTestContext,
) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn overlay_open(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_window, app| host.read(app).shell.read(app).drawing_overlay_open())
}

fn overlay_entity(
    host: &Entity<ShellHost>,
    cx: &mut VisualTestContext,
) -> Option<Entity<DrawingOverlay>> {
    cx.update(|_window, app| host.read(app).shell.read(app).drawing_overlay_entity())
}

fn overlay_scene_json(overlay: &Entity<DrawingOverlay>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| overlay.read(app).scene_json(app).expect("场景必须能序列化"))
}

/// 注入一个矩形（模拟用户在画布上画了一笔；不模拟画布点击——vendor 的命中用
/// 窗口坐标，headless 下不可靠）。
fn inject_rectangle(overlay: &Entity<DrawingOverlay>, cx: &mut VisualTestContext) {
    let mut scene = gpui_excalidraw::core::scene::Scene::new();
    scene.add(gpui_excalidraw::core::factory::new_rectangle(
        12.0,
        24.0,
        120.0,
        80.0,
        &gpui_excalidraw::core::factory::ElementOptions::default(),
    ));
    let json =
        gpui_excalidraw::scene_to_json(&scene, &gpui_excalidraw::core::scene::AppState::default())
            .expect("序列化注入场景失败");
    cx.update(|_window, app| {
        overlay.update(app, |overlay, cx| overlay.load_scene_json(&json, cx));
    });
    cx.run_until_parked();
}

/// 侧栏（目录优先 + 文件名 A-Z）里标题对应的先序下标。
fn tree_row(vault: &Vault, title: &str) -> usize {
    list_tree_sorted(vault, TreeSort::NameAsc)
        .expect("读树失败")
        .iter()
        .position(|entry| entry.title == title)
        .unwrap_or_else(|| panic!("结构树里找不到「{title}」"))
}

#[gpui::test]
async fn new_drawing_opens_overlay_and_saves_scene_json(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("save");
    seed_note(&vault, "普通笔记", "普通正文");
    let (host, cx) = open_shell(cx, vault.clone());

    // 启动打开的是排序后的第一篇（普通笔记）：没有覆盖层。
    assert!(!overlay_open(&host, cx), "普通笔记不能出现画布覆盖层");
    assert!(
        cx.debug_bounds("drawing-overlay").is_none(),
        "普通笔记不能渲染覆盖层"
    );

    // 侧栏「画布」：新建并直接打开覆盖层。
    click_selector(cx, "sidebar-new-drawing");
    assert!(overlay_open(&host, cx), "新建画布后必须打开覆盖层");
    let bounds = cx
        .debug_bounds("drawing-overlay")
        .expect("覆盖层必须画出来");
    assert!(
        bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0,
        "覆盖层必须有真实尺寸：{bounds:?}"
    );
    // 画布像普通笔记一样只占内容区，侧栏保持可见。
    let sidebar = cx.debug_bounds("sidebar").expect("侧栏必须画出来");
    assert!(
        bounds.origin.x >= sidebar.origin.x + sidebar.size.width,
        "覆盖层必须只占内容区（侧栏右边界 {:.1}，覆盖层左边界 {:.1}）",
        (sidebar.origin.x + sidebar.size.width).as_f32(),
        bounds.origin.x.as_f32()
    );
    assert!(
        cx.debug_bounds("drawing-editor").is_some(),
        "`drawing-editor` 必须填满内容区"
    );
    assert!(
        cx.debug_bounds("drawing-title").is_some(),
        "bar 标题必须可见"
    );

    // 新建的标题必须仍是画布笔记（序号在后缀**之前**）。
    let tree = list_tree_sorted(&vault, TreeSort::NameAsc).expect("读树失败");
    let titles: Vec<&str> = tree.iter().map(|entry| entry.title.as_str()).collect();
    assert!(
        titles.contains(&"未命名画布.excalidraw"),
        "新建画布标题必须带后缀：{titles:?}"
    );

    // 打开时正文是空串。
    let view = active_view(&host, cx);
    assert_eq!(source_of(&view, cx), "", "新建画布正文必须为空");

    // 注入矩形 → 保存并关闭。
    let overlay = overlay_entity(&host, cx).expect("覆盖层实体必须存在");
    inject_rectangle(&overlay, cx);
    click_selector(cx, "drawing-save");
    assert!(!overlay_open(&host, cx), "保存后必须关闭覆盖层");

    // 源文必须变成合法 `.excalidraw` JSON，且包含注入的矩形。
    let source = source_of(&view, cx);
    assert!(
        source.contains("\"type\": \"excalidraw\""),
        "保存后的正文必须是 .excalidraw 信封：{source}"
    );
    assert!(
        source.contains("\"rectangle\""),
        "保存后的正文必须包含注入的矩形：{source}"
    );
    assert!(
        source.contains("\"version\": 2"),
        "信封版本必须是 2：{source}"
    );
    // 合法 JSON：vendor 的解析器必须能读回。
    let (scene, _) = gpui_excalidraw::json_to_scene(&source).expect("保存后的正文必须是合法 JSON");
    assert_eq!(scene.len(), 1, "必须恰好一个元素：{source}");

    // 重新打开画布笔记：能读回保存的场景。
    let drawing_row = tree_row(&vault, "未命名画布.excalidraw");
    let selector: &'static str = Box::leak(format!("tree-row-{drawing_row}").into_boxed_str());
    // 先切到普通笔记（关闭覆盖层），再切回来。
    let plain_row = tree_row(&vault, "普通笔记");
    let plain_selector: &'static str = Box::leak(format!("tree-row-{plain_row}").into_boxed_str());
    click_selector(cx, plain_selector);
    assert!(!overlay_open(&host, cx), "切到普通笔记必须关闭覆盖层");
    click_selector(cx, selector);
    assert!(overlay_open(&host, cx), "切回画布必须重新打开覆盖层");
    let reopened = overlay_entity(&host, cx).expect("覆盖层实体必须存在");
    let scene_json = overlay_scene_json(&reopened, cx);
    assert!(
        scene_json.contains("\"rectangle\""),
        "重开画布必须读回保存的场景：{scene_json}"
    );

    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn close_without_saving_keeps_source(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("discard");
    seed_note(&vault, "普通笔记", "普通正文");
    let (host, cx) = open_shell(cx, vault.clone());

    click_selector(cx, "sidebar-new-drawing");
    let view = active_view(&host, cx);
    let before = source_of(&view, cx);
    assert_eq!(before, "", "新建画布正文必须为空");

    let overlay = overlay_entity(&host, cx).expect("覆盖层实体必须存在");
    inject_rectangle(&overlay, cx);
    click_selector(cx, "drawing-close");
    assert!(!overlay_open(&host, cx), "关闭后必须卸载覆盖层");
    assert_eq!(source_of(&view, cx), before, "关闭（不保存）不能改正文");

    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn closed_canvas_shows_placeholder_not_json_and_reopens(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("placeholder");
    seed_note(&vault, "普通笔记", "普通正文");
    let (host, cx) = open_shell(cx, vault.clone());

    click_selector(cx, "sidebar-new-drawing");
    assert!(overlay_open(&host, cx), "新建画布后必须打开覆盖层");
    click_selector(cx, "drawing-close");
    assert!(!overlay_open(&host, cx), "关闭后覆盖层实体必须卸载");

    // 关闭后**不能**把 .excalidraw JSON 铺到编辑区：必须显示占位页。
    let placeholder = cx
        .debug_bounds("drawing-placeholder")
        .expect("关闭画布后必须显示占位页（而不是 JSON 源码）");
    assert!(
        placeholder.size.width.as_f32() > 0.0 && placeholder.size.height.as_f32() > 0.0,
        "占位页必须有真实尺寸：{placeholder:?}"
    );
    assert!(
        cx.debug_bounds("drawing-open").is_some(),
        "占位页必须有「打开画布」按钮"
    );

    click_selector(cx, "drawing-open");
    assert!(overlay_open(&host, cx), "点「打开画布」必须重新渲染画布");
    assert!(
        cx.debug_bounds("drawing-placeholder").is_none(),
        "画布重新打开后占位页必须消失"
    );

    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn second_drawing_gets_numbered_suffix(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("unique");
    seed_note(&vault, "普通笔记", "普通正文");
    let (host, cx) = open_shell(cx, vault.clone());

    click_selector(cx, "sidebar-new-drawing");
    click_selector(cx, "sidebar-new-drawing");

    let tree = list_tree_sorted(&vault, TreeSort::NameAsc).expect("读树失败");
    let titles: Vec<&str> = tree.iter().map(|entry| entry.title.as_str()).collect();
    assert!(
        titles.contains(&"未命名画布.excalidraw"),
        "第一篇必须是未命名画布.excalidraw：{titles:?}"
    );
    assert!(
        titles.contains(&"未命名画布 2.excalidraw"),
        "重名必须把序号插在后缀之前（后缀仍在最后）：{titles:?}"
    );

    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn switching_to_plain_note_closes_overlay(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("switch");
    seed_note(&vault, "普通笔记", "普通正文");
    let (host, cx) = open_shell(cx, vault.clone());

    click_selector(cx, "sidebar-new-drawing");
    assert!(overlay_open(&host, cx), "新建画布后必须打开覆盖层");
    let plain_row = tree_row(&vault, "普通笔记");
    let plain_selector: &'static str = Box::leak(format!("tree-row-{plain_row}").into_boxed_str());
    click_selector(cx, plain_selector);
    assert!(!overlay_open(&host, cx), "切换笔记必须关闭覆盖层");
    assert!(
        cx.debug_bounds("drawing-overlay").is_none(),
        "普通笔记不能渲染覆盖层"
    );

    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn canvas_hit_testing_subtracts_content_origin(cx: &mut TestAppContext) {
    use gpui::{point, px, MouseButton};

    let (_dir, vault) = temp_vault("coords");
    seed_note(&vault, "普通笔记", "普通正文");
    let (host, cx) = open_shell(cx, vault.clone());

    click_selector(cx, "sidebar-new-drawing");
    let overlay = overlay_entity(&host, cx).expect("覆盖层实体必须存在");
    let editor = cx.update(|_window, app| overlay.read(app).editor_entity());
    editor.update(cx, |editor, _| {
        editor.set_tool(gpui_excalidraw::ui::Tool::Ellipse);
    });

    let canvas = cx
        .debug_bounds("drawing-editor")
        .expect("画布内容区必须画出来");
    // 点在**内容区**内部 (120,120)，即窗口坐标 = 内容区原点 + 120。
    let start = point(canvas.origin.x + px(120.0), canvas.origin.y + px(120.0));
    let end = point(start.x + px(160.0), start.y + px(100.0));
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();

    editor.update(cx, |editor, _| {
        let base = editor
            .document
            .scene
            .elements
            .last()
            .expect("拖拽必须画出椭圆")
            .base();
        assert!(
            (base.x - 120.0).abs() < 2.0 && (base.y - 120.0).abs() < 2.0,
            "画布命中必须扣除内容区原点，实际落在 ({}, {})",
            base.x,
            base.y
        );
    });

    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn canvas_hit_testing_survives_dpi_scale(cx: &mut TestAppContext) {
    use gpui::{point, px, MouseButton};

    let (_dir, vault) = temp_vault("coords-dpi");
    seed_note(&vault, "普通笔记", "普通正文");
    let (host, cx) = open_shell(cx, vault.clone());

    click_selector(cx, "sidebar-new-drawing");
    let overlay = overlay_entity(&host, cx).expect("覆盖层实体必须存在");
    let editor = cx.update(|_window, app| overlay.read(app).editor_entity());
    editor.update(cx, |editor, _| {
        editor.set_tool(gpui_excalidraw::ui::Tool::Ellipse);
    });

    cx.simulate_scale_factor_change(1.5);
    cx.run_until_parked();
    let canvas = cx.debug_bounds("drawing-editor").expect("画布内容区");
    let start = point(canvas.origin.x + px(120.0), canvas.origin.y + px(120.0));
    let end = point(start.x + px(160.0), start.y + px(100.0));
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();

    editor.update(cx, |editor, _| {
        let base = editor.document.scene.elements.last().expect("椭圆").base();
        assert!(
            (base.x - 120.0).abs() < 2.0 && (base.y - 120.0).abs() < 2.0,
            "DPI 1.5 后画布命中仍须扣除内容区原点，实际 ({}, {})",
            base.x,
            base.y
        );
    });
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn middle_drag_release_outside_canvas_stops_panning(cx: &mut TestAppContext) {
    use gpui::{point, px};

    let (_dir, vault) = temp_vault("middle-up");
    seed_note(&vault, "普通笔记", "普通正文");
    let (host, cx) = open_shell(cx, vault.clone());

    click_selector(cx, "sidebar-new-drawing");
    let overlay = overlay_entity(&host, cx).expect("覆盖层实体必须存在");
    let editor = cx.update(|_window, app| overlay.read(app).editor_entity());
    let canvas = cx
        .debug_bounds("drawing-editor")
        .expect("画布内容区必须画出来");

    let start = point(canvas.origin.x + px(180.0), canvas.origin.y + px(160.0));
    let mid = point(start.x + px(80.0), start.y + px(40.0));
    // 拖到侧栏外再松开：元素级 hitbox 会漏掉 mouse-up，窗口级监听必须兜底。
    let outside = point(canvas.origin.x - px(60.0), mid.y);
    cx.simulate_mouse_down(start, MouseButton::Middle, Modifiers::default());
    cx.simulate_mouse_move(mid, Some(MouseButton::Middle), Modifiers::default());
    cx.simulate_mouse_move(outside, Some(MouseButton::Middle), Modifiers::default());
    cx.simulate_mouse_up(outside, MouseButton::Middle, Modifiers::default());
    let scroll_after_up = editor.update(cx, |editor, _| editor.document.scroll);
    cx.simulate_mouse_move(
        point(outside.x + px(50.0), outside.y + px(50.0)),
        None,
        Modifiers::default(),
    );
    cx.run_until_parked();

    editor.update(cx, |editor, _| {
        let scroll = editor.document.scroll;
        assert_eq!(
            (scroll.x, scroll.y),
            (scroll_after_up.x, scroll_after_up.y),
            "中键松开后的移动不能再平移"
        );
        assert!(!editor.is_interacting(), "中键松开后不能仍处于 interacting");
    });

    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn frontmatter_renders_property_panel_and_hides_yaml(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("properties");
    seed_note(
        &vault,
        "属性笔记",
        "---\ntags: []\ndate_created: 2025-08-10T22:23:41\n---\n正文",
    );
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    let view = active_view(&host, cx);

    assert!(
        cx.debug_bounds("note-properties").is_some(),
        "有 frontmatter 时必须渲染属性面板"
    );
    assert!(
        cx.debug_bounds("property-row-0").is_some(),
        "第一条属性必须画出来"
    );
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains("date_created") && !display.contains("---"),
        "frontmatter 源码不能被铺进正文：{display:?}"
    );
    assert!(
        display.starts_with("正文"),
        "正文应在属性面板之后：{display:?}"
    );
}

#[gpui::test]
async fn typing_dashes_immediately_opens_a_frontmatter_block(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("frontmatter-convert");
    seed_note(&vault, "空笔记", "");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    let view = active_view(&host, cx);

    // 单击编辑区，让 caret 落在正文（空文档 = 第 0 行）。
    let pane = cx.debug_bounds("editor-pane").expect("编辑器面板");
    cx.simulate_click(pane.center(), Modifiers::default());
    cx.simulate_input("---");
    cx.run_until_parked();

    let source = source_of(&view, cx);
    assert!(
        source.starts_with("---\n\n---\n"),
        "输入 --- 必须立即补全 frontmatter（不用 Enter），实际 {source:?}"
    );
    assert!(
        cx.debug_bounds("note-properties").is_some(),
        "自动补全后属性面板必须出现"
    );
}

#[gpui::test]
async fn editing_a_property_value_rewrites_the_yaml(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("properties-edit");
    seed_note(&vault, "属性笔记", "---\ntags: []\n---\n正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    let view = active_view(&host, cx);

    click_selector(cx, "property-value-0");
    cx.simulate_input("rust");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    let source = source_of(&view, cx);
    assert!(
        source.contains("tags: rust"),
        "属性值编辑必须写回 YAML，实际 {source:?}"
    );
    assert!(
        !source.starts_with("tags"),
        "frontmatter 分隔符必须保留：{source:?}"
    );
}

#[gpui::test]
async fn adding_and_deleting_properties_updates_the_block(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("properties-add");
    seed_note(&vault, "属性笔记", "---\ntags: []\n---\n正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    let view = active_view(&host, cx);

    click_selector(cx, "properties-add");
    assert!(
        cx.debug_bounds("property-edit").is_some(),
        "新增属性后应直接进入 key 编辑"
    );
    cx.simulate_input("status");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let source = source_of(&view, cx);
    assert!(
        source.contains("status:"),
        "新增属性 key 未写回：{source:?}"
    );

    click_selector(cx, "property-delete-1");
    cx.run_until_parked();
    let source = source_of(&view, cx);
    assert!(!source.contains("status:"), "删除属性未写回：{source:?}");
}

#[gpui::test]
async fn typing_dashes_before_existing_content_keeps_the_body(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("frontmatter-existing");
    seed_note(&vault, "已有正文", "第一行\n第二行");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    let view = active_view(&host, cx);

    // 把 caret 放到文档最开头，再输入 `---` + Enter。
    cx.simulate_keystrokes("ctrl-home");
    cx.simulate_input("---");
    cx.run_until_parked();

    let source = source_of(&view, cx);
    assert!(
        source.starts_with("---\n\n---\n第一行\n第二行"),
        "输入 --- 必须立即补全且不能吞掉已有正文：{source:?}"
    );
    assert!(
        cx.debug_bounds("note-properties").is_some(),
        "已有正文时也要出现属性面板"
    );
}

#[gpui::test]
async fn property_panel_scrolls_with_the_body(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("properties-scroll");
    let mut body = String::from("---\ntags: []\nstatus: active\n---\n");
    for index in 0..80 {
        body.push_str(&format!("第{index}行正文\n"));
    }
    seed_note(&vault, "长属性笔记", &body);
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    let view = active_view(&host, cx);

    let before = cx.debug_bounds("note-properties").expect("属性面板");
    view.update(cx, |view, cx| view.scroll_by(40.0, cx));
    cx.run_until_parked();
    let after = cx
        .debug_bounds("note-properties")
        .expect("滚动后属性面板仍存在");
    let moved = (before.origin.y - after.origin.y).as_f32();
    assert!(
        (moved - 40.0).abs() < 2.0,
        "属性面板必须随正文一起滚动：before={:?} after={:?}",
        before.origin,
        after.origin
    );
}

#[gpui::test]
async fn backspace_at_body_start_does_not_edit_hidden_frontmatter(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("properties-backspace");
    seed_note(&vault, "属性笔记", "---\ntags: []\n---\n正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    let view = active_view(&host, cx);
    let before = source_of(&view, cx);

    // 打开即 caret 在正文起点；Backspace 只能停在正文开头，不能删到隐藏的 `---`。
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        before,
        "正文起点的 Backspace 不能改隐藏 frontmatter"
    );
}

#[gpui::test]
async fn property_panel_can_collapse_and_clear_all(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("properties-collapse");
    seed_note(
        &vault,
        "属性笔记",
        "---\ntags: []\nstatus: active\n---\n正文",
    );
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    let view = active_view(&host, cx);

    let expanded = cx.debug_bounds("note-properties").expect("面板");
    click_selector(cx, "properties-collapse");
    let collapsed = cx.debug_bounds("note-properties").expect("折叠后面板");
    assert!(
        collapsed.size.height < expanded.size.height,
        "折叠后高度必须变小：{expanded:?} -> {collapsed:?}"
    );
    assert!(
        cx.debug_bounds("property-row-0").is_none(),
        "折叠后不再画属性行"
    );
    click_selector(cx, "properties-collapse");
    assert!(cx.debug_bounds("property-row-0").is_some(), "再点应展开");

    click_selector(cx, "properties-clear");
    cx.run_until_parked();
    let source = source_of(&view, cx);
    assert!(
        !source.contains("tags:") && !source.contains("status:"),
        "清除后属性行必须为空：{source:?}"
    );
    assert!(
        source.starts_with("---\n---\n"),
        "清除只清属性，frontmatter 分隔符保留：{source:?}"
    );
}

#[gpui::test]
async fn trailing_dashes_render_as_a_thematic_break(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("thematic-break");
    seed_note(
        &vault,
        "分隔线笔记",
        "---\ntags: []\n---\n正文\n\n---\n\n后文",
    );
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    let view = active_view(&host, cx);
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains("---"),
        "正文里的 --- 不能以源码形式铺出来：{display:?}"
    );
    assert!(
        display.contains("正文") && display.contains("后文"),
        "分隔线前后的正文必须保留：{display:?}"
    );
    let mut start = 0usize;
    let thematic = display.split_inclusive('\n').any(|line| {
        let row_start = start;
        start += line.len();
        cx.update(|_window, app| view.read(app).is_thematic_display_row(row_start))
    });
    assert!(thematic, "display 里必须有一行被识别为分隔线：{display:?}");
}

fn active_title(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_window, app| host.read(app).shell.read(app).active_note_title())
}

fn active_path(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_window, app| host.read(app).shell.read(app).active_note_path())
}

#[gpui::test]
async fn note_header_shows_path_and_renames_the_file(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("note-header");
    seed_note(&vault, "旧标题", "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");

    assert!(
        cx.debug_bounds("note-header").is_some(),
        "顶部标题栏必须存在"
    );
    assert!(cx.debug_bounds("note-path").is_some(), "路径面包屑必须存在");
    assert_eq!(active_title(&host, cx).as_deref(), Some("旧标题"));
    assert_eq!(active_path(&host, cx).as_deref(), Some("旧标题"));

    click_selector(cx, "note-title");
    assert!(
        cx.debug_bounds("note-title-edit").is_some(),
        "点标题进入编辑"
    );
    cx.simulate_input("新标题");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    assert_eq!(
        active_title(&host, cx).as_deref(),
        Some("新标题"),
        "标题修改必须同步到文件名"
    );
    assert_eq!(active_path(&host, cx).as_deref(), Some("新标题"));
}

#[gpui::test]
async fn note_history_buttons_and_mouse_side_buttons(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("note-history");
    seed_note(&vault, "第一篇", "A");
    seed_note(&vault, "第二篇", "B");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "tree-row-1");
    assert_eq!(active_title(&host, cx).as_deref(), Some("第二篇"));

    click_selector(cx, "note-nav-back");
    assert_eq!(active_title(&host, cx).as_deref(), Some("第一篇"));
    click_selector(cx, "note-nav-forward");
    assert_eq!(active_title(&host, cx).as_deref(), Some("第二篇"));

    let editor = cx.debug_bounds("editor-pane").expect("编辑器面板");
    cx.simulate_mouse_down(
        editor.center(),
        MouseButton::Navigate(NavigationDirection::Back),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert_eq!(
        active_title(&host, cx).as_deref(),
        Some("第一篇"),
        "鼠标侧键后退必须切历史"
    );
    cx.simulate_mouse_down(
        editor.center(),
        MouseButton::Navigate(NavigationDirection::Forward),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert_eq!(
        active_title(&host, cx).as_deref(),
        Some("第二篇"),
        "鼠标侧键前进必须切历史"
    );
}

#[gpui::test]
async fn embedded_drawing_renders_with_the_real_vault_provider(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("embed-preview");
    let mut scene = gpui_excalidraw::core::scene::Scene::new();
    scene.add(gpui_excalidraw::core::factory::new_rectangle(
        10.0,
        20.0,
        160.0,
        90.0,
        &gpui_excalidraw::core::factory::ElementOptions::default(),
    ));
    let scene_json =
        gpui_excalidraw::scene_to_json(&scene, &gpui_excalidraw::core::scene::AppState::default())
            .expect("序列化画布失败");
    seed_note(&vault, "流程图.excalidraw", &scene_json);
    seed_note(&vault, "嵌入页", "前文\n\n![[流程图.excalidraw]]\n\n后文\n");

    let (host, cx) = open_shell(cx, vault.clone());
    let embed_row = tree_row(&vault, "嵌入页");
    let selector: &'static str = Box::leak(format!("tree-row-{embed_row}").into_boxed_str());
    click_selector(cx, selector);
    let view = active_view(&host, cx);

    // 真实 provider：读「流程图.excalidraw」正文 → scene_json_to_png → ImageService 解码。
    cx.update(|_window, app| {
        view.update(app, |view, cx| {
            assert!(
                view.settle_images(std::time::Duration::from_secs(10), cx),
                "画布预览必须在有界时间内完成解码"
            );
        });
    });
    cx.run_until_parked();

    let snapshots = cx.update(|_window, app| view.read(app).drawing_snapshots());
    assert_eq!(snapshots.len(), 1, "必须识别一个画布嵌入");
    let snapshot = &snapshots[0];
    assert_eq!(snapshot.target, "流程图.excalidraw");
    assert!(snapshot.display.is_some(), "画布嵌入必须投影成 Object");
    assert!(!snapshot.source_visible, "投影后不能显示 ![[...]] 源码");
    assert_eq!(
        snapshot.state,
        ObjectImageState::Ready,
        "真实 provider 必须渲染出预览图；label={:?}",
        snapshot.label
    );
    assert!(
        snapshot
            .size
            .is_some_and(|(width, height)| width > 0.0 && height > 0.0),
        "预览必须有非零尺寸"
    );
    assert_eq!(
        source_of(&view, cx),
        "前文\n\n![[流程图.excalidraw]]\n\n后文\n",
        "渲染预览不得改写正文"
    );
}

#[gpui::test]
async fn clicking_embedded_drawing_opens_the_target_canvas(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("embed-open");
    let mut scene = gpui_excalidraw::core::scene::Scene::new();
    scene.add(gpui_excalidraw::core::factory::new_rectangle(
        10.0,
        20.0,
        120.0,
        80.0,
        &gpui_excalidraw::core::factory::ElementOptions::default(),
    ));
    let scene_json =
        gpui_excalidraw::scene_to_json(&scene, &gpui_excalidraw::core::scene::AppState::default())
            .expect("序列化画布失败");
    seed_note(&vault, "目标画布.excalidraw", &scene_json);
    seed_note(&vault, "嵌入入口", "![[目标画布.excalidraw]]\n");

    let (host, cx) = open_shell(cx, vault.clone());
    let row = tree_row(&vault, "嵌入入口");
    let selector: &'static str = Box::leak(format!("tree-row-{row}").into_boxed_str());
    click_selector(cx, selector);
    let view = active_view(&host, cx);
    cx.update(|_window, app| {
        view.update(app, |view, cx| {
            assert!(view.settle_images(std::time::Duration::from_secs(10), cx));
        });
    });
    cx.run_until_parked();
    let rect = cx
        .update(|_window, app| view.read(app).object_rects())
        .into_iter()
        .next()
        .map(|(_, rect)| rect)
        .expect("嵌入预览必须有可点击矩形");
    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();

    assert_eq!(
        cx.update(|_window, app| host.read(app).shell.read(app).active_note_title()),
        Some("目标画布.excalidraw".to_owned()),
        "点击嵌入预览必须打开目标画布笔记"
    );
    assert!(
        cx.debug_bounds("drawing-overlay").is_some(),
        "打开画布后必须出现覆盖层"
    );
}
