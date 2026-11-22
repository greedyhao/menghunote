//! 根视图：左侧笔记树 + 右侧编辑器（ADR 0003 切片 7b）。
//!
//! ## 为什么需要这一层
//!
//! 切片 5a–6 的窗口根视图直接是 `EditorView`，文档是硬编码的一个。7b 要"能选文档"，
//! Lead 已拍板的取舍是**切换时重建整个 `EditorView`**（重新 `SessionDriver::open` +
//! `EditorController::open`），不做"单实例换文档"——切片 7 要闭环，不要架构最优。
//!
//! ## 切换契约（硬约束：不许静默丢编辑）
//!
//! 1. **先开新文档**：新 driver/controller 失败时完全不动当前视图，只提示 + 日志；
//! 2. **再排空旧文档**：`end_composition()` + `close(1500ms)`；
//! 3. `CloseReport.timed_out == true` → **中止切换**：保留旧视图，状态栏（侧栏底部）
//!    提示"保存未完成，稍后再切换"并打日志，新开的那个 EditorView 直接关闭丢弃。
//!
//! ## 侧栏刷新时机
//!
//! 只在**启动**与**切换完成后**读 `list_tree`。任务 7a 已明确：投影表在 `commit`
//! 之前读不到结构改动——轮询解决不了这个边界，等同步事件再接。

use std::collections::HashSet;

use gpui::prelude::*;
use gpui::{
    canvas, div, point, px, AnyElement, App, Context, Entity, FocusHandle, FontWeight, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, ScrollDelta, SharedString,
    Size, Window,
};
use menghunote_editor::EditorController;
use menghunote_storage::rename_links::{
    link_sources_for_note, rewrite_link_sources, RenameLinkOptions, DEFAULT_REWRITE_TIMEOUT,
};
#[cfg(feature = "dev-sync")]
use menghunote_storage::session::{
    prepare_committed_delta, AdoptError, AdoptOutcome, AppliedHeads, ExternalCommitLease,
    ExternalCommitTicket, GateError, GateState, PreparedCommittedDelta,
};
use menghunote_storage::session::{DriverOptions, SessionDriver};
use menghunote_storage::{
    list_backlinks, list_tree, Backlink, StructureWriter, SyncContext, TreeEntry, Vault,
};
#[cfg(feature = "dev-sync")]
use menghunote_storage::{load_head, STRUCTURE_DOC_ID};

use crate::editor::view::EditorEvent;
// 图谱（G1/G2）：模型/视口数学全在 `graph` 模块，这里只做覆盖层与交互。
// `hint_line` / `scroll_zoom_factor` / `GraphLod` / `arrow_head` 都是纯函数，
// UI 与测试共用同一份口径。
use crate::graph::{
    arrow_head, hint_line, node_offscreen, scroll_zoom_factor, GraphLimits, GraphLod, GraphModel,
    GraphOptions, GraphScope, GraphSearch, GraphViewport, DEFAULT_LOCAL_DEPTH, GHOST_RADIUS_SCALE,
    MAX_LOCAL_DEPTH, MIN_LOCAL_DEPTH,
};
use crate::search_input::{SearchInput, SearchInputEvent};
use crate::sidebar_rename::{RenameEvent, RenameInput};

/// 新建笔记的默认标题（与现有标题重名时自动加序号）。
const DEFAULT_NOTE_TITLE: &str = "未命名笔记";
/// 新建目录的默认标题。
const DEFAULT_FOLDER_TITLE: &str = "未命名文件夹";
/// 默认本机 peer id（与 dev 播种一致）。
///
/// **默认构建 / dev-sync 未启用时保持常量 1**，行为与切片 7 完全相同；dev-sync 启用时
/// `main.rs` 走 [`AppShell::new_with_peer`]，peer 来自持久身份
/// （`LocalIdentityV1::loro_peer_id()`，ADR 0009 切片 2a），不再用这个常量。
pub const DEFAULT_PEER_ID: u64 = 1;
/// 菜单刚打开时的"失焦宽限期"：gpui 的 active 标志在把焦点交给菜单面板后会短暂
/// 变 false，这段时间内的失焦事件忽略（实测会立刻把菜单关掉）。
const DEACTIVATE_GUARD: std::time::Duration = std::time::Duration::from_millis(500);
/// 菜单刚打开后的"手势余波"窗口：这段时间内的遮罩点击不关菜单。
const OPEN_GUARD: std::time::Duration = std::time::Duration::from_millis(100);
/// `MENGHUNOTE_TRACE_MOUSE=1` 时才输出窗口级鼠标日志（排查输入链路用）。
pub(crate) fn trace_mouse_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("MENGHUNOTE_TRACE_MOUSE").is_some_and(|v| v != "0"))
}

/// `MENGHUNOTE_TRACE_MENU=1` 时才输出菜单/行右键日志（排查右键链路用）。
pub(crate) fn trace_menu_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("MENGHUNOTE_TRACE_MENU").is_some_and(|v| v != "0"))
}

/// 按 `MENGHUNOTE_TRACE_MOUSE` 决定是否输出鼠标日志。
macro_rules! trace_mouse {
    ($($arg:tt)*) => {
        if trace_mouse_enabled() {
            eprintln!($($arg)*);
        }
    };
}

/// 按 `MENGHUNOTE_TRACE_MENU` 决定是否输出菜单日志。
macro_rules! trace_menu {
    ($($arg:tt)*) => {
        if trace_menu_enabled() {
            eprintln!($($arg)*);
        }
    };
}

/// 右键菜单宽度（逻辑像素）。
const MENU_WIDTH: f32 = 148.0;
/// 菜单项高度。
const MENU_ITEM_HEIGHT: f32 = 26.0;
/// 菜单表头高度（标题一行 + padding）。
const MENU_HEADER_HEIGHT: f32 = 24.0;

use crate::editor::theme;
use crate::editor::view::EditorView;

/// 侧栏宽度（逻辑像素）。固定宽度：7b 不做可拖拽分栏。
const SIDEBAR_WIDTH: f32 = 220.0;
/// 每一级缩进（逻辑像素）。
const INDENT_PER_DEPTH: f32 = 16.0;
/// 行高（与状态栏字号 12 搭配）。
const ROW_HEIGHT: f32 = 24.0;
/// 切换文档时旧 driver 的有界排空上限（ADR 0002 §9：`close` 必须有界）。
const SWITCH_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);
/// dev-sync 的转发接口在「没有打开任何笔记」时用的明确失败文案（切片 3b）。
#[cfg(feature = "dev-sync")]
const NO_ACTIVE_DOC: &str = "dev-sync：当前没有打开的笔记，闸门/adopt 无处转发";

// ---- 图谱覆盖层（G1）----

/// 图谱节点标签盒子的宽度（逻辑像素）：标签在圆点下方、固定宽度单行省略，
/// 这样节点层的布局不会随标题长度抖动（绘制/命中只用圆点本身）。
const GRAPH_NODE_BOX_WIDTH: f32 = 116.0;
/// 图谱顶部工具条高度（逻辑像素）。
const GRAPH_HEADER_HEIGHT: f32 = 40.0;
/// 滚轮 `Lines` 增量换算成像素时用的行高（与编辑器行高量级一致）。
const GRAPH_SCROLL_LINE_HEIGHT: f32 = 20.0;
/// 侧栏「图谱」入口的底部留白（像素）。
///
/// dev-sync 构建里 `AppRoot` 的「⚙ 设置」是绝对定位（`bottom: 30`、高约 24），
/// 这段留白把「图谱」顶到它正上方，两个入口不重叠；默认构建没有那个按钮，
/// 留一点内边距贴底即可。
#[cfg(feature = "dev-sync")]
const GRAPH_ENTRY_BOTTOM_GAP: f32 = 40.0;
/// 默认构建（没有 `AppRoot` 设置按钮）的底部留白。
#[cfg(not(feature = "dev-sync"))]
const GRAPH_ENTRY_BOTTOM_GAP: f32 = 8.0;
/// 图谱过滤条高度（逻辑像素；工具条下面一行）。
const GRAPH_FILTER_HEIGHT: f32 = 32.0;
/// 图谱搜索框宽度（逻辑像素）。
const GRAPH_SEARCH_WIDTH: f32 = 168.0;
/// 过滤条最多显示几个文件夹 chips（超出只提示数量，不做横向滚动）。
const MAX_FOLDER_CHIPS: usize = 8;
/// 过滤条最多显示几个标签 chips。
const MAX_TAG_CHIPS: usize = 6;

/// 当前打开的笔记。
/// 正在编辑的行。
struct Editing {
    /// 目标节点。
    node_id: String,
    /// 输入框实体。
    input: Entity<RenameInput>,
}

struct OpenNote {
    /// 结构节点 ID（侧栏高亮用）。
    node_id: String,
    /// 笔记 ID（反向链接查询、链接改写用）。
    note_id: String,
    /// 真正渲染的编辑器视图。
    view: Entity<EditorView>,
    /// 编辑器事件的订阅（切换文档时 drop = 取消订阅）。
    /// 字段本身不读，靠 **Drop** 生效。
    _events: gpui::Subscription,
}

/// 图谱覆盖层的运行态（`None` = 没打开）。
///
/// **坐标只存在内存里**（`model` 里的布局 + `view` 的平移缩放），不写库、不进同步；
/// 关闭即丢弃，下次打开重新构建（G3 才考虑布局缓存）。
struct GraphOverlay {
    /// 整库模型（切换 scope/过滤的**基底**；打开时构建一次，不截断）。
    full: GraphModel,
    /// 当前显示的派生模型（局部深度 + 过滤 + 上限）。
    model: GraphModel,
    /// 平移/缩放：绘制、命中、点击共用这一份映射。
    view: GraphViewport,
    /// 最近一帧的画布尺寸（窗口坐标；命中与绘制必须用同一个值）。
    viewport: Size<Pixels>,
    /// 左键按下的手势（用来区分"点击"与"拖拽平移"）。
    drag: Option<GraphDrag>,
    /// 悬停节点下标（高亮它和邻居；`None` = 没悬停）。
    hovered: Option<usize>,
    /// 当前观察范围（局部/全库；G2）。
    scope: GraphScope,
    /// 当前过滤 + 显示选项（文件夹/标签/未解析/方向；G2）。
    options: GraphOptions,
    /// 顶层文件夹 chips（来自整库模型）。
    folders: Vec<String>,
    /// 标签 chips（来自整库模型；当前 app 没有打标签入口，通常为空）。
    tags: Vec<String>,
    /// 搜索输入框（自绘 + IME；复用 `search_input` 模块）。
    search_input: Entity<SearchInput>,
    /// 搜索状态（查询 / 匹配 / 选中）。
    search: GraphSearch,
    /// 搜索框事件订阅（drop = 取消订阅）。
    _search_events: gpui::Subscription,
}

/// 图谱里的一次左键手势。
struct GraphDrag {
    /// 按下时的屏幕位置。
    start: Point<Pixels>,
    /// 按下时的平移量：拖拽 = 从它加总增量，不累积浮点误差。
    origin_pan: Point<f32>,
    /// 按下瞬间命中的节点（点击时用它决定"打开笔记"还是"点遮罩关闭"）。
    pressed_node: Option<usize>,
    /// 按下瞬间命中的未解析占位（点它只提示，不关覆盖层）。
    pressed_ghost: Option<usize>,
    /// 已经超过拖拽阈值（此时抬手不算点击）。
    moved: bool,
}

/// 重命名联动改写后的汇总（UI 提示用）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LinkRenameTotals {
    /// 成功改写的来源篇数。
    notes_updated: usize,
    /// 改写的链接条数。
    links_rewritten: usize,
    /// 失败/未完成的篇数。
    notes_failed: usize,
}

/// 窗口根视图。
pub struct AppShell {
    /// 与侧栏共用同一个 vault 句柄（`Vault` 是 `Clone + Send + Sync`）。
    vault: Vault,
    /// 本机 Loro peer id（**唯一来源**：dev-sync 走持久身份映射，默认路径是
    /// [`DEFAULT_PEER_ID`]）。结构写入 / 会话 / 搜索物化全部用它，不得再出现常量。
    peer_id: u64,
    /// 本地写入的同步上下文（ADR 0009 切片 2b）：`Some` 时正文 flush 与结构提交在
    /// 同一个 A 事务里写 `sync_outbox` 意图；`None`（默认构建）行为完全不变。
    sync: Option<SyncContext>,
    /// 状态栏展示的库路径。
    vault_label: String,
    /// `list_tree` 的最近一次结果（先序 DFS，已展平）。
    tree: Vec<TreeEntry>,
    /// 当前打开的笔记；`None` 表示库里还没有可打开的笔记。
    active: Option<OpenNote>,
    /// 当前笔记的反向链接（L2；打开/切换/点开面板时刷新）。
    backlinks: Vec<Backlink>,
    /// 反向链接面板是否展开。
    backlinks_open: bool,
    /// 侧栏底部的临时提示（切换被拒、读树失败等）。
    notice: Option<String>,
    /// 已折叠的目录 `node_id`（默认全部展开；**不持久化**，v1 只存内存）。
    collapsed: HashSet<String>,
    /// 当前**选中**的节点（与"已打开的笔记"是两件事：选中可以是目录）。
    selected_node_id: Option<String>,
    /// 右键行弹出的菜单；`None` = 没有菜单。
    menu: Option<ContextMenu>,
    /// 笔记菜单里的「删除」是否已进入二次确认。
    /// **不设超时**：菜单关闭即复位（比"3 秒后自动复位"更可预期）。
    menu_delete_armed: bool,
    /// 是否正在后台导出（防重复点击）。
    exporting: bool,
    /// 下一帧要打开搜索浮层（事件回调里没有 `&mut Window`）。
    pending_search: bool,
    /// `MENGHUNOTE_SEARCH_ON_START` 只用一次（关闭后不再自动开）。
    search_auto_used: bool,
    /// 搜索浮层（`None` = 关闭）。
    search: Option<Entity<crate::search_overlay::SearchOverlay>>,
    /// 图谱覆盖层（`None` = 关闭）。打开时不销毁编辑器：只盖一层 canvas。
    graph: Option<GraphOverlay>,
    /// 图谱覆盖层的焦点（接 `Esc`；打开时抢焦点，关闭后还给编辑器）。
    graph_focus: FocusHandle,
    /// 正在行内重命名的节点（含输入框实体）。
    editing: Option<Editing>,
    /// 下一帧需要把焦点给新的输入框（订阅回调里没有 `&mut Window`）。
    pending_focus_rename: bool,
    /// 菜单打开的时刻（写日志、以及"同一次手势不关菜单"的守卫用）。
    menu_opened_at: Option<std::time::Instant>,
    /// 最近一次点中菜单动作的时刻（吞掉紧跟着的 mouse-up 落到下层行）。
    menu_action_at: Option<std::time::Instant>,
    /// 本次打开是否已经画过一帧（诊断"状态已设置但没画出来"）。
    menu_painted: std::cell::Cell<bool>,
    /// 视图创建时刻：日志里的相对时间戳。
    epoch: std::time::Instant,
    /// 菜单面板的焦点（用于接 `Esc`）。
    menu_focus: FocusHandle,
    /// 是否已订阅窗口激活变化（只订阅一次）。
    activation_observed: bool,
    /// 激活变化的订阅（持有到视图销毁；drop 即取消订阅）。
    activation_subscription: Option<gpui::Subscription>,
    /// 系统外观（浅色/深色）变化的订阅（ADR 0021 P2；只订阅一次，drop 即取消）。
    ///
    /// 默认构建也订阅：`ThemeMode::System` 下 OS 切换要立即重绘。
    appearance_subscription: Option<gpui::Subscription>,
    /// dev-only 同步面板（ADR 0009 Phase 1 / 切片 1）。
    ///
    /// **只有** `--features dev-sync` + 运行时门禁通过时才存在；默认构建里这个字段
    /// 连类型都不存在（模块被 cfg 掉），因此结构体布局/渲染分支零变化。
    #[cfg(feature = "dev-sync")]
    dev_sync: Option<Entity<crate::dev_sync::DevSyncPanel>>,
    /// V2 管理面板（ADR 0016 S4d1c；设置页里的「管理」区）。
    ///
    /// 只有 `dev-sync` + 真实 dev-local 服务（有 `dev-identity.bin` 路径）时才创建；
    /// 默认构建/注入 fake runner 的测试里是 `None`（不渲染管理区）。
    #[cfg(feature = "dev-sync")]
    management: Option<Entity<crate::dev_sync::ManagementPanel>>,
    /// 设置页里的「管理」区是否展开（默认收起，避免设置页默认触发网络刷新）。
    #[cfg(feature = "dev-sync")]
    management_open: bool,
    /// dev-sync 设置页是否打开（默认关闭；面板只在设置页里渲染）。
    #[cfg(feature = "dev-sync")]
    settings_open: bool,
    /// 设置页焦点（Esc 关闭用）。
    #[cfg(feature = "dev-sync")]
    settings_focus: FocusHandle,
    /// 面板事件订阅（切片 4c-2）：「同步」按钮只发出 `SyncRequested`，由本视图持闸后派发。
    #[cfg(feature = "dev-sync")]
    dev_sync_subscription: Option<gpui::Subscription>,
    /// 在途的持闸同步（ADR 0009 切片 4c-2）；`None` = 没有同步在跑。
    #[cfg(feature = "dev-sync")]
    dev_sync_sync: Option<crate::dev_sync::ActiveSyncOnce>,
    /// 已收到「同步」按钮请求、还没在 `dev_sync_pump` 里消费（事件回调里不重入面板）。
    #[cfg(feature = "dev-sync")]
    dev_sync_requested: bool,
    /// 已收到「发布基线」按钮请求、还没在 `dev_sync_pump` 里消费（切片 4c-4）。
    #[cfg(feature = "dev-sync")]
    dev_sync_publish_requested: bool,
    /// 已收到「轮换密钥」确认请求、还没在 `dev_sync_pump` 里消费（切片 6b）。
    ///
    /// 事件只带"用户确认了"，真正的恢复码/revoke 列表留在面板里，由泵取走
    /// （[`crate::dev_sync::DevSyncPanel::take_rotate_request`]）——秘密不进事件/日志。
    #[cfg(feature = "dev-sync")]
    dev_sync_rotate_requested: bool,
    /// 已收到「应用轮换」确认请求、还没在 `dev_sync_pump` 里消费（切片 6b）。
    #[cfg(feature = "dev-sync")]
    dev_sync_apply_requested: bool,
    /// 最近一次持闸同步的报告（面板文案 + 测试断言）。
    #[cfg(feature = "dev-sync")]
    dev_sync_last_report: Option<crate::dev_sync::SyncOnceReport>,
    /// 自动同步接线（ADR 0012 S3）：纯调度器 + 状态投影 + 注入的单调时钟。
    ///
    /// `None` = dev-sync 未启用（默认构建/未挂面板）：零自动行为。
    #[cfg(feature = "dev-sync")]
    dev_sync_auto: Option<crate::dev_sync::AutoSync>,
    /// 当前这一轮的来源（自动/手动）。收尾时交给 [`crate::dev_sync::AutoSync`] 记账：
    /// 自动轮进退避/续批，手动轮清退避但不参与自动续批。
    #[cfg(feature = "dev-sync")]
    dev_sync_round_origin: Option<crate::dev_sync::OpOrigin>,
    /// 本轮自动同步是因为"等不到输入法空闲"主动 defer 的（不算失败、不计退避）。
    #[cfg(feature = "dev-sync")]
    dev_sync_auto_deferred: bool,
    /// 已观测到的活动文档本地编辑代数（自动同步的 debounce 触发源）。
    #[cfg(feature = "dev-sync")]
    dev_sync_auto_edit_seq: u64,
    /// 上一次打印/观察到的同步阶段（只用于诊断日志，避免每帧重复）。
    #[cfg(feature = "dev-sync")]
    dev_sync_last_stage: Option<crate::dev_sync::SyncOnceStage>,
    /// 粘贴/拖放导入的失败原因（`import-error` 行；校验失败/持闸/落库失败都写这里）。
    import_error: Option<String>,
    /// 粘贴/拖放导入的成功提示（`import-notice` 行）。
    import_notice: Option<String>,
}

/// 右键行弹出的上下文菜单。
#[derive(Debug, Clone, PartialEq)]
struct ContextMenu {
    /// 目标节点（目录 = 新建落点；笔记 = 删除目标）。
    node_id: String,
    /// 节点标题（菜单表头用）。
    title: String,
    /// 目标是不是目录（决定菜单项集合）。
    is_folder: bool,
    /// 菜单左上角（窗口逻辑坐标，渲染前会夹进窗口内）。
    position: Point<Pixels>,
}

