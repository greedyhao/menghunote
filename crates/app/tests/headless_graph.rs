//! 图谱覆盖层（G1 + G2）的原生 headless 测试：真实 `AppShell` + 临时 vault。
//!
//! G1 覆盖：打开/关闭、点节点打开笔记、空图提示、点遮罩关闭、Esc 关闭，
//! 外加"编辑器原有状态不被破坏""拖拽平移不误当点击""缩放有界""悬停高亮"。
//!
//! G2 覆盖：局部图默认（以当前笔记为中心、深度 1–3）与全库切换、无当前笔记回退全库、
//! 文件夹过滤、未解析链接开关（虚线占位、点占位不关覆盖层）、搜索高亮/居中/Enter 打开/
//! 空查询恢复、方向箭头开关、低缩放 LOD（不画标签/箭头、细边）。
//!
//! 纪律（AGENTS.md §6）：
//!
//! - 临时库一律建在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下，**不用** `%TEMP%`；
//! - 不 `sleep`：唯一收敛点是 `run_until_parked()`；
//! - 点击位置**不硬编码**：节点中心取自 `AppShell::graph_node_center`（与绘制同一份
//!   `GraphViewport::screen_pos`），再与 `graph-node-<i>` 的 `debug_bounds` 交叉验证，
//!   确保"画在哪就点在哪"。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    ParentElement as _, Render, ScrollDelta, ScrollEvent, ScrollSource, Size, Styled as _,
    TestAppContext, TouchPhase,
};
use menghunote_app::shell::AppShell;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 窗口根视图：只是把 `AppShell` 包一层（与 `headless_ui.rs` 同一模式）。
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
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("headless-graph-{prefix}"));
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
    writer.commit("headless-graph-seed").expect("结构提交失败");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-graph", update)],
    )
    .expect("写入正文失败");
}

/// 标题 → (`node_id`, `note_id`)：图谱节点只认 `note_id`，测试点击要用它。
fn note_ids(vault: &Vault) -> std::collections::HashMap<String, String> {
    let rows = vault
        .query("SELECT title, note_id FROM tree WHERE deleted = 0", &[])
        .expect("查树");
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (menghunote_storage::Value::Text(title), menghunote_storage::Value::Text(note_id)) =
            (&row[0], &row[1])
        else {
            panic!("tree 列类型异常");
        };
        out.insert(title.clone(), note_id.clone());
    }
    out
}

/// 直接写一条 `links` 行（图谱只读这张派生表；物化时机不在本测试范围）。
///
/// `start_byte` 同时是 PRIMARY KEY 的一部分，同对多链接必须给不同偏移。
fn seed_wiki_link(
    vault: &Vault,
    from_note_id: &str,
    to_note_id: &str,
    target: &str,
    start_byte: i64,
) {
    vault
        .exec(
            "INSERT INTO links(from_note_id,start_byte,end_byte,to_note_id,target,kind,body_seq) \
             VALUES(?1,?4,?5,?2,?3,'wiki',1)",
            &[
                menghunote_storage::Value::Text(from_note_id.to_owned()),
                menghunote_storage::Value::Text(to_note_id.to_owned()),
                menghunote_storage::Value::Text(target.to_owned()),
                menghunote_storage::Value::Integer(start_byte),
                menghunote_storage::Value::Integer(start_byte + 8),
            ],
        )
        .expect("插入 wiki 链接");
}

/// 在**顶层文件夹**里建一篇笔记（真实库里笔记的父节点是目录节点）。
///
/// 图谱的 `path`/`folder` 解析必须穿过目录行（`tree.note_id IS NULL`），
/// 只查笔记行会退化成根级——G2 的文件夹过滤就靠这条路径。
fn seed_note_in_folder(vault: &Vault, folder_title: &str, title: &str, text: &str) {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let folder_id = writer
        .doc()
        .create_folder(folder_title, 1)
        .expect("创建目录失败");
    let seed = writer.doc().create_note(title, 1).expect("创建笔记失败");
    writer
        .doc()
        .move_node(&seed.node_id, Some(&folder_id), 1)
        .expect("移动笔记到目录失败");
    writer
        .commit("headless-graph-folder-seed")
        .expect("结构提交失败");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-graph", update)],
    )
    .expect("写入正文失败");
}

/// 直接给笔记打标签（`tags`/`note_tags` 是 projector 写的派生表；图谱只读）。
fn seed_tag(vault: &Vault, note_id: &str, name: &str) {
    let tag_id = format!("tag-{name}");
    vault
        .exec(
            "INSERT INTO tags(tag_id,name,normalized_name) VALUES(?1,?2,?2) ON CONFLICT DO NOTHING",
            &[
                menghunote_storage::Value::Text(tag_id.clone()),
                menghunote_storage::Value::Text(name.to_owned()),
            ],
        )
        .expect("插入标签");
    vault
        .exec(
            "INSERT INTO note_tags(note_id,tag_id) VALUES(?1,?2) ON CONFLICT DO NOTHING",
            &[
                menghunote_storage::Value::Text(note_id.to_owned()),
                menghunote_storage::Value::Text(tag_id),
            ],
        )
        .expect("插入笔记标签");
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut gpui::VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-graph".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn view_text(cx: &mut gpui::VisualTestContext, host: &Entity<ShellHost>) -> String {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.read(app);
        let view = shell.active_editor_view().expect("活动视图");
        view.read(app).source_text()
    })
}

