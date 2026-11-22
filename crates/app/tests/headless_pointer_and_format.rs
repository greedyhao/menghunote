//! 编辑器指针交互（mouse-up 落 caret / Shift+单击扩选 / 拖选）与 Ctrl+B / Ctrl+I
//! 格式快捷键的 headless 集成测试。
//!
//! 覆盖（真实 `AppShell` + 临时 vault + `#[gpui::test]` 窗口，**不 sleep**）：
//!
//! 1. 单击正文：mouse-down **不**落 caret（不出现非空选区、不显现隐藏的 `###`），
//!    mouse-up 才落 caret 且标题语法显现；
//! 2. Shift+单击正文文本：mouse-down 不改选区；mouse-up 从**原选区 anchor** 扩到点击处；
//!    连续两次 Shift+单击继续从同一个 anchor 扩；
//! 3. Shift+单击普通链接：**扩选**而不是打开链接；普通单击仍然打开（证明点击点有效）；
//! 4. 拖拽仍实时扩选（mouse-up 保持拖选结果，不回落成 caret）；
//! 5. Ctrl+B：有选区用 `**` 包裹、再按一次取消；一次快捷键**一个** `EditRequest`
//!    （一次 `Ctrl+Z` 只撤销一层）；Ctrl+I 同理（`*`）；
//! 6. 折叠 caret：Ctrl+I / Ctrl+B 插入成对标记、caret 在中间，紧接着打字落在标记中间；
//! 7. IME 组合中 Ctrl+B 不触发（正文/请求数都不变）。
//!
//! 纪律（AGENTS.md §6）：临时目录一律 `CARGO_TARGET_TMPDIR`；收敛点只用
//! `run_until_parked()`。像素/DPI/真实鼠标仍必须 Windows 真机验收。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
};
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

fn temp_vault(prefix: &str) -> (TempDir, Vault) {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("创建临时 vault 失败");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开 vault 失败");
    (dir, vault)
}

/// 在空 vault 里建一篇正文（结构 CRDT → 投影 → 正文 A 事务）。
fn seed_note(vault: &Vault, title: &str, text: &str) {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-pointer-seed")
        .expect("结构提交失败");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-pointer", update)],
    )
    .expect("A 事务写入正文失败");
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-pointer".to_owned(), cx));
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
        host.read(app)
            .shell
            .read(app)
            .active_editor_view()
            .expect("打开笔记后必须有活动编辑器视图")
    })
}

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn display_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).display_text())
}

/// 当前选区两端（scalar 序号）。
fn selection_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> (usize, usize) {
    cx.update(|_window, app| {
        let selection = view.read(app).selection();
        (selection.anchor().get(), selection.head().get())
    })
}

/// 源码字节 → 可点击的窗口坐标（字符左边界**左侧 0.5px**，行顶 + 8px）。
///
/// 为什么不点左边界右侧：`ShapedLine::closest_index_for_x` 对“超过最后一个字形
/// 位置”的 x 直接返回行长，点最后一个字符内部会落到行尾；边界左侧 0.5px 对任何
/// 字符都稳定命中**该字节**（与真实鼠标点字符右半同一套偏置规则）。
fn point_at_byte(
    view: &Entity<EditorView>,
    byte: usize,
    cx: &mut VisualTestContext,
) -> gpui::Point<gpui::Pixels> {
    let origin = cx
        .update(|window, app| view.read(app).point_for_source_byte(byte, window))
        .unwrap_or_else(|| panic!("源码字节 {byte} 必须能换算成窗口坐标"));
    point(origin.x - px(0.5), origin.y + px(8.0))
}

fn shift() -> Modifiers {
    Modifiers {
        shift: true,
        ..Modifiers::default()
    }
}

fn click_at(view: &Entity<EditorView>, byte: usize, cx: &mut VisualTestContext) {
    let position = point_at_byte(view, byte, cx);
    cx.simulate_click(position, Modifiers::default());
    cx.run_until_parked();
}

fn shift_click_at(view: &Entity<EditorView>, byte: usize, cx: &mut VisualTestContext) {
    let position = point_at_byte(view, byte, cx);
    cx.simulate_mouse_down(position, MouseButton::Left, shift());
    cx.run_until_parked();
    cx.simulate_mouse_up(position, MouseButton::Left, shift());
    cx.run_until_parked();
}