impl AppShell {
    /// 打开搜索浮层（`Ctrl+K` / `Ctrl+F`）。
    ///
    /// 创建与聚焦都在这里做：事件回调里拿不到 `&mut Window`，所以由 render 的
    /// "待打开"标记转进来一次。
    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_some() {
            return;
        }
        let weak = cx.weak_entity();
        // 打开一条结果：按 note_id 在树里找节点 → 复用 open_entry；找不到说明笔记已删。
        let on_open: crate::search_overlay::OpenHandler = {
            let weak = weak.clone();
            std::rc::Rc::new(move |note_id: &str, window: &mut Window, cx: &mut App| {
                let Some(shell) = weak.upgrade() else {
                    return;
                };
                shell.update(cx, |shell, cx| {
                    let index = shell
                        .tree
                        .iter()
                        .position(|entry| entry.note_id.as_deref() == Some(note_id));
                    match index {
                        Some(index) => {
                            eprintln!(
                                "[menghunote] 打开搜索结果 note={note_id}（树节点 #{index}）"
                            );
                            shell.open_entry(index, cx);
                            shell.close_search(window, cx);
                        }
                        None => {
                            // 笔记已删：留在浮层里，给一行提示。
                            eprintln!("[menghunote] 搜索结果已失效 note={note_id}（树里找不到）");
                            shell.notice = Some("笔记已不存在".to_owned());
                            cx.notify();
                        }
                    }
                });
            })
        };
        let on_close: crate::search_overlay::CloseHandler =
            std::rc::Rc::new(move |window: &mut Window, cx: &mut App| {
                if let Some(shell) = weak.upgrade() {
                    shell.update(cx, |shell, cx| {
                        shell.close_search(window, cx);
                    });
                }
            });
        let vault = self.vault.clone();
        let peer_id = self.peer_id;
        let overlay = cx.new(|cx| {
            crate::search_overlay::SearchOverlay::new(vault, peer_id, window, cx, on_open, on_close)
        });
        overlay.update(cx, |overlay, cx| overlay.focus_input(window, cx));
        trace_menu!("[menghunote] 打开搜索浮层");
        self.search = Some(overlay);
        cx.notify();
    }

    /// 关闭搜索浮层并把焦点还给正文编辑器。
    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_none() {
            return;
        }
        trace_menu!("[menghunote] 关闭搜索浮层");
        // 焦点回到编辑器：`EntityInputHandler` 重新成为输入目标，编辑器 IME 状态无需重建。
        if let Some(active) = &self.active {
            active.view.update(cx, |_view, cx| {
                cx.focus_self(window);
            });
        }
        cx.notify();
    }

    // ---- 图谱覆盖层（G1/G2）：打开/关闭/交互 ----

    /// 打开图谱覆盖层（侧栏底部「图谱」）。
    ///
    /// **默认视图（G2 决策）**：有当前打开的笔记 → 以它为中心的**局部图（2 跳）**；
    /// 没有当前笔记 → 全库图（空库就是空图）。打开前先按现有表构建一次**整库模型**
    /// （`tree`/`notes`/`links`/`note_tags` 只读查询，几毫秒级）；失败只写侧栏提示，
    /// **不 panic、不改编辑器**。打开时把焦点交给搜索框（`Esc` 由它抛事件关闭覆盖层），
    /// 编辑器的文档/滚动/撤销状态原样保留（覆盖层只是盖在上面）。
    fn open_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.graph.is_some() {
            return;
        }
        let viewport = window.bounds().size;
        match GraphModel::build(&self.vault) {
            Ok(full) => {
                // 局部图优先：中心 = 当前笔记；没有打开的笔记时回退全库图。
                let scope = match self.active.as_ref() {
                    Some(active) => GraphScope::local(active.note_id.clone(), DEFAULT_LOCAL_DEPTH),
                    None => GraphScope::full(),
                };
                let options = GraphOptions::default();
                let model = full.derive(&scope, &options, GraphLimits::default());
                let mut view = GraphViewport::default();
                view.fit_to(&model, viewport);
                let folders = full.folders();
                let tags = full.tags();
                eprintln!(
                    "[menghunote] 打开图谱：{} · 节点 {} 边 {} 未解析 {} 自环 {} 悬空 {}（zoom={:.2}）",
                    if scope.is_local() { "局部" } else { "全库" },
                    model.node_count(),
                    model.edge_count(),
                    model.unresolved_links(),
                    model.self_links(),
                    model.dangling_links(),
                    view.zoom
                );
                // 自绘输入框（带 IME）：组件 `Input` 没有 `EntityInputHandler`，见 search_input.rs。
                let search_input = cx.new(SearchInput::new);
                let search_events = cx.subscribe_in(
                    &search_input,
                    window,
                    |shell, _input, event: &SearchInputEvent, window, cx| match event {
                        SearchInputEvent::Changed => shell.graph_search_changed(cx),
                        SearchInputEvent::Submit => shell.graph_search_submit(window, cx),
                        SearchInputEvent::Escape => shell.close_graph(window, cx),
                    },
                );
                self.graph = Some(GraphOverlay {
                    full,
                    model,
                    view,
                    viewport,
                    drag: None,
                    hovered: None,
                    scope,
                    options,
                    folders,
                    tags,
                    search_input: search_input.clone(),
                    search: GraphSearch::default(),
                    _search_events: search_events,
                });
                // 焦点直接给搜索框（打开即可输入；`Esc` 由输入框抛事件关闭覆盖层）。
                search_input.update(cx, |input, cx| input.focus(window, cx));
                cx.notify();
            }
            Err(error) => {
                eprintln!("[menghunote] 构建图谱失败：{error:?}");
                self.notice = Some(format!("图谱构建失败：{error}"));
                cx.notify();
            }
        }
    }

    /// 关闭图谱覆盖层，并把焦点还给当前编辑器。
    fn close_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.graph.take().is_none() {
            return;
        }
        eprintln!("[menghunote] 关闭图谱");
        self.focus_active_editor(window, cx);
        cx.notify();
    }

    /// 点中图谱节点：按 `note_id` 找结构树行 → 关覆盖层 → 复用 [`Self::open_entry`]。
    ///
    /// 找不到（笔记已删/结构树还没刷新）时只提示，不猜、不自动建。
    fn open_graph_node(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(note_id) = self
            .graph
            .as_ref()
            .and_then(|state| state.model.nodes().get(index))
            .map(|node| node.note_id.clone())
        else {
            return;
        };
        let Some(tree_index) = self
            .tree
            .iter()
            .position(|entry| entry.note_id.as_deref() == Some(note_id.as_str()))
        else {
            eprintln!("[menghunote] 图谱节点已失效 note={note_id}（结构树里找不到）");
            self.notice = Some("笔记已不存在".to_owned());
            self.close_graph(window, cx);
            return;
        };
        eprintln!("[menghunote] 图谱点开 note={note_id}（树节点 #{tree_index}）");
        self.close_graph(window, cx);
        self.open_entry(tree_index, cx);
    }

    /// 重新派生当前视图（scope/过滤/深度变化）：**不重查数据库**，只做 O(V+E) 派生。
    ///
    /// 派生后重新自适应（过滤/范围变化会改变图的大小），并把搜索匹配重算一遍
    /// （同一查询在新视图里可能不同）；有匹配时仍居中到选中项。
    fn graph_rebuild(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.graph.as_mut() else {
            return;
        };
        state.model = state
            .full
            .derive(&state.scope, &state.options, GraphLimits::default());
        state.drag = None;
        state.hovered = None;
        let query = state.search.query.clone();
        state.search = GraphSearch::apply(&state.model, &query);
        state.view.fit_to(&state.model, state.viewport);
        if let Some(index) = state.search.selected_node() {
            if let Some(node) = state.model.nodes().get(index) {
                state.view.center_on(node.pos);
            }
        }
        cx.notify();
    }

    /// 切换局部/全库（局部图以当前笔记为中心；没有打开的笔记时只提示、保持全库）。
    fn graph_set_scope(&mut self, local: bool, cx: &mut Context<Self>) {
        if self.graph.is_none() {
            return;
        }
        if local {
            let Some(note_id) = self.active.as_ref().map(|active| active.note_id.clone()) else {
                self.notice = Some("没有打开的笔记，已显示全库图".to_owned());
                cx.notify();
                return;
            };
            let depth = self
                .graph
                .as_ref()
                .map_or(DEFAULT_LOCAL_DEPTH, |state| state.scope.clamped_depth());
            if let Some(state) = self.graph.as_mut() {
                state.scope = GraphScope::local(note_id, depth);
            }
        } else if let Some(state) = self.graph.as_mut() {
            // 切回全库时**保留上次的局部深度**（再切回局部不用重设 1/2/3）。
            state.scope = GraphScope {
                center: None,
                depth: state.scope.clamped_depth(),
            };
        }
        self.graph_rebuild(cx);
    }

    /// 设置局部图深度（1–3 跳；全库图下无效果）。
    fn graph_set_depth(&mut self, depth: usize, cx: &mut Context<Self>) {
        {
            let Some(state) = self.graph.as_mut() else {
                return;
            };
            if !state.scope.is_local() {
                return;
            }
            state.scope.depth = depth.clamp(MIN_LOCAL_DEPTH, MAX_LOCAL_DEPTH);
        }
        self.graph_rebuild(cx);
    }

    /// 按顶层文件夹过滤（`None` = 全部）。
    fn graph_set_folder(&mut self, folder: Option<String>, cx: &mut Context<Self>) {
        {
            let Some(state) = self.graph.as_mut() else {
                return;
            };
            state.options.folder = folder;
        }
        self.graph_rebuild(cx);
    }

    /// 按标签过滤（`None` = 全部）。
    fn graph_set_tag(&mut self, tag: Option<String>, cx: &mut Context<Self>) {
        {
            let Some(state) = self.graph.as_mut() else {
                return;
            };
            state.options.tag = tag;
        }
        self.graph_rebuild(cx);
    }

    /// 未解析链接开关（虚线 + 灰色占位；默认关）。
    fn graph_toggle_unresolved(&mut self, cx: &mut Context<Self>) {
        {
            let Some(state) = self.graph.as_mut() else {
                return;
            };
            state.options.show_unresolved = !state.options.show_unresolved;
        }
        self.graph_rebuild(cx);
    }

    /// 方向箭头开关（默认关；只影响绘制，不重派生模型）。
    fn graph_toggle_arrows(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.graph.as_mut() else {
            return;
        };
        state.options.show_arrows = !state.options.show_arrows;
        cx.notify();
    }

    /// 搜索框文本变化：重算匹配；非空时居中到第一个匹配，空查询恢复全景。
    fn graph_search_changed(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.graph.as_mut() else {
            return;
        };
        let query = state.search_input.read(cx).text().to_owned();
        state.search = GraphSearch::apply(&state.model, &query);
        if state.search.is_active() {
            if let Some(index) = state.search.selected_node() {
                if let Some(node) = state.model.nodes().get(index) {
                    state.view.center_on(node.pos);
                }
            }
        } else {
            // 空查询恢复：重新自适应（回到打开时的全景）。
            state.view.fit_to(&state.model, state.viewport);
        }
        cx.notify();
    }

    /// 搜索框 `Enter`：打开当前选中的匹配（没有匹配时只提示）。
    fn graph_search_submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self
            .graph
            .as_ref()
            .and_then(|state| state.search.selected_node());
        let Some(index) = selected else {
            if self
                .graph
                .as_ref()
                .is_some_and(|state| state.search.is_active())
            {
                self.notice = Some("没有匹配的笔记".to_owned());
                cx.notify();
            }
            return;
        };
        self.open_graph_node(index, window, cx);
    }

    /// 搜索框 `↑`/`↓`：在匹配之间切换（并居中到新选中项）。
    fn graph_search_move(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(state) = self.graph.as_mut() else {
            return;
        };
        if !state.search.is_active() {
            return;
        }
        if forward {
            state.search.select_next();
        } else {
            state.search.select_prev();
        }
        if let Some(index) = state.search.selected_node() {
            if let Some(node) = state.model.nodes().get(index) {
                state.view.center_on(node.pos);
            }
        }
        cx.notify();
    }

    /// 左键按下：记录手势（是否命中节点/占位 + 起始平移量）。
    fn graph_mouse_down(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(state) = self.graph.as_mut() else {
            return;
        };
        let pressed_node = state.view.hit_test(&state.model, position, state.viewport);
        let pressed_ghost = if pressed_node.is_none() {
            state
                .view
                .ghost_hit_test(&state.model, position, state.viewport)
        } else {
            None
        };
        state.drag = Some(GraphDrag {
            start: position,
            origin_pan: state.view.pan,
            pressed_node,
            pressed_ghost,
            moved: false,
        });
        cx.notify();
    }

    /// 鼠标移动：拖拽平移 + 悬停高亮（同一个几何：`hit_test`）。
    fn graph_mouse_move(
        &mut self,
        position: Point<Pixels>,
        pressed: Option<MouseButton>,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.graph.as_mut() else {
            return;
        };
        let mut changed = false;
        let mut pan_to = None;
        if let Some(drag) = state.drag.as_mut() {
            if pressed == Some(MouseButton::Left) {
                let dx = position.x.as_f32() - drag.start.x.as_f32();
                let dy = position.y.as_f32() - drag.start.y.as_f32();
                if !drag.moved && (dx * dx + dy * dy).sqrt() > crate::graph::DRAG_THRESHOLD {
                    drag.moved = true;
                }
                if drag.moved {
                    pan_to = Some(point(drag.origin_pan.x + dx, drag.origin_pan.y + dy));
                }
            }
        }
        if let Some(pan) = pan_to {
            state.view.pan = pan;
            changed = true;
        }
        let dragging = state.drag.as_ref().is_some_and(|drag| drag.moved);
        let hovered = if dragging {
            None
        } else {
            state.view.hit_test(&state.model, position, state.viewport)
        };
        if hovered != state.hovered {
            state.hovered = hovered;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// 左键抬起：没拖动才算"点击"——命中节点打开笔记，命中未解析占位只提示，
    /// 空白处 = 点遮罩关闭。
    fn graph_mouse_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drag) = self.graph.as_mut().and_then(|state| state.drag.take()) else {
            return;
        };
        if drag.moved {
            // 拖拽结束：只结束手势，不打开/不关闭。
            cx.notify();
            return;
        }
        match (drag.pressed_node, drag.pressed_ghost) {
            (Some(index), _) => self.open_graph_node(index, window, cx),
            (None, Some(ghost)) => {
                let target = self
                    .graph
                    .as_ref()
                    .and_then(|state| state.model.ghosts().get(ghost))
                    .map(|ghost| ghost.target.clone());
                if let Some(target) = target {
                    eprintln!("[menghunote] 图谱未解析占位：{target}（不自动建笔记）");
                    self.notice = Some(format!("未解析链接：{target}（不会自动建笔记）"));
                }
                cx.notify();
            }
            (None, None) => self.close_graph(window, cx),
        }
    }

    /// 滚轮缩放（以鼠标位置为锚点，缩放有界 0.2–3.0）。
    fn graph_scroll(
        &mut self,
        position: Point<Pixels>,
        delta: ScrollDelta,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.graph.as_mut() else {
            return;
        };
        let pixels = delta.pixel_delta(px(GRAPH_SCROLL_LINE_HEIGHT));
        // 平台给的是"内容位移"（正 = 滚轮向上）→ 向上放大。
        let factor = scroll_zoom_factor(pixels.y.as_f32());
        state.view.zoom_by(factor, position, state.viewport);
        cx.notify();
    }

    /// 复位：重新自适应缩放并居中（图被拖丢时的手动兜底）。
    fn graph_reset(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.graph.as_mut() else {
            return;
        };
        state.view.fit_to(&state.model, state.viewport);
        state.hovered = None;
        state.drag = None;
        cx.notify();
    }

    /// 图谱覆盖层是否打开（测试/诊断用）。
    #[must_use]
    pub fn graph_is_open(&self) -> bool {
        self.graph.is_some()
    }

    /// 当前图谱模型（`None` = 未打开）。
    #[must_use]
    pub fn graph_model(&self) -> Option<&GraphModel> {
        self.graph.as_ref().map(|state| &state.model)
    }

    /// 当前视口（平移/缩放；`None` = 未打开）。
    #[must_use]
    pub fn graph_view(&self) -> Option<GraphViewport> {
        self.graph.as_ref().map(|state| state.view)
    }

    /// 最近一帧的画布尺寸（命中/绘制共用；`None` = 未打开）。
    #[must_use]
    pub fn graph_viewport(&self) -> Option<Size<Pixels>> {
        self.graph.as_ref().map(|state| state.viewport)
    }

    /// 图谱头部统计行（`None` = 未打开）。
    #[must_use]
    pub fn graph_hint_text(&self) -> Option<String> {
        self.graph.as_ref().map(|state| hint_line(&state.model))
    }

    /// 当前悬停节点下标（`None` = 未打开或没悬停）。
    #[must_use]
    pub fn graph_hovered(&self) -> Option<usize> {
        self.graph.as_ref().and_then(|state| state.hovered)
    }

    /// 当前观察范围（`None` = 未打开）。
    #[must_use]
    pub fn graph_scope(&self) -> Option<&GraphScope> {
        self.graph.as_ref().map(|state| &state.scope)
    }

    /// 当前过滤 + 显示选项（`None` = 未打开）。
    #[must_use]
    pub fn graph_options(&self) -> Option<&GraphOptions> {
        self.graph.as_ref().map(|state| &state.options)
    }

    /// 当前搜索查询（`None` = 未打开；空串 = 未搜索）。
    #[must_use]
    pub fn graph_search_query(&self) -> Option<&str> {
        self.graph.as_ref().map(|state| state.search.query.as_str())
    }

    /// 当前搜索匹配的节点下标（模型顺序）。
    #[must_use]
    pub fn graph_search_matches(&self) -> Vec<usize> {
        self.graph
            .as_ref()
            .map(|state| state.search.matches.clone())
            .unwrap_or_default()
    }

    /// 当前选中的匹配（`None` = 未搜索/无匹配）。
    #[must_use]
    pub fn graph_selected_match(&self) -> Option<usize> {
        self.graph
            .as_ref()
            .and_then(|state| state.search.selected_node())
    }

    /// 未解析占位节点数（`show_unresolved = false` 时恒为 0）。
    #[must_use]
    pub fn graph_ghost_count(&self) -> usize {
        self.graph
            .as_ref()
            .map_or(0, |state| state.model.ghost_count())
    }

    /// 被 scope/过滤隐藏的节点数。
    #[must_use]
    pub fn graph_hidden_nodes(&self) -> usize {
        self.graph
            .as_ref()
            .map_or(0, |state| state.model.hidden_nodes())
    }

    /// 当前 LOD（低缩放不画标签/箭头/细边）。
    #[must_use]
    pub fn graph_lod(&self) -> Option<GraphLod> {
        self.graph
            .as_ref()
            .map(|state| GraphLod::for_zoom(state.view.zoom))
    }

    /// 当前打开的笔记 ID（`None` = 没有打开的笔记；局部图的默认中心）。
    #[must_use]
    pub fn active_note_id(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.note_id.as_str())
    }

    /// 图谱节点标题（按模型顺序；headless 断言排序/数量用）。
    #[must_use]
    pub fn graph_node_titles(&self) -> Vec<&str> {
        self.graph
            .as_ref()
            .map(|state| {
                state
                    .model
                    .nodes()
                    .iter()
                    .map(|node| node.title.as_str())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 图谱节点的屏幕中心（窗口坐标；headless 点击用）。
    ///
    /// 与绘制共用 [`GraphViewport::screen_pos`]，不是第二份几何。
    #[must_use]
    pub fn graph_node_center(&self, note_id: &str) -> Option<Point<Pixels>> {
        let state = self.graph.as_ref()?;
        let index = state.model.node_index(note_id)?;
        let node = state.model.nodes().get(index)?;
        Some(state.view.screen_pos(node.pos, state.viewport))
    }

    /// 组装根视图：读一次树，并打开第一篇可打开的笔记（默认 peer = 1，行为不变）。
    pub fn new(vault: Vault, vault_label: String, cx: &mut Context<Self>) -> Self {
        Self::new_with_peer(vault, vault_label, DEFAULT_PEER_ID, cx)
    }

    /// 组装根视图并显式注入本机 peer（ADR 0009 切片 2a）。
    ///
    /// dev-sync 启用时 `main.rs` 传入 `LocalIdentityV1::loro_peer_id()`：结构写入、
    /// `SessionDriver`（flush/物化）与搜索物化都统一用它。默认路径继续走 [`Self::new`]
    /// 的常量 1，仓内既有调用点不受影响。
    pub fn new_with_peer(
        vault: Vault,
        vault_label: String,
        peer_id: u64,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_peer_and_sync(vault, vault_label, peer_id, None, cx)
    }

    /// 组装根视图并注入 peer + 同步上下文（ADR 0009 切片 2b）。
    ///
    /// `sync = Some(ctx)` 时，**构造期就打开**的第一篇笔记的 session 与之后的每次
    /// [`Self::write_structure`] 都会用同一个 `SyncContext`；`None` 等价于
    /// [`Self::new_with_peer`]（零行为变化）。
    pub fn new_with_peer_and_sync(
        vault: Vault,
        vault_label: String,
        peer_id: u64,
        sync: Option<SyncContext>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut shell = Self {
            vault,
            peer_id,
            sync,
            vault_label,
            tree: Vec::new(),
            active: None,
            backlinks: Vec::new(),
            backlinks_open: false,
            notice: None,
            collapsed: HashSet::new(),
            selected_node_id: None,
            menu: None,
            menu_delete_armed: false,
            editing: None,
            exporting: false,
            pending_search: false,
            search_auto_used: false,
            search: None,
            graph: None,
            graph_focus: cx.focus_handle(),
            pending_focus_rename: false,
            menu_opened_at: None,
            menu_action_at: None,
            menu_painted: std::cell::Cell::new(false),
            epoch: std::time::Instant::now(),
            menu_focus: cx.focus_handle(),
            activation_observed: false,
            activation_subscription: None,
            appearance_subscription: None,
            #[cfg(feature = "dev-sync")]
            dev_sync: None,
            #[cfg(feature = "dev-sync")]
            management: None,
            #[cfg(feature = "dev-sync")]
            management_open: false,
            #[cfg(feature = "dev-sync")]
            settings_open: false,
            #[cfg(feature = "dev-sync")]
            settings_focus: cx.focus_handle(),
            #[cfg(feature = "dev-sync")]
            dev_sync_subscription: None,
            #[cfg(feature = "dev-sync")]
            dev_sync_sync: None,
            #[cfg(feature = "dev-sync")]
            dev_sync_requested: false,
            #[cfg(feature = "dev-sync")]
            dev_sync_publish_requested: false,
            #[cfg(feature = "dev-sync")]
            dev_sync_rotate_requested: false,
            #[cfg(feature = "dev-sync")]
            dev_sync_apply_requested: false,
            #[cfg(feature = "dev-sync")]
            dev_sync_last_report: None,
            #[cfg(feature = "dev-sync")]
            dev_sync_auto: None,
            #[cfg(feature = "dev-sync")]
            dev_sync_round_origin: None,
            #[cfg(feature = "dev-sync")]
            dev_sync_auto_deferred: false,
            #[cfg(feature = "dev-sync")]
            dev_sync_auto_edit_seq: 0,
            #[cfg(feature = "dev-sync")]
            dev_sync_last_stage: None,
            import_error: None,
            import_notice: None,
        };
        shell.refresh_tree();
        if let Some(index) = shell.tree.iter().position(TreeEntry::is_note) {
            shell.open_entry(index, cx);
        } else {
            eprintln!("[menghunote] 库里还没有笔记，侧栏为空（不报错）");
        }
        shell
    }

    /// 组装根视图，并挂上 dev-only 同步面板（ADR 0009 Phase 1 / 切片 1）。
    ///
    /// 只有 `--features dev-sync` **且** `main.rs` 的运行时门禁通过才会走到这里；
    /// 面板是**覆盖层**（绝对定位在右上角），不挤压侧栏/编辑器的布局。
    ///
    /// `sync` 是持久身份给出的 `SyncContext { peer_id: loro_peer_id(), key_epoch }`：
    /// 结构与正文写入都用它，因此 outbox 里的行天然带身份 peer/epoch，而不是常量 1/0。
    /// 生产 Main 入口（ADR 0012 S3）：与 [`Self::new_with_dev_sync`] 相同，但**启用**自动
    /// 同步的墙钟 ticker（40ms 一拍推进调度器）。
    ///
    /// 为什么不默认开在 [`Self::new_with_dev_sync`]：那条入口同时被大量 headless 测试
    /// 使用（它们靠显式 `dev_sync_pump`/`dev_sync_auto_tick` 收敛，不该有后台 ticker
    /// 在真实时间里偷偷启动同步）。生产入口（`main.rs` / `AppRoot` 的 Main）走这一条。
    #[cfg(feature = "dev-sync")]
    pub fn new_with_dev_sync_with_autosync(
        vault: Vault,
        vault_label: String,
        dev_sync: crate::dev_sync::gate::DevSyncConfig,
        sync: SyncContext,
        cx: &mut Context<Self>,
    ) -> Self {
        let crate::dev_sync::gate::DevSyncConfig {
            vault: vault_path,
            relay_mode,
            service,
        } = dev_sync;
        eprintln!(
            "[menghunote] dev-sync 已启用（自动同步）：vault = {}，relay = {}，模式 = {}",
            vault_path.display(),
            service.relay,
            relay_mode.as_str()
        );
        let service = crate::dev_sync::service::SyncService::new_dev_local(service, vault_path);
        let auto = crate::dev_sync::AutoSync::with_system_clock(std::time::Instant::now());
        Self::new_with_dev_sync_service_auto(vault, vault_label, sync, service, Some(auto), cx)
    }

    /// dev-only 面板 + worker（**不**启用自动同步 ticker）：headless 测试与确定性复现用。
    ///
    /// 生产 Main 请用 [`Self::new_with_dev_sync_with_autosync`]。
    #[cfg(feature = "dev-sync")]
    pub fn new_with_dev_sync(
        vault: Vault,
        vault_label: String,
        dev_sync: crate::dev_sync::gate::DevSyncConfig,
        sync: SyncContext,
        cx: &mut Context<Self>,
    ) -> Self {
        let crate::dev_sync::gate::DevSyncConfig {
            vault: vault_path,
            relay_mode,
            service,
        } = dev_sync;
        eprintln!(
            "[menghunote] dev-sync 已启用：vault = {}，relay = {}，模式 = {}",
            vault_path.display(),
            service.relay,
            relay_mode.as_str()
        );
        // 切片 4c-2：生产 runner —— 同步在 worker 线程里真实执行
        // （自己的 `Vault` 连接 + 第一次同步时解封身份）；UI 线程不碰网络/Argon2。
        let service = crate::dev_sync::service::SyncService::new_dev_local(service, vault_path);
        // 这条入口保持确定性（headless 测试）：自动同步接线在，但**没有**墙钟 ticker。
        let auto = crate::dev_sync::AutoSync::new(
            crate::dev_sync::AutoSyncConfig::new().no_ticker(),
            std::sync::Arc::new(crate::dev_sync::SystemOperationClock),
            std::time::Instant::now(),
        );
        Self::new_with_dev_sync_service_auto(vault, vault_label, sync, service, Some(auto), cx)
    }

    /// 与 [`Self::new_with_dev_sync`] 相同，但注入**已经构造好的** `SyncService`。
    ///
    /// 生产路径不用它；测试用它注入 `ControlledRunner`（ADR 0012 S2a 的阶段/超时/
    /// 授权竞争），从而在真实 `AppShell` 上验证"网络阶段不冻结、Held 只覆盖
    /// integrate+adopt"这些**窗口**行为，而不是只测纯状态机。
    #[cfg(feature = "dev-sync")]
    pub fn new_with_dev_sync_service(
        vault: Vault,
        vault_label: String,
        sync: SyncContext,
        service: std::sync::Arc<crate::dev_sync::service::SyncService>,
        cx: &mut Context<Self>,
    ) -> Self {
        // 注入服务的构造保持确定性（headless）：自动同步接线在，但没有墙钟 ticker；
        // 确定性用例（手动时钟）用 `_auto` 变体显式注入 [`crate::dev_sync::AutoSync`]。
        let auto = crate::dev_sync::AutoSync::new(
            crate::dev_sync::AutoSyncConfig::new().no_ticker(),
            std::sync::Arc::new(crate::dev_sync::SystemOperationClock),
            std::time::Instant::now(),
        );
        Self::new_with_dev_sync_service_auto(vault, vault_label, sync, service, Some(auto), cx)
    }

    /// 与 [`Self::new_with_dev_sync_service`] 相同，但注入**已经构造好的**自动同步接线。
    ///
    /// 测试用 `AutoSyncConfig::no_ticker()` + 手动时钟（`OperationClock`），再显式
    /// [`Self::dev_sync_auto_tick`]；生产走 [`Self::new_with_dev_sync`] 的系统时钟 + ticker。
    #[cfg(feature = "dev-sync")]
    pub fn new_with_dev_sync_service_auto(
        vault: Vault,
        vault_label: String,
        sync: SyncContext,
        service: std::sync::Arc<crate::dev_sync::service::SyncService>,
        auto: Option<crate::dev_sync::AutoSync>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_dev_sync_service_auto_and_management(
            vault,
            vault_label,
            sync,
            service,
            auto,
            None,
            cx,
        )
    }

    /// 与 [`Self::new_with_dev_sync_service_auto`] 相同，但允许注入**管理执行体**
    /// （ADR 0016 S4d1c）。
    ///
    /// 生产路径传 `None`：若同步服务带真实库路径，就用 [`crate::dev_sync::RealManagementRunner`]
    /// 构造管理面板（身份解封/网络都在管理 worker 线程里）。测试传脚本化 runner，
    /// 在真实 `AppShell` + 设置页里验证管理区交互（不发真实网络请求）。
    #[cfg(feature = "dev-sync")]
    pub fn new_with_dev_sync_service_auto_and_management(
        vault: Vault,
        vault_label: String,
        sync: SyncContext,
        service: std::sync::Arc<crate::dev_sync::service::SyncService>,
        auto: Option<crate::dev_sync::AutoSync>,
        management: Option<Box<dyn crate::dev_sync::ManagementRunner>>,
        cx: &mut Context<Self>,
    ) -> Self {
        // 管理面板与同步面板共享同一个 `Vault` 句柄（`Vault` 是 `Clone + Send + Sync`）。
        let management_vault = vault.clone();
        let mut shell =
            Self::new_with_peer_and_sync(vault, vault_label, sync.peer_id, Some(sync), cx);
        let panel =
            cx.new(|cx| crate::dev_sync::panel::DevSyncPanel::with_service(service.clone(), cx));
        // 「同步」按钮不直接发命令：面板只请求，本视图先 Prepare/持闸再派发（顺序在
        // `dev_sync_sync_once` / `dev_sync_pump` 里，见 dev_sync::sync_once 的模块文档）。
        let subscription = cx.subscribe(
            &panel,
            |shell, _panel, event: &crate::dev_sync::panel::DevSyncPanelEvent, cx| {
                // 只置标记：事件回调发生在面板实体更新期间，这里不能反过来改面板。
                // 「同步」与「发布基线」都只表达意图——先 Prepare/持闸、再派发在本视图的状态机里。
                match event {
                    crate::dev_sync::panel::DevSyncPanelEvent::SyncRequested => {
                        shell.dev_sync_requested = true;
                    }
                    crate::dev_sync::panel::DevSyncPanelEvent::PublishRequested => {
                        shell.dev_sync_publish_requested = true;
                    }
                    // 切片 6b：面板只登记请求（含恢复码/revoke 列表），这里只置标记；
                    // 持闸与派发在 `dev_sync_pump` 里（同一时刻只允许一条持闸任务）。
                    crate::dev_sync::panel::DevSyncPanelEvent::RotateRequested => {
                        shell.dev_sync_rotate_requested = true;
                    }
                    crate::dev_sync::panel::DevSyncPanelEvent::ApplyEpochRequested => {
                        shell.dev_sync_apply_requested = true;
                    }
                }
                cx.notify();
            },
        );
        // 切片 6b：把"当前有没有活动文档"这个宿主事实写给面板（轮换/apply 按钮的硬
        // 前置）。构造顺序里 `open_entry` 早于面板创建，所以这里补一次初始投影。
        let has_active_document = shell.active.is_some();
        panel.update(cx, |panel, _cx| {
            panel.set_active_document(has_active_document);
        });
        shell.dev_sync = Some(panel);
        shell.dev_sync_subscription = Some(subscription);
        // ADR 0016 S4d1c：管理面板（设置页里的「管理」区）。执行体优先用调用方注入的
        // （测试），否则只在真实 dev-local 服务（有库路径）时构造；默认构建/无路径 = None。
        let management_service = management
            .map(crate::dev_sync::ManagementService::new)
            .or_else(|| {
                let path = service.vault_path()?;
                let runner = crate::dev_sync::RealManagementRunner::new(
                    crate::dev_sync::ManagementRunConfig::from_service(service.config()),
                    management_vault,
                    path.to_path_buf(),
                );
                Some(crate::dev_sync::ManagementService::new(Box::new(runner)))
            });
        shell.management = management_service.map(|service| {
            cx.new(|cx| crate::dev_sync::ManagementPanel::with_service(service, cx))
        });
        // ADR 0012 S3：启动合并一次（只有 durable outbox/inbox 有待办才排队）。
        // 真正的派发在第一拍（生产 ticker 40ms；测试显式 `dev_sync_auto_tick`），
        // 避免在构造期重入同步状态机。
        if let Some(mut auto) = auto {
            let backlog = crate::dev_sync::autosync::durable_backlog(&shell.vault);
            let now = auto.now();
            if auto.prime_startup(backlog, now) {
                eprintln!(
                    "[menghunote] 自动同步：启动时有 durable 待办（待发送 {} / 待应用 {}），已排队",
                    backlog.outbox, backlog.inbox
                );
            }
            shell.dev_sync_auto = Some(auto);
        }
        shell.spawn_dev_sync_ticker(cx);
        shell
    }

    /// dev-only：关窗时**只请求停止 + 查一次关闭门禁**，不 join、不等网络（ADR 0009）。
    ///
    /// 在途操作还在跑时门禁是 `Pending`——这是**预期**的：worker 在后台线程里自己退出，
    /// 关窗不能被同步网络拖住。面板/测试要等明确收敛点时用 `poll_close`（票据）。
    #[cfg(feature = "dev-sync")]
    fn shutdown_dev_sync(&mut self, cx: &mut Context<Self>) {
        // ADR 0012 S3：关窗/切库先停调度器（不再自动启动），再请求 worker 停止；
        // 二者都**不** join、不等网络（S1/S2 的安全收尾语义不变）。
        if let Some(auto) = &mut self.dev_sync_auto {
            auto.stop();
        }
        // ADR 0016 S4d1c：管理 worker 同样只请求停止（在途命令跑完后退出），不 join。
        if let Some(panel) = &self.management {
            panel.read(cx).service().request_stop();
        }
        let Some(panel) = &self.dev_sync else {
            return;
        };
        let service = panel.read(cx).service();
        let ticket = service.request_stop();
        let close = service.poll_close(&ticket);
        eprintln!(
            "[menghunote] dev-sync 关窗：已请求停止，关闭门禁 = {}",
            close.label()
        );
    }

    /// 折叠/展开一个目录，返回操作后**是否处于折叠态**。
    fn toggle_folder(&mut self, node_id: &str) -> bool {
        if self.collapsed.contains(node_id) {
            self.collapsed.remove(node_id);
            false
        } else {
            self.collapsed.insert(node_id.to_owned());
            true
        }
    }

    /// 侧栏「新建笔记」：走结构写入路径 → commit → 刷新 → 直接打开新笔记。
    fn create_note(&mut self, parent: Option<String>, cx: &mut Context<Self>) {
        let title = unique_title(DEFAULT_NOTE_TITLE, &self.tree);
        // `parent = None` → 根级；`Some(目录)` → 右键菜单指定的目录内。
        let Some(seed) = self.write_structure("新建笔记", |writer| {
            let seed = writer.doc().create_note(&title, now_millis())?;
            // `create_note` 没有 parent 参数：同一次 commit 里用 move_node 挂进去
            // （分两次 commit 会让侧栏闪两次树）。
            if let Some(parent) = &parent {
                writer
                    .doc()
                    .move_node(&seed.node_id, Some(parent), now_millis())?;
            }
            Ok(seed.node_id)
        }) else {
            cx.notify();
            return;
        };
        // 父目录若被折叠，展开它，否则新笔记"看不见"。
        if let Some(parent) = &parent {
            self.collapsed.remove(parent);
        }
        eprintln!(
            "[menghunote] 新建笔记「{title}」（node={}），刷新后打开",
            seed
        );
        // 刷新保留 collapsed（refresh_tree 不改它）；选中新节点并把新笔记打开。
        self.refresh_tree();
        self.selected_node_id = Some(seed.clone());
        if let Some(index) = self.tree.iter().position(|entry| entry.node_id == seed) {
            self.open_entry(index, cx);
        } else {
            eprintln!("[menghunote] 新建的笔记没有出现在结构树里（投影滞后？）");
            self.notice = Some("新建成功，但列表里暂时看不到（稍后刷新）".to_owned());
        }
        cx.notify();
    }

    /// 侧栏「新建文件夹」：同样走结构写入，只刷新列表，不切换文档。
    fn create_folder(&mut self, parent: Option<String>, cx: &mut Context<Self>) {
        let title = unique_title(DEFAULT_FOLDER_TITLE, &self.tree);
        let Some(node_id) = self.write_structure("新建文件夹", |writer| {
            let node_id = writer.doc().create_folder(&title, now_millis())?;
            if let Some(parent) = &parent {
                writer
                    .doc()
                    .move_node(&node_id, Some(parent), now_millis())?;
            }
            Ok(node_id)
        }) else {
            cx.notify();
            return;
        };
        if let Some(parent) = &parent {
            self.collapsed.remove(parent);
        }
        eprintln!("[menghunote] 新建目录「{title}」（node={node_id}）");
        self.refresh_tree();
        self.selected_node_id = Some(node_id);
        self.notice = None;
        cx.notify();
    }

    /// 右键点击侧栏某一行（目录/笔记都弹菜单；菜单动作不触发左键打开）。
    /// 侧栏某一行的"打开菜单"入口（目前只有右键；曾另有「⋯」左键入口，已移除）。
    ///
    /// **入口先无条件打一行日志**（在任何 is_folder 判断之前）：这样"点到了笔记行"
    /// 也能从日志看出来，不必再猜是没到行、还是行里被分支吞掉。
    fn open_row_menu(
        &mut self,
        index: usize,
        source: &str,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 编辑中按右键 = 点外部 → 先确认提交，再开菜单。
        // 若标题为空（提交被拒绝、仍停在编辑态），就**不开菜单**，避免编辑与菜单并存。
        if self.editing.is_some() && !self.commit_editing_from_outside(cx) {
            return;
        }
        let handler_started = std::time::Instant::now();
        let Some(entry) = self.tree.get(index).cloned() else {
            eprintln!("[menghunote] {source} index={index} 越界（树里没这行）");
            return;
        };
        trace_menu!(
            "[menghunote] {source} node={} kind={} at=({:.1},{:.1})",
            entry.node_id,
            if entry.is_folder() { "Folder" } else { "Note" },
            position.x.as_f32(),
            position.y.as_f32()
        );
        // 目录 → 新建菜单；笔记 → 删除菜单（重命名等行内输入框做好再加）。
        let viewport = window.bounds().size;
        let panel = Size {
            width: px(MENU_WIDTH),
            height: px(menu_height(entry.is_folder())),
        };
        let origin = clamp_menu_position(position, viewport, panel);
        self.menu = Some(ContextMenu {
            node_id: entry.node_id.clone(),
            title: entry.title.clone(),
            is_folder: entry.is_folder(),
            position,
        });
        // 每次打开菜单都从"未确认"开始
        self.menu_delete_armed = false;
        self.menu_opened_at = Some(std::time::Instant::now());
        self.menu_painted.set(false);
        // 一行日志记全：点、菜单矩形、本函数耗时、相对时间戳（排查"开了看不到"用）
        trace_menu!(
            "[menghunote] 菜单打开 node={} at=({:.1},{:.1}) rect=({:.1},{:.1}) {:.0}x{:.0} 处理={:.2}ms t=+{:.1}ms",
            entry.node_id,
            position.x.as_f32(),
            position.y.as_f32(),
            origin.x.as_f32(),
            origin.y.as_f32(),
            MENU_WIDTH,
            menu_height(entry.is_folder()),
            handler_started.elapsed().as_secs_f64() * 1000.0,
            self.epoch.elapsed().as_secs_f64() * 1000.0
        );
        // 菜单要能收 `Esc`，所以把焦点给它；关闭时再还给编辑器。
        window.focus(&self.menu_focus, cx);
        cx.notify();
    }

    /// 关闭右键菜单，并把焦点还给当前编辑器（如果有）。
    fn close_menu(&mut self, reason: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_none() {
            return;
        }
        let alive_ms = self
            .menu_opened_at
            .take()
            .map_or(-1.0, |opened| opened.elapsed().as_secs_f64() * 1000.0);
        trace_menu!(
            "[menghunote] 菜单关闭 原因={reason} 存活={alive_ms:.1}ms t=+{:.1}ms",
            self.epoch.elapsed().as_secs_f64() * 1000.0
        );
        if let Some(active) = &self.active {
            active.view.update(cx, |_view, cx| cx.focus_self(window));
        }
        cx.notify();
    }

    /// 执行一个菜单动作（右键菜单里的"新建笔记/新建文件夹/删除"）。
    ///
    /// 菜单项的点击由遮罩的几何命中统一转进来（见 [`menu_hit`]），
    /// 这样不依赖元素级 `is_hovered` 的命中链。
    fn run_menu_action(
        &mut self,
        action: MenuAction,
        node_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 菜单是在 mouse-down 上执行的；菜单随即消失，同一次手势的 mouse-up 会落到
        // 下面的树行（右键重命名因此被"点外部提交"立刻结束）。记下时刻，让行点击忽略它。
        self.menu_action_at = Some(std::time::Instant::now());
        match action {
            MenuAction::Rename => {
                let title = self
                    .tree
                    .iter()
                    .find(|entry| entry.node_id == node_id)
                    .map(|entry| entry.title.clone());
                if let Some(title) = title {
                    self.close_menu("菜单项-重命名", window, cx);
                    self.pending_focus_rename = true;
                    self.begin_rename(node_id, title, cx);
                }
            }
            MenuAction::NewNote => {
                self.create_note(Some(node_id), cx);
                self.close_menu("菜单项-新建笔记", window, cx);
            }
            MenuAction::NewFolder => {
                self.create_folder(Some(node_id), cx);
                self.close_menu("菜单项-新建文件夹", window, cx);
            }
            MenuAction::Delete => {
                // 两步确认：第一次点只把菜单项变红（菜单不关），再点才真删。
                // 不设超时——菜单关闭即复位，比"几秒后自动复位"更可预期。
                // ⚠️ 注意：结构删除目前**没有撤销**（undo 只管正文），误删无法恢复；
                // 以后做 undo 或「最近删除」再补，在那之前这两步就是唯一防线。
                if !self.menu_delete_armed {
                    self.menu_delete_armed = true;
                    trace_menu!("[menghunote] 删除待确认 node={node_id}");
                    cx.notify();
                    return;
                }
                if let Some(entry) = self
                    .tree
                    .iter()
                    .find(|entry| entry.node_id == node_id)
                    .cloned()
                {
                    self.perform_delete(entry, cx);
                    self.close_menu("菜单项-删除", window, cx);
                }
            }
        }
    }

    /// 导出全部笔记为 Markdown（S3-1）。
    ///
    /// 源文来自 `BodyDoc::text()`（**不是** UI 的 display 文本）；写盘放在后台执行，
    /// 避免大库卡住 UI。完成后用侧栏提示给出导出路径与成功/失败计数。
    fn export_all(&mut self, cx: &mut Context<Self>) {
        if self.exporting {
            self.notice = Some("导出正在进行中…".to_owned());
            cx.notify();
            return;
        }
        let Some(vault_dir) = self
            .vault
            .path()
            .parent()
            .map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
        else {
            self.notice = Some("找不到库所在目录，无法导出".to_owned());
            cx.notify();
            return;
        };
        let root = vault_dir
            .join("export")
            .join(crate::export::timestamp_label(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis())
                    .unwrap_or(0),
            ));
        // 后台导出：UI 线程只负责"开始"与"完成"，写盘全在 `background_spawn` 里。
        //
        // 写法要点（task-52 实测结论）：
        // - `cx.spawn` 的 future 由**前台** executor 驱动，每帧 poll 一下就返回；
        // - `cx.background_spawn(...)` 等价于 `background_executor().spawn(...)`，
        //   它的 future 在后台线程池里跑，主线程只是在 `.await` 处挂起；
        // - 因此"导出期间 UI 卡住"**不是**这套写法的问题（详见下面的实测记录）。
        //
        // 曾经的误判：一度以为后台导出会让 UI 停止处理事件。复核发现当时**根本原因
        // 是自动化点击落点偏了**（点到侧栏树而不是底栏按钮），导出压根没启动，日志里
        // 自然什么都看不到。改用正确坐标后，2000 篇导出期间点侧栏仍能立刻切换文档
        // （`MENGHUNOTE_TRACE_MOUSE=1` 有 `mouse down` 与 `打开笔记` 日志为证）。
        let vault = self.vault.clone();
        let target = root.clone();
        self.exporting = true;
        self.notice = Some("正在导出…".to_owned());
        trace_menu!("[menghunote] 开始导出 → {}", root.display());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let started = std::time::Instant::now();
            let outcome = cx
                .background_spawn(async move {
                    std::fs::create_dir_all(&target)
                        .map_err(|error| format!("建导出目录失败 {}：{error}", target.display()))?;
                    crate::export::export_vault(&vault, &target)
                        .map_err(|error| format!("导出失败：{error}"))
                })
                .await;
            let elapsed = started.elapsed();
            this.update(cx, |shell, cx| {
                shell.exporting = false;
                match outcome {
                    Ok(report) => {
                        eprintln!(
                            "[menghunote] 导出完成：成功 {} 篇，失败 {} 条，用时 {} ms → {}",
                            report.succeeded,
                            report.failures.len(),
                            elapsed.as_millis(),
                            report.root.display()
                        );
                        for failure in report.failures.iter().take(3) {
                            eprintln!("[menghunote]   失败：{failure}");
                        }
                        let suffix = if report.failures.is_empty() {
                            String::new()
                        } else {
                            format!("，失败 {} 条", report.failures.len())
                        };
                        shell.notice = Some(format!(
                            "已导出 {} 篇到 {}{suffix}",
                            report.succeeded,
                            report.root.display()
                        ));
                    }
                    Err(message) => {
                        eprintln!("[menghunote] 导出失败：{message}");
                        shell.notice = Some(message);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 进入行内重命名：预填标题并**全选**，焦点交给输入框。
    fn begin_rename(&mut self, node_id: String, title: String, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.editing = None;
        }
        let input = cx.new(|cx| RenameInput::new(&title, cx));
        // 输入框 → 宿主：提交 / 空标题 / 取消
        // `subscribe` 的回调签名是 (this, source, event, cx)：没有 `&mut Window`，
        // 所以提交/取消时拿到的是 `Context`，`focus` 用 `cx.focus_self` 的等价路径
        // （`EditorView` 自己会在下次绘制时保持焦点）。
        cx.subscribe(
            &input,
            |shell, _input, event: &RenameEvent, cx| match event {
                RenameEvent::Commit(title) => shell.commit_rename(title.clone(), cx),
                RenameEvent::Blank => {
                    shell.notice = Some("标题不能为空".to_owned());
                    cx.notify();
                }
                RenameEvent::Cancel => shell.cancel_rename(cx),
                // 标题输入框没有“跳格”语义（只有表格单元格会发）。
                RenameEvent::Advance { .. } => {}
                // 逐键变化只给表格浮层同步草稿用，侧栏不需要。
                RenameEvent::Changed => {}
            },
        )
        .detach();
        trace_menu!("[menghunote] 开始重命名 node={node_id} 原标题={title:?}");
        // 已决定的交互：重命名默认联动改写，不提供“是否更新”选择；提前把数量告诉用户。
        let link_count = self
            .tree
            .iter()
            .find(|entry| entry.node_id == node_id)
            .and_then(|entry| entry.note_id.as_deref())
            .and_then(|note_id| list_backlinks(&self.vault, note_id).ok())
            .map_or(0, |links| links.len());
        self.notice = (link_count > 0).then(|| format!("将同时更新 {link_count} 条链接"));
        self.editing = Some(Editing { node_id, input });
        cx.notify();
    }

    /// 提交重命名：标题没变就直接退出；否则 `rename` + commit + 刷新。
    ///
    /// **笔记**重命名成功后才联动改写正文里的 wiki 链接（目录改名不做路径改写）。
    /// 结构已提交、正文改写失败时**不回滚**结构：失败篇数写进提示，其余篇不受影响。
    fn commit_rename(&mut self, title: String, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let node_id = editing.node_id;
        let entry = self
            .tree
            .iter()
            .find(|entry| entry.node_id == node_id)
            .cloned();
        let old_title = entry.as_ref().map(|entry| entry.title.clone());
        if old_title.as_deref() == Some(title.as_str()) {
            trace_menu!("[menghunote] 重命名未变化，直接退出 node={node_id}");
            self.notice = None;
            cx.notify();
            return;
        }
        let done = self
            .write_structure("重命名", |writer| {
                writer.doc().rename(&node_id, &title, now_millis())?;
                Ok(node_id.clone())
            })
            .is_some();
        if done {
            eprintln!("[menghunote] 已重命名 node={node_id} → {title:?}");
            self.refresh_tree();
            self.selected_node_id = Some(node_id);
            let note = entry.as_ref().filter(|entry| entry.is_note());
            if let Some(note) = note {
                if let Some(note_id) = note.note_id.as_deref() {
                    self.rewrite_links_after_rename(note_id, &note.title, &title, cx);
                    self.refresh_backlinks();
                    self.refresh_wiki_resolution(cx);
                }
            } else {
                self.notice = None;
            }
        }
        cx.notify();
    }

    /// 结构重命名成功后的 wiki 链接联动改写。
    ///
    /// - 非活动来源：交给 storage 逐篇 `SessionDriver`（每篇一次正文提交，互不原子）；
    /// - 当前打开的笔记：**必须**走活动 `EditorView` 的同一个 driver，另开会话会用
    ///   旧快照覆盖在途编辑；
    /// - 失败只计入 `notes_failed`，不回滚结构重命名，也不影响其它篇。
    fn rewrite_links_after_rename(
        &mut self,
        note_id: &str,
        old_title: &str,
        new_title: &str,
        cx: &mut Context<Self>,
    ) {
        let sources = match link_sources_for_note(&self.vault, note_id) {
            Ok(sources) => sources,
            Err(error) => {
                eprintln!("[menghunote] 重命名联动：读取待更新链接失败 note={note_id}：{error}");
                self.notice = Some(format!(
                    "已重命名为「{new_title}」；读取待更新链接失败（详见 stderr）"
                ));
                return;
            }
        };
        let active_note_id = self.active.as_ref().map(|active| active.note_id.clone());
        let mut totals = LinkRenameTotals::default();
        let mut deferred = Vec::new();
        for source in sources {
            if active_note_id.as_deref() == Some(source.note_id.as_str()) {
                let Some(active) = self.active.as_ref() else {
                    deferred.push(source);
                    continue;
                };
                let view = active.view.clone();
                let targets = source.targets.clone();
                let result = view.update(cx, |view, cx| {
                    let result = view.rewrite_wiki_link_targets(
                        old_title,
                        new_title,
                        &targets,
                        DEFAULT_REWRITE_TIMEOUT,
                    );
                    cx.notify();
                    result
                });
                match result {
                    Ok(0) => {}
                    Ok(links) => {
                        totals.notes_updated += 1;
                        totals.links_rewritten += links;
                    }
                    Err(message) => {
                        eprintln!(
                            "[menghunote] 重命名联动：活动笔记改写失败 note={}：{message}",
                            source.note_id
                        );
                        totals.notes_failed += 1;
                    }
                }
            } else {
                deferred.push(source);
            }
        }
        if !deferred.is_empty() {
            let options = RenameLinkOptions {
                peer_id: self.peer_id,
                sync: self.sync,
                timeout: DEFAULT_REWRITE_TIMEOUT,
            };
            let report =
                rewrite_link_sources(&self.vault, &deferred, old_title, new_title, options);
            totals.notes_updated += report.notes_updated;
            totals.links_rewritten += report.links_rewritten;
            totals.notes_failed += report.notes_failed;
            for failure in &report.failures {
                eprintln!(
                    "[menghunote] 重命名联动：来源「{}」未更新：{}",
                    failure.title, failure.message
                );
            }
        }
        eprintln!(
            "[menghunote] 重命名联动：更新 {} 篇 / {} 条链接，失败 {} 篇（note={note_id}）",
            totals.notes_updated, totals.links_rewritten, totals.notes_failed
        );
        let retry = if totals.notes_failed == 0 {
            String::new()
        } else {
            "（可重试）".to_owned()
        };
        self.notice = Some(format!(
            "已重命名为「{new_title}」；更新 {} 篇 / {} 条链接，失败 {} 篇{retry}",
            totals.notes_updated, totals.links_rewritten, totals.notes_failed
        ));
    }

    /// **点外部**（别的行 / 正文 / 右键）：按"确认提交"处理，与 `Enter` 走同一条路径。
    ///
    /// 返回 `true` 表示编辑已结束（可以继续做触发它的动作）；`false` 表示标题为空、
    /// 仍停在编辑态（此时调用方**不要**再做别的动作，避免编辑与菜单并存）。
    fn commit_editing_from_outside(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(editing) = &self.editing else {
            return true;
        };
        let title = editing.input.read(cx).text().trim().to_owned();
        if title.is_empty() {
            self.notice = Some("标题不能为空".to_owned());
            cx.notify();
            return false;
        }
        self.commit_rename(title, cx);
        true
    }

    /// 放弃重命名（不写库）。
    fn cancel_rename(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_none() {
            return;
        }
        trace_menu!("[menghunote] 取消重命名");
        self.notice = None;
        cx.notify();
    }

    /// 宿主收到编辑器的 F2 事件：重命名侧栏当前选中的节点。
    fn on_rename_requested(&mut self, cx: &mut Context<Self>) {
        let Some(node_id) = self.selected_node_id.clone() else {
            self.notice = Some("先在左侧选中一行，再按 F2".to_owned());
            cx.notify();
            return;
        };
        let Some(title) = self
            .tree
            .iter()
            .find(|entry| entry.node_id == node_id)
            .map(|entry| entry.title.clone())
        else {
            self.notice = Some("选中的节点已不在列表里".to_owned());
            cx.notify();
            return;
        };
        // 事件回调里没有 `&mut Window`：用 `window.defer` 拿窗口再聚焦输入框。
        self.pending_focus_rename = true;
        self.begin_rename(node_id, title, cx);
    }

    /// 真正执行删除：若删的是当前打开的笔记，**先排空关闭会话**（超时则中止删除）。
    fn perform_delete(&mut self, entry: TreeEntry, cx: &mut Context<Self>) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.node_id == entry.node_id)
        {
            let Some(active) = self.active.take() else {
                return;
            };
            let report = active
                .view
                .update(cx, |view, _| view.drain_and_close(SWITCH_CLOSE_TIMEOUT));
            eprintln!(
                "[menghunote] 删除前排空「{}」：durable={} pending_edits={} timed_out={}",
                active.node_id,
                report.durable.is_some(),
                report.pending_edits,
                report.timed_out
            );
            if report.timed_out {
                // 与切换同一原则：没排空就不动数据，保留现场。
                self.active = Some(active);
                self.notice = Some("保存未完成，稍后再删除".to_owned());
                eprintln!("[menghunote] 保存未完成，中止删除");
                cx.notify();
                return;
            }
        }

        // 切片 6b：删掉的若是当前打开的笔记，`active` 已经清空——面板的轮换/apply
        // 前置条件必须跟着变（否则按钮还显示可用，点了才被拒）。
        #[cfg(feature = "dev-sync")]
        self.dev_sync_push_active_document(cx);
        let deleted = self
            .write_structure("删除笔记", |writer| {
                writer.doc().delete_note(&entry.node_id, now_millis())?;
                Ok(entry.node_id.clone())
            })
            .is_some();
        if deleted {
            eprintln!(
                "[menghunote] 已删除笔记「{}」（node={}）",
                entry.title, entry.node_id
            );
            self.collapsed.remove(&entry.node_id);
            self.selected_node_id = None;
            self.notice = None;
            self.refresh_tree();
        }
        cx.notify();
    }

    /// 结构写入的公共路径：开 `StructureWriter` → 执行一次结构 op → `commit`。
    ///
    /// 失败只记日志 + 侧栏提示，**不 panic**；`commit` 之后投影表才是准的，
    /// 所以调用方紧接着要 `refresh_tree()`。
    ///
    /// dev-sync：**同步在途时冻结结构写入**（新建/重命名/删除/移动都走这里）。没有活动
    /// 文档的无会话同步也会 integrate 结构更新，此时本地再提交结构 op 会拿同一个
    /// `head_seq` 基准去 CAS（投影与 CRDT 会被撕开），所以直接拒绝并提示；同步结束后
    /// 自动恢复（不是永久禁用）。打开/滚动已有内容不受影响。
    fn write_structure(
        &mut self,
        action: &str,
        op: impl FnOnce(&StructureWriter) -> Result<String, menghunote_storage::StorageError>,
    ) -> Option<String> {
        #[cfg(feature = "dev-sync")]
        if self.dev_sync_sync.is_some() {
            eprintln!("[menghunote] {action}被拒绝：dev-sync 同步进行中（结构写入已冻结）");
            self.notice = Some("同步进行中，稍后再改结构".to_owned());
            return None;
        }
        // dev-sync：结构提交与 intent 同一个 A 事务；默认路径 sync = None（零变化）。
        let mut writer = match StructureWriter::open_with_sync(&self.vault, self.peer_id, self.sync)
        {
            Ok(writer) => writer,
            Err(error) => {
                eprintln!("[menghunote] {action}失败：打开结构写入口出错 {error:?}");
                self.notice = Some(format!("{action}失败（详见 stderr）"));
                return None;
            }
        };
        let node_id = match op(&writer) {
            Ok(node_id) => node_id,
            Err(error) => {
                eprintln!("[menghunote] {action}失败：结构 op 出错 {error:?}");
                self.notice = Some(format!("{action}失败（详见 stderr）"));
                return None;
            }
        };
        if let Err(error) = writer.commit("sidebar") {
            eprintln!("[menghunote] {action}失败：提交结构出错 {error:?}");
            self.notice = Some(format!("{action}失败（提交阶段）"));
            return None;
        }
        Some(node_id)
    }

    /// 重新读取结构树（只在启动 / 切换完成后调用）。
    fn refresh_tree(&mut self) {
        match list_tree(&self.vault) {
            Ok(tree) => {
                eprintln!(
                    "[menghunote] 侧栏树：{} 项（笔记 {} / 目录 {}）",
                    tree.len(),
                    tree.iter().filter(|entry| entry.is_note()).count(),
                    tree.iter().filter(|entry| entry.is_folder()).count()
                );
                self.tree = tree;
            }
            Err(error) => {
                eprintln!("[menghunote] 读取侧栏树失败：{error:?}");
                self.notice = Some("读取笔记树失败（详见 stderr）".to_owned());
            }
        }
    }

    /// wiki 链接点击：按“路径精确 > 标题（大小写不敏感）> 最短路径”解析并打开。
    ///
    /// 解析不到/歧义时只写提示，不自动创建笔记（L1 的明确约束）。`heading` 目前
    /// 只参与语法识别，L1 打开笔记即可；滚动到标题留给后续切片。
    fn open_wiki_link(&mut self, target: String, _heading: Option<String>, cx: &mut Context<Self>) {
        let raw = target.trim();
        let normalized = raw
            .trim_end_matches(".md")
            .trim_matches('/')
            .replace('\\', "/")
            .to_lowercase();
        if normalized.is_empty() {
            if self.active.is_none() {
                self.notice = Some("wiki 链接指向当前笔记，但当前没有打开的笔记".to_owned());
                cx.notify();
            }
            return;
        }
        let Some(index) = self.resolve_wiki_target(&normalized) else {
            self.notice = Some(format!("未找到 wiki 链接目标「{}」（不会自动创建）", raw));
            cx.notify();
            return;
        };
        self.selected_node_id = self.tree.get(index).map(|entry| entry.node_id.clone());
        self.notice = None;
        self.open_entry(index, cx);
        cx.notify();
    }

    /// 普通外链点击：只放行 http/https/mailto，其余 scheme 明确拒绝。
    fn open_external_link(&mut self, url: String, cx: &mut Context<Self>) {
        let lower = url.trim().to_ascii_lowercase();
        if !(lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("mailto:"))
        {
            self.notice = Some("只允许打开 http/https/mailto 链接".to_owned());
            cx.notify();
            return;
        }
        cx.open_url(url.trim());
        self.notice = None;
        cx.notify();
    }

    /// 结构树里某节点的完整路径（目录标题 + 笔记标题，`/` 分隔）。
    fn tree_path_for(&self, index: usize) -> String {
        let mut titles = Vec::new();
        let mut current = index;
        for _ in 0..64 {
            let Some(entry) = self.tree.get(current) else {
                break;
            };
            titles.push(entry.title.clone());
            let Some(parent) = entry.parent_id.as_deref() else {
                break;
            };
            let Some(parent_index) = self.tree.iter().position(|entry| entry.node_id == parent)
            else {
                break;
            };
            current = parent_index;
        }
        titles.reverse();
        titles.join("/")
    }

    /// 把归一化后的 target 解析成树里的笔记下标；歧义/无匹配返回 `None`。
    fn resolve_wiki_target(&self, normalized: &str) -> Option<usize> {
        let mut candidates: Vec<(u8, usize, usize)> = Vec::new();
        for (index, entry) in self.tree.iter().enumerate() {
            if !entry.is_note() {
                continue;
            }
            let path = self.tree_path_for(index);
            let path_lower = path.to_lowercase();
            let title_lower = entry.title.to_lowercase();
            let score = if path_lower == normalized {
                0
            } else if !normalized.contains('/') && title_lower == normalized {
                1
            } else if path_lower.ends_with(&format!("/{normalized}")) {
                2
            } else {
                continue;
            };
            candidates.push((score, path.chars().count(), index));
        }
        candidates.sort_unstable();
        match candidates.as_slice() {
            [] => None,
            [only] => Some(only.2),
            [best, next, ..] if best.0 == next.0 && best.1 == next.1 => None,
            [best, ..] => Some(best.2),
        }
    }

    /// 重新计算活动文档里的未解析 wiki 目标（用于灰色虚线样式）。
    fn refresh_wiki_resolution(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        let view = active.view.clone();
        let targets = view.read(cx).wiki_targets();
        let unresolved: HashSet<String> = targets
            .into_iter()
            .filter(|target| {
                if target.is_empty() {
                    self.active.is_none()
                } else {
                    self.resolve_wiki_target(target).is_none()
                }
            })
            .collect();
        view.update(cx, |view, cx| {
            view.set_unresolved_wiki_targets(unresolved, cx);
        });
    }

    /// 点击侧栏第 `index` 行。
    ///
    /// 目录不打开文档；当前笔记重复点击直接忽略。
    fn on_row_click(&mut self, index: usize, cx: &mut Context<Self>) {
        // 菜单项的 mouse-up 余波：不要让下层行把刚进入的重命名立刻提交掉。
        if self
            .menu_action_at
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(150))
        {
            return;
        }
        let Some(entry) = self.tree.get(index).cloned() else {
            return;
        };
        // 编辑中点别的行 = 点外部 → **确认提交**（不是取消）。
        if self.editing.is_some() {
            self.commit_editing_from_outside(cx);
        }
        trace_menu!(
            "[menghunote] 行点击 node={} kind={}（会 toggle/打开）",
            entry.node_id,
            if entry.is_folder() { "Folder" } else { "Note" }
        );
        // 选中与"打开"分开：目录也能被选中（新建会建在它下面）。
        self.selected_node_id = Some(entry.node_id.clone());
        self.notice = None;
        self.open_entry(index, cx);
        cx.notify();
    }

    /// 打开一篇笔记（或切换目录折叠）；选中态由 [`Self::on_row_click`] 负责。
    fn open_entry(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.tree.get(index).cloned() else {
            return;
        };
        if entry.is_folder() {
            // 目录点击 = 折叠/展开（目录没有正文）。折叠状态只在内存里，重启回到全展开。
            let now_collapsed = self.toggle_folder(&entry.node_id);
            eprintln!(
                "[menghunote] 目录「{}」→ {}（折叠 {} 个目录）",
                entry.title,
                if now_collapsed { "折叠" } else { "展开" },
                self.collapsed.len()
            );
            cx.notify();
            return;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.node_id == entry.node_id)
        {
            return;
        }
        // dev-sync：NeedGate/持闸之后禁止打开/切换文档（同步的 integrate 会直接追加外部
        // 行，新会话会拿过期 head 基准；持闸路径 moreover 握着租约）。ADR 0012 S2a：
        // **网络阶段（Prepare）不冻结切换**——此时还没有闸门、也没有 integrate。
        // 目录折叠不受影响（上面已经提前返回）：它只改内存里的展开状态。
        #[cfg(feature = "dev-sync")]
        if let Some(sync) = &self.dev_sync_sync {
            if sync.blocks_document_switch() {
                let kind = if sync.is_no_session() {
                    "无会话同步"
                } else if sync.two_phase {
                    "两阶段同步（已进入持闸阶段）"
                } else {
                    "持闸同步"
                };
                eprintln!(
                    "[menghunote] {kind}进行中：暂不打开/切换到「{}」（同步结束后再试）",
                    entry.title
                );
                self.notice = Some("同步进行中，稍后再打开/切换文档".to_owned());
                cx.notify();
                return;
            }
        }
        let Some(doc_id) = entry.body_doc_id.clone() else {
            eprintln!(
                "[menghunote] 节点「{}」没有 body_doc_id，跳过（结构投影异常？）",
                entry.title
            );
            return;
        };

        // 1) 先开新文档：失败就完全不动当前视图。
        // peer 来自身份（dev-sync）或默认常量；sync 上下文让 flush 在同事务里写 intent。
        let mut options = DriverOptions::default();
        options.session.peer_id = self.peer_id;
        options.session.sync = self.sync;
        let driver = match SessionDriver::open(&self.vault, &doc_id, options) {
            Ok(driver) => driver,
            Err(error) => {
                eprintln!("[menghunote] 打开文档 {doc_id} 失败：{error:?}");
                self.notice = Some(format!("打开「{}」失败", entry.title));
                return;
            }
        };
        let controller = match EditorController::open(driver.session().snapshot()) {
            Ok(controller) => controller,
            Err(error) => {
                eprintln!("[menghunote] 初始化编辑器模型失败（{doc_id}）：{error}");
                self.notice = Some(format!("打开「{}」失败", entry.title));
                return;
            }
        };
        // ADR 0019 S2a：图片预览要按 `asset://<hash>` 取本地字节。EditorView 不依赖
        // storage，这里注入一个只读闭包（`Vault` 是 Clone + Send + Sync，后台解码线程
        // 可以安全调用 storage 的只读 API）。
        let asset_provider = vault_asset_provider(&self.vault);
        // ADR 0019 S2b：粘贴/拖放导入的落库能力面（读字节 → 内容寻址 → 写引用 → 对账）。
        // `EditorView` 只认 trait，不认识 `Vault`；编辑器因此不反向依赖 storage 细节。
        let import_writer = crate::attachment::VaultAssetWriter::shared(&self.vault);
        let view = cx.new(|cx| {
            EditorView::new(
                controller,
                driver,
                self.vault_label.clone(),
                asset_provider,
                import_writer,
                cx,
            )
        });
        // 编辑器 → 宿主：F2 请重命名侧栏选中项；点正文则取消行内编辑。
        // 订阅挂在 `OpenNote` 上，切换文档时随之 drop。
        let events = cx.subscribe(&view, |shell, _view, event: &EditorEvent, cx| match event {
            EditorEvent::RenameRequested => shell.on_rename_requested(cx),
            EditorEvent::PointerDown => {
                // 点正文 = 点外部 → 确认提交
                if shell.editing.is_some() {
                    shell.commit_editing_from_outside(cx);
                }
            }
            EditorEvent::WikiLinkRequested { target, heading } => {
                shell.open_wiki_link(target.clone(), heading.clone(), cx);
            }
            EditorEvent::ExternalLinkRequested(url) => shell.open_external_link(url.clone(), cx),
            EditorEvent::SearchRequested => {
                // 编辑器只发事件；窗口在宿主手里（事件回调拿不到 `&mut Window`），
                // 因此置一个待打开标记，下一帧 render 里真正创建浮层。
                shell.pending_search = true;
                cx.notify();
            }
            // 粘贴/拖放导入的结果：只写提示行（正文/引用的成败已在编辑器侧处理完）。
            EditorEvent::ImportReported(report) => shell.on_import_reported(report, cx),
        });

        // 2) 再排空旧文档：超时则**中止切换**，不许静默丢编辑。
        let timed_out = if let Some(previous) = &self.active {
            let report = previous
                .view
                .update(cx, |view, _| view.drain_and_close(SWITCH_CLOSE_TIMEOUT));
            eprintln!(
                "[menghunote] 切换前排空「{}」：durable={} pending_edits={} pending_materialize={} timed_out={}",
                previous.node_id,
                report.durable.is_some(),
                report.pending_edits,
                report.pending_materialize,
                report.timed_out
            );
            report.timed_out
        } else {
            false
        };
        if timed_out {
            // 新开的视图没有写入任何东西，直接排空丢弃即可。
            view.update(cx, |view, _| {
                view.drain_and_close(SWITCH_CLOSE_TIMEOUT);
            });
            eprintln!(
                "[menghunote] 保存未完成，中止切换到「{}」；当前文档保持不变",
                entry.title
            );
            self.notice = Some("保存未完成，稍后再切换".to_owned());
            cx.notify();
            return;
        }

        eprintln!(
            "[menghunote] 打开笔记「{}」（node={} doc={}）",
            entry.title, entry.node_id, doc_id
        );
        self.active = Some(OpenNote {
            node_id: entry.node_id,
            note_id: entry.note_id.clone().expect("已打开笔记必须有 note_id"),
            view,
            _events: events,
        });
        self.refresh_backlinks();
        self.backlinks_open = false;
        // ADR 0012 S3：编辑代数按视图计（新视图从 0 开始），观测基准一起复位。
        #[cfg(feature = "dev-sync")]
        {
            self.dev_sync_auto_edit_seq = 0;
        }
        self.notice = None;
        // 3) 切换完成后刷新侧栏（投影表在 commit 之后才是准的）。
        self.refresh_tree();
        self.refresh_wiki_resolution(cx);
        // 切片 6b：活动文档变了（轮换/apply 的前置条件）。
        #[cfg(feature = "dev-sync")]
        self.dev_sync_push_active_document(cx);
        cx.notify();
    }

    /// 粘贴/拖放导入的结果 → 侧栏提示行（`import-error` / `import-notice`）。
    ///
    /// 编辑器已经完成了全部分流/事务/回滚/对账，这里只做投影：**不读剪贴板、不写库**。
    fn on_import_reported(
        &mut self,
        report: &crate::editor::clipboard::ImportReport,
        cx: &mut Context<Self>,
    ) {
        match &report.result {
            Ok(summary) => {
                self.import_error = None;
                self.import_notice = Some(format!(
                    "已{} {} 个附件（{} 字节，引用 {} 条）",
                    report.source.label(),
                    summary.items,
                    summary.byte_len,
                    summary.refs
                ));
            }
            Err(reason) => {
                self.import_notice = None;
                self.import_error = Some(format!("{}失败：{reason}", report.source.label()));
            }
        }
        cx.notify();
    }

    /// 最近一次导入的失败原因（`None` = 没有错误行；headless 断言用）。
    #[must_use]
    pub fn import_error_text(&self) -> Option<&str> {
        self.import_error.as_deref()
    }

    /// 最近一次导入的成功提示（`None` = 没有提示行；headless 断言用）。
    #[must_use]
    pub fn import_notice_text(&self) -> Option<&str> {
        self.import_notice.as_deref()
    }

    /// 把键盘焦点还给当前编辑器（设置面板关闭时调用；没有活动文档则 no-op）。
    ///
    /// 设置是 `AppRoot` 的覆盖层：它打开时焦点在 relay 输入框，关闭后不重建
    /// `AppShell`，只把焦点还回来，编辑器的文档/滚动/撤销状态原样保留。
    pub fn focus_active_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(active) = &self.active {
            active.view.update(cx, |_view, cx| cx.focus_self(window));
        }
    }

    /// 是否正在行内重命名（测试/诊断用）。
    #[must_use]
    pub fn is_renaming(&self) -> bool {
        self.editing.is_some()
    }

    /// 侧栏提示文本（测试/诊断用；`None` = 没有提示）。
    #[must_use]
    pub fn notice_text(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 当前活动文档的编辑器视图（`None` = 没有打开的笔记）。
    ///
    /// 与 `dev_sync_active_view` 同一个实体，但**不依赖 dev-sync feature**：
    /// headless 图片/列表测试用它断言编辑器内部状态。
    #[must_use]
    pub fn active_editor_view(&self) -> Option<Entity<EditorView>> {
        self.active.as_ref().map(|active| active.view.clone())
    }

    pub fn close_all(&mut self, cx: &mut Context<Self>) {
        // dev-only 同步面板：先请求停止（非阻塞，GUI 不 join）。
        #[cfg(feature = "dev-sync")]
        self.shutdown_dev_sync(cx);
        let Some(active) = self.active.take() else {
            return;
        };
        let report = active
            .view
            .update(cx, |view, _| view.drain_and_close(SWITCH_CLOSE_TIMEOUT));
        eprintln!(
            "[menghunote] 关窗前排空「{}」：durable={} pending_edits={} timed_out={}",
            active.node_id,
            report.durable.is_some(),
            report.pending_edits,
            report.timed_out
        );
    }

    /// 常驻同步状态（ADR 0012 S3）：`None` = dev-sync 未启用（整块不渲染）。
    ///
    /// 打开 AppShell 内的同步设置页（测试/程序入口；生产统一入口在 AppRoot 设置页）。
    /// 默认构建没有面板时是 no-op。
    #[cfg(feature = "dev-sync")]
    pub fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dev_sync.is_none() {
            return;
        }
        self.settings_open = true;
        self.settings_focus.focus(window, cx);
        cx.notify();
    }

    /// 测试/无窗口入口：只置位，不抢焦点。
    #[cfg(feature = "dev-sync")]
    pub fn open_settings_headless(&mut self, cx: &mut Context<Self>) {
        if self.dev_sync.is_some() {
            self.settings_open = true;
            cx.notify();
        }
    }

    /// 关闭同步设置页。
    #[cfg(feature = "dev-sync")]
    pub fn close_settings(&mut self, cx: &mut Context<Self>) {
        if self.settings_open {
            self.settings_open = false;
            cx.notify();
        }
    }

    /// 设置页是否打开（测试/诊断用）。
    #[cfg(feature = "dev-sync")]
    #[must_use]
    pub const fn settings_open(&self) -> bool {
        self.settings_open
    }

    /// 订阅系统外观变化（ADR 0021 P2）：只订阅一次，首帧先按当前 OS 外观解析。
    ///
    /// 默认构建也订阅：`ThemeMode::System` 下 OS 切换必须立即重绘。回调只改全局
    /// 主题并 `refresh_windows()`，不重建编辑器/不写正文。
    fn observe_appearance_once(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.appearance_subscription.is_some() {
            return;
        }
        crate::theme::apply_system_appearance(window.appearance(), cx);
        self.appearance_subscription =
            Some(cx.observe_window_appearance(window, |_shell, window, cx| {
                crate::theme::apply_system_appearance(window.appearance(), cx);
            }));
    }

    /// 只做投屏：状态由 [`crate::dev_sync::AutoSync`] 决定（`sync-status` / `sync-status-error`
    /// 两个固定 selector）。每轮成功不弹 toast、不抢焦点；"需要处理"才多一行。
    #[cfg(feature = "dev-sync")]
    fn dev_sync_status_projection(&self) -> Option<(String, Option<String>)> {
        #[cfg(feature = "dev-sync")]
        let projection: Option<(String, Option<String>)> = self
            .dev_sync_status_line()
            .map(|line| (line, self.dev_sync_status_error_line()));
        #[cfg(not(feature = "dev-sync"))]
        let projection: Option<(String, Option<String>)> = None;
        projection
    }

    /// 刷新当前笔记的反向链接（派生表；失败只记日志，不影响编辑）。
    fn refresh_backlinks(&mut self) {
        self.backlinks.clear();
        let Some(active) = &self.active else {
            return;
        };
        match list_backlinks(&self.vault, &active.note_id) {
            Ok(links) => self.backlinks = links,
            Err(error) => eprintln!("[menghunote] 反向链接查询失败：{error}"),
        }
    }

    /// 反向链接面板（右侧；L2 最小版：来源标题 + 点击打开）。
    fn render_backlinks_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.backlinks.iter().enumerate().map(|(index, link)| {
            div()
                .id(SharedString::from(format!("backlink-{index}")))
                .debug_selector(move || format!("backlink-{index}"))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .px_2()
                .py_1()
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|style| style.bg(theme::sidebar_selected_bg()))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme::text_color())
                        .truncate()
                        .child(SharedString::from(link.from_title.clone())),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme::status_bar_text())
                        .truncate()
                        .child(SharedString::from(format!("[[{}]]", link.target))),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |shell, _event: &gpui::MouseDownEvent, _window, cx| {
                        shell.open_backlink_note(index, cx);
                    }),
                )
        });
        div()
            .id("backlinks-panel")
            .debug_selector(|| "backlinks-panel".to_string())
            .w(px(240.0))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .border_l(px(1.0))
            .border_color(theme::sidebar_border())
            .bg(theme::sidebar_bg())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .border_b(px(1.0))
                    .border_color(theme::sidebar_border())
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(crate::ui_text::sidebar_backlinks(
                                self.backlinks.len(),
                            ))),
                    )
                    .child(
                        div()
                            .id("backlinks-close")
                            .debug_selector(|| "backlinks-close".to_string())
                            .cursor_pointer()
                            .px_1()
                            .text_size(px(12.0))
                            .text_color(theme::status_bar_text())
                            .child("×")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|shell, _event: &gpui::MouseDownEvent, _window, cx| {
                                    shell.backlinks_open = false;
                                    cx.notify();
                                }),
                            ),
                    ),
            )
            .child(
                div()
                    .id("backlinks-list")
                    .debug_selector(|| "backlinks-list".to_string())
                    .flex_1()
                    .overflow_y_scroll()
                    .py_1()
                    .when(self.backlinks.is_empty(), |this| {
                        this.child(
                            div()
                                .px_2()
                                .py_2()
                                .text_size(px(11.0))
                                .text_color(theme::status_bar_text())
                                .child(SharedString::from(
                                    crate::ui_text::sidebar_backlinks_empty(),
                                )),
                        )
                    })
                    .children(rows),
            )
            .into_any_element()
    }

    /// 反向链接点击：按 `note_id` 找到树行并打开（不按标题匹配）。
    fn open_backlink_note(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(link) = self.backlinks.get(index).cloned() else {
            return;
        };
        let Some((tree_index, _)) = self
            .tree
            .iter()
            .enumerate()
            .find(|(_, entry)| entry.note_id.as_deref() == Some(link.from_note_id.as_str()))
        else {
            self.notice = Some("来源笔记已不存在".to_owned());
            cx.notify();
            return;
        };
        self.open_entry(tree_index, cx);
    }

    /// 侧栏面板。
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let note_count = self.tree.iter().filter(|entry| entry.is_note()).count();
        let visible = visible_rows(&self.tree, &self.collapsed);
        let rows: Vec<_> = visible
            .into_iter()
            .map(|index| {
                let entry = &self.tree[index];
                // 两种状态分开：`selected` = 当前打开的笔记（蓝底），
                // `is_selected` = 侧栏选中的节点（浅底，可以是目录）。
                let selected = self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.node_id == entry.node_id);
                let is_selected = self
                    .selected_node_id
                    .as_deref()
                    .is_some_and(|node_id| node_id == entry.node_id);
                let is_folder = entry.is_folder();
                let editing_input = self
                    .editing
                    .as_ref()
                    .filter(|editing| editing.node_id == entry.node_id)
                    .map(|editing| editing.input.clone());
                let label = if is_folder {
                    // ▾ = 展开（能看到子项）、▸ = 折叠
                    let indicator = if self.collapsed.contains(&entry.node_id) {
                        "▸"
                    } else {
                        "▾"
                    };
                    format!("{indicator} {}", entry.title)
                } else {
                    entry.title.clone()
                };
                div()
                    .id(SharedString::from(format!("tree-row-{index}")))
                    // 测试元数据：`tree-row-<先序索引>`（只读断言用，见
                    // `docs/notes/ui-headless-tests.md`）。`index` 是结构树的先序下标
                    // （`visible_rows` 返回的正是这组下标），**不是**渲染行号——折叠目录后
                    // 行号会跳，树下标不会，`debug_bounds` 才能稳定查到。
                    .debug_selector(move || format!("tree-row-{index}"))
                    .group(SharedString::from("sidebar-row"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(ROW_HEIGHT))
                    .w_full()
                    .pl(px(10.0 + entry.depth as f32 * INDENT_PER_DEPTH))
                    .pr_2()
                    .text_size(px(13.0))
                    .text_color(if is_folder {
                        theme::status_bar_text()
                    } else {
                        theme::text_color()
                    })
                    .bg(if selected {
                        // 已打开 = 蓝色高亮
                        theme::selection_bg()
                    } else if is_selected {
                        // 已选中但没打开（可能是目录）= 浅色
                        theme::sidebar_selected_bg()
                    } else {
                        theme::sidebar_bg()
                    })
                    // 编辑中的行原地换成输入框；其余行照常显示标题。
                    // （曾用行右端「⋯」做菜单兜底入口，按用户要求移除，只保留右键菜单。）
                    .when_some(editing_input, |this, input| this.child(input))
                    .when(
                        self.editing.is_none() || {
                            self.editing
                                .as_ref()
                                .is_some_and(|editing| editing.node_id != entry.node_id)
                        },
                        |this| {
                            this.child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    // 选中态标记挂在这层（同一个 div 只能记一个
                                    // `debug_selector`，行本身已占用 `tree-row-N`）。
                                    .when(selected, |this| {
                                        this.debug_selector(move || {
                                            format!("tree-row-{index}-selected")
                                        })
                                    })
                                    .child(SharedString::from(label)),
                            )
                        },
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |shell, _event: &gpui::MouseDownEvent, _window, cx| {
                            shell.on_row_click(index, cx);
                        }),
                    )
                    // 右键：只对目录弹菜单，且**不**触发左键的打开/折叠。
                    //
                    // 为什么用 `on_mouse_down` 而不是 `on_mouse_up`：gpui 的
                    // `on_mouse_up` 额外要求 `hitbox.is_hovered(window)`（依赖上一次
                    // mouse move 的命中链），真实用户"右键直接抬手"时不满足 → **静默
                    // 跳过**，连日志都没有（用户真机日志就是这么验证的）。
                    // `on_mouse_down` 与左键同一条路径，稳定可达；同一次手势的 up 不会
                    // 落到遮罩上（遮罩只响应左键，且打开后 100ms 内的点击被守卫忽略）。
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |shell, event: &gpui::MouseDownEvent, window, cx| {
                            shell.open_row_menu(index, "行右键", event.position, window, cx);
                        }),
                    )
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            // 测试元数据（只读断言用）：侧栏容器。
            .debug_selector(|| "sidebar".to_string())
            .bg(theme::sidebar_bg())
            .border_r(px(1.0))
            .border_color(theme::sidebar_border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .h(px(28.0))
                    .px_2()
                    .text_size(px(12.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(crate::ui_text::sidebar_notes()))
                    .child(SharedString::from(format!("{note_count}"))),
            )
            .child(
                div()
                    .w_full()
                    .border_b(px(1.0))
                    .border_color(theme::sidebar_border())
                    .flex()
                    .flex_row()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .id("sidebar-new-note")
                            // 测试元数据：headless 测试要真实点击它验"同步中结构写入被拒"。
                            .debug_selector(|| "sidebar-new-note".to_string())
                            .flex_1()
                            .h(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from(crate::ui_text::sidebar_new_note()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|shell, _event: &gpui::MouseDownEvent, _window, cx| {
                                    shell.create_note(None, cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("sidebar-new-folder")
                            .debug_selector(|| "sidebar-new-folder".to_string())
                            .flex_1()
                            .h(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(crate::ui_text::sidebar_new_folder()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|shell, _event: &gpui::MouseDownEvent, _window, cx| {
                                    shell.create_folder(None, cx);
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("sidebar-export")
                            .flex_1()
                            .h(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(if self.exporting {
                                crate::ui_text::sidebar_exporting()
                            } else {
                                crate::ui_text::sidebar_export()
                            }))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|shell, _event: &gpui::MouseDownEvent, _window, cx| {
                                    shell.export_all(cx);
                                }),
                            ),
                    ),
            )
            .child(
                div()
                    .id("sidebar-tree")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .children(rows),
            )
            // 提示放在按钮**上方**：按钮位置固定，二次确认时不会"跑掉"，
            // 也不会把「图谱」入口往上顶（真机反馈：未解析链接提示出现后，
            // 图谱与设置之间突然多出一段偏移）。
            .when_some(self.notice.clone(), |this, notice| {
                this.child(
                    div()
                        .w_full()
                        .px_2()
                        .py_1()
                        .text_size(px(11.0))
                        .text_color(theme::caret_color())
                        .child(SharedString::from(notice)),
                )
            })
            // 粘贴/拖放导入的提示行（S2b：路径输入条已移除，粘贴/拖放是唯一入口）。
            .when_some(self.import_error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "import-error".to_string())
                        .w_full()
                        .px_2()
                        .py_1()
                        .text_size(px(11.0))
                        .text_color(theme::caret_color())
                        .child(SharedString::from(error)),
                )
            })
            .when_some(self.import_notice.clone(), |this, notice| {
                this.child(
                    div()
                        .debug_selector(|| "import-notice".to_string())
                        .w_full()
                        .px_2()
                        .py_1()
                        .text_size(px(11.0))
                        .text_color(theme::status_bar_text())
                        .child(SharedString::from(notice)),
                )
            })
            // 图谱入口：侧栏底部（「⚙ 设置」正上方，见 `GRAPH_ENTRY_BOTTOM_GAP`）。
            // 用 `on_mouse_down` 而不是 up：与侧栏其它动作行一致，真实右键/快速点击
            // 都不会因为"上一次 mouse move 的命中链"不满足而被静默跳过。
            .child(
                div().w_full().px_2().pb(px(GRAPH_ENTRY_BOTTOM_GAP)).child(
                    div()
                        .id("sidebar-graph")
                        // 测试元数据（只读断言用）：侧栏图谱入口。
                        .debug_selector(|| "sidebar-graph".to_string())
                        .h(px(22.0))
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .border(px(1.0))
                        .border_color(theme::sidebar_border())
                        .text_size(px(12.0))
                        .text_color(theme::text_color())
                        .cursor_pointer()
                        .child(SharedString::from(crate::ui_text::sidebar_graph()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|shell, _event: &MouseDownEvent, window, cx| {
                                shell.open_graph(window, cx);
                            }),
                        ),
                ),
            )
    }

    /// 右键菜单（根层绝对定位）：透明遮罩负责"点外面关闭"，面板负责菜单项。
    fn render_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let menu = self.menu.as_ref()?;
        let viewport = window.bounds().size;
        let panel = Size {
            width: px(MENU_WIDTH),
            height: px(menu_height(menu.is_folder)),
        };
        let origin = clamp_menu_position(menu.position, viewport, panel);
        if !self.menu_painted.replace(true) {
            trace_menu!(
                "[menghunote] 菜单渲染 rect=({:.1},{:.1}) {:.0}x{:.0} t=+{:.1}ms",
                origin.x.as_f32(),
                origin.y.as_f32(),
                MENU_WIDTH,
                menu_height(menu.is_folder),
                self.epoch.elapsed().as_secs_f64() * 1000.0
            );
        }
        let is_folder = menu.is_folder;

        // `slot` 是**渲染顺序**下标（0 起），只用于测试 selector。
        //
        // 为什么不用标题做 key：`VisualTestContext::debug_bounds` 只接受
        // `&'static str`，中文标题既不是 `'static` 也会随文案变；而渲染顺序在
        // 每种菜单里都是固定的（目录：重命名/新建笔记/新建文件夹；笔记：重命名/删除）。
        // 菜单项本身不挂鼠标监听：点击由遮罩的几何命中统一处理（见 `menu_hit`），
        // selector 只是给测试读矩形用。
        let item = |slot: usize, label: &'static str, color: gpui::Rgba, action: MenuAction| {
            div()
                .id(SharedString::from(format!("menu-item-{}", action.label())))
                // 测试元数据（只读断言用）：`menu-item-<渲染下标>`。
                .debug_selector(move || format!("menu-item-{slot}"))
                .flex()
                .items_center()
                .h(px(MENU_ITEM_HEIGHT))
                .w_full()
                .px_2()
                .text_size(px(12.0))
                .text_color(color)
                .child(SharedString::from(label))
        };

        Some(
            div()
                .absolute()
                .inset_0()
                // 遮罩：任意左键点击 = 关闭菜单（并吃掉这次点击，避免误触下面的行）
                .child(
                    div()
                        .id("context-menu-overlay")
                        .absolute()
                        .inset_0()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|shell, event: &gpui::MouseDownEvent, window, cx| {
                                // 命中菜单项 → 执行动作（不依赖元素级 hover 命中链）
                                let panel = Size {
                                    width: px(MENU_WIDTH),
                                    height: px(shell
                                        .menu
                                        .as_ref()
                                        .map_or(0.0, |menu| menu_height(menu.is_folder))),
                                };
                                let hit = shell.menu.as_ref().and_then(|menu| {
                                    let origin = clamp_menu_position(
                                        menu.position,
                                        window.bounds().size,
                                        panel,
                                    );
                                    menu_hit(event.position, origin, menu.is_folder)
                                        .map(|action| (action, menu.node_id.clone()))
                                });
                                if let Some((action, node_id)) = hit {
                                    shell.run_menu_action(action, node_id, window, cx);
                                    return;
                                }
                                // 刚打开的那一下（<100ms）不当作"点外部"，避免"开了立刻被关"。
                                let elapsed = shell
                                    .menu_opened_at
                                    .map_or(std::time::Duration::MAX, |opened| opened.elapsed());
                                if overlay_click_closes(elapsed) {
                                    shell.close_menu("遮罩点击", window, cx);
                                } else {
                                    trace_menu!(
                                        "[menghunote] 遮罩点击过快（{:.1}ms），忽略",
                                        elapsed.as_secs_f64() * 1000.0
                                    );
                                }
                            }),
                        ),
                )
                .child(
                    div()
                        .id("context-menu")
                        // 测试元数据（只读断言用）：右键菜单面板（遮罩不算）。
                        .debug_selector(|| "context-menu".to_string())
                        .absolute()
                        .left(origin.x)
                        .top(origin.y)
                        .w(px(MENU_WIDTH))
                        .flex()
                        .flex_col()
                        .py_1()
                        .track_focus(&self.menu_focus)
                        .on_key_down(cx.listener(
                            |shell, event: &gpui::KeyDownEvent, window, cx| {
                                if event.keystroke.key == "escape" {
                                    shell.close_menu("Esc", window, cx);
                                    window.prevent_default();
                                }
                            },
                        ))
                        .bg(theme::menu_bg())
                        .border(px(1.0))
                        .border_color(theme::sidebar_border())
                        .shadow_md()
                        .child(
                            div()
                                .px_2()
                                .pb_1()
                                .w_full()
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .text_size(px(11.0))
                                .text_color(theme::status_bar_text())
                                // 表头必须单行：长标题换行会把菜单项顶下去，
                                // 菜单高度估算也会失准（实测点空过）。
                                .text_ellipsis()
                                .child(SharedString::from(menu.title.clone())),
                        )
                        .child(item(0, "重命名", theme::text_color(), MenuAction::Rename))
                        .when(is_folder, |this| {
                            this.child(item(
                                1,
                                "新建笔记",
                                theme::text_color(),
                                MenuAction::NewNote,
                            ))
                            .child(item(
                                2,
                                "新建文件夹",
                                theme::text_color(),
                                MenuAction::NewFolder,
                            ))
                        })
                        .when(!is_folder, |this| {
                            // 两步确认：未确认显示「删除」，确认态显示「确认删除」（都是红色）
                            let label = if self.menu_delete_armed {
                                "确认删除"
                            } else {
                                "删除"
                            };
                            this.child(item(1, label, theme::danger_text(), MenuAction::Delete))
                        }),
                )
                .into_any_element(),
        )
    }

    /// 图谱覆盖层（G1/G2）：全屏 canvas 自绘（不引 WebView/JS）。
    ///
    /// 层次（后画的在上，也先收到鼠标事件）：
    ///
    /// 1. `graph-canvas` 舞台：全屏、半透明底、[`gpui::InteractiveElement::occlude`]
    ///    挡住后面编辑器的鼠标命中；**平移/缩放/悬停/点击全走
    ///    [`crate::graph::GraphViewport::hit_test`]**（与绘制同一份坐标）；
    /// 2. 边层（canvas 自绘）：视口裁剪 + LOD 细边 + 可选方向箭头 + 未解析虚线；
    /// 3. 节点层：圆点 + 标签（低缩放不画标签），位置来自同一个 `screen_pos`，
    ///    本身不挂鼠标监听（没有 hitbox 就不会挡住舞台）；视口裁剪掉画布外的节点；
    /// 4. 顶部工具条（统计行 + 局部/全库 + 深度 + 搜索 + 复位/关闭）与过滤条
    ///    （文件夹/标签/未解析/方向），自己吞掉鼠标事件——点工具条空白不等于
    ///    "点遮罩关闭"。
    fn render_graph(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let viewport = window.bounds().size;
        let state = self.graph.as_mut()?;
        // 每帧同步一次画布尺寸：命中/拖拽/测试与绘制共用这个值（窗口缩放后自动跟上）。
        state.viewport = viewport;
        let model = &state.model;
        let view = state.view;
        let hovered = state.hovered;
        let lod = GraphLod::for_zoom(view.zoom);
        let show_arrows = state.options.show_arrows && lod.show_arrows;
        let hint = hint_line(model);
        let empty = model.is_empty();
        let radius = view.node_radius();
        let ghost_radius = radius * GHOST_RADIUS_SCALE;
        let matches: HashSet<usize> = state.search.matches.iter().copied().collect();
        let selected_match = state.search.selected_node();
        let neighbors: HashSet<usize> = hovered
            .map(|index| model.neighbor_indices_of(index).into_iter().collect())
            .unwrap_or_default();

        // 边：先做视口裁剪，画布外的边不产生图元。
        let mut edges: Vec<(Point<Pixels>, Point<Pixels>, bool)> = Vec::new();
        for edge in model.edges() {
            let (Some(from), Some(to)) = (model.nodes().get(edge.from), model.nodes().get(edge.to))
            else {
                continue;
            };
            let a = view.screen_pos(from.pos, viewport);
            let b = view.screen_pos(to.pos, viewport);
            if edge_offscreen(a, b, viewport) {
                continue;
            }
            let active = hovered.is_some_and(|index| edge.from == index || edge.to == index);
            edges.push((a, b, active));
        }

        // 方向箭头：只在开关打开且缩放够大（LOD）时画。
        let mut arrows: Vec<[Point<Pixels>; 3]> = Vec::new();
        if show_arrows {
            for edge in model.edges() {
                let (Some(from), Some(to)) =
                    (model.nodes().get(edge.from), model.nodes().get(edge.to))
                else {
                    continue;
                };
                let a = view.screen_pos(from.pos, viewport);
                let b = view.screen_pos(to.pos, viewport);
                if edge_offscreen(a, b, viewport) {
                    continue;
                }
                if let Some(head) = arrow_head(a, b, radius + 2.0) {
                    arrows.push(head);
                }
            }
        }

        // 未解析占位：虚线边 + 灰色虚线小圆（不是正式节点、不可打开）。
        let mut ghost_edges: Vec<(Point<Pixels>, Point<Pixels>)> = Vec::new();
        let mut ghost_layer = div().absolute().inset_0().overflow_hidden();
        for (index, ghost) in model.ghosts().iter().enumerate() {
            let Some(source) = model.nodes().get(ghost.from) else {
                continue;
            };
            let from = view.screen_pos(source.pos, viewport);
            let center = view.screen_pos(ghost.pos, viewport);
            if node_offscreen(center, viewport) {
                continue;
            }
            ghost_edges.push((from, center));
            let mut ghost_box = div()
                .absolute()
                .left(px(center.x.as_f32() - GRAPH_NODE_BOX_WIDTH * 0.5))
                .top(px(center.y.as_f32() - ghost_radius))
                .w(px(GRAPH_NODE_BOX_WIDTH))
                .flex()
                .flex_col()
                .items_center()
                .child(
                    div()
                        // 测试元数据（只读断言用）：`graph-ghost-<占位下标>`。
                        .debug_selector(move || format!("graph-ghost-{index}"))
                        .w(px(ghost_radius * 2.0))
                        .h(px(ghost_radius * 2.0))
                        .rounded_full()
                        .border(px(1.0))
                        .border_dashed()
                        .border_color(theme::graph_node_dim_border())
                        .bg(theme::graph_node_dim_bg()),
                );
            if lod.show_labels {
                ghost_box = ghost_box.child(
                    div()
                        .w(px(GRAPH_NODE_BOX_WIDTH))
                        .mt(px(2.0))
                        .text_size(px(11.0))
                        .text_color(theme::status_bar_text())
                        .text_center()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(SharedString::from(ghost.target.clone())),
                );
            }
            ghost_layer = ghost_layer.child(ghost_box);
        }

        // 节点层：圆点位置 = `screen_pos`（命中/绘制/测试共用），标签固定宽度单行省略。
        let mut layer = div().absolute().inset_0().overflow_hidden();
        for (index, node) in model.nodes().iter().enumerate() {
            let center = view.screen_pos(node.pos, viewport);
            // 视口裁剪：远离画布的节点不生成图元（大库/远平移省开销）。
            if node_offscreen(center, viewport) {
                continue;
            }
            let active = hovered == Some(index);
            let related = active || (hovered.is_some() && neighbors.contains(&index));
            let matched = matches.contains(&index);
            let selected = selected_match == Some(index);
            // 悬停时压暗无关节点；搜索匹配不压暗（否则高亮看不出来）。
            let dimmed = hovered.is_some() && !related && !matched;
            let (fill, border, text) = if selected || active {
                (
                    theme::graph_node_active_bg(),
                    theme::graph_node_active_bg(),
                    theme::graph_node_active_text(),
                )
            } else if matched {
                (
                    theme::graph_node_bg(),
                    theme::graph_edge_active(),
                    theme::graph_label_text(),
                )
            } else if dimmed {
                (
                    theme::graph_node_dim_bg(),
                    theme::graph_node_dim_border(),
                    theme::status_bar_text(),
                )
            } else if related {
                (
                    theme::graph_node_bg(),
                    theme::graph_edge_active(),
                    theme::graph_label_text(),
                )
            } else {
                (
                    theme::graph_node_bg(),
                    theme::graph_node_border(),
                    theme::graph_label_text(),
                )
            };
            let selector_index = index;
            let selector_matched = matched || selected;
            let mut node_box = div()
                .absolute()
                .left(px(center.x.as_f32() - GRAPH_NODE_BOX_WIDTH * 0.5))
                .top(px(center.y.as_f32() - radius))
                .w(px(GRAPH_NODE_BOX_WIDTH))
                .flex()
                .flex_col()
                .items_center()
                .child(
                    div()
                        // 测试元数据（只读断言用）：`graph-node-<模型下标>`；
                        // 搜索匹配时换成 `graph-node-<i>-match`（高亮态）。
                        .debug_selector(move || {
                            if selector_matched {
                                format!("graph-node-{selector_index}-match")
                            } else {
                                format!("graph-node-{selector_index}")
                            }
                        })
                        .w(px(radius * 2.0))
                        .h(px(radius * 2.0))
                        .rounded_full()
                        .bg(fill)
                        .border(px(1.0))
                        .border_color(border),
                );
            if lod.show_labels {
                node_box = node_box.child(
                    div()
                        .w(px(GRAPH_NODE_BOX_WIDTH))
                        .mt(px(2.0))
                        .text_size(px(11.0))
                        .text_color(text)
                        .text_center()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(SharedString::from(node.title.clone())),
                );
            }
            layer = layer.child(node_box);
        }

        let edge_width = lod.edge_width;
        let paint_edges = std::rc::Rc::new(edges);
        let paint_arrows = std::rc::Rc::new(arrows);
        let paint_ghost_edges = std::rc::Rc::new(ghost_edges);
        let paint_dim = hovered.is_some();

        let stage = div()
            .id("graph-canvas")
            // 测试元数据（只读断言用）：舞台（全窗口）+ 尺寸来源。
            .debug_selector(|| "graph-canvas".to_string())
            .absolute()
            .inset_0()
            .bg(theme::graph_backdrop())
            // 覆盖层语义：不挤压编辑器，也不把点击漏给下面的编辑器。
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|shell, event: &MouseDownEvent, _window, cx| {
                    shell.graph_mouse_down(event.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|shell, event: &MouseMoveEvent, _window, cx| {
                shell.graph_mouse_move(event.position, event.pressed_button, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|shell, _event: &MouseUpEvent, window, cx| {
                    shell.graph_mouse_up(window, cx);
                }),
            )
            .on_scroll(
                cx.listener(|shell, event: &gpui::ScrollEvent, _window, cx| {
                    shell.graph_scroll(event.position, event.delta, cx);
                    // **阻断穿透**：图谱覆盖层是模态视图，滚轮只缩放图谱；
                    // 底层 EditorView 的窗口级滚动监听不得收到这次事件。
                    cx.stop_propagation();
                }),
            )
            .child(
                canvas(
                    move |bounds, _window, _cx| bounds,
                    move |_bounds, _prepaint, window, _cx| {
                        for (a, b, active) in paint_edges.iter() {
                            let color = if *active {
                                theme::graph_edge_active()
                            } else if paint_dim {
                                theme::graph_edge_dim()
                            } else {
                                theme::graph_edge()
                            };
                            let width = if *active { 1.8 } else { edge_width };
                            let mut builder = gpui::PathBuilder::stroke(px(width));
                            builder.move_to(*a);
                            builder.line_to(*b);
                            if let Ok(path) = builder.build() {
                                window.paint_path(path, color);
                            }
                        }
                        // 未解析占位：虚线（低缩放也画，否则看不到"有未解析"）。
                        for (a, b) in paint_ghost_edges.iter() {
                            let mut builder =
                                gpui::PathBuilder::stroke(px(1.0)).dash_array(&[px(4.0), px(3.0)]);
                            builder.move_to(*a);
                            builder.line_to(*b);
                            if let Ok(path) = builder.build() {
                                window.paint_path(path, theme::graph_edge_dim());
                            }
                        }
                        // 方向箭头：两条翼（V 形）。
                        for head in paint_arrows.iter() {
                            let [tip, left, right] = *head;
                            let mut builder = gpui::PathBuilder::stroke(px(1.2));
                            builder.move_to(tip);
                            builder.line_to(left);
                            builder.move_to(tip);
                            builder.line_to(right);
                            if let Ok(path) = builder.build() {
                                window.paint_path(path, theme::graph_edge_active());
                            }
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(ghost_layer)
            .child(layer);

        let search_input = state.search_input.clone();
        let scope = state.scope.clone();
        let options = state.options.clone();
        let folders = state.folders.clone();
        let tags = state.tags.clone();

        // ---- 顶部工具条：统计 + 局部/全库 + 深度 + 搜索 + 复位/关闭 ----
        let mut header = div()
            .id("graph-header")
            .debug_selector(|| "graph-header".to_string())
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .h(px(GRAPH_HEADER_HEIGHT))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .overflow_hidden()
            .bg(theme::graph_toolbar_bg())
            .border_b(px(1.0))
            .border_color(theme::sidebar_border())
            // 工具条自己吞掉鼠标：点标题/点空白既不算"点遮罩关闭"，也不开始拖拽。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_shell, _event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|_shell, _event: &MouseUpEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from("图谱")),
                    )
                    .child(
                        div()
                            // 测试元数据（只读断言用）：节点/边/未解析/截断统计行。
                            .debug_selector(|| "graph-hint".to_string())
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(11.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from(hint.clone())),
                    ),
            );
        // 局部/全库（G2：默认局部，可切回全库）。
        header = header.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .child(
                    graph_chip(
                        "graph-scope-local".to_owned(),
                        "局部".to_owned(),
                        scope.is_local(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|shell, _event: &MouseDownEvent, _window, cx| {
                            shell.graph_set_scope(true, cx);
                            cx.stop_propagation();
                        }),
                    ),
                )
                .child(
                    graph_chip(
                        "graph-scope-full".to_owned(),
                        "全库".to_owned(),
                        !scope.is_local(),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|shell, _event: &MouseDownEvent, _window, cx| {
                            shell.graph_set_scope(false, cx);
                            cx.stop_propagation();
                        }),
                    ),
                ),
        );
        // 深度 1–3（只在局部图下显示；全库图没有"深度"概念）。
        if scope.is_local() {
            let mut depth_group = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_shrink_0();
            for depth in MIN_LOCAL_DEPTH..=MAX_LOCAL_DEPTH {
                depth_group = depth_group.child(
                    graph_chip(
                        format!("graph-depth-{depth}"),
                        depth.to_string(),
                        scope.clamped_depth() == depth,
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |shell, _event: &MouseDownEvent, _window, cx| {
                            shell.graph_set_depth(depth, cx);
                            cx.stop_propagation();
                        }),
                    ),
                );
            }
            header = header.child(depth_group);
        }
        header = header
            .child(
                div()
                    .id("graph-search")
                    // 测试元数据（只读断言用）：搜索框容器。
                    .debug_selector(|| "graph-search".to_string())
                    .w(px(GRAPH_SEARCH_WIDTH))
                    .h(px(28.0))
                    .flex_shrink_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_shell, _event: &MouseDownEvent, _window, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .child(search_input),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(graph_button("graph-reset", "复位", false).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|shell, _event: &MouseDownEvent, _window, cx| {
                            shell.graph_reset(cx);
                            cx.stop_propagation();
                        }),
                    ))
                    .child(graph_button("graph-close", "关闭", true).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|shell, _event: &MouseDownEvent, window, cx| {
                            shell.close_graph(window, cx);
                            cx.stop_propagation();
                        }),
                    )),
            );

        // ---- 过滤条：文件夹 / 标签 / 未解析 / 方向 ----
        let mut filter_bar = div()
            .id("graph-filters")
            .debug_selector(|| "graph-filters".to_string())
            .absolute()
            .top(px(GRAPH_HEADER_HEIGHT))
            .left(px(0.0))
            .right(px(0.0))
            .h(px(GRAPH_FILTER_HEIGHT))
            .px_3()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .overflow_hidden()
            .bg(theme::graph_toolbar_bg())
            .border_b(px(1.0))
            .border_color(theme::sidebar_border())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_shell, _event: &MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|_shell, _event: &MouseUpEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .flex_shrink_0()
                    .child(SharedString::from("文件夹")),
            )
            .child(
                graph_chip(
                    "graph-folder-all".to_owned(),
                    "全部".to_owned(),
                    options.folder.is_none(),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|shell, _event: &MouseDownEvent, _window, cx| {
                        shell.graph_set_folder(None, cx);
                        cx.stop_propagation();
                    }),
                ),
            );
        for (index, folder) in folders.iter().take(MAX_FOLDER_CHIPS).enumerate() {
            let selected = options.folder.as_deref() == Some(folder.as_str());
            let value = folder.clone();
            filter_bar = filter_bar.child(
                graph_chip(format!("graph-folder-{index}"), folder.clone(), selected)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |shell, _event: &MouseDownEvent, _window, cx| {
                            shell.graph_set_folder(Some(value.clone()), cx);
                            cx.stop_propagation();
                        }),
                    ),
            );
        }
        if folders.len() > MAX_FOLDER_CHIPS {
            filter_bar = filter_bar.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .flex_shrink_0()
                    .child(SharedString::from(format!(
                        "+{}",
                        folders.len() - MAX_FOLDER_CHIPS
                    ))),
            );
        }
        filter_bar = filter_bar
            .child(
                div()
                    .w(px(1.0))
                    .h(px(16.0))
                    .flex_shrink_0()
                    .bg(theme::sidebar_border()),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::status_bar_text())
                    .flex_shrink_0()
                    .child(SharedString::from("标签")),
            );
        if tags.is_empty() {
            // 当前 app 没有打标签入口（schema 有 tags/note_tags，projector 会写），
            // 列表通常为空——空时明确提示，不留空白。
            filter_bar = filter_bar.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::graph_edge_dim())
                    .flex_shrink_0()
                    .child(SharedString::from("无标签")),
            );
        } else {
            filter_bar = filter_bar.child(
                graph_chip(
                    "graph-tag-all".to_owned(),
                    "全部".to_owned(),
                    options.tag.is_none(),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|shell, _event: &MouseDownEvent, _window, cx| {
                        shell.graph_set_tag(None, cx);
                        cx.stop_propagation();
                    }),
                ),
            );
            for (index, tag) in tags.iter().take(MAX_TAG_CHIPS).enumerate() {
                let selected = options.tag.as_deref() == Some(tag.as_str());
                let value = tag.clone();
                filter_bar = filter_bar.child(
                    graph_chip(format!("graph-tag-{index}"), tag.clone(), selected).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |shell, _event: &MouseDownEvent, _window, cx| {
                            shell.graph_set_tag(Some(value.clone()), cx);
                            cx.stop_propagation();
                        }),
                    ),
                );
            }
        }
        filter_bar = filter_bar
            .child(
                div()
                    .w(px(1.0))
                    .h(px(16.0))
                    .flex_shrink_0()
                    .bg(theme::sidebar_border()),
            )
            .child(
                graph_chip(
                    "graph-toggle-unresolved".to_owned(),
                    "未解析".to_owned(),
                    options.show_unresolved,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|shell, _event: &MouseDownEvent, _window, cx| {
                        shell.graph_toggle_unresolved(cx);
                        cx.stop_propagation();
                    }),
                ),
            )
            .child(
                graph_chip(
                    "graph-toggle-arrows".to_owned(),
                    "方向".to_owned(),
                    options.show_arrows,
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|shell, _event: &MouseDownEvent, _window, cx| {
                        shell.graph_toggle_arrows(cx);
                        cx.stop_propagation();
                    }),
                ),
            );

        let empty_state = div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    // 测试元数据（只读断言用）：空图占位（不画节点、不 panic）。
                    .debug_selector(|| "graph-empty".to_string())
                    .text_size(px(13.0))
                    .text_color(theme::status_bar_text())
                    .child(SharedString::from(hint)),
            );

        Some(
            div()
                .id("graph-overlay")
                // 测试元数据（只读断言用）：覆盖层存在 = 打开；`debug_bounds` 为 `None` = 关闭。
                .debug_selector(|| "graph-overlay".to_string())
                .absolute()
                .inset_0()
                .track_focus(&self.graph_focus)
                .on_key_down(
                    cx.listener(|shell, event: &gpui::KeyDownEvent, window, cx| {
                        // 输入法组合态的 Esc 归输入框（只取消组合），不能关覆盖层。
                        let composing = shell
                            .graph
                            .as_ref()
                            .is_some_and(|state| state.search_input.read(cx).is_composing());
                        match event.keystroke.key.as_str() {
                            "escape" if !composing => {
                                shell.close_graph(window, cx);
                                cx.stop_propagation();
                                window.prevent_default();
                            }
                            "up" if !composing => {
                                shell.graph_search_move(false, cx);
                                cx.stop_propagation();
                                window.prevent_default();
                            }
                            "down" if !composing => {
                                shell.graph_search_move(true, cx);
                                cx.stop_propagation();
                                window.prevent_default();
                            }
                            _ => {}
                        }
                    }),
                )
                .child(stage)
                .child(header)
                .child(filter_bar)
                .when(empty, |this| this.child(empty_state))
                .into_any_element(),
        )
    }

    /// 右侧：当前编辑器；没有笔记时给占位。
    /// 底部状态栏：左侧 vault；右侧同步状态（只读投影）。设置入口统一在侧栏底部。
    fn render_status_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let left = div()
            .min_w(px(0.0))
            .truncate()
            .text_size(px(11.0))
            .text_color(theme::status_bar_text())
            .child(SharedString::from(self.vault_label.clone()));
        let mut right = div().flex().flex_row().items_center().gap_2();
        if self.active.is_some() {
            right = right.child(
                div()
                    .id("backlinks-toggle")
                    .debug_selector(|| "backlinks-toggle".to_string())
                    .flex()
                    .flex_row()
                    .items_center()
                    .h_full()
                    .px_1()
                    .cursor_pointer()
                    .text_color(if self.backlinks_open {
                        theme::text_color()
                    } else {
                        theme::status_bar_text()
                    })
                    .child(SharedString::from(crate::ui_text::status_backlinks(
                        self.backlinks.len(),
                    )))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|shell, _event: &gpui::MouseDownEvent, _window, cx| {
                            shell.refresh_backlinks();
                            shell.backlinks_open = !shell.backlinks_open;
                            cx.notify();
                        }),
                    ),
            );
        }
        #[cfg(feature = "dev-sync")]
        {
            // dev-sync 未启用时整块不存在。
            right = right.when_some(self.dev_sync_status_projection(), |this, (line, error)| {
                this.child(
                    div()
                        .id("sync-status-button")
                        .debug_selector(|| "sync-status".to_string())
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .h_full()
                        .px_1()
                        .when(error.is_some(), |this| {
                            this.text_color(theme::danger_text())
                        })
                        .child(SharedString::from(if error.is_some() {
                            crate::ui_text::status_sync_attention()
                        } else {
                            crate::ui_text::status_sync(&line)
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|shell, _event: &gpui::MouseDownEvent, window, cx| {
                                shell.open_settings(window, cx);
                            }),
                        ),
                )
            });
        }
        div()
            .id("status-bar")
            .debug_selector(|| "status-bar".to_string())
            .h(px(24.0))
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .px_2()
            .border_t(px(1.0))
            .border_color(theme::sidebar_border())
            .bg(theme::status_bar_bg())
            .text_size(px(11.0))
            .text_color(theme::status_bar_text())
            .child(left)
            .child(right)
            .into_any_element()
    }

    /// 设置页里的「管理」区（ADR 0016 S4d1c）。
    ///
    /// 默认收起；展开时挂载面板并触发一次刷新。没有管理面板时整块不渲染（默认构建/
    /// 注入 fake service 且没有真实库路径的测试）。
    #[cfg(feature = "dev-sync")]
    fn render_management_section(&self, cx: &mut Context<Self>) -> AnyElement {
        if !crate::dev_sync::experimental_v2_ui() {
            return div().into_any_element();
        }
        let Some(panel) = self.management.clone() else {
            return div().into_any_element();
        };
        let open = self.management_open;
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .pb_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from("管理（V2 控制面 / 通道 A 双签）")),
                    )
                    .child(
                        div()
                            .id("settings-toggle-management")
                            .debug_selector(|| "settings-toggle-management".to_string())
                            .px(px(10.0))
                            .py(px(5.0))
                            .rounded_sm()
                            .border_1()
                            .border_color(theme::input_border())
                            .bg(theme::menu_bg())
                            .text_size(px(12.0))
                            .text_color(theme::text_color())
                            .cursor_pointer()
                            .child(SharedString::from(if open { "收起" } else { "展开" }))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|shell, _event: &gpui::MouseDownEvent, _window, cx| {
                                    shell.toggle_management(cx);
                                    cx.stop_propagation();
                                }),
                            ),
                    ),
            )
            .when(open, |this| this.child(div().w_full().child(panel)))
            .into_any_element()
    }

    /// 同步设置页：遮罩 + 居中卡片；面板实体只在打开时挂载。
    #[cfg(feature = "dev-sync")]
    fn render_settings(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.settings_open {
            return None;
        }
        let panel = self.dev_sync.clone()?;
        let card = div()
            .id("settings-panel")
            .debug_selector(|| "settings-panel".to_string())
            .relative()
            .w(px(540.0))
            .max_h(px(760.0))
            .overflow_y_scroll()
            .rounded_md()
            .border_1()
            .border_color(theme::sidebar_border())
            .bg(theme::sidebar_bg())
            .shadow_lg()
            .track_focus(&self.settings_focus)
            .on_key_down(
                cx.listener(|shell, event: &gpui::KeyDownEvent, _window, cx| {
                    if event.keystroke.key.as_str() == "escape" {
                        cx.stop_propagation();
                        shell.close_settings(cx);
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_shell, _event: &gpui::MouseDownEvent, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b(px(1.0))
                    .border_color(theme::sidebar_border())
                    .child(
                        div()
                            .text_size(px(14.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from(crate::ui_text::settings_sync_title())),
                    )
                    .child(
                        div()
                            .id("settings-close")
                            .debug_selector(|| "settings-close".to_string())
                            .px_2()
                            .rounded_sm()
                            .hover(|style| style.bg(theme::sidebar_selected_bg()))
                            .child(SharedString::from(crate::ui_text::settings_close()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|shell, _event: &gpui::MouseDownEvent, _window, cx| {
                                    shell.close_settings(cx);
                                }),
                            ),
                    ),
            )
            .child(div().p_2().child(panel))
            .child(self.render_management_section(cx))
            // ADR 0021 P2：外观（浅色/深色/跟随系统）。放在最下面，避免把既有
            // 同步/设备/轮换按钮顶出设置页可视区（headless 真机都按可见 bounds 点击）。
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme::sidebar_border())
                    .child(crate::language_settings::appearance_and_language_row()),
            );
        Some(
            div()
                .id("settings-overlay")
                .debug_selector(|| "settings-overlay".to_string())
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(24.0))
                .bg(theme::overlay_scrim_soft())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|shell, _event: &gpui::MouseDownEvent, _window, cx| {
                        shell.close_settings(cx);
                    }),
                )
                .child(card)
                .into_any_element(),
        )
    }

    fn render_main(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = self.active.as_ref().map(|active| active.view.clone());
        let editor = div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_w(px(0.0))
            // 测试元数据（只读断言用）：右侧编辑器面板（空态也存在）。
            .debug_selector(|| "editor-pane".to_string())
            .when_some(view, |this, view| this.child(view))
            .when(self.active.is_none(), |this| {
                this.child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0))
                        .text_color(theme::status_bar_text())
                        .child(SharedString::from("没有打开的笔记（从左侧选一篇）")),
                )
            });
        div()
            .flex()
            .flex_row()
            .flex_1()
            .h_full()
            .min_w(px(0.0))
            .child(editor)
            .when(self.backlinks_open && self.active.is_some(), |this| {
                this.child(self.render_backlinks_panel(cx))
            })
    }
}

