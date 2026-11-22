//! ADR 0021 P1/P2：应用级主题的 headless 集成测试（默认构建）。
//!
//! 覆盖：
//!
//! 1. **切主题不改变编辑状态**：正文、选区、滚动、撤销栈、表格选中、Mermaid 源码
//!    全部不变；切主题不产生 `EditRequest`（本地编辑代数不变）。
//! 2. **Mermaid 缓存 key 带真实主题 id**：浅色/深色 key 不同；切到深色后重新渲染
//!    **一次**，再切同一主题命中缓存。
//! 3. **跟随系统**：`System` 按 OS 外观解析；非 `System` 忽略 OS 变化。
//! 4. **持久化**：写盘 → 模拟重启恢复；损坏/未知版本/未知模式回退 `System`（浅色）。
//!
//! 像素/DPI/真实 OS 外观切换仍必须 Windows 真机验收（headless 平台外观恒为 Light，
//! 这里用 `apply_system_appearance` 直接驱动同一状态机）。
//!
//! 纪律（AGENTS.md §6）：临时目录一律在 `CARGO_TARGET_TMPDIR` 下；收敛点只用
//! `run_until_parked()` / `wait_idle`，**不 sleep**。

use std::path::Path;
use std::time::Duration;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Styled as _, TestAppContext, VisualTestContext, WindowAppearance,
};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::EditorView;
use menghunote_app::shell::AppShell;
use menghunote_app::theme as app_theme;
use menghunote_app::theme::ThemeMode;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-theme-{}", std::process::id()));
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
    writer.commit("headless-theme-seed").expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-theme", update)],
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
        let shell = cx.new(|cx| AppShell::new(vault, "headless-theme".to_owned(), cx));
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

/// 只强制一帧（让 prepaint 跑一遍），不等待后台渲染。
fn force_frame(cx: &mut VisualTestContext) {
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    cx.simulate_mouse_move(
        point(
            pane.origin.x + px(theme::PAD_X + 4.0),
            pane.origin.y + px(theme::PAD_Y + 4.0),
        ),
        None,
        Modifiers::default(),
    );
    cx.run_until_parked();
}

fn settle_mermaid(view: &Entity<EditorView>, cx: &mut VisualTestContext) {
    force_frame(cx);
    let settled = cx.update(|_window, app| {
        view.update(app, |view, cx| {
            view.settle_mermaid(Duration::from_secs(30), cx)
        })
    });
    assert!(settled, "Mermaid 渲染必须在有界时间内收敛");
    force_frame(cx);
}

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn switch_theme(mode: ThemeMode, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        app_theme::set_mode(mode, WindowAppearance::Light, app);
    });
    cx.run_until_parked();
}

