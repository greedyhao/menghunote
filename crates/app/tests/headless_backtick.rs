//! 反引号自动补全的 app 输入集成测试（真实 AppShell + 平台文本输入路径）。
//!
//! 覆盖：
//! 1. 普通位置输入 `` ` `` → 成对插入、caret 在中间；再输入 `` ` `` 跳过自动补的那个；
//! 2. 行内代码闭合位置也跳过自动补的闭引号；
//! 3. 行首连续输入到 ```` ``` ```` → 补全 fenced code block，caret 在内容行，
//!    且**一次撤销**只回退这次补全；
//! 4. IME 组合中不触发自动补全（组合上屏走 `commit_composition`）。
//!
//! 规则本身在 `menghunote-editor` 的 `model::backtick` 纯函数里单测；这里证明
//! EntityInputHandler → `insert_typed_text` → 一次 `EditRequest` 的接线。

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-backtick-{}", std::process::id()));
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
        .commit("headless-backtick-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-backtick", update)],
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
        let shell = cx.new(|cx| AppShell::new(vault, "headless-backtick".to_owned(), cx));
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

fn focus_editor(cx: &mut VisualTestContext) {
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    cx.simulate_click(
        gpui::point(
            pane.origin.x + gpui::px(theme::PAD_X + 2.0),
            pane.origin.y + gpui::px(theme::PAD_Y + 10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
}

fn source(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn caret_byte(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> usize {
    cx.update(|_window, app| {
        let text = view.read(app).source_text();
        view.read(app)
            .selection()
            .head()
            .to_source_byte(&text)
            .expect("caret 必须落在 source 边界")
            .get()
    })
}

fn close_shell(host: &Entity<ShellHost>, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

#[gpui::test]
async fn backtick_inserts_a_pair_then_skips_the_paired_tick(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "成对反引号", "abc\n");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    focus_editor(cx);
    cx.simulate_keystrokes("end");
    cx.run_until_parked();

    cx.simulate_input("`");
    cx.run_until_parked();
    assert_eq!(source(&view, cx), "abc``\n", "普通位置必须成对插入");
    assert_eq!(caret_byte(&view, cx), 4, "caret 必须落在两个反引号之间");

    // 光标在成对中间再输入：跳过自动补的那个，不产生新字符。
    cx.simulate_input("`");
    cx.run_until_parked();
    assert_eq!(source(&view, cx), "abc``\n", "跳过补引号不得再插入");
    assert_eq!(caret_byte(&view, cx), 5, "caret 右移到自动补的引号之后");

    close_shell(&host, cx);
}

#[gpui::test]
async fn closing_an_inline_code_skips_the_paired_tick(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "行内代码", "abc\n");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    focus_editor(cx);
    cx.simulate_keystrokes("end");
    cx.run_until_parked();

    cx.simulate_input("`"); // 成对，caret 在中间
    cx.simulate_input("x"); // 内容
    cx.run_until_parked();
    assert_eq!(source(&view, cx), "abc`x`\n");
    assert_eq!(caret_byte(&view, cx), 5, "caret 在闭引号之前");

    cx.simulate_input("`"); // 闭合：跳过自动补的闭引号
    cx.run_until_parked();
    assert_eq!(source(&view, cx), "abc`x`\n", "闭合不得再插一对");
    assert_eq!(caret_byte(&view, cx), 6, "caret 越过闭引号");

    close_shell(&host, cx);
}

#[gpui::test]
async fn three_backticks_at_line_start_complete_a_fence_in_one_undo(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "围栏补全", "");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    focus_editor(cx);

    // 第 1 个：成对；第 2 个：跳过；第 3 个：补全 fenced block。
    cx.simulate_input("`");
    cx.run_until_parked();
    assert_eq!(source(&view, cx), "``");
    cx.simulate_input("`");
    cx.run_until_parked();
    assert_eq!(source(&view, cx), "``", "第二个反引号只跳过");
    cx.simulate_input("`");
    cx.run_until_parked();
    assert_eq!(
        source(&view, cx),
        "```\n\n```",
        "行首第三个反引号必须补全开围栏 + 空行 + 闭围栏"
    );
    assert_eq!(caret_byte(&view, cx), 3, "caret 落在开围栏之后");

    // 紧接输入语言标记的首字符：应追加到开围栏，而不是内容行。
    cx.simulate_input("m");
    cx.run_until_parked();
    assert_eq!(
        source(&view, cx),
        "```m\n\n```",
        "caret 必须在开围栏后，才能直接输入 info string"
    );

    // 撤销：先回退语言字符，再回退围栏补全（一个撤销组），最后回退成对插入。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source(&view, cx), "```\n\n```", "撤销语言字符");
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source(&view, cx), "``", "围栏补全必须是一个撤销组");
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source(&view, cx), "", "再撤销回退第一个成对插入");

    close_shell(&host, cx);
}

#[gpui::test]
async fn backtick_autocomplete_is_disabled_during_ime_composition(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(&vault, "IME 反引号", "");
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    focus_editor(cx);

    // F8 = 调试入口：只开始组合、不提交（与真实 IME preedit 同一条 app 路径）。
    cx.simulate_keystrokes("f8");
    cx.run_until_parked();
    cx.simulate_input("`");
    cx.run_until_parked();
    assert_eq!(
        source(&view, cx),
        "`",
        "组合上屏必须按普通文本处理（不触发成对补全）"
    );

    close_shell(&host, cx);
}