/// dev-sync（ADR 0009 切片 3b）：对**当前活动文档**转发外部提交闸门 / adopt 的最小接口。
///
/// 只做转发，不做网络/push（那是切片 4 的 worker）；所有语义（Held 冻结、adopt 幂等、
/// 失败不释放闸门）都在 `storage::session` 的 3a 实现里。为什么放在 `AppShell`：
/// 活动文档的 `SessionDriver` 属于 `EditorView`，而面板/worker/测试只认识 shell。
///
/// 没有活动文档时（还没打开任何笔记）：
/// - `begin` → [`GateError::Storage`]（没有会话就没有闸门，明确失败而不是静默 no-op）；
/// - `poll` → [`GateState::Stale`]（"票据从未登记"的既有语义）；
/// - `adopt` → [`AdoptError::Storage`]；`end`/`cancel` → [`GateError::Storage`]。
#[cfg(feature = "dev-sync")]
impl AppShell {
    /// 当前活动文档的编辑器视图（`None` = 没有打开的笔记）。
    ///
    /// 测试用它断言"adopt 之后**没有重开**文档"：同一个实体 = 没有重建视图。
    #[must_use]
    pub fn dev_sync_active_view(&self) -> Option<Entity<EditorView>> {
        self.active.as_ref().map(|active| active.view.clone())
    }