fn open_graph(cx: &mut gpui::VisualTestContext) {
    let button = cx
        .debug_bounds("sidebar-graph")
        .expect("侧栏底部必须有「图谱」入口");
    cx.simulate_click(button.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "点「图谱」必须打开全屏覆盖层"
    );
}

/// 节点标题（owned；`shell_read` 的返回类型不能借用 shell）。
fn node_titles(cx: &mut gpui::VisualTestContext, host: &Entity<ShellHost>) -> Vec<String> {
    shell_read(cx, host, |shell| {
        shell
            .graph_node_titles()
            .into_iter()
            .map(str::to_owned)
            .collect()
    })
}

/// `debug_bounds` 只接受 `&'static str`：测试里动态 selector 用一次性泄漏
/// （进程级、数量很小，测试结束即回收）。
fn selector(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

fn shell_read<T>(
    cx: &mut gpui::VisualTestContext,
    host: &Entity<ShellHost>,
    read: impl FnOnce(&AppShell) -> T,
) -> T {
    cx.update(|_window, app| read(host.read(app).shell.read(app)))
}

/// 打开 → Esc 关闭；编辑器还在、正文原样保留。
#[gpui::test]
async fn graph_opens_fullscreen_and_escape_closes(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("open-close");
    seed_note(&vault, "来源", "[[目标]]");
    seed_note(&vault, "目标", "目标正文");
    let ids = note_ids(&vault);
    seed_wiki_link(&vault, &ids["来源"], &ids["目标"], "目标", 0);
    let (host, cx) = open_shell(cx, vault);
    let before = view_text(cx, &host);
    assert_eq!(before, "[[目标]]", "启动应打开第一篇笔记");

    assert!(
        cx.debug_bounds("graph-overlay").is_none(),
        "启动时不应有图谱覆盖层"
    );
    open_graph(cx);

    let overlay = cx.debug_bounds("graph-overlay").expect("覆盖层");
    let canvas = cx.debug_bounds("graph-canvas").expect("画布");
    let host_bounds = cx.debug_bounds("app-shell-host").expect("窗口根视图");
    assert!(
        overlay.size.width >= host_bounds.size.width
            && overlay.size.height >= host_bounds.size.height,
        "覆盖层必须铺满窗口（不挤压编辑器）：overlay={overlay:?} host={host_bounds:?}"
    );
    assert_eq!(
        canvas.size, overlay.size,
        "画布尺寸 = 覆盖层尺寸（命中/绘制同一份几何）"
    );
    assert!(
        cx.debug_bounds("graph-hint").is_some(),
        "覆盖层必须有节点/边统计行"
    );

    // 模型口径：2 个节点、1 条边、没有未解析。
    let (nodes, edges, unresolved, titles) = shell_read(cx, &host, |shell| {
        let model = shell.graph_model().expect("图谱模型");
        (
            model.node_count(),
            model.edge_count(),
            model.unresolved_links(),
            shell.graph_node_titles().join(","),
        )
    });
    assert_eq!(nodes, 2);
    assert_eq!(edges, 1);
    assert_eq!(unresolved, 0);
    assert_eq!(
        titles, "来源,目标",
        "节点顺序按 (folder, path, title, note_id) 固定"
    );
    let hint = shell_read(cx, &host, |shell| {
        shell.graph_hint_text().unwrap_or_default()
    });
    assert!(hint.contains("节点 2") && hint.contains("边 1"), "{hint}");

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("graph-overlay").is_none(),
        "Esc 必须关闭图谱覆盖层"
    );
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "关掉覆盖层后编辑器必须还在"
    );
    assert_eq!(
        view_text(cx, &host),
        before,
        "开关覆盖层不得改动正文（编辑器状态原样保留）"
    );
}

/// 点节点 → 打开对应笔记；覆盖层同时关闭；点击位置与绘制位置一致。
#[gpui::test]
async fn clicking_node_opens_note_and_closes_overlay(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("open-node");
    seed_note(&vault, "来源", "[[目标]]");
    seed_note(&vault, "目标", "目标正文");
    let ids = note_ids(&vault);
    seed_wiki_link(&vault, &ids["来源"], &ids["目标"], "目标", 0);
    let (host, cx) = open_shell(cx, vault);
    assert_eq!(view_text(cx, &host), "[[目标]]");
    open_graph(cx);

    // 绘制位置（debug_bounds）与命中用的中心必须一致：同一份 `screen_pos`。
    let drawn = cx
        .debug_bounds("graph-node-1")
        .expect("第二个节点（目标）必须画出来");
    let center = shell_read(cx, &host, |shell| shell.graph_node_center(&ids["目标"]))
        .expect("图谱节点的屏幕中心");
    assert!(
        (drawn.center().x - center.x).as_f32().abs() < 0.5
            && (drawn.center().y - center.y).as_f32().abs() < 0.5,
        "画出来的圆点中心 {drawn:?} 必须等于命中用的中心 {center:?}"
    );

    cx.simulate_click(center, Modifiers::default());
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("graph-overlay").is_none(),
        "点节点后覆盖层必须关闭"
    );
    assert_eq!(
        view_text(cx, &host),
        "目标正文",
        "点图谱节点必须打开那篇笔记（复用 open_entry）"
    );
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "打开笔记后编辑器必须就位"
    );
}

