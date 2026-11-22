//! ADR 0020 P1：语言基础设施的 headless 集成测试（默认构建）。
//!
//! 覆盖：
//!
//! 1. **切语言不改变编辑状态**：正文、选区、滚动、撤销栈全部不变；编辑器实体
//!    不重建（语言只是全局状态 + 重绘）。
//! 2. **首批文案随语言变化**：侧栏/状态栏/右键菜单/设置页共用的 `ui_text`
//!    （渲染与测试同一入口）在中文/英文下给出对应文案。
//! 3. **偏好持久化**：`language.json` 原子写 → 模拟重启恢复；损坏/未知版本/
//!    未知值回退 `System`（按 `LC_ALL`/`LANG` 解析，未知语言回退中文）。
//! 4. **跟随系统**：`System` 解析结果 == `Locale::detect()`。
//!
//! 设置页里的**真实点击**在 `headless_language_ui.rs`（`dev-sync` 构建才有设置页）；
//! 像素/DPI 仍需 Windows 真机验收。
//!
//! 纪律（AGENTS.md §6）：临时目录一律在 `CARGO_TARGET_TMPDIR` 下；收敛点只用
//! `run_until_parked()`，**不 sleep**。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::EditorView;
use menghunote_app::language_settings;
use menghunote_app::shell::AppShell;
use menghunote_app::ui_text;
use menghunote_i18n::{Locale, LocaleMode};
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-language-{}", std::process::id()));
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
        .commit("headless-language-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-language", update)],
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
        let shell = cx.new(|cx| AppShell::new(vault, "headless-language".to_owned(), cx));
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

fn focus_editor(cx: &mut VisualTestContext) {
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    cx.simulate_click(
        point(
            pane.origin.x + px(theme::PAD_X + 2.0),
            pane.origin.y + px(theme::PAD_Y + 10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
}

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn switch_mode(mode: LocaleMode, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        language_settings::set_mode(mode, app);
    });
    cx.run_until_parked();
}

/// 切语言不改变正文/选区/滚动/撤销栈，也不重建编辑器。
#[gpui::test]
async fn switching_language_preserves_editor_state_and_undo(cx: &mut TestAppContext) {
    let guard = language_settings::lock_for_test();
    guard.reset(None);

    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let text = "# 语言\n\n正文 **粗体** 与 `代码`\n\n".to_owned() + &"行\n".repeat(80);
    seed_note(&vault, "语言切换", &text);
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    let view_id = view.entity_id();

    // 制造一个真实撤销组 + 选区 + 滚动。
    focus_editor(cx);
    cx.simulate_keystrokes("home");
    cx.simulate_input("x");
    cx.run_until_parked();
    let edited = source_of(&view, cx);
    assert_ne!(edited, text, "输入必须真的改了正文");
    cx.simulate_keystrokes("shift-down");
    cx.run_until_parked();
    cx.update(|_window, app| {
        view.update(app, |view, cx| view.scroll_by(40.0, cx));
    });
    cx.run_until_parked();

    let selection = cx.update(|_window, app| view.read(app).selection());
    let scroll_y = cx.update(|_window, app| view.read(app).scroll_y());
    assert!(scroll_y > 0.0, "必须真的滚动过：{scroll_y}");

    // 中文 → 英文：只改全局语言 + 重绘。
    switch_mode(LocaleMode::En, cx);
    assert_eq!(menghunote_i18n::current_locale(), Locale::En);
    assert_eq!(menghunote_i18n::mode(), LocaleMode::En);
    assert_eq!(ui_text::sidebar_notes(), "Notes");
    assert_eq!(ui_text::sidebar_new_note(), "+ Note");
    assert_eq!(ui_text::sidebar_new_folder(), "+ Folder");
    assert_eq!(ui_text::sidebar_export(), "Export");
    assert_eq!(ui_text::sidebar_graph(), "Graph");
    assert_eq!(ui_text::sidebar_settings(), "⚙ Settings");
    assert_eq!(ui_text::sidebar_backlinks(1), "1 backlink");
    assert_eq!(ui_text::sidebar_backlinks(3), "3 backlinks");
    assert_eq!(ui_text::status_sync_attention(), "Sync: action needed");
    assert_eq!(ui_text::editor_menu_table(), "Table");

    // 文档/选区/滚动/撤销栈全部不变，且没有新的本地编辑（不产生 EditRequest）。
    assert_eq!(source_of(&view, cx), edited, "切语言不得改正文");
    assert_eq!(
        cx.update(|_window, app| view.read(app).selection()),
        selection,
        "切语言不得丢选区"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).scroll_y()),
        scroll_y,
        "切语言不得改变滚动位置"
    );
    assert_eq!(view.entity_id(), view_id, "切语言不得重建编辑器");

    // 撤销栈仍可用：Ctrl+Z 回退输入 "x"。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), text, "切语言后撤销栈必须仍可用");

    // 英文 → 中文：文案恢复。
    switch_mode(LocaleMode::ZhCn, cx);
    assert_eq!(menghunote_i18n::current_locale(), Locale::ZhCn);
    assert_eq!(ui_text::sidebar_notes(), "笔记");
    assert_eq!(ui_text::sidebar_new_note(), "＋ 笔记");
    assert_eq!(ui_text::editor_menu_table(), "表格");
    assert_eq!(view.entity_id(), view_id, "切回中文也不得重建编辑器");

    close_shell(&host, cx);
}