/// 切主题不改变正文/选区/滚动/撤销栈/表格选中/Mermaid 源码，也不产生 `EditRequest`。
#[gpui::test]
async fn switching_theme_preserves_editor_state_and_undo(cx: &mut TestAppContext) {
    let guard = app_theme::lock_for_test();
    guard.reset(None);

    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let mut text = String::from(
        "# 主题\n\n正文 **粗体** 与 `代码`\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |\n\n```mermaid\nflowchart TD\nA-->B\n```\n\n",
    );
    for index in 0..120 {
        text.push_str(&format!("行 {index}\n"));
    }
    seed_note(&vault, "主题切换", &text);
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_mermaid(&view, cx);

    // 1) 先制造一个真实的撤销组（输入 "x"），再扩选 + 滚动 + 选中表格。
    focus_editor(cx);
    cx.simulate_keystrokes("home");
    cx.simulate_input("x");
    cx.run_until_parked();
    let edited = source_of(&view, cx);
    assert_ne!(edited, text, "输入必须真的改了正文");
    let table_line = cx.update(|_window, app| {
        view.read(app)
            .table_snapshots()
            .first()
            .map(|snapshot| snapshot.line)
            .expect("正文必须解析出表格对象")
    });
    cx.update(|_window, app| {
        view.update(app, |view, cx| {
            view.select_object_at(table_line, None, cx);
            view.scroll_by(40.0, cx);
        });
    });
    cx.run_until_parked();

    let selection = cx.update(|_window, app| view.read(app).selection());
    let scroll_y = cx.update(|_window, app| view.read(app).scroll_y());
    let table_selected = cx.update(|_window, app| {
        view.read(app)
            .table_snapshots()
            .first()
            .is_some_and(|snapshot| snapshot.selected)
    });
    let mermaid_source = cx.update(|_window, app| {
        view.read(app)
            .mermaid_snapshots()
            .first()
            .map(|snapshot| (snapshot.span, snapshot.content_span))
            .expect("正文必须解析出 Mermaid 对象")
    });
    assert!(scroll_y > 0.0, "必须真的滚动过：{scroll_y}");
    assert!(table_selected, "表格必须处于整块选中态");

    // 2) 切到深色：只改全局主题 + 重绘。
    switch_theme(ThemeMode::Dark, cx);
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::DARK);
    assert_eq!(
        app_theme::current_theme().window_bg,
        app_theme::dark_theme().window_bg
    );

    // 3) 文档/选区/滚动/撤销栈/表格选中/Mermaid 源码全部不变，且没有新编辑。
    assert_eq!(source_of(&view, cx), edited, "切主题不得改正文");
    assert_eq!(
        cx.update(|_window, app| view.read(app).selection()),
        selection,
        "切主题不得丢选区"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).scroll_y()),
        scroll_y,
        "切主题不得改变滚动位置"
    );
    assert!(
        cx.update(|_window, app| {
            view.read(app)
                .table_snapshots()
                .first()
                .is_some_and(|snapshot| snapshot.selected)
        }),
        "切主题不得丢表格选中"
    );
    assert_eq!(
        cx.update(|_window, app| {
            view.read(app)
                .mermaid_snapshots()
                .first()
                .map(|snapshot| (snapshot.span, snapshot.content_span))
                .expect("Mermaid 对象必须仍在")
        }),
        mermaid_source,
        "切主题不得改 Mermaid 源码"
    );

    // 4) 撤销栈仍在：Ctrl+Z 回退到输入 "x" 之前。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), text, "切主题后撤销栈必须仍可用");

    // 5) 切回浅色：仍是同一个编辑器实体（没有重建）。
    switch_theme(ThemeMode::Light, cx);
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::LIGHT);
    assert_eq!(
        app_theme::current_theme().window_bg,
        app_theme::light_theme().window_bg
    );

    close_shell(&host, cx);
}

/// Mermaid 缓存 key 用真实主题 id：浅色/深色不同；切到深色重新渲染一次。
#[gpui::test]
async fn mermaid_theme_switch_rerenders_exactly_once(cx: &mut TestAppContext) {
    let guard = app_theme::lock_for_test();
    guard.reset(None);

    let dir = temp_vault_dir();
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    seed_note(
        &vault,
        "Mermaid 主题",
        "开头\n\n```mermaid\nflowchart TD\nA-->B\n```\n\n结尾\n",
    );
    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);
    settle_mermaid(&view, cx);

    let renders_before = cx.update(|_window, app| view.read(app).mermaid_render_count());
    let requests_before = cx.update(|_window, app| view.read(app).mermaid_request_count());
    assert_eq!(renders_before, 1, "浅色下同一源码只渲染一次");

    // 切深色：新 key → 重新渲染一次。
    switch_theme(ThemeMode::Dark, cx);
    settle_mermaid(&view, cx);
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_render_count()),
        renders_before + 1,
        "切主题必须让旧缓存失效并重新渲染一次"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_request_count()),
        requests_before + 1
    );

    // 再切同一个主题：key 不变，命中缓存，不再渲染。
    switch_theme(ThemeMode::Dark, cx);
    settle_mermaid(&view, cx);
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_render_count()),
        renders_before + 1,
        "同一主题必须命中缓存"
    );

    close_shell(&host, cx);
}

