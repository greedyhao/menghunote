//! 列表项里的行内 Markdown 必须按普通行内规则渲染（不残留反引号/链接标记）。

use std::path::Path;

use gpui::{div, AppContext as _, Entity, ParentElement as _, Styled as _, TestAppContext};
use menghunote_app::shell::AppShell;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-list-inline-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 测试根");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("创建临时 vault")
}

fn seed_note(vault: &Vault, title: &str, text: &str) {
    let mut writer = StructureWriter::open(vault, 1).expect("结构写入口");
    let seed = writer.doc().create_note(title, 1).expect("建笔记");
    writer.commit("list-inline-seed").expect("结构提交");
    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文");
    doc.insert_scalars(0, text).expect("写正文");
    let base = head.version_vector().expect("VV");
    let update = doc.pending_update(&base).expect("增量");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("list-inline", update)],
    )
    .expect("A 事务");
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
        div().size_full().child(self.shell.clone())
    }
}

#[gpui::test]
async fn list_items_render_inline_markdown(cx: &mut TestAppContext) {
    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开 vault");
    seed_note(
        &vault,
        "列表",
        "- `javascript:`: 拒绝\n- **粗** 和 [链接](https://example.com) 还有 *斜*\n",
    );
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "list".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    let view = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_editor_view()
            .expect("活动视图")
    });
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains('`'),
        "列表行内代码不得残留反引号：{display:?}"
    );
    assert!(!display.contains("**"), "列表粗体不得残留标记：{display:?}");
    assert!(
        !display.contains("](https://example.com)"),
        "列表链接不得残留目标：{display:?}"
    );
    assert!(display.contains("javascript:"), "内容必须保留：{display:?}");

    // 光标移动到行尾：行内代码在行中，标记仍应隐藏。
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(
        !display.contains('`'),
        "行尾 caret 不得让列表行内代码显现：{display:?}"
    );

    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
}
