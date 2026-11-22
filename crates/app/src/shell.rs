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
    div, px, AnyElement, App, Context, Entity, FocusHandle, MouseButton, Pixels, Point, Render,
    SharedString, Size, Window,
};
use menghunote_editor::EditorController;
use menghunote_storage::session::{DriverOptions, SessionDriver};
use menghunote_storage::{list_tree, StructureWriter, TreeEntry, Vault};

use crate::editor::view::EditorEvent;
use crate::sidebar_rename::{RenameEvent, RenameInput};

/// 新建笔记的默认标题（与现有标题重名时自动加序号）。
const DEFAULT_NOTE_TITLE: &str = "未命名笔记";
/// 新建目录的默认标题。
const DEFAULT_FOLDER_TITLE: &str = "未命名文件夹";
/// 本机 peer id（与 dev 播种一致；多设备同步切片再引入真实 peer 身份）。
const PEER_ID: u64 = 1;
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
    /// 真正渲染的编辑器视图。
    view: Entity<EditorView>,
    /// 编辑器事件的订阅（切换文档时 drop = 取消订阅）。
    /// 字段本身不读，靠 **Drop** 生效。
    _events: gpui::Subscription,
}

/// 窗口根视图。
pub struct AppShell {
    /// 与侧栏共用同一个 vault 句柄（`Vault` 是 `Clone + Send + Sync`）。
    vault: Vault,
    /// 状态栏展示的库路径。
    vault_label: String,
    /// `list_tree` 的最近一次结果（先序 DFS，已展平）。
    tree: Vec<TreeEntry>,
    /// 当前打开的笔记；`None` 表示库里还没有可打开的笔记。
    active: Option<OpenNote>,
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
    /// 正在行内重命名的节点（含输入框实体）。
    editing: Option<Editing>,
    /// 下一帧需要把焦点给新的输入框（订阅回调里没有 `&mut Window`）。
    pending_focus_rename: bool,
    /// 菜单打开的时刻（写日志、以及"同一次手势不关菜单"的守卫用）。
    menu_opened_at: Option<std::time::Instant>,
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
        let overlay = cx.new(|cx| {
            crate::search_overlay::SearchOverlay::new(vault, PEER_ID, window, cx, on_open, on_close)
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

    /// 组装根视图：读一次树，并打开第一篇可打开的笔记。
    pub fn new(vault: Vault, vault_label: String, cx: &mut Context<Self>) -> Self {
        let mut shell = Self {
            vault,
            vault_label,
            tree: Vec::new(),
            active: None,
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
            pending_focus_rename: false,
            menu_opened_at: None,
            menu_painted: std::cell::Cell::new(false),
            epoch: std::time::Instant::now(),
            menu_focus: cx.focus_handle(),
            activation_observed: false,
            activation_subscription: None,
        };
        shell.refresh_tree();
        if let Some(index) = shell.tree.iter().position(TreeEntry::is_note) {
            shell.open_entry(index, cx);
        } else {
            eprintln!("[menghunote] 库里还没有笔记，侧栏为空（不报错）");
        }
        shell
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

    /// 右键点击侧栏某一行：**只对目录**弹菜单（右键笔记不弹，也不打开）。
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
            },
        )
        .detach();
        trace_menu!("[menghunote] 开始重命名 node={node_id} 原标题={title:?}");
        self.editing = Some(Editing { node_id, input });
        self.notice = None;
        cx.notify();
    }

