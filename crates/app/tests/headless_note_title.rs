//! 笔记顶部大标题的点击定位 / 拖动选区 / 粘贴 / 新建后直接改名
//! （headless，真实 `AppShell` + 临时 vault）。
//!
//! 覆盖用户反馈「拷贝的文字没法在笔记标题栏粘贴；点标题直接全选」以及追加需求
//! 「新建笔记后光标先停在标题处」「标题支持鼠标拖动选择文字」：
//!
//! 1. 进入标题编辑时**不预选**（初始选区 0 长度，不是全选）；
//! 2. 点击标题的位置决定光标（首次进入 / 编辑态内再次点击），不是行尾/全选；
//! 3. 按住在标题上拖动把选区扩到指针处（正向/反向/拖出元素边界），抬起保留选区；
//!    没拖动的按下仍按单击处理（折叠光标落在按下位置）；
//! 4. `Ctrl+V` / `Cmd+V` 把剪贴板文本按单行过滤后插入；有选区时替换选区；
//!    拖动后 `Ctrl+A` 全选、粘贴也仍然正常；
//! 5. 新建普通笔记成功后自动进入标题编辑并聚焦，默认名预选，键入即整体替换。
//!
//! 纪律（AGENTS.md §6）：临时目录用 `CARGO_TARGET_TMPDIR`；收敛点只用
//! `run_until_parked()`（不 sleep、不依赖系统时间/窗口焦点）。

use std::path::Path;
use std::time::Duration;

use gpui::{
    div, point, px, AppContext as _, ClipboardItem, Entity, EntityInputHandler as _,
    InteractiveElement as _, Modifiers, MouseButton, ParentElement as _, Render, Styled as _,
    TestAppContext, VisualTestContext,
};
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_app::sidebar_rename::{RenameInput, RenameState, CARET_BLINK_INTERVAL};
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 30 个 CJK 字符（90 字节）：长到足以在窗口里点中"中间"，又不需要横向滚动。
const LONG_TITLE: &str = "这是一个用于测试点击定位的非常长的笔记标题一二三四五六七八九十";

/// 拖动选区用的 CJK / ASCII 混排标题（29 字节）。
///
/// 字符下标 → byte：项=0 目=3 A=6 l=7 p=8 h=9 a=10 笔=11 记=14 标=17
/// 题=20 测=23 试=26。混排用来验证"位置 → byte"不会把 ASCII 的 1 字节字符与
/// CJK 的 3 字节字符按同一个字节步长处理。
const MIXED_TITLE: &str = "项目Alpha笔记标题测试";

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
        "headless-note-title-{prefix}-{}",
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

/// 正文 A 事务：把 `text` 写进已存在的 body doc（与 `headless_excalidraw` 同一条路径）。
fn write_body(vault: &Vault, body_doc_id: &str, text: &str) {
    let (doc, head) = load_body_doc(vault, body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-note-title", update)],
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
        .commit("headless-note-title-seed")
        .expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, text);
    seed.node_id
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-note-title".to_owned(), cx));
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

/// 编辑态标题输入框的矩形（`note-title-edit` 与 `RenameElement` 共用左边界）。
fn title_edit_bounds(cx: &mut VisualTestContext) -> gpui::Bounds<gpui::Pixels> {
    cx.debug_bounds("note-title-edit")
        .expect("编辑态必须有标题输入框")
}

/// 第 `char_index` 个字符**起点**附近的窗口坐标（y 取标题行中线）。
///
/// 测试文本系统（`NoopTextSystem`）每字符 advance = 0.6 × 28px = 16.8px，
/// 与大标题绘制/shaping 用的是同一个字号；多 +1px 让按下点稳稳落在字符内。
fn title_char_point(cx: &mut VisualTestContext, char_index: usize) -> gpui::Point<gpui::Pixels> {
    let bounds = title_edit_bounds(cx);
    point(
        bounds.origin.x + px(16.8 * char_index as f32) + px(1.0),
        bounds.center().y,
    )
}

/// 在标题上按住拖动（down → move → up），全程走 GPUI 原生鼠标事件链路。
fn drag_title(cx: &mut VisualTestContext, from_char: usize, to_char: usize) {
    let from = title_char_point(cx, from_char);
    let to = title_char_point(cx, to_char);
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
}

/// 取消标题编辑（`Esc`），避免临时 vault 关闭时残留输入框。
fn cancel_title_edit(cx: &mut VisualTestContext) {
    cx.simulate_keystrokes("escape");
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

fn active_title(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_window, app| host.read(app).shell.read(app).active_note_title())
}

fn title_state(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> RenameState {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .title_edit_input()
            .expect("标题编辑必须打开")
            .read(app)
            .state()
            .clone()
    })
}

