//! 原生 headless UI 测试：真实 `AppShell` + 临时 dev vault，在**没有真实窗口/GPU**
//! 的 `TestAppContext` 里跑一遍渲染，并断言布局与数据。
//!
//! 为什么需要这一层：`ui_smoke.ps1` 是 Windows 真机脚本，慢且只能在有桌面的机器上跑；
//! 而"侧栏树画出来了没有""根视图占满窗口没有"这类**状态/布局**回归，完全可以在
//! 进程内用一个无窗口的测试平台确定性地测掉。像素/IME/DPI/真实窗口管理仍然测不到，
//! 那些继续靠真机脚本（边界见 `docs/notes/ui-headless-tests.md`）。
//!
//! 纪律（AGENTS.md §6）：
//!
//! - 临时目录一律建在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下，**不用** `%TEMP%`；
//! - 不 `sleep`：唯一的收敛点是 `run_until_parked()`（测试调度器把可运行的 future 跑空），
//!   也不依赖系统时间/窗口焦点（vault 播种时间戳是固定常量）。

use std::path::Path;

use gpui::{
    div, px, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    ParentElement as _, Render, Styled as _, TestAppContext,
};
use menghunote_app::dev_vault::{
    self, DevVault, DEMO_FOLDER_TITLE, DEMO_SECOND_TITLE, DEMO_TITLE, DEV_PEER_ID,
};
use menghunote_app::shell::AppShell;
use menghunote_storage::{list_tree, list_tree_sorted, load_body_doc, TreeSort};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 窗口根视图：只是把 `AppShell` 包一层并给一个 `debug_selector`。
///
/// 直接拿 `AppShell` 当窗口根视图也能画，但那样就**没有稳定的 selector** 去查
/// `debug_bounds`（`debug_selector` 是 `InteractiveElement` 的方法，`AppShell` 不是 div）。
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

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个**独立**的临时 dev vault。
///
/// 返回的 `TempDir` 必须活到测试结束（由 `Drop` 清理）；只留路径会提前删库。
/// 用 `tempdir_in` 而不是 `tempdir()`：AGENTS 禁止把测试数据写进 `%TEMP%`
/// （本机 cargo 子进程不可写）。
fn open_temp_dev_vault() -> (TempDir, DevVault) {
    let root =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("headless-ui-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    let dir = TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败");
    let dev = dev_vault::open_vault_at(&dir.path().join("vault.sqlite3"))
        .expect("打开临时 dev vault 失败");
    (dir, dev)
}

#[gpui::test]
async fn app_shell_paints_headless(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    assert!(dev.seeded, "空临时库必须播种演示树");
    let vault = dev.vault.clone();

    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-ui".to_owned(), cx));
        ShellHost { shell }
    });
    // `add_window_view` 内部已经 `run_until_parked()` 过一次；这里再显式跑一遍，
    // 把 `AppShell::new` 里"打开第一篇笔记"留下的前台任务收干净（仍不 sleep）。
    cx.run_until_parked();

    assert_eq!(cx.windows().len(), 1, "测试里应该只有一个 headless 窗口");
    let bounds = cx
        .debug_bounds("app-shell-host")
        .expect("ShellHost 必须画出 headless 帧（debug_selector 未命中）");
    assert!(
        bounds.size.width.as_f32() > 0.0,
        "根视图宽度必须大于 0（真实布局，不是退化尺寸）：{bounds:?}"
    );
    assert!(
        bounds.size.height.as_f32() > 0.0,
        "根视图高度必须大于 0（真实布局，不是退化尺寸）：{bounds:?}"
    );
    println!("[headless-ui] app-shell-host bounds = {bounds:?}");
}

