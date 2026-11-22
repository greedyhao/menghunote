//! 侧栏两项交互的 headless 集成测试（真实 `AppShell` + 临时 vault）：
//!
//! 1. **新建文件夹立即进入行内重命名**：默认名全选、输入框真的拿到焦点；
//!    点其它行 / 正文 / 侧栏空白 = 确认命名；不改名点外部不产生重命名写入。
//! 2. **笔记/文件夹拖拽移动（文件夹范围）**：行可从行拖起（ghost 卡片）；drop 落点是
//!    目标行**最近的祖先文件夹**（文件夹行 = 自身、子笔记/子文件夹行 = 父文件夹、
//!    根级笔记行 = 根），所以文件夹展开后的任意子行都能拖入；拒绝自己/自己的后代；
//!    一次合法拖放只写一次结构。
//!
//! 边界（AGENTS.md §6）：`run_until_parked()` 是唯一收敛点，不 sleep；临时目录建在
//! `CARGO_TARGET_TMPDIR` 下。真实鼠标拖拽的观感/ghost 位置仍需 Windows 真机验；
//! 这里用 `simulate_mouse_down/move/up` 走 GPUI 原生 drag/drop 路径，并断言结构结果。

use std::path::Path;

use gpui::{
    div, point, px, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::shell::AppShell;
use menghunote_storage::{
    append_updates, load_body_doc, load_head, StructureWriter, UpdateRecord, Vault,
};
use tempfile::{Builder as TempDirBuilder, TempDir};

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
        "headless-sidebar-interactions-{prefix}-{}",
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

/// 正文 A 事务：把 `text` 写进已存在的 body doc。
fn write_body(vault: &Vault, body_doc_id: &str, text: &str) {
    let (doc, head) = load_body_doc(vault, body_doc_id, 1, None).expect("加载正文失败");
    doc.insert_scalars(0, text).expect("写入正文失败");
    let base = head.version_vector().expect("解码 VV 失败");
    let update = doc.pending_update(&base).expect("导出增量失败");
    append_updates(
        vault,
        body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-sidebar-interactions", update)],
    )
    .expect("A 事务写入正文失败");
}

fn open_shell(
    cx: &mut TestAppContext,
    vault: Vault,
) -> (Entity<ShellHost>, &mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-sidebar".to_owned(), cx));
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

fn bounds_of(cx: &mut VisualTestContext, selector: &'static str) -> gpui::Bounds<gpui::Pixels> {
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("必须画出 selector={selector}"))
}

fn click_selector(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = bounds_of(cx, selector);
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

/// 结构树里某标题的节点 ID（不存在就失败）。
fn node_id_of(host: &Entity<ShellHost>, cx: &mut VisualTestContext, title: &str) -> String {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .tree_snapshot()
            .iter()
            .find(|entry| entry.title == title)
            .map_or_else(
                || panic!("结构树里没有「{title}」"),
                |entry| entry.node_id.clone(),
            )
    })
}

/// 某标题节点的父节点标题（根级 = `None`）。
fn parent_title_of(
    host: &Entity<ShellHost>,
    cx: &mut VisualTestContext,
    title: &str,
) -> Option<String> {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.read(app);
        let tree = shell.tree_snapshot();
        let entry = tree
            .iter()
            .find(|entry| entry.title == title)
            .unwrap_or_else(|| panic!("结构树里没有「{title}」"));
        entry.parent_id.as_deref().map(|parent_id| {
            tree.iter()
                .find(|entry| entry.node_id == parent_id)
                .map_or_else(
                    || panic!("父节点 {parent_id} 不在树里"),
                    |entry| entry.title.clone(),
                )
        })
    })
}

fn notice_of(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Option<String> {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .notice_text()
            .map(str::to_owned)
    })
}

fn is_renaming(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_window, app| host.read(app).shell.read(app).is_renaming())
}

/// 调用抽出来的移动逻辑（与侧栏 drop 共用同一条路径）。
fn move_to(
    host: &Entity<ShellHost>,
    cx: &mut VisualTestContext,
    node_id: &str,
    parent: Option<&str>,
) {
    let node_id = node_id.to_owned();
    let parent = parent.map(str::to_owned);
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| {
            shell.move_node_to_parent(node_id, parent, cx);
        });
    });
    cx.run_until_parked();
}