/// 空库：只显示提示，不画节点、不 panic；点空白（遮罩）关闭。
#[gpui::test]
async fn empty_graph_shows_hint_and_backdrop_click_closes(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("empty");
    let (host, cx) = open_shell(cx, vault);
    assert!(
        shell_read(cx, &host, |shell| shell.active_editor_view().is_none()),
        "空库不该有打开的笔记"
    );
    open_graph(cx);

    assert!(
        cx.debug_bounds("graph-empty").is_some(),
        "空图必须给出占位提示"
    );
    assert!(
        cx.debug_bounds("graph-node-0").is_none(),
        "空图不该画出任何节点"
    );
    let hint = shell_read(cx, &host, |shell| {
        shell.graph_hint_text().unwrap_or_default()
    });
    assert!(hint.contains("空的"), "空图提示要写清楚：{hint}");
    assert_eq!(
        shell_read(cx, &host, |shell| {
            shell.graph_model().map_or(0, |model| model.node_count())
        }),
        0
    );

    // 点画布左下角（远离任何节点与工具条）= 点遮罩 → 关闭。
    let canvas = cx.debug_bounds("graph-canvas").expect("画布");
    let empty_spot = point(
        canvas.origin.x + px(6.0),
        canvas.origin.y + canvas.size.height - px(6.0),
    );
    cx.simulate_click(empty_spot, Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("graph-overlay").is_none(),
        "空图上点遮罩也必须能关闭"
    );
}