#[gpui::test]
async fn dev_vault_tree_and_first_note_are_loaded(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    assert!(dev.seeded, "空临时库必须播种演示树");

    // 1. 结构树：先序 DFS 三个节点，标题与 dev_vault 的播种常量一一对应。
    let tree = list_tree(&dev.vault).expect("读取结构树失败");
    let shape: Vec<(&str, usize, bool)> = tree
        .iter()
        .map(|entry| (entry.title.as_str(), entry.depth, entry.is_folder()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (DEMO_TITLE, 0, false),
            (DEMO_FOLDER_TITLE, 0, true),
            (DEMO_SECOND_TITLE, 1, false),
        ],
        "侧栏节点数/顺序/深度必须与播种一致"
    );

    // 2. 第一篇（根级笔记）的正文：从库里重放 CRDT 真源，必须非空且有内容。
    let root = &tree[0];
    assert!(root.is_note(), "根级第一项必须是笔记");
    let doc_id = root
        .body_doc_id
        .clone()
        .expect("根级笔记必须带 body_doc_id");
    let (body, _head) =
        load_body_doc(&dev.vault, &doc_id, DEV_PEER_ID, None).expect("重放第一篇正文失败");
    let text = body.text();
    assert!(!text.is_empty(), "第一篇笔记的正文不能为空");
    assert!(
        text.contains(DEMO_TITLE),
        "第一篇正文应包含演示标题 {DEMO_TITLE:?}，实际前 80 字符：{:?}",
        text.chars().take(80).collect::<String>()
    );

    // 3. 同样的 ShellHost 再开一个窗口并画一帧：有内容 + 无窗口绘制不 panic。
    let vault = dev.vault.clone();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-ui-tree".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    assert_eq!(cx.windows().len(), 1, "测试里应该只有一个 headless 窗口");
    let bounds = cx
        .debug_bounds("app-shell-host")
        .expect("带真实笔记的 AppShell 必须画出 headless 帧");
    assert!(
        bounds.size.width.as_f32() > 0.0 && bounds.size.height.as_f32() > 0.0,
        "根视图必须占据非零尺寸：{bounds:?}"
    );
    println!(
        "[headless-ui] tree nodes = {}，first note = {doc_id}，body scalars = {}，bounds = {bounds:?}",
        tree.len(),
        body.text().chars().count()
    );
}

// ---------------------------------------------------------------------------
// 第二批：交互级测试（真实点击 / 按键驱动 AppShell，而不是只查静态布局）。
//
// 交互靠生产代码里的固定 `debug_selector`（见 `docs/notes/ui-headless-tests.md`）：
// 先 `debug_bounds` 拿到元素矩形 → `simulate_click` / `simulate_keystrokes` 打真实事件
// → `run_until_parked()`（唯一收敛点）→ 再查 selector 断言状态变化。**不 sleep**。
// ---------------------------------------------------------------------------

/// 侧栏（**目录优先 + 文件名 A-Z**）先序下标：
/// 0 = 示例目录，1 = 目录里的第二篇，2 = 根级演示笔记。
///
/// 为什么不是 rank 顺序：侧栏从「侧栏排序」切片起读 `list_tree_sorted`（每个层级
/// 目录在前、组内按当前排序键）；`list_tree` 的 rank 语义用于同步/导出，不再是展示顺序。
/// 点击用**树下标**而不是"第 N 行"：目录折叠时行号会跳，树下标才是稳定的。
const ROW_FOLDER: usize = 0;
const ROW_SECOND_NOTE: usize = 1;
const ROW_FIRST_NOTE: usize = 2;

