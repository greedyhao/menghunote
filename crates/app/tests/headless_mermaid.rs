//! ADR 0019 M2：app headless Mermaid Object Block 集成测试。
//!
//! 真实 `AppShell` + 真实临时 vault（`CARGO_TARGET_TMPDIR`，**不 sleep**）+ 真实
//! merman 渲染 + GPUI `SvgRenderer` 光栅化。断言：
//!
//! 1. 打开含 mermaid 的笔记不 panic、不重写源码；
//! 2. 顶层 fenced mermaid 投影成 Object（display 只有占位符），后台渲染收敛为
//!    Ready；失败块显示「Mermaid 渲染失败」占位并可回退到源码；
//! 3. 单击只选中块（描边/工具条），工具条有「编辑源码」「重新渲染」、**没有**放大；
//! 4. 工具条/F2 进入源码模式显示完整 fenced 源码，Esc 恢复对象；
//! 5. 复制得到完整 fenced 源码；重新渲染会再执行一次（缓存命中时帧循环不重复渲染）；
//! 6. 非普通段落上下文（列表里的 mermaid）保持普通代码块源码。
//!
//! 像素级外观（DPI/字体）仍必须真机验证；这里证明投影/状态机/事务/几何链路。

use std::path::Path;
use std::time::Duration;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::{EditorView, MermaidObjectState};
use menghunote_app::shell::AppShell;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-mermaid-{}", std::process::id()));
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
        .commit("headless-mermaid-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-mermaid", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

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
        let shell = cx.new(|cx| AppShell::new(vault, "headless-mermaid".to_owned(), cx));
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

/// 强制一帧（让 prepaint 登记 Mermaid 请求），再等后台渲染收敛，最后再强制一帧
/// 让新的图片尺寸进入几何。收敛点是 `wait_idle`（Condvar），**不 sleep**。
fn settle_mermaid(view: &Entity<EditorView>, cx: &mut VisualTestContext) {
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    let probe = gpui::point(
        pane.origin.x + gpui::px(theme::PAD_X + 4.0),
        pane.origin.y + gpui::px(theme::PAD_Y + 4.0),
    );
    cx.simulate_mouse_move(probe, None, Modifiers::default());
    cx.run_until_parked();
    let settled = cx.update(|_window, app| {
        view.update(app, |view, cx| {
            view.settle_mermaid(Duration::from_secs(30), cx)
        })
    });
    assert!(settled, "Mermaid 渲染必须在有界时间内收敛");
    cx.simulate_mouse_move(probe, None, Modifiers::default());
    cx.run_until_parked();
}

/// 只强制一帧（让 prepaint 跑一遍），不等待后台渲染。
fn force_frame(cx: &mut VisualTestContext) {
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

#[gpui::test]
async fn mermaid_waits_until_the_caret_leaves_the_block(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    // 文档从 mermaid block 开始，默认 caret = block 起点 → 打开时源码可见、不投影 Object。
    let text = "```mermaid\nflowchart TD\nA-->B\n```\n\n后文\n";
    seed_note(&vault, "延迟渲染", text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    force_frame(cx);

    // 1) caret 在 block 内：display 是完整源码，不出现 Object/工具条，零请求。
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains("```mermaid\nflowchart TD\nA-->B\n```"),
        "caret 在 block 内必须直接显示完整源码：{display:?}"
    );
    assert!(
        cx.update(|_window, app| view.read(app).object_rects())
            .is_empty(),
        "caret 在 block 内不得出现 Object 矩形"
    );
    assert!(
        cx.debug_bounds("object-toolbar").is_none(),
        "caret 在 block 内不得出现工具条"
    );
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].state, MermaidObjectState::Waiting);
    assert_eq!(
        snapshots[0].label.as_deref(),
        Some("等待光标移出后渲染…"),
        "等待态必须显示明确文案，而不是失败"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_request_count()),
        0
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_render_count()),
        0
    );
    // 源码可见时布局必须回落到普通 fenced code 行高；否则 caret 会被撑成占位块
    // 高度（用户实测的“大光标”）。这里直接钉住视觉行度量。
    let metrics = cx.update(|_window, app| view.read(app).visual_row_metrics());
    let mermaid_row = metrics
        .iter()
        .find(|metric| metric.bytes.0 == 0)
        .expect("Mermaid 开围栏行必须可见");
    assert!(
        (mermaid_row.line_height - theme::DEFAULT_LINE_HEIGHT).abs() < 0.01,
        "Mermaid 源码行高必须是普通行高 {}，实际 {}",
        theme::DEFAULT_LINE_HEIGHT,
        mermaid_row.line_height
    );

    // 2) 在内容行直接编辑（不经过 F2）：仍保持源码可见、不请求。
    cx.simulate_keystrokes("down");
    cx.simulate_input(" ");
    cx.run_until_parked();
    force_frame(cx);
    let source = cx.update(|_window, app| view.read(app).source_text());
    assert!(
        source.contains(" flowchart TD"),
        "源码必须可直接编辑：{source:?}"
    );
    assert!(
        cx.update(|_window, app| view.read(app).object_rects())
            .is_empty(),
        "编辑中不得投影 Object"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_request_count()),
        0
    );

    // 3) 移出 block（4 行之后到空行）→ Object 出现、只请求一次、Ready。
    for _ in 0..4 {
        cx.simulate_keystrokes("down");
    }
    cx.run_until_parked();
    force_frame(cx);
    let settled = cx.update(|_window, app| {
        view.update(app, |view, cx| {
            view.settle_mermaid(Duration::from_secs(30), cx)
        })
    });
    assert!(settled, "移出后渲染必须收敛");
    force_frame(cx);
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains("```mermaid") && display.contains("\u{FFFC}"),
        "移出后必须恢复 Object 占位：{display:?}"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).object_rects())
            .len(),
        1,
        "移出后必须出现 Object 矩形"
    );
    let requests = cx.update(|_window, app| view.read(app).mermaid_request_count());
    let renders = cx.update(|_window, app| view.read(app).mermaid_render_count());
    assert_eq!(requests, 1, "移出 block 后只应请求一次");
    assert_eq!(renders, 1);
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(snapshots[0].state, MermaidObjectState::Ready);

    // 4) 已有缓存不受影响：caret 再进 block → 源码可见/无 Object；移出 → 直接 Ready，
    //    不产生第二次请求/渲染。
    cx.update(|_window, app| {
        view.update(app, |view, cx| view.enter_object_source_edit(0, cx));
    });
    cx.run_until_parked();
    force_frame(cx);
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains("```mermaid"),
        "工具条「编辑源码」必须能重新进入源码"
    );
    assert!(cx
        .update(|_window, app| view.read(app).object_rects())
        .is_empty());
    cx.update(|_window, app| {
        view.update(app, |view, cx| view.exit_object_source_edit(cx));
    });
    cx.run_until_parked();
    force_frame(cx);
    assert_eq!(
        cx.update(|_window, app| view.read(app).object_rects())
            .len(),
        1,
        "退出源码后必须恢复 Object"
    );
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(snapshots[0].state, MermaidObjectState::Ready);
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_request_count()),
        1
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_render_count()),
        1
    );

    // 多次强制帧不会重复请求。
    for _ in 0..3 {
        force_frame(cx);
    }
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_request_count()),
        1
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn empty_or_unclosed_mermaid_never_requests_and_never_fails(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    // 空内容：block 是合法的；caret 在 block 内时显示源码，移出后是等待占位（不是失败）。
    let text = "```mermaid\n```\n\n后文\n";
    seed_note(&vault, "空 mermaid", text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    force_frame(cx);
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains("```mermaid"),
        "caret 在 block 内显示源码：{display:?}"
    );
    assert!(cx
        .update(|_window, app| view.read(app).object_rects())
        .is_empty());

    // 移出 block 后：空 Mermaid 仍保持源码可见，不出现“等待渲染”占位，也不请求。
    for _ in 0..2 {
        cx.simulate_keystrokes("down");
    }
    cx.run_until_parked();
    force_frame(cx);
    assert!(
        cx.update(|_window, app| view.read(app).object_rects())
            .is_empty(),
        "空 Mermaid 不应投影成 Object 占位"
    );
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains("```mermaid"),
        "空 Mermaid 移出后仍应显示源码：{display:?}"
    );
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(snapshots.len(), 1);
    assert!(
        snapshots[0].source_visible,
        "空 Mermaid 的源码必须可见：{snapshots:?}"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_request_count()),
        0
    );
    assert!(
        snapshots[0].label.as_deref() != Some("Mermaid 渲染失败"),
        "空内容不得进入 Failed"
    );
    close_shell(&host, cx);

    // 未闭合围栏：markdown 不产生 Object，因此也不会有渲染请求。
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let text = "```mermaid\nflowchart TD\nA-->B\n";
    seed_note(&vault, "未闭合 mermaid", text);
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    force_frame(cx);
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert!(snapshots.is_empty(), "未闭合围栏不是 Object：{snapshots:?}");
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_request_count()),
        0
    );
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains("flowchart TD"),
        "未闭合块保持源码：{display:?}"
    );
    close_shell(&host, cx);
}

