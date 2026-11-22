//! ADR 0025 后续切片：Markdown 内嵌画布预览（`![[目标.excalidraw]]`）headless
//! 集成测试。
//!
//! 真实 `EditorView`（真实 `SessionDriver` + 临时 vault，`CARGO_TARGET_TMPDIR`，
//! **不 sleep**）+ **注入的 fake asset provider**（识别 `drawing://` 合成 key 并
//! 返回一张 8×8 PNG）。断言：
//!
//! 1. 整行 `![[流程图.excalidraw]]` 投影成**图片式 Object**：display 只有占位符，
//!    不出现 `![[...]]` 源码；
//! 2. provider 命中 → 后台解码为 Ready，对象尺寸 = 8×8（等比），行高随内容变化；
//! 3. 单击预览**打开目标画布笔记**：发出
//!    `EditorEvent::WikiLinkRequested { target, heading: None }`（target 取 `|` 前），
//!    不移动 caret、不进源码模式、正文一个字节都不动；
//! 4. provider 缺失（目标不存在）→ 占位文案「画布笔记不存在」，正文不动；
//! 5. 非画布嵌入（`![[a.png]]`）保持普通文本（不吞半行）。
//!
//! 像素级外观（DPI/真实窗口）仍必须真机验证；这里证明解析→投影→请求→绘制状态→
//! 点击事件的完整链路。真实 provider（读目标笔记正文 + `scene_json_to_png`）由宿主
//! 接线，本切片只验证编辑器侧的请求/绘制/点击协议。

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    div, point, px, App, AppContext as _, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::attachment::VaultAssetWriter;
use menghunote_app::drawing::parse_drawing_asset_key;
use menghunote_app::editor::object_image::{empty_asset_provider, SharedAssetProvider};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::{EditorEvent, EditorView, ObjectImageState};
use menghunote_editor::{EditorController, Selection};
use menghunote_storage::session::{DriverOptions, SessionDriver};
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 8×8 纯色 PNG：fake provider 命中 `drawing://` key 时返回它。
fn png_bytes() -> Vec<u8> {
    let buffer = image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 60, 40, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("测试 PNG 编码失败");
    out.into_inner()
}

fn temp_vault_dir(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-excalidraw-embed-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 在空 vault 里建一篇正文（结构 CRDT → 投影 → 正文 A 事务），返回 `(node_id, body_doc_id)`。
fn seed_note(vault: &Vault, title: &str, text: &str) -> (String, String) {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-excalidraw-embed-seed")
        .expect("结构提交失败");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-excalidraw-embed", update)],
    )
    .expect("A 事务写入正文失败");
    (seed.node_id, seed.body_doc_id)
}

/// 一个最小合法 `.excalidraw` 场景信封（目标画布笔记的正文）。
fn drawing_scene_json() -> String {
    let mut scene = gpui_excalidraw::core::scene::Scene::new();
    scene.add(gpui_excalidraw::core::factory::new_rectangle(
        10.0,
        20.0,
        120.0,
        80.0,
        &gpui_excalidraw::core::factory::ElementOptions::default(),
    ));
    gpui_excalidraw::scene_to_json(&scene, &gpui_excalidraw::core::scene::AppState::default())
        .expect("序列化画布场景失败")
}

/// 窗口根视图：给 `EditorView` 一个稳定的渲染宿主。
struct ViewHost {
    view: Entity<EditorView>,
}

impl Render for ViewHost {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .size_full()
            .debug_selector(|| "embed-host".to_string())
            .child(self.view.clone())
    }
}

/// 用显式 provider 打开一篇正文，返回 `(host, view, cx)`。
fn open_view<'a>(
    cx: &'a mut TestAppContext,
    vault: Vault,
    body_doc_id: &str,
    provider: SharedAssetProvider,
) -> (Entity<ViewHost>, &'a mut VisualTestContext) {
    let mut options = DriverOptions::default();
    options.session.peer_id = 1;
    let driver = SessionDriver::open(&vault, body_doc_id, options).expect("打开会话驱动失败");
    let controller =
        EditorController::open(driver.session().snapshot()).expect("初始化编辑器控制器失败");
    let import_writer = VaultAssetWriter::shared(&vault);
    let doc_id = body_doc_id.to_owned();
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let view = cx.new(|cx| {
            EditorView::new(
                controller,
                driver,
                format!("headless-excalidraw-embed-{doc_id}"),
                provider,
                import_writer,
                cx,
            )
        });
        ViewHost { view }
    });
    cx.run_until_parked();
    (host, cx)
}

fn view_entity(host: &Entity<ViewHost>, cx: &mut VisualTestContext) -> Entity<EditorView> {
    cx.update(|_window, app| host.read(app).view.clone())
}

fn display_text(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).display_text())
}