/// 启动时打开第一篇；点 `tree-row-2` 后必须切到第二篇（选中标记随 `active` 走）。
#[gpui::test]
async fn click_tree_row_switches_note(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    assert!(dev.seeded, "空临时库必须播种演示树");
    let tree = list_tree_sorted(&dev.vault, TreeSort::NameAsc).expect("读取结构树失败");
    // 点击测试依赖 demo 树的形状：0 = 目录、1 = 目录里的第二篇、2 = 根级笔记。
    assert_eq!(
        (
            tree[ROW_FIRST_NOTE].title.as_str(),
            tree[ROW_FOLDER].is_folder(),
            tree[ROW_SECOND_NOTE].title.as_str(),
        ),
        (DEMO_TITLE, true, DEMO_SECOND_TITLE),
        "demo 树必须是「目录 / 目录内笔记 / 根级笔记」三行（目录优先 + A-Z）"
    );
    let root_doc_id = tree[ROW_FIRST_NOTE]
        .body_doc_id
        .clone()
        .expect("根级演示笔记必须带 body_doc_id");

    let vault = dev.vault.clone();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-ui-click".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    // 启动态：排序后的第一**笔记**（目录内那篇，行 1）是当前打开笔记，行 2 还没选中。
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "启动后必须有打开中的文档视图"
    );
    assert!(
        cx.debug_bounds("editor-pane").is_some(),
        "右侧编辑器面板必须画出 selector=editor-pane"
    );
    assert!(
        cx.debug_bounds("tree-row-1-selected").is_some(),
        "启动时第一篇（目录内那篇，行 1）应带 tree-row-1-selected"
    );
    assert!(
        cx.debug_bounds("tree-row-2-selected").is_none(),
        "启动时根级笔记还没有选中标记"
    );

    // 侧栏行是可点的：拿真实 bounds 的中心点击（不是猜坐标）。
    let sidebar = cx
        .debug_bounds("sidebar")
        .expect("侧栏容器必须画出 selector=sidebar");
    let row = cx
        .debug_bounds("tree-row-2")
        .expect("tree-row-2 必须画出 headless 帧（debug_selector 未命中）");
    assert!(
        row.size.width.as_f32() > 0.0 && row.size.height.as_f32() > 0.0,
        "行必须有非零尺寸：{row:?}"
    );
    assert!(
        row.origin.x >= sidebar.origin.x
            && row.origin.x + row.size.width <= sidebar.origin.x + sidebar.size.width + px(1.0),
        "行必须落在侧栏容器内：row={row:?} sidebar={sidebar:?}"
    );
    cx.simulate_click(row.center(), Modifiers::default());
    cx.run_until_parked();

    // 切换后：行 2 变蓝（-selected），行 1 摘掉标记，编辑器仍在（新文档的 EditorView）。
    assert!(
        cx.debug_bounds("tree-row-2-selected").is_some(),
        "点击 tree-row-2 后应选中根级演示笔记（tree-row-2-selected 未出现）"
    );
    assert!(
        cx.debug_bounds("tree-row-1-selected").is_none(),
        "切换后原来那篇不应再带 tree-row-1-selected"
    );
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "切换后新文档必须画出编辑器"
    );

    // 存储侧确认"第二篇"是真实笔记（正文非空且是它的内容）。
    // 断言切换本身靠上面的选中标记（由 `AppShell::active.node_id` 驱动）。
    let (body, _head) =
        load_body_doc(&dev.vault, &root_doc_id, DEV_PEER_ID, None).expect("重放根级笔记正文失败");
    let text = body.text();
    assert!(
        text.contains(DEMO_TITLE),
        "根级演示笔记正文应包含标题 {DEMO_TITLE:?}，实际前 80 字符：{:?}",
        text.chars().take(80).collect::<String>()
    );
    println!(
        "[headless-ui] 点击切换完成：row={row:?} root_doc={root_doc_id} body scalars={}",
        text.chars().count()
    );
}

/// `Ctrl+K`（真实按键，验证编辑器里的绑定）打开浮层，`Esc` 关闭。
///
/// 只断言浮层开/关与输入框存在：结果列表要等后台真实 tantivy 索引，那属于
/// `search_service.rs` 的测试范围，不在这里赌后台线程的时序。
#[gpui::test]
async fn ctrl_k_opens_search_and_escape_closes(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-ui-search".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("search-overlay").is_none(),
        "启动时不应有搜索浮层"
    );

    // 首帧把焦点交给编辑器（EditorView::render 的 defer），Ctrl+K 由编辑器
    // `on_key_down` 捕获后发事件给宿主 → 宿主下一帧建浮层。
    cx.simulate_keystrokes("ctrl-k");
    cx.run_until_parked();

    let overlay = cx
        .debug_bounds("search-overlay")
        .expect("Ctrl+K 必须打开搜索浮层（快捷键没到编辑器？）");
    assert!(
        overlay.size.width.as_f32() > 0.0 && overlay.size.height.as_f32() > 0.0,
        "浮层必须覆盖出非零尺寸：{overlay:?}"
    );
    assert!(
        cx.debug_bounds("search-input").is_some(),
        "浮层里必须有搜索输入框"
    );
    assert!(
        cx.debug_bounds("search-results").is_some(),
        "浮层里必须有结果容器"
    );

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("search-overlay").is_none(),
        "Esc 必须关闭搜索浮层"
    );
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "关掉浮层后编辑器必须还在"
    );
    println!("[headless-ui] Ctrl+K / Esc 开关浮层通过，overlay bounds = {overlay:?}");
}