/// 偏好写盘 → 模拟重启恢复；损坏/未知版本回退 `System`。
#[gpui::test]
async fn preferences_persist_restore_and_corrupt_falls_back(cx: &mut TestAppContext) {
    let guard = language_settings::lock_for_test();

    let dir = temp_vault_dir();
    let path = dir.path().join(language_settings::LANGUAGE_FILE_NAME);
    guard.reset(Some(path.clone()));

    // 测试守卫把语言固定到中文，避免本机 `LANG` 影响断言；生产缺文件时是
    // `System`（按环境解析），由下面的 init 路径覆盖。
    assert_eq!(menghunote_i18n::mode(), LocaleMode::ZhCn);

    // 损坏文件：回退 System，不阻塞、不 panic。
    std::fs::write(&path, b"{ this is not json").expect("写损坏偏好失败");
    language_settings::init_from_path(path.clone());
    assert_eq!(menghunote_i18n::mode(), LocaleMode::System);
    assert_eq!(
        menghunote_i18n::current_locale(),
        Locale::detect(),
        "System 必须按 LC_ALL/LANG 解析"
    );

    // 正常写盘（英文）→ 模拟重启恢复英文。
    cx.update(|app| {
        language_settings::set_mode(LocaleMode::En, app);
    });
    let raw = std::fs::read_to_string(&path).expect("偏好必须写盘");
    assert!(raw.contains("\"locale\": \"en\""), "{raw}");
    assert!(raw.contains("\"version\": 1"), "{raw}");
    assert!(
        !path.with_extension("json.tmp").exists(),
        "原子写完成后不能留下临时文件"
    );

    guard.reset(Some(path.clone()));
    language_settings::init_from_path(path.clone());
    assert_eq!(menghunote_i18n::mode(), LocaleMode::En);
    assert_eq!(menghunote_i18n::current_locale(), Locale::En);

    // 再写回「跟随系统」→ 重启后模式仍是 System。
    cx.update(|app| {
        language_settings::set_mode(LocaleMode::System, app);
    });
    let raw = std::fs::read_to_string(&path).expect("读回偏好");
    assert!(raw.contains("\"locale\": \"system\""), "{raw}");
    guard.reset(Some(path.clone()));
    language_settings::init_from_path(path);
    assert_eq!(menghunote_i18n::mode(), LocaleMode::System);
    assert_eq!(menghunote_i18n::current_locale(), Locale::detect());
}

/// 「跟随系统」解析为 `Locale::detect()`；固定模式不随系统环境变化。
#[gpui::test]
async fn system_mode_resolves_to_detected_locale(cx: &mut TestAppContext) {
    let guard = language_settings::lock_for_test();
    guard.reset(None);

    let detected = Locale::detect();
    cx.update(|app| {
        language_settings::set_mode(LocaleMode::System, app);
    });
    assert_eq!(menghunote_i18n::current_locale(), detected);

    let fixed = if detected == Locale::ZhCn {
        LocaleMode::En
    } else {
        LocaleMode::ZhCn
    };
    cx.update(|app| {
        language_settings::set_mode(fixed, app);
    });
    assert_eq!(menghunote_i18n::mode(), fixed);
    assert_eq!(
        menghunote_i18n::current_locale(),
        fixed.fixed_locale().unwrap()
    );
}
