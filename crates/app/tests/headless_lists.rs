//! ADR 0019 S1：app headless 列表布局 / 命中 / 光标 / Enter 集成测试。
//!
//! 真实 `AppShell` + 临时 vault（不 sleep，收敛点只有 `run_until_parked`）。
//! 断言：
//! 1. 列表文档能 headless 渲染，打开/关闭**不改写源码**；
//! 2. 点击列表行（经过 bullet gutter 的几何）→ Enter → 同级新 item 落库，
//!    且“删掉插入的 `\n- ` 能还原原文”（证明命中/光标落点正确）；
//! 3. 嵌套列表 Enter 保留层级缩进（`\n  - `）。
//!
//! 像素级外观仍必须真机验证；这里证明几何/命中/事务链路是同一套。

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::theme;
use menghunote_app::shell::AppShell;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 窗口根视图：给 `AppShell` 一个稳定 selector，避免直接拿 `AppShell` 查 bounds。
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

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-lists-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 在空 vault 里用真实 storage 路径建一篇笔记（结构 CRDT → 投影 → 正文 A 事务）。
fn seed_list_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer.commit("headless-lists-seed").expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-lists", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

/// 打开 AppShell（它会自动打开树里第一篇笔记）并跑完首帧。
fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-lists".to_owned(), cx));
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

/// 删掉 after 里某个 `needle` 后能否精确还原 before（判据比“长度 +1”更强）。
fn removing_one_insertion_restores(after: &str, before: &str, needle: &str) -> bool {
    for (index, _) in after.match_indices(needle) {
        let mut candidate = after.to_owned();
        candidate.replace_range(index..index + needle.len(), "");
        if candidate == before {
            return true;
        }
    }
    false
}

#[gpui::test]
async fn opening_and_closing_a_list_note_does_not_rewrite_source(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let text = "# 列表\n\n- alpha\n- beta\n  - nested\n\n1. one\n2. two\n";
    let doc_id = seed_list_note(&vault, "列表笔记", text);

    let (host, cx) = open_shell(cx, vault);
    assert!(
        cx.debug_bounds("editor-pane").is_some(),
        "编辑器面板必须画出来"
    );
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "必须打开一篇正文"
    );
    close_shell(&host, cx);

    let (after, _head) = load_body_doc(
        &Vault::open(dir.path().join("vault.sqlite3")).expect("重开 vault"),
        &doc_id,
        1,
        None,
    )
    .expect("重放正文失败");
    assert_eq!(after.text(), text, "打开/关闭列表笔记不得改写源码");
}

