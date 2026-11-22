//! 外部链接增强（ADR 0019 backlog「外部链接」）：Ctrl/Cmd+单击、右键菜单、
//! scheme 策略、内部链接。
//!
//! 真实 `AppShell` + 临时 vault（`CARGO_TARGET_TMPDIR`、不 sleep）+ 真实
//! `#[gpui::test]` 窗口。覆盖：
//!
//! 1. 普通单击即打开（Ctrl/Cmd 同样打开）；
//! 2. 链接右键菜单三项（打开链接 / 复制链接 / 复制 Markdown），动作后不移动 caret；
//! 3. Zotero 原样交给系统；`file:` 先确认；未知 scheme 确认并记住（本机偏好）；
//!    危险 scheme 拒绝且不回显路径；
//! 4. 内部链接：`#heading` 滚动/定位到标题；相对笔记路径打开对应笔记；
//! 5. 链接末尾边界不误触。
//!
//! 像素级外观（手形光标、确认框阴影、DPI）仍必须真机验证；这里证明交互与策略链路。

use std::path::{Path, PathBuf};

use gpui::{
    div, AppContext as _, Bounds, Entity, InteractiveElement as _, Modifiers, MouseButton,
    ParentElement as _, Pixels, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::shell::AppShell;
use menghunote_editor::SourceByte;
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

fn temp_vault() -> (TempDir, Vault) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-links-{}", std::process::id()));
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
    writer.commit("headless-links-seed").expect("结构提交失败");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-links", update)],
    )
    .expect("写入正文失败");
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-links".to_owned(), cx));
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

fn active_view(
    cx: &mut VisualTestContext,
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
    cx: &mut VisualTestContext,
    view: &Entity<menghunote_app::editor::view::EditorView>,
) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn caret_of(
    view: &Entity<menghunote_app::editor::view::EditorView>,
    cx: &mut VisualTestContext,
) -> (usize, usize) {
    cx.update(|_window, app| {
        let selection = view.read(app).selection();
        (selection.anchor().get(), selection.head().get())
    })
}

fn link_notice(
    view: &Entity<menghunote_app::editor::view::EditorView>,
    cx: &mut VisualTestContext,
) -> Option<String> {
    cx.update(|_window, app| view.read(app).link_notice_text().map(str::to_owned))
}

fn clipboard_text(cx: &mut VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

/// Ctrl（macOS Cmd）+ 单击的修饰键。
fn open_modifiers() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers {
            platform: true,
            ..Modifiers::default()
        }
    } else {
        Modifiers {
            control: true,
            ..Modifiers::default()
        }
    }
}

/// 普通链接在窗口坐标里的矩形（与链接 hitbox 同源）。
fn link_rect(
    view: &Entity<menghunote_app::editor::view::EditorView>,
    cx: &mut VisualTestContext,
) -> Bounds<Pixels> {
    cx.update(|_window, app| {
        view.read(app)
            .external_link_rects()
            .into_iter()
            .next()
            .map(|(_, _, _, rect)| rect)
            .expect("必须画出普通链接矩形")
    })
}

fn scalar_of_byte(source: &str, byte: usize) -> usize {
    SourceByte::new(byte)
        .to_scalar(source)
        .expect("测试用字节必须是 scalar 边界")
        .get()
}

/// 点击一个 debug selector 目标（找不到就失败：浮层/菜单项必须真实画出来）。
fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
async fn ordinary_click_opens_http_link(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    seed_note(&vault, "链接", "[链接](https://example.com/x)");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);
    assert_eq!(caret_of(&view, cx), (0, 0), "打开笔记时 caret 在文档开头");

    cx.simulate_click(rect.center(), Modifiers::default());
    cx.run_until_parked();

    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/x"),
        "普通单击即打开链接"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "打开链接不移动 caret");

    close_shell(&host, cx);
}

#[gpui::test]
async fn ctrl_click_opens_http_link_without_moving_caret(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    seed_note(&vault, "链接", "[链接](https://example.com/x)");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);

    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();

    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/x"),
        "Ctrl（macOS Cmd）+单击同样交给系统打开"
    );
    assert_eq!(caret_of(&view, cx), (0, 0), "打开链接不得移动 caret");

    close_shell(&host, cx);
}