    /// 侧栏底部的临时提示（测试断言"同步中结构写入被拒"的文案）。
    ///
    /// 只读投影：`write_structure` 在同步在途时会把"同步进行中，稍后再改结构"写进它。
    #[must_use]
    pub fn dev_sync_sidebar_notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// 对当前活动文档登记一次外部提交。
    pub fn dev_sync_begin_external(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Result<ExternalCommitTicket, GateError> {
        self.update_active_view(cx, |view, cx| view.dev_sync_begin_external(cx))
            .unwrap_or_else(|| Err(GateError::Storage(NO_ACTIVE_DOC.to_owned())))
    }

    /// 非阻塞推进闸门：`WaitingIME` → `Flushing` → `Held(lease)`。
    pub fn dev_sync_poll_external(
        &mut self,
        ticket: &ExternalCommitTicket,
        cx: &mut Context<Self>,
    ) -> GateState {
        self.update_active_view(cx, |view, cx| view.dev_sync_poll_external(ticket, cx))
            .unwrap_or(GateState::Stale)
    }

    /// adopt 一份**已 durable** 的外部增量（必须 Held）。
    pub fn dev_sync_adopt_external(
        &mut self,
        lease: &ExternalCommitLease,
        prepared: PreparedCommittedDelta,
        cx: &mut Context<Self>,
    ) -> Result<AdoptOutcome, AdoptError> {
        self.update_active_view(cx, |view, cx| {
            view.dev_sync_adopt_external(lease, prepared, cx)
        })
        .unwrap_or_else(|| Err(AdoptError::Storage(NO_ACTIVE_DOC.to_owned())))
    }

    /// 结束外部提交并释放闸门（`applied` 必须就是库中当前 head）。
    pub fn dev_sync_end_external(
        &mut self,
        lease: ExternalCommitLease,
        applied: AppliedHeads,
        cx: &mut Context<Self>,
    ) -> Result<(), GateError> {
        self.update_active_view(cx, |view, cx| {
            view.dev_sync_end_external(lease, applied, cx)
        })
        .unwrap_or_else(|| Err(GateError::Storage(NO_ACTIVE_DOC.to_owned())))
    }

    /// 取消/放弃一次外部提交（失败路径；已 durable 的交付仍由驱动事件推送）。
    pub fn dev_sync_cancel_external(
        &mut self,
        ticket: &ExternalCommitTicket,
        cx: &mut Context<Self>,
    ) -> Result<(), GateError> {
        self.update_active_view(cx, |view, cx| view.dev_sync_cancel_external(ticket, cx))
            .unwrap_or_else(|| Err(GateError::Storage(NO_ACTIVE_DOC.to_owned())))
    }

    /// 泵一次 dev-sync（面板请求 → 持闸同步状态机 → 活动视图的 `Remote`）。
    ///
    /// 生产路径由本视图的 ticker 每 40 ms 调一次（`spawn_dev_sync_ticker`）；headless
    /// 测试显式调它（没有真实时钟），因此它是"单步收敛点"，**不是** sleep 的替代。
    ///
    /// 顺序：
    /// 1. 消费「同步」按钮请求（`dev_sync_sync_once`）；
    /// 2. 推进持闸同步（等 Held / 等 worker / 交付 + 释放）；
    /// 3. 把活动驱动的队列推进 `EditorView`（adopt 出来的 `Remote` → 镜像正文）。
    pub fn dev_sync_pump(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        // 0) 面板/服务事件泵：把 worker 线程发来的 Progress/Completed 收进 `SyncState`
        //    （`snapshot().last_completed` 是下面等待终态的唯一观察点，必须有人 poll）。
        //    面板自己也有 40 ms 泵；这里是给测试与非阻塞主循环的确定性入口（重复 poll 无副作用）。
        if let Some(panel) = self.dev_sync.clone() {
            changed |= panel.update(cx, |panel, cx| {
                let pumped = panel.pump();
                if pumped {
                    cx.notify();
                }
                pumped
            });
        }
        // 0b) 切片 6b：把"有没有活动文档"这个宿主事实同步给面板（轮换/apply 的硬前置）。
        //     每拍都写一次是幂等的（`set_active_document` 不变就不通知），不引入轮询。
        self.dev_sync_push_active_document(cx);
        // 1) 面板只发请求：真正的"先持闸、再派发"在这里（见 dev_sync::sync_once）。
        if std::mem::take(&mut self.dev_sync_requested) {
            // 失败原因已经写进面板/报告；这里只关心"有没有变化"。
            let _ = self.dev_sync_sync_once(cx);
            changed = true;
        }
        if std::mem::take(&mut self.dev_sync_publish_requested) {
            // 同一套持闸状态机，只换 worker 命令与报告种类（切片 4c-4）。
            let _ = self.dev_sync_publish_baseline(cx);
            changed = true;
        }
        // 1b) 切片 6b：轮换 / 应用新 epoch 的确认请求。面板把恢复码/revoke 列表留在
        //     自己的 `pending_rotate_request` 里（事件不带秘密），这里取走并派发。
        if std::mem::take(&mut self.dev_sync_rotate_requested) {
            let request = self
                .dev_sync
                .clone()
                .and_then(|panel| panel.update(cx, |panel, _cx| panel.take_rotate_request()));
            match request {
                Some(request) => {
                    // 拒绝原因由 `dev_sync_start_gated` 写进面板（不吞、不 panic）。
                    let _ = self.dev_sync_start_gated(
                        crate::dev_sync::GatedTask::Rotate,
                        Some(request),
                        cx,
                    );
                    changed = true;
                }
                None => {
                    // 面板没有登记请求（重复事件/已被消费）：绝不派发空轮换（那会丢掉
                    // 恢复码选项、变成"意外不补发"）。
                    if let Some(panel) = self.dev_sync.clone() {
                        panel.update(cx, |panel, _cx| {
                            panel.set_notice(
                                "轮换未派发：面板没有登记请求（请重新点击「轮换密钥…」）",
                            );
                        });
                    }
                    changed = true;
                }
            }
        }
        if std::mem::take(&mut self.dev_sync_apply_requested) {
            let _ = self.dev_sync_apply_epoch(cx);
            changed = true;
        }
        // 2) 持闸同步推进（gate + service 事件都在这一步收敛）。
        changed |= self.dev_sync_advance(cx);
        // 3) 活动视图泵：adopt 的 Remote 在这里变成编辑器里的文字（不重开文档）。
        if let Some(active) = &self.active {
            changed |= active.view.update(cx, |view, cx| view.dev_sync_pump(cx));
        }
        // 4) ADR 0012 S3：本地编辑拿到 `DurableAck` 后触发自动同步的 2s 尾缘 debounce
        //    （远端 adopt 不产生本地编辑代数，因此不会触发）。
        changed |= self.dev_sync_auto_observe_edits(cx);
        if changed {
            cx.notify();
        }
        changed
    }

    /// 面板实体（测试读同步行/notice 用；宿主内部也用它写状态行）。
    #[must_use]
    pub fn dev_sync_panel(&self) -> Option<Entity<crate::dev_sync::DevSyncPanel>> {
        self.dev_sync.clone()
    }

    /// V2 管理面板（ADR 0016 S4d1c；`None` = 默认构建或没有真实库路径）。
    #[must_use]
    pub fn management_panel(&self) -> Option<Entity<crate::dev_sync::ManagementPanel>> {
        self.management.clone()
    }

    /// 设置页里的「管理」区是否展开。
    #[must_use]
    pub const fn management_open(&self) -> bool {
        self.management_open
    }

    /// 展开/收起设置页里的「管理」区；展开时触发一次刷新（不重复触发）。
    pub fn toggle_management(&mut self, cx: &mut Context<Self>) {
        if self.management.is_none() {
            return;
        }
        self.management_open = !self.management_open;
        if self.management_open {
            if let Some(panel) = &self.management {
                panel.update(cx, |panel, cx| panel.activate(cx));
            }
        }
        cx.notify();
    }

    /// 持闸同步是否在途（面板按钮置灰 / 测试收敛判据）。
    #[must_use]
    pub fn dev_sync_sync_in_progress(&self) -> bool {
        self.dev_sync_sync.is_some()
    }

    /// 最近一次**持闸同步**的报告（成功或失败都保留；开始新一轮时不清空）。
    ///
    /// 发布基线也写同一个 `dev_sync_last_report`（同一套状态机），这里按任务种类过滤：
    /// 同步的断言不会被发布覆盖，反之亦然。
    #[must_use]
    pub fn dev_sync_last_sync(&self) -> Option<&crate::dev_sync::SyncOnceReport> {
        self.dev_sync_last_report
            .as_ref()
            .filter(|report| report.task == crate::dev_sync::GatedTask::SyncOnce)
    }

    /// 最近一次「发布基线」的报告（切片 4c-4；成功或失败都保留）。
    #[must_use]
    pub fn dev_sync_last_publish(&self) -> Option<&crate::dev_sync::SyncOnceReport> {
        self.dev_sync_last_report
            .as_ref()
            .filter(|report| report.task == crate::dev_sync::GatedTask::PublishBaseline)
    }

    /// 「发布基线」是否在途（面板按钮置灰 / 测试收敛判据）。
    #[must_use]
    pub fn dev_sync_publish_in_progress(&self) -> bool {
        self.dev_sync_sync
            .as_ref()
            .is_some_and(|state| state.task == crate::dev_sync::GatedTask::PublishBaseline)
    }

    /// 轮换 / 应用新 epoch 是否在途（测试收敛判据；切片 6a）。
    #[must_use]
    pub fn dev_sync_rotation_in_progress(&self) -> bool {
        self.dev_sync_sync
            .as_ref()
            .is_some_and(|state| state.task.is_rotation())
    }

    /// 最近一次轮换 / apply 的报告（成功或失败都保留；切片 6a）。
    #[must_use]
    pub fn dev_sync_last_rotation(&self) -> Option<&crate::dev_sync::SyncOnceReport> {
        self.dev_sync_last_report
            .as_ref()
            .filter(|report| report.task.is_rotation())
    }

    // ---- ADR 0012 S3：自动同步接线（调度/状态 UX/重启恢复/跨操作续批）----

    /// 自动同步接线的只读投影（`None` = dev-sync 未启用）。
    #[must_use]
    pub fn dev_sync_auto(&self) -> Option<&crate::dev_sync::AutoSync> {
        self.dev_sync_auto.as_ref()
    }

    /// 常驻状态投影（`sync-status` 的文本来源；测试断言用）。
    #[must_use]
    pub fn dev_sync_status(&self) -> Option<crate::dev_sync::SyncStatus> {
        self.dev_sync_auto
            .as_ref()
            .map(|auto| auto.status().clone())
    }

    /// 常驻状态行文案（selector `sync-status`）。
    #[must_use]
    pub fn dev_sync_status_line(&self) -> Option<String> {
        let auto = self.dev_sync_auto.as_ref()?;
        Some(auto.status().line(auto.now()))
    }

    /// 「需要处理」行文案（selector `sync-status-error`；`None` = 不渲染）。
    ///
    /// 只有"需要人工处理 / 安全暂停 / 交付失败仍持有闸门"才出现：普通退避与每轮
    /// 成功都不打扰。
    #[must_use]
    pub fn dev_sync_status_error_line(&self) -> Option<String> {
        let auto = self.dev_sync_auto.as_ref()?;
        auto.status().error_line()
    }

    /// 当前安全暂停的脱敏类别（设置页「错误类别」用；`None` = 没有安全暂停）。
    #[must_use]
    pub fn dev_sync_security_category(&self) -> Option<crate::dev_sync::SecurityCategory> {
        self.dev_sync_auto
            .as_ref()
            .and_then(crate::dev_sync::AutoSync::security_category)
    }

    /// 读取本实例 durable `sync.control_pin`（设置页安全指纹的唯一来源；**只读**）。
    ///
    /// `Ok(None)` = 从未建立（未建立信任），`Err` = 损坏/绑定不符（**不是**没有 pin）。
    #[cfg(feature = "dev-sync")]
    pub fn dev_sync_control_pin(
        &self,
        vault_id: [u8; 16],
    ) -> Result<Option<menghunote_sync::control_pin::PinV1>, menghunote_sync::error::SyncError>
    {
        menghunote_sync::control_pin::load_control_pin(&self.vault, vault_id)
    }

    /// 当前这一轮的来源（`None` = 没有登记过在途轮）。
    #[must_use]
    pub fn dev_sync_round_origin(&self) -> Option<crate::dev_sync::OpOrigin> {
        self.dev_sync_round_origin
    }

    /// 清理"记账里有轮次、但没有真实 worker op"的陈旧状态。
    ///
    /// 宿主先看 `ActiveSyncOnce` 与 service snapshot：任意一个真实在途都返回 false，
    /// 保证正常同步不会被杀。只有两边都空时才让 AutoSync 撤回 origin/running。
    fn dev_sync_recover_stale_round(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(service) = self.dev_sync_service(cx) else {
            return false;
        };
        let snapshot = service.snapshot();
        if self.dev_sync_sync.is_some() || snapshot.in_flight.is_some() || snapshot.worker_stopped {
            return false;
        }
        let Some(auto) = self.dev_sync_auto.as_mut() else {
            return false;
        };
        let now = auto.now();
        let had_origin = auto.origin().is_some();
        let scheduler_running = auto.scheduler().is_running();
        if !had_origin && !scheduler_running {
            return false;
        }
        let recovered = auto.recover_stale_round(now);
        if recovered {
            self.dev_sync_round_origin = None;
            eprintln!(
                "[menghunote] 同步：清理陈旧轮次（origin 残留={had_origin} scheduler_running={scheduler_running}；没有 ActiveSyncOnce/in_flight）"
            );
            cx.notify();
        }
        recovered
    }

    /// 自动同步推进一拍：刷新计数 → 观测本地编辑 → 调度决策 → 派发。
    ///
    /// 生产由 [`Self::spawn_dev_sync_ticker`] 每 40ms 调用；headless 测试显式调用
    /// （时钟可注入，故不需要 sleep，也不需要真的推进 GPUI 定时器）。
    pub fn dev_sync_auto_tick(&mut self, cx: &mut Context<Self>) -> bool {
        // 启动/上一轮收尾的陈旧记账：没有真实 op 时才清理，避免永远卡在"单操作在途"。
        let recovered = self.dev_sync_recover_stale_round(cx);
        let Some(auto) = self.dev_sync_auto.as_ref() else {
            return recovered;
        };
        if auto.is_stopped() {
            return recovered;
        }
        let now = auto.now();
        let mut changed = false;
        // 1) durable 计数（节流；状态行与续批判据都读它）。
        if auto.status_refresh_due(now) {
            let backlog = crate::dev_sync::autosync::durable_backlog(&self.vault);
            if let Some(auto) = self.dev_sync_auto.as_mut() {
                auto.observe_backlog(backlog, now);
            }
            changed = true;
        }
        // 2) 本地编辑是否已经 DurableAck（尾缘 debounce 的触发源）。
        changed |= self.dev_sync_auto_observe_edits(cx);
        // 3) 宿主繁忙（单操作在途 / Hed 之外的设备命令 / worker 停止）：自动不抢占。
        let busy = self.dev_sync_sync.is_some()
            || self.dev_sync_service(cx).is_some_and(|service| {
                let snapshot = service.snapshot();
                snapshot.in_flight.is_some() || snapshot.worker_stopped
            });
        if let Some(auto) = self.dev_sync_auto.as_mut() {
            auto.set_host_busy(busy);
        }
        // 3b) 合并的手动请求（自动轮期间点过「同步」）：空闲时优先补一次。
        let manual_queued = self
            .dev_sync_auto
            .as_ref()
            .is_some_and(crate::dev_sync::AutoSync::manual_queued);
        if manual_queued && !busy {
            if let Some(auto) = self.dev_sync_auto.as_mut() {
                let _ = auto.take_manual_request();
            }
            let _ = self.dev_sync_sync_once(cx);
            return true;
        }
        // 4) 调度决策（启动合并/周期/退避/续批都在纯调度器里）。
        let decision = match self.dev_sync_auto.as_mut() {
            Some(auto) => auto.tick(now),
            None => return changed,
        };
        if busy {
            return changed;
        }
        changed | self.dev_sync_auto_handle(decision, cx, now)
    }

    /// 撤销本轮登记（派发被拒/没有真的跑起来）：不计失败、不退避。
    fn dev_sync_abort_round(&mut self) {
        let now = self
            .dev_sync_auto
            .as_ref()
            .map(crate::dev_sync::AutoSync::now)
            .unwrap_or_else(std::time::Instant::now);
        if let Some(auto) = self.dev_sync_auto.as_mut() {
            auto.abort_round(now);
        }
        self.dev_sync_round_origin = None;
        self.dev_sync_auto_deferred = false;
        self.dev_sync_last_stage = None;
    }

    /// 观测活动文档的本地编辑：`DurableAck` 后交给调度器做 2s 尾缘 debounce。
    ///
    /// 远端 adopt 不产生本地编辑代数（`EditorView` 只在 `EditOutcome::Applied` 上自增），
    /// 所以"远端 ACK 不触发"是结构性的，不靠调用点自觉。
    fn dev_sync_auto_observe_edits(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(auto) = self.dev_sync_auto.as_ref() else {
            return false;
        };
        if auto.is_stopped() {
            return false;
        }
        let Some(active) = self.active.as_ref() else {
            return false;
        };
        let (seq, saved) = {
            let view = active.view.read(cx);
            (view.dev_sync_local_edit_seq(), view.dev_sync_is_saved())
        };
        let mut changed = false;
        if seq != self.dev_sync_auto_edit_seq {
            self.dev_sync_auto_edit_seq = seq;
            if let Some(auto) = self.dev_sync_auto.as_mut() {
                auto.note_local_edit_pending();
            }
            changed = true;
        }
        let pending = self
            .dev_sync_auto
            .as_ref()
            .is_some_and(crate::dev_sync::AutoSync::local_edit_pending);
        if pending && saved {
            let now = self
                .dev_sync_auto
                .as_ref()
                .map(crate::dev_sync::AutoSync::now)
                .unwrap_or_else(std::time::Instant::now);
            if let Some(auto) = self.dev_sync_auto.as_mut() {
                let _ = auto.on_local_durable(now);
            }
            changed = true;
        }
        changed
    }

    /// 调度器要求启动：派发一轮自动同步（两阶段 / 无会话）。
    fn dev_sync_auto_handle(
        &mut self,
        decision: crate::dev_sync::Decision,
        cx: &mut Context<Self>,
        now: std::time::Instant,
    ) -> bool {
        let crate::dev_sync::Decision::Start(trigger) = decision else {
            return false;
        };
        eprintln!("[menghunote] 自动同步开始：trigger={trigger:?}");
        let Some(service) = self.dev_sync_service(cx) else {
            self.dev_sync_abort_round();
            return true;
        };
        let snapshot = service.snapshot();
        if snapshot.worker_stopped || snapshot.in_flight.is_some() {
            // 单操作/worker 停止（例如面板的自动设备刷新）：退回这次 Start，
            // 再合并一条"稍后重试"——不抢占已开始的事务，也不留下假的 Running。
            if let Some(auto) = self.dev_sync_auto.as_mut() {
                let _ = auto.on_busy_after_start(now);
            }
            return true;
        }
        let backlog = crate::dev_sync::autosync::durable_backlog(&self.vault);
        if let Some(auto) = self.dev_sync_auto.as_mut() {
            auto.begin_auto_round(trigger, backlog);
        }
        self.dev_sync_round_origin = Some(crate::dev_sync::OpOrigin::Auto);
        self.dev_sync_auto_deferred = false;
        let dispatched = if self.active.is_none() {
            self.dev_sync_dispatch_no_session(cx, &service)
        } else {
            self.dev_sync_start_two_phase(cx, &service)
        };
        if let Err(error) = dispatched {
            if let Some(auto) = self.dev_sync_auto.as_mut() {
                let _ = auto.on_busy(now);
            }
            eprintln!("[menghunote] 自动同步派发被拒（{trigger:?}）：{error}");
        }
        true
    }

    /// 窗口激活变化（失焦 60s；重新聚焦时若有 durable 待办合并一次探测）。
    fn dev_sync_auto_on_activation(&mut self, active: bool, cx: &mut Context<Self>) {
        let Some(auto) = self.dev_sync_auto.as_mut() else {
            return;
        };
        if auto.is_stopped() {
            return;
        }
        let now = auto.now();
        if auto.status_refresh_due(now) {
            let backlog = crate::dev_sync::autosync::durable_backlog(&self.vault);
            auto.observe_backlog(backlog, now);
        }
        auto.on_activation(active, now);
        cx.notify();
    }

    /// 一轮收尾：把结果交回自动同步调度器（退避/续批/状态行）。
    fn dev_sync_auto_finish_round(&mut self, report: &crate::dev_sync::SyncOnceReport) {
        if report.task != crate::dev_sync::GatedTask::SyncOnce {
            // 发布基线不是"同步轮"：不参与自动退避/续批。
            return;
        }
        let deferred_by_gate = std::mem::take(&mut self.dev_sync_auto_deferred);
        let gate_still_held = self.dev_sync_sync.is_some();
        let Some(auto) = self.dev_sync_auto.as_mut() else {
            self.dev_sync_round_origin = None;
            return;
        };
        let now = auto.now();
        let backlog = crate::dev_sync::autosync::durable_backlog(&self.vault);
        let Some(origin) = self.dev_sync_round_origin.take() else {
            // 没有新派发的收尾（例如交付失败后的「重试收尾」）：成功后只恢复状态行。
            if report.is_ok() {
                auto.note_recovery(backlog, now);
            }
            return;
        };
        let frozen = match (&report.error, gate_still_held) {
            (Some(error), true) => Some(error.clone()),
            _ => None,
        };
        let outcome = crate::dev_sync::RoundOutcome {
            success: report.is_ok() && !deferred_by_gate,
            error: report.error.clone(),
            deferred_by_gate,
            frozen,
        };
        let decision = auto.finish_round(outcome, backlog, now);
        if decision.is_start() {
            // 收尾路径**不**重入派发：`finish_round` 已把它退回 pending，下一拍 tick 再启动。
            eprintln!("[menghunote] 自动同步（{origin:?}）收尾：还有待办，等下一拍继续");
        } else if !report.is_ok() {
            eprintln!(
                "[menghunote] 自动同步（{origin:?}）收尾：{}",
                report.error.as_deref().unwrap_or("未成功")
            );
        }
    }

    /// 启动一次**持闸同步**（面板「同步」按钮 / 测试入口）。
    ///
    /// 只是 [`Self::dev_sync_start_gated`] 的 `SyncOnce` 入口：两条持闸任务（同步 /
    /// 发布基线）共用同一条状态机，差别只有 worker 命令与报告种类（切片 4c-4）。
    /// 没有活动文档时走**无会话同步**（空库/还没打开笔记）：
    /// [`Self::dev_sync_dispatch_no_session`]。
    pub fn dev_sync_sync_once(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        // 0) 两阶段收尾失败（闸门仍 Held）：再点一次只重试**交付**，不重新 Prepare。
        //    这是 fail-closed 路径的唯一出口（否则用户只能重启实例）。
        if let Some(state) = self.dev_sync_sync.as_ref() {
            if state.two_phase
                && state.stage == crate::dev_sync::SyncOnceStage::Delivering
                && state.lease.is_some()
            {
                let task = state.task;
                let error = state
                    .pending_error
                    .clone()
                    .unwrap_or_else(|| "上一轮收尾未完成（闸门仍持有）".to_owned());
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    "重试收尾（闸门仍持有）",
                    Some(format!("{}：重试收尾（{error}）", task.short_label())),
                );
                let _ = self.dev_sync_retry_two_phase_delivery(cx);
                return Ok(());
            }
        }
        // 0b) ADR 0012 S3：自动轮在跑时，手动点击**合并**（不派发第二条）。
        //     自动轮收尾后由 `dev_sync_auto_finish_round` 立刻补一次手动轮；
        //     这里只如实告诉用户"已排队"，不假装本轮是手动触发的。
        if let Some(state) = self.dev_sync_sync.as_ref() {
            let auto_in_flight =
                self.dev_sync_round_origin == Some(crate::dev_sync::OpOrigin::Auto);
            if auto_in_flight {
                if let Some(auto) = self.dev_sync_auto.as_mut() {
                    auto.queue_manual();
                }
                let in_flight_task = state.task;
                self.dev_sync_panel_task_state(
                    cx,
                    in_flight_task,
                    true,
                    "自动同步中：已合并手动同步",
                    Some(
                        "自动同步进行中：手动「同步」已合并到本轮结束后立即执行（单操作串行）"
                            .to_owned(),
                    ),
                );
                return Ok(());
            }
        }
        // 1) 同一时刻只允许一条同步任务（两阶段三阶段共用同一 slot）。
        if let Some(state) = self.dev_sync_sync.as_ref() {
            let in_flight_task = state.task;
            let message = in_flight_task.busy_label().to_owned();
            self.dev_sync_panel_task_state(
                cx,
                in_flight_task,
                true,
                format!("{}已在进行", in_flight_task.label()),
                Some(message.clone()),
            );
            return Err(message);
        }
        // 2) 面板/服务可用，且 worker 没有别的命令在跑。
        let Some(service) = self.dev_sync_service(cx) else {
            let message = "dev-sync 未启用：没有同步面板/服务，无法同步".to_owned();
            return Err(message);
        };
        let snapshot = service.snapshot();
        if snapshot.worker_stopped {
            let message = "同步失败：dev-sync worker 已停止（请重启实例）".to_owned();
            self.dev_sync_panel_task_state(
                cx,
                crate::dev_sync::GatedTask::SyncOnce,
                false,
                "worker 已停止",
                Some(message.clone()),
            );
            return Err(message);
        }
        if let Some((op_id, kind)) = snapshot.in_flight {
            let (line, message) = if snapshot.deadline_exceeded == Some(op_id) {
                (
                    "上一轮已超时（等待真实收尾）",
                    format!(
                        "同步被拒绝：上一轮操作已超时（{op_id}），仍在等待 worker 真实收尾；                         不会派发第二操作（若长期无回执请重启实例）"
                    ),
                )
            } else {
                (
                    "已有操作在途",
                    format!("同步被拒绝：已有操作在途（{} {op_id}）", kind.label()),
                )
            };
            self.dev_sync_panel_task_state(
                cx,
                crate::dev_sync::GatedTask::SyncOnce,
                true,
                line,
                Some(message.clone()),
            );
            return Err(message);
        }
        // 2b) ADR 0012 S3：手动优先——先过调度器（可绕过一次网络退避；不能绕过
        //     单操作/Held/Paused）。被挡住时把原因如实写回面板，不静默排队。
        if let Some(auto) = self.dev_sync_auto.as_mut() {
            // 手动优先：调用方已经确认没有在途 op/worker 停止，这里的 `host_busy` 只可能
            // 是上一拍 tick 留下的陈旧标记（自动轮刚收尾）——不拿它挡用户。
            auto.set_host_busy(false);
            let now = auto.now();
            let backlog = crate::dev_sync::autosync::durable_backlog(&self.vault);
            match auto.request_manual(now) {
                crate::dev_sync::Decision::Start(_) => {
                    auto.begin_manual_round(backlog);
                }
                crate::dev_sync::Decision::Wait(reason) => {
                    let message = format!("同步被拒绝：调度器当前不可用（{}）", reason.label());
                    self.dev_sync_panel_task_state(
                        cx,
                        crate::dev_sync::GatedTask::SyncOnce,
                        true,
                        "调度器暂不启动",
                        Some(message.clone()),
                    );
                    return Err(message);
                }
                crate::dev_sync::Decision::Idle => {}
            }
        }
        self.dev_sync_round_origin = Some(crate::dev_sync::OpOrigin::Manual);
        self.dev_sync_auto_deferred = false;
        // 3) 没有活动文档 = 旧的无会话同步（S2a 只把**有活动文档**的手动同步改成两阶段）。
        if self.active.is_none() {
            return self.dev_sync_dispatch_no_session(cx, &service);
        }
        self.dev_sync_start_two_phase(cx, &service)
    }