fn source_text(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

/// 强制一帧（`render` 里登记画布预览请求 / 让新尺寸进入几何）。
fn force_frame(cx: &mut VisualTestContext) {
    let pane = cx
        .debug_bounds("editor-current-doc")
        .expect("编辑器正文必须画出来");
    let probe = point(
        pane.origin.x + px(theme::PAD_X + 4.0),
        pane.origin.y + px(theme::PAD_Y + 4.0),
    );
    cx.simulate_mouse_move(probe, None, Modifiers::default());
    cx.run_until_parked();
}

/// 等后台预览解码收敛 + 强制一帧（让新的图片尺寸进入几何）。收敛点是
/// `ImageService::wait_idle`（Condvar），**不 sleep**。
fn settle_images(view: &Entity<EditorView>, cx: &mut VisualTestContext) {
    // 先强制一帧：`render` → `request_object_images` 登记合成 key（幂等）。
    force_frame(cx);
    let settled = cx.update(|_window, app| {
        view.update(app, |view, cx| {
            view.settle_images(Duration::from_secs(20), cx)
        })
    });
    assert!(settled, "画布预览解码必须在有界时间内收敛");
    force_frame(cx);
}

/// 订阅视图的 `EditorEvent`，返回共享的事件收集器（订阅本身由调用方持有）。
fn subscribe_events(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
) -> (Rc<RefCell<Vec<EditorEvent>>>, gpui::Subscription) {
    let events: Rc<RefCell<Vec<EditorEvent>>> = Rc::new(RefCell::new(Vec::new()));
    let sink = Rc::clone(&events);
    let subscription = cx.update(|_window, app: &mut App| {
        app.subscribe(view, move |_view, event: &EditorEvent, _app| {
            sink.borrow_mut().push(event.clone());
        })
    });
    (events, subscription)
}

fn click_rect(cx: &mut VisualTestContext, rect: &gpui::Bounds<gpui::Pixels>) {
    cx.simulate_click(
        point(
            rect.origin.x + rect.size.width / 2.0,
            rect.origin.y + rect.size.height / 2.0,
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
}

fn wiki_requests(events: &Rc<RefCell<Vec<EditorEvent>>>) -> Vec<(String, Option<String>)> {
    events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            EditorEvent::WikiLinkRequested { target, heading } => {
                Some((target.clone(), heading.clone()))
            }
            _ => None,
        })
        .collect()
}

const HOST_BODY: &str = "前言\n\n![[流程图.excalidraw]]\n\n后文\n\n![[缺失.excalidraw]]\n";

/// 命中「流程图.excalidraw」的 fake provider；其它目标（缺失）返回 `None`。
fn drawing_provider() -> SharedAssetProvider {
    Arc::new(move |key: &str| -> Option<Vec<u8>> {
        let (_theme, target) = parse_drawing_asset_key(key)?;
        if target == "流程图.excalidraw" {
            Some(png_bytes())
        } else {
            None
        }
    })
}

#[gpui::test]
async fn embedded_drawing_renders_preview_and_click_opens_target(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("ready");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "流程图.excalidraw", &drawing_scene_json());
    let (_node, host_doc) = seed_note(&vault, "嵌入笔记", HOST_BODY);
    let source_before = HOST_BODY.to_owned();

    let (host, cx) = open_view(cx, vault, &host_doc, drawing_provider());
    let view = view_entity(&host, cx);

    // 1) 投影：整行画布嵌入是 Object（display 只有占位符），非画布嵌入保持源码。
    let display = display_text(&view, cx);
    assert!(
        display.contains(menghunote_editor::OBJECT_PLACEHOLDER),
        "整行画布嵌入必须投影成占位符：{display:?}"
    );
    assert!(
        !display.contains("![[流程图.excalidraw]]"),
        "对象行不得显示源码：{display:?}"
    );
    assert!(
        display.contains("前言") && display.contains("后文"),
        "普通正文必须原样可见：{display:?}"
    );

    // 2) 等 provider 在后台解码完成：Ready + 8×8 等比尺寸。
    settle_images(&view, cx);
    let snapshots = cx.update(|_window, app| view.read(app).drawing_snapshots());
    assert_eq!(snapshots.len(), 2, "应有两个整行画布对象：{snapshots:?}");
    assert_eq!(snapshots[0].line, 2);
    assert_eq!(snapshots[0].target, "流程图.excalidraw");
    assert_eq!(snapshots[0].alias, None);
    assert_eq!(
        snapshots[0].state,
        ObjectImageState::Ready,
        "provider 命中后必须解码为 Ready：{snapshots:?}"
    );
    assert_eq!(snapshots[0].size, Some((8.0, 8.0)));
    assert_eq!(snapshots[0].label, None, "就绪时不显示占位文案");
    assert!(!snapshots[0].source_visible);
    assert!(!snapshots[0].editing_source);
    // 第二个目标不存在（provider 返回 None）→ 缺失占位。
    assert_eq!(snapshots[1].line, 6);
    assert_eq!(snapshots[1].target, "缺失.excalidraw");
    assert_eq!(snapshots[1].state, ObjectImageState::Missing);
    assert_eq!(
        snapshots[1].label.as_deref(),
        Some("画布笔记不存在"),
        "缺失目标必须显示「画布笔记不存在」占位：{snapshots:?}"
    );

    // 3) 几何：两个对象都可见；第一个是 8×8（等比，不放大）。
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    assert_eq!(rects.len(), 2, "两个画布对象都必须可见：{rects:?}");
    let (line, ready_rect) = rects
        .iter()
        .find(|(line, _)| *line == 2)
        .copied()
        .expect("第一个画布对象必须可见");
    assert_eq!(line, 2);
    assert!(
        (ready_rect.size.width.as_f32() - 8.0).abs() < 0.5
            && (ready_rect.size.height.as_f32() - 8.0).abs() < 0.5,
        "预览图必须按原始尺寸等比绘制：{:?}",
        ready_rect.size
    );

    // 4) 单击预览：发出 WikiLinkRequested（target 取 `|` 前），不移动 caret、
    //    不进源码模式，正文一个字节都不动。
    let selection_before: Selection = cx.update(|_window, app| view.read(app).selection());
    let (events, _subscription) = subscribe_events(&view, cx);
    click_rect(cx, &ready_rect);
    assert_eq!(
        wiki_requests(&events),
        vec![("流程图.excalidraw".to_owned(), None)],
        "单击画布预览必须请求宿主打开目标画布笔记"
    );
    let after_click = display_text(&view, cx);
    assert!(
        !after_click.contains("![[流程图.excalidraw]]"),
        "点击不得进入源码模式：{after_click:?}"
    );
    let snapshots = cx.update(|_window, app| view.read(app).drawing_snapshots());
    assert!(!snapshots[0].editing_source, "点击不得进入源码模式");
    assert!(!snapshots[0].selected, "点击画布预览不得选中对象");
    assert_eq!(
        source_text(&view, cx),
        source_before,
        "点击画布预览不得改正文"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).selection()),
        selection_before,
        "点击画布预览不得移动 caret/选区"
    );

    // 5) 目标不存在时点击同样打开（由宿主解析后提示“未找到”）。
    let (missing_line, missing_rect) = rects
        .iter()
        .find(|(line, _)| *line == 6)
        .copied()
        .expect("第二个画布对象必须可见");
    assert_eq!(missing_line, 6);
    click_rect(cx, &missing_rect);
    assert_eq!(
        wiki_requests(&events),
        vec![
            ("流程图.excalidraw".to_owned(), None),
            ("缺失.excalidraw".to_owned(), None)
        ],
        "缺失目标的点击也必须交给宿主解析（不自动建笔记）"
    );
    assert_eq!(source_text(&view, cx), source_before, "点击不得改正文");
}