/// 已受理的本地编辑代数（`dev-sync` 之外的构建没有这个只读计数器）。
fn edit_seq(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> Option<u64> {
    #[cfg(feature = "sync-ui")]
    {
        Some(cx.update(|_window, app| view.read(app).dev_sync_local_edit_seq()))
    }
    #[cfg(not(feature = "sync-ui"))]
    {
        let _ = (view, cx);
        None
    }
}

#[gpui::test]
async fn single_click_commits_on_mouse_up_and_reveals_heading_markers(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("headless-pointer-up");
    // 标题在第二行：打开笔记时 caret 在第一行，`###` 应保持隐藏。
    seed_note(&vault, "标题显现", "正文第一行\n\n### 标题\n\n结尾");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    let before = display_of(&view, cx);
    assert!(
        !before.contains("###"),
        "初始 caret 不在标题行：`###` 必须隐藏，实际 {before:?}"
    );
    assert_eq!(selection_of(&view, cx), (0, 0), "初始 caret 在第一行");

    // 标题正文 `标` 的源码字节：15（正文）+ 2（空行）+ 4（`### `）= 21。
    let position = point_at_byte(&view, 21, cx);
    cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    // mouse-down：**不**改选区、不显现语法（这是本次交互修复的核心）。
    assert_eq!(
        selection_of(&view, cx),
        (0, 0),
        "mouse-down 不得直接落 caret（应等 mouse-up）"
    );
    assert!(
        !display_of(&view, cx).contains("###"),
        "mouse-down 不得提前显现隐藏语法"
    );

    cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    let (anchor, head) = selection_of(&view, cx);
    assert_eq!(anchor, head, "单击是折叠 caret（不产生非空选区）");
    assert!(
        (8..=13).contains(&head),
        "caret 必须落在标题行（scalar 8..=13），实际 {head}"
    );
    let after = display_of(&view, cx);
    assert!(
        after.contains("###"),
        "mouse-up 落 caret 后标题语法必须显现，实际 {after:?}"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn shift_click_extends_from_the_original_anchor_on_mouse_up(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("headless-shift-click");
    seed_note(&vault, "扩选", "abcdefghij");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 普通单击 A（mouse-up 才落 caret）。
    click_at(&view, 2, cx);
    assert_eq!(selection_of(&view, cx), (2, 2), "第一次单击落 caret 到 A");

    // Shift+单击 B：按下不改选区，抬起才扩选。
    let b = point_at_byte(&view, 6, cx);
    cx.simulate_mouse_down(b, MouseButton::Left, shift());
    cx.run_until_parked();
    assert_eq!(
        selection_of(&view, cx),
        (2, 2),
        "Shift+单击的 mouse-down 不得提交选区（等 mouse-up）"
    );
    cx.simulate_mouse_up(b, MouseButton::Left, shift());
    cx.run_until_parked();
    assert_eq!(
        selection_of(&view, cx),
        (2, 6),
        "Shift+单击 B：anchor 保持 A、head 到 B（非空选区）"
    );

    // 再 Shift+单击 C：继续从 A 扩到 C。
    shift_click_at(&view, 9, cx);
    assert_eq!(
        selection_of(&view, cx),
        (2, 9),
        "第二次 Shift+单击仍从原 anchor A 扩到 C"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn shift_click_on_a_link_extends_instead_of_opening(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("headless-shift-link");
    seed_note(
        &vault,
        "链接",
        "前言 abc [链接](https://example.com/x) 后文",
    );
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 先在普通文本落 caret（不落在链接上）。
    click_at(&view, 0, cx);
    let link_rect = cx
        .update(|_window, app| {
            view.read(app)
                .external_link_rects()
                .into_iter()
                .next()
                .map(|(_, _, _, rect)| rect)
        })
        .expect("必须画出普通链接矩形");

    // Shift+单击链接：扩选，不能打开外链（外链分支在 Shift 之后）。
    cx.simulate_mouse_down(link_rect.center(), MouseButton::Left, shift());
    cx.run_until_parked();
    cx.simulate_mouse_up(link_rect.center(), MouseButton::Left, shift());
    cx.run_until_parked();
    assert_eq!(
        cx.opened_url(),
        None,
        "Shift+单击链接不得打开外链（必须是扩选）"
    );
    let (anchor, head) = selection_of(&view, cx);
    assert_eq!(anchor, 0, "扩选 anchor 保持原 caret");
    assert!(head > 0, "Shift+单击链接必须产生非空选区，实际 head={head}");

    // 对照：普通单击链接仍然打开（证明点击点确实命中链接）。
    cx.simulate_click(link_rect.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://example.com/x"),
        "普通单击链接必须打开外链"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn dragging_text_still_extend_selects(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("headless-text-drag");
    seed_note(&vault, "拖选", "abcdefghijklmnop");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    let start = point_at_byte(&view, 2, cx);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(selection_of(&view, cx), (0, 0), "mouse-down 不提交选区");

    // 第一段移动只挪 1px：未越过点击阈值，仍是“点击抖动”。
    cx.simulate_mouse_move(
        point(start.x + px(1.0), start.y),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert_eq!(
        selection_of(&view, cx),
        (0, 0),
        "阈值内的抖动不得扩选（也不得提前落 caret）"
    );

    // 真正拖到 8：实时扩选。
    let end = point_at_byte(&view, 8, cx);
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        selection_of(&view, cx),
        (2, 8),
        "拖拽必须实时扩选（anchor=按下点 2，head=8）"
    );

    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        selection_of(&view, cx),
        (2, 8),
        "mouse-up 不得把拖选结果回落成 caret"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn ctrl_b_wraps_and_unwraps_with_a_single_request_per_press(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("headless-ctrl-b");
    seed_note(&vault, "粗体", "abcdef");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 用真实鼠标选出 "bcd"（scalar 1..4）。
    click_at(&view, 1, cx);
    shift_click_at(&view, 4, cx);
    assert_eq!(selection_of(&view, cx), (1, 4));

    let before = edit_seq(&view, cx);
    cx.simulate_keystrokes("ctrl-b");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "a**bcd**ef", "Ctrl+B 包裹选区");
    assert_eq!(
        selection_of(&view, cx),
        (3, 6),
        "包裹后选区保留在内层 bcd 上"
    );
    if let Some(before) = before {
        assert_eq!(
            edit_seq(&view, cx),
            Some(before + 1),
            "一次 Ctrl+B 只能产生一个 EditRequest"
        );
    }

    cx.simulate_keystrokes("ctrl-b");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "abcdef", "再按一次去掉包裹");
    assert_eq!(selection_of(&view, cx), (1, 4), "取消后保留文本选区");

    // 一次撤销只回退一层（说明每次按键是一个撤销组）。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "a**bcd**ef",
        "一次 Ctrl+Z 只撤销最后一次 Ctrl+B（一个撤销组）"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn ctrl_i_on_collapsed_caret_inserts_a_pair_and_types_between(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("headless-ctrl-i");
    seed_note(&vault, "斜体", "abcdef");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    click_at(&view, 3, cx);
    assert_eq!(selection_of(&view, cx), (3, 3));

    cx.simulate_keystrokes("ctrl-i");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "abc**def", "折叠 caret 插入成对 `*`");
    assert_eq!(
        selection_of(&view, cx),
        (4, 4),
        "caret 必须落在两个 `*` 中间"
    );

    // 紧接着打字：必须落在标记中间（`*x*`）。
    cx.simulate_input("x");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "abc*x*def");
    assert_eq!(selection_of(&view, cx), (5, 5));

    close_shell(&host, cx);
}

#[gpui::test]
async fn ctrl_b_and_i_ignore_non_ctrl_modifiers(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("headless-format-modifiers");
    seed_note(&vault, "修饰键", "abcdef");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_at(&view, 1, cx);
    shift_click_at(&view, 4, cx);
    let before = source_of(&view, cx);

    // Ctrl+Shift+B / Alt+B / Ctrl+Alt+I 都不触发格式化。
    for keys in ["ctrl-shift-b", "alt-b", "ctrl-alt-i"] {
        cx.simulate_keystrokes(keys);
        cx.run_until_parked();
    }
    assert_eq!(
        source_of(&view, cx),
        before,
        "带 Shift/Alt 的快捷键不得格式化"
    );

    close_shell(&host, cx);
}

#[cfg(feature = "sync-ui")]
#[gpui::test]
async fn ctrl_b_does_nothing_during_ime_composition(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("headless-format-ime");
    seed_note(&vault, "组合", "abcdef");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_at(&view, 1, cx);
    shift_click_at(&view, 4, cx);
    let before = source_of(&view, cx);
    let before_seq = edit_seq(&view, cx);

    // 进入真实组合态（与平台 IME 同一条 app 入口）。
    view.update(cx, |view, cx| view.dev_sync_debug_begin_composition(cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("ctrl-b");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        before,
        "IME 组合中不得格式化（正文零变化）"
    );
    assert_eq!(
        edit_seq(&view, cx),
        before_seq,
        "IME 组合中不得产生 EditRequest"
    );

    // 收尾：上屏一个字符结束组合（不把组合态带出测试）。
    view.update(cx, |view, cx| {
        view.dev_sync_debug_commit_composition("中", cx);
    });
    cx.run_until_parked();

    close_shell(&host, cx);
}