/// 右键**目录行**弹菜单，`Esc` 关闭（笔记行按设计不弹菜单）。
#[gpui::test]
async fn right_click_folder_row_opens_menu_and_escape_closes(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-ui-menu".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("context-menu").is_none(),
        "启动时不应有右键菜单"
    );

    let folder = cx
        .debug_bounds("tree-row-0")
        .expect("tree-row-0 必须是目录行（demo 树：0 目录 / 1 目录内笔记 / 2 根级笔记）");
    cx.simulate_mouse_down(folder.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("context-menu").is_some(),
        "右键目录行必须弹出菜单"
    );

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("context-menu").is_none(),
        "Esc 必须关闭右键菜单"
    );
    println!("[headless-ui] 右键菜单开/关通过，folder row bounds = {folder:?}");
}

// ---------------------------------------------------------------------------
// 第三批：目录折叠 / 右键菜单项点击 / 编辑器键入。
// ---------------------------------------------------------------------------

/// 点目录行 = 折叠/展开（目录没有正文，点击不切换文档）。
///
/// demo 树（侧栏排序后）：0 目录 / 1 目录内笔记 / 2 根级笔记。折叠是**内存状态**
/// （`AppShell::collapsed`，重启回到全展开），UI 上的唯一投影就是
/// `visible_rows()` 少画哪些 `tree-row-N`——因此断言只能用 bounds 的有无，
/// 不能靠"行的高度变了"这类像素事实。
#[gpui::test]
async fn folder_click_collapses_and_expands_children(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    assert!(dev.seeded, "空临时库必须播种演示树");
    let tree = list_tree_sorted(&dev.vault, TreeSort::NameAsc).expect("读取结构树失败");
    assert_eq!(
        (
            tree[ROW_FIRST_NOTE].title.as_str(),
            tree[ROW_FOLDER].is_folder(),
            tree[ROW_SECOND_NOTE].depth,
        ),
        (DEMO_TITLE, true, 1),
        "demo 树必须是「目录 / 目录内笔记 / 根级笔记」（目录优先 + A-Z）"
    );

    let vault = dev.vault.clone();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-ui-collapse".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    // 前置态：子节点默认展开（有 bounds），第一篇（行 2）是当前文档。
    let child = cx
        .debug_bounds("tree-row-1")
        .expect("启动时目录子行 tree-row-1 应可见（默认全展开）");
    assert!(
        child.size.width.as_f32() > 0.0 && child.size.height.as_f32() > 0.0,
        "展开态子行必须有非零尺寸：{child:?}"
    );
    assert!(
        cx.debug_bounds("tree-row-1-selected").is_some(),
        "启动时第一篇（目录内那篇）应带 tree-row-1-selected"
    );
    let folder = cx
        .debug_bounds("tree-row-0")
        .expect("tree-row-0 必须是目录行");

    // 第一次点击 → 折叠：子行 bounds 消失，目录行与当前文档都保持。
    cx.simulate_click(folder.center(), Modifiers::default());
    cx.run_until_parked();

    assert!(
        cx.debug_bounds("tree-row-1").is_none(),
        "点击目录行后子行 tree-row-1 必须从布局中消失（折叠没生效？）"
    );
    assert!(
        cx.debug_bounds("tree-row-0").is_some(),
        "折叠后目录行本身必须还在"
    );
    assert!(
        cx.debug_bounds("tree-row-0-selected").is_none(),
        "目录折叠不改「已打开的笔记」标记（-selected 只随 active 走）"
    );
    // 当前文档在**被折叠的目录里**：它的行被藏起来（Obsidian 同样如此），但文档本身不关。
    assert!(
        cx.debug_bounds("tree-row-2-selected").is_none(),
        "折叠目录不能把别的行标成已打开"
    );
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "折叠目录不能关掉当前文档的编辑器"
    );

    // 第二次点击 → 展开：子行回来，且仍是真实布局（非零 bounds）。
    let folder = cx.debug_bounds("tree-row-0").expect("折叠后目录行仍应可点");
    cx.simulate_click(folder.center(), Modifiers::default());
    cx.run_until_parked();

    let child = cx
        .debug_bounds("tree-row-1")
        .expect("再次点击目录行后子行必须恢复（展开没生效？）");
    assert!(
        child.size.width.as_f32() > 0.0 && child.size.height.as_f32() > 0.0,
        "展开后子行必须占真实尺寸：{child:?}"
    );
    assert!(
        cx.debug_bounds("tree-row-1-selected").is_some(),
        "展开同样不能切换当前文档（行 1 重新可见并保留 -selected）"
    );
    println!("[headless-ui] 目录折叠/展开通过，folder={folder:?} child={child:?}");
}

