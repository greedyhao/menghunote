//! ADR 0019 S2a：app headless 图片 Object Block 集成测试。
//!
//! 真实 `AppShell` + 真实临时 vault（CARGO_TARGET_TMPDIR，不 sleep）+ **真实 PNG 资产**
//! （走 storage 的 `insert_asset` 内容寻址写入），断言：
//!
//! 1. 打开含图片的正文不 panic、不重写源码；
//! 2. 整行独占图片投影成对象（display 只有占位符，不出现 `![...]`）；行高随解码后的
//!    图片尺寸变化；
//! 3. 单击对象只选中块（描边/工具条），不展开源码；点工具条「编辑源码」/ Esc 往返；
//! 4. Delete/Backspace 删除**完整源跨度**（不是删几个字符）；
//! 5. 缺失资产的图片显示“缺失/待同步”占位，正文不动；
//! 6. 混排/行内图片首版保持源码（明确选择，见 editor 的 block 判定）。
//!
//! 像素级外观（DPI、真实窗口）仍必须真机验证；这里证明几何/事务/状态链路。

use std::path::Path;
use std::time::Duration;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::{EditorView, ObjectImageState};
use menghunote_app::shell::AppShell;
use menghunote_storage::{
    append_updates, asset_content_hash, insert_asset, load_body_doc, StructureWriter, UpdateRecord,
    Vault,
};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 6×3 的纯色 PNG：解码后宽 6 高 3（小于编辑区宽度，等比后行高 = 3 + 12）。
fn png_bytes() -> Vec<u8> {
    let buffer = image::RgbaImage::from_pixel(6, 3, image::Rgba([200, 60, 40, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("测试 PNG 编码失败");
    out.into_inner()
}

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-images-{}", std::process::id()));
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
    writer.commit("headless-images-seed").expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-images", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

/// 窗口根视图（与 headless_lists 一致：给 AppShell 一个稳定 selector）。
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
        let shell = cx.new(|cx| AppShell::new(vault, "headless-images".to_owned(), cx));
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

/// 等后台解码收敛 + 强制一帧（让 prepaint 用新的图片尺寸重算几何）。
fn settle_images(view: &Entity<EditorView>, cx: &mut VisualTestContext) {
    let settled = cx.update(|_window, app| {
        view.update(app, |view, cx| {
            view.settle_images(Duration::from_secs(10), cx)
        })
    });
    assert!(settled, "图片解码必须在有界时间内收敛");
    // 触发一帧：测试窗口的鼠标移动会走完整的 prepaint/paint。
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    cx.simulate_mouse_move(
        gpui::point(
            pane.origin.x + gpui::px(theme::PAD_X + 4.0),
            pane.origin.y + gpui::px(theme::PAD_Y + 4.0),
        ),
        None,
        Modifiers::default(),
    );
    cx.run_until_parked();
}

fn hash_hex(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[gpui::test]
async fn image_object_renders_selects_edits_and_deletes_whole_block(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");

    // 真实 PNG 资产（内容寻址）。
    let png = png_bytes();
    let hash = asset_content_hash(&png);
    insert_asset(&vault, &hash, png.len() as u64, Some("image/png"), &png).expect("写入资产失败");
    let hash = hash_hex(&hash);
    let missing = "0".repeat(64);

    let text = format!(
        "# 图片\n\n![示意](asset://{hash})\n\n说明文字\n\n![缺失](asset://{missing})\n\n前 ![内联](asset://{hash}) 后\n"
    );
    let doc_id = seed_note(&vault, "图片笔记", &text);

    let (host, cx) = open_shell(cx, vault);
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    assert!(pane.size.height.as_f32() > 0.0);
    let view = active_view(&host, cx);
    settle_images(&view, cx);

    // 1) 投影：整行独占图片是对象；混排行内图片保持源码。
    let display = cx.update(|_window, app| view.read(app).display_text());
    let placeholder = menghunote_editor::OBJECT_PLACEHOLDER;
    assert!(
        display.contains(placeholder),
        "整行独占图片必须投影成占位符：{display:?}"
    );
    assert!(
        !display.contains("![示意]"),
        "对象行不得显示图片源码：{display:?}"
    );
    assert!(
        display.contains("![内联]"),
        "混排行内图片首版保持源码（明确选择）：{display:?}"
    );
    assert!(
        display.contains("说明文字"),
        "普通正文必须原样可见：{display:?}"
    );

    // 2) 状态：真实资产 Ready、假资产 Missing。
    let snapshots = cx.update(|_window, app| view.read(app).object_snapshots());
    assert_eq!(snapshots.len(), 2, "应有两个整行独占图片：{snapshots:?}");
    assert_eq!(snapshots[0].state, ObjectImageState::Ready);
    assert_eq!(snapshots[1].state, ObjectImageState::Missing);
    assert!(!snapshots[0].editing_source);
    assert!(snapshots[0].display.is_some());

    // 3) 行高随图片：6×3 的图片等比后内容高 3px，行高 = 3 + 2×6 = 15 < 正文 22px。
    //    这里通过对象矩形高度断言（绘制/命中/布局同源几何）。
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    assert_eq!(rects.len(), 2, "两个对象都必须可见：{rects:?}");
    let (image_line, image_rect) = rects[0];
    assert_eq!(image_line, 2);
    assert!(
        (image_rect.size.width.as_f32() - 6.0).abs() < 0.5,
        "图片按内容宽度等比：{:?}",
        image_rect.size
    );
    assert!(
        (image_rect.size.height.as_f32() - 3.0).abs() < 0.5,
        "图片按内容宽度等比：{:?}",
        image_rect.size
    );

    // 4) 单击对象：只选中块（描边 + 工具条），不展开源码、不产生对象内部 caret。
    let click = gpui::point(
        image_rect.origin.x + image_rect.size.width / 2.0,
        image_rect.origin.y + image_rect.size.height / 2.0,
    );
    cx.simulate_click(click, Modifiers::default());
    cx.run_until_parked();
    let snapshots = cx.update(|_window, app| view.read(app).object_snapshots());
    assert!(snapshots[0].selected, "单击后对象必须处于选中态");
    assert!(!snapshots[0].editing_source);
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains("![示意]"),
        "选中对象不得显示源码：{display:?}"
    );
    assert!(
        cx.debug_bounds("object-toolbar").is_some(),
        "选中对象必须显示悬浮工具条"
    );
    assert!(
        cx.debug_bounds("object-toolbar-edit").is_some(),
        "工具条必须有「编辑源码」按钮"
    );
    assert!(
        cx.debug_bounds("object-toolbar-zoom").is_some(),
        "工具条必须有「放大」按钮"
    );

    // 5) 工具条「编辑源码」→ 显示完整源码；Esc 退出恢复图片。
    let edit_button = cx
        .debug_bounds("object-toolbar-edit")
        .expect("「编辑源码」按钮必须可见");
    cx.simulate_click(
        gpui::point(
            edit_button.origin.x + edit_button.size.width / 2.0,
            edit_button.origin.y + edit_button.size.height / 2.0,
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains(&format!("![示意](asset://{hash})")),
        "源码模式必须显示完整 Markdown：{display:?}"
    );
    let snapshots = cx.update(|_window, app| view.read(app).object_snapshots());
    assert!(snapshots[0].editing_source);
    assert!(snapshots[0].source_visible);

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains("![示意]") && display.contains(placeholder),
        "Esc 必须退出源码模式并恢复图片：{display:?}"
    );

    // F2 同样可以进入源码模式（先点选对象）；点击块外退出（Esc / 点击块外 / Ctrl+Enter）。
    cx.simulate_click(
        gpui::point(
            image_rect.origin.x + image_rect.size.width / 2.0,
            image_rect.origin.y + image_rect.size.height / 2.0,
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains(&format!("![示意](asset://{hash})")),
        "F2 在对象目标上必须进入源码模式：{display:?}"
    );
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    cx.simulate_click(
        gpui::point(
            pane.origin.x + gpui::px(theme::PAD_X + 40.0),
            pane.origin.y + gpui::px(theme::PAD_Y + 10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains("![示意]") && display.contains(placeholder),
        "点击块外必须退出源码模式：{display:?}"
    );

    // 6) 再点选对象后 Backspace：删除**完整源跨度**（一次事务），不留残片。
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    let image_rect = rects
        .iter()
        .find(|(line, _)| *line == 2)
        .map(|(_, rect)| *rect)
        .expect("图片对象必须仍可见");
    cx.simulate_click(
        gpui::point(
            image_rect.origin.x + image_rect.size.width / 2.0,
            image_rect.origin.y + image_rect.size.height / 2.0,
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains("![示意]"),
        "删除对象必须连整段 `![示意](...)` 一起删掉：{display:?}"
    );
    // 剩下的 `asset://` 只应来自**混排行内图片**（首版保持源码）。
    assert_eq!(
        display.matches("asset://").count(),
        1,
        "对象源码必须整段删除，不得留下残片：{display:?}"
    );
    assert!(
        display.contains("说明文字"),
        "删除对象不得影响其它正文：{display:?}"
    );
    assert!(
        !display.contains("![缺失]") && display.contains(menghunote_editor::OBJECT_PLACEHOLDER),
        "另一个对象（缺失资产）仍是对象，不得显示源码：{display:?}"
    );

    close_shell(&host, cx);
    // 7) 落库校验：只有目标图片消失，其它正文逐字节保留。
    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (after, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    let after = after.text();
    let expected = format!(
        "# 图片\n\n\n\n说明文字\n\n![缺失](asset://{missing})\n\n前 ![内联](asset://{hash}) 后\n"
    );
    assert_eq!(after, expected, "删除必须只去掉图片那一段源码");
}

#[gpui::test]
async fn backspace_next_to_an_image_selects_the_block_before_deleting(cx: &mut TestAppContext) {
    // ADR 0019 §1：Delete/Backspace **先选块、再删整个源跨度**。
    // 折叠 caret 停在图片之后（上下移动到对象行 → 落在对象末尾）：
    // 第一次 Backspace 只选块，第二次才删。
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let png = png_bytes();
    let hash = asset_content_hash(&png);
    insert_asset(&vault, &hash, png.len() as u64, Some("image/png"), &png).expect("写入资产失败");
    let hash = hash_hex(&hash);
    let text = format!("正文\n\n![示意](asset://{hash})\n\n结尾\n");
    seed_note(&vault, "退格图片", &text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_images(&view, cx);

    let rects = cx.update(|_window, app| view.read(app).object_rects());
    let (line, rect) = *rects.first().expect("图片对象必须可见");
    assert_eq!(line, 2);
    // 点图片下方的空行，再按 Up 走进对象行（上下移动落在对象末尾，不落进源码）。
    let below = gpui::point(
        rect.origin.x + gpui::px(2.0),
        rect.origin.y + rect.size.height + gpui::px(11.0),
    );
    cx.simulate_click(below, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("up");
    cx.run_until_parked();

    // 第一次 Backspace：只选中块，不改正文。
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    let source = cx.update(|_window, app| view.read(app).source_text());
    assert!(
        source.contains("![示意]"),
        "第一次 Backspace 只能选块，不得删正文：{source:?}"
    );
    let snapshots = cx.update(|_window, app| view.read(app).object_snapshots());
    assert!(
        snapshots[0].selected,
        "第一次 Backspace 后对象必须处于选中态"
    );

    // 第二次 Backspace：删除完整源跨度。
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    let source = cx.update(|_window, app| view.read(app).source_text());
    assert!(
        !source.contains("![示意]") && !source.contains(&hash),
        "第二次 Backspace 必须删掉整段源码：{source:?}"
    );
    assert_eq!(source, "正文\n\n\n\n结尾\n", "只剩空行，不留残片");
    close_shell(&host, cx);
}

#[gpui::test]
async fn wide_image_is_scaled_to_the_editor_width_with_aspect_ratio(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    // 2000×500：比编辑区宽 → 必须缩到内容宽度，并按 4:1 等比。
    let buffer = image::RgbaImage::from_pixel(2000, 500, image::Rgba([10, 120, 200, 255]));
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("测试 PNG 编码失败");
    let png = png.into_inner();
    let hash = asset_content_hash(&png);
    insert_asset(&vault, &hash, png.len() as u64, Some("image/png"), &png).expect("写入资产失败");
    let hash = hash_hex(&hash);
    seed_note(&vault, "宽图片", &format!("![宽](asset://{hash})\n"));

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_images(&view, cx);

    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    let (_, rect) = rects.first().expect("图片对象必须可见");
    let content_width = pane.size.width.as_f32() - theme::PAD_X * 2.0;
    assert!(
        (rect.size.width.as_f32() - content_width).abs() < 1.0,
        "超宽图片必须缩到内容宽度：rect={:?} content={content_width}",
        rect.size
    );
    assert!(
        (rect.size.height.as_f32() - content_width / 4.0).abs() < 1.0,
        "缩放必须保持 4:1 比例：{:?}",
        rect.size
    );
    close_shell(&host, cx);
}

#[gpui::test]
async fn text_selection_across_the_image_copies_complete_markdown(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let png = png_bytes();
    let hash = asset_content_hash(&png);
    insert_asset(&vault, &hash, png.len() as u64, Some("image/png"), &png).expect("写入资产失败");
    let hash = hash_hex(&hash);
    let text = format!("前\n\n![示意](asset://{hash})\n\n后\n");
    seed_note(&vault, "拖选图片", &text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_images(&view, cx);

    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    // 点第一行 → Home 落到行首 → Shift+Down 逐行扩选，跨过图片对象。
    cx.simulate_click(
        gpui::point(
            pane.origin.x + gpui::px(theme::PAD_X + 2.0),
            pane.origin.y + gpui::px(theme::PAD_Y + 10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    cx.simulate_keystrokes("home");
    for _ in 0..3 {
        cx.simulate_keystrokes("shift-down");
    }
    cx.run_until_parked();

    // 选区相交 → 对象进入选中态（描边），但仍不显示源码。
    let snapshots = cx.update(|_window, app| view.read(app).object_snapshots());
    assert!(snapshots[0].selected, "文本选区跨过对象必须整体选中对象");
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains("![示意]"),
        "选区相交不得展开源码：{display:?}"
    );

    // 复制 → 完整 Markdown（对象按完整源跨度还原）。
    cx.simulate_keystrokes("ctrl-c");
    cx.run_until_parked();
    let copied = cx
        .update(|_window, app| app.read_from_clipboard().and_then(|item| item.text()))
        .expect("Ctrl+C 必须写入剪贴板");
    assert!(
        copied.starts_with("前\n\n"),
        "选区必须从第一行行首开始：{copied:?}"
    );
    assert!(
        copied.contains(&format!("![示意](asset://{hash})")),
        "复制必须包含对象完整 Markdown：{copied:?}"
    );
    close_shell(&host, cx);
}

#[gpui::test]
async fn missing_image_shows_placeholder_and_does_not_touch_the_body(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let missing = "a".repeat(64);
    let text = format!("正文\n\n![待同步](asset://{missing})\n\n结尾\n");
    let doc_id = seed_note(&vault, "缺失图片", &text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_images(&view, cx);

    let snapshots = cx.update(|_window, app| view.read(app).object_snapshots());
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        snapshots[0].state,
        ObjectImageState::Missing,
        "缺失资产必须降级为占位"
    );
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains("![待同步]"),
        "缺失对象也不显示源码：{display:?}"
    );
    assert!(display.contains(menghunote_editor::OBJECT_PLACEHOLDER));

    // 打开/关闭不改写正文。
    close_shell(&host, cx);
    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (after, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    assert_eq!(after.text(), text, "打开/关闭缺失图片的笔记不得改写正文");
}

#[gpui::test]
async fn corrupt_asset_degrades_to_placeholder_without_panicking(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    // 写入坏字节的“图片”资产：解码必须失败并降级，而不是 panic。
    let bad = b"not a png".to_vec();
    let hash = asset_content_hash(&bad);
    insert_asset(&vault, &hash, bad.len() as u64, Some("image/png"), &bad).expect("写入资产失败");
    let hash = hash_hex(&hash);
    let text = format!("![坏图](asset://{hash})\n");
    seed_note(&vault, "坏图片", &text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_images(&view, cx);

    let snapshots = cx.update(|_window, app| view.read(app).object_snapshots());
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].state, ObjectImageState::Failed);
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(display.contains(menghunote_editor::OBJECT_PLACEHOLDER));
    close_shell(&host, cx);
}

#[gpui::test]
async fn image_object_zoom_button_and_second_click_open_overlay(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let png = png_bytes();
    let hash = asset_content_hash(&png);
    insert_asset(&vault, &hash, png.len() as u64, Some("image/png"), &png).expect("写入资产失败");
    let hash = hash_hex(&hash);
    let text = format!("![示意](asset://{hash})\n");
    seed_note(&vault, "放大图片", &text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_images(&view, cx);

    let rects = cx.update(|_window, app| view.read(app).object_rects());
    let (_, rect) = rects.first().expect("图片对象必须可见");
    let center = gpui::point(
        rect.origin.x + rect.size.width / 2.0,
        rect.origin.y + rect.size.height / 2.0,
    );
    cx.simulate_click(center, Modifiers::default());
    cx.run_until_parked();
    let zoom = cx
        .debug_bounds("object-toolbar-zoom")
        .expect("「放大」按钮必须可见");
    cx.simulate_click(
        gpui::point(
            zoom.origin.x + zoom.size.width / 2.0,
            zoom.origin.y + zoom.size.height / 2.0,
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("image-zoom-overlay").is_some(),
        "点放大按钮必须打开遮罩"
    );
    assert!(cx.debug_bounds("image-zoom-image").is_some());

    // 点遮罩关闭；图片仍处于选中态，再单击图片应直接打开放大。
    let overlay = cx.debug_bounds("image-zoom-overlay").unwrap();
    cx.simulate_click(overlay.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(cx.debug_bounds("image-zoom-overlay").is_none());
    // 遮罩点击会经过编辑器并清掉块选中；重新选中后再点一次才是“第二次单击”。
    cx.simulate_click(center, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_click(center, Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("image-zoom-overlay").is_some(),
        "已选中的图片再次单击必须放大"
    );
    close_shell(&host, cx);
}