#[gpui::test]
async fn mermaid_object_renders_selects_edits_and_rerenders(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");

    let valid_source = "flowchart TD\nA-->B";
    let invalid_source = "flowchart TD\nA[未闭合";
    let text = format!(
        "# 图\n\n```mermaid\n{valid_source}\n```\n\n说明\n\n```mermaid\n{invalid_source}\n```\n\n结尾\n"
    );
    let doc_id = seed_note(&vault, "Mermaid 笔记", &text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_mermaid(&view, cx);

    // 1) 投影：两个顶层 fenced mermaid 都是 Object；源码不出现。
    let placeholder = menghunote_editor::OBJECT_PLACEHOLDER;
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert_eq!(
        display.matches(placeholder).count(),
        2,
        "两个 mermaid 块都必须投影成占位符：{display:?}"
    );
    assert!(
        !display.contains("```mermaid") && !display.contains("flowchart"),
        "对象行不得显示 fenced 源码：{display:?}"
    );
    assert!(display.contains("说明") && display.contains("结尾"));

    // 2) 状态：块 0 渲染成功，块 1 失败（失败是终态占位，不阻塞编辑）。
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(snapshots.len(), 2, "应有两个 Mermaid 对象：{snapshots:?}");
    assert_eq!(snapshots[0].state, MermaidObjectState::Ready);
    assert_eq!(snapshots[1].state, MermaidObjectState::Failed);
    assert_eq!(snapshots[1].label.as_deref(), Some("Mermaid 渲染失败"));
    assert_eq!(snapshots[0].info, "mermaid");
    assert!(snapshots[0].size.is_some(), "就绪后必须有逻辑尺寸");
    assert!(!snapshots[0].editing_source);

    // 3) 几何：成功块按 SVG 自然尺寸（168×156）绘制，失败块是占位高度。
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    assert_eq!(rects.len(), 2, "两个对象行都必须可见：{rects:?}");
    let (valid_line, valid_rect) = rects[0];
    assert_eq!(valid_line, 2);
    assert!(
        (valid_rect.size.width.as_f32() - 168.0).abs() < 1.0,
        "小图不放大：{:?}",
        valid_rect.size
    );
    assert!(
        (valid_rect.size.height.as_f32() - 156.0).abs() < 1.0,
        "按 SVG viewBox 等比：{:?}",
        valid_rect.size
    );
    let (failed_line, failed_rect) = rects[1];
    assert_eq!(failed_line, 9);
    assert!(
        (failed_rect.size.height.as_f32() - theme::MERMAID_PLACEHOLDER_HEIGHT).abs() < 1.0,
        "失败占位高度：{:?}",
        failed_rect.size
    );

    // 4) 单击只选中块；工具条有编辑/重新渲染，没有放大。
    let click = gpui::point(
        valid_rect.origin.x + valid_rect.size.width / 2.0,
        valid_rect.origin.y + valid_rect.size.height / 2.0,
    );
    cx.simulate_click(click, Modifiers::default());
    cx.run_until_parked();
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert!(snapshots[0].selected, "单击后对象必须选中");
    assert!(!snapshots[0].editing_source, "单击不得展开源码");
    assert!(cx.debug_bounds("object-toolbar").is_some());
    assert!(cx.debug_bounds("object-toolbar-edit").is_some());
    assert!(cx.debug_bounds("object-toolbar-rerender").is_some());
    assert!(
        cx.debug_bounds("object-toolbar-zoom").is_none(),
        "Mermaid 不得出现放大按钮"
    );

    // 4b) 点对象**末尾区域**（底边内侧）：仍保持 Object，不得自动切源码。
    let bottom_edge = gpui::point(
        valid_rect.origin.x + valid_rect.size.width / 2.0,
        valid_rect.origin.y + valid_rect.size.height - gpui::px(1.0),
    );
    cx.simulate_click(bottom_edge, Modifiers::default());
    cx.run_until_parked();
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert!(!snapshots[0].source_visible, "点末尾区域不得切源码");
    assert!(!snapshots[0].editing_source);
    assert_eq!(
        cx.update(|_window, app| view.read(app).object_rects())
            .len(),
        2,
        "末尾点击后对象仍应存在"
    );

    // 5) Ctrl+C：选中对象复制完整 fenced 源码。
    cx.simulate_keystrokes("ctrl-c");
    cx.run_until_parked();
    let copied = cx
        .update(|_window, app| app.read_from_clipboard().and_then(|item| item.text()))
        .expect("Ctrl+C 必须写入剪贴板");
    assert_eq!(
        copied,
        format!("```mermaid\n{valid_source}\n```"),
        "复制必须包含完整 fenced 源码"
    );

    // 6) 工具条「编辑源码」→ 完整源码；Esc 恢复对象。
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
        display.contains(&format!("```mermaid\n{valid_source}\n```")),
        "源码模式必须显示完整 fenced 源码：{display:?}"
    );
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert!(snapshots[0].editing_source);
    assert!(snapshots[0].source_visible);

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.matches(placeholder).count() == 2,
        "Esc 必须恢复对象占位：{display:?}"
    );

    // 7) F2 同样进入源码模式。
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    let valid_rect = rects
        .iter()
        .find(|(line, _)| *line == 2)
        .map(|(_, rect)| *rect)
        .expect("mermaid 对象必须仍可见");
    cx.simulate_click(
        gpui::point(
            valid_rect.origin.x + valid_rect.size.width / 2.0,
            valid_rect.origin.y + valid_rect.size.height / 2.0,
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains(&format!("```mermaid\n{valid_source}\n```")),
        "F2 必须进入源码模式：{display:?}"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    // 8) 缓存：帧循环/源码往返不重复渲染；「重新渲染」恰好再渲染一次。
    let renders_before = cx.update(|_window, app| view.read(app).mermaid_render_count());
    assert_eq!(renders_before, 2, "两个块各渲染一次：{renders_before}");
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    for _ in 0..3 {
        cx.simulate_mouse_move(
            gpui::point(
                pane.origin.x + gpui::px(theme::PAD_X + 8.0),
                pane.origin.y + gpui::px(theme::PAD_Y + 8.0),
            ),
            None,
            Modifiers::default(),
        );
        cx.run_until_parked();
    }
    let renders_after_frames = cx.update(|_window, app| view.read(app).mermaid_render_count());
    assert_eq!(
        renders_after_frames, renders_before,
        "同一源码不得重复渲染（缓存命中）"
    );

    // 重新渲染失败块：再渲染一次，状态仍是失败（可反复试，不写正文）。
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    let failed_rect = rects
        .iter()
        .find(|(line, _)| *line == 9)
        .map(|(_, rect)| *rect)
        .expect("失败对象必须可见");
    cx.simulate_click(
        gpui::point(
            failed_rect.origin.x + failed_rect.size.width / 2.0,
            failed_rect.origin.y + failed_rect.size.height / 2.0,
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    let rerender_button = cx
        .debug_bounds("object-toolbar-rerender")
        .expect("重新渲染按钮必须可见");
    cx.simulate_click(
        gpui::point(
            rerender_button.origin.x + rerender_button.size.width / 2.0,
            rerender_button.origin.y + rerender_button.size.height / 2.0,
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    let settled = cx.update(|_window, app| {
        view.update(app, |view, cx| {
            view.settle_mermaid(Duration::from_secs(30), cx)
        })
    });
    assert!(settled, "重新渲染必须收敛");
    let renders_after_retry = cx.update(|_window, app| view.read(app).mermaid_render_count());
    assert_eq!(
        renders_after_retry,
        renders_before + 1,
        "重新渲染必须恰好再执行一次"
    );
    // 单击对象会把 caret 放到 block 外（Object 保持），重渲染后读到失败终态。
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(snapshots[1].state, MermaidObjectState::Failed);

    // 9) 正文从未被渲染改写。
    let source = cx.update(|_window, app| view.read(app).source_text());
    assert_eq!(source, text, "预览/失败/重渲染都不得改正文");
    close_shell(&host, cx);

    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (after, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    assert_eq!(after.text(), text, "落库正文逐字节不变");
}

#[gpui::test]
async fn mermaid_failure_keeps_source_fallback_and_never_touches_body(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let text = "前\n\n```mermaid\n这不是图\n```\n\n后\n";
    seed_note(&vault, "失败 mermaid", text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_mermaid(&view, cx);

    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].state, MermaidObjectState::Failed);
    assert_eq!(snapshots[0].label.as_deref(), Some("Mermaid 渲染失败"));
    let source_before = cx.update(|_window, app| view.read(app).source_text());

    // 失败对象仍可编辑源码：完整 fenced 源码可见，不产生任何编辑请求。
    let rects = cx.update(|_window, app| view.read(app).object_rects());
    let (_, rect) = rects.first().expect("失败对象必须有占位矩形");
    cx.simulate_click(
        gpui::point(
            rect.origin.x + rect.size.width / 2.0,
            rect.origin.y + rect.size.height / 2.0,
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
    cx.simulate_keystrokes("f2");
    cx.run_until_parked();
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains("```mermaid\n这不是图\n```"),
        "失败回退必须显示完整源码：{display:?}"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains(menghunote_editor::OBJECT_PLACEHOLDER),
        "Esc 后恢复失败占位：{display:?}"
    );
    let source_after = cx.update(|_window, app| view.read(app).source_text());
    assert_eq!(source_before, source_after, "失败回退不得改正文");
    close_shell(&host, cx);
}

#[gpui::test]
async fn nested_mermaid_stays_ordinary_code(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let text = "- item\n\n  ```mermaid\n  flowchart TD\n  A-->B\n  ```\n";
    seed_note(&vault, "列表内 mermaid", text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_mermaid(&view, cx);

    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert!(
        snapshots.is_empty(),
        "列表里的 mermaid 不是 Object：{snapshots:?}"
    );
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        display.contains("flowchart TD"),
        "非 Object 的 mermaid 保持普通代码块源码：{display:?}"
    );
    assert!(
        !display.contains(menghunote_editor::OBJECT_PLACEHOLDER),
        "不得产生对象占位：{display:?}"
    );
    close_shell(&host, cx);
}