    /// 提交重命名：标题没变就直接退出；否则 `rename` + commit + 刷新。
    fn commit_rename(&mut self, title: String, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else {
            return;
        };
        let node_id = editing.node_id;
        let current = self
            .tree
            .iter()
            .find(|entry| entry.node_id == node_id)
            .map(|entry| entry.title.clone());
        if current.as_deref() == Some(title.as_str()) {
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
            self.notice = None;
        }
        cx.notify();
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
    fn write_structure(
        &mut self,
        action: &str,
        op: impl FnOnce(&StructureWriter) -> Result<String, menghunote_storage::StorageError>,
    ) -> Option<String> {
        let mut writer = match StructureWriter::open(&self.vault, PEER_ID) {
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

    /// 点击侧栏第 `index` 行。
    ///
    /// 目录不打开文档；当前笔记重复点击直接忽略。
    fn on_row_click(&mut self, index: usize, cx: &mut Context<Self>) {
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
        let Some(doc_id) = entry.body_doc_id.clone() else {
            eprintln!(
                "[menghunote] 节点「{}」没有 body_doc_id，跳过（结构投影异常？）",
                entry.title
            );
            return;
        };

        // 1) 先开新文档：失败就完全不动当前视图。
        let driver = match SessionDriver::open(&self.vault, &doc_id, DriverOptions::default()) {
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
        let view = cx.new(|cx| EditorView::new(controller, driver, self.vault_label.clone(), cx));
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
            EditorEvent::SearchRequested => {
                // 编辑器只发事件；窗口在宿主手里（事件回调拿不到 `&mut Window`），
                // 因此置一个待打开标记，下一帧 render 里真正创建浮层。
                shell.pending_search = true;
                cx.notify();
            }
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
            view,
            _events: events,
        });
        self.notice = None;
        // 3) 切换完成后刷新侧栏（投影表在 commit 之后才是准的）。
        self.refresh_tree();
        cx.notify();
    }

    /// 关窗前排空当前文档（`on_window_should_close` 里调用）。
    pub fn close_all(&mut self, cx: &mut Context<Self>) {
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
                    .child(SharedString::from("笔记"))
                    .child(SharedString::from(format!("{note_count}"))),
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
            // 提示放在按钮**上方**：按钮位置固定，二次确认时不会"跑掉"
            // （踩过：提示出现在按钮下面，按钮被顶上去，第二次点击点空 → 只剩超时复位）。
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
            .child(
                div()
                    .w_full()
                    .border_t(px(1.0))
                    .border_color(theme::sidebar_border())
                    .flex()
                    .flex_row()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .id("sidebar-new-note")
                            .flex_1()
                            .h(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .text_color(theme::text_color())
                            .child(SharedString::from("＋ 笔记"))
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
                            .flex_1()
                            .h(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(12.0))
                            .text_color(theme::status_bar_text())
                            .child(SharedString::from("＋ 文件夹"))
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
                                "导出中…"
                            } else {
                                "导出"
                            }))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|shell, _event: &gpui::MouseDownEvent, _window, cx| {
                                    shell.export_all(cx);
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

        let item = |label: &'static str, color: gpui::Rgba, action: MenuAction| {
            div()
                .id(SharedString::from(format!("menu-item-{}", action.label())))
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
                        .child(item("重命名", theme::text_color(), MenuAction::Rename))
                        .when(is_folder, |this| {
                            this.child(item("新建笔记", theme::text_color(), MenuAction::NewNote))
                                .child(item(
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
                            this.child(item(label, theme::danger_text(), MenuAction::Delete))
                        }),
                )
                .into_any_element(),
        )
    }

    /// 右侧：当前编辑器；没有笔记时给占位。
    fn render_main(&self) -> impl IntoElement {
        let view = self.active.as_ref().map(|active| active.view.clone());
        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_w(px(0.0))
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
            })
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 输入框刚创建时拿不到 `&mut Window`（订阅回调里没有），在这一帧补一次焦点。
        if self.pending_focus_rename {
            self.pending_focus_rename = false;
            if let Some(editing) = &self.editing {
                let input = editing.input.clone();
                window.defer(cx, move |window, cx| {
                    input.update(cx, |input, cx| input.focus(window, cx));
                });
            }
        }
        // 窗口失焦 → 关菜单（只订阅一次，避免每帧重复注册）。
        if !self.activation_observed {
            self.activation_observed = true;
            self.activation_subscription =
                Some(cx.observe_window_activation(window, |shell, window, cx| {
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
        let main = self.render_main();
        // 浮层同样渲染在根层（侧栏会裁剪子元素）。
        let search = self.search.clone();
        // 菜单渲染在**根层**：侧栏是 `overflow_y_scroll`，放里面会被裁掉。
        let popup = self.render_menu(window, cx);
        div()
            .relative()
            .size_full()
            .flex()
            .flex_row()
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
            .child(sidebar)
            .child(main)
            .when_some(popup, |this, popup| this.child(popup))
            .when_some(search, |this, search| this.child(search))
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