#[gpui::test]
async fn link_end_boundary_does_not_open(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    seed_note(&vault, "边界", "[链接](https://example.com/x)");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);
    let ranges = cx.update(|_window, app| view.read(app).external_link_display_ranges());
    let (start, end, _) = ranges[0].clone();
    let (start_hit, end_hit) = cx.update(|_window, app| {
        (
            view.read(app).external_link_hit_at_display(start).is_some(),
            view.read(app).external_link_hit_at_display(end).is_some(),
        )
    });
    assert!(start_hit, "链接起点必须命中");
    assert!(!end_hit, "链接末尾边界（display == end）不能算命中");

    // 点末尾右侧 1px：不打开，正常落 caret。
    cx.simulate_click(
        gpui::point(rect.right() + gpui::px(1.0), rect.center().y),
        open_modifiers(),
    );
    cx.run_until_parked();
    assert_eq!(cx.opened_url(), None, "末尾边界不得误触打开");

    close_shell(&host, cx);
}

#[gpui::test]
async fn zotero_link_is_passed_through_verbatim(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    let url = "zotero://select/groups/123/items/KEY?foo=bar#frag";
    seed_note(&vault, "zotero", &format!("[Z]({url})"));
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);

    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();

    assert_eq!(
        cx.opened_url().as_deref(),
        Some(url),
        "Zotero 链接必须原样交给系统（不解析/不改写）"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn file_link_requires_confirmation(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    let url = "file:///home/user/secret.txt";
    seed_note(&vault, "file", &format!("[F]({url})"));
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);

    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-link-confirm").is_some(),
        "file: 打开前必须弹确认框"
    );
    assert_eq!(cx.opened_url(), None, "确认前不得打开");
    assert_eq!(caret_of(&view, cx), (0, 0), "确认框不移动 caret");

    click_selector(cx, "editor-link-confirm-open");
    assert_eq!(cx.opened_url().as_deref(), Some(url), "确认后原样交给系统");
    assert!(
        cx.debug_bounds("editor-link-confirm").is_none(),
        "确认后关闭浮层"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn file_link_confirmation_can_be_cancelled(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    let url = "file:///home/user/secret.txt";
    seed_note(&vault, "file", &format!("[F]({url})"));
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);

    // 「取消」：不开、不改正文、caret 不动。
    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();
    click_selector(cx, "editor-link-confirm-cancel");
    assert_eq!(cx.opened_url(), None, "取消后不得打开");
    assert!(
        cx.debug_bounds("editor-link-confirm").is_none(),
        "取消必须关闭确认框"
    );
    assert_eq!(caret_of(&view, cx), (0, 0));

    // `Esc` 同样取消。
    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();
    assert!(cx.debug_bounds("editor-link-confirm").is_some());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(cx.opened_url(), None, "Esc 后不得打开");
    assert!(
        cx.debug_bounds("editor-link-confirm").is_none(),
        "Esc 必须关闭确认框"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn unknown_scheme_confirms_once_and_remembers(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    let url = "menghunote-link-test://open?x=1";
    seed_note(&vault, "自定义", &format!("[U]({url})"));
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);
    // 本机偏好文件落到 target/tmp（不碰真实用户配置）。
    let pref_path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("link-prefs-{}", std::process::id()))
        .join("link-schemes.txt");
    let _ = std::fs::remove_dir_all(pref_path.parent().expect("父目录"));
    menghunote_app::editor::link_settings::reset_for_tests(Some(pref_path.clone()));

    // 第一次：确认 + 记住。
    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-link-confirm").is_some(),
        "未知自定义 scheme 首次必须确认"
    );
    assert!(
        cx.debug_bounds("editor-link-confirm-remember").is_some(),
        "未知自定义 scheme 必须提供「记住」"
    );
    assert_eq!(cx.opened_url(), None);
    click_selector(cx, "editor-link-confirm-remember");
    click_selector(cx, "editor-link-confirm-open");
    assert_eq!(cx.opened_url().as_deref(), Some(url), "确认后打开");

    // 第二次：直接打开，不再确认。
    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-link-confirm").is_none(),
        "记住之后不再确认"
    );
    assert_eq!(cx.opened_url().as_deref(), Some(url));

    // 本机偏好只存 scheme（不写 URL/正文）。
    let raw = std::fs::read_to_string(&pref_path).expect("偏好文件必须落盘");
    assert!(
        raw.contains("menghunote-link-test"),
        "记住的 scheme：{raw:?}"
    );
    assert!(
        !raw.contains("://"),
        "偏好文件只存 scheme，不存 URL：{raw:?}"
    );
    assert_eq!(
        view_text(cx, &view),
        format!("[U]({url})"),
        "记住偏好不得改写正文"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn dangerous_scheme_is_rejected_without_echoing_path(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    seed_note(&vault, "危险", "[J](javascript:alert(1))");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);

    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();

    assert_eq!(cx.opened_url(), None, "危险 scheme 绝不能打开");
    assert!(
        cx.debug_bounds("editor-link-notice").is_some(),
        "拒绝必须给非阻塞提示"
    );
    let notice = link_notice(&view, cx).expect("必须有提示文案");
    assert!(
        notice.contains("javascript"),
        "提示必须说明被拒绝的 scheme：{notice}"
    );
    assert!(
        !notice.contains("alert"),
        "提示不得回显链接路径/参数：{notice}"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn link_context_menu_has_three_actions_and_keeps_caret(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    let url = "https://example.com/x";
    seed_note(&vault, "菜单", &format!("开头\n\n[链接]({url})\n"));
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);

    // 右键链接：三个菜单项可见，caret 不动。
    cx.simulate_mouse_down(rect.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-context-menu").is_some(),
        "右键链接必须弹出菜单"
    );
    for selector in [
        "editor-context-open-link",
        "editor-context-copy-link",
        "editor-context-copy-markdown",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "菜单项 `{selector}` 必须可见"
        );
    }
    assert_eq!(caret_of(&view, cx), (0, 0), "右键本身不得移动 caret");

    // 复制链接：剪贴板是 destination 原文；caret 不动。
    click_selector(cx, "editor-context-copy-link");
    assert_eq!(clipboard_text(cx).as_deref(), Some(url));
    assert_eq!(caret_of(&view, cx), (0, 0), "复制动作不得移动 caret");

    // 复制 Markdown：完整源码 `[text](dest)`。
    cx.simulate_mouse_down(rect.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    click_selector(cx, "editor-context-copy-markdown");
    assert_eq!(clipboard_text(cx), Some(format!("[链接]({url})")));
    assert_eq!(caret_of(&view, cx), (0, 0));

    // 打开链接：与 Ctrl+单击同一路径；caret 不动。
    cx.simulate_mouse_down(rect.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    click_selector(cx, "editor-context-open-link");
    assert_eq!(cx.opened_url().as_deref(), Some(url));
    assert_eq!(caret_of(&view, cx), (0, 0), "打开动作不得移动 caret");

    close_shell(&host, cx);
}

#[gpui::test]
async fn internal_heading_link_scrolls_and_places_caret(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    let mut text = String::from("# 第一章\n\n[跳转](#第二章)\n\n");
    for index in 0..40 {
        text.push_str(&format!("第 {index} 行\n\n"));
    }
    text.push_str("## 第二章\n\n结尾\n");
    seed_note(&vault, "标题", &text);
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);

    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();

    assert_eq!(cx.opened_url(), None, "内部链接不得交给系统打开");
    let heading_byte = text.find("## 第二章").expect("标题行");
    let heading = scalar_of_byte(&text, heading_byte);
    assert_eq!(
        caret_of(&view, cx),
        (heading, heading),
        "caret 必须定位到标题行首"
    );
    assert!(
        cx.update(|_window, app| view.read(app).scroll_y()) > 0.0,
        "必须滚动到标题（不能停在文档顶部）"
    );
    assert_eq!(link_notice(&view, cx), None, "找到标题时不应有提示");

    close_shell(&host, cx);
}

#[gpui::test]
async fn relative_note_link_opens_the_note(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    seed_note(&vault, "来源", "[目标](目标)");
    seed_note(&vault, "目标", "目标正文");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);

    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();

    let after = active_view(cx, &host);
    assert_eq!(
        view_text(cx, &after),
        "目标正文",
        "相对笔记路径必须打开对应笔记"
    );
    assert_eq!(cx.opened_url(), None, "内部笔记链接不走系统打开");

    close_shell(&host, cx);
}

#[gpui::test]
async fn unresolved_relative_note_link_only_notices(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault();
    seed_note(&vault, "来源", "[不存在](不存在的笔记)");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(cx, &host);
    let rect = link_rect(&view, cx);

    cx.simulate_click(rect.center(), open_modifiers());
    cx.run_until_parked();

    assert_eq!(
        view_text(cx, &view),
        "[不存在](不存在的笔记)",
        "解析不到不得自动建笔记/切换"
    );
    assert_eq!(cx.opened_url(), None);
    let notice = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .notice_text()
            .map(str::to_owned)
    });
    assert!(
        notice
            .as_deref()
            .is_some_and(|notice| notice.contains("未找到")),
        "必须给非阻塞提示：{notice:?}"
    );

    close_shell(&host, cx);
}