    /// 发起一次两阶段同步：**只派发 Prepare**（网络阶段不 begin gate、不冻结输入）。
    fn dev_sync_start_two_phase(
        &mut self,
        cx: &mut Context<Self>,
        service: &std::sync::Arc<crate::dev_sync::SyncService>,
    ) -> Result<(), String> {
        // 结构 head 基线：收尾时只有它真的变了才重投影（不假刷新）。
        let structure_base_seq = load_head(&self.vault, STRUCTURE_DOC_ID)
            .map(|head| head.head_seq)
            .unwrap_or(0);
        match service.try_command_two_phase() {
            Ok(op_id) => {
                eprintln!("[menghunote] 同步已派发 op={op_id}（两阶段 Prepare）");
                let origin = self
                    .dev_sync_round_origin
                    .unwrap_or(crate::dev_sync::OpOrigin::Manual);
                let mut state = crate::dev_sync::ActiveSyncOnce::new_two_phase(
                    crate::dev_sync::GatedTask::SyncOnce,
                    structure_base_seq,
                )
                .with_origin(origin);
                state.op_id = Some(op_id);
                self.dev_sync_sync = Some(state);
                self.dev_sync_panel_task_state(
                    cx,
                    crate::dev_sync::GatedTask::SyncOnce,
                    true,
                    "同步中（网络阶段，不冻结输入）",
                    Some("同步：网络阶段（不持闸，可继续编辑/切换）".to_owned()),
                );
                // 第一拍：本地 relay 可能已经 NeedGate；没有就等下一帧。
                self.dev_sync_advance(cx);
                Ok(())
            }
            Err(busy) => {
                // 没有真的派发出去：撤回调度登记（不计失败/不退避），自动轮稍后重试。
                self.dev_sync_abort_round();
                let message = format!("同步失败：无法派发 Prepare（{}）", busy.reason.label());
                self.dev_sync_panel_task_state(
                    cx,
                    crate::dev_sync::GatedTask::SyncOnce,
                    false,
                    "派发失败",
                    Some(message.clone()),
                );
                Err(message)
            }
        }
    }

    /// 启动一次**持闸发布初始基线**（面板「发布基线」按钮 / 测试入口，切片 4c-4）。
    ///
    /// 与同步完全同一条持闸状态机（`begin gate → Held → try_command_with_gate →
    /// Completed → prepare/adopt/end → notice`）：
    ///
    /// - 发布读取的是 durable 快照，所以必须先持闸（活动文档 flush 到 durable）；
    /// - 发布**不需要**额外的 adopt 语义——它不改写正文，只是把已 durable 的外部行按同步的
    ///   同一套规则交付（没有新行时什么也不做）；
    /// - 闸门释放前不允许任何本地写入；失败同样先交付再释放（与同步一致）；
    /// - 发布**没有**无会话路径：没有打开的笔记就没有闸门目标 → 明确拒绝。
    ///
    /// 额外前置条件：本机身份必须有管理密钥（Join 设备不能发布）。面板按钮已置灰，这里
    /// 再兜一次；权威判定在 worker（它重新读身份）。
    pub fn dev_sync_publish_baseline(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        self.dev_sync_start_gated(crate::dev_sync::GatedTask::PublishBaseline, None, cx)
    }

    /// A 侧**轮换密钥**（ADR 0009 Phase 2 切片 6a；dev-sync feature）。
    ///
    /// 仅供测试与 6b 的设备列表按钮调用：**不做**独立按钮、不自动轮换、不自动重试。
    /// `recovery_code` 是当前管理恢复码（`None` = 不补发；轮换后新设备无法用恢复码加入，
    /// 报告/面板会明确警告）。没有 revoke 列表时用本方法；需要一并吊销设备时用
    /// [`Self::dev_sync_rotate_revoking`]。
    ///
    /// 顺序：面板/服务可用、无在途 → 活动文档 `begin_external_commit` 到 `Held` →
    /// 派发 `Command::Rotate`（整段持闸）→ worker 完成后 `prepare/adopt/end` →
    /// **重建活动视图**（新 `EditorView`/`SessionDriver` 用新 epoch）→ 刷新树/状态。
    /// 失败保持旧身份/旧视图，错误原文写进面板。
    pub fn dev_sync_rotate(
        &mut self,
        recovery_code: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.dev_sync_rotate_revoking(recovery_code, Vec::new(), cx)
    }

    /// 同 [`Self::dev_sync_rotate`]，并附带本次 signed revoke 列表（ADR 0010 的轮换入口，
    /// 供测试/6b 的设备列表按钮；不在本片做 UI）。
    pub fn dev_sync_rotate_revoking(
        &mut self,
        recovery_code: Option<String>,
        revoke: Vec<menghunote_sync::DeviceId>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let request = crate::dev_sync::RotateRequest::new(recovery_code, revoke);
        self.dev_sync_start_gated(crate::dev_sync::GatedTask::Rotate, Some(request), cx)
    }

    /// B 侧**应用 relay 最新 epoch**（ADR 0009 Phase 2 切片 6a；dev-sync feature）。
    ///
    /// 与轮换共用同一条持闸状态机：活动文档冻结到 durable → 派发 `Command::ApplyEpoch`
    /// → worker 认证 activation 链 + 本机 grant 解根 + 基线 merge/推广 → UI 交付/解闸 →
    /// 重建活动视图并切换 `SyncContext.key_epoch`。
    ///
    /// 本机不在本次 grant 列表（被移除/吊销）时 worker 明确拒绝：身份与视图保持旧
    /// epoch，**不半推广**。
    pub fn dev_sync_apply_epoch(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        self.dev_sync_start_gated(crate::dev_sync::GatedTask::ApplyEpoch, None, cx)
    }