/// `System` 跟随 OS 外观；固定模式忽略 OS 变化。
#[gpui::test]
async fn system_mode_follows_os_appearance(cx: &mut TestAppContext) {
    let guard = app_theme::lock_for_test();
    guard.reset(None);

    cx.update(|app| {
        app_theme::set_mode(ThemeMode::System, WindowAppearance::Light, app);
    });
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::LIGHT);

    let changed = cx.update(|app| app_theme::apply_system_appearance(WindowAppearance::Dark, app));
    assert!(changed, "System 模式下 OS 变深色必须更新解析结果");
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::DARK);

    let changed = cx.update(|app| app_theme::apply_system_appearance(WindowAppearance::Dark, app));
    assert!(!changed, "同一外观不得重复触发重绘");

    // 固定浅色：OS 变深色也不跟随。
    cx.update(|app| {
        app_theme::set_mode(ThemeMode::Light, WindowAppearance::Dark, app);
    });
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::LIGHT);
    let changed = cx.update(|app| app_theme::apply_system_appearance(WindowAppearance::Dark, app));
    assert!(!changed);
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::LIGHT);

    // 切回 System：立即按当前 OS 外观解析。
    cx.update(|app| {
        app_theme::set_mode(ThemeMode::System, WindowAppearance::Dark, app);
    });
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::DARK);
    cx.update(|app| app_theme::apply_system_appearance(WindowAppearance::Light, app));
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::LIGHT);
}

/// 持久化：写盘 → 模拟重启恢复；损坏/未知版本/未知模式回退 `System`（浅色）。
#[gpui::test]
async fn preferences_persist_and_corrupt_falls_back(cx: &mut TestAppContext) {
    let guard = app_theme::lock_for_test();
    guard.reset(None);

    let dir = temp_vault_dir();
    let path = dir.path().join(app_theme::THEME_FILE_NAME);

    // 损坏文件：回退 System，且不阻塞（不 panic）。
    std::fs::write(&path, b"{ this is not json").expect("写损坏偏好失败");
    app_theme::init_from_path(path.clone());
    assert_eq!(app_theme::mode(), ThemeMode::System);
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::LIGHT);

    // 未知版本 / 未知模式：同样回退 System。
    std::fs::write(&path, b"{\"version\": 99, \"mode\": \"dark\"}").expect("写未知版本失败");
    app_theme::init_from_path(path.clone());
    assert_eq!(app_theme::mode(), ThemeMode::System);
    std::fs::write(&path, b"{\"version\": 1, \"mode\": \"neon\"}").expect("写未知模式失败");
    app_theme::init_from_path(path.clone());
    assert_eq!(app_theme::mode(), ThemeMode::System);

    // 正常写盘 → 模拟重启恢复深色。
    cx.update(|app| {
        app_theme::set_mode(ThemeMode::Dark, WindowAppearance::Light, app);
    });
    let raw = std::fs::read_to_string(&path).expect("偏好必须写盘");
    assert!(raw.contains("\"mode\": \"dark\""), "{raw}");
    guard.reset(None);
    app_theme::init_from_path(path.clone());
    assert_eq!(app_theme::mode(), ThemeMode::Dark);
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::DARK);
    assert_eq!(
        app_theme::current_theme().window_bg,
        app_theme::dark_theme().window_bg
    );

    // 再写浅色 → 重启恢复浅色。
    cx.update(|app| {
        app_theme::set_mode(ThemeMode::Light, WindowAppearance::Light, app);
    });
    guard.reset(None);
    app_theme::init_from_path(path.clone());
    assert_eq!(app_theme::mode(), ThemeMode::Light);
    assert_eq!(app_theme::theme_id(), app_theme::ThemeId::LIGHT);
}
