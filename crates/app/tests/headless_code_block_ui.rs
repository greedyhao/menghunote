//! 代码块「层次感」+ 右上角「复制」按钮（headless，真实 `AppShell` + 临时 vault）。
//!
//! 覆盖用户真机反馈的两条：
//!
//! 1. fenced / indented 代码块整块有一层灰色圆角底（行内代码不铺整行底）；
//! 2. 光标/选区**不在**代码块内时，块右上角浮出复制按钮；点击只把围栏之间的源码
//!    写进系统剪贴板 + 一条非阻塞提示，**不移动 caret/选区**；光标进入块内或 IME
//!    组合中按钮隐藏，退出后恢复。
//!
//! 几何与真实绘制共用 `EditorView` 的 display 代码块快照与元素层几何：按钮位置由
//! `code-copy-<i>` 的 `debug_bounds` 断言（与 `EditorElement` 的灰底同一套行顶），
//! 灰底本身无法做像素断言，改为断言绘制时记录的 `code_block_background_count()`
//! （行内代码不计入）。
//!
//! 纪律（AGENTS.md §6）：临时目录一律在 `CARGO_TARGET_TMPDIR` 下；收敛点只用
//! `run_until_parked()`，**不 sleep**；像素/DIP 观感仍需 Windows 真机验收。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Pixels, Point, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 单块笔记：正文 → `rust` 围栏块 → 正文。
const SINGLE_BLOCK: &str = "开头正文\n\n```rust\nfn main() {}\n```\n\n结尾正文\n";
/// 双块笔记：两个 `rust` 围栏块（用内容区分谁被复制）。
const TWO_BLOCKS: &str =
    "开头正文\n\n```rust\nfn one() {}\n```\n\n中间正文\n\n```rust\nfn two() {}\n```\n\n结尾正文\n";

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

fn temp_vault_dir(name: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-code-block-ui-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 在空 vault 里建一篇正文（结构 CRDT → 投影 → 正文 A 事务）。
fn seed_note(vault: &Vault, title: &str, text: &str) {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-code-block-ui-seed")
        .expect("结构提交失败");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-code-block-ui", update)],
    )
    .expect("A 事务写入正文失败");
}

fn open_shell<'a>(
    cx: &'a mut TestAppContext,
    vault: Vault,
    name: &str,
) -> (Entity<ShellHost>, &'a mut VisualTestContext) {
    let name = format!("headless-code-block-ui-{name}");
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, name.clone(), cx));
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

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn display_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).display_text())
}

fn caret_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> (usize, usize) {
    cx.update(|_window, app| {
        let selection = view.read(app).selection();
        (selection.anchor().get(), selection.head().get())
    })
}

fn link_notice(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_window, app| view.read(app).link_notice_text().map(str::to_owned))
}

fn clipboard_text(cx: &mut VisualTestContext) -> Option<String> {
    cx.read_from_clipboard().and_then(|item| item.text())
}