/// 右键目录行 → 点「新建笔记」菜单项：真的建出节点、菜单关闭、新行可点。
///
/// 为什么选「新建笔记」：它是目录菜单里唯一**非破坏性且有可直接观察结果**的动作
/// （重命名进输入框、新建文件夹不打开文档；删除是破坏性的，不在自动化里执行）。
/// 菜单项本身没有鼠标监听，点击由遮罩的**几何命中**（`menu_hit`）接住，所以这里
/// 点的是 `menu-item-1` 的真实中心坐标——同时验证 selector 与几何估算一致。
#[gpui::test]
async fn context_menu_new_note_item_creates_note(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    assert!(dev.seeded, "空临时库必须播种演示树");
    let before = list_tree_sorted(&dev.vault, TreeSort::NameAsc).expect("读取结构树失败");
    assert_eq!(before.len(), 3, "demo 树必须恰好 3 个节点");
    let folder_node_id = before[ROW_FOLDER].node_id.clone();
    assert!(before[ROW_FOLDER].is_folder(), "row 0 必须是目录");

    let vault = dev.vault.clone();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-ui-menu-item".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    // 1) 右键目录行 → 菜单出现，且三个菜单项都画出真实 bounds。
    let folder = cx
        .debug_bounds("tree-row-0")
        .expect("tree-row-0 必须是目录行");
    cx.simulate_mouse_down(folder.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();

    let panel = cx
        .debug_bounds("context-menu")
        .expect("右键目录行必须弹出菜单");
    for slot in ["menu-item-0", "menu-item-1", "menu-item-2"] {
        assert!(
            cx.debug_bounds(slot).is_some(),
            "目录菜单必须画出 selector={slot}（渲染顺序：重命名/新建笔记/新建文件夹）"
        );
    }
    assert!(
        cx.debug_bounds("menu-item-3").is_none(),
        "目录菜单只有 3 项，不应有 menu-item-3"
    );

    // 2) 点「新建笔记」（渲染下标 1）：
    //    坐标必须落在菜单面板内，否则几何命中会当"点外部"处理。
    let item = cx
        .debug_bounds("menu-item-1")
        .expect("目录菜单必须画出「新建笔记」项");
    assert!(
        item.origin.x >= panel.origin.x
            && item.origin.x + item.size.width <= panel.origin.x + panel.size.width + px(1.0),
        "菜单项必须落在菜单面板内：item={item:?} panel={panel:?}"
    );
    cx.simulate_click(item.center(), Modifiers::default());
    cx.run_until_parked();

    // 3) 菜单关闭 + 树里真的多了一篇（UI 优先，存储侧复核）。
    assert!(
        cx.debug_bounds("context-menu").is_none(),
        "点了菜单项后菜单必须关闭"
    );
    let after = list_tree_sorted(&dev.vault, TreeSort::NameAsc).expect("读取结构树失败");
    assert_eq!(after.len(), 4, "「新建笔记」必须让树从 3 项变 4 项");
    let created = &after[ROW_FOLDER + 1];
    assert!(created.is_note(), "目录的第一行必须是笔记");
    assert_eq!(
        created.title, "未命名笔记",
        "新建笔记的默认标题（「未」U+672A 排在「目」U+76EE 之前）"
    );
    assert_eq!(
        created.parent_id.as_deref(),
        Some(folder_node_id.as_str()),
        "新笔记必须建在右键的那个目录里"
    );
    assert_eq!(created.depth, 1, "目录里的新笔记 depth 必须是 1");

    // 目录优先 + A-Z：新笔记（未命名笔记）排在「目录里的第二篇」之前 → 先序下标 1。
    // `create_note` 会顺手打开它，所以选中标记也在这一行。
    assert!(
        cx.debug_bounds("tree-row-1").is_some(),
        "新笔记必须画出 tree-row-1"
    );
    assert!(
        cx.debug_bounds("tree-row-1-selected").is_some(),
        "新建后应自动打开新笔记（tree-row-1-selected）"
    );
    assert!(
        cx.debug_bounds("tree-row-2-selected").is_none(),
        "打开新笔记后旧的第一篇不应再带 -selected"
    );
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "新笔记必须有编辑器视图（空文档也算）"
    );
    println!(
        "[headless-ui] 菜单项「新建笔记」通过：node={} parent={}",
        created.node_id, folder_node_id
    );
}

