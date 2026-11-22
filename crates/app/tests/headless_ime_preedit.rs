//! IME preedit 回归：组合串必须真的进入绘制路径，不能只剩候选窗；
//! 有组合串时 caret 必须画在组合串**末尾**，不能停在起点（真机反馈的 `biao'p`）。
//!
//! 背景：用户反馈在某些版本点正文后用输入法打字，只能看到候选窗、看不到组合串；
//! 另一处真机反馈是组合串输入时正文 caret 画在组合串开头。headless 平台注入不了
//! Windows IME，但 F8 走的是与平台相同的 `update_preedit` → `EditorView::paint_preedit`
//! 路径；这里用 `preedit_paint_count` 锁住“有 preedit 时绘制入口没有提前 return”，
//! 再用 `preedit_caret_x_last_frame` 锁住“串尾唯一 caret”的位置。
//!
//! 同一文件还锁 caret 闪烁（~530 ms）：由真实 50 ms pump 推进相位，输入/点击/聚焦
//! 后必须恢复可见；IME 组合期间保持常亮，不与串尾 caret 冲突。

use std::path::Path;
use std::time::Duration;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Pixels, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::glyph;
use menghunote_app::editor::theme;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-ime-preedit-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

fn seed_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-ime-preedit-seed")
        .expect("结构提交失败");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-ime-preedit", update)],
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
        let shell = cx.new(|cx| AppShell::new(vault, "headless-ime-preedit".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
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

fn click_editor(cx: &mut VisualTestContext) {
    // 注意不能点 `editor-pane` 的左上角：那里是笔记 header（标题/路径），不是正文。
    // `editor-current-doc` 是编辑器视图本身，点它的左上角才是第一行文字。
    let editor = cx
        .debug_bounds("editor-current-doc")
        .expect("编辑器正文必须画出来");
    cx.simulate_click(
        point(
            editor.origin.x + px(theme::PAD_X + 2.0),
            editor.origin.y + px(theme::PAD_Y + 10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
}

/// 当前光标（折叠选区）在窗口坐标里的起点 x。
///
/// 与绘制共用 `point_for_source_byte` 的几何；测试只接受折叠选区，否则"起点"没有意义。
fn caret_start_x(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> Pixels {
    let byte = cx.update(|_window, app| {
        let selection = view.read(app).selection();
        assert_eq!(
            selection.start(),
            selection.end(),
            "IME 组合时选区应当是折叠的（caret 才有一个起点）"
        );
        selection.start().get()
    });
    cx.update(|window, app| view.read(app).point_for_source_byte(byte, window))
        .expect("光标起点必须能换算成窗口坐标")
        .x
}

/// 组合串在正文行上的真实字形宽度（与 `paint_preedit` 同一套字体/字号 shaping）。
fn shaped_width(text: &str, cx: &mut VisualTestContext) -> Pixels {
    cx.update(|window, _app| glyph::shape_row(window, text).width())
}

#[gpui::test]
async fn ime_preedit_enters_paint_path_after_click(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "IME 回归", "第一行文字\n");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    click_editor(cx);

    // F8 与真实平台 IME 共用 update_preedit → paint_preedit；不是直接改测试状态。
    cx.simulate_keystrokes("f8");
    cx.run_until_parked();

    let (preedit, paints) = cx.update(|_window, app| {
        (
            view.read(app).preedit_text().map(str::to_owned),
            view.read(app).preedit_paint_count(),
        )
    });
    assert_eq!(preedit.as_deref(), Some("zhongwen"));
    assert!(
        paints > 0,
        "有 preedit 时必须进入 paint_preedit；否则界面只剩候选窗（回归）"
    );
}

#[gpui::test]
async fn ime_preedit_erases_text_underneath_when_caret_is_in_the_middle(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "IME 重叠回归", "甲乙丙丁\n");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    click_editor(cx);
    // caret 移到“乙/丙”之间：后面还有“丙丁”，组合串如果不擦底纸就会压住它们。
    cx.simulate_keystrokes("right right");
    cx.simulate_keystrokes("f8");
    cx.run_until_parked();

    let (preedit, mask) = cx.update(|_window, app| {
        let view = view.read(app);
        (
            view.preedit_text().map(str::to_owned),
            view.preedit_mask_width_last_frame(),
        )
    });
    assert_eq!(preedit.as_deref(), Some("zhongwen"));
    let expected = shaped_width("zhongwen", cx);
    assert!(expected > px(0.0), "组合串宽度必须为正，否则 mask 断言空转");
    assert_eq!(
        mask,
        Some(expected),
        "组合串必须在自身宽度上先擦掉底下的正文，否则会与后续文字重叠"
    );
}

#[gpui::test]
async fn ime_preedit_survives_click_transition(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "IME 点击回归", "第一行文字\n");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    click_editor(cx);
    // 单击的待定 caret 必须已经落定，否则 preedit 会按旧光标位置绘制。
    let collapsed = cx.update(|_window, app| {
        let selection = view.read(app).selection();
        selection.start() == selection.end()
    });
    assert!(collapsed, "单击后不应留下非空选区");

    cx.simulate_keystrokes("f8");
    cx.run_until_parked();
    let paints = cx.update(|_window, app| view.read(app).preedit_paint_count());
    assert!(paints > 0, "点击后开始组合也必须实际绘制 preedit");
}

#[gpui::test]
async fn ime_without_preedit_keeps_original_caret_path(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "IME 无组合回归", "第一行文字\n");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    click_editor(cx);
    let (preedit, caret_painted, preedit_caret_painted, preedit_caret_x) =
        cx.update(|_window, app| {
            let view = view.read(app);
            (
                view.preedit_text().map(str::to_owned),
                view.caret_painted_last_frame(),
                view.preedit_caret_painted_last_frame(),
                view.preedit_caret_x_last_frame(),
            )
        });
    assert!(preedit.is_none(), "点击正文后还没开始 IME 组合");
    assert!(
        caret_painted,
        "无 preedit 时必须仍走原来的 paint_caret 路径（起点竖线）"
    );
    assert!(
        !preedit_caret_painted,
        "无 preedit 时不应画组合串末尾 caret"
    );
    assert!(
        preedit_caret_x.is_none(),
        "无 preedit 时不应残留组合串末尾 caret 的 x"
    );
}

#[gpui::test]
async fn ime_preedit_caret_is_painted_at_end_of_composition(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "IME 串尾 caret 回归", "第一行文字\n");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    click_editor(cx);
    // F8 与真实平台 IME 共用 update_preedit；随后一帧走真实 paint_preedit。
    cx.simulate_keystrokes("f8");
    cx.run_until_parked();

    let (preedit, paints, caret_painted, preedit_caret_painted, preedit_caret_x) =
        cx.update(|_window, app| {
            let view = view.read(app);
            (
                view.preedit_text().map(str::to_owned),
                view.preedit_paint_count(),
                view.caret_painted_last_frame(),
                view.preedit_caret_painted_last_frame(),
                view.preedit_caret_x_last_frame(),
            )
        });
    assert_eq!(preedit.as_deref(), Some("zhongwen"));
    assert!(paints > 0, "有 preedit 时必须进入 paint_preedit");
    assert!(
        caret_painted,
        "有组合串且正文聚焦时，串尾那条 caret 就是本帧正文 caret"
    );
    assert!(
        preedit_caret_painted,
        "组合串存在时必须在串尾画 caret（否则只剩起点那条，真机回归）"
    );
    let preedit_caret_x = preedit_caret_x.expect("组合串存在时必须记录串尾 caret 的窗口 x");

    let start_x = caret_start_x(&view, cx);
    let width = shaped_width("zhongwen", cx);
    assert!(
        width > px(0.0),
        "组合串 shaping 宽度必须为正，否则 x 断言会空转"
    );
    assert_eq!(
        preedit_caret_x,
        start_x + width,
        "caret 必须画在组合串末尾：x = 组合起点 + sanitized 文本的真实 shaping 宽度"
    );
    assert!(
        preedit_caret_x > start_x,
        "caret 不能再停在组合串开头（真机反馈：`biao'p` 前那条竖线）"
    );
}

#[gpui::test]
async fn ime_cancel_leaves_no_extra_caret(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "IME 取消回归", "第一行文字\n");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    click_editor(cx);
    cx.simulate_keystrokes("f8");
    cx.run_until_parked();
    let paints_before_cancel = cx.update(|_window, app| view.read(app).preedit_paint_count());
    assert!(paints_before_cancel > 0, "取消前必须真的画过 preedit");
    let preedit_before = cx.update(|_window, app| view.read(app).preedit_text().map(str::to_owned));
    assert_eq!(preedit_before.as_deref(), Some("zhongwen"));

    // F10：update_preedit("nihao") → cancel_composition（与真实平台 Esc 等价）。
    cx.simulate_keystrokes("f10");
    cx.run_until_parked();

    let (preedit, caret_painted, preedit_caret_painted, preedit_caret_x) =
        cx.update(|_window, app| {
            let view = view.read(app);
            (
                view.preedit_text().map(str::to_owned),
                view.caret_painted_last_frame(),
                view.preedit_caret_painted_last_frame(),
                view.preedit_caret_x_last_frame(),
            )
        });
    assert!(preedit.is_none(), "F10 取消后不能残留 preedit");
    assert!(
        caret_painted,
        "取消组合后必须回到原 caret 路径（起点竖线仍要画）"
    );
    assert!(
        !preedit_caret_painted,
        "取消组合后不能残留组合串末尾 caret（否则会出现双竖线）"
    );
    assert!(
        preedit_caret_x.is_none(),
        "取消组合后不能残留组合串末尾 caret 的 x"
    );
}