/// 拖拽手势：按下 `from`，移到 `to`（跨过阈值触发 drag），可断言 ghost 后再放手。
fn start_drag(
    cx: &mut VisualTestContext,
    from: gpui::Point<gpui::Pixels>,
    to: gpui::Point<gpui::Pixels>,
) {
    cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::default());
    // 先移动一小段越过 2px 的拖拽阈值，再移到目标（模拟真实手势）。
    cx.simulate_mouse_move(
        point(from.x + px(2.0), from.y + px(4.0)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_move(to, Some(MouseButton::Left), Modifiers::default());
    cx.run_until_parked();
}

fn finish_drag(cx: &mut VisualTestContext, to: gpui::Point<gpui::Pixels>) {
    cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
}

fn leaked_row_selector(index: usize) -> &'static str {
    Box::leak(format!("tree-row-{index}").into_boxed_str())
}

/// 在树里按标题找当前渲染行下标（未折叠时先序下标 = 行下标）。
fn row_index_of(host: &Entity<ShellHost>, cx: &mut VisualTestContext, title: &str) -> usize {
    cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .tree_snapshot()
            .iter()
            .position(|entry| entry.title == title)
            .unwrap_or_else(|| panic!("结构树里没有「{title}」"))
    })
}

fn render_row(host: &Entity<ShellHost>, cx: &mut VisualTestContext, title: &str) -> &'static str {
    leaked_row_selector(row_index_of(host, cx, title))
}

// ---------- 1. 新建文件夹立即进入重命名 ----------

#[gpui::test]
async fn new_folder_enters_inline_rename_with_focus_and_selected_default(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("rename-focus");
    let mut writer = StructureWriter::open(&vault, 1).expect("打开结构写入口失败");
    let note = writer.doc().create_note("笔记A", 1).expect("新建笔记失败");
    writer.commit("seed-note").expect("结构提交失败");
    write_body(&vault, &note.body_doc_id, "正文A");
    let (host, cx) = open_shell(cx, vault.clone());

    click_selector(cx, "sidebar-new-folder");
    assert!(
        is_renaming(&host, cx),
        "新建文件夹成功后必须立即进入行内重命名"
    );
    let input = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .rename_input()
            .expect("重命名输入框必须存在")
    });
    let (text, selection, focused) = cx.update(|window, app| {
        let input = input.read(app);
        let state = input.state();
        (
            state.text().to_owned(),
            state.selected_byte_range(),
            input.is_focused(window),
        )
    });
    assert_eq!(text, "未命名文件夹", "默认标题");
    assert_eq!(
        selection,
        0..text.len(),
        "默认名必须预选，用户一次输入即可整体替换"
    );
    assert!(focused, "新建文件夹后焦点必须在行内重命名输入框");

    // 键入直接替换预选默认名（不能进正文，也不能追加到默认名后面）。
    cx.simulate_input("项目");
    cx.run_until_parked();
    let typed = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .rename_input()
            .expect("仍应处于重命名")
            .read(app)
            .state()
            .text()
            .to_owned()
    });
    assert_eq!(typed, "项目", "预选的默认名被键入整体替换");

    // 点另一行 = 点外部 → 确认提交；行点击动作照常执行（打开这篇笔记）。
    let note_row = render_row(&host, cx, "笔记A");
    click_selector(cx, note_row);
    assert!(!is_renaming(&host, cx), "点其它行后必须结束重命名");
    assert_eq!(
        parent_title_of(&host, cx, "项目"),
        None,
        "文件夹改名必须落库（根级）"
    );
    assert!(
        cx.update(|_window, app| host.read(app).shell.read(app).active_note_title())
            .as_deref()
            == Some("笔记A"),
        "确认命名后同一手势里的行点击仍应打开该笔记"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn new_folder_rename_commits_on_editor_and_blank_click(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("rename-outside");
    let mut writer = StructureWriter::open(&vault, 1).expect("打开结构写入口失败");
    let note = writer.doc().create_note("笔记B", 1).expect("新建笔记失败");
    writer.commit("seed-note").expect("结构提交失败");
    write_body(&vault, &note.body_doc_id, "正文B");
    let (host, cx) = open_shell(cx, vault.clone());

    // (a) 点正文 = 确认命名；标题真的落库。
    click_selector(cx, "sidebar-new-folder");
    cx.simulate_input("正文确认");
    cx.run_until_parked();
    let editor = bounds_of(cx, "editor-current-doc");
    cx.simulate_click(editor.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(!is_renaming(&host, cx), "点正文后必须结束重命名");
    assert_eq!(
        parent_title_of(&host, cx, "正文确认"),
        None,
        "点正文也必须提交改名"
    );

    // (b) 点侧栏空白 = 确认命名。
    click_selector(cx, "sidebar-new-folder");
    cx.simulate_input("空白确认");
    cx.run_until_parked();
    click_selector(cx, "sidebar-tree-blank");
    assert!(!is_renaming(&host, cx), "点侧栏空白后必须结束重命名");
    assert_eq!(
        parent_title_of(&host, cx, "空白确认"),
        None,
        "点侧栏空白也必须提交改名"
    );

    // (c) 什么都不改、直接点外面：正常结束且**不产生重命名写入**。
    click_selector(cx, "sidebar-new-folder");
    assert!(is_renaming(&host, cx));
    let head_before = load_head(&vault, "structure")
        .expect("读取结构 head")
        .head_seq;
    let editor = bounds_of(cx, "editor-current-doc");
    cx.simulate_click(editor.center(), Modifiers::default());
    cx.run_until_parked();
    let head_after = load_head(&vault, "structure")
        .expect("读取结构 head")
        .head_seq;
    assert!(!is_renaming(&host, cx), "点外部必须正常结束重命名");
    assert_eq!(
        head_before, head_after,
        "默认名没改 → 点外部提交同名不应写入结构"
    );
    assert!(
        notice_of(&host, cx).is_none(),
        "同名校验应静默结束，不应留下提示"
    );
    // 默认文件夹仍在（没有被误删/误改名）。
    assert!(cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .tree_snapshot()
            .iter()
            .any(|entry| entry.title == "未命名文件夹")
    }));

    close_shell(&host, cx);
}