/// 编辑器键入：真实按键 → **平台文本输入通道** → 正文持久化。
///
/// 这条测试回答"headless 里能不能真的把字符写进文档"，走的是生产路径而不是
/// 直接调 `insert_text`：
/// 1. `simulate_keystrokes("a")` 先派发 `KeyDown`；`EditorView::on_key_down` 不处理
///    普通字母（不 `prevent_default`），事件继续传播；
/// 2. gpui 随后把 `keystroke.key_char` 交给**当前聚焦**元素的
///    `EntityInputHandler::replace_text_in_range` —— 与真实平台键入/IME 上屏同一条入口
///    （`EditorView` 实现在 `editor/input.rs`），落成一次 `EditRequest`；
/// 3. 关窗前的 `AppShell::close_all` 走 `drain_and_close` 做有界排空（durable flush），
///    然后从库里重放正文与 `head_seq` 比对。
///
/// 因此本测试同时证明了：按键绑定可达、焦点/输入处理器挂上了、flush 链路把这次编辑
/// 落成 durable。**不证明** IME 候选窗/组合串（平台通道根本不经过测试平台）。
#[gpui::test]
async fn editor_keystroke_inserts_character_and_persists(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    assert!(dev.seeded, "空临时库必须播种演示树");
    let tree = list_tree_sorted(&dev.vault, TreeSort::NameAsc).expect("读取结构树失败");
    // 用**根级演示笔记**（正文 > 100 字符，打字有判别力）；它在侧栏排序后是最后一行，
    // 测试里显式点开它，避免依赖"启动时打开哪一篇"的顺序细节。
    let doc_id = tree[ROW_FIRST_NOTE]
        .body_doc_id
        .clone()
        .expect("根级演示笔记必须带 body_doc_id");
    let (before, before_head) =
        load_body_doc(&dev.vault, &doc_id, DEV_PEER_ID, None).expect("重放第一篇正文失败");
    let before_text: Vec<char> = before.text().chars().collect();
    assert!(
        before_text.len() > 100,
        "前置态：第一篇正文必须非空（否则「插入一个字符」没有判别力）"
    );
    let before_seq = before_head.head_seq;

    let vault = dev.vault.clone();
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-ui-typing".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    // 先点开根级演示笔记（行 2）再打字：不依赖启动时打开的是哪一篇。
    let root_row = cx
        .debug_bounds("tree-row-2")
        .expect("tree-row-2 必须是根级演示笔记");
    cx.simulate_click(root_row.center(), Modifiers::default());
    cx.run_until_parked();

    // 先点正文：把焦点落在编辑器上（首帧的 defer 焦点之外再显式点一次，
    // 这样"按键被谁消费"不依赖渲染时序），同时把 caret 落到行内而不是文首。
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("右侧编辑器面板必须画出 selector=editor-pane");
    cx.simulate_click(pane.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("editor-current-doc").is_some(),
        "点正文后必须仍有打开中的文档"
    );

    // 一个普通字母：不带修饰键，走"按键 → 文本输入处理器"这条路。
    cx.simulate_keystrokes("a");
    cx.run_until_parked();

    // 收尾：关窗前的有界排空是生产代码自己的 flush 入口（不 sleep、不加私有 API）。
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();

    let (after, after_head) =
        load_body_doc(&dev.vault, &doc_id, DEV_PEER_ID, None).expect("重放写入后的正文失败");
    let after_text: Vec<char> = after.text().chars().collect();

    // 差异必须**恰好是一个插入的 'a'**：既证明字符真的进了正文，也证明没有
    // 顺带删/改其它内容（caret 在行内，所以不是简单 append）。
    //
    // 判据用"删掉 after 的某一个字符能否还原 before"，不用最长公共前后缀：
    // 插入点两侧若也恰好是 'a'，公共前后缀会把同一次插入拆到两个方向（歧义）。
    assert_eq!(
        after_text.len(),
        before_text.len() + 1,
        "键入一个字母后正文必须只多 1 个字符（实际 {} → {}）",
        before_text.len(),
        after_text.len()
    );
    let mut insertion_point = None;
    for (index, ch) in after_text.iter().enumerate() {
        if *ch != 'a' {
            continue;
        }
        let mut candidate = after_text.clone();
        candidate.remove(index);
        if candidate == before_text {
            insertion_point = Some(index);
            break;
        }
    }
    let insertion_point = insertion_point.unwrap_or_else(|| {
        panic!(
            "正文必须只多出一个插入的 'a'（scalars {} → {}）",
            before_text.len(),
            after_text.len()
        )
    });

    // body 版本：durable 的 batch seq 必须推进（证明这次编辑真的落库，不只是内存镜像）。
    assert!(
        after_head.head_seq > before_seq,
        "正文 head_seq 必须推进（durable 版本变化）：{before_seq} → {}",
        after_head.head_seq
    );
    println!(
        "[headless-ui] 键入 'a' 通过：scalars {} → {}，head_seq {before_seq} → {}，插入点 character #{}",
        before_text.len(),
        after_text.len(),
        after_head.head_seq,
        insertion_point
    );
}

