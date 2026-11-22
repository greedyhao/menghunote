//! 快捷键中心绑定的真实按键测试（`AppShell`，默认构建与 `--no-default-features` 都跑）。
//!
//! 覆盖：
//!
//! 1. `Ctrl+N`（默认 `new_note`）→ 真实新建一篇根级笔记（与侧栏入口同一条结构路径）；
//! 2. `Ctrl+G`（默认 `toggle_graph`）→ 真实打开图谱覆盖层；
//! 3. 改绑 `search` 后立即生效：旧 `Ctrl+K` 不再打开搜索，新组合打开；
//! 4. 搜索浮层打开时全局快捷键被守卫挡住（`Ctrl+N` 不会在浮层后面建笔记）。
//!
//! 收敛点只有 `run_until_parked()`，不 sleep；临时库在 `CARGO_TARGET_TMPDIR`。
//! 全局快捷键状态用 `shortcuts::lock_for_test()` 串行并自动复位。

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, ParentElement as _, Render, Styled as _,
    TestAppContext,
};
use menghunote_app::dev_vault::{self, DevVault};
use menghunote_app::shell::AppShell;
use menghunote_app::shortcuts::{self, CommandId, KeyCombo};
use menghunote_storage::{list_tree, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

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

fn open_temp_dev_vault() -> (TempDir, DevVault) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-shortcut-keys-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 测试目录失败");
    let dir = TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("创建临时目录失败");
    let dev = dev_vault::open_vault_at(&dir.path().join("vault.sqlite3"))
        .expect("打开临时 dev vault 失败");
    (dir, dev)
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut gpui::VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-shortcut-keys".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn note_titles(vault: &Vault) -> Vec<String> {
    list_tree(vault)
        .expect("读取结构树失败")
        .into_iter()
        .map(|entry| entry.title)
        .collect()
}

fn key(text: &str) -> KeyCombo {
    KeyCombo::parse(text).unwrap_or_else(|| panic!("测试组合必须合法：{text}"))
}

/// Ctrl+N：真实按键 → 新建根级笔记（树多一行且是「未命名笔记」）。
#[gpui::test]
async fn ctrl_n_creates_a_new_note(cx: &mut TestAppContext) {
    let guard = shortcuts::lock_for_test();
    guard.reset(None);
    let (_dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let (_host, cx) = open_shell(cx, dev.vault.clone());
    let before = note_titles(&vault);
    assert_eq!(before.len(), 3, "dev vault 播种 3 个节点");

    cx.simulate_keystrokes("ctrl-n");
    cx.run_until_parked();

    let after = note_titles(&vault);
    assert_eq!(after.len(), before.len() + 1, "Ctrl+N 必须新建一篇笔记");
    assert!(
        after.iter().any(|title| title == "未命名笔记"),
        "新笔记标题必须是默认名：{after:?}"
    );
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "新建后必须打开编辑器"
    );
}

/// Ctrl+G：真实按键 → 打开图谱覆盖层（不销毁编辑器）。
#[gpui::test]
async fn ctrl_g_opens_graph_overlay(cx: &mut TestAppContext) {
    let guard = shortcuts::lock_for_test();
    guard.reset(None);
    let (_dir, dev) = open_temp_dev_vault();
    let (_host, cx) = open_shell(cx, dev.vault.clone());
    assert!(
        cx.debug_bounds("graph-overlay").is_none(),
        "启动时不得有图谱覆盖层"
    );

    cx.simulate_keystrokes("ctrl-g");
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("graph-overlay").is_some(),
        "Ctrl+G 必须打开图谱覆盖层"
    );
    assert!(cx.debug_bounds("graph-canvas").is_some());
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "图谱只是覆盖层，不卸载编辑器"
    );
}

/// 改绑后立即生效：旧 Ctrl+K 不再触发，新 Ctrl+Shift+K 触发搜索浮层。
#[gpui::test]
async fn rebinding_search_takes_effect_immediately(cx: &mut TestAppContext) {
    let guard = shortcuts::lock_for_test();
    guard.reset(None);
    shortcuts::assign(CommandId::Search, key("ctrl-shift-k")).expect("改绑必须成功");

    let (_dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let (_host, cx) = open_shell(cx, dev.vault.clone());
    assert!(cx.debug_bounds("search-overlay").is_none());

    cx.simulate_keystrokes("ctrl-k");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("search-overlay").is_none(),
        "旧 Ctrl+K 在改绑后不得再打开搜索"
    );

    cx.simulate_keystrokes("ctrl-shift-k");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("search-overlay").is_some(),
        "新绑定 Ctrl+Shift+K 必须打开搜索浮层"
    );

    // 搜索浮层打开时 Ctrl+N 被守卫挡住（不会在浮层后面新建笔记）。
    let before = note_titles(&vault);
    cx.simulate_keystrokes("ctrl-n");
    cx.run_until_parked();
    assert_eq!(
        note_titles(&vault),
        before,
        "搜索浮层打开时 Ctrl+N 不得新建笔记"
    );
}