/// 右键菜单进入重命名后，输入框必须是真正的键盘输入目标（哪怕正文里还有
/// 尚未 flush 的本地编辑）。这是"新建文件夹立即重命名"能直接键入的前提。
#[gpui::test]
async fn menu_rename_after_editor_typing_accepts_typing(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("rename-menu-focus");
    let mut writer = StructureWriter::open(&vault, 1).expect("打开结构写入口失败");
    let target = writer.doc().create_note("目标", 1).expect("笔记 目标");
    let source = writer.doc().create_note("来源", 2).expect("笔记 来源");
    writer.commit("seed-menu-focus").expect("结构提交失败");
    write_body(&vault, &target.body_doc_id, "目标正文");
    write_body(&vault, &source.body_doc_id, "来源正文");
    let (host, cx) = open_shell(cx, vault.clone());

    // 先在正文里留下未 flush 的本地编辑（复现"重命名时后台请求刚回来"的时序）。
    let pane = bounds_of(cx, "editor-pane");
    cx.simulate_click(pane.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_input("a");
    cx.run_until_parked();

    // 右键目标行 → 菜单「重命名」。
    let target_row = render_row(&host, cx, "目标");
    let target_bounds = bounds_of(cx, target_row);
    cx.simulate_mouse_down(
        target_bounds.center(),
        MouseButton::Right,
        Modifiers::default(),
    );
    cx.run_until_parked();
    let rename = bounds_of(cx, "menu-item-0");
    cx.simulate_click(rename.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(is_renaming(&host, cx), "菜单重命名必须进入编辑态");

    let input = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .rename_input()
            .expect("重命名输入框必须存在")
    });
    let focused = cx.update(|window, app| input.read(app).is_focused(window));
    assert!(focused, "菜单进入重命名后输入框必须持有焦点");

    cx.simulate_input("新目标");
    cx.run_until_parked();
    let typed = cx.update(|_window, app| input.read(app).state().text().to_owned());
    assert_eq!(typed, "新目标", "键入必须进入重命名输入框（替换预选标题）");

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(!is_renaming(&host, cx));
    assert!(
        cx.update(|_window, app| {
            host.read(app)
                .shell
                .read(app)
                .tree_snapshot()
                .iter()
                .any(|entry| entry.title == "新目标")
        }),
        "菜单重命名必须落库"
    );

    close_shell(&host, cx);
}