/// 推进 `ticks` 个 EditorView pump 周期（每个 50 ms，不 sleep）。
///
/// EditorView 的 `spawn_pump` 每 50 ms 调一次 `pump`；`advance_clock` 让模拟时钟
/// 恰好越过一次 timer，`run_until_parked` 再跑掉随之触发的绘制。
fn advance_pump_ticks(cx: &mut VisualTestContext, ticks: usize) {
    for _ in 0..ticks {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
    }
}

/// 半周期 530 ms ≈ 11 拍 × 50 ms：跨过半周期即可断言翻转。
const BLINK_HALF_TICKS: usize = 11;

#[gpui::test]
async fn caret_hides_after_half_period_and_input_shows_it_again(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "caret 闪烁回归", "第一行文字\n");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    click_editor(cx);
    let (visible, painted) = cx.update(|_window, app| {
        let view = view.read(app);
        (view.caret_blink_visible(), view.caret_painted_last_frame())
    });
    assert!(visible, "点击后 caret 必须立刻可见并开始计时");
    assert!(painted, "可见相位下正文 caret 必须真的画出来");

    // 跨过半个闪烁周期（~530 ms）：caret 必须隐藏，且该帧不再画。
    advance_pump_ticks(cx, BLINK_HALF_TICKS);
    let (visible, painted) = cx.update(|_window, app| {
        let view = view.read(app);
        (view.caret_blink_visible(), view.caret_painted_last_frame())
    });
    assert!(!visible, "超过半个周期后 caret 应进入隐藏相位");
    assert!(!painted, "隐藏相位下不能继续画正文 caret");

    // 鼠标点击（即使落在同一个位置）也必须立刻恢复可见并重新计时。
    click_editor(cx);
    let (visible, painted) = cx.update(|_window, app| {
        let view = view.read(app);
        (view.caret_blink_visible(), view.caret_painted_last_frame())
    });
    assert!(visible, "鼠标点击后 caret 必须立刻恢复可见");
    assert!(painted, "点击后的下一帧正文 caret 必须重新画出来");

    // 再跨过半个周期确认计时确实被点击重置（而不是一直亮着）。
    advance_pump_ticks(cx, BLINK_HALF_TICKS);
    assert!(
        !cx.update(|_window, app| view.read(app).caret_blink_visible()),
        "点击必须重新计时：跨过半个周期后仍应再次隐藏"
    );

    // 任何键盘输入都立刻恢复可见并重新计时（即使这次按键不改变正文）。
    cx.simulate_keystrokes("left");
    cx.run_until_parked();
    let (visible, painted) = cx.update(|_window, app| {
        let view = view.read(app);
        (view.caret_blink_visible(), view.caret_painted_last_frame())
    });
    assert!(visible, "输入后 caret 必须立刻恢复可见");
    assert!(painted, "输入后的下一帧正文 caret 必须重新画出来");

    // 重新计时：再推进 10 拍（500 ms < 530 ms）仍应保持可见。
    advance_pump_ticks(cx, BLINK_HALF_TICKS - 1);
    assert!(
        cx.update(|_window, app| view.read(app).caret_blink_visible()),
        "输入必须重置计时：不足半周期时 caret 不能提前隐藏"
    );
}

#[gpui::test]
async fn caret_stays_solid_during_ime_composition(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "组合常亮回归", "第一行文字\n");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    click_editor(cx);
    cx.simulate_keystrokes("f8");
    cx.run_until_parked();
    let preedit = cx.update(|_window, app| view.read(app).preedit_text().map(str::to_owned));
    assert_eq!(preedit.as_deref(), Some("zhongwen"));

    // 组合期间跨过多个半周期：caret 必须常亮（不闪，避免组合时晃），串尾 caret 一直在。
    advance_pump_ticks(cx, BLINK_HALF_TICKS * 2);
    let (visible, preedit_caret_painted, preedit_caret_x) = cx.update(|_window, app| {
        let view = view.read(app);
        (
            view.caret_blink_visible(),
            view.preedit_caret_painted_last_frame(),
            view.preedit_caret_x_last_frame(),
        )
    });
    assert!(visible, "IME 组合期间 caret 必须常亮（不闪烁）");
    assert!(
        preedit_caret_painted,
        "组合期间串尾 caret 必须始终绘制（不随闪烁隐藏）"
    );
    assert!(
        preedit_caret_x.is_some(),
        "组合期间串尾 caret 的位置必须保持可测"
    );
}