fn background_count(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> usize {
    cx.update(|_window, app| view.read(app).code_block_background_count())
}

/// 点击一个 debug selector 目标（找不到就失败：浮层必须真实画出来）。
fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("`{selector}` 必须可见"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 点击源码字节 `byte` 所在的行内位置（与绘制/命中共用 `point_for_source_byte` 几何）。
///
/// 返回实际点击的窗口坐标。行顶 + 6px / 字节起点 + 2px：稳稳落在该行该字符内部。
fn click_source_byte(
    view: &Entity<EditorView>,
    cx: &mut VisualTestContext,
    byte: usize,
) -> Point<Pixels> {
    let origin = cx
        .update(|window, app| view.read(app).point_for_source_byte(byte, window))
        .expect("正文布局几何必须存在（否则无法点击）");
    let position = point(origin.x + px(2.0), origin.y + px(6.0));
    cx.simulate_click(position, Modifiers::default());
    cx.run_until_parked();
    position
}

/// 种子正文里 `needle` 第一个字符之后第 `offset` 个字节（跳过命中文字本身的位移）。
fn byte_of(source: &str, needle: &str, offset: usize) -> usize {
    source
        .find(needle)
        .unwrap_or_else(|| panic!("种子正文必须包含 {needle:?}"))
        + offset
}

/// 光标在块外：点第一行正文（代码块上方）。
fn click_outside_block(view: &Entity<EditorView>, cx: &mut VisualTestContext, source: &str) {
    click_source_byte(view, cx, byte_of(source, "开头正文", 2));
}

#[gpui::test]
async fn copy_button_copies_fence_content_without_moving_caret(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("copy");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "复制代码", SINGLE_BLOCK);
    let (host, cx) = open_shell(cx, vault, "copy");
    let view = active_view(&host, cx);
    let source = source_of(&view, cx);

    // 1) 光标在代码块外的正文 → 右上角出现复制按钮。
    click_outside_block(&view, cx, &source);
    let button = cx
        .debug_bounds("code-copy-0")
        .expect("光标在代码块外时必须有复制按钮（code-copy-0）");
    // 位置钉住（写死口径）：贴块右上角 —— 右边距 = PAD_X，按钮 26×22。
    let doc = cx
        .debug_bounds("editor-current-doc")
        .expect("编辑器正文必须画出来");
    assert_eq!(
        button.origin.x + button.size.width,
        doc.origin.x + doc.size.width - px(theme::PAD_X),
        "按钮右边缘必须与代码块底色右边缘对齐（距右 PAD_X）"
    );
    assert_eq!(
        (button.size.width, button.size.height),
        (px(26.0), px(22.0)),
        "复制按钮尺寸固定 26×22（不要做成常驻大按钮）"
    );
    let caret_before = caret_of(&view, cx);

    // 2) 点击按钮：剪贴板是**围栏之间**的源码（不含 ```rust / 收尾围栏）。
    click_selector(cx, "code-copy-0");
    assert_eq!(
        clipboard_text(cx).as_deref(),
        Some("fn main() {}\n"),
        "必须复制围栏之间的源码原文（不含围栏与 info string）"
    );
    assert_eq!(
        caret_of(&view, cx),
        caret_before,
        "点击复制按钮不得移动 caret/选区"
    );
    assert_eq!(
        link_notice(&view, cx).as_deref(),
        Some("已复制代码"),
        "复制后必须有非阻塞提示"
    );
    assert!(
        cx.debug_bounds("editor-link-notice").is_some(),
        "提示必须真实画出来（复用 link_notice 文案机制）"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn copy_button_hides_inside_block_and_returns_after_exit(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("hide");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "按钮显隐", SINGLE_BLOCK);
    let (host, cx) = open_shell(cx, vault, "hide");
    let view = active_view(&host, cx);
    let source = source_of(&view, cx);

    click_outside_block(&view, cx, &source);
    assert!(cx.debug_bounds("code-copy-0").is_some(), "块外必须有按钮");

    // 点进代码内容：围栏显现（证明 caret 真的进了块），按钮让位。
    click_source_byte(&view, cx, byte_of(&source, "fn main() {}", 3));
    let revealed = display_of(&view, cx);
    assert!(
        revealed.contains("```rust"),
        "点击代码内容后围栏必须显现（caret 已进入代码块）：{revealed:?}"
    );
    assert!(
        cx.debug_bounds("code-copy-0").is_none(),
        "caret 在代码块内时复制按钮必须隐藏"
    );

    // 退出块 → 按钮恢复。
    click_outside_block(&view, cx, &source);
    assert!(
        cx.debug_bounds("code-copy-0").is_some(),
        "光标退出代码块后复制按钮必须恢复"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn only_non_caret_block_shows_button_and_copy_keeps_caret(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("two");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "两个代码块", TWO_BLOCKS);
    let (host, cx) = open_shell(cx, vault, "two");
    let view = active_view(&host, cx);
    let source = source_of(&view, cx);

    // 块外：两个可见代码块各有一个按钮（序号按本帧画出的按钮顺序）。
    click_outside_block(&view, cx, &source);
    assert!(cx.debug_bounds("code-copy-0").is_some(), "第一块必须有按钮");
    assert!(
        cx.debug_bounds("code-copy-1").is_some(),
        "第二块也可见时同样必须有按钮"
    );

    // caret 进入第一块：第一块的按钮隐藏，剩第二块（序号 0）。
    click_source_byte(&view, cx, byte_of(&source, "fn one() {}", 3));
    assert!(
        cx.debug_bounds("code-copy-0").is_some(),
        "第一块被 caret 占据后，第二块必须仍显示按钮"
    );
    assert!(
        cx.debug_bounds("code-copy-1").is_none(),
        "被 caret 占据的块不占按钮序号"
    );

    let caret_before = caret_of(&view, cx);
    click_selector(cx, "code-copy-0");
    assert_eq!(
        clipboard_text(cx).as_deref(),
        Some("fn two() {}\n"),
        "剩下的按钮必须复制第二块（非 caret 块）"
    );
    assert_eq!(
        caret_of(&view, cx),
        caret_before,
        "复制第二块不得把 caret 从第一块挪走"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn fenced_block_paints_one_background_and_inline_code_does_not(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("bg");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(
        &vault,
        "灰底计数",
        "行内 `code` 与正文\n\n```rust\nfn main() {}\n```\n\n结尾\n",
    );
    let (host, cx) = open_shell(cx, vault, "bg");
    let view = active_view(&host, cx);
    let source = source_of(&view, cx);

    // 光标放在第一行正文（代码块外）；这一行里还有行内代码 `code`。
    click_source_byte(&view, cx, byte_of(&source, "行内", 1));
    assert_eq!(
        background_count(&view, cx),
        1,
        "只给整块代码块铺底：行内代码不铺整行底（否则会数成 2）"
    );

    // 进入块内仍然只有这一块（围栏显现不改变块数）。
    click_source_byte(&view, cx, byte_of(&source, "fn main() {}", 3));
    assert_eq!(
        background_count(&view, cx),
        1,
        "caret 进入代码块后灰底数量不变"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn ime_composition_hides_copy_button(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("ime");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "组合隐藏按钮", SINGLE_BLOCK);
    let (host, cx) = open_shell(cx, vault, "ime");
    let view = active_view(&host, cx);
    let source = source_of(&view, cx);

    click_outside_block(&view, cx, &source);
    assert!(cx.debug_bounds("code-copy-0").is_some(), "块外必须有按钮");

    // F8 与真实平台 IME 共用 `update_preedit` 路径（不是直接改测试状态）。
    cx.simulate_keystrokes("f8");
    cx.run_until_parked();
    assert!(
        cx.update(|_window, app| view.read(app).is_composing()),
        "F8 必须真的进入 IME 组合"
    );
    assert!(
        cx.debug_bounds("code-copy-0").is_none(),
        "IME 组合中复制按钮必须隐藏（临时布局与 display 区间会错位）"
    );

    // 取消组合 → 按钮恢复（组合中不画、也不该永久消失）。
    cx.simulate_keystrokes("f10");
    cx.run_until_parked();
    assert!(
        !cx.update(|_window, app| view.read(app).is_composing()),
        "F10 必须结束组合"
    );
    assert!(
        cx.debug_bounds("code-copy-0").is_some(),
        "组合结束后复制按钮必须恢复"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn indented_code_block_copies_whole_block(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("indented");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(
        &vault,
        "缩进代码块",
        "开头正文\n\n    缩进代码一行\n    缩进代码二行\n\n结尾正文\n",
    );
    let (host, cx) = open_shell(cx, vault, "indented");
    let view = active_view(&host, cx);
    let source = source_of(&view, cx);

    click_outside_block(&view, cx, &source);
    assert!(
        cx.debug_bounds("code-copy-0").is_some(),
        "缩进代码块（indented）同样必须有复制按钮"
    );
    click_selector(cx, "code-copy-0");
    assert_eq!(
        clipboard_text(cx).as_deref(),
        Some("    缩进代码一行\n    缩进代码二行\n"),
        "indented 块的 content 是整块（含缩进空格，不加围栏）"
    );

    close_shell(&host, cx);
}