// ---------- 2. 拖拽移动（纯状态路径） ----------

#[gpui::test]
async fn move_node_to_parent_moves_and_rejects_invalid_targets(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("move-pure");
    let mut writer = StructureWriter::open(&vault, 1).expect("打开结构写入口失败");
    let note_a = writer.doc().create_note("A", 1).expect("笔记 A");
    let folder_b = writer.doc().create_folder("B", 2).expect("目录 B");
    let folder_c = writer.doc().create_folder("C", 3).expect("目录 C");
    writer.doc().create_folder("D", 4).expect("目录 D");
    writer
        .doc()
        .move_node(&folder_c, Some(&folder_b), 5)
        .expect("C 挂到 B");
    writer.commit("seed-tree").expect("结构提交失败");
    write_body(&vault, &note_a.body_doc_id, "A");

    let (host, cx) = open_shell(cx, vault.clone());
    let a = node_id_of(&host, cx, "A");
    let b = node_id_of(&host, cx, "B");
    let c = node_id_of(&host, cx, "C");
    let d = node_id_of(&host, cx, "D");
    assert_eq!(parent_title_of(&host, cx, "C"), Some("B".to_owned()));

    // A → B：树中 parent = B。
    move_to(&host, cx, &a, Some(&b));
    assert_eq!(parent_title_of(&host, cx, "A"), Some("B".to_owned()));
    assert_eq!(notice_of(&host, cx), None);

    // B → C（C 在 B 下）= 自己的后代：拒绝且结构不变。
    move_to(&host, cx, &b, Some(&c));
    let notice = notice_of(&host, cx).expect("拒绝必须给提示");
    assert!(
        notice.contains("子文件夹"),
        "后代拒绝的提示要说清原因：{notice}"
    );
    assert_eq!(parent_title_of(&host, cx, "B"), None, "B 不能移动");
    assert_eq!(parent_title_of(&host, cx, "C"), Some("B".to_owned()));

    // B → B（自己）：拒绝且结构不变。
    move_to(&host, cx, &b, Some(&b));
    let notice = notice_of(&host, cx).expect("拒绝必须给提示");
    assert!(notice.contains("自己"), "自身拒绝的提示：{notice}");
    assert_eq!(parent_title_of(&host, cx, "B"), None);

    // B → A（笔记不是目标）：拒绝且结构不变。
    move_to(&host, cx, &b, Some(&a));
    let notice = notice_of(&host, cx).expect("拒绝必须给提示");
    assert!(notice.contains("不是文件夹"), "非目标提示：{notice}");
    assert_eq!(parent_title_of(&host, cx, "B"), None);

    // A → D（合法）；B → D：文件夹整体移动，子树 C 仍挂在 B 下。
    move_to(&host, cx, &a, Some(&d));
    move_to(&host, cx, &b, Some(&d));
    assert_eq!(parent_title_of(&host, cx, "A"), Some("D".to_owned()));
    assert_eq!(parent_title_of(&host, cx, "B"), Some("D".to_owned()));
    assert_eq!(
        parent_title_of(&host, cx, "C"),
        Some("B".to_owned()),
        "移动文件夹必须带着子树"
    );

    // A → 根级；再拖一次到根级 = 正常结束、不写库。
    move_to(&host, cx, &a, None);
    assert_eq!(parent_title_of(&host, cx, "A"), None);
    let head_before = load_head(&vault, "structure")
        .expect("读取结构 head")
        .head_seq;
    move_to(&host, cx, &a, None);
    let head_after = load_head(&vault, "structure")
        .expect("读取结构 head")
        .head_seq;
    assert_eq!(head_before, head_after, "同位置重复拖放不应写入结构");
    assert!(
        notice_of(&host, cx).is_some_and(|notice| notice.contains("已经")),
        "同位置拖放应说明为什么没动"
    );

    close_shell(&host, cx);
}

// ---------- 3. 拖拽移动（GPUI 原生 drag/drop UI 路径） ----------