/// 编辑态标题输入框的实体（闪烁相位与上一帧绘制状态都在它上面）。
fn title_input(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Entity<RenameInput> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .title_edit_input()
            .expect("标题编辑必须打开")
    })
}

/// `(caret 相位是否可见, 上一帧是否真的画了 caret)`。
///
/// 两个值必须一起看：只翻转状态而没重绘时，用户屏幕上的光标还停在旧相位。
fn caret_blink(input: &Entity<RenameInput>, cx: &mut VisualTestContext) -> (bool, bool) {
    cx.update(|_window, app| {
        let input = input.read(app);
        (
            input.caret_blink_visible(),
            input.caret_painted_last_frame(),
        )
    })
}

/// 推进一个闪烁半周期并让队列（含重绘）落定；不 sleep，全靠测试虚拟时钟。
fn advance_half_period(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(CARET_BLINK_INTERVAL);
    cx.run_until_parked();
}

fn close_shell(host: &Entity<ShellHost>, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn opening_title_edit_does_not_select_all(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("no-preselect");
    seed_note(&vault, LONG_TITLE, "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");

    let state = title_state(&host, cx);
    assert_eq!(state.text(), LONG_TITLE, "点标题进入编辑不能改文本");
    let selection = state.selected_byte_range();
    assert_eq!(
        selection.start, selection.end,
        "进入标题编辑必须是 collapsed 选区，不能整段全选：{selection:?}"
    );
    assert!(
        !(selection.start == 0 && selection.end == state.text().len()),
        "全选 0..len 是用户反馈的 bug，必须消失"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn clicking_title_places_caret_at_the_clicked_character(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("click-caret");
    seed_note(&vault, LONG_TITLE, "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");

    // 测试文本系统（NoopTextSystem）每字符 advance = 0.6 × font_size；大标题 28px
    // → 16.8px/字符。点 x = origin + 336px 落在第 21 个字符的起点（CJK 3 字节 → 60）。
    let title_bounds = cx.debug_bounds("note-title").expect("标题行必须可见");
    cx.simulate_click(
        point(title_bounds.origin.x + px(336.0), title_bounds.center().y),
        Modifiers::default(),
    );
    cx.run_until_parked();

    let state = title_state(&host, cx);
    assert_eq!(
        state.caret(),
        60,
        "首次点标题必须把光标放到点击位置（第 21 个字符起点）"
    );
    assert_ne!(state.caret(), 0, "不能落行首");
    assert_ne!(state.caret(), state.text().len(), "不能落行尾");
    assert_eq!(state.selected_byte_range(), 60..60, "点击定位不能顺带全选");
    assert_eq!(
        state.selection_utf16(),
        20..20,
        "UTF-16 坐标必须是字符边界（CJK 每字 1 个 UTF-16 码元）"
    );

    // 编辑态内再次点击：第 11 个字符起点（10 × 16.8 = 168px → byte 30）。
    let edit_bounds = cx
        .debug_bounds("note-title-edit")
        .expect("编辑态必须有标题输入框");
    cx.simulate_click(
        point(edit_bounds.origin.x + px(168.0), edit_bounds.center().y),
        Modifiers::default(),
    );
    cx.run_until_parked();
    let state = title_state(&host, cx);
    assert_eq!(state.caret(), 30, "编辑态内再次点击也必须按点击位置放光标");
    assert!(
        cx.update(|_window, app| host.read(app).shell.read(app).title_edit_input().is_some()),
        "点输入框内部不能提交/退出标题编辑"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn dragging_title_selects_forward_and_keeps_the_selection(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-forward");
    seed_note(&vault, MIXED_TITLE, "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");

    // 从「项」左侧拖到「笔」起点：byte 0 → 11 = "项目Alpha"（CJK + ASCII 混排）。
    drag_title(cx, 0, 7);

    let state = title_state(&host, cx);
    assert_eq!(
        state.selected_byte_range(),
        0..11,
        "拖动必须选中 0..11（字符 0 起点到字符 7=「笔」起点），实际文本={:?}",
        state.text()
    );
    assert_eq!(
        &state.text()[state.selected_byte_range()],
        "项目Alpha",
        "选中的是 CJK+ASCII 混排子串（字节边界必须按 UTF-8 字符算）"
    );
    assert!(!state.is_reversed(), "从左往右拖是正向选区");
    assert_eq!(state.caret(), 11, "head 必须停在拖动终点");
    assert!(
        cx.update(|_window, app| host.read(app).shell.read(app).title_edit_input().is_some()),
        "拖动/抬起不能提交或退出标题编辑"
    );
    assert!(
        cx.debug_bounds("note-title-edit").is_some(),
        "拖选后输入框仍在（不能因为鼠标手势触发失焦/提交）"
    );

    cancel_title_edit(cx);
    close_shell(&host, cx);
}

#[gpui::test]
async fn dragging_title_backwards_keeps_direction_and_selected_text(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-backward");
    seed_note(&vault, MIXED_TITLE, "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");

    // 反向：从「笔」起点拖回「项」左侧。anchor 在后 → 选区反向，但内容一致。
    drag_title(cx, 7, 0);

    let state = title_state(&host, cx);
    assert_eq!(
        state.selected_byte_range(),
        0..11,
        "反向拖动的选区范围（起点 ≤ 终点）仍是 0..11"
    );
    assert!(
        state.is_reversed(),
        "从右往左拖必须保持反向选择（anchor 在后）"
    );
    assert_eq!(
        &state.text()[state.selected_byte_range()],
        "项目Alpha",
        "反向拖动选中的内容必须与正向一致"
    );
    assert_eq!(state.caret(), 0, "反向拖动 head 在选区头");

    cancel_title_edit(cx);
    close_shell(&host, cx);
}

#[gpui::test]
async fn clicking_title_without_moving_still_places_a_collapsed_caret(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-click");
    seed_note(&vault, MIXED_TITLE, "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");

    // down/up 同点（第 4 个字符「p」起点，byte 8）：没有拖动 → 仍按单击定位。
    let at = title_char_point(cx, 4);
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();

    let state = title_state(&host, cx);
    assert_eq!(
        state.selected_byte_range(),
        8..8,
        "单击（没有移动）必须保持折叠光标，不能冒出选区"
    );
    assert_eq!(state.caret(), 8, "光标落在点击位置（「p」= byte 8）");
    assert!(!state.is_reversed());
    assert_eq!(state.selection_utf16(), 4..4, "UTF-16 坐标同步正确");

    cancel_title_edit(cx);
    close_shell(&host, cx);
}

#[gpui::test]
async fn select_all_and_paste_still_work_after_dragging_a_selection(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-then-shortcuts");
    seed_note(&vault, MIXED_TITLE, "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");

    drag_title(cx, 0, 7);
    assert_eq!(
        title_state(&host, cx).selected_byte_range(),
        0..11,
        "先拖出一段选区"
    );

    // Ctrl+A 仍应全选（拖选状态不能把键盘快捷键吞掉）。
    cx.simulate_keystrokes("ctrl-a");
    cx.run_until_parked();
    let state = title_state(&host, cx);
    assert_eq!(
        state.selected_byte_range(),
        0..MIXED_TITLE.len(),
        "拖动后 Ctrl+A 仍要全选"
    );

    // Ctrl+C 必须把标题选区复制到系统剪贴板（先覆盖旧内容，证明真的写了）。
    cx.write_to_clipboard(ClipboardItem::new_string("旧剪贴板".to_owned()));
    cx.simulate_keystrokes("ctrl-c");
    cx.run_until_parked();
    let copied = cx
        .update(|_window, app| app.read_from_clipboard())
        .and_then(|item| item.text());
    assert_eq!(
        copied.as_deref(),
        Some(MIXED_TITLE),
        "标题 Ctrl+C 必须复制当前选区，而不是被 ClipboardUnsupported 吞掉"
    );

    // 有选区时粘贴 = 整体替换（与之前的语义一致）。
    cx.write_to_clipboard(ClipboardItem::new_string("新标题".to_owned()));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    let state = title_state(&host, cx);
    assert_eq!(state.text(), "新标题", "拖选后粘贴仍必须正常");
    assert_eq!(
        state.selected_byte_range(),
        state.text().len()..state.text().len(),
        "粘贴后选区折叠到插入文本之后"
    );

    cancel_title_edit(cx);
    close_shell(&host, cx);
}

#[gpui::test]
async fn dragging_outside_the_title_still_extends_the_selection(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-outside");
    seed_note(&vault, MIXED_TITLE, "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");

    // 按在第 2 个字符「A」（byte 6）上，然后把指针拖到标题右侧很远的地方。
    let bounds = title_edit_bounds(cx);
    let from = title_char_point(cx, 2);
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    let far_right = point(
        bounds.origin.x + bounds.size.width + px(160.0),
        bounds.center().y,
    );
    cx.simulate_mouse_move(far_right, Some(MouseButton::Left), Modifiers::default());
    cx.run_until_parked();
    let state = title_state(&host, cx);
    assert_eq!(
        state.selected_byte_range(),
        6..MIXED_TITLE.len(),
        "拖出标题右边界仍必须扩到行尾（不能停在中途）"
    );
    assert!(!state.is_reversed());

    // 反向拖出左边界：选区扩到行首，方向变反，不 panic。
    let far_left = point(bounds.origin.x - px(160.0), bounds.center().y);
    cx.simulate_mouse_move(far_left, Some(MouseButton::Left), Modifiers::default());
    cx.run_until_parked();
    let state = title_state(&host, cx);
    assert_eq!(
        state.selected_byte_range(),
        0..6,
        "反向拖出左边界必须扩到行首"
    );
    assert!(state.is_reversed());

    // 抬起后选区保留，编辑态仍在。
    cx.simulate_mouse_up(far_left, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    let state = title_state(&host, cx);
    assert_eq!(state.selected_byte_range(), 0..6, "抬起后必须保留选区");
    assert!(
        cx.debug_bounds("note-title-edit").is_some(),
        "拖出边界再抬起不能提交/退出标题编辑"
    );

    cancel_title_edit(cx);
    close_shell(&host, cx);
}

#[gpui::test]
async fn ctrl_v_pastes_into_the_title_and_replaces_selection(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("paste");
    seed_note(&vault, "原标题", "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");

    // 多行 + 制表符：标题按单行过滤（丢弃），不产生换行。
    cx.write_to_clipboard(ClipboardItem::new_string("粘贴\n的\t标题".to_owned()));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    let state = title_state(&host, cx);
    assert_eq!(
        state.text(),
        "原标题粘贴的标题",
        "Ctrl+V 必须把剪贴板文本插入标题，并过滤换行/制表符"
    );
    assert!(
        !state.text().contains('\n')
            && !state.text().contains('\r')
            && !state.text().contains('\t')
    );
    assert_eq!(
        state.selected_byte_range(),
        state.text().len()..state.text().len(),
        "粘贴后选区 collapsed、光标在插入文本之后"
    );

    // 有选区时粘贴 = 替换选区：Ctrl+A 后用 Cmd+V（macOS 语义，headless 同样走
    // `paste_shortcut` 的 platform 分支）。
    cx.simulate_keystrokes("ctrl-a");
    cx.write_to_clipboard(ClipboardItem::new_string("替换\n标题".to_owned()));
    cx.simulate_keystrokes("cmd-v");
    cx.run_until_parked();
    let state = title_state(&host, cx);
    assert_eq!(state.text(), "替换标题", "有选区时粘贴必须整体替换选区");

    // Enter 走既有的提交路径，标题同步到侧栏/文件名。
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(active_title(&host, cx).as_deref(), Some("替换标题"));
    assert!(
        cx.debug_bounds("note-title-edit").is_none(),
        "Enter 提交后退出标题编辑"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn new_note_opens_title_edit_with_default_name_selected(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("new-note-title");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "sidebar-new-note");

    assert!(
        cx.debug_bounds("note-title-edit").is_some(),
        "新建普通笔记后必须直接进入标题编辑"
    );
    let state = title_state(&host, cx);
    assert_eq!(state.text(), "未命名笔记", "默认标题");
    assert_eq!(
        state.selected_byte_range(),
        0..state.text().len(),
        "新建笔记的默认名必须预选，用户一次输入即可整体替换"
    );
    let focused = cx.update(|window, app| {
        host.read(app)
            .shell
            .read(app)
            .title_edit_input()
            .expect("标题编辑必须打开")
            .read(app)
            .is_focused(window)
    });
    assert!(focused, "新建笔记后焦点必须在标题输入框");

    let view = active_view(&host, cx);
    cx.simulate_input("我的笔记");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "", "键入必须进标题，不能进正文");
    assert_eq!(
        title_state(&host, cx).text(),
        "我的笔记",
        "预选的默认名被键入整体替换"
    );

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(active_title(&host, cx).as_deref(), Some("我的笔记"));
    assert!(
        cx.debug_bounds("note-title-edit").is_none(),
        "Enter 提交后退出标题编辑"
    );

    // 提交标题后焦点必须回到正文（否则用户改完标题就"打不了字"）。
    cx.simulate_input("正文");
    cx.run_until_parked();
    assert!(
        source_of(&view, cx).contains("正文"),
        "标题提交后键入应进正文：{:?}",
        source_of(&view, cx)
    );

    close_shell(&host, cx);
}

/// caret 必须按 ~530ms 严格交替：先亮，再灭，再亮，并且**隐藏相位真的不画**。
///
/// `caret_painted_last_frame` 是绘制侧的观测点，所以这条用例同时锁住两件事：
/// 相位状态翻转了，以及那一帧重绘后 `render` 的 caret 分支没有画。
#[gpui::test]
async fn caret_blinks_every_530ms_and_hidden_phase_does_not_paint(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("caret-blink");
    seed_note(&vault, "闪烁的标题", "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");

    let input = title_input(&host, cx);
    assert_eq!(
        caret_blink(&input, cx),
        (true, true),
        "刚进入编辑先亮 530ms：点进来的一瞬间必须看得到 caret"
    );

    // 隐藏相位：状态与绘制同步熄灭（只改状态不重绘 = 光标还挂在屏幕上）。
    advance_half_period(cx);
    assert_eq!(
        caret_blink(&input, cx),
        (false, false),
        "530ms 后进入隐藏相位，这一帧不能画 caret"
    );

    advance_half_period(cx);
    assert_eq!(
        caret_blink(&input, cx),
        (true, true),
        "再 530ms 必须重新画出 caret（严格交替、没有漏拍）"
    );

    // 多走两拍，防止循环只翻转一次就退出（例如 update 后忘了继续 await）。
    advance_half_period(cx);
    assert_eq!(caret_blink(&input, cx), (false, false));
    advance_half_period(cx);
    assert_eq!(caret_blink(&input, cx), (true, true));

    cancel_title_edit(cx);
    close_shell(&host, cx);
}

/// 隐藏相位下键入 / 移动光标 / 粘贴都要**立刻**回到可见，并重新从 530ms 计时。
#[gpui::test]
async fn typing_restores_a_hidden_caret_and_restarts_the_clock(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("caret-blink-input");
    seed_note(&vault, "闪烁的标题", "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");
    let input = title_input(&host, cx);

    advance_half_period(cx);
    assert_eq!(caret_blink(&input, cx), (false, false), "先落到隐藏相位");

    // 键入：走文本输入路径，立刻可见（不能等下一拍，也不能干脆不亮）。
    cx.simulate_input("新");
    assert_eq!(
        caret_blink(&input, cx),
        (true, true),
        "输入后 caret 必须立刻可见"
    );

    // 重新计时：还没走满新的 530ms 就不能熄灭（否则"刚打完字就灭"）。
    cx.executor()
        .advance_clock(CARET_BLINK_INTERVAL - Duration::from_millis(1));
    cx.run_until_parked();
    assert_eq!(
        caret_blink(&input, cx),
        (true, true),
        "最后一次输入后不足 530ms，不能熄灭"
    );

    // 正好满 530ms 才隐藏。
    cx.executor().advance_clock(Duration::from_millis(1));
    cx.run_until_parked();
    assert_eq!(
        caret_blink(&input, cx),
        (false, false),
        "输入后满 530ms 才进入隐藏相位"
    );

    // 光标键（只移动 caret、不改文本）同样复位。
    cx.simulate_keystrokes("left");
    assert_eq!(
        caret_blink(&input, cx),
        (true, true),
        "光标移动也必须复位为可见"
    );

    // 粘贴走 on_key_down 的粘贴分支，同样要复位。
    advance_half_period(cx);
    assert_eq!(caret_blink(&input, cx), (false, false));
    cx.write_to_clipboard(ClipboardItem::new_string("粘".to_owned()));
    cx.simulate_keystrokes("ctrl-v");
    assert_eq!(
        caret_blink(&input, cx),
        (true, true),
        "粘贴后 caret 必须可见"
    );
    assert!(
        title_state(&host, cx).text().contains('粘'),
        "粘贴仍然要真的写进标题：{:?}",
        title_state(&host, cx).text()
    );

    cancel_title_edit(cx);
    close_shell(&host, cx);
}

/// 鼠标点击定位与拖选（含拖出标题）同样复位 caret，且不破坏选区语义。
#[gpui::test]
async fn clicking_and_dragging_restore_the_caret_immediately(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("caret-blink-pointer");
    seed_note(&vault, MIXED_TITLE, "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");
    let input = title_input(&host, cx);

    advance_half_period(cx);
    assert_eq!(caret_blink(&input, cx), (false, false), "先落到隐藏相位");

    // 编辑态内点击（焦点没切换）：按下就要复位，不能等 focus 事件。
    let at = title_char_point(cx, 3);
    cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        caret_blink(&input, cx),
        (true, true),
        "鼠标按下定位后 caret 必须立刻可见"
    );
    cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();

    // 拖选：每一帧移动与抬起都算"用户还在动选区"。
    advance_half_period(cx);
    assert_eq!(caret_blink(&input, cx), (false, false));
    drag_title(cx, 0, 5);
    assert_eq!(
        caret_blink(&input, cx),
        (true, true),
        "拖选（移动/抬起）后 caret 必须可见"
    );
    let state = title_state(&host, cx);
    assert!(
        state.selected_byte_range().start != state.selected_byte_range().end,
        "拖选本身必须仍然选中一段文本：{:?}",
        state.selected_byte_range()
    );
    assert!(
        cx.debug_bounds("note-title-edit").is_some(),
        "闪烁改动不能影响编辑态（拖选后输入框仍要在）"
    );

    cancel_title_edit(cx);
    close_shell(&host, cx);
}

/// IME 组合串存在时 caret **常亮**（不随相位闪），上屏后恢复普通闪烁。
#[gpui::test]
async fn ime_composition_keeps_the_caret_solid_and_commit_restores_blinking(
    cx: &mut TestAppContext,
) {
    let (_dir, vault) = temp_vault("caret-blink-ime");
    seed_note(&vault, "组合", "正文");
    let (host, cx) = open_shell(cx, vault.clone());
    click_selector(cx, "tree-row-0");
    click_selector(cx, "note-title");
    let input = title_input(&host, cx);

    // 组合开始：走平台同一条 `EntityInputHandler` 入口（不是直接改测试状态）。
    cx.update(|window, app| {
        input.update(app, |input, cx| {
            input.replace_and_mark_text_in_range(None, "ni", None, window, cx);
        });
    });
    cx.run_until_parked();
    assert_eq!(
        caret_blink(&input, cx),
        (true, true),
        "组合开始后 caret 可见（贴在 preedit 末尾）"
    );

    // 组合中相位照常翻转，但绘制必须常亮——正在拼写的人看不到落点会以为输入丢了。
    advance_half_period(cx);
    assert_eq!(
        caret_blink(&input, cx),
        (false, true),
        "组合中即使进入隐藏相位也必须画 caret（常亮）"
    );
    advance_half_period(cx);
    assert_eq!(caret_blink(&input, cx), (true, true));

    // 上屏 = 一次文本提交：相位复位为可见并重新计时。
    // 上屏前仍是组合态，所以隐藏相位里的 caret 依旧常亮（上面已确认）。
    advance_half_period(cx);
    assert_eq!(
        caret_blink(&input, cx),
        (false, true),
        "组合中第二个隐藏相位也必须常亮"
    );
    cx.update(|window, app| {
        input.update(app, |input, cx| {
            input.replace_text_in_range(None, "你", window, cx);
        });
    });
    cx.run_until_parked();
    assert_eq!(
        caret_blink(&input, cx),
        (true, true),
        "IME 上屏后 caret 必须立刻可见"
    );
    let state = title_state(&host, cx);
    assert!(
        state.text().ends_with("你"),
        "上屏文本必须写进标题：{:?}",
        state.text()
    );
    assert!(
        !state.display().contains("ni"),
        "上屏后不能残留组合串：display={:?}",
        state.display()
    );

    cancel_title_edit(cx);
    close_shell(&host, cx);
}
