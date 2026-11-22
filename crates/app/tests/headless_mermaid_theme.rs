//! ADR 0019 M3a：暗色主题 + CJK 字体的 headless Mermaid 集成测试。
//!
//! 真实 `AppShell` + 真实临时 vault（`CARGO_TARGET_TMPDIR`，**不 sleep**）+ 真实
//! merman（`EditorDark`/`EditorLight` 预设 + CJK 字体栈）+ GPUI `SvgRenderer`
//! 光栅化。断言：
//!
//! 1. 浅色下占位/等待态收敛为 Ready（真实光栅化成功，不需要窗口）；
//! 2. 切深色后**新 key** 先回到占位（Loading），再收敛为 Ready，且只多渲染一次；
//! 3. 中文标签「开始/结束」在深色下同样 Ready（不因 CJK 缺字失败）；
//! 4. 切主题不改变正文/选区/视图实体；再切回浅色/深色都命中旧缓存（不再渲染）；
//! 5. 落库正文逐字节不变。
//!
//! **单独一个 test binary**：本测试切换全局主题，而其它 Mermaid headless 测试的
//! 缓存 key 会读全局 `theme_id()`；libtest 在同一 binary 内并行跑测试，混在一起会
//! 让别的测试偶发看到 `dark` key。cargo 逐 binary 串行执行，拆文件即可互不干扰。
//!
//! 像素级字体回退观感（尤其 Windows 真机）仍必须人工验收；这里钉住“渲染/光栅化
//! 成功 + CJK 不失败 + 缓存按主题隔离”的链路。

use std::path::Path;
use std::time::Duration;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Styled as _, TestAppContext, VisualTestContext, WindowAppearance,
};
use menghunote_app::editor::theme;
use menghunote_app::editor::view::{EditorView, MermaidObjectState};
use menghunote_app::shell::AppShell;
use menghunote_app::theme as app_theme;
use menghunote_app::theme::ThemeMode;
use menghunote_storage::{append_updates, load_body_doc, StructureWriter, UpdateRecord, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

fn temp_vault_dir() -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-mermaid-theme-{}", std::process::id()));
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
        .commit("headless-mermaid-theme-seed")
        .expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-mermaid-theme", update)],
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
        let shell = cx.new(|cx| AppShell::new(vault, "headless-mermaid-theme".to_owned(), cx));
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

/// 强制一帧（让 prepaint 登记 Mermaid 请求），再等后台渲染收敛，最后再强制一帧
/// 让新的图片尺寸进入几何。收敛点是 `wait_idle`（Condvar），**不 sleep**。
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

/// 只强制一帧（让 prepaint 跑一遍），不等待后台渲染。
fn force_frame(cx: &mut VisualTestContext) {
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    cx.simulate_mouse_move(
        gpui::point(
            pane.origin.x + gpui::px(theme::PAD_X + 4.0),
            pane.origin.y + gpui::px(theme::PAD_Y + 4.0),
        ),
        None,
        Modifiers::default(),
    );
    cx.run_until_parked();
}

/// M3a：深色主题 + CJK 标签的真实 headless 链路。
#[gpui::test]
async fn dark_theme_renders_cjk_mermaid_and_keeps_editor_state(cx: &mut TestAppContext) {
    let guard = app_theme::lock_for_test();
    guard.reset(None);

    let dir = temp_vault_dir();
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let text = "# 中文图\n\n```mermaid\nflowchart TD\n  开始[开始] --> 结束[结束]\n```\n\n后文\n";
    let doc_id = seed_note(&vault, "中文 Mermaid", text);

    let (host, cx) = open_shell(cx, vault);
    let view = active_view(&host, cx);

    // 1) 初始浅色：占位 → Ready（真实光栅化）。
    settle_mermaid(&view, cx);
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].state, MermaidObjectState::Ready);
    assert!(snapshots[0].size.is_some(), "Ready 必须有逻辑尺寸");
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_render_count()),
        1,
        "浅色下同一 key 只渲染一次"
    );
    let source_before = cx.update(|_window, app| view.read(app).source_text());
    let selection_before = cx.update(|_window, app| view.read(app).selection());

    // 2) 切深色：theme id 生效；新 key 尚未请求 → 先显示占位（Loading）。
    cx.update(|_window, app| {
        app_theme::set_mode(ThemeMode::Dark, WindowAppearance::Light, app);
    });
    cx.run_until_parked();
    assert_eq!(
        app_theme::theme_id(),
        app_theme::ThemeId::DARK,
        "切深色必须立即生效"
    );
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(
        snapshots[0].state,
        MermaidObjectState::Loading,
        "新主题 key 未命中时必须先显示占位"
    );
    assert_eq!(snapshots[0].label.as_deref(), Some("Mermaid 渲染中…"));

    // 3) 后台渲染收敛：深色 + 中文标签 → Ready，且恰好再渲染一次。
    settle_mermaid(&view, cx);
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(
        snapshots[0].state,
        MermaidObjectState::Ready,
        "深色 CJK 图必须光栅化成功：{snapshots:?}"
    );
    assert!(snapshots[0].size.is_some());
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_render_count()),
        2,
        "切深色必须让旧 key 失效并只重渲染一次"
    );

    // 4) 切主题不得改正文/选区（也不重建视图）。
    assert_eq!(
        cx.update(|_window, app| view.read(app).source_text()),
        source_before,
        "切主题不得改正文"
    );
    assert_eq!(
        cx.update(|_window, app| view.read(app).selection()),
        selection_before,
        "切主题不得丢选区"
    );

    // 5) 再切回浅色：旧 key 仍在缓存，命中且不再渲染。
    cx.update(|_window, app| {
        app_theme::set_mode(ThemeMode::Light, WindowAppearance::Light, app);
    });
    cx.run_until_parked();
    settle_mermaid(&view, cx);
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_render_count()),
        2,
        "切回浅色必须命中旧缓存"
    );
    let snapshots = cx.update(|_window, app| view.read(app).mermaid_snapshots());
    assert_eq!(snapshots[0].state, MermaidObjectState::Ready);

    // 6) 切回深色：同样命中，不重复渲染。
    cx.update(|_window, app| {
        app_theme::set_mode(ThemeMode::Dark, WindowAppearance::Light, app);
    });
    cx.run_until_parked();
    settle_mermaid(&view, cx);
    assert_eq!(
        cx.update(|_window, app| view.read(app).mermaid_render_count()),
        2,
        "同一主题重复切换不得重复渲染"
    );

    close_shell(&host, cx);

    // 7) 落库正文逐字节不变。
    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (after, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    assert_eq!(after.text(), text, "渲染/切主题都不得改正文");
}