#[gpui::test]
async fn dragging_folder_row_moves_subtree_without_collapsing_it(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-folder");
    let mut writer = StructureWriter::open(&vault, 1).expect("打开结构写入口失败");
    let folder_b = writer.doc().create_folder("B", 1).expect("目录 B");
    let child = writer.doc().create_note("B 里的笔记", 2).expect("子笔记");
    writer
        .doc()
        .move_node(&child.node_id, Some(&folder_b), 3)
        .expect("子笔记挂到 B");
    writer.doc().create_folder("D", 4).expect("目录 D");
    writer.commit("seed-drag-folder").expect("结构提交失败");
    write_body(&vault, &child.body_doc_id, "子笔记正文");
    let (host, cx) = open_shell(cx, vault.clone());

    let active_before = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_note_id()
            .map(str::to_owned)
    });
    let from_selector = render_row(&host, cx, "B");
    let to_selector = render_row(&host, cx, "D");
    let from = bounds_of(cx, from_selector).center();
    let to = bounds_of(cx, to_selector).center();
    start_drag(cx, from, to);
    assert!(
        cx.debug_bounds("sidebar-drag-ghost").is_some(),
        "拖文件夹必须同样出现 ghost"
    );
    finish_drag(cx, to);

    assert_eq!(parent_title_of(&host, cx, "B"), Some("D".to_owned()));
    assert_eq!(
        parent_title_of(&host, cx, "B 里的笔记"),
        Some("B".to_owned()),
        "文件夹整体拖动必须带着子树"
    );
    let active_after = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_note_id()
            .map(str::to_owned)
    });
    assert_eq!(active_before, active_after, "拖文件夹不得触发打开动作");
    // 若拖拽被误判成行点击，B 会被折叠 → 子笔记行消失。
    let child_row = render_row(&host, cx, "B 里的笔记");
    assert!(
        cx.debug_bounds(child_row).is_some(),
        "拖拽不得触发文件夹行的折叠"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn dropping_note_on_root_note_row_keeps_it_at_root(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-root-note");
    let mut writer = StructureWriter::open(&vault, 1).expect("打开结构写入口失败");
    let first = writer.doc().create_note("首页", 1).expect("笔记 首页");
    let dragged = writer
        .doc()
        .create_note("被拖的笔记", 2)
        .expect("笔记 被拖");
    writer.commit("seed-drag-root-note").expect("结构提交失败");
    write_body(&vault, &first.body_doc_id, "首页正文");
    write_body(&vault, &dragged.body_doc_id, "正文");
    let (host, cx) = open_shell(cx, vault.clone());

    let head_before = load_head(&vault, "structure")
        .expect("读取结构 head")
        .head_seq;
    let from_selector = render_row(&host, cx, "被拖的笔记");
    let to_selector = render_row(&host, cx, "首页");
    let from = bounds_of(cx, from_selector).center();
    let to = bounds_of(cx, to_selector).center();
    start_drag(cx, from, to);
    finish_drag(cx, to);

    assert_eq!(
        parent_title_of(&host, cx, "被拖的笔记"),
        None,
        "根级笔记行等价于根级：原本就在根级的笔记不能移动"
    );
    let head_after = load_head(&vault, "structure")
        .expect("读取结构 head")
        .head_seq;
    assert_eq!(
        head_before, head_after,
        "根级 note→note 是根级无变化，不能写结构"
    );
    assert!(
        notice_of(&host, cx).is_some_and(|notice| notice.contains("已经")),
        "无变化时要说明已经在该位置"
    );

    close_shell(&host, cx);
}

#[gpui::test]
async fn dropping_note_on_note_row_inside_folder_moves_into_that_folder(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-folder-scope");
    let mut writer = StructureWriter::open(&vault, 1).expect("打开结构写入口失败");
    let folder = writer
        .doc()
        .create_folder("输出笔记", 1)
        .expect("目录 输出笔记");
    let child = writer
        .doc()
        .create_note("输出笔记里的笔记", 2)
        .expect("子笔记");
    writer
        .doc()
        .move_node(&child.node_id, Some(&folder), 3)
        .expect("子笔记挂到输出笔记");
    let dragged = writer
        .doc()
        .create_note("被拖的笔记", 4)
        .expect("笔记 被拖");
    writer
        .commit("seed-drag-folder-scope")
        .expect("结构提交失败");
    write_body(&vault, &child.body_doc_id, "子笔记正文");
    write_body(&vault, &dragged.body_doc_id, "正文");
    let (host, cx) = open_shell(cx, vault.clone());

    let head_before = load_head(&vault, "structure")
        .expect("读取结构 head")
        .head_seq;
    // 拖到文件夹展开后的**子笔记行**，而不是文件夹标题行。
    let from_selector = render_row(&host, cx, "被拖的笔记");
    let to_selector = render_row(&host, cx, "输出笔记里的笔记");
    let from = bounds_of(cx, from_selector).center();
    let to = bounds_of(cx, to_selector).center();
    start_drag(cx, from, to);
    finish_drag(cx, to);

    assert_eq!(
        parent_title_of(&host, cx, "被拖的笔记"),
        Some("输出笔记".to_owned()),
        "拖到文件夹内的子笔记行必须视为拖入该文件夹"
    );
    let head_after = load_head(&vault, "structure")
        .expect("读取结构 head")
        .head_seq;
    assert_eq!(head_before + 1, head_after, "一次合法拖放只写一次结构");
    assert_eq!(notice_of(&host, cx), None);

    close_shell(&host, cx);
}

#[gpui::test]
async fn dropping_note_on_child_row_targets_nearest_nested_folder(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-nested-folder");
    // B
    // ├─ A（被拖）
    // └─ C
    //     └─ C 的子笔记
    let mut writer = StructureWriter::open(&vault, 1).expect("打开结构写入口失败");
    let folder_b = writer.doc().create_folder("B", 1).expect("目录 B");
    let note_a = writer.doc().create_note("A", 2).expect("笔记 A");
    let folder_c = writer.doc().create_folder("C", 3).expect("目录 C");
    let child_c = writer.doc().create_note("C 的子笔记", 4).expect("子笔记");
    writer
        .doc()
        .move_node(&note_a.node_id, Some(&folder_b), 5)
        .expect("A 挂到 B");
    writer
        .doc()
        .move_node(&folder_c, Some(&folder_b), 6)
        .expect("C 挂到 B");
    writer
        .doc()
        .move_node(&child_c.node_id, Some(&folder_c), 7)
        .expect("子笔记挂到 C");
    writer.commit("seed-drag-nested").expect("结构提交失败");
    write_body(&vault, &note_a.body_doc_id, "A 正文");
    write_body(&vault, &child_c.body_doc_id, "C 子笔记正文");
    let (host, cx) = open_shell(cx, vault.clone());

    assert_eq!(parent_title_of(&host, cx, "A"), Some("B".to_owned()));
    let from_selector = render_row(&host, cx, "A");
    let to_selector = render_row(&host, cx, "C 的子笔记");
    let from = bounds_of(cx, from_selector).center();
    let to = bounds_of(cx, to_selector).center();
    start_drag(cx, from, to);
    finish_drag(cx, to);

    assert_eq!(
        parent_title_of(&host, cx, "A"),
        Some("C".to_owned()),
        "拖到嵌套子文件夹的子笔记行必须落到最近的祖先文件夹 C"
    );
    assert_eq!(notice_of(&host, cx), None);

    close_shell(&host, cx);
}

#[gpui::test]
async fn dropping_folder_on_own_child_row_is_rejected(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-folder-self");
    // B
    // ├─ B 的子笔记
    // └─ C
    //     └─ C 的子笔记
    let mut writer = StructureWriter::open(&vault, 1).expect("打开结构写入口失败");
    let folder_b = writer.doc().create_folder("B", 1).expect("目录 B");
    let child_b = writer.doc().create_note("B 的子笔记", 2).expect("子笔记");
    let folder_c = writer.doc().create_folder("C", 3).expect("目录 C");
    let child_c = writer.doc().create_note("C 的子笔记", 4).expect("子笔记");
    writer
        .doc()
        .move_node(&child_b.node_id, Some(&folder_b), 5)
        .expect("B 的子笔记挂到 B");
    writer
        .doc()
        .move_node(&folder_c, Some(&folder_b), 6)
        .expect("C 挂到 B");
    writer
        .doc()
        .move_node(&child_c.node_id, Some(&folder_c), 7)
        .expect("C 的子笔记挂到 C");
    writer.commit("seed-drag-self").expect("结构提交失败");
    write_body(&vault, &child_b.body_doc_id, "B 子笔记正文");
    write_body(&vault, &child_c.body_doc_id, "C 子笔记正文");
    let (host, cx) = open_shell(cx, vault.clone());

    let head_before = load_head(&vault, "structure")
        .expect("读取结构 head")
        .head_seq;

    // B → 自己的子笔记行：最近祖先文件夹 = B 自己，必须拒绝。
    let from_selector = render_row(&host, cx, "B");
    let to_selector = render_row(&host, cx, "B 的子笔记");
    let from = bounds_of(cx, from_selector).center();
    let to = bounds_of(cx, to_selector).center();
    start_drag(cx, from, to);
    finish_drag(cx, to);
    let notice = notice_of(&host, cx).expect("拒绝必须给提示");
    assert!(
        notice.contains("自己"),
        "拖到自己的子行必须提示不能落到自己：{notice}"
    );
    assert_eq!(parent_title_of(&host, cx, "B"), None);

    // B → 后代目录 C 的子笔记行：最近祖先文件夹 = C（B 的后代），必须拒绝。
    let from_selector = render_row(&host, cx, "B");
    let to_selector = render_row(&host, cx, "C 的子笔记");
    let from = bounds_of(cx, from_selector).center();
    let to = bounds_of(cx, to_selector).center();
    start_drag(cx, from, to);
    finish_drag(cx, to);
    let notice = notice_of(&host, cx).expect("拒绝必须给提示");
    assert!(
        notice.contains("子文件夹"),
        "拖到后代子行必须提示不能进自己的子文件夹：{notice}"
    );
    assert_eq!(parent_title_of(&host, cx, "B"), None);

    let head_after = load_head(&vault, "structure")
        .expect("读取结构 head")
        .head_seq;
    assert_eq!(head_before, head_after, "被拒绝的拖放不能写结构");

    close_shell(&host, cx);
}

#[gpui::test]
async fn dragging_note_row_onto_folder_row_moves_it(cx: &mut TestAppContext) {
    let (_dir, vault) = temp_vault("drag-ui");
    let mut writer = StructureWriter::open(&vault, 1).expect("打开结构写入口失败");
    let first = writer.doc().create_note("首页", 1).expect("笔记 首页");
    let dragged = writer
        .doc()
        .create_note("要拖的笔记", 2)
        .expect("笔记 要拖的笔记");
    writer.doc().create_folder("目标文件夹", 3).expect("目录");
    writer.commit("seed-drag").expect("结构提交失败");
    write_body(&vault, &first.body_doc_id, "首页正文");
    write_body(&vault, &dragged.body_doc_id, "拖拽正文");
    let (host, cx) = open_shell(cx, vault.clone());

    // 先记下当前打开的笔记：拖拽**不得**触发行的打开动作。
    let active_before = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_note_id()
            .map(str::to_owned)
    });
    let from_selector = render_row(&host, cx, "要拖的笔记");
    let to_selector = render_row(&host, cx, "目标文件夹");
    let from = bounds_of(cx, from_selector).center();
    let to = bounds_of(cx, to_selector).center();

    start_drag(cx, from, to);
    assert!(
        cx.debug_bounds("sidebar-drag-ghost").is_some(),
        "按住行移动后必须出现拖拽 ghost（原生 on_drag 已启动）"
    );
    finish_drag(cx, to);

    assert_eq!(
        parent_title_of(&host, cx, "要拖的笔记"),
        Some("目标文件夹".to_owned()),
        "拖到文件夹行必须移进去"
    );
    let active_after = cx.update(|_window, app| {
        host.read(app)
            .shell
            .read(app)
            .active_note_id()
            .map(str::to_owned)
    });
    assert_eq!(
        active_before, active_after,
        "拖拽不得触发普通行点击的打开动作"
    );
    assert_eq!(notice_of(&host, cx), None);

    // 再拖到侧栏空白 = 移到根级。
    let note_selector = render_row(&host, cx, "要拖的笔记");
    let from = bounds_of(cx, note_selector).center();
    let blank = bounds_of(cx, "sidebar-tree-blank").center();
    start_drag(cx, from, blank);
    finish_drag(cx, blank);
    assert_eq!(
        parent_title_of(&host, cx, "要拖的笔记"),
        None,
        "拖到侧栏空白必须移到根级"
    );

    close_shell(&host, cx);
}
