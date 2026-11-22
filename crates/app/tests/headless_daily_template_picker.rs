//! 日记模板「选择模板笔记…」笔记选择器的 headless 门禁。
//!
//! 覆盖（真实 `AppRoot` 设置页 + 临时布局/偏好目录，**不 sleep**，收敛点只用
//! `run_until_parked()`）：
//!
//! 1. 空库（Welcome，没有打开的库）→ 打开选择器显示明确空态，不 panic、不写偏好；
//! 2. 有库时列表包含**当前库全部普通笔记**（模板文件夹外的笔记也在），搜索按标题/
//!    路径过滤；`Enter` / 点击选中 → 写回日记模板字段并**立即原子写**
//!    `daily-notes.json`，选择器关闭；
//! 3. 当前已选模板在列表里标记（`daily-template-picker-current`）；
//! 4. `Esc` / 点空白关闭不写盘；设置关闭/重新打开后选择器不残留。
//!
//! 有库路径在 `--features dev-sync` 下是默认 dev vault（共享演示树，不可控），因此
//! 该用例只在产品 `sync-ui` 构建跑；选择器的纯逻辑（候选/过滤）另有单测。
//!
//! 像素/DPI 仍必须 Windows 真机验收；这里证明状态机、持久化与 selector 契约。

#![cfg(feature = "sync-ui")]

use std::path::Path;

use gpui::{
    point, px, Entity, Modifiers, ScrollDelta, ScrollEvent, ScrollSource, TestAppContext,
    TouchPhase, VisualTestContext,
};
use menghunote_app::daily_notes;
use menghunote_app::onboarding::{AppRoot, AppRootOptions, ProfileLayout};
#[cfg(not(feature = "dev-sync"))]
use menghunote_storage::{
    append_updates, list_tree, load_body_doc, StructureWriter, UpdateRecord, Vault,
};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开独立临时目录 + 显式公开布局。
fn temp_layout(prefix: &str) -> (TempDir, ProfileLayout) {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-daily-template-picker-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败");
    let layout = ProfileLayout::new(
        dir.path().join("config/sync.json"),
        dir.path().join("data/instances"),
    );
    (dir, layout)
}

fn temp_preferences_dir(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "headless-daily-template-picker-prefs-{prefix}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的偏好根目录失败");
    TempDirBuilder::new()
        .prefix("prefs-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时偏好目录失败")
}

fn product_options(layout: ProfileLayout) -> AppRootOptions {
    AppRootOptions {
        layout,
        default_relay: "https://relay.example".to_owned(),
        instance_id: Some("inst-product".to_owned()),
        // 产品构建的「仅本地使用」入口：headless 测试显式打开。
        allow_local_vault: true,
        operation_budget: std::time::Duration::from_secs(2),
    }
}

fn open_root(
    cx: &mut TestAppContext,
    layout: ProfileLayout,
) -> (Entity<AppRoot>, &mut VisualTestContext) {
    let options = product_options(layout);
    let (root, cx) = cx.add_window_view(move |_window, cx| AppRoot::new(options, cx));
    cx.run_until_parked();
    (root, cx)
}

fn open_settings(root: &Entity<AppRoot>, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        root.update(app, |root, cx| {
            root.open_settings(cx);
        });
    });
    cx.run_until_parked();
    assert!(
        cx.update(|_window, app| root.read(app).settings_is_open()),
        "设置页必须真的打开"
    );
}

fn bounds_of(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("必须画出 selector={selector}"));
    assert!(
        bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0,
        "{selector} 必须有非零尺寸：{bounds:?}"
    );
    bounds
}