    /// 持闸任务的公共启动路径：登记闸门 → 第一拍推进（不阻塞 UI）。
    ///
    /// 返回 `Err(String)` 是**明确拒绝/失败**（面板显示原文），不吞：
    /// 已在同步/发布（并发）、没有活动文档（仅发布拒绝；同步转无会话路径）、没有面板/
    /// 服务、闸门已被占用、worker 已停、本机没有管理密钥（仅发布）。
    ///
    /// 只做登记与第一拍推进，不阻塞 UI：闸门可能要等 IME 自然结束 / flush 到 durable，
    /// 那些都在 [`Self::dev_sync_advance`] 里一帧一帧推进（状态机见
    /// `dev_sync::sync_once` 的模块文档）。
    fn dev_sync_start_gated(
        &mut self,
        task: crate::dev_sync::GatedTask,
        rotation: Option<crate::dev_sync::RotateRequest>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let label = task.label();
        // 发布基线不是"同步轮"：不参与自动同步的退避/续批/状态行记账。
        self.dev_sync_round_origin = None;
        self.dev_sync_auto_deferred = false;
        // 0a) 上一轮"收尾失败"（闸门仍 Held）：再点一次只重试收尾，不 begin 新闸门。
        //     这是 fail-closed 路径的唯一出口（否则用户只能重启实例）。
        let retry_delivery = self
            .dev_sync_sync
            .as_ref()
            .is_some_and(|state| state.stage == crate::dev_sync::SyncOnceStage::Delivering);
        if retry_delivery {
            // 只允许用**同一个任务**重试收尾：发布在途时点同步不会偷走它的闸门。
            let same_task = self
                .dev_sync_sync
                .as_ref()
                .is_some_and(|state| state.task == task);
            if !same_task {
                let in_flight_task = self
                    .dev_sync_sync
                    .as_ref()
                    .map_or("其它任务", |state| state.task.label());
                let message = format!(
                    "{label}被拒绝：{in_flight_task}的收尾还没完成（闸门仍持有），请先点它重试收尾"
                );
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    "收尾未完成（闸门仍持有）",
                    Some(message.clone()),
                );
                return Err(message);
            }
            let finish = self
                .dev_sync_last_report
                .as_ref()
                .and_then(|report| report.error.clone())
                .map_or_else(
                    || SyncOnceFinish::Ok("重试收尾".to_owned()),
                    SyncOnceFinish::Failed,
                );
            self.dev_sync_panel_task_state(
                cx,
                task,
                true,
                "重试收尾（闸门仍持有）",
                Some(format!("{}：重试收尾", task.short_label())),
            );
            self.dev_sync_finish(cx, finish);
            return Ok(());
        }
        // 0b) 同一时刻只允许一条持闸任务（重复命令 = Busy 语义）。
        if let Some(state) = self.dev_sync_sync.as_ref() {
            let in_flight_task = state.task;
            let message = in_flight_task.busy_label().to_owned();
            self.dev_sync_panel_task_state(
                cx,
                task,
                true,
                format!("{}已在进行", in_flight_task.label()),
                Some(message.clone()),
            );
            return Err(message);
        }
        // 1) 面板/服务必须可用，且 worker 没有正在跑别的命令。
        let Some(service) = self.dev_sync_service(cx) else {
            let message = format!("dev-sync 未启用：没有同步面板/服务，无法{label}");
            return Err(message);
        };
        let snapshot = service.snapshot();
        if snapshot.worker_stopped {
            let message = format!("{label}失败：dev-sync worker 已停止（请重启实例）");
            self.dev_sync_panel_task_state(cx, task, false, "worker 已停止", Some(message.clone()));
            return Err(message);
        }
        if let Some((op_id, kind)) = snapshot.in_flight {
            // 超时后 worker 仍在写边界内：明确告诉用户"在等真实收尾"，不是普通忙。
            let (line, message) = if snapshot.deadline_exceeded == Some(op_id) {
                (
                    "上一轮已超时（等待真实收尾）",
                    format!(
                        "{label}被拒绝：上一轮操作已超时（{op_id}），仍在等待 worker 真实收尾；\
                         不会派发第二操作（若长期无回执请重启实例）"
                    ),
                )
            } else {
                (
                    "已有操作在途",
                    format!("{label}被拒绝：已有操作在途（{} {op_id}）", kind.label()),
                )
            };
            self.dev_sync_panel_task_state(cx, task, true, line, Some(message.clone()));
            return Err(message);
        }
        // 1b) 发布基线/轮换需要管理密钥：面板按钮已置灰，这里再兜一次（权威判定在 worker）。
        //     只在 worker 已经上报过身份时提前拒绝；否则交给 worker 给真正原因。
        if matches!(
            task,
            crate::dev_sync::GatedTask::PublishBaseline | crate::dev_sync::GatedTask::Rotate
        ) && snapshot.self_device.is_some()
            && !snapshot.can_manage_devices
        {
            let message = match task {
                crate::dev_sync::GatedTask::Rotate => {
                    "轮换被拒绝：本机没有管理密钥（Join 设备）；请用 Create 设备/管理设备发起"
                        .to_owned()
                }
                _ => {
                    "发布基线被拒绝：本机没有管理密钥（Join 设备）；请用 Create 设备发布".to_owned()
                }
            };
            self.dev_sync_panel_task_state(cx, task, false, "没有管理密钥", Some(message.clone()));
            return Err(message);
        }
        // 2) 没有活动文档：同步走无会话路径（空库/还没打开笔记也能拿到远端结构与正文）；
        //    发布基线/轮换/应用 epoch 必须有活动文档——它们要在活动文档上持闸冻结写入，
        //    没有会话就没有闸门目标（无会话路径没有 `HeldGate` 证明，轮换不得降级）。
        let Some(active) = self.active.as_ref() else {
            if task == crate::dev_sync::GatedTask::SyncOnce {
                return self.dev_sync_dispatch_no_session(cx, &service);
            }
            let message = if task.is_rotation() {
                format!(
                    "{label}被拒绝：当前没有打开的笔记；{label}需要先在活动文档上持闸冻结\
                     （请先打开一篇笔记再试）"
                )
            } else {
                "发布基线失败：当前没有打开的笔记（发布需要先在活动文档上持闸；\
                 请先打开一篇笔记）"
                    .to_owned()
            };
            self.dev_sync_panel_task_state(cx, task, false, "没有活动文档", Some(message.clone()));
            return Err(message);
        };
        // 3) 有活动文档：持闸路径（本片只保证活动文档的正文；结构树收尾时按需重投影）。
        let view = active.view.clone();
        let doc_id = view.read(cx).dev_sync_doc_id().to_owned();
        // 记录任务开始前的结构 head：收尾时只有它真的变了才重投影（不假刷新）。
        let structure_base_seq = load_head(&self.vault, STRUCTURE_DOC_ID)
            .map(|head| head.head_seq)
            .unwrap_or(0);
        // 4) 登记外部提交闸门（Held 之前不派发任何命令）。
        let ticket = match view.update(cx, |view, cx| view.dev_sync_begin_external(cx)) {
            Ok(ticket) => ticket,
            Err(error) => {
                let message = format!("{label}失败：无法持有外部提交闸门（{error}）");
                self.dev_sync_panel_task_state(cx, task, false, "持闸失败", Some(message.clone()));
                return Err(message);
            }
        };
        let mut state = crate::dev_sync::ActiveSyncOnce::new(
            task,
            doc_id.clone(),
            view,
            ticket,
            structure_base_seq,
        );
        if let Some(rotation) = rotation {
            state = state.with_rotate(rotation);
        }
        self.dev_sync_sync = Some(state);
        let waiting = if task.is_rotation() {
            format!("{}中（等待持闸，{doc_id}）", task.short_label())
        } else {
            format!("等待持闸（{doc_id}）")
        };
        self.dev_sync_panel_task_state(
            cx,
            task,
            true,
            waiting,
            Some(format!("{}：已提交持闸请求，等待落盘", task.short_label())),
        );
        // 5) 第一拍：没有脏数据时直接就 Held 并派发；否则等下一帧继续 poll。
        self.dev_sync_advance(cx);
        Ok(())
    }

    /// 没有活动文档时派发**无会话同步**（空库/还没打开笔记）。
    ///
    /// 这里不碰任何 `EditorView`/`DocumentSession`：没有会话可 begin/adopt，所以不持闸；
    /// 同步期间 [`Self::write_structure`] 会拒绝结构写入（`dev_sync_sync.is_some()`），
    /// 收尾在 [`Self::dev_sync_finish`] 的无视图路径里重投影 + 自动打开第一篇。
    fn dev_sync_dispatch_no_session(
        &mut self,
        cx: &mut Context<Self>,
        service: &std::sync::Arc<crate::dev_sync::SyncService>,
    ) -> Result<(), String> {
        // 结构 head 基线：收尾时只有它真的变了才重投影（不假刷新）。
        let structure_base_seq = load_head(&self.vault, STRUCTURE_DOC_ID)
            .map(|head| head.head_seq)
            .unwrap_or(0);
        let origin = self
            .dev_sync_round_origin
            .unwrap_or(crate::dev_sync::OpOrigin::Manual);
        match service.try_command_no_session() {
            Ok(op_id) => {
                eprintln!("[menghunote] 同步已派发 op={op_id}（无会话）");
                self.dev_sync_sync = Some(
                    crate::dev_sync::ActiveSyncOnce::new_no_session(op_id, structure_base_seq)
                        .with_origin(origin),
                );
                self.dev_sync_panel_state(
                    cx,
                    true,
                    format!("无活动文档：已派发无会话同步（{op_id}）"),
                    Some("同步：没有活动文档，已派发无会话同步".to_owned()),
                );
                // 第一拍：worker 可能已经回执（本地 relay 很快）；没有就等下一帧。
                self.dev_sync_advance(cx);
                Ok(())
            }
            Err(busy) => {
                self.dev_sync_abort_round();
                let message = format!("同步失败：无法派发无会话同步（{}）", busy.reason.label());
                self.dev_sync_panel_state(cx, false, "派发失败", Some(message.clone()));
                Err(message)
            }
        }
    }

    /// 推进一拍同步；返回是否有可视变化（不阻塞、不做网络）。
    fn dev_sync_advance(&mut self, cx: &mut Context<Self>) -> bool {
        // 阶段变化打一行（不含 secret；op_id 是公开事实），用来定位"卡在哪一步"。
        let stage = self.dev_sync_sync.as_ref().map(|state| state.stage);
        if stage != self.dev_sync_last_stage {
            self.dev_sync_last_stage = stage;
            if let Some(stage) = stage {
                let op = self
                    .dev_sync_sync
                    .as_ref()
                    .and_then(|state| state.op_id)
                    .map_or_else(|| "None".to_owned(), |op| op.to_string());
                eprintln!("[menghunote] 同步阶段 → {stage:?} op={op}");
            }
        }
        let two_phase = self
            .dev_sync_sync
            .as_ref()
            .is_some_and(|state| state.two_phase);
        match self.dev_sync_sync.as_ref().map(|state| state.stage) {
            None => false,
            Some(crate::dev_sync::SyncOnceStage::Preparing) => self.dev_sync_advance_prepare(cx),
            Some(crate::dev_sync::SyncOnceStage::NeedGate) => self.dev_sync_advance_need_gate(cx),
            Some(crate::dev_sync::SyncOnceStage::WaitingGate) => {
                if two_phase {
                    self.dev_sync_advance_two_phase_gate(cx)
                } else {
                    self.dev_sync_advance_gate(cx)
                }
            }
            Some(crate::dev_sync::SyncOnceStage::Integrating) => {
                self.dev_sync_advance_integrating(cx)
            }
            Some(crate::dev_sync::SyncOnceStage::Finishing) => self.dev_sync_advance_finishing(cx),
            Some(crate::dev_sync::SyncOnceStage::Cancelling) => {
                self.dev_sync_advance_cancelling(cx)
            }
            Some(crate::dev_sync::SyncOnceStage::WorkerRunning) => self.dev_sync_advance_worker(cx),
            // 收尾失败（闸门仍 Held）：用户再次点「同步」重试；但若之前是"没有可靠回执"
            // 才留下的 Held，真实回执一到就自动收尾（ADR 0012 S1：迟到回执不能丢）。
            Some(crate::dev_sync::SyncOnceStage::Delivering) => {
                self.dev_sync_advance_delivering(cx)
            }
        }
    }

    /// `Delivering` 且**尚未**拿到真实回执时的补收：迟到回执一到就自动收尾一次。
    ///
    /// 为什么只做一次：`worker_settled` 一旦置位，后续收尾失败仍是"用户显式重试"路径，
    /// 不能每 40 ms 重放（否则收尾失败会变成死循环）。
    fn dev_sync_advance_delivering(&mut self, cx: &mut Context<Self>) -> bool {
        // 两阶段的 Delivering 是"收尾失败、等用户显式重试"的 fail-closed 状态：
        // 这里不自动重放（否则失败会变成每帧死循环）。
        if self
            .dev_sync_sync
            .as_ref()
            .is_some_and(|state| state.two_phase)
        {
            return false;
        }
        let Some((task, op_id)) = self
            .dev_sync_sync
            .as_ref()
            .filter(|state| !state.worker_settled)
            .and_then(|state| state.op_id.map(|op| (state.task, op)))
        else {
            return false;
        };
        let Some(service) = self.dev_sync_service(cx) else {
            return false;
        };
        let snapshot = service.snapshot();
        if let Some((completed_op, _heads, result)) = &snapshot.last_completed {
            if *completed_op == op_id {
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.worker_settled = true;
                    state.rotation_facts = snapshot
                        .last_rotation
                        .clone()
                        .filter(|_| snapshot.last_rotation_op == Some(op_id));
                }
                return match result.clone() {
                    crate::dev_sync::OpResult::Ok { summary } => {
                        self.dev_sync_finish(cx, SyncOnceFinish::Ok(summary))
                    }
                    crate::dev_sync::OpResult::Failed { message } => {
                        self.dev_sync_finish(cx, SyncOnceFinish::Failed(message))
                    }
                };
            }
        }
        if snapshot.worker_stopped {
            if let Some(state) = self.dev_sync_sync.as_mut() {
                state.worker_settled = true;
            }
            return self.dev_sync_finish(
                cx,
                SyncOnceFinish::Failed(format!(
                    "{}失败：worker 已停止，未拿到本次任务的明确终态",
                    task.short_label()
                )),
            );
        }
        false
    }

    /// `Preparing`（ADR 0012 S2a）：等 Prepare 的 `NeedGate`；**不持闸、不冻结输入**。
    ///
    /// 观察点是服务快照里的 `two_phase`（阶段）与 `last_completed`（真实终态）。
    fn dev_sync_advance_prepare(&mut self, cx: &mut Context<Self>) -> bool {
        let (task, op_id) = match self.dev_sync_sync.as_ref() {
            Some(state) if state.two_phase => match state.op_id {
                Some(op_id) => (state.task, op_id),
                None => return false,
            },
            _ => return false,
        };
        let Some(service) = self.dev_sync_service(cx) else {
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: format!("{}失败：dev-sync 服务已不可用", task.short_label()),
                },
            );
        };
        let snapshot = service.snapshot();
        if let Some((completed_op, _heads, result)) = &snapshot.last_completed {
            if *completed_op == op_id {
                let Some(mut state) = self.dev_sync_sync.take() else {
                    return false;
                };
                state.worker_settled = true;
                return self.dev_sync_finish_two_phase_with_state(cx, state, result.clone());
            }
        }
        if snapshot.worker_stopped {
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: format!(
                        "{}失败：worker 已停止，未拿到 Prepare 的明确终态",
                        task.short_label()
                    ),
                },
            );
        }
        if snapshot.deadline_exceeded == Some(op_id) {
            let first_time = self
                .dev_sync_sync
                .as_ref()
                .is_some_and(|state| !state.deadline_noted);
            if first_time {
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.deadline_noted = true;
                    state.pending_error = Some(format!(
                        "{}超时：网络阶段（未持闸）已登记取消，等 worker 真实收尾",
                        task.short_label()
                    ));
                }
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    format!("已超时（{op_id}）：网络阶段，未持闸"),
                    Some(format!(
                        "{}超时：Prepare 阶段不持闸、编辑器不冻结；worker 会在集成前停手，\
                         等真实回执（不会派发第二操作）",
                        task.short_label()
                    )),
                );
                return true;
            }
        }
        let Some(status) = snapshot.two_phase else {
            return false;
        };
        if status.op_id != op_id || status.phase != crate::dev_sync::state::SyncPhase::NeedGate {
            return false;
        }
        let Some(state) = self.dev_sync_sync.as_mut() else {
            return false;
        };
        state.authorization_epoch = status.authorization_epoch;
        state.stage = crate::dev_sync::SyncOnceStage::NeedGate;
        // ADR 0012 S3：把 worker 上报的"已接收·待应用"记进常驻状态（收到 ≠ 已应用）。
        if let Some(auto) = self.dev_sync_auto.as_mut() {
            auto.note_need_gate(status.stored, status.deferred);
        }
        self.dev_sync_panel_task_state(
            cx,
            task,
            true,
            format!("网络阶段完成，等待持闸授权（{op_id}）"),
            Some(format!(
                "{}：Prepare 已完成（网络阶段未冻结输入），准备在当前活动文档上短暂持闸",
                task.short_label()
            )),
        );
        true
    }

    /// `NeedGate`：在**当时**的活动文档上 `begin_external_commit`（短持闸窗口从这里开始）。
    ///
    /// 没有活动文档 / 超时 / 持闸失败时走取消路径：worker 还在等授权，取消会穿透控制
    /// 通道；闸门从未授予，所以不会 append。
    fn dev_sync_advance_need_gate(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((task, op_id)) = self.dev_sync_sync.as_ref().and_then(|state| {
            (state.two_phase && state.stage == crate::dev_sync::SyncOnceStage::NeedGate)
                .then(|| state.op_id.map(|op| (state.task, op)))
                .flatten()
        }) else {
            return false;
        };
        let Some(service) = self.dev_sync_service(cx) else {
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: format!("{}失败：dev-sync 服务已不可用", task.short_label()),
                },
            );
        };
        let snapshot = service.snapshot();
        if let Some((completed_op, _heads, result)) = &snapshot.last_completed {
            if *completed_op == op_id {
                let Some(mut state) = self.dev_sync_sync.take() else {
                    return false;
                };
                state.worker_settled = true;
                return self.dev_sync_finish_two_phase_with_state(cx, state, result.clone());
            }
        }
        if snapshot.worker_stopped {
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: format!("{}失败：worker 已停止，未拿到授权回执", task.short_label()),
                },
            );
        }
        // 已登记取消（超时/无活动文档/授权失败）：只等真实回执，不再 begin gate。
        if self
            .dev_sync_sync
            .as_ref()
            .is_some_and(|state| state.cancel_requested)
        {
            return false;
        }
        let timed_out = snapshot.deadline_exceeded == Some(op_id);
        let user_cancelled = snapshot.cancel_requested;
        let Some(active) = self.active.as_ref() else {
            if let Some(state) = self.dev_sync_sync.as_mut() {
                state.cancel_requested = true;
                state.pending_error = Some(
                    "同步被取消：NeedGate 时没有活动文档（不会在无会话状态下 append）".to_owned(),
                );
            }
            let _ = service.try_command(crate::dev_sync::Command::Cancel);
            self.dev_sync_panel_task_state(
                cx,
                task,
                true,
                "没有活动文档：取消本次集成",
                Some("同步：授权时没有活动文档，已取消（不 append）".to_owned()),
            );
            return true;
        };
        if timed_out || user_cancelled {
            if let Some(state) = self.dev_sync_sync.as_mut() {
                state.cancel_requested = true;
                state.pending_error = Some(if timed_out {
                    format!(
                        "{}超时：Prepare 完成前已登记取消，未持闸",
                        task.short_label()
                    )
                } else {
                    format!(
                        "{}已取消：NeedGate 时用户取消，未持闸（不 append）",
                        task.short_label()
                    )
                });
            }
            let _ = service.try_command(crate::dev_sync::Command::Cancel);
            self.dev_sync_panel_task_state(
                cx,
                task,
                true,
                if timed_out {
                    "已超时：不持闸、等真实收尾"
                } else {
                    "已取消：不持闸、等真实收尾"
                },
                Some(if timed_out {
                    "同步：操作已超时，未持闸，等 worker 真实收尾".to_owned()
                } else {
                    "同步：已取消，未持闸，等 worker 真实收尾".to_owned()
                }),
            );
            return true;
        }
        let view = active.view.clone();
        match view.update(cx, |view, cx| view.dev_sync_begin_external(cx)) {
            Ok(ticket) => {
                let doc_id = ticket.doc_id().to_owned();
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.target = crate::dev_sync::SyncOnceTarget::TwoPhase {
                        view: Some(view.clone()),
                        doc_id: Some(doc_id.clone()),
                        ticket: Some(ticket),
                    };
                    state.stage = crate::dev_sync::SyncOnceStage::WaitingGate;
                }
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    format!("等待持闸（短暂冻结，{doc_id}）"),
                    Some(format!(
                        "{}：已请求持闸，等待落盘/输入法结束",
                        task.short_label()
                    )),
                );
                true
            }
            Err(error) => {
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.cancel_requested = true;
                    state.pending_error = Some(format!(
                        "{}失败：无法持有外部提交闸门（{error}）；已取消集成（不 append）",
                        task.short_label()
                    ));
                }
                let _ = service.try_command(crate::dev_sync::Command::Cancel);
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    "持闸失败：取消集成",
                    Some(format!(
                        "{}：持闸失败（{error}），已取消",
                        task.short_label()
                    )),
                );
                true
            }
        }
    }

    /// 两阶段的 `WaitingGate`：把闸门驱到 Held → 发 `GrantGate` → 进入 `Integrating`。
    fn dev_sync_advance_two_phase_gate(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((task, op_id, view, doc_id, ticket, epoch)) =
            self.dev_sync_sync.as_ref().and_then(|state| {
                if !state.two_phase || state.stage != crate::dev_sync::SyncOnceStage::WaitingGate {
                    return None;
                }
                let parts = state.target.two_phase_parts()?;
                Some((
                    state.task,
                    state.op_id?,
                    parts.view?,
                    parts.doc_id?,
                    parts.ticket?,
                    state.authorization_epoch?,
                ))
            })
        else {
            return false;
        };
        let Some(service) = self.dev_sync_service(cx) else {
            // 面板被释放：只能 cancel 释放票据，报告失败。
            let _ = view.update(cx, |view, cx| view.dev_sync_cancel_external(&ticket, cx));
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: format!("{}失败：dev-sync 服务已不可用", task.short_label()),
                },
            );
        };
        let snapshot = service.snapshot();
        if let Some((completed_op, _heads, result)) = &snapshot.last_completed {
            if *completed_op == op_id {
                // 授权前就收到真实终态：释放票据（没有 GrantGate，worker 不可能 append）。
                let _ = view.update(cx, |view, cx| view.dev_sync_cancel_external(&ticket, cx));
                let Some(mut state) = self.dev_sync_sync.take() else {
                    return false;
                };
                state.worker_settled = true;
                return self.dev_sync_finish_two_phase_with_state(cx, state, result.clone());
            }
        }
        if snapshot.worker_stopped {
            let _ = view.update(cx, |view, cx| view.dev_sync_cancel_external(&ticket, cx));
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: format!("{}失败：worker 已停止", task.short_label()),
                },
            );
        }
        match view.update(cx, |view, cx| view.dev_sync_poll_external(&ticket, cx)) {
            GateState::WaitingIME | GateState::Flushing => {
                // ADR 0012 S3：自动轮最多等 ~2s 的输入法自然结束/空闲。拿不到就
                // **defer**（不强制结束组合、不 append），把单操作槽让出来稍后再试；
                // 手动轮保持既有语义（继续等 IME 结束，不替用户放弃）。
                let auto_round =
                    self.dev_sync_round_origin == Some(crate::dev_sync::OpOrigin::Auto);
                if !auto_round {
                    return false;
                }
                let now = self
                    .dev_sync_auto
                    .as_ref()
                    .map(crate::dev_sync::AutoSync::now)
                    .unwrap_or_else(std::time::Instant::now);
                let expired = {
                    let Some(auto) = self.dev_sync_auto.as_mut() else {
                        return false;
                    };
                    auto.note_gate_wait(now);
                    auto.gate_wait_remaining(now)
                        .is_some_and(|remaining| remaining.is_zero())
                };
                if !expired {
                    return false;
                }
                // 先释放票据（还没 Held，worker 不可能 append），再取消本轮：
                // worker 在集成前停手，durable inbox 保留，稍后续批。
                let _ = view.update(cx, |view, cx| view.dev_sync_cancel_external(&ticket, cx));
                let _ = service.try_command(crate::dev_sync::Command::Cancel);
                let remaining = crate::dev_sync::autosync::durable_backlog(&self.vault).inbox;
                if let Some(auto) = self.dev_sync_auto.as_mut() {
                    auto.note_gate_deferred(remaining);
                }
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.auto_deferred = true;
                    state.cancel_requested = true;
                    state.lease = None;
                    state.pending_error =
                        Some("自动同步：输入法组合中，稍后再试（未持闸、未追加）".to_owned());
                    state.stage = crate::dev_sync::SyncOnceStage::Cancelling;
                }
                self.dev_sync_auto_deferred = true;
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    "输入法组合中：稍后再试",
                    Some(
                        "自动同步：拿不到输入法空闲（约 2s），本轮已 defer（未持闸、未追加）"
                            .to_owned(),
                    ),
                );
                true
            }
            GateState::Held(lease) => {
                let gate = crate::dev_sync::HeldGate::new(
                    doc_id.clone(),
                    lease.durable_head_seq(),
                    lease.generation().0,
                )
                .with_session_epoch(lease.session_epoch().0);
                match service.grant_gate(op_id, epoch, gate) {
                    Ok(()) => {
                        if let Some(state) = self.dev_sync_sync.as_mut() {
                            state.lease = Some(lease);
                            state.stage = crate::dev_sync::SyncOnceStage::Integrating;
                            state.dispatched_under_gate = true;
                        }
                        self.dev_sync_panel_task_state(
                            cx,
                            task,
                            true,
                            format!("已持闸，整合中（{op_id}）"),
                            Some(format!(
                                "{}：已授权 integrate（持闸仅覆盖集成+交付）",
                                task.short_label()
                            )),
                        );
                        true
                    }
                    Err(error) => {
                        // 授权没生效：worker 绝不会 append；释放闸门并等真实回执。
                        let released =
                            view.update(cx, |view, cx| view.dev_sync_cancel_external(&ticket, cx));
                        let _ = service.try_command(crate::dev_sync::Command::Cancel);
                        let message = match released {
                            Ok(()) => format!("授权失败：{error}（闸门已释放，等真实收尾）"),
                            Err(cancel_error) => format!(
                                "授权失败：{error}；释放闸门也失败（{cancel_error}），编辑器保持冻结"
                            ),
                        };
                        if let Some(state) = self.dev_sync_sync.as_mut() {
                            state.cancel_requested = true;
                            state.pending_error = Some(message.clone());
                            state.lease = None;
                            state.stage = crate::dev_sync::SyncOnceStage::Cancelling;
                        }
                        self.dev_sync_panel_task_state(
                            cx,
                            task,
                            true,
                            "授权失败：等真实收尾",
                            Some(message),
                        );
                        true
                    }
                }
            }
            GateState::Stale => {
                let _ = view.update(cx, |view, cx| view.dev_sync_cancel_external(&ticket, cx));
                let _ = service.try_command(crate::dev_sync::Command::Cancel);
                let message = format!(
                    "{}失败：外部提交票据已失效（闸门没有登记），已取消集成",
                    task.short_label()
                );
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.cancel_requested = true;
                    state.pending_error = Some(message.clone());
                    state.stage = crate::dev_sync::SyncOnceStage::Cancelling;
                }
                self.dev_sync_panel_task_state(cx, task, true, "持闸票据失效：取消", Some(message));
                true
            }
            GateState::Failed(error) => {
                let cancel = view.update(cx, |view, cx| view.dev_sync_cancel_external(&ticket, cx));
                let _ = service.try_command(crate::dev_sync::Command::Cancel);
                let message = match cancel {
                    Ok(()) => format!(
                        "{}失败：持闸失败（{error}），已释放闸门并取消集成",
                        task.short_label()
                    ),
                    Err(cancel_error) => format!(
                        "{}失败：持闸失败（{error}），且释放闸门也失败（{cancel_error}）；\
                         编辑器保持冻结（fail-closed）",
                        task.short_label()
                    ),
                };
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.cancel_requested = true;
                    state.pending_error = Some(message.clone());
                    state.lease = None;
                    state.stage = crate::dev_sync::SyncOnceStage::Cancelling;
                }
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    "持闸失败：等真实收尾",
                    Some(message),
                );
                true
            }
        }
    }

    /// `Integrating`（两阶段）：等 `Integrated`；收到后 `adopt → end → Finish`。
    fn dev_sync_advance_integrating(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((task, op_id)) = self.dev_sync_sync.as_ref().and_then(|state| {
            (state.two_phase && state.stage == crate::dev_sync::SyncOnceStage::Integrating)
                .then(|| state.op_id.map(|op| (state.task, op)))
                .flatten()
        }) else {
            return false;
        };
        let Some(service) = self.dev_sync_service(cx) else {
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_deliver_two_phase_with_state(
                cx,
                state,
                Some(crate::dev_sync::OpResult::Failed {
                    message: format!("{}失败：dev-sync 服务已不可用", task.short_label()),
                }),
            );
        };
        let snapshot = service.snapshot();
        if let Some((completed_op, _heads, result)) = &snapshot.last_completed {
            if *completed_op == op_id {
                let Some(mut state) = self.dev_sync_sync.take() else {
                    return false;
                };
                state.worker_settled = true;
                // 集成可能已经提交：先交付已 durable 的变化、再 end，最后报告真实结果。
                return self.dev_sync_deliver_two_phase_with_state(cx, state, Some(result.clone()));
            }
        }
        if snapshot.worker_stopped {
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_deliver_two_phase_with_state(
                cx,
                state,
                Some(crate::dev_sync::OpResult::Failed {
                    message: format!(
                        "{}失败：worker 已停止（交付已 durable 变化后释放闸门）",
                        task.short_label()
                    ),
                }),
            );
        }
        let integrated = snapshot.two_phase.as_ref().is_some_and(|status| {
            status.op_id == op_id && status.phase == crate::dev_sync::state::SyncPhase::Integrated
        });
        if integrated {
            let Some(mut state) = self.dev_sync_sync.take() else {
                return false;
            };
            if let Some(status) = snapshot.two_phase {
                // ADR 0012 S3：本轮已应用 / 仍 deferred（续批的"有进展"判据）。
                if let Some(auto) = self.dev_sync_auto.as_mut() {
                    auto.note_integrated(status.integrated, status.deferred);
                }
                state.integrated_heads = status.changed_heads;
            }
            return self.dev_sync_deliver_two_phase_with_state(cx, state, None);
        }
        if snapshot.deadline_exceeded == Some(op_id) {
            let first_time = self
                .dev_sync_sync
                .as_ref()
                .is_some_and(|state| !state.deadline_noted);
            if first_time {
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.deadline_noted = true;
                }
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    format!("整合已超时（{op_id}）：保持 Held 等真实回执"),
                    Some(format!(
                        "{}超时：worker 仍可能在外写边界内；闸门保持 Held、不会解闸，\
                         收到真实回执后才交付/释放",
                        task.short_label()
                    )),
                );
                return true;
            }
        }
        false
    }

    /// 两阶段收尾失败后的重试入口：用**已有**的闸门/租约重新交付，不重新 Prepare。
    fn dev_sync_retry_two_phase_delivery(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.dev_sync_sync.take() else {
            return false;
        };
        self.dev_sync_deliver_two_phase_with_state(cx, state, None)
    }

    /// `Finishing`：闸门已释放，等 worker 的 `Completed`（ack/push tail 的真实回执）。
    fn dev_sync_advance_finishing(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((task, op_id)) = self.dev_sync_sync.as_ref().and_then(|state| {
            (state.two_phase && state.stage == crate::dev_sync::SyncOnceStage::Finishing)
                .then(|| state.op_id.map(|op| (state.task, op)))
                .flatten()
        }) else {
            return false;
        };
        let Some(service) = self.dev_sync_service(cx) else {
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: format!("{}失败：dev-sync 服务已不可用", task.short_label()),
                },
            );
        };
        let snapshot = service.snapshot();
        if let Some((completed_op, _heads, result)) = &snapshot.last_completed {
            if *completed_op == op_id {
                let Some(mut state) = self.dev_sync_sync.take() else {
                    return false;
                };
                state.worker_settled = true;
                return self.dev_sync_finish_two_phase_with_state(cx, state, result.clone());
            }
        }
        if snapshot.worker_stopped {
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: format!(
                        "{}失败：worker 已停止，未拿到收尾（ack）回执",
                        task.short_label()
                    ),
                },
            );
        }
        if snapshot.deadline_exceeded == Some(op_id) {
            let first_time = self
                .dev_sync_sync
                .as_ref()
                .is_some_and(|state| !state.deadline_noted);
            if first_time {
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.deadline_noted = true;
                }
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    format!("收尾已超时（{op_id}）：等真实回执"),
                    Some(format!(
                        "{}超时：闸门已释放（正文可编辑），但 operation slot 仍被占着；\
                         等 worker 真实收尾，不派发第二操作",
                        task.short_label()
                    )),
                );
                return true;
            }
        }
        false
    }

    /// `Cancelling`（两阶段）：闸门已释放/从未持有，等 worker 真实回执后收尾。
    fn dev_sync_advance_cancelling(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((task, op_id)) = self.dev_sync_sync.as_ref().and_then(|state| {
            (state.two_phase && state.stage == crate::dev_sync::SyncOnceStage::Cancelling)
                .then(|| state.op_id.map(|op| (state.task, op)))
                .flatten()
        }) else {
            return false;
        };
        let Some(service) = self.dev_sync_service(cx) else {
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: format!("{}失败：dev-sync 服务已不可用", task.short_label()),
                },
            );
        };
        let snapshot = service.snapshot();
        if let Some((completed_op, _heads, result)) = &snapshot.last_completed {
            if *completed_op == op_id {
                let Some(mut state) = self.dev_sync_sync.take() else {
                    return false;
                };
                state.worker_settled = true;
                let pending = state.pending_error.take();
                let result = match pending {
                    Some(message) => crate::dev_sync::OpResult::Failed {
                        message: match result {
                            crate::dev_sync::OpResult::Failed { message: worker } => {
                                format!("{message}（worker：{worker}）")
                            }
                            crate::dev_sync::OpResult::Ok { .. } => message,
                        },
                    },
                    None => result.clone(),
                };
                return self.dev_sync_finish_two_phase_with_state(cx, state, result);
            }
        }
        if snapshot.worker_stopped {
            let Some(state) = self.dev_sync_sync.take() else {
                return false;
            };
            let message = state
                .pending_error
                .clone()
                .unwrap_or_else(|| format!("{}已取消", task.short_label()));
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: format!("{message}（worker 已停止，无终态回执）"),
                },
            );
        }
        false
    }

    /// [`Self::dev_sync_deliver_two_phase`] 的实现（状态已从 `dev_sync_sync` 取出）。
    fn dev_sync_deliver_two_phase_with_state(
        &mut self,
        cx: &mut Context<Self>,
        mut state: crate::dev_sync::ActiveSyncOnce,
        terminal: Option<crate::dev_sync::OpResult>,
    ) -> bool {
        let (view, doc_id) = match state.target.two_phase_parts() {
            Some(parts) => (parts.view, parts.doc_id),
            None => {
                return self.dev_sync_finish_two_phase_with_state(
                    cx,
                    state,
                    terminal.unwrap_or_else(|| crate::dev_sync::OpResult::Failed {
                        message: "同步内部错误：两阶段目标不可用".to_owned(),
                    }),
                );
            }
        };
        let (Some(view), Some(doc_id)) = (view, doc_id) else {
            // 还没持闸：没有可交付的会话（NeedGate/Cancelling 路径）。
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                terminal.unwrap_or_else(|| crate::dev_sync::OpResult::Failed {
                    message: "同步内部错误：两阶段交付缺少活动文档".to_owned(),
                }),
            );
        };
        let Some(lease) = state.lease.clone() else {
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                terminal.unwrap_or_else(|| crate::dev_sync::OpResult::Failed {
                    message: "同步内部错误：两阶段交付缺少租约".to_owned(),
                }),
            );
        };

        // 0) worker 是否上报过"结构投影失败"：结构 CRDT 已 durable，但本地 tree/notes
        //    没追上。先照常交付正文，随后**保持写禁用**（见下面的投影复核）。
        if let Some(crate::dev_sync::OpResult::Failed { message }) = terminal.as_ref() {
            if message.contains(crate::dev_sync::sync_once::PROJECTION_FAILURE_MARKER) {
                state.projection_failed = true;
            }
        }

        // 1) 已 durable 的外部变化先交付进 live（失败不改头，但也绝不丢弃闸门）。
        let mut changes = state.delivered_changes;
        let mut delivery_error: Option<String> = None;
        let mut heads = AppliedHeads::default();
        match AppliedHeads::load(&self.vault, &doc_id) {
            Ok(loaded) => heads = loaded,
            Err(error) => delivery_error = Some(format!("读取库中 head 失败：{error}")),
        }
        if delivery_error.is_none() && heads.head_seq > lease.durable_head_seq() {
            match prepare_committed_delta(&self.vault, &doc_id, lease.durable_head_seq()) {
                Ok(prepared) => {
                    changes = changes.saturating_add(prepared.updates.len());
                    if let Err(error) = view.update(cx, |view, cx| {
                        view.dev_sync_adopt_external(&lease, prepared, cx)
                    }) {
                        delivery_error = Some(format!("交付已落盘变化失败：{error}"));
                    }
                }
                Err(error) => {
                    delivery_error = Some(format!("读取已落盘变化失败：{error}"));
                }
            }
        }

        // 2) 释放闸门（end 会补齐 Held 期间新落库的行；applied 必须是库中当前 head）。
        if delivery_error.is_none() {
            if let Err(error) = view.update(cx, |view, cx| {
                view.dev_sync_end_external(lease.clone(), heads, cx)
            }) {
                delivery_error = Some(format!("释放闸门失败：{error}"));
            }
        }

        // 3) 视图泵：adopt 的 Remote → 镜像正文（不重开文档）。
        let _ = view.update(cx, |view, cx| view.dev_sync_pump(cx));

        if let Some(error) = delivery_error {
            // fail-closed：闸门仍 Held，留给用户显式重试收尾。
            state.stage = crate::dev_sync::SyncOnceStage::Delivering;
            state.delivered_changes = changes;
            state.pending_error = Some(error.clone());
            let task = state.task;
            let dispatched = state.dispatched_under_gate;
            self.dev_sync_sync = Some(state);
            let message = format!(
                "{}收尾未完成：{error}；闸门仍持有（编辑器保持冻结），可再次点「{}」重试收尾",
                task.short_label(),
                task.short_label()
            );
            self.dev_sync_finish_report(
                cx,
                ReportParts {
                    task,
                    doc_id: Some(doc_id),
                    dispatched_under_gate: dispatched,
                    changes,
                    error: Some(message),
                    ..ReportParts::default()
                }
                .report(),
            );
            return true;
        }

        // 2b) 结构投影失败 → 保持写禁用（fail-closed）：正文已交付，但本地侧栏投影
        //     还没有追上，不能 `end` 解闸、也不能假装刷新。用户再点「同步」会重试收尾
        //     （先重新投影，成功后才 end/finish）。
        if state.projection_failed {
            let head_changed = load_head(&self.vault, STRUCTURE_DOC_ID)
                .map(|head| head.head_seq != state.structure_base_seq)
                .unwrap_or(true);
            let projected = if head_changed {
                self.dev_sync_refresh_structure_if_needed(cx, state.structure_base_seq)
            } else {
                true
            };
            if projected {
                state.projection_failed = false;
            } else {
                let task = state.task;
                let message = format!(
                    "{}收尾未完成：结构投影失败（已交付 {changes} 条已落盘正文变化）；\
                     闸门仍持有（编辑器保持冻结），可再次点「{}」重试收尾",
                    task.short_label(),
                    task.short_label()
                );
                state.stage = crate::dev_sync::SyncOnceStage::Delivering;
                state.delivered_changes = changes;
                state.pending_error = Some(message.clone());
                self.dev_sync_sync = Some(state);
                self.dev_sync_finish_report(
                    cx,
                    ReportParts {
                        task,
                        doc_id: Some(doc_id),
                        dispatched_under_gate: true,
                        changes,
                        error: Some(message),
                        ..ReportParts::default()
                    }
                    .report(),
                );
                return true;
            }
        }

        state.delivered_changes = changes;
        state.lease = None;
        state.target = crate::dev_sync::SyncOnceTarget::TwoPhase {
            view: Some(view),
            doc_id: Some(doc_id),
            ticket: None,
        };

        if let Some(result) = terminal {
            // worker 已回真实终态：交付完成后照实报告（成功/失败都不再发 Finish）。
            return self.dev_sync_finish_two_phase_with_state(cx, state, result);
        }

        // 4) 正常路径：闸门已释放，授权 worker 做 push tail + ack。
        if state.finish_sent {
            self.dev_sync_sync = Some(state);
            return false;
        }
        let op_id = state.op_id;
        let Some(service) = self.dev_sync_service(cx) else {
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: "收尾失败：dev-sync 服务已不可用（闸门已释放）".to_owned(),
                },
            );
        };
        let Some(op_id) = op_id else {
            return self.dev_sync_finish_two_phase_with_state(
                cx,
                state,
                crate::dev_sync::OpResult::Failed {
                    message: "收尾失败：两阶段操作缺少 op id（内部错误）".to_owned(),
                },
            );
        };
        match service.finish_op(op_id) {
            Ok(()) => {
                state.finish_sent = true;
                state.stage = crate::dev_sync::SyncOnceStage::Finishing;
                let task = state.task;
                self.dev_sync_sync = Some(state);
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    format!("已解闸，收尾中（{op_id}）"),
                    Some(format!(
                        "{}：已交付 {} 条变化并释放闸门，等待 ack",
                        task.short_label(),
                        changes
                    )),
                );
                true
            }
            Err(error) => {
                // 可能 worker 已经先回了真实终态（例如取消竞争）：以终态为准。
                let snapshot = service.snapshot();
                if let Some((completed_op, _heads, result)) = &snapshot.last_completed {
                    if *completed_op == op_id {
                        let mut state = state;
                        state.worker_settled = true;
                        return self.dev_sync_finish_two_phase_with_state(
                            cx,
                            state,
                            result.clone(),
                        );
                    }
                }
                // 投递失败时发一条 Cancel 解开 worker 的 Finish 等待（不再需要授权收尾），
                // 让它回到真实终态；否则单操作 slot 会一直占着。
                let _ = service.try_command(crate::dev_sync::Command::Cancel);
                state.pending_error = Some(format!("收尾授权失败：{error}（闸门已释放）"));
                state.stage = crate::dev_sync::SyncOnceStage::Finishing;
                let task = state.task;
                self.dev_sync_sync = Some(state);
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    "闸门已释放，收尾授权失败",
                    Some(format!(
                        "{}：交付完成并解闸，但 Finish 发不出去（{error}）；等 worker 停止/真实回执",
                        task.short_label()
                    )),
                );
                true
            }
        }
    }

    /// 两阶段最终报告（不碰闸门：调用方已经交付/释放，或从未持闸）。
    fn dev_sync_finish_two_phase_with_state(
        &mut self,
        cx: &mut Context<Self>,
        state: crate::dev_sync::ActiveSyncOnce,
        result: crate::dev_sync::OpResult,
    ) -> bool {
        let structure_refreshed =
            self.dev_sync_refresh_structure_if_needed(cx, state.structure_base_seq);
        let (error, worker_summary) = match result {
            crate::dev_sync::OpResult::Ok { summary } => (None, Some(summary)),
            crate::dev_sync::OpResult::Failed { message } => (Some(message), None),
        };
        let error = match (error, state.pending_error.clone()) {
            (Some(message), Some(pending)) if !message.contains(&pending) => {
                Some(format!("{pending}；{message}"))
            }
            (Some(message), _) => Some(message),
            (None, Some(pending)) => Some(pending),
            (None, None) => None,
        };
        self.dev_sync_finish_report(
            cx,
            ReportParts {
                task: state.task,
                doc_id: state.document_id().map(str::to_owned),
                dispatched_under_gate: state.dispatched_under_gate,
                changes: state.delivered_changes,
                worker_summary,
                error,
                structure_refreshed,
                ..ReportParts::default()
            }
            .report(),
        );
        true
    }

    /// `WaitingGate`：把闸门驱到 Held，然后**只派发一次** `SyncOnce`。
    ///
    /// 只有持闸路径会进这个阶段（无会话路径派发即 `WorkerRunning`），所以这里能从
    /// [`crate::dev_sync::SyncOnceTarget::Held`] 取出活动文档/视图/票据。
    fn dev_sync_advance_gate(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((
            task,
            crate::dev_sync::SyncOnceTarget::Held {
                view,
                ticket,
                doc_id,
            },
        )) = self
            .dev_sync_sync
            .as_ref()
            .map(|state| (state.task, state.target.clone()))
        else {
            return false;
        };
        // 命令从在途状态构造（轮换要带恢复码/revoke；同步/发布是无参命令）。
        let Some(command) = self
            .dev_sync_sync
            .as_ref()
            .and_then(crate::dev_sync::ActiveSyncOnce::pending_command)
        else {
            // 参数缺失是内部错误：绝不能盲派发（会退化成一条无参数的轮换）。
            self.dev_sync_sync = None;
            self.dev_sync_finish_report(
                cx,
                ReportParts {
                    task,
                    doc_id: Some(doc_id),
                    error: Some(format!(
                        "{}失败：在途任务缺少派发参数（内部错误，未派发任何命令）",
                        task.short_label()
                    )),
                    ..ReportParts::default()
                }
                .report(),
            );
            return true;
        };
        match view.update(cx, |view, cx| view.dev_sync_poll_external(&ticket, cx)) {
            GateState::WaitingIME | GateState::Flushing => {
                // 等 IME 自然结束 / flush 到明确 durable：不改状态，下一帧继续 poll。
                false
            }
            GateState::Held(lease) => {
                let Some(service) = self.dev_sync_service(cx) else {
                    return self.dev_sync_finish(
                        cx,
                        SyncOnceFinish::Failed(format!(
                            "{}失败：dev-sync 服务已不可用（面板被释放？）",
                            task.short_label()
                        )),
                    );
                };
                let gate = crate::dev_sync::HeldGate::new(
                    doc_id.clone(),
                    lease.durable_head_seq(),
                    lease.generation().0,
                );
                // 发布基线/轮换/apply 也走这条路径：命令不同，持闸证明完全相同。
                match service.try_command_with_gate(command, gate) {
                    Ok(op_id) => {
                        if let Some(state) = self.dev_sync_sync.as_mut() {
                            state.lease = Some(lease);
                            state.op_id = Some(op_id);
                            state.stage = crate::dev_sync::SyncOnceStage::WorkerRunning;
                            state.dispatched_under_gate = true;
                        }
                        self.dev_sync_panel_task_state(
                            cx,
                            task,
                            true,
                            format!("已持闸，worker {}中（{op_id}）", task.label()),
                            Some(format!(
                                "{}：已持闸，worker {}中",
                                task.short_label(),
                                task.label()
                            )),
                        );
                        true
                    }
                    Err(busy) if busy.reason == crate::dev_sync::BusyReason::Cancelling => {
                        // 单操作位被一个**已超时但还没真实收尾**的 op 占着：绝不能派发第二条，
                        // 也绝不能解闸（那会与仍在集成的 worker 竞争）。保持 Held 等收尾。
                        if let Some(state) = self.dev_sync_sync.as_mut() {
                            state.lease = Some(lease);
                            state.stage = crate::dev_sync::SyncOnceStage::Delivering;
                            state.worker_settled = false;
                        }
                        let message = format!(
                            "{}被拒绝：上一轮已超时、仍在等待 worker 真实收尾（{}）；\
                             闸门保持持有（ReconcileRequired），不会派发第二操作",
                            task.short_label(),
                            busy.reason.label()
                        );
                        self.dev_sync_finish_report(
                            cx,
                            ReportParts {
                                task,
                                doc_id: Some(doc_id),
                                dispatched_under_gate: false,
                                error: Some(message),
                                ..ReportParts::default()
                            }
                            .report(),
                        );
                        true
                    }
                    Err(busy) => {
                        // 闸门已经 Held，但命令发不出去：仍然要先交付/释放，不能留下冻结的编辑器。
                        // 这里没有派发任何 op（`op_id = None`），所以解闸是安全的。
                        if let Some(state) = self.dev_sync_sync.as_mut() {
                            state.lease = Some(lease);
                            state.stage = crate::dev_sync::SyncOnceStage::Delivering;
                        }
                        self.dev_sync_finish(
                            cx,
                            SyncOnceFinish::Failed(format!(
                                "{}失败：派发命令被拒绝（{}）",
                                task.short_label(),
                                busy.reason.label()
                            )),
                        )
                    }
                }
            }
            GateState::Stale => {
                let message = format!(
                    "{}失败：外部提交票据已失效（闸门没有登记）",
                    task.short_label()
                );
                self.dev_sync_sync = None;
                self.dev_sync_finish_report(
                    cx,
                    ReportParts {
                        task,
                        doc_id: Some(doc_id),
                        error: Some(message),
                        ..ReportParts::default()
                    }
                    .report(),
                );
                true
            }
            GateState::Failed(error) => {
                // 闸门不会静默释放：用 cancel 显式释放（cancel 会先把已 durable 变化交付进 live）。
                let cancel = view.update(cx, |view, cx| view.dev_sync_cancel_external(&ticket, cx));
                let message = match cancel {
                    Ok(_) => format!("{}失败：持闸失败（{error}）", task.short_label()),
                    Err(cancel_error) => format!(
                        "{}失败：持闸失败（{error}），且释放闸门也失败（{cancel_error}）；\
                         编辑器保持冻结",
                        task.short_label()
                    ),
                };
                self.dev_sync_sync = None;
                self.dev_sync_finish_report(
                    cx,
                    ReportParts {
                        task,
                        doc_id: Some(doc_id),
                        error: Some(message),
                        ..ReportParts::default()
                    }
                    .report(),
                );
                true
            }
        }
    }

    /// `WorkerRunning`：等 worker 的终态（成功或失败），然后收尾。
    ///
    /// 观察点是 [`SyncService::snapshot`] 里的 `last_completed`（服务端保留、不被消费），
    /// 因此面板自己的 40 ms 事件泵不会把这条事件"吃掉"。
    fn dev_sync_advance_worker(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((task, op_id, no_session, doc_id)) =
            self.dev_sync_sync.as_ref().and_then(|state| {
                state.op_id.map(|op| {
                    (
                        state.task,
                        op,
                        state.is_no_session(),
                        state.document_id().map(str::to_owned),
                    )
                })
            })
        else {
            return false;
        };
        let Some(service) = self.dev_sync_service(cx) else {
            // 面板被释放（窗口关闭）：只能报告，不假装成功。
            self.dev_sync_sync = None;
            self.dev_sync_finish_report(
                cx,
                ReportParts {
                    task,
                    doc_id,
                    no_session,
                    // 无会话路径从来没有持闸证明；有会话路径的命令确实在 Held 下派发过。
                    dispatched_under_gate: !no_session,
                    error: Some(format!(
                        "{}失败：dev-sync 服务已不可用（面板被释放）",
                        task.short_label()
                    )),
                    ..ReportParts::default()
                }
                .report(),
            );
            return true;
        };
        let snapshot = service.snapshot();
        if let Some((completed_op, _heads, result)) = &snapshot.last_completed {
            if *completed_op == op_id {
                // 真实 `Completed` = worker 已结束本轮外部写：这是允许解闸的可靠回执。
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.worker_settled = true;
                    // 轮换/apply 的公开事实（事件先于 Completed 入状态）：只接受**本次 op**
                    // 的事实，避免把上一轮成功轮换的 epoch 当成这一轮的结果。
                    state.rotation_facts = snapshot
                        .last_rotation
                        .clone()
                        .filter(|_| snapshot.last_rotation_op == Some(op_id));
                }
                return match result.clone() {
                    crate::dev_sync::OpResult::Ok { summary } => {
                        self.dev_sync_finish(cx, SyncOnceFinish::Ok(summary))
                    }
                    crate::dev_sync::OpResult::Failed { message } => {
                        self.dev_sync_finish(cx, SyncOnceFinish::Failed(message))
                    }
                };
            }
        }
        if snapshot.worker_stopped {
            // `Stopped` 回执 = worker 已退出，不会再有未来外部写：同样允许解闸。
            if let Some(state) = self.dev_sync_sync.as_mut() {
                state.worker_settled = true;
            }
            return self.dev_sync_finish(
                cx,
                SyncOnceFinish::Failed(format!(
                    "{}失败：worker 已停止，未拿到本次任务的明确终态",
                    task.short_label()
                )),
            );
        }
        // 看门狗只把 op 标成 `Cancelling`（ADR 0012 S1）：**保持 Held**、不派发第二条，
        // 等真实终态。这里只把这件事写进面板一次（不每帧重复通知）。
        if snapshot.deadline_exceeded == Some(op_id) {
            let first_time = self
                .dev_sync_sync
                .as_ref()
                .is_some_and(|state| !state.deadline_noted);
            if first_time {
                if let Some(state) = self.dev_sync_sync.as_mut() {
                    state.deadline_noted = true;
                }
                let label = task.short_label();
                self.dev_sync_panel_task_state(
                    cx,
                    task,
                    true,
                    format!("已超时（{op_id}）：等待 worker 真实收尾"),
                    Some(format!(
                        "{label}超时：worker 仍可能在外写边界内，闸门保持持有，不会派发第二操作、\
                         也不会解闸；收到真实回执后才收尾（若长期无回执请重启实例）"
                    )),
                );
                return true;
            }
        }
        false
    }

    /// 收尾：按"有没有活动文档"分叉（见 [`crate::dev_sync::SyncOnceTarget`]）。
    ///
    /// - **持闸路径**（有活动文档）：交付已 durable 的变化 → `end` 释放闸门 → 泵视图 →
    ///   按需重投影结构树（本函数内联的原有语义）；
    /// - **无会话路径**（没有活动文档）：没有闸门/租约/adopt，只重投影结构树 + 自动打开
    ///   第一篇（[`Self::dev_sync_finish_no_session`]）。
    ///
    /// 失败语义（ADR 0009 切片 3a/4c-2）：
    /// - 已 durable 的外部行**先交付**再释放（不丢数据）；
    /// - 交付/释放本身失败 → **保持 Held**（不静默解冻），阶段留在 `Delivering`，
    ///   用户再点「同步」会重试收尾；
    /// - 成功释放后才写最终报告（失败时正文不变、闸门无残留）；
    /// - 无会话路径没有闸门可残留：失败只保留错误并释放 busy 状态。
    fn dev_sync_finish(&mut self, cx: &mut Context<Self>, finish: SyncOnceFinish) -> bool {
        let Some(mut state) = self.dev_sync_sync.take() else {
            return false;
        };
        // 两阶段有自己的交付/收尾路径（见 `dev_sync_deliver_two_phase_with_state`）：
        // 旧路径不能把它当成 `Held` 目标处理。
        if state.two_phase {
            let result = match finish {
                SyncOnceFinish::Ok(summary) => crate::dev_sync::OpResult::Ok { summary },
                SyncOnceFinish::Failed(message) => crate::dev_sync::OpResult::Failed { message },
            };
            return self.dev_sync_deliver_two_phase_with_state(cx, state, Some(result));
        }
        // 无会话路径：没有闸门/租约/adopt，只按结构 head 重投影 + 自动打开第一篇
        //（结构→正文依赖已由 worker 的 Integrate 收敛，UI 不再补轮）。
        if state.is_no_session() {
            return self.dev_sync_finish_no_session(cx, state, finish);
        }
        let crate::dev_sync::SyncOnceTarget::Held {
            doc_id,
            view,
            ticket,
        } = state.target.clone()
        else {
            // `is_no_session()` 已经拦下另一条路径：走到这里说明目标被换过（内部错误）。
            self.dev_sync_finish_report(
                cx,
                ReportParts {
                    task: state.task,
                    error: Some("同步内部错误：在途同步的目标不可用".to_owned()),
                    ..ReportParts::default()
                }
                .report(),
            );
            return true;
        };
        let Some(lease) = state.lease.clone() else {
            // 还没 Held 就退出（Stale/Failed/派发前失败）：只能 cancel 释放票据。
            let cancel = view.update(cx, |view, cx| view.dev_sync_cancel_external(&ticket, cx));
            let message = match finish {
                SyncOnceFinish::Ok(summary) => {
                    format!("{}结束（未进入 Held）：{summary}", state.task.short_label())
                }
                SyncOnceFinish::Failed(message) => message,
            };
            // 票据释放失败要如实写进报告：否则下一次 begin 会拿到 AlreadyHeld，用户不知道为什么。
            let message = match cancel {
                Ok(_) => message,
                Err(error) => format!("{message}（释放闸门票据失败：{error}）"),
            };
            self.dev_sync_finish_report(
                cx,
                ReportParts {
                    task: state.task,
                    doc_id: Some(doc_id),
                    error: Some(message),
                    ..ReportParts::default()
                }
                .report(),
            );
            return true;
        };

        // 0) fail-closed 守卫（ADR 0012 S1）：没有真实收尾回执就**绝不**解闸。
        //
        //    看门狗只产生 `Cancelling`，不会伪造可解闸的终态；只有收到真实 `Completed`
        //    （含失败）或 `Stopped`，或本次任务根本没派发过 op，才允许继续交付/`end`。
        //    不满足时保持 Held，并明确要求对账（重启实例或显式重试收尾）。
        let receipt_ok =
            crate::dev_sync::sync_once::release_gate_allowed(state.op_id, state.worker_settled)
                && (state.op_id.is_some() || self.dev_sync_no_unsettled_worker_op(cx));
        if !receipt_ok {
            let task = state.task;
            let dispatched = state.dispatched_under_gate;
            let op_id = state.op_id;
            state.stage = crate::dev_sync::SyncOnceStage::Delivering;
            self.dev_sync_sync = Some(state);
            let op = op_id.map_or_else(|| "未知".to_owned(), |op| op.to_string());
            let message = format!(
                "{}收尾被拒绝：未收到 worker 真实完成/停止回执（{op}）；\
                 闸门保持持有（ReconcileRequired），请联系对账：重启实例或显式重试收尾",
                task.short_label()
            );
            self.dev_sync_finish_report(
                cx,
                ReportParts {
                    task,
                    doc_id: Some(doc_id),
                    dispatched_under_gate: dispatched,
                    error: Some(message),
                    ..ReportParts::default()
                }
                .report(),
            );
            return true;
        }

        // 1) 已 durable 的外部变化先交付（正常路径 = prepare → adopt；失败路径同样不丢）。
        let mut changes = 0usize;
        let mut delivery_error: Option<String> = None;
        let mut heads = AppliedHeads::default();
        match AppliedHeads::load(&self.vault, &doc_id) {
            Ok(loaded) => heads = loaded,
            Err(error) => {
                delivery_error = Some(format!("读取库中 head 失败：{error}"));
            }
        }
        if delivery_error.is_none() && heads.head_seq > lease.durable_head_seq() {
            match prepare_committed_delta(&self.vault, &doc_id, lease.durable_head_seq()) {
                Ok(prepared) => {
                    changes = prepared.updates.len();
                    if let Err(error) = view.update(cx, |view, cx| {
                        view.dev_sync_adopt_external(&lease, prepared, cx)
                    }) {
                        delivery_error = Some(format!("交付已落盘变化失败：{error}"));
                    }
                }
                Err(error) => {
                    delivery_error = Some(format!("读取已落盘变化失败：{error}"));
                }
            }
        }

        // 2) 释放闸门（end 会补齐 Held 期间新落库的行；`applied` 必须是库中当前 head）。
        if delivery_error.is_none() {
            if let Err(error) =
                view.update(cx, |view, cx| view.dev_sync_end_external(lease, heads, cx))
            {
                delivery_error = Some(format!("释放闸门失败：{error}"));
            }
        }

        // 3) 视图泵：adopt 的 `Remote` → 镜像正文（不重开文档）。
        let _ = view.update(cx, |view, cx| view.dev_sync_pump(cx));

        // 4) 收尾失败：保持 Held（fail-closed），留给用户重试。
        if let Some(error) = delivery_error {
            state.stage = crate::dev_sync::SyncOnceStage::Delivering;
            let task = state.task;
            let dispatched = state.dispatched_under_gate;
            self.dev_sync_sync = Some(state);
            let message = format!(
                "{}收尾未完成：{error}；闸门仍持有（编辑器保持冻结），可再次点「{}」重试收尾",
                task.short_label(),
                task.short_label()
            );
            self.dev_sync_finish_report(
                cx,
                ReportParts {
                    task,
                    doc_id: Some(doc_id),
                    dispatched_under_gate: dispatched,
                    changes,
                    error: Some(message),
                    ..ReportParts::default()
                }
                .report(),
            );
            return true;
        }

        // 5) 轮换/apply 成功（切片 6a）：先把 `SyncContext.key_epoch` 切到新 epoch，
        //    再按需重投影结构，最后重建活动视图。
        //
        //    顺序理由：新视图的 `SessionDriver`/结构写入都从 `self.sync` 取 epoch；先换
        //    epoch 才不会让重开后的第一次 flush 写出旧 epoch 的 intent。视图重建放在
        //    结构重投影之后，才能按最新树定位同一篇笔记。
        let succeeded = matches!(&finish, SyncOnceFinish::Ok(_));
        let rotation_facts = state.rotation_facts.clone();
        let mut rotation_advanced = false;
        let mut rotation_error: Option<String> = None;
        if state.task.is_rotation() && succeeded {
            match &rotation_facts {
                Some(facts) => {
                    if let Some(to_epoch) = facts.to_epoch {
                        if let Some(context) = self.sync {
                            self.sync = Some(SyncContext::new(context.peer_id, to_epoch));
                        }
                    } else {
                        rotation_error = Some(format!(
                            "{}已完成但没有拿到新 epoch（内部错误）：保持旧视图，请重启实例",
                            state.task.short_label()
                        ));
                    }
                    rotation_advanced = facts.advanced();
                }
                None => {
                    rotation_error = Some(format!(
                        "{}已完成但缺少公开事实（内部错误）：保持旧视图，请重启实例",
                        state.task.short_label()
                    ));
                }
            }
        }

        // 6) 结构树：只有结构 head 真的变了才重放 + 重投影 + 刷新侧栏（不假刷新）。
        let structure_refreshed =
            self.dev_sync_refresh_structure_if_needed(cx, state.structure_base_seq);

        // 7) 重建活动视图（旧 `EditorView`/`SessionDriver` 属于旧 epoch；新意图必须带新
        //    epoch）。失败不吞：身份已推广，必须如实报告（旧视图已关闭，请重开/重启）。
        if rotation_advanced && rotation_error.is_none() {
            if let Err(error) = self.dev_sync_rebuild_after_rotation(cx, &doc_id) {
                rotation_error = Some(format!(
                    "{}已完成（epoch 已切换），但重建活动视图失败：{error}",
                    state.task.short_label()
                ));
            }
        }

        // 8) 最终报告 + 面板文案。
        let (mut error, worker_summary) = match finish {
            SyncOnceFinish::Ok(summary) => (None, Some(summary)),
            SyncOnceFinish::Failed(message) => (Some(message), None),
        };
        if let Some(rotation_error) = rotation_error {
            error = Some(match error {
                Some(message) => format!("{message}；{rotation_error}"),
                None => rotation_error,
            });
        }
        self.dev_sync_finish_report(
            cx,
            ReportParts {
                task: state.task,
                doc_id: Some(doc_id),
                dispatched_under_gate: state.dispatched_under_gate,
                changes,
                worker_summary,
                error,
                structure_refreshed,
                rotation: rotation_facts,
                ..ReportParts::default()
            }
            .report(),
        );
        true
    }

    /// 轮换/apply 成功后重建活动视图：关掉旧 `EditorView`/`SessionDriver`，用**新身份
    /// epoch/root** 重新 `open_entry`，让新意图（正文 flush + 结构提交）带新 epoch。
    ///
    /// 只在 worker 成功、闸门已释放、`self.sync.key_epoch` 已切到新 epoch 后调用。
    ///
    /// 失败语义：不再回退旧视图（旧 session 属于旧 epoch，回退就等于让新写入用旧根），
    /// 直接关掉它并明确报错（调用方把错误写进报告/面板）；用户可重新打开笔记或重启实例。
    fn dev_sync_rebuild_after_rotation(
        &mut self,
        cx: &mut Context<Self>,
        doc_id: &str,
    ) -> Result<(), String> {
        let Some(old) = self.active.take() else {
            // 没有活动视图就没有"用新 epoch 重开"的对象（本片轮换要求有活动文档）。
            return Ok(());
        };
        let old_node = old.node_id.clone();
        // 优先按 node_id 找回同一篇；树被重投影后 node 仍在，但 body doc id 是兜底。
        let index = self
            .tree
            .iter()
            .position(|entry| entry.node_id == old_node)
            .or_else(|| {
                self.tree
                    .iter()
                    .position(|entry| entry.body_doc_id.as_deref() == Some(doc_id))
            })
            .or_else(|| self.tree.iter().position(TreeEntry::is_note));
        let Some(index) = index else {
            let report = old
                .view
                .update(cx, |view, _| view.drain_and_close(SWITCH_CLOSE_TIMEOUT));
            eprintln!(
                "[menghunote] 轮换后结构树里没有可重开的笔记：已关闭旧 epoch 视图（durable={} timed_out={}）",
                report.durable.is_some(),
                report.timed_out
            );
            self.refresh_tree();
            return Err("结构树里没有可重新打开的笔记（旧 epoch 视图已关闭）".to_owned());
        };
        let expected_node = self.tree[index].node_id.clone();
        // `self.active` 已是 None：`open_entry` 不会去排空旧视图（我们自己关）。
        self.selected_node_id = Some(expected_node.clone());
        self.open_entry(index, cx);
        // 旧视图代表旧 epoch：无论如何都要关掉（它没有未落盘的本地编辑——闸门在 Held
        // 之前已经 flush 到 durable，end 也只补 Held 期间的新行）。
        let report = old
            .view
            .update(cx, |view, _| view.drain_and_close(SWITCH_CLOSE_TIMEOUT));
        if report.timed_out {
            eprintln!(
                "[menghunote] 轮换后关闭旧视图超时（durable={}）：旧 epoch session 可能仍有后台任务",
                report.durable.is_some()
            );
        }
        if self.active.as_ref().is_some_and(|active| {
            active.node_id == expected_node && active.view.read(cx).dev_sync_doc_id() == doc_id
        }) {
            eprintln!(
                "[menghunote] 轮换后已重建活动视图（新 epoch；node={expected_node} doc={doc_id}）"
            );
            Ok(())
        } else {
            self.refresh_tree();
            Err("重新打开活动文档失败（旧 epoch 视图已关闭；详见 stderr）".to_owned())
        }
    }

    /// 无会话同步的收尾（没有活动文档）：重投影结构树 → 自动打开第一篇。
    ///
    /// 没有闸门、没有租约、没有 adopt，所以这里只有两步：
    ///
    /// 1. 结构 head 变了才重放 + 重投影 + `refresh_tree`（`dev_sync_refresh_structure_if_needed`
    ///    是幂等的；没变就**不碰**它——不假刷新）；
    /// 2. 成功且当前仍没有活动文档时，自动打开树里的第一篇笔记（失败只记日志，不 panic）。
    ///
    /// **不再补一轮网络同步**（ADR 0012 S2b）：结构→正文的依赖已在 worker 的 Integrate
    /// 阶段内收敛（结构先集成 → 只投影建立 body 行 → 正文公平续批/同轮重试 deferred），
    /// 所以一次点击就同时拿到结构与正文。UI 只保留结构 head 变化时的侧栏刷新。
    ///
    /// 失败时也刷新树（integrate 可能已经追加了部分结构行），但不替用户打开文档。
    fn dev_sync_finish_no_session(
        &mut self,
        cx: &mut Context<Self>,
        state: crate::dev_sync::ActiveSyncOnce,
        finish: SyncOnceFinish,
    ) -> bool {
        // 1) 结构树：head 变了才重放 + 重投影 + 刷新侧栏（不假刷新）。
        let structure_refreshed =
            self.dev_sync_refresh_structure_if_needed(cx, state.structure_base_seq);
        // 2) 树里出现笔记且当前没有活动文档 → 自动打开第一篇（"空库同步完就能直接写"）。
        let succeeded = matches!(finish, SyncOnceFinish::Ok(_));
        let opened_note = if succeeded {
            self.open_first_note_after_sync(cx)
        } else {
            None
        };
        // 3) 最终报告 + 面板文案（失败只保留错误，不再冻结任何后续操作）。
        let (error, worker_summary) = match finish {
            SyncOnceFinish::Ok(summary) => (None, Some(summary)),
            SyncOnceFinish::Failed(message) => (Some(message), None),
        };
        self.dev_sync_finish_report(
            cx,
            ReportParts {
                doc_id: None,
                no_session: true,
                changes: 0,
                worker_summary,
                error,
                structure_refreshed,
                opened_note,
                ..ReportParts::default()
            }
            .report(),
        );
        true
    }

    /// 同步结束后自动打开树里的第一篇笔记（只在没有活动文档时调用）。
    ///
    /// 返回被打开的标题；树里没有笔记 / 打开失败返回 `None`（失败只记日志，不 panic）。
    /// 打开成功时同步把侧栏选中态指到它，`open_entry` 自己负责建会话、读库并刷新树。
    fn open_first_note_after_sync(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let index = self.tree.iter().position(TreeEntry::is_note)?;
        let title = self.tree[index].title.clone();
        let node_id = self.tree[index].node_id.clone();
        self.selected_node_id = Some(node_id.clone());
        self.open_entry(index, cx);
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.node_id == node_id)
        {
            eprintln!("[menghunote] 同步完成后自动打开第一篇笔记「{title}」");
            Some(title)
        } else {
            eprintln!("[menghunote] 同步完成后自动打开「{title}」失败（详见 stderr）");
            None
        }
    }

    /// 写最终报告并更新面板两行（同步行 + 最近操作）。
    fn dev_sync_finish_report(
        &mut self,
        cx: &mut Context<Self>,
        report: crate::dev_sync::SyncOnceReport,
    ) -> bool {
        // 失败但闸门仍 Held（收尾没完成）时不能显示"闸门已释放"——那是假话。
        let gate_still_held = self.dev_sync_sync.is_some();
        let task = report.task;
        let line = if report.is_ok() {
            if report.no_session {
                // 无会话路径没有"交付 N 条变化"这回事，别照抄持闸路径的措辞。
                "完成（无会话同步）".to_owned()
            } else if task == crate::dev_sync::GatedTask::PublishBaseline {
                "初始基线已发布".to_owned()
            } else if task == crate::dev_sync::GatedTask::Rotate {
                "轮换完成（epoch 已切换）".to_owned()
            } else if task == crate::dev_sync::GatedTask::ApplyEpoch {
                "已应用新 epoch".to_owned()
            } else {
                format!("完成（{} 条变化）", report.changes)
            }
        } else if gate_still_held {
            "收尾未完成（闸门仍持有）".to_owned()
        } else if report.no_session {
            "失败（无会话同步）".to_owned()
        } else {
            format!("{}失败（闸门已释放）", task.short_label())
        };
        let notice = report.notice.clone();
        let ok = report.is_ok();
        self.dev_sync_last_stage = None;
        eprintln!(
            "[menghunote] 同步收尾：task={task:?} ok={ok} changes={} error={:?}",
            report.changes, report.error
        );
        // 面板行先写（此时自动轮的来源标记还在，面板显示「自动同步：…」），
        // 再交回调度器记账（退避/续批/状态行）；发布基线不参与。
        self.dev_sync_panel_task_state(cx, task, gate_still_held, line, Some(notice));
        self.dev_sync_auto_finish_round(&report);
        self.dev_sync_last_report = Some(report);
        // ADR 0010 切片 2a-2：同步成功后自动刷新一次设备列表（`auth_rev` 可能已被别的
        // 管理设备推进）。只置待办、由面板的 40 ms 泵在空闲时派发——不在这一帧发命令，
        // 免得和用户紧接着点的「同步」抢单操作串行位。
        if ok {
            if let Some(panel) = self.dev_sync.clone() {
                panel.update(cx, |panel, _cx| panel.request_device_refresh());
            }
        }
        true
    }

    /// 结构 head 变了才重放结构文档 + 重投影 + 刷新侧栏。
    ///
    /// 为什么能"只补投影"：`StructureWriter::commit` 在没有新 op 时只做一次全量投影
    /// （幂等），不会凭空改 CRDT。结构没变就**不碰**它（不假刷新）。
    fn dev_sync_refresh_structure_if_needed(
        &mut self,
        cx: &mut Context<Self>,
        base_seq: u64,
    ) -> bool {
        let changed = load_head(&self.vault, STRUCTURE_DOC_ID)
            .map(|head| head.head_seq != base_seq)
            .unwrap_or(false);
        if !changed {
            return false;
        }
        let mut writer = match StructureWriter::open_with_sync(&self.vault, self.peer_id, self.sync)
        {
            Ok(writer) => writer,
            Err(error) => {
                eprintln!("[menghunote] 同步后重放结构文档失败：{error:?}");
                return false;
            }
        };
        if let Err(error) = writer.commit("dev-sync-refresh") {
            eprintln!("[menghunote] 同步后结构投影失败：{error:?}");
            return false;
        }
        self.refresh_tree();
        cx.notify();
        true
    }

    /// 服务里是否**没有**在途/超时待收尾的 worker op（解闸守卫的第二个条件）。
    ///
    /// 只在"本次任务根本没派发 op"时用到：必须确认不是另一个已超时的 op 仍占着单操作位，
    /// 否则解闸会与仍在 `integrate` 的 worker 竞争。服务不可用（面板被释放）时按
    /// fail-closed 返回 `false`。
    fn dev_sync_no_unsettled_worker_op(&self, cx: &App) -> bool {
        self.dev_sync_service(cx).is_some_and(|service| {
            let snapshot = service.snapshot();
            snapshot.in_flight.is_none() && snapshot.deadline_exceeded.is_none()
        })
    }

    /// 面板的服务（`None` = 面板没挂）。
    fn dev_sync_service(&self, cx: &App) -> Option<std::sync::Arc<crate::dev_sync::SyncService>> {
        self.dev_sync
            .as_ref()
            .map(|panel| panel.read(cx).service().clone())
    }

    /// 把"当前有没有活动文档"这个宿主事实写给面板（切片 6b）。
    ///
    /// 轮换/应用新 epoch 都必须先在一篇**活动文档**上持闸冻结；面板只有知道这个事实，
    /// 才能在按钮上提前给原因（而不是让用户点完才被 worker 拒绝）。写在三点：
    /// 构造期（`open_entry` 早于面板）、打开/删除文档后、以及每次泵。
    fn dev_sync_push_active_document(&self, cx: &mut Context<Self>) {
        let Some(panel) = self.dev_sync.clone() else {
            return;
        };
        let has_active = self.active.is_some();
        panel.update(cx, |panel, cx| {
            if panel.set_active_document(has_active) {
                cx.notify();
            }
        });
    }

    /// 写面板的「同步行 + 最近操作」（只在 tick 上下文调用，不能在面板事件回调里调用）。
    fn dev_sync_panel_state(
        &mut self,
        cx: &mut Context<Self>,
        busy: bool,
        line: impl Into<String>,
        notice: Option<String>,
    ) {
        self.dev_sync_panel_task_state(
            cx,
            crate::dev_sync::GatedTask::SyncOnce,
            busy,
            line,
            notice,
        );
    }

    /// 按任务种类写面板的「持闸任务行 + 最近操作」（同步 / 发布基线共用一行）。
    fn dev_sync_panel_task_state(
        &mut self,
        cx: &mut Context<Self>,
        task: crate::dev_sync::GatedTask,
        busy: bool,
        line: impl Into<String>,
        notice: Option<String>,
    ) {
        let Some(panel) = self.dev_sync.clone() else {
            return;
        };
        let line = line.into();
        let auto_round = task == crate::dev_sync::GatedTask::SyncOnce
            && self.dev_sync_round_origin == Some(crate::dev_sync::OpOrigin::Auto);
        panel.update(cx, |panel, cx| {
            match task {
                crate::dev_sync::GatedTask::SyncOnce if auto_round => {
                    panel.set_auto_sync_state(busy, line);
                }
                crate::dev_sync::GatedTask::SyncOnce => panel.set_sync_state(busy, line),
                crate::dev_sync::GatedTask::PublishBaseline => panel.set_publish_state(busy, line),
                crate::dev_sync::GatedTask::Rotate => panel.set_rotate_state(busy, line),
                crate::dev_sync::GatedTask::ApplyEpoch => {
                    panel.set_apply_epoch_state(busy, line);
                }
            }
            if let Some(notice) = notice {
                panel.set_notice(notice);
            }
            cx.notify();
        });
    }

    /// 每 40 ms 泵一次持闸同步（只在有同步在途/有请求时才动，空闲不打扰编辑器）。
    fn spawn_dev_sync_ticker(&self, cx: &mut Context<Self>) {
        let timers = cx.background_executor().clone();
        cx.spawn(async move |this, cx| loop {
            timers
                .timer(crate::dev_sync::sync_once::SYNC_PUMP_INTERVAL)
                .await;
            let updated = this.update(cx, |shell, cx| {
                // ADR 0012 S3：自动同步的时钟由 `AutoSync` 注入（测试用手动时钟且
                // `ticker: false`，于是这里完全不跑，只有显式 `dev_sync_auto_tick`）。
                let auto_ticked = if shell
                    .dev_sync_auto
                    .as_ref()
                    .is_some_and(crate::dev_sync::AutoSync::ticker_enabled)
                {
                    shell.dev_sync_auto_tick(cx)
                } else {
                    false
                };
                if auto_ticked
                    || shell.dev_sync_sync.is_some()
                    || shell.dev_sync_requested
                    || shell.dev_sync_publish_requested
                    || shell.dev_sync_rotate_requested
                    || shell.dev_sync_apply_requested
                {
                    shell.dev_sync_pump(cx);
                }
            });
            if updated.is_err() {
                // 视图已释放（窗口关闭）：结束循环。
                break;
            }
        })
        .detach();
    }

    /// 只读：当前活动文档的镜像正文（测试断言用）。
    #[must_use]
    pub fn dev_sync_current_text(&self, cx: &App) -> Option<String> {
        self.active
            .as_ref()
            .map(|active| active.view.read(cx).dev_sync_current_text())
    }

    /// 对活动视图执行一次需要 `&mut Context<AppShell>` 的转发。
    fn update_active_view<R>(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut EditorView, &mut Context<EditorView>) -> R,
    ) -> Option<R> {
        self.active
            .as_ref()
            .map(|active| active.view.update(cx, |view, cx| f(view, cx)))
    }
}