/// 有图时点空白（不是节点、不是工具条）= 关闭；拖拽（超过阈值）= 只平移不关。
#[gpui::test]
async fn backdrop_click_closes_but_drag_only_pans(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("backdrop");
    seed_note(&vault, "来源", "[[目标]]");
    seed_note(&vault, "目标", "目标正文");
    let ids = note_ids(&vault);
    seed_wiki_link(&vault, &ids["来源"], &ids["目标"], "目标", 0);
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);

    let canvas = cx.debug_bounds("graph-canvas").expect("画布");
    let empty_spot = point(
        canvas.origin.x + px(6.0),
        canvas.origin.y + canvas.size.height - px(6.0),
    );
    assert!(
        shell_read(cx, &host, |shell| {
            let model = shell.graph_model().expect("模型");
            let view = shell.graph_view().expect("视口");
            let size = shell.graph_viewport().expect("画布尺寸");
            view.hit_test(model, empty_spot, size).is_none()
        }),
        "这个角落不该命中任何节点"
    );

    // 1) 拖拽平移：按下 → 移动 60px → 抬手。覆盖层必须还开着，平移量变了。
    let before_pan = shell_read(cx, &host, |shell| shell.graph_view().expect("视口").pan);
    cx.simulate_mouse_down(empty_spot, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(
        point(empty_spot.x + px(60.0), empty_spot.y),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_up(
        point(empty_spot.x + px(60.0), empty_spot.y),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "拖拽平移不能把覆盖层关掉"
    );
    let after_pan = shell_read(cx, &host, |shell| shell.graph_view().expect("视口").pan);
    assert!(
        (after_pan.x - before_pan.x - 60.0).abs() < 1.0,
        "拖拽 60px 应该让平移量加 60：before={before_pan:?} after={after_pan:?}"
    );

    // 2) 真正的点击（按下抬起同一位置）= 点遮罩 → 关闭。
    cx.simulate_click(empty_spot, Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("graph-overlay").is_none(),
        "点空白（遮罩）必须关闭覆盖层"
    );
}

/// 关闭按钮与「复位」：按钮点得中、复位把平移/缩放还原。
#[gpui::test]
async fn close_button_and_reset_work(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("buttons");
    seed_note(&vault, "来源", "[[目标]]");
    seed_note(&vault, "目标", "目标正文");
    let ids = note_ids(&vault);
    seed_wiki_link(&vault, &ids["来源"], &ids["目标"], "目标", 0);
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);

    // 先把图拖走，再点「复位」：平移归零、缩放回到自适应值。
    let canvas = cx.debug_bounds("graph-canvas").expect("画布");
    let spot = point(canvas.origin.x + px(20.0), canvas.origin.y + px(120.0));
    cx.simulate_mouse_down(spot, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(
        point(spot.x + px(40.0), spot.y + px(20.0)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_up(
        point(spot.x + px(40.0), spot.y + px(20.0)),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.run_until_parked();
    let moved = shell_read(cx, &host, |shell| shell.graph_view().expect("视口"));
    assert!(moved.pan.x.abs() > 1.0, "拖拽后平移量应非零：{moved:?}");

    let reset = cx.debug_bounds("graph-reset").expect("「复位」按钮");
    cx.simulate_click(reset.center(), Modifiers::default());
    cx.run_until_parked();
    let reset_view = shell_read(cx, &host, |shell| shell.graph_view().expect("视口"));
    assert!(
        reset_view.pan.x.abs() < 0.01 && reset_view.pan.y.abs() < 0.01,
        "复位必须把平移归零：{reset_view:?}"
    );
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "点「复位」不能关掉覆盖层"
    );

    let close = cx.debug_bounds("graph-close").expect("「关闭」按钮");
    cx.simulate_click(close.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("graph-overlay").is_none(),
        "点「关闭」必须关掉覆盖层"
    );
}

/// 悬停高亮邻居 + 滚轮缩放有界（顺带验证"缩放锚点不动"的链路）。
#[gpui::test]
async fn hover_highlights_neighbors_and_zoom_is_bounded(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("hover-zoom");
    seed_note(&vault, "来源", "[[目标]]");
    seed_note(&vault, "目标", "目标正文");
    let ids = note_ids(&vault);
    seed_wiki_link(&vault, &ids["来源"], &ids["目标"], "目标", 0);
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);

    let center =
        shell_read(cx, &host, |shell| shell.graph_node_center(&ids["来源"])).expect("来源节点中心");
    cx.simulate_mouse_move(center, None, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        shell_read(cx, &host, |shell| shell.graph_hovered()),
        Some(0),
        "鼠标移到节点上必须记录悬停（高亮它和邻居）"
    );

    // 滚轮放大 200 次也不许越过 3.0；缩小 400 次也不许低于 0.2。
    let canvas = cx.debug_bounds("graph-canvas").expect("画布");
    let anchor = canvas.center();
    for _ in 0..200 {
        cx.simulate_event(ScrollEvent {
            position: anchor,
            delta: ScrollDelta::Pixels(point(px(0.0), px(120.0))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        });
    }
    cx.run_until_parked();
    let zoomed_in = shell_read(cx, &host, |shell| shell.graph_view().expect("视口").zoom);
    assert!(
        (zoomed_in - 3.0).abs() < 1e-4,
        "缩放上限必须夹在 3.0：{zoomed_in}"
    );
    for _ in 0..400 {
        cx.simulate_event(ScrollEvent {
            position: anchor,
            delta: ScrollDelta::Pixels(point(px(0.0), px(-120.0))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        });
    }
    cx.run_until_parked();
    let zoomed_out = shell_read(cx, &host, |shell| shell.graph_view().expect("视口").zoom);
    assert!(
        (zoomed_out - 0.2).abs() < 1e-4,
        "缩放下限必须夹在 0.2：{zoomed_out}"
    );
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "缩放不该关覆盖层"
    );

    // 缩放后节点仍能在新位置被点中（几何/命中/绘制同一份坐标）。
    let scaled = cx
        .debug_bounds("graph-node-1")
        .expect("缩放后节点仍在画布范围内");
    assert!(
        scaled.size.width.as_f32() > 0.0 && scaled.size.height.as_f32() > 0.0,
        "低缩放下节点仍是可见图元：{scaled:?}"
    );
    assert_eq!(
        shell_read(cx, &host, |shell| {
            let model = shell.graph_model().expect("模型");
            let view = shell.graph_view().expect("视口");
            let size = shell.graph_viewport().expect("画布尺寸");
            view.hit_test(model, scaled.center(), size)
        }),
        Some(1),
        "缩放后圆心命中必须还是那个节点"
    );
}

/// 图谱口径：未解析链接只统计、不画边；自环排除；同一对去重加权。
#[gpui::test]
async fn unresolved_links_are_counted_not_drawn(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("unresolved");
    seed_note(&vault, "来源", "[[目标]] [[不存在]] [[来源]]");
    seed_note(&vault, "目标", "目标正文");
    let ids = note_ids(&vault);
    // 两条指向目标的链接（同对多链接 → 一条边、权重 2）+ 一条未解析 + 一条自环。
    seed_wiki_link(&vault, &ids["来源"], &ids["目标"], "目标", 0);
    seed_wiki_link(&vault, &ids["来源"], &ids["目标"], "目标", 10);
    vault
        .exec(
            "INSERT INTO links(from_note_id,start_byte,end_byte,to_note_id,target,kind,body_seq) \
             VALUES(?1,20,30,NULL,'不存在','wiki',1),(?1,40,50,?1,'来源','wiki',1)",
            &[menghunote_storage::Value::Text(ids["来源"].clone())],
        )
        .expect("插入未解析/自环链接");
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);

    let (edges, weight, unresolved, self_links) = shell_read(cx, &host, |shell| {
        let model = shell.graph_model().expect("模型");
        (
            model.edge_count(),
            model.edges().first().map_or(0, |edge| edge.weight),
            model.unresolved_links(),
            model.self_links(),
        )
    });
    assert_eq!(edges, 1, "未解析/自环都不成边；同对两条合并成一条");
    assert_eq!(weight, 2, "同一对的多条链接合并后权重 = 条数");
    assert_eq!(unresolved, 1);
    assert_eq!(self_links, 1);
    let hint = shell_read(cx, &host, |shell| {
        shell.graph_hint_text().unwrap_or_default()
    });
    assert!(hint.contains("未解析 1"), "{hint}");
    assert!(hint.contains("自环 1"), "{hint}");
}

/// 窗口尺寸影响布局：窗口越小，自适应缩放越小（但仍然有界），不会 panic。
#[gpui::test]
async fn fit_zoom_respects_viewport_size(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("fit");
    seed_note(&vault, "来源", "[[目标]]");
    seed_note(&vault, "目标", "目标正文");
    let ids = note_ids(&vault);
    seed_wiki_link(&vault, &ids["来源"], &ids["目标"], "目标", 0);
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);

    let zoom_at_default = shell_read(cx, &host, |shell| shell.graph_view().expect("视口").zoom);
    cx.simulate_resize(Size::new(px(420.0), px(320.0)));
    cx.run_until_parked();
    let canvas = cx.debug_bounds("graph-canvas").expect("画布");
    assert!(
        (canvas.size.width.as_f32() - 420.0).abs() < 1.0
            && (canvas.size.height.as_f32() - 320.0).abs() < 1.0,
        "覆盖层必须跟着窗口尺寸走：{canvas:?}"
    );
    // 小窗口不重算自适应（首版只在打开时算一次），但命中/绘制仍用同一份坐标，且缩放留在界内。
    let zoom = shell_read(cx, &host, |shell| shell.graph_view().expect("视口").zoom);
    assert!(
        (0.2..=3.0).contains(&zoom),
        "缩放必须始终在 0.2–3.0：{zoom}"
    );
    let center = shell_read(cx, &host, |shell| shell.graph_node_center(&ids["目标"]))
        .expect("缩小窗口后仍能算节点中心");
    assert!(
        center.x.as_f32().is_finite() && center.y.as_f32().is_finite(),
        "坐标必须是有限值：{center:?}"
    );
    assert!(
        zoom_at_default > 0.0,
        "默认窗口下自适应缩放应为正：{zoom_at_default}"
    );
}

/// 图谱滚轮缩放时**不得**把同一次滚轮事件穿透给底层编辑器（ADR 0019 G1 回归）。
#[gpui::test]
async fn graph_wheel_zoom_does_not_scroll_the_editor_underneath(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("scroll-block");
    // 长正文：底层编辑器确实可滚动，穿透才会被检测到。
    let long = (0..160)
        .map(|index| format!("行{index}\n"))
        .collect::<String>()
        + "[[目标]]\n";
    seed_note(&vault, "长文", &long);
    seed_note(&vault, "目标", "目标正文");
    let ids = note_ids(&vault);
    seed_wiki_link(
        &vault,
        &ids["长文"],
        &ids["目标"],
        "目标",
        long.len() as i64 - 10,
    );

    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);

    let before_scroll = cx.update(|_window, app| {
        let shell = host.read(app).shell.read(app);
        shell
            .active_editor_view()
            .expect("活动编辑器视图")
            .read(app)
            .scroll_y()
    });
    assert_eq!(before_scroll, 0.0, "起始滚动位置在顶部");
    let before_zoom = shell_read(cx, &host, |shell| {
        shell.graph_view().expect("图谱视口").zoom
    });

    let canvas = cx.debug_bounds("graph-canvas").expect("画布").center();
    // 滚轮向下（内容下移）：如果穿透，底层编辑器会向下滚动。
    for _ in 0..8 {
        cx.simulate_event(ScrollEvent {
            position: canvas,
            delta: ScrollDelta::Pixels(point(px(0.0), px(-120.0))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        });
    }
    cx.run_until_parked();

    let after_zoom = shell_read(cx, &host, |shell| {
        shell.graph_view().expect("图谱视口").zoom
    });
    assert!(
        (after_zoom - before_zoom).abs() > 1e-4,
        "滚轮必须缩放图谱：before={before_zoom} after={after_zoom}"
    );
    let after_scroll = cx.update(|_window, app| {
        let shell = host.read(app).shell.read(app);
        shell
            .active_editor_view()
            .expect("活动编辑器视图")
            .read(app)
            .scroll_y()
    });
    assert_eq!(
        after_scroll, before_scroll,
        "图谱覆盖层的滚轮必须 stop_propagation：底层编辑器滚动位置不得变化"
    );
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "缩放不能关掉图谱"
    );
}