/// 点设置页左侧分类导航（`settings-nav-<section>`）。
fn open_settings_nav(cx: &mut VisualTestContext, section: &'static str) {
    let selector: &'static str = Box::leak(format!("settings-nav-{section}").into_boxed_str());
    let bounds = bounds_of(cx, selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
    let section_selector: &'static str =
        Box::leak(format!("settings-section-{section}").into_boxed_str());
    bounds_of(cx, section_selector);
}

/// 把设置内容区里的目标滚进可见区域（真实滚轮事件；不 sleep）。
fn scroll_into_view(cx: &mut VisualTestContext, target: &'static str) {
    let card = bounds_of(cx, "settings-content");
    let card_bottom = card.origin.y + card.size.height;
    for _ in 0..40 {
        let bounds = bounds_of(cx, target);
        let center = bounds.center();
        if center.y > card.origin.y && center.y < card_bottom {
            return;
        }
        assert!(
            center.y >= card_bottom,
            "{target} 需要在卡片上方（当前滚动方向只支持向下）：{bounds:?} / {card:?}"
        );
        cx.simulate_event(ScrollEvent {
            position: card.center(),
            delta: ScrollDelta::Pixels(point(px(0.0), px(-160.0))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
            source: ScrollSource::Pointer,
        });
        cx.run_until_parked();
    }
    panic!("滚动后 {target} 仍不可见");
}

fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    scroll_into_view(cx, selector);
    let bounds = bounds_of(cx, selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 点当前已经可见的元素（选择器覆盖层里的搜索框/行，不要用设置页滚动逻辑）。
#[cfg(not(feature = "dev-sync"))]
fn click_visible(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = bounds_of(cx, selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 日记模板字段的当前输入文本（选择器写回后必须与偏好/磁盘一致）。
#[cfg(not(feature = "dev-sync"))]
fn template_field_text(root: &Entity<AppRoot>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| {
        let panel = root
            .read(app)
            .daily_notes_settings_panel()
            .expect("设置页打开后必须有日记设置面板");
        let input = panel
            .read(app)
            .field_input(daily_notes::DailyNotesField::Template)
            .expect("日记设置必须有模板字段");
        input.read(app).text().to_owned()
    })
}

/// 正文 A 事务：把 `text` 写进已存在的 body doc。
#[cfg(not(feature = "dev-sync"))]
fn write_body(vault: &Vault, body_doc_id: &str, text: &str) {
    let (doc, head) = load_body_doc(vault, body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-daily-template-picker", update)],
    )
    .expect("A 事务写入正文失败");
}

/// 在根级建一篇笔记（结构提交 + 正文），返回节点 ID。
#[cfg(not(feature = "dev-sync"))]
fn seed_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .commit("headless-daily-picker-seed")
        .expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, text);
    seed.node_id
}

#[cfg(not(feature = "dev-sync"))]
fn seed_folder(vault: &Vault, title: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let node_id = writer
        .doc()
        .create_folder(title, 1)
        .expect("结构 CRDT 新建目录失败");
    writer
        .commit("headless-daily-picker-folder")
        .expect("结构提交失败");
    node_id
}

#[cfg(not(feature = "dev-sync"))]
fn seed_note_in_folder(vault: &Vault, folder: &str, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer
        .doc()
        .move_node(&seed.node_id, Some(folder), 1)
        .expect("挂到目录失败");
    writer
        .commit("headless-daily-picker-inner")
        .expect("结构提交失败");
    write_body(vault, &seed.body_doc_id, text);
    seed.node_id
}

/// 1) 空库（没有打开的库）：选择器显示明确空态，不写偏好、不 panic。
#[gpui::test]
async fn empty_library_shows_explicit_empty_state_without_writing(cx: &mut TestAppContext) {
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir("empty");
    let prefs_path = prefs_dir.path().join(daily_notes::DAILY_NOTES_FILE_NAME);
    _daily.reset(Some(prefs_path.clone()));

    let (_dir, layout) = temp_layout("empty");
    let (root, cx) = open_root(cx, layout);
    open_settings(&root, cx);
    open_settings_nav(cx, "daily_notes");

    // 按钮存在，但没有打开的库 → 列表为空、显示明确空态。
    click_selector(cx, "settings-daily-notes-pick-template");
    bounds_of(cx, "daily-template-picker");
    bounds_of(cx, "daily-template-picker-search");
    bounds_of(cx, "daily-template-picker-list");
    bounds_of(cx, "daily-template-picker-empty");
    assert!(
        cx.debug_bounds("daily-template-picker-item-0").is_none(),
        "空库不得有候选行"
    );
    // 不 panic、不写空路径、不创建偏好文件。
    assert_eq!(daily_notes::preferences().template, "");
    assert!(
        !prefs_path.exists(),
        "空态只读：不得创建 {}",
        prefs_path.display()
    );

    // Esc 关闭：选择器卸载，设置页仍在。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("daily-template-picker").is_none(),
        "Esc 必须关闭选择器"
    );
    bounds_of(cx, "dev-sync-settings");
    assert_eq!(daily_notes::preferences().template, "");
    assert!(!prefs_path.exists());

    // 点空白关闭（面板之外的遮罩区域）：同样不写盘。
    click_selector(cx, "settings-daily-notes-pick-template");
    let overlay = bounds_of(cx, "daily-template-picker");
    cx.simulate_click(
        point(overlay.center().x, overlay.origin.y + px(8.0)),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("daily-template-picker").is_none(),
        "点空白必须关闭选择器"
    );
    assert_eq!(daily_notes::preferences().template, "");
    assert!(!prefs_path.exists());

    // 设置关闭/重新打开后选择器不残留（即使关闭时它正开着）。
    click_selector(cx, "settings-daily-notes-pick-template");
    bounds_of(cx, "daily-template-picker");
    cx.update(|_window, app| {
        root.update(app, |root, cx| {
            root.close_settings(cx);
        });
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("daily-template-picker").is_none(),
        "设置关闭时选择器必须一起卸载"
    );
    open_settings(&root, cx);
    assert!(
        cx.debug_bounds("daily-template-picker").is_none(),
        "重新打开设置页不得残留上一次的选择器"
    );
    assert!(!root.update(cx, |root, _| root.daily_template_picker_is_open()));
    assert_eq!(daily_notes::preferences().template, "");
    assert!(!prefs_path.exists());
}

/// 2) 有库：列出全部普通笔记（含模板文件夹外）、搜索过滤、Enter/点击写回并写盘、
///    当前模板高亮、Esc/点空白关闭不写。
#[cfg(not(feature = "dev-sync"))]
#[gpui::test]
async fn picker_lists_all_notes_and_writes_back_atomically(cx: &mut TestAppContext) {
    let _daily = daily_notes::lock_for_test();
    let prefs_dir = temp_preferences_dir("all");
    let prefs_path = prefs_dir.path().join(daily_notes::DAILY_NOTES_FILE_NAME);
    _daily.reset(Some(prefs_path.clone()));
    daily_notes::set_preferences(daily_notes::DailyNotesPreferences {
        enabled: true,
        date_format: "YYYY-MM-DD".to_owned(),
        folder: "日记".to_owned(),
        template: "模板/日记模板".to_owned(),
    });

    let (_dir, layout) = temp_layout("all");
    // 产品本地库：预置「模板文件夹内 + 文件夹外 + 根级」的普通笔记。
    let vault_path = layout.local_vault_path();
    std::fs::create_dir_all(vault_path.parent().expect("本地库有父目录"))
        .expect("创建 local/ 目录失败");
    {
        let vault = Vault::open(&vault_path).expect("打开本地库失败");
        seed_note(&vault, "普通笔记", "root\n");
        let templates = seed_folder(&vault, "模板");
        seed_note_in_folder(&vault, &templates, "日记模板", "T\n");
        let outside = seed_folder(&vault, "外部");
        seed_note_in_folder(&vault, &outside, "外部模板", "O\n");
        let daily = seed_folder(&vault, "日记");
        seed_note_in_folder(&vault, &daily, "2026-11-18", "D\n");
        assert_eq!(
            list_tree(&vault)
                .expect("读树失败")
                .iter()
                .filter(|entry| entry.is_note())
                .count(),
            4
        );
    }

    let (root, cx) = open_root(cx, layout.clone());
    // 真实点击「仅本地使用」进入 Main（不是直接调方法）。
    let local = bounds_of(cx, "app-root-local");
    cx.simulate_click(local.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(root.update(cx, |root, _| root.has_main_shell()));

    open_settings(&root, cx);
    open_settings_nav(cx, "daily_notes");
    click_selector(cx, "settings-daily-notes-pick-template");

    // 列表包含库里全部 4 篇普通笔记（不只模板文件夹）。
    bounds_of(cx, "daily-template-picker");
    bounds_of(cx, "daily-template-picker-search");
    bounds_of(cx, "daily-template-picker-list");
    for row in 0..4 {
        let selector: &'static str =
            Box::leak(format!("daily-template-picker-item-{row}").into_boxed_str());
        bounds_of(cx, selector);
    }
    assert!(
        cx.debug_bounds("daily-template-picker-item-4").is_none(),
        "目录不是候选（4 篇笔记 = 4 行）"
    );

    // 搜索过滤：标题/路径都参与（这里用小写路径片段），只剩 1 行。
    click_visible(cx, "daily-template-picker-search");
    cx.simulate_input("2026");
    cx.run_until_parked();
    bounds_of(cx, "daily-template-picker-item-0");
    assert!(
        cx.debug_bounds("daily-template-picker-item-1").is_none(),
        "搜索必须过滤列表"
    );

    // Enter 写回：路径写进字段 + 偏好 + 磁盘；选择器关闭。
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("daily-template-picker").is_none(),
        "Enter 选中后必须关闭选择器"
    );
    assert_eq!(daily_notes::preferences().template, "日记/2026-11-18");
    assert_eq!(
        daily_notes::load_preferences(&prefs_path).template,
        "日记/2026-11-18",
        "选中必须立即原子写盘（{}）",
        prefs_path.display()
    );
    assert!(
        !prefs_path.with_extension("json.tmp").exists(),
        "原子写完成后不能留下临时文件"
    );
    assert_eq!(
        template_field_text(&root, cx),
        "日记/2026-11-18",
        "选择结果必须写回日记模板字段（输入框显示新值）"
    );

    // 点击写回另一篇（模板文件夹外的「外部/外部模板」）。
    click_selector(cx, "settings-daily-notes-pick-template");
    click_visible(cx, "daily-template-picker-search");
    cx.simulate_input("外部");
    cx.run_until_parked();
    click_visible(cx, "daily-template-picker-item-0");
    assert!(
        cx.debug_bounds("daily-template-picker").is_none(),
        "点击行后必须关闭选择器"
    );
    assert_eq!(daily_notes::preferences().template, "外部/外部模板");
    assert_eq!(
        daily_notes::load_preferences(&prefs_path).template,
        "外部/外部模板"
    );
    assert_eq!(template_field_text(&root, cx), "外部/外部模板");

    // 当前已选模板在列表里标记（再打开 + 过滤到当前项）。
    click_selector(cx, "settings-daily-notes-pick-template");
    click_visible(cx, "daily-template-picker-search");
    cx.simulate_input("外部模板");
    cx.run_until_parked();
    bounds_of(cx, "daily-template-picker-current");

    // Esc 关闭不写：清空搜索后再选另一个也不会落地。
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.debug_bounds("daily-template-picker").is_none());
    assert_eq!(daily_notes::preferences().template, "外部/外部模板");
    assert_eq!(
        daily_notes::load_preferences(&prefs_path).template,
        "外部/外部模板"
    );

    // 点空白关闭不写。
    click_selector(cx, "settings-daily-notes-pick-template");
    let overlay = bounds_of(cx, "daily-template-picker");
    cx.simulate_click(
        point(overlay.center().x, overlay.origin.y + px(8.0)),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(cx.debug_bounds("daily-template-picker").is_none());
    assert_eq!(daily_notes::preferences().template, "外部/外部模板");
    assert_eq!(
        daily_notes::load_preferences(&prefs_path).template,
        "外部/外部模板"
    );

    // 设置关闭/重新打开不残留。
    click_selector(cx, "settings-daily-notes-pick-template");
    bounds_of(cx, "daily-template-picker");
    cx.update(|_window, app| {
        root.update(app, |root, cx| {
            root.close_settings(cx);
        });
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("daily-template-picker").is_none());
    open_settings(&root, cx);
    assert!(cx.debug_bounds("daily-template-picker").is_none());

    // 手动输入框仍然可用（选择器不改变手填语义）。
    open_settings_nav(cx, "daily_notes");
    click_selector(cx, "settings-daily-notes-template");
    cx.simulate_keystrokes("ctrl-a");
    cx.simulate_input("模板/日记模板");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(daily_notes::preferences().template, "模板/日记模板");
    assert_eq!(
        daily_notes::load_preferences(&prefs_path).template,
        "模板/日记模板"
    );
}