#[gpui::test]
async fn right_click_root_note_can_rename_after_folder_creation(cx: &mut TestAppContext) {
    let (_dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let (_host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, "headless-ui-rename-root".to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();

    // 先创建一个根目录文件夹（用户报告就是在这之后右键失效）。
    let folder_button = cx
        .debug_bounds("sidebar-new-folder")
        .expect("侧栏必须画出新建文件夹按钮");
    cx.simulate_click(folder_button.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        list_tree(&dev.vault).expect("读树失败").len(),
        4,
        "新建文件夹后树应有 4 项"
    );

    let note = cx.debug_bounds("tree-row-0").expect("根笔记行");
    cx.simulate_mouse_down(note.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("context-menu").is_some(),
        "右键根笔记必须弹菜单"
    );
    let rename = cx
        .debug_bounds("menu-item-0")
        .expect("笔记菜单第一项是重命名");
    cx.simulate_click(rename.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("context-menu").is_none(),
        "点重命名后菜单必须关闭"
    );
    let renaming = cx.update(|_window, app| _host.read(app).shell.read(app).is_renaming());
    assert!(renaming, "点「重命名」必须进入行内重命名");

    cx.simulate_input("根笔记新名");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let tree = list_tree(&dev.vault).expect("读树失败");
    assert!(
        tree.iter().any(|entry| entry.title == "根笔记新名"),
        "右键重命名必须真的写入结构树：{:?}",
        tree.iter()
            .map(|entry| entry.title.clone())
            .collect::<Vec<_>>()
    );
}