// ---------------------------------------------------------------------------
// G2：局部图 / 过滤 / 搜索 / LOD / 方向箭头
// ---------------------------------------------------------------------------

/// 默认局部图（以当前笔记为中心、2 跳）；深度 1/3 可切；切回全库后再切局部。
#[gpui::test]
async fn local_graph_defaults_to_active_note_and_switches_to_full(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("local-scope");
    seed_note(&vault, "A", "[[B]]");
    seed_note(&vault, "B", "[[C]]");
    seed_note(&vault, "C", "[[D]]");
    seed_note(&vault, "D", "终点");
    let ids = note_ids(&vault);
    seed_wiki_link(&vault, &ids["A"], &ids["B"], "B", 0);
    seed_wiki_link(&vault, &ids["B"], &ids["C"], "C", 0);
    seed_wiki_link(&vault, &ids["C"], &ids["D"], "D", 0);
    let (host, cx) = open_shell(cx, vault);
    assert_eq!(view_text(cx, &host), "[[B]]", "启动应打开第一篇笔记 A");
    open_graph(cx);

    let scope = shell_read(cx, &host, |shell| shell.graph_scope().cloned()).expect("观察范围");
    assert!(scope.is_local(), "有当前笔记时默认局部图");
    assert_eq!(
        scope.center.as_deref(),
        Some(ids["A"].as_str()),
        "局部图中心 = 当前打开的笔记"
    );
    assert_eq!(scope.clamped_depth(), 2, "默认深度 2 跳");
    assert_eq!(node_titles(cx, &host), ["A", "B", "C"], "2 跳 = 邻居的邻居");
    let hint = shell_read(cx, &host, |shell| {
        shell.graph_hint_text().unwrap_or_default()
    });
    assert!(hint.contains("局部 2 跳"), "{hint}");

    // 深度 1：只剩 A/B。
    let depth1 = cx.debug_bounds("graph-depth-1").expect("深度 1 按钮");
    cx.simulate_click(depth1.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(node_titles(cx, &host), ["A", "B"]);
    assert_eq!(
        shell_read(cx, &host, |shell| {
            shell.graph_scope().expect("范围").clamped_depth()
        }),
        1
    );

    // 深度 3：整条链。
    let depth3 = cx.debug_bounds("graph-depth-3").expect("深度 3 按钮");
    cx.simulate_click(depth3.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(node_titles(cx, &host), ["A", "B", "C", "D"]);

    // 切全库：范围变全库、深度按钮消失、统计行写「全库」。
    let full = cx.debug_bounds("graph-scope-full").expect("全库按钮");
    cx.simulate_click(full.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        !shell_read(cx, &host, |shell| shell
            .graph_scope()
            .expect("范围")
            .is_local()),
        "点「全库」必须切到全库图"
    );
    assert!(
        cx.debug_bounds("graph-depth-1").is_none(),
        "全库图没有深度按钮"
    );
    assert_eq!(node_titles(cx, &host), ["A", "B", "C", "D"]);
    let hint = shell_read(cx, &host, |shell| {
        shell.graph_hint_text().unwrap_or_default()
    });
    assert!(hint.contains("全库"), "{hint}");

    // 切回局部：中心仍是 A，深度保留 3。
    let local = cx.debug_bounds("graph-scope-local").expect("局部按钮");
    cx.simulate_click(local.center(), Modifiers::default());
    cx.run_until_parked();
    let scope = shell_read(cx, &host, |shell| shell.graph_scope().cloned()).expect("范围");
    assert!(scope.is_local());
    assert_eq!(scope.clamped_depth(), 3, "切回局部保留上次深度");
    assert_eq!(node_titles(cx, &host), ["A", "B", "C", "D"]);
}

/// 文件夹过滤：chips 来自整库模型；选中后只留该顶层文件夹的笔记，可恢复「全部」。
#[gpui::test]
async fn folder_filter_keeps_selected_folder(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("folder-filter");
    seed_note(&vault, "根笔记", "根正文");
    seed_note_in_folder(&vault, "工作", "工作笔记", "工作正文");
    seed_note_in_folder(&vault, "生活", "生活笔记", "生活正文");
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);

    // 默认局部图（中心 = 根笔记，没有边）→ 先切全库，让三个文件夹都可见。
    let full = cx.debug_bounds("graph-scope-full").expect("全库按钮");
    cx.simulate_click(full.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        node_titles(cx, &host),
        ["根笔记", "工作笔记", "生活笔记"],
        "文件夹按顶层标题升序（工作 < 生活）"
    );

    // 第一个文件夹 chip = 「工作」。
    let work = cx
        .debug_bounds("graph-folder-0")
        .expect("第一个文件夹 chip");
    cx.simulate_click(work.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(node_titles(cx, &host), ["工作笔记"]);
    let options = shell_read(cx, &host, |shell| shell.graph_options().cloned()).expect("选项");
    assert_eq!(options.folder.as_deref(), Some("工作"));
    let hint = shell_read(cx, &host, |shell| {
        shell.graph_hint_text().unwrap_or_default()
    });
    assert!(hint.contains("已隐藏 2 个节点"), "{hint}");

    // 「全部」恢复。
    let all = cx.debug_bounds("graph-folder-all").expect("「全部」chip");
    cx.simulate_click(all.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(node_titles(cx, &host), ["根笔记", "工作笔记", "生活笔记"]);
    assert!(
        shell_read(cx, &host, |shell| shell
            .graph_options()
            .expect("选项")
            .folder
            .is_none()),
        "「全部」必须清掉文件夹过滤"
    );
}

/// 未解析链接开关：默认只统计；打开后画虚线灰色占位，点占位只提示、不关覆盖层。
#[gpui::test]
async fn unresolved_toggle_draws_dashed_ghosts(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("unresolved-ghost");
    seed_note(&vault, "来源", "[[目标]] [[不存在]]");
    seed_note(&vault, "目标", "目标正文");
    let ids = note_ids(&vault);
    seed_wiki_link(&vault, &ids["来源"], &ids["目标"], "目标", 0);
    vault
        .exec(
            "INSERT INTO links(from_note_id,start_byte,end_byte,to_note_id,target,kind,body_seq) \
             VALUES(?1,10,20,NULL,'不存在','wiki',1)",
            &[menghunote_storage::Value::Text(ids["来源"].clone())],
        )
        .expect("插入未解析链接");
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);

    assert_eq!(
        shell_read(cx, &host, |shell| shell.graph_ghost_count()),
        0,
        "默认关：未解析只统计、不画"
    );
    assert!(cx.debug_bounds("graph-ghost-0").is_none());
    let hint = shell_read(cx, &host, |shell| {
        shell.graph_hint_text().unwrap_or_default()
    });
    assert!(hint.contains("未解析 1"), "{hint}");

    // 打开开关 → 一个灰色虚线占位（同一 target 只占一个位）。
    let toggle = cx
        .debug_bounds("graph-toggle-unresolved")
        .expect("未解析开关");
    cx.simulate_click(toggle.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(shell_read(cx, &host, |shell| shell.graph_ghost_count()), 1);
    let ghost = cx
        .debug_bounds("graph-ghost-0")
        .expect("未解析占位必须画出来");
    let hint = shell_read(cx, &host, |shell| {
        shell.graph_hint_text().unwrap_or_default()
    });
    assert!(hint.contains("未解析占位 1"), "{hint}");

    // 点占位：只提示，不关覆盖层、不切换笔记。
    cx.simulate_click(ghost.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "点未解析占位不能把覆盖层关掉"
    );
    assert_eq!(
        view_text(cx, &host),
        "[[目标]] [[不存在]]",
        "点未解析占位不能打开/切换笔记"
    );
    assert!(
        shell_read(cx, &host, |shell| {
            shell.notice_text().map(str::to_owned).unwrap_or_default()
        })
        .contains("未解析链接"),
        "点占位必须给出提示"
    );

    // 关掉开关：占位消失，统计仍在。
    let toggle = cx
        .debug_bounds("graph-toggle-unresolved")
        .expect("未解析开关");
    cx.simulate_click(toggle.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(shell_read(cx, &host, |shell| shell.graph_ghost_count()), 0);
    assert!(cx.debug_bounds("graph-ghost-0").is_none());
    let hint = shell_read(cx, &host, |shell| {
        shell.graph_hint_text().unwrap_or_default()
    });
    assert!(hint.contains("未解析 1"), "{hint}");
}

/// 搜索：匹配高亮 + 居中；Enter 打开；空查询恢复全景。
#[gpui::test]
async fn search_highlights_centers_and_enter_opens(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("search");
    seed_note(&vault, "阿尔法", "阿尔法正文");
    seed_note(&vault, "贝塔", "贝塔正文");
    seed_note(&vault, "伽马", "伽马正文");
    let ids = note_ids(&vault);
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);

    // 全库：三个节点都在（局部图只会留当前笔记）。
    let full = cx.debug_bounds("graph-scope-full").expect("全库按钮");
    cx.simulate_click(full.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        node_titles(cx, &host),
        ["伽马", "贝塔", "阿尔法"],
        "排序键 = (folder, path, title, note_id)（按 Unicode 码位）"
    );

    // 点搜索框 → 输入「贝塔」。
    let search = cx.debug_bounds("graph-search").expect("搜索框容器");
    cx.simulate_click(search.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_input("贝塔");
    cx.run_until_parked();

    let matches = shell_read(cx, &host, |shell| shell.graph_search_matches());
    let titles = node_titles(cx, &host);
    assert_eq!(matches.len(), 1, "只有一个匹配：{titles:?}");
    let index = matches[0];
    assert_eq!(titles[index], "贝塔");
    assert_eq!(
        shell_read(cx, &host, |shell| shell
            .graph_search_query()
            .map(str::to_owned)),
        Some("贝塔".to_owned())
    );
    // 高亮：匹配节点换成 `-match` selector（未匹配的仍是普通 selector）。
    assert!(
        cx.debug_bounds(selector(format!("graph-node-{index}-match")))
            .is_some(),
        "匹配节点必须高亮"
    );
    assert!(
        cx.debug_bounds(selector(format!("graph-node-{index}")))
            .is_none(),
        "高亮态用 `-match` selector，不再有普通 selector"
    );
    // 居中：节点中心 = 画布中心（与绘制共用 `screen_pos`）。
    let canvas = cx.debug_bounds("graph-canvas").expect("画布");
    let center =
        shell_read(cx, &host, |shell| shell.graph_node_center(&ids["贝塔"])).expect("贝塔节点中心");
    assert!(
        (center.x - canvas.center().x).as_f32().abs() < 1.0
            && (center.y - canvas.center().y).as_f32().abs() < 1.0,
        "匹配节点必须居中：center={center:?} canvas={canvas:?}"
    );

    // Enter：打开匹配的笔记并关掉覆盖层。
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("graph-overlay").is_none(),
        "Enter 必须打开笔记并关闭覆盖层"
    );
    assert_eq!(view_text(cx, &host), "贝塔正文");

    // 重新打开：搜索状态是新的（空查询），不会残留上次的高亮。
    open_graph(cx);
    assert!(
        shell_read(cx, &host, |shell| shell.graph_search_matches()).is_empty(),
        "重新打开图谱必须是空查询"
    );
}

/// 空查询恢复：清空搜索后匹配清空、视图回到自适应全景。
#[gpui::test]
async fn clearing_search_restores_the_view(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("search-clear");
    seed_note(&vault, "阿尔法", "阿尔法正文");
    seed_note(&vault, "贝塔", "贝塔正文");
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);
    let full = cx.debug_bounds("graph-scope-full").expect("全库按钮");
    cx.simulate_click(full.center(), Modifiers::default());
    cx.run_until_parked();

    let search = cx.debug_bounds("graph-search").expect("搜索框容器");
    cx.simulate_click(search.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_input("贝塔");
    cx.run_until_parked();
    let matches = shell_read(cx, &host, |shell| shell.graph_search_matches());
    assert_eq!(matches.len(), 1);
    let index = matches[0];
    let view = shell_read(cx, &host, |shell| shell.graph_view().expect("视口"));
    assert!(
        view.pan.x.abs() > 0.5 || view.pan.y.abs() > 0.5,
        "搜索定位必须改平移：{view:?}"
    );

    // 两次退格清空查询 → 匹配清空、高亮消失、视图恢复（pan = 0）。
    cx.simulate_keystrokes("backspace backspace");
    cx.run_until_parked();
    assert!(
        shell_read(cx, &host, |shell| shell.graph_search_matches()).is_empty(),
        "空查询必须清空匹配"
    );
    assert_eq!(
        shell_read(cx, &host, |shell| shell
            .graph_search_query()
            .map(str::to_owned)),
        Some(String::new())
    );
    assert!(
        cx.debug_bounds(selector(format!("graph-node-{index}-match")))
            .is_none(),
        "空查询必须清掉高亮"
    );
    let view = shell_read(cx, &host, |shell| shell.graph_view().expect("视口"));
    assert!(
        view.pan.x.abs() < 0.01 && view.pan.y.abs() < 0.01,
        "空查询必须恢复全景（pan 归零）：{view:?}"
    );
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "清空查询不能关覆盖层"
    );
}

/// 方向箭头开关 + 低缩放 LOD：低缩放不画标签/箭头、边变细。
#[gpui::test]
async fn arrows_toggle_and_low_zoom_lod(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("arrows-lod");
    seed_note(&vault, "来源", "[[目标]]");
    seed_note(&vault, "目标", "目标正文");
    let ids = note_ids(&vault);
    seed_wiki_link(&vault, &ids["来源"], &ids["目标"], "目标", 0);
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);

    // 默认关；默认缩放下 LOD 全开。
    assert!(
        !shell_read(cx, &host, |shell| shell
            .graph_options()
            .expect("选项")
            .show_arrows),
        "方向箭头默认关"
    );
    let lod = shell_read(cx, &host, |shell| shell.graph_lod().expect("LOD"));
    assert!(lod.show_labels && lod.show_arrows, "默认缩放：{lod:?}");

    // 打开方向箭头。
    let toggle = cx.debug_bounds("graph-toggle-arrows").expect("方向开关");
    cx.simulate_click(toggle.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        shell_read(cx, &host, |shell| shell
            .graph_options()
            .expect("选项")
            .show_arrows),
        "点「方向」必须打开箭头"
    );

    // 缩到最小：标签/箭头隐藏、边变细。
    let canvas = cx.debug_bounds("graph-canvas").expect("画布").center();
    for _ in 0..400 {
        cx.simulate_event(ScrollEvent {
            position: canvas,
            delta: ScrollDelta::Pixels(point(px(0.0), px(-120.0))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        });
    }
    cx.run_until_parked();
    let lod = shell_read(cx, &host, |shell| shell.graph_lod().expect("LOD"));
    assert!(!lod.show_labels, "低缩放不画标签：{lod:?}");
    assert!(!lod.show_arrows, "低缩放不画箭头：{lod:?}");
    assert!(
        (lod.edge_width - 0.5).abs() < f32::EPSILON,
        "低缩放画细边：{lod:?}"
    );
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "缩放不能关掉图谱"
    );
}

/// 没有当前笔记：回退全库图；点「局部」只提示、保持全库。
#[gpui::test]
async fn graph_without_active_note_falls_back_to_full(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("no-active");
    seed_note(&vault, "一", "一正文");
    seed_note(&vault, "二", "二正文");
    let (host, cx) = open_shell(cx, vault);
    // 关掉当前文档（模拟"没有打开的笔记"）。
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
    assert!(
        shell_read(cx, &host, |shell| shell.active_note_id().map(str::to_owned)).is_none(),
        "close_all 之后不该有打开的笔记"
    );

    open_graph(cx);
    let scope = shell_read(cx, &host, |shell| shell.graph_scope().cloned()).expect("范围");
    assert!(!scope.is_local(), "没有当前笔记 → 回退全库图");
    assert_eq!(node_titles(cx, &host), ["一", "二"]);

    // 点「局部」：没有中心，只提示、保持全库。
    let local = cx.debug_bounds("graph-scope-local").expect("局部按钮");
    cx.simulate_click(local.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        !shell_read(cx, &host, |shell| shell
            .graph_scope()
            .expect("范围")
            .is_local()),
        "没有当前笔记时不能切局部图"
    );
    assert!(
        shell_read(cx, &host, |shell| {
            shell.notice_text().map(str::to_owned).unwrap_or_default()
        })
        .contains("没有打开的笔记"),
        "必须提示为什么切不了局部图"
    );
    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "提示不能关掉覆盖层"
    );
}