#[gpui::test]
async fn click_list_line_and_enter_creates_sibling_item(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let text = "# 列表\n\n- alpha\n- beta\n";
    let doc_id = seed_list_note(&vault, "列表笔记", text);

    let (host, cx) = open_shell(cx, vault);
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");

    // 第 0 行是 h1（26px → 39px 行高），第 1 行空行（22px），第 2 行是 `- alpha`。
    // x 落在 bullet gutter 之后的内容里（不是 marker），Enter 才走“同级新 item”。
    let y = pane.origin.y + gpui::px(theme::PAD_Y + 39.0 + 22.0 + 10.0);
    let x = pane.origin.x + gpui::px(theme::PAD_X + 60.0);
    cx.simulate_click(gpui::point(x, y), Modifiers::default());
    cx.run_until_parked();

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    close_shell(&host, cx);

    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (after, after_head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    let after_text = after.text();
    assert!(
        removing_one_insertion_restores(&after_text, text, "\n- "),
        "Enter 必须在 caret 处插入一个同级 `- ` item；before={text:?} after={after_text:?}"
    );
    assert!(
        after_head.head_seq > 0,
        "列表 Enter 必须推进 durable head_seq：{}",
        after_head.head_seq
    );
    println!("[headless-lists] Enter 同级 item 落库：{after_text:?}");
}

#[gpui::test]
async fn click_nested_list_line_keeps_hanging_indent_and_level(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let text = "- alpha\n  - beta\n\n1. one\n2. two\n";
    let doc_id = seed_list_note(&vault, "嵌套列表", text);

    let (host, cx) = open_shell(cx, vault);
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");

    // 第 1 行是深度 1 的嵌套 item：x 要越过 `depth*24px + bullet gutter`。
    let y = pane.origin.y + gpui::px(theme::PAD_Y + 22.0 + 10.0);
    let x = pane.origin.x + gpui::px(theme::PAD_X + theme::LIST_INDENT_PER_LEVEL + 40.0);
    cx.simulate_click(gpui::point(x, y), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    close_shell(&host, cx);

    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (after, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    let after_text = after.text();
    assert!(
        removing_one_insertion_restores(&after_text, text, "\n  - "),
        "嵌套 item 的 Enter 必须保留 2 空格缩进；before={text:?} after={after_text:?}"
    );
}

#[gpui::test]
async fn backspace_on_empty_list_item_deletes_only_the_marker_space(cx: &mut TestAppContext) {
    // 两种场景都要只删空格：空列表单独一行；非空列表后自动生成的空项。
    for (before, after_expected) in [("1. \n", "1.\n"), ("- alpha\n- \n", "- alpha\n-\n")] {
        let dir = temp_vault_dir();
        let vault_path = dir.path().join("vault.sqlite3");
        let vault = Vault::open(&vault_path).expect("打开临时 vault");
        let doc_id = seed_list_note(&vault, "空列表退格", before);

        let (host, cx) = open_shell(cx, vault);
        let pane = cx
            .debug_bounds("editor-pane")
            .expect("编辑器面板必须画出来");
        // 空 item 在最后一行；行高按正文 22px。
        let lines = before.lines().count().max(1) as f32;
        let y = pane.origin.y + gpui::px(theme::PAD_Y + (lines - 1.0) * 22.0 + 10.0);
        let x = pane.origin.x + gpui::px(theme::PAD_X + 50.0);
        cx.simulate_click(gpui::point(x, y), Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("end");
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        close_shell(&host, cx);

        let vault = Vault::open(&vault_path).expect("重开 vault");
        let (after, _) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
        assert_eq!(
            after.text(),
            after_expected,
            "空列表项 Backspace 只能删 marker 后的空格；before={before:?}"
        );
    }
}

#[gpui::test]
async fn deleting_the_last_list_marker_char_drops_leftover_indent(cx: &mut TestAppContext) {
    // 自动生成的嵌套空项：先删空格（`  -`），再删 `-` 时不能留下 `  ` 空缩进行。
    let before = "- alpha\n  - \n";
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let doc_id = seed_list_note(&vault, "删空项缩进", before);

    let (host, cx) = open_shell(cx, vault);
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    let y = pane.origin.y + gpui::px(theme::PAD_Y + 22.0 + 10.0);
    let x = pane.origin.x + gpui::px(theme::PAD_X + theme::LIST_INDENT_PER_LEVEL + 40.0);
    cx.simulate_click(gpui::point(x, y), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("end");
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    close_shell(&host, cx);

    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (after, _) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    assert_eq!(
        after.text(),
        "- alpha\n\n",
        "删掉 marker 最后一个字符后不能遗留缩进行"
    );
}

#[gpui::test]
async fn ordered_item_deleted_to_plain_number_has_no_list_state(cx: &mut TestAppContext) {
    // `2. ` 删空格 → `2.`，再删 `.` → `2`；此时是普通段落，不能仍按列表续行缩进。
    let before = "1. sdf\n2. \n";
    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let doc_id = seed_list_note(&vault, "有序列表删 marker", before);

    let (host, cx) = open_shell(cx, vault);
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    let y = pane.origin.y + gpui::px(theme::PAD_Y + 22.0 + 10.0);
    let x = pane.origin.x + gpui::px(theme::PAD_X + 50.0);
    cx.simulate_click(gpui::point(x, y), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("end");
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    cx.simulate_keystrokes("backspace");
    cx.run_until_parked();
    close_shell(&host, cx);

    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (after, _) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    assert_eq!(after.text(), "1. sdf\n2\n");
}