/// ADR 0019 S2a：给 `EditorView` 的图片预览注入“按 hash 读 asset 字节”的能力。
///
/// 依赖方向：app → storage 的**只读** API（`asset_bytes`）；EditorView 只认
/// `AssetBytesProvider` trait，不认识 `Vault`。读取发生在解码线程上（慢盘不阻塞主线程），
/// 资产不存在 / `data IS NULL` 都返回 `None`（占位显示“缺失/待同步”）。
fn vault_asset_provider(vault: &Vault) -> crate::editor::object_image::SharedAssetProvider {
    let vault = vault.clone();
    std::sync::Arc::new(move |hash: &str| -> Option<Vec<u8>> {
        let hash = asset_hash_bytes(hash)?;
        match menghunote_storage::asset_bytes(&vault, hash) {
            Ok(bytes) => bytes,
            Err(error) => {
                // 读取失败不是致命错误：图片降级为占位，正文照常编辑。
                eprintln!("[menghunote] 读取图片资产失败（降级占位）：{error}");
                None
            }
        }
    })
}

/// `asset://` 里的 64 个小写 hex → 32 字节哈希（非法长度/字符返回 `None`）。
fn asset_hash_bytes(hash: &str) -> Option<[u8; 32]> {
    if hash.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let (chunks, remainder) = hash.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    for (index, chunk) in chunks.iter().enumerate() {
        let high = (chunk[0] as char).to_digit(16)?;
        let low = (chunk[1] as char).to_digit(16)?;
        out[index] = u8::try_from((high << 4) | low).ok()?;
    }
    Some(out)
}