/// 标签过滤：标签 chips 来自 `note_tags`（当前 app 没有打标签入口，测试直接写派生表）。
#[gpui::test]
async fn tag_filter_keeps_tagged_notes(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("tag-filter");
    seed_note(&vault, "工作笔记", "工作正文");
    seed_note(&vault, "生活笔记", "生活正文");
    let ids = note_ids(&vault);
    seed_tag(&vault, &ids["工作笔记"], "工作");
    let (host, cx) = open_shell(cx, vault);
    open_graph(cx);
    // 局部图（中心 = 工作笔记，无边）→ 切全库，两个节点都在。
    let full = cx.debug_bounds("graph-scope-full").expect("全库按钮");
    cx.simulate_click(full.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(node_titles(cx, &host), ["工作笔记", "生活笔记"]);

    // 标签 chip 只有一个（工作）；点它只留打了该标签的笔记。
    let tag = cx.debug_bounds("graph-tag-0").expect("标签 chip");
    cx.simulate_click(tag.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(node_titles(cx, &host), ["工作笔记"]);
    let options = shell_read(cx, &host, |shell| shell.graph_options().cloned()).expect("选项");
    assert_eq!(options.tag.as_deref(), Some("工作"));
    let hint = shell_read(cx, &host, |shell| {
        shell.graph_hint_text().unwrap_or_default()
    });
    assert!(hint.contains("已隐藏 1 个节点"), "{hint}");

    // 「全部」恢复。
    let all = cx.debug_bounds("graph-tag-all").expect("标签「全部」");
    cx.simulate_click(all.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(node_titles(cx, &host), ["工作笔记", "生活笔记"]);
    assert!(
        shell_read(cx, &host, |shell| shell
            .graph_options()
            .expect("选项")
            .tag
            .is_none()),
        "「全部」必须清掉标签过滤"
    );
}