#[gpui::test]
async fn embedded_drawing_without_provider_shows_placeholder_and_keeps_body(
    cx: &mut TestAppContext,
) {
    let dir = temp_vault_dir("missing");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let (_node, host_doc) = seed_note(&vault, "嵌入笔记", HOST_BODY);
    let source_before = HOST_BODY.to_owned();

    // 没有 provider（默认构建 / 宿主还没接线）→ 所有画布预览都是缺失占位。
    let (host, cx) = open_view(cx, vault, &host_doc, empty_asset_provider());
    let view = view_entity(&host, cx);
    settle_images(&view, cx);

    let snapshots = cx.update(|_window, app| view.read(app).drawing_snapshots());
    assert_eq!(snapshots.len(), 2);
    for snapshot in &snapshots {
        assert_eq!(snapshot.state, ObjectImageState::Missing);
        assert_eq!(snapshot.size, None);
        assert_eq!(snapshot.label.as_deref(), Some("画布笔记不存在"));
        assert!(!snapshot.source_visible, "缺失预览仍必须是 Object");
    }
    let display = display_text(&view, cx);
    assert!(display.contains(menghunote_editor::OBJECT_PLACEHOLDER));
    assert!(!display.contains("![["));
    assert_eq!(source_text(&view, cx), source_before, "占位不得改正文");

    // 占位同样可点：仍交给宿主打开（目标不存在由宿主提示）。
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    assert_eq!(rects.len(), 2);
    let (events, _subscription) = subscribe_events(&view, cx);
    click_rect(cx, &rects[0].1);
    assert_eq!(
        wiki_requests(&events),
        vec![("流程图.excalidraw".to_owned(), None)]
    );
    assert_eq!(source_text(&view, cx), source_before);
}