/// 组装同步报告的材料（notice 由这些字段派生，避免调用点各写一遍）。
///
/// 两条路径共用：持闸路径填 `doc_id`/`changes`/`dispatched_under_gate`；无会话路径
/// `no_session = true`、`doc_id = None`、`changes = 0`（没有视图可交付）。
#[cfg(feature = "dev-sync")]
#[derive(Debug, Default)]
struct ReportParts {
    /// 本次持闸任务的种类（同步 / 发布基线；无会话同步恒为 `SyncOnce`）。
    task: crate::dev_sync::GatedTask,
    /// 本次同步的活动文档（无会话路径 = `None`）。
    doc_id: Option<String>,
    /// 是否走无会话路径（没有闸门/没有 adopt）。
    no_session: bool,
    /// 是否在 Held 下派发过（无会话路径恒为 `false`）。
    dispatched_under_gate: bool,
    /// 交进 live 的已 durable 变化条数（无会话路径恒为 0）。
    changes: usize,
    /// worker 摘要（失败时 `None`）。
    worker_summary: Option<String>,
    /// 明确失败原因。
    error: Option<String>,
    /// 结构 head 变了才为 `true`（不假刷新）。
    structure_refreshed: bool,
    /// 同步后自动打开的笔记标题（仅无会话路径 + 树里出现笔记）。
    opened_note: Option<String>,
    /// 轮换 / apply 的公开事实（切片 6a；其它任务为 `None`）。
    rotation: Option<crate::dev_sync::RotationFacts>,
}

#[cfg(feature = "dev-sync")]
impl ReportParts {
    /// 派生面板 notice 并产出最终报告。
    ///
    /// notice 的措辞按路径分开——无会话路径没有"交付已落盘变化"这回事，不能沿用
    /// 持闸路径的措辞（否则面板会宣称一件没发生的事）。
    fn report(self) -> crate::dev_sync::SyncOnceReport {
        let structure = if self.structure_refreshed {
            "，结构树已更新"
        } else {
            ""
        };
        let notice = match (&self.error, self.task) {
            (None, crate::dev_sync::GatedTask::SyncOnce) if self.no_session => {
                let opened = self
                    .opened_note
                    .as_ref()
                    .map(|title| format!("，已打开「{title}」"))
                    .unwrap_or_default();
                format!("同步完成：无会话同步{structure}{opened}")
            }
            (None, crate::dev_sync::GatedTask::SyncOnce) => {
                format!("同步完成：{} 条变化{structure}", self.changes)
            }
            // 发布：worker 的摘要是公开事实（checkpoint 短 id / 覆盖 seq / B 可以 Join），
            // 直接当 notice；缺摘要时不能编造细节。
            (None, crate::dev_sync::GatedTask::PublishBaseline) => self
                .worker_summary
                .clone()
                .unwrap_or_else(|| "初始基线已发布（B 现在可以 Join）".to_owned()),
            (None, crate::dev_sync::GatedTask::Rotate) => rotation_notice(self.rotation.as_ref()),
            (None, crate::dev_sync::GatedTask::ApplyEpoch) => {
                apply_epoch_notice(self.rotation.as_ref())
            }
            (Some(error), _) if self.no_session => format!("同步失败：{error}{structure}"),
            (Some(error), crate::dev_sync::GatedTask::SyncOnce) => format!(
                "同步失败：{error}（已交付 {} 条已落盘变化{structure}）",
                self.changes
            ),
            (Some(error), crate::dev_sync::GatedTask::PublishBaseline) => format!(
                "发布基线失败：{error}（已交付 {} 条已落盘变化{structure}）",
                self.changes
            ),
            (Some(error), crate::dev_sync::GatedTask::Rotate) => {
                // 失败停在 journal 之后（阶段不是「未开始」）时，库级入口保留了 rotation
                // journal：同一实例再点轮换会被 "已有未完成 rotation journal" 拒绝，
                // 必须先重启实例再走恢复流程。这里如实说清，别让用户反复点。
                let journal = match self
                    .rotation
                    .as_ref()
                    .and_then(|facts| facts.failure_phase.as_deref())
                {
                    Some(phase) if phase != "未开始" => format!(
                        "；失败停在「{phase}」，保留了 rotation journal：重试前需重启实例（不自动恢复/重试）"
                    ),
                    _ => String::new(),
                };
                format!("轮换失败：{error}（身份/视图保持旧 epoch；未自动重试）{journal}")
            }
            (Some(error), crate::dev_sync::GatedTask::ApplyEpoch) => {
                format!("应用新 epoch 失败：{error}（身份/视图保持旧 epoch；未自动重试）")
            }
        };
        crate::dev_sync::SyncOnceReport {
            task: self.task,
            doc_id: self.doc_id,
            no_session: self.no_session,
            dispatched_under_gate: self.dispatched_under_gate,
            changes: self.changes,
            worker_summary: self.worker_summary,
            error: self.error,
            structure_refreshed: self.structure_refreshed,
            opened_note: self.opened_note,
            rotation: self.rotation,
            notice,
        }
    }
}

/// 轮换成功的面板文案（epoch / checkpoint 短 id / 恢复码是否补发）。
#[cfg(feature = "dev-sync")]
fn rotation_notice(facts: Option<&crate::dev_sync::RotationFacts>) -> String {
    let Some(facts) = facts else {
        return "轮换完成（缺少公开事实）".to_owned();
    };
    let epoch = match facts.to_epoch {
        Some(to) => format!("epoch {} → {to}", facts.from_epoch),
        None => format!("epoch {}（未知新 epoch）", facts.from_epoch),
    };
    let checkpoint = match (&facts.primary_checkpoint, &facts.fallback_checkpoint) {
        (Some(primary), Some(fallback)) => format!("checkpoint {primary}/{fallback}"),
        (Some(primary), None) => format!("checkpoint {primary}"),
        _ => "checkpoint 未知".to_owned(),
    };
    let recovery = if facts.recovery_code_reissued {
        "恢复码已补发（新设备可用恢复码加入）"
    } else {
        "恢复码未补发（轮换后新设备无法用恢复码加入）"
    };
    let revoked = if facts.revoked == 0 {
        String::new()
    } else {
        format!("；移除 {} 台", facts.revoked)
    };
    format!("轮换完成：{epoch}；{checkpoint}{revoked}；{recovery}")
}

/// apply 成功的面板文案（epoch / checkpoint 短 id；幂等 AlreadyCurrent 单独措辞）。
#[cfg(feature = "dev-sync")]
fn apply_epoch_notice(facts: Option<&crate::dev_sync::RotationFacts>) -> String {
    let Some(facts) = facts else {
        return "已应用新 epoch（缺少公开事实）".to_owned();
    };
    let Some(to) = facts.to_epoch else {
        return format!(
            "已应用新 epoch（epoch {}，新 epoch 未知）",
            facts.from_epoch
        );
    };
    if facts.already_current {
        return format!("已是最新 epoch：{to}（无需 apply）");
    }
    let checkpoint = facts.primary_checkpoint.as_ref().map_or_else(
        || "checkpoint 未知".to_owned(),
        |id| format!("checkpoint {id}"),
    );
    format!("已应用新 epoch：{} → {to}；{checkpoint}", facts.from_epoch)
}

/// 持闸同步的终态（worker 回执；`Ok` 带 worker 摘要，`Failed` 带明确原因）。
#[cfg(feature = "dev-sync")]
#[derive(Debug, Clone)]
enum SyncOnceFinish {
    /// worker 正常完成。
    Ok(String),
    /// worker 明确失败（或命令没派发出去）。
    Failed(String),
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ADR 0021 P2：系统外观（浅色/深色）变化 → 全局主题 + 重绘；只订阅一次。
        self.observe_appearance_once(window, cx);
        // 输入框刚创建时拿不到 `&mut Window`（订阅回调里没有），在这一帧补一次焦点。
        if self.pending_focus_rename {
            self.pending_focus_rename = false;
            if let Some(editing) = &self.editing {
                let input = editing.input.clone();
                window.defer(cx, move |window, cx| {
                    // 右键菜单的 mouse-up 可能在本帧之后把焦点抢回编辑器；再延一帧聚焦，
                    // 保证输入框最终拿到焦点（F2 无 mouse-up，所以一直正常）。
                    window.defer(cx, move |window, cx| {
                        input.update(cx, |input, cx| input.focus(window, cx));
                    });
                });
            }
        }
        // 窗口失焦 → 关菜单（只订阅一次，避免每帧重复注册）。
        if !self.activation_observed {
            self.activation_observed = true;
            self.activation_subscription =
                Some(cx.observe_window_activation(window, |shell, window, cx| {
                    // ADR 0012 S3：失焦降到 60s；重新聚焦时若有 durable 待办合并一次探测。
                    #[cfg(feature = "dev-sync")]
                    shell.dev_sync_auto_on_activation(window.is_window_active(), cx);
                    if window.is_window_active() || shell.menu.is_none() {
                        return;
                    }
                    // 实测坑：菜单打开时把焦点交给菜单面板，gpui 的 active 标志会短暂
                    // 变 false；若直接关菜单，用户看到的就是"右键闪一下/没有菜单"。
                    // 刚打开的一段时间内忽略失焦，只有真正离开窗口才关。
                    let opened_for = shell
                        .menu_opened_at
                        .map_or(std::time::Duration::MAX, |opened| opened.elapsed());
                    if opened_for < DEACTIVATE_GUARD {
                        trace_menu!(
                            "[menghunote] 失焦(忽略：菜单刚打开 {:.0}ms)",
                            opened_for.as_secs_f64() * 1000.0
                        );
                        return;
                    }
                    trace_menu!(
                        "[menghunote] 窗口失焦 → 关闭右键菜单（已开 {:.0}ms）",
                        opened_for.as_secs_f64() * 1000.0
                    );
                    shell.close_menu("窗口失焦", window, cx);
                }));
        }

        // 开发开关：自动打开一次搜索浮层（给 UI 冒烟/截图用，正常运行时不存在）。
        // 快捷键 `Ctrl+K` 在自动化环境里注不进去（前台被别的窗口占着，见 AGENTS §6），
        // 所以留这个开关让脚本能稳定截到浮层。
        if !self.search_auto_used && std::env::var_os("MENGHUNOTE_SEARCH_ON_START").is_some() {
            self.search_auto_used = true;
            self.pending_search = true;
        }
        // 「请打开搜索」在事件回调里置位，这里才拿得到 `&mut Window`。
        if self.pending_search {
            self.pending_search = false;
            self.open_search(window, cx);
        }

        let sidebar = self.render_sidebar(cx);
        let main = self.render_main(cx);
        let content = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h(px(0.0))
            .child(sidebar)
            .child(main);
        let status_bar = self.render_status_bar(cx);
        // 浮层同样渲染在根层（侧栏会裁剪子元素）。
        let search = self.search.clone();
        // 菜单渲染在**根层**：侧栏是 `overflow_y_scroll`，放里面会被裁掉。
        let popup = self.render_menu(window, cx);
        // 图谱覆盖层：全屏、盖在编辑器上（不挤压编辑器，也不卸载它）。
        let graph = self.render_graph(window, cx);
        #[cfg(feature = "dev-sync")]
        let settings = self.render_settings(cx);
        let root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::window_bg())
            .font_family(theme::FONT_FAMILY)
            // ---- 窗口级鼠标取证（capture 相，不受命中影响）----
            // 目的是区分三种可能：(a) 窗口根本没收到；(b) 收到了但没命中行；
            // (c) 命中行但行上的 up 没触发。日志一直开着（不藏进 debug 构建）。
            .capture_any_mouse_down(cx.listener(
                |shell, event: &gpui::MouseDownEvent, window, _cx| {
                    trace_mouse!(
                        "[menghunote] mouse down button={:?} at=({:.1},{:.1}) active={} t=+{:.1}ms",
                        event.button,
                        event.position.x.as_f32(),
                        event.position.y.as_f32(),
                        window.is_window_active(),
                        shell.epoch.elapsed().as_secs_f64() * 1000.0
                    );
                },
            ))
            .capture_any_mouse_up(
                cx.listener(|shell, event: &gpui::MouseUpEvent, window, _cx| {
                    trace_mouse!(
                        "[menghunote] mouse up   button={:?} at=({:.1},{:.1}) active={} t=+{:.1}ms",
                        event.button,
                        event.position.x.as_f32(),
                        event.position.y.as_f32(),
                        window.is_window_active(),
                        shell.epoch.elapsed().as_secs_f64() * 1000.0
                    );
                }),
            )
            .child(content)
            .child(status_bar)
            .when_some(popup, |this, popup| this.child(popup))
            .when_some(search, |this, search| this.child(search));
        // 同步设置页：只在打开时挂载面板（默认主界面不显示覆盖层）。
        #[cfg(feature = "dev-sync")]
        let root = root.when_some(settings, |this, settings| this.child(settings));
        // 图谱在最上层：覆盖层打开时抢焦点，搜索/菜单/设置不可能同时被操作。
        root.when_some(graph, |this, graph| this.child(graph))
    }
}

/// 右键菜单里的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    /// 行内重命名（目录与笔记都有）。
    Rename,
    /// 在目标目录里新建笔记。
    NewNote,
    /// 在目标目录里新建文件夹。
    NewFolder,
    /// 删除目标笔记（两步确认）。
    Delete,
}

/// 菜单项集合（纯函数）：目录 = 新建两项；笔记 = 删除一项。
#[must_use]
fn menu_items(is_folder: bool) -> &'static [MenuAction] {
    const FOLDER: [MenuAction; 3] = [
        MenuAction::Rename,
        MenuAction::NewNote,
        MenuAction::NewFolder,
    ];
    const NOTE: [MenuAction; 2] = [MenuAction::Rename, MenuAction::Delete];
    if is_folder {
        &FOLDER
    } else {
        &NOTE
    }
}

/// 菜单面板的几何命中（纯函数）：点落在哪个菜单项上。
///
/// 为什么不用每个菜单项的 `on_mouse_down`：gpui 的元素级鼠标监听要求
/// `hitbox.is_hovered(window)`，而 hover 依赖"上一次 mouse move 建立的命中链"。
/// 菜单在**同一个坐标**连点两次时（例如删除的二次确认），第二次往往没有新的
/// mouse move，新元素树的 hitbox 没被判为 hovered → 监听被**静默跳过**，
/// 点击落到遮罩上把菜单关了。改成"遮罩统一接点击 + 纯几何命中"，行为可预测、可单测。
#[must_use]
fn menu_hit(position: Point<Pixels>, origin: Point<Pixels>, is_folder: bool) -> Option<MenuAction> {
    let x = position.x.as_f32();
    let y = position.y.as_f32();
    let left = origin.x.as_f32();
    let top = origin.y.as_f32();
    if x < left || x > left + MENU_WIDTH || y < top || y > top + menu_height(is_folder) {
        return None;
    }
    let item_top = top + MENU_HEADER_HEIGHT;
    if y < item_top {
        // 表头（标题）不是可点区域
        return None;
    }
    let index = ((y - item_top) / MENU_ITEM_HEIGHT) as usize;
    menu_items(is_folder).get(index).copied()
}

/// 菜单整体高度估算（表头 + 菜单项 + 上下 padding + 边框），用于夹取。
#[must_use]
fn menu_height(is_folder: bool) -> f32 {
    24.0 + menu_items(is_folder).len() as f32 * MENU_ITEM_HEIGHT + 12.0
}

impl MenuAction {
    /// 用于元素 id 的稳定标签（避免复用同一 id）。
    const fn label(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::NewNote => "new-note",
            Self::NewFolder => "new-folder",
            Self::Delete => "delete",
        }
    }
}

/// 遮罩上的这次点击是否应该关闭菜单（纯函数）。
///
/// 菜单打开后极短时间内落下的点击，视为"打开它的那一次手势"的余波（鼠标 up 后紧跟
/// 的抬指/双击第二击等），**不关闭**；超过 `OPEN_GUARD` 才算真正的"点外部"。
#[must_use]
fn overlay_click_closes(elapsed_since_open: std::time::Duration) -> bool {
    elapsed_since_open >= OPEN_GUARD
}

/// 图谱工具条上的小按钮（纯样式；事件由调用方挂，便于每个按钮自己的动作）。
#[must_use]
fn graph_button(
    selector: &'static str,
    label: &'static str,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(selector)
        // 测试元数据（只读断言用）：`graph-reset` / `graph-close`。
        .debug_selector(move || selector.to_string())
        .h(px(24.0))
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border(px(1.0))
        .border_color(if primary {
            theme::graph_edge_active()
        } else {
            theme::sidebar_border()
        })
        .bg(if primary {
            theme::graph_edge_active()
        } else {
            theme::graph_toolbar_bg()
        })
        .text_size(px(12.0))
        .text_color(if primary {
            theme::graph_node_active_text()
        } else {
            theme::text_color()
        })
        .cursor_pointer()
        .child(SharedString::from(label))
}

/// 图谱过滤条上的小 chip（动态标签：文件夹/标签/开关；`active` = 当前选中）。
///
/// 与 [`graph_button`] 同一套配色，只是标签是运行时字符串（文件夹/标签名）。
#[must_use]
fn graph_chip(selector: String, label: String, active: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(selector.clone()))
        // 测试元数据（只读断言用）：`graph-folder-<i>` / `graph-tag-<i>` / `graph-toggle-*`。
        .debug_selector(move || selector)
        .h(px(20.0))
        .px_2()
        .flex()
        .items_center()
        .justify_center()
        .rounded_sm()
        .border(px(1.0))
        .border_color(if active {
            theme::graph_edge_active()
        } else {
            theme::sidebar_border()
        })
        .bg(if active {
            theme::graph_edge_active()
        } else {
            theme::graph_toolbar_bg()
        })
        .text_size(px(11.0))
        .text_color(if active {
            theme::graph_node_active_text()
        } else {
            theme::text_color()
        })
        .cursor_pointer()
        .child(SharedString::from(label))
}

/// 一条边是否完全落在画布外（视口裁剪；纯几何，绘制前后都不改状态）。
///
/// 边的包围盒与画布不相交（含一圈余量）就跳过绘制：大图/远平移时省掉无用图元，
/// 但**不影响**命中与模型（它们只看节点坐标）。
#[must_use]
fn edge_offscreen(a: Point<Pixels>, b: Point<Pixels>, viewport: Size<Pixels>) -> bool {
    const MARGIN: f32 = 32.0;
    let min_x = a.x.min(b.x).as_f32();
    let max_x = a.x.max(b.x).as_f32();
    let min_y = a.y.min(b.y).as_f32();
    let max_y = a.y.max(b.y).as_f32();
    max_x < -MARGIN
        || max_y < -MARGIN
        || min_x > viewport.width.as_f32() + MARGIN
        || min_y > viewport.height.as_f32() + MARGIN
}

/// 把菜单左上角**夹进窗口内**（纯函数）：`position` 是右键点，`viewport` 是窗口
/// 逻辑尺寸，`menu` 是菜单面板尺寸。
///
/// 只做"夹取"不做翻转：菜单永远在点的右下方，贴边时往回收，保证整块可见。
#[must_use]
fn clamp_menu_position(
    position: Point<Pixels>,
    viewport: Size<Pixels>,
    menu: Size<Pixels>,
) -> Point<Pixels> {
    const MARGIN: f32 = 4.0;
    let max_x = (viewport.width.as_f32() - menu.width.as_f32() - MARGIN).max(0.0);
    let max_y = (viewport.height.as_f32() - menu.height.as_f32() - MARGIN).max(0.0);
    Point {
        x: px(position.x.as_f32().clamp(0.0, max_x)),
        y: px(position.y.as_f32().clamp(0.0, max_y)),
    }
}

/// 生成与现有标题不冲突的默认名：`未命名笔记`、`未命名笔记 2`、`未命名笔记 3`……
///
/// storage 不限制标题唯一（同名笔记是合法的），这里只为**侧栏可辨认**去重；
/// 全是内存里的树快照，不查库、不加锁。
#[must_use]
fn unique_title(base: &str, tree: &[TreeEntry]) -> String {
    if !tree.iter().any(|entry| entry.title == base) {
        return base.to_owned();
    }
    for suffix in 2..1000 {
        let candidate = format!("{base} {suffix}");
        if !tree.iter().any(|entry| entry.title == candidate) {
            return candidate;
        }
    }
    base.to_owned()
}

/// 当前时间戳（毫秒，i64）：结构文档的 `created_at` / `edited_at` 用。
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

/// 计算当前应该渲染的树行（`self.tree` 的下标），**纯函数**。
///
/// 依据 `list_tree()` 的**先序 DFS** 性质：一个节点的所有后代必然紧随其后、
/// 且 `depth` 严格更大；折叠某节点时，只把它之后 `depth >` 它的条目藏起来，
/// 直到遇到第一个 `depth <=` 它的条目为止（那就是它的兄弟或祖先的后继）。
///
/// 为什么不用 `parent_id` 递归：脏数据（父节点缺失/环）会让递归丢节点；
/// 按"深度 + 先序"过滤则**永远只是跳过一段连续区间**，不会漏掉别的分支。
#[must_use]
fn visible_rows(tree: &[TreeEntry], collapsed: &HashSet<String>) -> Vec<usize> {
    let mut rows = Vec::with_capacity(tree.len());
    // 正在被折叠目录遮住的深度（None = 没有遮罩）
    let mut hidden_below: Option<usize> = None;
    for (index, entry) in tree.iter().enumerate() {
        if let Some(depth) = hidden_below {
            if entry.depth > depth {
                continue;
            }
            // 回到同层或更外层：遮罩结束，这条条目正常参与渲染
            hidden_below = None;
        }
        rows.push(index);
        if entry.is_folder() && collapsed.contains(&entry.node_id) {
            hidden_below = Some(entry.depth);
        }
    }
    rows
}

/// 让编译器提醒：`App` 只在这里用到类型约束（保留导入一致性）。
#[allow(dead_code)]
fn _assert_app(_: &App) {}

#[cfg(test)]
mod tests {
    use super::visible_rows;
    use gpui::{px, Point, Size};
    use menghunote_storage::{NodeKind, TreeEntry};
    use std::collections::HashSet;

    /// 造一条树条目；`kind` 由 `note_id` 是否为空决定（与真实数据同构）。
    fn entry(node: &str, title: &str, depth: usize, is_folder: bool) -> TreeEntry {
        TreeEntry {
            node_id: node.to_owned(),
            note_id: (!is_folder).then(|| format!("{node}-note")),
            body_doc_id: (!is_folder).then(|| format!("{node}-body")),
            title: title.to_owned(),
            parent_id: None,
            kind: if is_folder {
                NodeKind::Folder
            } else {
                NodeKind::Note
            },
            sibling_rank: 0,
            depth,
        }
    }

    /// A / ├ A1 / ├ A2 / └ A3 / B（先序 DFS）
    fn sample() -> Vec<TreeEntry> {
        vec![
            entry("A", "目录A", 0, true),
            entry("A1", "笔记A1", 1, false),
            entry("A2", "笔记A2", 1, false),
            entry("A3", "笔记A3", 1, false),
            entry("B", "笔记B", 0, false),
        ]
    }

    fn collapse(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn collapsing_a_folder_hides_only_its_descendants() {
        let tree = sample();
        // 全展开：全部可见
        assert_eq!(visible_rows(&tree, &HashSet::new()), vec![0, 1, 2, 3, 4]);
        // 折叠 A：只藏 A 的三个子项，兄弟 B 不受影响
        assert_eq!(visible_rows(&tree, &collapse(&["A"])), vec![0, 4]);
    }

    #[test]
    fn nested_collapse_hides_the_inner_branch_entirely() {
        // A / ├ A1(目录) / │ ├ A1a / │ └ A1b / └ A2 / B
        let tree = vec![
            entry("A", "目录A", 0, true),
            entry("A1", "目录A1", 1, true),
            entry("A1a", "笔记A1a", 2, false),
            entry("A1b", "笔记A1b", 2, false),
            entry("A2", "笔记A2", 1, false),
            entry("B", "笔记B", 0, false),
        ];
        // 只折叠内层 A1：A2 仍可见
        assert_eq!(visible_rows(&tree, &collapse(&["A1"])), vec![0, 1, 4, 5]);
        // 折叠外层 A：内层后代一并隐藏，折叠状态**保留**在集合里（展开外层后仍生效）
        let both = collapse(&["A", "A1"]);
        assert_eq!(visible_rows(&tree, &both), vec![0, 5]);
        let mut opened = both.clone();
        opened.remove("A");
        assert_eq!(
            visible_rows(&tree, &opened),
            vec![0, 1, 4, 5],
            "外层展开后，内层 A1 的折叠状态应继续生效"
        );
    }

    #[test]
    fn collapsing_a_leaf_folder_changes_nothing() {
        let tree = vec![entry("A", "空目录", 0, true), entry("B", "笔记B", 0, false)];
        // 叶子目录折叠后没有后代可藏，行不变（只是 ▾/▸ 变了）
        assert_eq!(visible_rows(&tree, &collapse(&["A"])), vec![0, 1]);
    }

    #[test]
    fn toggling_twice_restores_the_original_rows() {
        let tree = sample();
        let mut collapsed = HashSet::new();
        // toggle 折叠
        assert!(collapsed.insert("A".to_owned()));
        assert_eq!(visible_rows(&tree, &collapsed), vec![0, 4]);
        // toggle 展开
        assert!(collapsed.remove("A"));
        assert_eq!(visible_rows(&tree, &collapsed), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn collapsing_a_note_id_has_no_effect() {
        // 防御：折叠集合里混入笔记 ID（或已删节点）不应影响任何行
        let tree = sample();
        assert_eq!(
            visible_rows(&tree, &collapse(&["A1", "ghost"])),
            vec![0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn unique_title_appends_a_suffix_when_taken() {
        // 空树：直接用默认名
        assert_eq!(super::unique_title("未命名笔记", &[]), "未命名笔记");

        let tree = vec![
            entry("A", "未命名笔记", 0, false),
            entry("B", "未命名笔记 2", 0, false),
        ];
        assert_eq!(
            super::unique_title("未命名笔记", &tree),
            "未命名笔记 3",
            "前两个都占用时要跳到 3"
        );

        // 只占用 base 时给 2
        let tree = vec![entry("A", "未命名笔记", 0, false)];
        assert_eq!(super::unique_title("未命名笔记", &tree), "未命名笔记 2");
        // 其它标题不影响
        let tree = vec![entry("A", "别的笔记", 0, false)];
        assert_eq!(super::unique_title("未命名笔记", &tree), "未命名笔记");
    }

    #[test]
    fn overlay_click_right_after_opening_does_not_close_the_menu() {
        use std::time::Duration;
        // 同一次手势的余波（0ms / 30ms / 99ms）→ 不关
        assert!(!super::overlay_click_closes(Duration::from_millis(0)));
        assert!(!super::overlay_click_closes(Duration::from_millis(30)));
        assert!(!super::overlay_click_closes(Duration::from_millis(99)));
        // 真正的新点击（>= 100ms）→ 关
        assert!(super::overlay_click_closes(Duration::from_millis(100)));
        assert!(super::overlay_click_closes(Duration::from_millis(250)));
        assert!(super::overlay_click_closes(Duration::MAX));
    }

    #[test]
    fn menu_items_depend_on_the_row_kind() {
        use super::{menu_height, menu_items, MenuAction};
        // 目录：重命名 / 新建笔记 / 新建文件夹
        assert_eq!(
            menu_items(true),
            &[
                MenuAction::Rename,
                MenuAction::NewNote,
                MenuAction::NewFolder
            ],
            "目录菜单 = 重命名 + 两个新建"
        );
        // 笔记：重命名 / 删除
        assert_eq!(
            menu_items(false),
            &[MenuAction::Rename, MenuAction::Delete],
            "笔记菜单 = 重命名 + 删除"
        );
        // 高度随项数变化，保证夹取用对尺寸
        assert!(menu_height(true) > menu_height(false));
    }

    #[test]
    fn menu_hit_maps_clicks_to_items() {
        use super::{menu_hit, MenuAction};
        let origin = Point {
            x: px(60.0),
            y: px(40.0),
        };
        // 表头（y < 表头高）不是可点区域
        assert_eq!(
            menu_hit(
                Point {
                    x: px(80.0),
                    y: px(45.0)
                },
                origin,
                true
            ),
            None
        );
        // 目录菜单三项：重命名 / 新建笔记 / 新建文件夹
        assert_eq!(
            menu_hit(
                Point {
                    x: px(80.0),
                    y: px(77.0)
                },
                origin,
                true
            ),
            Some(MenuAction::Rename)
        );
        assert_eq!(
            menu_hit(
                Point {
                    x: px(80.0),
                    y: px(103.0)
                },
                origin,
                true
            ),
            Some(MenuAction::NewNote)
        );
        assert_eq!(
            menu_hit(
                Point {
                    x: px(80.0),
                    y: px(129.0)
                },
                origin,
                true
            ),
            Some(MenuAction::NewFolder)
        );
        // 笔记菜单两项：重命名 / 删除；再往下就出界
        assert_eq!(
            menu_hit(
                Point {
                    x: px(80.0),
                    y: px(77.0)
                },
                origin,
                false
            ),
            Some(MenuAction::Rename)
        );
        assert_eq!(
            menu_hit(
                Point {
                    x: px(80.0),
                    y: px(103.0)
                },
                origin,
                false
            ),
            Some(MenuAction::Delete)
        );
        assert_eq!(
            menu_hit(
                Point {
                    x: px(80.0),
                    y: px(129.0)
                },
                origin,
                false
            ),
            None
        );
        // 面板外 → None
        assert_eq!(
            menu_hit(
                Point {
                    x: px(10.0),
                    y: px(10.0)
                },
                origin,
                true
            ),
            None
        );
    }

    #[test]
    fn menu_position_is_clamped_into_the_window() {
        let viewport = Size {
            width: px(1000.0),
            height: px(800.0),
        };
        let menu = Size {
            width: px(148.0),
            height: px(80.0),
        };
        // 中间位置：原样
        let inside = Point {
            x: px(300.0),
            y: px(200.0),
        };
        assert_eq!(super::clamp_menu_position(inside, viewport, menu), inside);
        // 贴右/贴下：往回收，保证整块可见
        let corner = Point {
            x: px(990.0),
            y: px(790.0),
        };
        assert_eq!(
            super::clamp_menu_position(corner, viewport, menu),
            Point {
                x: px(1000.0 - 148.0 - 4.0),
                y: px(800.0 - 80.0 - 4.0),
            }
        );
        // 负坐标（理论上不该出现）：夹到 0
        let negative = Point {
            x: px(-20.0),
            y: px(-5.0),
        };
        assert_eq!(
            super::clamp_menu_position(negative, viewport, menu),
            Point {
                x: px(0.0),
                y: px(0.0),
            }
        );
        // 窗口比菜单还小：夹到 0，不出现负坐标
        let tiny = Size {
            width: px(100.0),
            height: px(60.0),
        };
        assert_eq!(
            super::clamp_menu_position(corner, tiny, menu),
            Point {
                x: px(0.0),
                y: px(0.0),
            }
        );
    }

    #[test]
    fn empty_tree_renders_nothing() {
        assert_eq!(visible_rows(&[], &HashSet::new()), Vec::<usize>::new());
    }
}
