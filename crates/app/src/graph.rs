//! Wiki 关系图谱（ADR 0019 backlog `backlog-wiki-links-graph.md` 的 G0 + G1 + G2）。
//!
//! 图谱是 `links` 表的**派生视图**：不新增表、不改 schema、不参与同步、不写正文。
//! 本模块只做两件事：
//!
//! 1. [`GraphModel`]：节点 = 未删除笔记（`tree t JOIN notes n`），边 = 已解析 wiki 链接
//!    （`links.kind = 'wiki' AND to_note_id IS NOT NULL`）。顺序与坐标**完全确定**
//!    （排序后落在圆周上，不用随机数、不读时钟、不落库）；
//! 2. [`GraphViewport`]：平移/缩放的唯一坐标映射（模型坐标 → 屏幕坐标），绘制、命中、
//!    headless 断言共用它——"几何/命中/绘制共用一份坐标"是 G1 的硬要求。
//!
//! ## G2 增量（局部图 / 过滤 / 搜索 / LOD / 方向箭头）
//!
//! - [`GraphScope`]：`center = None` 是全库图；`Some(note_id)` 是以该笔记为中心、
//!   沿**无向**边 BFS `depth`（1–3，默认 [`DEFAULT_LOCAL_DEPTH`] = 2）跳以内的局部图；
//!   中心不存在或被子图过滤掉时得到空图（不 panic、不猜）；
//! - [`GraphOptions`]：按顶层文件夹 / 标签过滤 + 未解析链接虚线占位开关 + 方向箭头开关；
//!   默认全关（**保持 G1 口径**：未解析只统计、不画）；
//! - [`GraphSearch`]：标题/路径大小写不敏感子串匹配；[`GraphModel::search`] 是纯函数，
//!   覆盖层只负责"高亮 + 居中 + Enter 打开"；
//! - [`GraphLod`]：低缩放不画标签、细边、箭头（阈值见 [`LABEL_ZOOM_THRESHOLD`] 等）；
//! - [`GraphModel::derive`]：从整库模型派生当前视图（**不重查数据库**）；切换
//!   scope/过滤/深度只做 O(V+E) 派生 + 圆周重排，大库也不会卡死（上限仍由
//!   [`GraphLimits`] 兜底）。
//!
//! ## 口径（G0/G1 决策，测试钉住）
//!
//! - **自环排除**：`[[#某标题]]`（空 target 解析到自己）会解析成 `from == to`，
//!   它不成边、不进度数，只计入 [`GraphModel::self_links`]（与"未解析"同样只是统计）；
//! - **未解析不画边**：`to_note_id IS NULL` 只计入 [`GraphModel::unresolved_links`]；
//!   打开 [`GraphOptions::show_unresolved`] 后额外画**虚线 + 灰色占位**（占位不是正式
//!   节点：不进 `nodes`、不进度数、不可打开）；
//! - **去重 + 权重**：同一对 `(from, to)` 的多条链接合并成**一条**边，
//!   [`GraphEdge::weight`] = 条数；`A → B` 与 `B → A` 是两条不同的边；
//! - **悬空链接**：目标 `to_note_id` 指向已删除/不存在笔记时只计入
//!   [`GraphModel::dangling_links`]（结构树与 `links` 短暂不一致时不会 panic）；
//! - **来源必须是活节点**：来源笔记已删除（或没进节点集）的链接整行跳过，不参与统计；
//! - **上限**：节点先按 `(folder, path, title, note_id)` 排序取前 `max_nodes` 个，
//!   再把边按 `(from, to)` 排序取前 `max_edges` 条；被截断时
//!   [`GraphModel::truncation`] 给出"总数 / 实显数"，UI 只提示、不卡死；
//! - **度数**：`in_degree` / `out_degree` 只用保留下来的边计算（与画面一致）。
//!
//! 坐标只存在内存里（`GraphNode::pos`），不写任何表——布局缓存是 G3 的事。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use gpui::{point, px, Pixels, Point, Size};
use menghunote_storage::{Row, Value, Vault};

/// 模型坐标里的一个点（逻辑像素；只在内存里，不落库）。
pub type GraphPoint = Point<f32>;

// ---------------------------------------------------------------------------
// 常量（布局 / 视口 / LOD）
// ---------------------------------------------------------------------------

/// 默认节点上限（超出只画前 N 个，并提示）。
pub const DEFAULT_MAX_NODES: usize = 800;
/// 默认边上限（超出只画前 N 条，并提示）。
pub const DEFAULT_MAX_EDGES: usize = 3000;
/// 局部图深度的下限（1 跳 = 直接邻居）。
pub const MIN_LOCAL_DEPTH: usize = 1;
/// 局部图深度的上限（3 跳；再大就接近全库，收益低）。
pub const MAX_LOCAL_DEPTH: usize = 3;
/// 局部图默认深度：**2 跳**（1 跳太窄、3 跳在全库图面前没区别；2 跳兼顾"看邻居的邻居"）。
pub const DEFAULT_LOCAL_DEPTH: usize = 2;
/// 环上相邻节点的弧长（模型单位）：节点越多环越大，直径随节点数线性增长。
const NODE_ARC_SPACING: f32 = 76.0;
/// 环的最小半径（模型单位）：节点很少时也不要挤在一点。
const MIN_RING_RADIUS: f32 = 96.0;
/// `zoom = 1.0` 时的节点半径（屏幕像素）。
pub const NODE_RADIUS: f32 = 9.0;
/// 缩放后节点半径的下限/上限（屏幕像素）：低缩放仍能看见一个点，高缩放也不盖满屏幕。
const NODE_RADIUS_RANGE: (f32, f32) = (3.0, 26.0);
/// 未解析占位节点的绘制半径 = 节点半径 × 这个比例（比正式节点小一圈）。
pub const GHOST_RADIUS_SCALE: f32 = 0.7;
/// 命中半径在绘制半径之外额外放宽的像素（手指/鼠标不必精确到 1px）。
const HIT_SLOP: f32 = 4.0;
/// 缩放下限（G1：有界 0.2–3.0）。
pub const MIN_ZOOM: f32 = 0.2;
/// 缩放上限。
pub const MAX_ZOOM: f32 = 3.0;
/// 自适应缩放时四周留白（像素；顶部还要避开工具条，所以取得比内边距大）。
const FIT_PADDING: f32 = 72.0;
/// 拖拽超过这个位移才算"平移"，否则算"点击"（像素）。
pub const DRAG_THRESHOLD: f32 = 4.0;
/// 滚动缩放灵敏度：每 1 像素滚动量对应的缩放比例变化。
const SCROLL_ZOOM_SENSITIVITY: f32 = 0.0015;
/// 单次滚轮最多放大/缩小到 0.5×–2×（避免高分辨率滚轮一次跳到底）。
const SCROLL_ZOOM_FACTOR_RANGE: (f32, f32) = (0.5, 2.0);
/// 树深度解析的防御上限（结构树有环/脏数据时不要死循环）。
const MAX_DEPTH_WALK: usize = 64;
/// 低于这个缩放**不画标签**（LOD：标签在低缩放下只会互相压住）。
pub const LABEL_ZOOM_THRESHOLD: f32 = 0.55;
/// 低于这个缩放不画方向箭头（箭头在低缩放下看不清，反而糊成一片）。
pub const ARROW_ZOOM_THRESHOLD: f32 = 0.45;
/// 低于这个缩放把边画"细"（0.5px），减少大图在缩小时的图元/像素开销。
pub const THIN_EDGE_ZOOM_THRESHOLD: f32 = 0.3;
/// 视口裁剪的余量（像素）：节点中心离画布超过这个距离就不生成图元。
pub const NODE_CULL_MARGIN: f32 = 160.0;
/// 未解析占位节点围绕来源节点的第一圈半径（模型单位）。
const GHOST_ORBIT_RADIUS: f32 = 44.0;
/// 每多一圈占位节点，半径增加多少。
const GHOST_ORBIT_STEP: f32 = 14.0;
/// 一圈最多摆几个占位节点（再多就换下一圈，避免挤成一坨）。
const GHOSTS_PER_ORBIT: usize = 6;
/// 方向箭头翼长（屏幕像素）。
const ARROW_WING_LENGTH: f32 = 7.0;
/// 方向箭头翼与边方向的夹角（弧度）。
const ARROW_WING_ANGLE: f32 = 25.0 * std::f32::consts::PI / 180.0;

// ---------------------------------------------------------------------------
// 构建输入（SQL 行 → 纯模型）
// ---------------------------------------------------------------------------

/// 一篇候选笔记（`tree JOIN notes` 的一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNoteRow {
    /// 结构树节点 ID（点击时按 `note_id` 找 tree 行用不到它，但模型要带住）。
    pub node_id: String,
    /// 笔记 ID。
    pub note_id: String,
    /// 结构树里的当前标题。
    pub title: String,
    /// 父节点 ID（根级 = `None`）。
    pub parent_id: Option<String>,
    /// 该笔记的标签名（`note_tags JOIN tags`；无标签 = 空）。
    pub tags: Vec<String>,
}

/// 结构树里的**目录**行：只用于解析笔记的 `path`/`folder`/`depth`，不成为图节点。
///
/// 为什么需要它：真实库里笔记的父节点通常是目录（`tree.note_id IS NULL`），
/// 只查笔记行会让所有笔记都退化成根级（G1 的已知缺口，G2 按文件夹过滤必须补上）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphFolderRow {
    /// 结构树节点 ID。
    pub node_id: String,
    /// 目录标题。
    pub title: String,
    /// 父节点 ID（根级 = `None`）。
    pub parent_id: Option<String>,
}

/// 一条候选链接（`links` 的一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphLinkRow {
    /// 来源笔记 ID。
    pub from_note_id: String,
    /// 目标笔记 ID（`None` = 未解析）。
    pub to_note_id: Option<String>,
    /// 链接的原始 target 文本（未解析占位节点显示用）。
    pub target: String,
}

// ---------------------------------------------------------------------------
// 模型
// ---------------------------------------------------------------------------

/// 图节点（= 一篇未删除笔记）。
#[derive(Debug, Clone, PartialEq)]
pub struct GraphNode {
    /// 笔记 ID（打开笔记、查邻接都用它）。
    pub note_id: String,
    /// 结构树节点 ID。
    pub node_id: String,
    /// 结构树当前标题。
    pub title: String,
    /// 结构路径（`文件夹/子文件夹/标题`，标题按树层级拼接）。
    pub path: String,
    /// 顶层文件夹名（根级笔记为空串）；布局按它分组排序。
    pub folder: String,
    /// 结构树深度（根级 = 0）。
    pub depth: usize,
    /// 标签名（排序后；过滤用）。
    pub tags: Vec<String>,
    /// 入度（保留边里指向本节点的条数，同对去重后计数）。
    pub in_degree: usize,
    /// 出度。
    pub out_degree: usize,
    /// 确定性布局坐标（模型单位；只在内存里）。
    pub pos: GraphPoint,
}

impl GraphNode {
    /// 总度数（入 + 出）：节点大小/统计用。
    #[must_use]
    pub const fn degree(&self) -> usize {
        self.in_degree + self.out_degree
    }
}

/// 图边：同一对 `(from, to)` 的多条链接合并成一条（`weight` = 条数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphEdge {
    /// 来源节点下标（[`GraphModel::nodes`] 的下标）。
    pub from: usize,
    /// 目标节点下标。
    pub to: usize,
    /// 这一对之间的链接条数（≥ 1）。
    pub weight: usize,
}

/// 一条未解析链接（`to_note_id IS NULL`）：只统计，打开开关后画虚线占位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedLink {
    /// 来源节点下标。
    pub from: usize,
    /// 链接的原始 target 文本。
    pub target: String,
}

/// 未解析链接的灰色占位（**不是**正式节点：不进度数、不可打开）。
#[derive(Debug, Clone, PartialEq)]
pub struct GraphGhost {
    /// 来源节点下标。
    pub from: usize,
    /// 未解析的 target 文本（占位标签）。
    pub target: String,
    /// 同一 `(来源, target)` 的链接条数。
    pub weight: usize,
    /// 确定性布局坐标（围绕来源节点的小圈）。
    pub pos: GraphPoint,
}

/// 节点/边上限的截断信息（`None` = 没截断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphTruncation {
    /// 截断前的节点总数（局部图 = 范围内总数）。
    pub total_nodes: usize,
    /// 截断前的去重边总数。
    pub total_edges: usize,
    /// 实际画出的节点数。
    pub shown_nodes: usize,
    /// 实际画出的边数。
    pub shown_edges: usize,
}

/// 图谱上限（首版固定值；测试可以放宽到无上限）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphLimits {
    /// 最多画多少个节点。
    pub max_nodes: usize,
    /// 最多画多少条边。
    pub max_edges: usize,
}

impl Default for GraphLimits {
    fn default() -> Self {
        Self {
            max_nodes: DEFAULT_MAX_NODES,
            max_edges: DEFAULT_MAX_EDGES,
        }
    }
}

impl GraphLimits {
    /// 不截断（单元测试用）。
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_nodes: usize::MAX,
            max_edges: usize::MAX,
        }
    }
}

/// 图的观察范围：全库，或以某篇笔记为中心的局部图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphScope {
    /// `None` = 全库图；`Some(note_id)` = 以该笔记为中心的局部图。
    pub center: Option<String>,
    /// 局部图半径（跳数；构造后按 1–3 夹取）。
    pub depth: usize,
}

impl Default for GraphScope {
    fn default() -> Self {
        Self::full()
    }
}

impl GraphScope {
    /// 全库图。
    #[must_use]
    pub const fn full() -> Self {
        Self {
            center: None,
            depth: DEFAULT_LOCAL_DEPTH,
        }
    }

    /// 以 `note_id` 为中心的局部图（`depth` 会按 1–3 夹取）。
    #[must_use]
    pub fn local(note_id: impl Into<String>, depth: usize) -> Self {
        Self {
            center: Some(note_id.into()),
            depth: depth.clamp(MIN_LOCAL_DEPTH, MAX_LOCAL_DEPTH),
        }
    }

    /// 是否局部图。
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.center.is_some()
    }

    /// 夹取后的深度（1–3）。
    #[must_use]
    pub fn clamped_depth(&self) -> usize {
        self.depth.clamp(MIN_LOCAL_DEPTH, MAX_LOCAL_DEPTH)
    }
}

/// 过滤 + 显示选项（覆盖层里的过滤条就是它的投影）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphOptions {
    /// 顶层文件夹过滤（`None` = 全部；`Some("")` = 根级笔记）。
    pub folder: Option<String>,
    /// 标签过滤（`None` = 全部）。
    pub tag: Option<String>,
    /// 是否把未解析链接画成虚线 + 灰色占位（默认关，保持 G1 口径）。
    pub show_unresolved: bool,
    /// 是否画方向箭头（默认关）。
    pub show_arrows: bool,
}

impl GraphOptions {
    /// 这个节点是否通过过滤（文件夹 + 标签）。
    #[must_use]
    pub fn allows(&self, node: &GraphNode) -> bool {
        let folder_ok = self
            .folder
            .as_ref()
            .is_none_or(|folder| &node.folder == folder);
        let tag_ok = self
            .tag
            .as_ref()
            .is_none_or(|tag| node.tags.iter().any(|candidate| candidate == tag));
        folder_ok && tag_ok
    }

    /// 是否设置了任何"会隐藏节点"的过滤。
    #[must_use]
    pub fn is_filtering(&self) -> bool {
        self.folder.is_some() || self.tag.is_some()
    }
}

/// 搜索状态（查询文本 + 匹配下标 + 当前选中项）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphSearch {
    /// 当前查询（原样保留，空查询 = 未搜索）。
    pub query: String,
    /// 匹配的节点下标（模型顺序升序）。
    pub matches: Vec<usize>,
    /// 当前选中项在 `matches` 里的下标（`matches` 为空时无意义）。
    pub selected: usize,
}

impl GraphSearch {
    /// 用查询文本构建搜索状态（空查询 = 空匹配）。
    #[must_use]
    pub fn apply(model: &GraphModel, query: &str) -> Self {
        Self {
            query: query.to_owned(),
            matches: model.search(query),
            selected: 0,
        }
    }

    /// 当前选中的节点下标。
    #[must_use]
    pub fn selected_node(&self) -> Option<usize> {
        self.matches.get(self.selected).copied()
    }

    /// 是否有非空查询（空/纯空白 = 未搜索）。
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.query.trim().is_empty()
    }

    /// 选中下一个匹配（循环）。
    pub fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1) % self.matches.len();
        }
    }

    /// 选中上一个匹配（循环）。
    pub fn select_prev(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + self.matches.len() - 1) % self.matches.len();
        }
    }
}

/// 细节层次（LOD）：低缩放时省略标签/箭头、把边画细。
///
/// 纯函数（只依赖缩放），绘制与 headless 断言共用同一份判定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphLod {
    /// 是否画节点标签。
    pub show_labels: bool,
    /// 是否画方向箭头（还要叠加用户开关）。
    pub show_arrows: bool,
    /// 普通边的线宽（屏幕像素）。
    pub edge_width: f32,
}

impl GraphLod {
    /// 按缩放级别算 LOD。
    #[must_use]
    pub fn for_zoom(zoom: f32) -> Self {
        Self {
            show_labels: zoom >= LABEL_ZOOM_THRESHOLD,
            show_arrows: zoom >= ARROW_ZOOM_THRESHOLD,
            edge_width: if zoom < THIN_EDGE_ZOOM_THRESHOLD {
                0.5
            } else {
                1.0
            },
        }
    }
}

/// 关系图谱的纯模型（节点/边/度数/布局/过滤/搜索）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphModel {
    /// 节点（顺序固定：`(folder, path, title, note_id)` 升序）。
    nodes: Vec<GraphNode>,
    /// 边（顺序固定：`(from, to)` 升序）。
    edges: Vec<GraphEdge>,
    /// `note_id → 节点下标`。
    index: HashMap<String, usize>,
    /// 节点下标 → 该节点关联的边下标（入 + 出，升序）。
    incident: Vec<Vec<usize>>,
    /// 未解析链接（只统计 + 可选占位；来源必须是当前视图里的节点）。
    unresolved: Vec<UnresolvedLink>,
    /// 未解析链接的灰色占位（`show_unresolved = false` 时为空）。
    ghosts: Vec<GraphGhost>,
    /// 自环链接条数（解析到自己；不成边、不进度数）。
    self_links: usize,
    /// 指向已删除/不存在笔记的链接条数。
    dangling_links: usize,
    /// 截断信息（`None` = 没截断）。
    truncation: Option<GraphTruncation>,
    /// 被 scope/过滤隐藏的节点数（整库 − 当前视图）。
    hidden_nodes: usize,
    /// 被 scope/过滤隐藏的边数。
    hidden_edges: usize,
    /// 当前观察范围（全库/局部）。
    scope: GraphScope,
    /// 当前过滤 + 显示选项。
    options: GraphOptions,
}

impl GraphModel {
    /// 从真实库构建**整库模型**（不截断；上限由 [`Self::derive`] 应用）。
    ///
    /// 为什么整库模型不截断：局部图必须能从任意笔记出发——如果基底先按上限截成
    /// 前 800 个节点，中心在 800 之外的局部图就永远是空的。上限只约束**当前视图**。
    ///
    /// 只用 `vault.query` 读现有表，**不新增 storage API**：图谱是派生视图。
    pub fn build(vault: &Vault) -> anyhow::Result<Self> {
        let notes = query_note_rows(vault)?;
        let folders = query_folder_rows(vault)?;
        let links = query_link_rows(vault)?;
        Ok(Self::build_full(notes, folders, links))
    }

    /// 从真实库构建全库视图（显式上限；G1 入口）。
    pub fn build_with_limits(vault: &Vault, limits: GraphLimits) -> anyhow::Result<Self> {
        let full = Self::build(vault)?;
        Ok(full.derive(&GraphScope::full(), &GraphOptions::default(), limits))
    }

    /// 纯构建（G1 入口）：不碰数据库、不读时钟、不用随机数。
    ///
    /// 输入顺序**不影响**结果：内部先按 `(folder, path, title, note_id)` 排序，
    /// 再按排序后的下标建边/算布局。目录行缺省（笔记父链只按笔记解析）。
    #[must_use]
    pub fn from_rows(
        notes: Vec<GraphNoteRow>,
        links: Vec<GraphLinkRow>,
        limits: GraphLimits,
    ) -> Self {
        Self::from_rows_with(
            notes,
            Vec::new(),
            links,
            limits,
            GraphScope::full(),
            GraphOptions::default(),
        )
    }

    /// 纯构建 + 目录行 + 观察范围 + 过滤（G2 入口）。
    #[must_use]
    pub fn from_rows_with(
        notes: Vec<GraphNoteRow>,
        folders: Vec<GraphFolderRow>,
        links: Vec<GraphLinkRow>,
        limits: GraphLimits,
        scope: GraphScope,
        options: GraphOptions,
    ) -> Self {
        let full = Self::build_full(notes, folders, links);
        full.derive(&scope, &options, limits)
    }

    /// 整库模型（G0/G1 的全部口径都在这里；**不截断**）。
    fn build_full(
        notes: Vec<GraphNoteRow>,
        folders: Vec<GraphFolderRow>,
        links: Vec<GraphLinkRow>,
    ) -> Self {
        // 1) 父链（笔记 + 目录）：path/folder/depth 必须能穿过目录节点。
        let mut parents: ParentMap<'_> = HashMap::with_capacity(notes.len() + folders.len());
        for folder in &folders {
            parents.insert(
                folder.node_id.as_str(),
                (folder.title.as_str(), folder.parent_id.as_deref()),
            );
        }
        for note in &notes {
            parents.insert(
                note.node_id.as_str(),
                (note.title.as_str(), note.parent_id.as_deref()),
            );
        }
        let mut sorted: Vec<(String, String, usize, &GraphNoteRow)> =
            Vec::with_capacity(notes.len());
        for note in &notes {
            let (path, folder, depth) = resolve_path(note, &parents);
            sorted.push((path, folder, depth, note));
        }
        // 排序键：folder → path → title → note_id（最后一档保证完全确定）。
        sorted.sort_by(|left, right| {
            left.1
                .cmp(&right.1)
                .then_with(|| left.0.cmp(&right.0))
                .then_with(|| left.3.title.cmp(&right.3.title))
                .then_with(|| left.3.note_id.cmp(&right.3.note_id))
        });

        let total_nodes = sorted.len();

        // 2) 全量下标（截断前）：未解析/自环/悬空的判定必须按**整库**口径，
        //    否则节点上限一开，同一批链接的语义会随上限变化。
        let mut full_index: HashMap<&str, usize> = HashMap::with_capacity(total_nodes);
        for (index, (_, _, _, note)) in sorted.iter().enumerate() {
            full_index.insert(note.note_id.as_str(), index);
        }

        let mut pairs: BTreeMap<(usize, usize), usize> = BTreeMap::new();
        let mut unresolved: Vec<UnresolvedLink> = Vec::new();
        let mut self_links = 0usize;
        let mut dangling_links = 0usize;
        for link in &links {
            // 来源必须是活节点：已删除笔记的链接整行跳过（不参与任何统计）。
            let Some(&from) = full_index.get(link.from_note_id.as_str()) else {
                continue;
            };
            match link.to_note_id.as_deref() {
                None => unresolved.push(UnresolvedLink {
                    from,
                    target: link.target.clone(),
                }),
                Some(to) => match full_index.get(to).copied() {
                    None => dangling_links += 1,
                    // 自环：解析到自己（`[[#标题]]` 的常态）。选"排除并统计"。
                    Some(to) if to == from => self_links += 1,
                    Some(to) => *pairs.entry((from, to)).or_insert(0) += 1,
                },
            }
        }
        // 未解析链接的顺序也必须确定（SQL 不保证行序）。
        unresolved.sort_by(|left, right| {
            left.from
                .cmp(&right.from)
                .then_with(|| left.target.cmp(&right.target))
        });
        let total_edges = pairs.len();

        // 3) 整库模型**不截断**：节点/边上限由 `derive` 应用到当前视图。
        //    （局部图必须能从任意节点出发，见 `build` 的文档。）
        let shown_nodes = total_nodes;
        let kept_pairs: Vec<((usize, usize), usize)> = pairs.into_iter().collect();
        let shown_edges = kept_pairs.len();
        let kept_pairs = &kept_pairs[..shown_edges];

        // 4) 度数（只用保留下来的边：画面与数字必须一致）。
        let mut nodes: Vec<GraphNode> = Vec::with_capacity(shown_nodes);
        for (path, folder, depth, note) in sorted.iter().take(shown_nodes) {
            nodes.push(GraphNode {
                note_id: note.note_id.clone(),
                node_id: note.node_id.clone(),
                title: note.title.clone(),
                path: path.clone(),
                folder: folder.clone(),
                depth: *depth,
                tags: note.tags.clone(),
                in_degree: 0,
                out_degree: 0,
                pos: point(0.0, 0.0),
            });
        }
        let mut edges: Vec<GraphEdge> = Vec::with_capacity(shown_edges);
        for &((from, to), weight) in kept_pairs {
            nodes[from].out_degree += 1;
            nodes[to].in_degree += 1;
            edges.push(GraphEdge { from, to, weight });
        }

        // 5) 确定性布局：按（已排序的）节点顺序铺在圆周上。
        layout_ring(&mut nodes);

        // 6) 索引与邻接表。
        let index: HashMap<String, usize> = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.note_id.clone(), index))
            .collect();
        let mut incident: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        for (edge_index, edge) in edges.iter().enumerate() {
            incident[edge.from].push(edge_index);
            if edge.to != edge.from {
                incident[edge.to].push(edge_index);
            }
        }
        for list in &mut incident {
            list.sort_unstable();
        }

        let truncation =
            (total_nodes > shown_nodes || total_edges > shown_edges).then_some(GraphTruncation {
                total_nodes,
                total_edges,
                shown_nodes,
                shown_edges,
            });

        Self {
            nodes,
            edges,
            index,
            incident,
            unresolved,
            ghosts: Vec::new(),
            self_links,
            dangling_links,
            truncation,
            hidden_nodes: 0,
            hidden_edges: 0,
            scope: GraphScope::full(),
            options: GraphOptions::default(),
        }
    }

    /// 从整库模型派生当前视图（局部深度 + 文件夹/标签过滤 + 未解析占位）。
    ///
    /// **不重查数据库**：只做 O(V+E) 的 BFS/重排；上限仍由 `limits` 兜底。
    /// 局部图的中心不存在、或被过滤掉时得到空图（`hidden_nodes` = 整库节点数）。
    #[must_use]
    pub fn derive(&self, scope: &GraphScope, options: &GraphOptions, limits: GraphLimits) -> Self {
        let total = self.nodes.len();
        let allowed: Vec<bool> = self.nodes.iter().map(|node| options.allows(node)).collect();
        let kept: Vec<bool> = match scope.center.as_deref() {
            None => allowed,
            Some(center) => match self.index.get(center).copied() {
                Some(start) if allowed.get(start).copied().unwrap_or(false) => {
                    local_neighborhood(&self.incident, &self.edges, start, scope.clamped_depth())
                        .into_iter()
                        .zip(allowed.iter())
                        .map(|(within, allowed)| within && *allowed)
                        .collect()
                }
                _ => vec![false; total],
            },
        };
        let kept_count = kept.iter().filter(|keep| **keep).count();

        // 旧下标 → 新下标（保持模型顺序）；超出节点上限的部分不建节点。
        let mut remap = vec![usize::MAX; total];
        let mut order: Vec<usize> = Vec::with_capacity(kept_count);
        for (old, keep) in kept.iter().enumerate() {
            if *keep {
                remap[old] = order.len();
                order.push(old);
            }
        }
        let shown_nodes = kept_count.min(limits.max_nodes);
        let order = &order[..shown_nodes];

        // 边：先按"两端都在过滤后的节点集里"算**范围内总数**（G1 的 total 口径），
        // 再丢掉被节点上限截断的端点，最后按边上限截断。
        let mut kept_edges: Vec<((usize, usize), usize)> = Vec::new();
        for edge in &self.edges {
            let (from, to) = (remap[edge.from], remap[edge.to]);
            if from == usize::MAX || to == usize::MAX {
                continue;
            }
            kept_edges.push(((from, to), edge.weight));
        }
        let kept_edge_count = kept_edges.len();
        let kept_edges: Vec<((usize, usize), usize)> = kept_edges
            .into_iter()
            .filter(|((from, to), _)| *from < shown_nodes && *to < shown_nodes)
            .collect();
        let shown_edges = kept_edges.len().min(limits.max_edges);
        let kept_edges = &kept_edges[..shown_edges];

        let mut nodes: Vec<GraphNode> = Vec::with_capacity(shown_nodes);
        for &old in order {
            let node = &self.nodes[old];
            nodes.push(GraphNode {
                note_id: node.note_id.clone(),
                node_id: node.node_id.clone(),
                title: node.title.clone(),
                path: node.path.clone(),
                folder: node.folder.clone(),
                depth: node.depth,
                tags: node.tags.clone(),
                in_degree: 0,
                out_degree: 0,
                pos: point(0.0, 0.0),
            });
        }
        let mut edges: Vec<GraphEdge> = Vec::with_capacity(shown_edges);
        for &((from, to), weight) in kept_edges {
            nodes[from].out_degree += 1;
            nodes[to].in_degree += 1;
            edges.push(GraphEdge { from, to, weight });
        }
        layout_ring(&mut nodes);

        let index: HashMap<String, usize> = nodes
            .iter()
            .enumerate()
            .map(|(index, node)| (node.note_id.clone(), index))
            .collect();
        let mut incident: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
        for (edge_index, edge) in edges.iter().enumerate() {
            incident[edge.from].push(edge_index);
            if edge.to != edge.from {
                incident[edge.to].push(edge_index);
            }
        }
        for list in &mut incident {
            list.sort_unstable();
        }

        // 未解析链接：只保留来源仍在当前视图里的行（下标重映射）。
        let unresolved: Vec<UnresolvedLink> = self
            .unresolved
            .iter()
            .filter_map(|link| {
                let from = *remap.get(link.from)?;
                (from < shown_nodes).then(|| UnresolvedLink {
                    from,
                    target: link.target.clone(),
                })
            })
            .collect();
        let ghosts = if options.show_unresolved {
            layout_ghosts(&nodes, &unresolved)
        } else {
            Vec::new()
        };

        let truncation = (kept_count > shown_nodes || kept_edge_count > shown_edges).then_some(
            GraphTruncation {
                total_nodes: kept_count,
                total_edges: kept_edge_count,
                shown_nodes,
                shown_edges,
            },
        );

        Self {
            nodes,
            edges,
            index,
            incident,
            unresolved,
            ghosts,
            self_links: self.self_links,
            dangling_links: self.dangling_links,
            truncation,
            hidden_nodes: total.saturating_sub(kept_count),
            hidden_edges: self.edges.len().saturating_sub(kept_edge_count),
            scope: scope.clone(),
            options: options.clone(),
        }
    }

    /// 全部节点（顺序固定）。
    #[must_use]
    pub fn nodes(&self) -> &[GraphNode] {
        &self.nodes
    }

    /// 全部边（顺序固定）。
    #[must_use]
    pub fn edges(&self) -> &[GraphEdge] {
        &self.edges
    }

    /// 未解析链接的灰色占位（`show_unresolved = false` 时为空）。
    #[must_use]
    pub fn ghosts(&self) -> &[GraphGhost] {
        &self.ghosts
    }

    /// 占位节点数。
    #[must_use]
    pub const fn ghost_count(&self) -> usize {
        self.ghosts.len()
    }

    /// 节点数。
    #[must_use]
    pub const fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// 边数（去重后）。
    #[must_use]
    pub const fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// 空图（库里没有笔记，或全被上限/过滤截掉）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 未解析链接条数（不画边，只统计；来源必须是当前视图里的节点）。
    #[must_use]
    pub const fn unresolved_links(&self) -> usize {
        self.unresolved.len()
    }

    /// 自环链接条数（不画边，只统计）。
    #[must_use]
    pub const fn self_links(&self) -> usize {
        self.self_links
    }

    /// 指向已删除/不存在笔记的链接条数。
    #[must_use]
    pub const fn dangling_links(&self) -> usize {
        self.dangling_links
    }

    /// 截断信息（`None` = 没超过上限）。
    #[must_use]
    pub const fn truncation(&self) -> Option<GraphTruncation> {
        self.truncation
    }

    /// 被 scope/过滤隐藏的节点数。
    #[must_use]
    pub const fn hidden_nodes(&self) -> usize {
        self.hidden_nodes
    }

    /// 被 scope/过滤隐藏的边数。
    #[must_use]
    pub const fn hidden_edges(&self) -> usize {
        self.hidden_edges
    }

    /// 当前观察范围。
    #[must_use]
    pub const fn scope(&self) -> &GraphScope {
        &self.scope
    }

    /// 当前过滤 + 显示选项。
    #[must_use]
    pub const fn options(&self) -> &GraphOptions {
        &self.options
    }

    /// 当前视图里出现过的顶层文件夹（去重、升序；根级笔记不算文件夹）。
    #[must_use]
    pub fn folders(&self) -> Vec<String> {
        let mut set: BTreeSet<&str> = BTreeSet::new();
        for node in &self.nodes {
            if !node.folder.is_empty() {
                set.insert(node.folder.as_str());
            }
        }
        set.into_iter().map(str::to_owned).collect()
    }

    /// 当前视图里出现过的标签（去重、升序）。
    #[must_use]
    pub fn tags(&self) -> Vec<String> {
        let mut set: BTreeSet<&str> = BTreeSet::new();
        for node in &self.nodes {
            for tag in &node.tags {
                set.insert(tag.as_str());
            }
        }
        set.into_iter().map(str::to_owned).collect()
    }

    /// 按标题/路径做大小写不敏感子串搜索（空查询 = 空匹配）。
    ///
    /// 纯函数：不改状态、不碰数据库；覆盖层只拿结果做高亮/居中。
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<usize> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(index, node)| {
                let title = node.title.to_lowercase();
                let path = node.path.to_lowercase();
                (title.contains(&needle) || path.contains(&needle)).then_some(index)
            })
            .collect()
    }

    /// 按 `note_id` 找节点。
    #[must_use]
    pub fn node(&self, note_id: &str) -> Option<&GraphNode> {
        self.index.get(note_id).map(|index| &self.nodes[*index])
    }

    /// 按 `note_id` 找节点下标。
    #[must_use]
    pub fn node_index(&self, note_id: &str) -> Option<usize> {
        self.index.get(note_id).copied()
    }

    /// 按 `note_id` 查邻居（入边 + 出边，去重，按下标升序）。
    #[must_use]
    pub fn neighbors(&self, note_id: &str) -> Vec<&GraphNode> {
        self.neighbor_indices(note_id)
            .into_iter()
            .filter_map(|index| self.nodes.get(index))
            .collect()
    }

    /// 按 `note_id` 查邻居下标（去重，升序）。
    #[must_use]
    pub fn neighbor_indices(&self, note_id: &str) -> Vec<usize> {
        match self.index.get(note_id) {
            Some(&index) => self.neighbor_indices_of(index),
            None => Vec::new(),
        }
    }

    /// 按下标查邻居（去重，升序）。
    #[must_use]
    pub fn neighbor_indices_of(&self, index: usize) -> Vec<usize> {
        let Some(list) = self.incident.get(index) else {
            return Vec::new();
        };
        let mut out: Vec<usize> = Vec::with_capacity(list.len());
        for &edge_index in list {
            let Some(edge) = self.edges.get(edge_index) else {
                continue;
            };
            let other = if edge.from == index {
                edge.to
            } else {
                edge.from
            };
            if other != index && !out.contains(&other) {
                out.push(other);
            }
        }
        out.sort_unstable();
        out
    }

    /// 节点下标 → 关联边下标（入 + 出，升序）。
    #[must_use]
    pub fn incident_edge_indices(&self, index: usize) -> &[usize] {
        self.incident.get(index).map_or(&[], Vec::as_slice)
    }

    /// 两个节点之间是否有一条边（任一方向；自环已被排除，故 `left == right` 恒为 false）。
    #[must_use]
    pub fn are_adjacent(&self, left: usize, right: usize) -> bool {
        if left == right {
            return false;
        }
        self.incident_edge_indices(left).iter().any(|&edge_index| {
            self.edges.get(edge_index).is_some_and(|edge| {
                (edge.from == left && edge.to == right) || (edge.from == right && edge.to == left)
            })
        })
    }

    /// 布局外接半径（模型单位）：视口用它算自适应缩放；空图 = `None`。
    #[must_use]
    pub fn layout_radius(&self) -> Option<f32> {
        let mut radius = 0.0f32;
        for node in &self.nodes {
            radius = radius.max(node.pos.x.abs()).max(node.pos.y.abs());
        }
        (!self.nodes.is_empty()).then_some(radius)
    }
}

/// 头部统计行（纯函数；UI 与测试共用同一份文案）。
///
/// 空图 / 正常 / 被上限截断 / 被过滤隐藏四种形态都写清楚：
/// **只提示，不假装全部画完了**。
#[must_use]
pub fn hint_line(model: &GraphModel) -> String {
    if model.is_empty() {
        if model.hidden_nodes() > 0 {
            return format!(
                "当前过滤下没有可显示的节点（整库 {} 篇）",
                model.hidden_nodes()
            );
        }
        return "图谱是空的：先写一篇笔记，再用 [[双链]] 连起来".to_owned();
    }
    let scope = model.scope();
    let mut line = if scope.is_local() {
        format!("局部 {} 跳", scope.clamped_depth())
    } else {
        "全库".to_owned()
    };
    line.push_str(&format!(
        " · 节点 {} · 边 {} · 未解析 {}",
        model.node_count(),
        model.edge_count(),
        model.unresolved_links()
    ));
    if model.self_links() > 0 {
        line.push_str(&format!(" · 自环 {}（不画）", model.self_links()));
    }
    if model.dangling_links() > 0 {
        line.push_str(&format!(" · 悬空 {}（不画）", model.dangling_links()));
    }
    if let Some(truncation) = model.truncation() {
        let total = if scope.is_local() {
            "范围内"
        } else {
            "整库"
        };
        line.push_str(&format!(
            " · 超过显示上限，只画前 {} 个节点 / {} 条边（{total} {} / {}）",
            truncation.shown_nodes,
            truncation.shown_edges,
            truncation.total_nodes,
            truncation.total_edges
        ));
    }
    if model.hidden_nodes() > 0 {
        line.push_str(&format!(" · 已隐藏 {} 个节点", model.hidden_nodes()));
    }
    if model.options().show_unresolved && model.ghost_count() > 0 {
        line.push_str(&format!(" · 未解析占位 {}", model.ghost_count()));
    }
    line
}

// ---------------------------------------------------------------------------
// 视口（平移 / 缩放 / 命中）
// ---------------------------------------------------------------------------

/// 图谱视口：模型坐标 → 屏幕坐标的**唯一**映射。
///
/// `screen = viewport_center + pan + model * zoom`
///
/// 绘制（节点位置、边端点）、命中（[`Self::hit_test`]）、headless 断言全部走这里，
/// 不存在"画一套、点另一套"的第二份几何。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphViewport {
    /// 平移量（屏幕像素；加在"居中之后"的坐标上）。
    pub pan: Point<f32>,
    /// 缩放（恒在 [`MIN_ZOOM`, `MAX_ZOOM`] 内）。
    pub zoom: f32,
}

impl Default for GraphViewport {
    fn default() -> Self {
        Self {
            pan: point(0.0, 0.0),
            zoom: 1.0,
        }
    }
}

impl GraphViewport {
    /// 模型坐标 → 屏幕坐标。
    #[must_use]
    pub fn screen_pos(&self, pos: GraphPoint, viewport: Size<Pixels>) -> Point<Pixels> {
        let center_x = viewport.width.as_f32() * 0.5;
        let center_y = viewport.height.as_f32() * 0.5;
        point(
            px(center_x) + px(self.pan.x) + px(pos.x * self.zoom),
            px(center_y) + px(self.pan.y) + px(pos.y * self.zoom),
        )
    }

    /// 屏幕坐标 → 模型坐标（缩放锚点用；与 [`Self::screen_pos`] 互为逆运算）。
    #[must_use]
    pub fn model_pos(&self, pos: Point<Pixels>, viewport: Size<Pixels>) -> GraphPoint {
        let center_x = viewport.width.as_f32() * 0.5;
        let center_y = viewport.height.as_f32() * 0.5;
        point(
            (pos.x.as_f32() - center_x - self.pan.x) / self.zoom,
            (pos.y.as_f32() - center_y - self.pan.y) / self.zoom,
        )
    }

    /// 节点半径（屏幕像素，随缩放，落在固定区间里）。
    #[must_use]
    pub fn node_radius(&self) -> f32 {
        let (min, max) = NODE_RADIUS_RANGE;
        (NODE_RADIUS * self.zoom).clamp(min, max)
    }

    /// 命中测试：屏幕点落在哪个节点上（后画的在上，同点重叠时取靠后的节点）。
    #[must_use]
    pub fn hit_test(
        &self,
        model: &GraphModel,
        point: Point<Pixels>,
        viewport: Size<Pixels>,
    ) -> Option<usize> {
        let radius = self.node_radius() + HIT_SLOP;
        let radius_sq = radius * radius;
        let mut hit = None;
        for (index, node) in model.nodes().iter().enumerate() {
            let center = self.screen_pos(node.pos, viewport);
            let dx = center.x.as_f32() - point.x.as_f32();
            let dy = center.y.as_f32() - point.y.as_f32();
            if dx * dx + dy * dy <= radius_sq {
                hit = Some(index);
            }
        }
        hit
    }

    /// 未解析占位节点的命中测试（占位不可打开，只用来"点它不关覆盖层"）。
    #[must_use]
    pub fn ghost_hit_test(
        &self,
        model: &GraphModel,
        point: Point<Pixels>,
        viewport: Size<Pixels>,
    ) -> Option<usize> {
        let radius = self.node_radius() * GHOST_RADIUS_SCALE + HIT_SLOP;
        let radius_sq = radius * radius;
        let mut hit = None;
        for (index, ghost) in model.ghosts().iter().enumerate() {
            let center = self.screen_pos(ghost.pos, viewport);
            let dx = center.x.as_f32() - point.x.as_f32();
            let dy = center.y.as_f32() - point.y.as_f32();
            if dx * dx + dy * dy <= radius_sq {
                hit = Some(index);
            }
        }
        hit
    }

    /// 拖拽平移（屏幕像素增量）。
    pub fn pan_by(&mut self, delta: Point<f32>) {
        self.pan = point(self.pan.x + delta.x, self.pan.y + delta.y);
    }

    /// 把模型坐标 `pos` 移到视口正中（搜索定位用；只改平移，不改缩放）。
    pub fn center_on(&mut self, pos: GraphPoint) {
        self.pan = point(-pos.x * self.zoom, -pos.y * self.zoom);
    }

    /// 以 `anchor`（屏幕点）为中心缩放；缩放后该点下的模型坐标保持不变。
    ///
    /// 缩放**总有界**（[`MIN_ZOOM`, `MAX_ZOOM`]）：到达边界后再滚也只是不动，
    /// 不会出现 0 倍/负倍导致坐标爆炸或除零。
    pub fn zoom_by(&mut self, factor: f32, anchor: Point<Pixels>, viewport: Size<Pixels>) {
        let old = self.zoom;
        let new = (old * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        if (new - old).abs() <= f32::EPSILON {
            self.zoom = new;
            return;
        }
        let model = self.model_pos(anchor, viewport);
        let center_x = viewport.width.as_f32() * 0.5;
        let center_y = viewport.height.as_f32() * 0.5;
        self.pan = point(
            anchor.x.as_f32() - center_x - model.x * new,
            anchor.y.as_f32() - center_y - model.y * new,
        );
        self.zoom = new;
    }

    /// 自适应：整张图缩放进视口并居中（`pan = 0`）。
    ///
    /// 空图/单节点（外接半径 ≈ 0）时用 `zoom = 1.0`，不除零、不 panic。
    pub fn fit_to(&mut self, model: &GraphModel, viewport: Size<Pixels>) {
        self.pan = point(0.0, 0.0);
        let Some(radius) = model.layout_radius() else {
            self.zoom = 1.0;
            return;
        };
        if radius <= 0.5 {
            self.zoom = 1.0;
            return;
        }
        let width = viewport.width.as_f32() - 2.0 * FIT_PADDING;
        let height = viewport.height.as_f32() - 2.0 * FIT_PADDING;
        let usable = width.min(height).max(1.0);
        self.zoom = (usable / (2.0 * radius)).clamp(MIN_ZOOM, MAX_ZOOM);
    }
}

/// 滚轮增量 → 缩放比例（纯函数；有界）。
///
/// `delta_pixels` 为屏幕像素滚动量（向上为正 = 放大）；单次限制在
/// [`SCROLL_ZOOM_FACTOR_RANGE`]，不会因为一次高分辨率滚轮直接跳到边界。
#[must_use]
pub fn scroll_zoom_factor(delta_pixels: f32) -> f32 {
    let (min, max) = SCROLL_ZOOM_FACTOR_RANGE;
    (1.0 + delta_pixels * SCROLL_ZOOM_SENSITIVITY).clamp(min, max)
}

/// 方向箭头的几何（纯函数）：返回 `[尖端, 左翼, 右翼]`（屏幕坐标）。
///
/// `tip_offset` 是尖端离目标节点中心的距离（通常 = 节点半径 + 2），
/// 这样箭头不会画进圆点里。边太短（`len <= tip_offset + 1`）时返回 `None`。
#[must_use]
pub fn arrow_head(
    from: Point<Pixels>,
    to: Point<Pixels>,
    tip_offset: f32,
) -> Option<[Point<Pixels>; 3]> {
    let dx = to.x.as_f32() - from.x.as_f32();
    let dy = to.y.as_f32() - from.y.as_f32();
    let len = (dx * dx + dy * dy).sqrt();
    if !len.is_finite() || len <= tip_offset + 1.0 {
        return None;
    }
    let ux = dx / len;
    let uy = dy / len;
    let tip = point(
        px(to.x.as_f32() - ux * tip_offset),
        px(to.y.as_f32() - uy * tip_offset),
    );
    let (sin, cos) = ARROW_WING_ANGLE.sin_cos();
    // 两条翼 = 从尖端沿边反方向、各偏 ±wing_angle 的短线段。
    let left = point(
        px(tip.x.as_f32() - (ux * cos - uy * sin) * ARROW_WING_LENGTH),
        px(tip.y.as_f32() - (ux * sin + uy * cos) * ARROW_WING_LENGTH),
    );
    let right = point(
        px(tip.x.as_f32() - (ux * cos + uy * sin) * ARROW_WING_LENGTH),
        px(tip.y.as_f32() - (-ux * sin + uy * cos) * ARROW_WING_LENGTH),
    );
    Some([tip, left, right])
}

/// 节点中心是否完全落在视口外（视口裁剪；纯几何，不改状态）。
///
/// 留 [`NODE_CULL_MARGIN`] 余量：部分可见的节点/标签仍然绘制。
#[must_use]
pub fn node_offscreen(center: Point<Pixels>, viewport: Size<Pixels>) -> bool {
    let margin = NODE_CULL_MARGIN;
    center.x.as_f32() < -margin
        || center.y.as_f32() < -margin
        || center.x.as_f32() > viewport.width.as_f32() + margin
        || center.y.as_f32() > viewport.height.as_f32() + margin
}

// ---------------------------------------------------------------------------
// 内部：结构树 → path/folder/depth；确定性圆周布局；局部 BFS；占位布局
// ---------------------------------------------------------------------------

/// 父链解析表：`node_id → (title, parent_id)`（笔记与目录共用）。
type ParentMap<'a> = HashMap<&'a str, (&'a str, Option<&'a str>)>;

/// 沿父链拼 `path` / 顶层 `folder` / `depth`（父节点缺失或有环时停在当前层）。
fn resolve_path(note: &GraphNoteRow, parents: &ParentMap<'_>) -> (String, String, usize) {
    let mut titles = vec![note.title.clone()];
    let mut current = note.parent_id.as_deref();
    let mut guard = 0usize;
    while let Some(parent_id) = current {
        guard += 1;
        if guard > MAX_DEPTH_WALK || guard > parents.len() {
            break;
        }
        let Some(&(title, parent)) = parents.get(parent_id) else {
            break;
        };
        titles.push(title.to_owned());
        current = parent;
    }
    titles.reverse();
    let depth = titles.len().saturating_sub(1);
    let path = titles.join("/");
    // 顶层祖先的标题就是"文件夹"；根级笔记没有文件夹。
    let folder = if depth == 0 {
        String::new()
    } else {
        titles.first().cloned().unwrap_or_default()
    };
    (path, folder, depth)
}

/// 确定性布局：第 `i` 个节点落在圆周上（从正上方开始顺时针），半径随节点数增长。
///
/// - 0 个节点：什么都不做；
/// - 1 个节点：正中 `(0, 0)`（不除零）；
/// - n ≥ 2：`R = max(MIN_RING_RADIUS, n * NODE_ARC_SPACING / 2π)`。
fn layout_ring(nodes: &mut [GraphNode]) {
    let count = nodes.len();
    if count == 0 {
        return;
    }
    if count == 1 {
        nodes[0].pos = point(0.0, 0.0);
        return;
    }
    let count_f = count as f32;
    let circumference = count_f * NODE_ARC_SPACING;
    let radius = (circumference / (2.0 * std::f32::consts::PI)).max(MIN_RING_RADIUS);
    let step = 2.0 * std::f32::consts::PI / count_f;
    let start = -std::f32::consts::FRAC_PI_2;
    for (index, node) in nodes.iter_mut().enumerate() {
        let angle = start + step * index as f32;
        node.pos = point(radius * angle.cos(), radius * angle.sin());
    }
}

/// 从 `start` 出发沿**无向**边做 BFS，返回 `dist <= depth` 的节点标记。
///
/// 只走 `incident` 里已有的边（自环已被排除）；`depth` 由调用方夹取。
fn local_neighborhood(
    incident: &[Vec<usize>],
    edges: &[GraphEdge],
    start: usize,
    depth: usize,
) -> Vec<bool> {
    let mut within = vec![false; incident.len()];
    if incident.get(start).is_none() {
        return within;
    }
    let mut dist = vec![usize::MAX; incident.len()];
    dist[start] = 0;
    within[start] = true;
    let mut queue = VecDeque::with_capacity(incident.len().min(64));
    queue.push_back(start);
    while let Some(current) = queue.pop_front() {
        if dist[current] >= depth {
            continue;
        }
        for &edge_index in &incident[current] {
            let Some(edge) = edges.get(edge_index) else {
                continue;
            };
            let other = if edge.from == current {
                edge.to
            } else {
                edge.from
            };
            if other == current || dist[other] != usize::MAX {
                continue;
            }
            dist[other] = dist[current] + 1;
            within[other] = true;
            queue.push_back(other);
        }
    }
    within
}

/// 未解析链接的占位布局：按来源分组，围绕来源节点摆小圈（确定性）。
///
/// 同一来源的占位按 target 文本升序摆位（`BTreeMap`），因此同一输入两次派生
/// 坐标逐字节一致；不同来源之间互不影响。
fn layout_ghosts(nodes: &[GraphNode], unresolved: &[UnresolvedLink]) -> Vec<GraphGhost> {
    let mut grouped: BTreeMap<usize, BTreeMap<&str, usize>> = BTreeMap::new();
    for link in unresolved {
        *grouped
            .entry(link.from)
            .or_default()
            .entry(link.target.as_str())
            .or_insert(0) += 1;
    }
    let mut ghosts = Vec::new();
    for (from, targets) in grouped {
        let Some(source) = nodes.get(from) else {
            continue;
        };
        let count = targets.len();
        for (index, (target, weight)) in targets.into_iter().enumerate() {
            let angle = -std::f32::consts::FRAC_PI_2
                + 2.0 * std::f32::consts::PI * index as f32 / count as f32;
            let radius = GHOST_ORBIT_RADIUS + GHOST_ORBIT_STEP * (index / GHOSTS_PER_ORBIT) as f32;
            ghosts.push(GraphGhost {
                from,
                target: target.to_owned(),
                weight,
                pos: point(
                    source.pos.x + radius * angle.cos(),
                    source.pos.y + radius * angle.sin(),
                ),
            });
        }
    }
    ghosts
}

// ---------------------------------------------------------------------------
// 内部：SQL 读取（只用现有表/列）
// ---------------------------------------------------------------------------

fn query_note_rows(vault: &Vault) -> anyhow::Result<Vec<GraphNoteRow>> {
    let rows = vault.query(
        "SELECT t.node_id, t.note_id, t.title, t.parent_id \
         FROM tree t JOIN notes n ON n.note_id = t.note_id \
         WHERE t.deleted = 0 AND n.deleted = 0",
        &[],
    )?;
    // 标签：`note_tags` 是派生投影（projector 写），这里只读。
    let mut tags: HashMap<String, Vec<String>> = HashMap::new();
    for row in vault.query(
        "SELECT nt.note_id, t.name FROM note_tags nt JOIN tags t ON t.tag_id = nt.tag_id \
         ORDER BY nt.note_id, t.name",
        &[],
    )? {
        let note_id = row_text(&row, 0, "note_tags.note_id")?;
        let name = row_text(&row, 1, "tags.name")?;
        tags.entry(note_id).or_default().push(name);
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let note_id = row_text(row, 1, "tree.note_id")?;
        out.push(GraphNoteRow {
            node_id: row_text(row, 0, "tree.node_id")?,
            tags: tags.get(&note_id).cloned().unwrap_or_default(),
            note_id,
            title: row_text(row, 2, "tree.title")?,
            parent_id: row_optional_text(row, 3, "tree.parent_id")?,
        });
    }
    Ok(out)
}

fn query_folder_rows(vault: &Vault) -> anyhow::Result<Vec<GraphFolderRow>> {
    let rows = vault.query(
        "SELECT node_id, title, parent_id FROM tree WHERE deleted = 0 AND note_id IS NULL",
        &[],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(GraphFolderRow {
            node_id: row_text(row, 0, "tree.node_id")?,
            title: row_text(row, 1, "tree.title")?,
            parent_id: row_optional_text(row, 2, "tree.parent_id")?,
        });
    }
    Ok(out)
}

fn query_link_rows(vault: &Vault) -> anyhow::Result<Vec<GraphLinkRow>> {
    let rows = vault.query(
        "SELECT from_note_id, to_note_id, target FROM links WHERE kind = 'wiki'",
        &[],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(GraphLinkRow {
            from_note_id: row_text(row, 0, "links.from_note_id")?,
            to_note_id: row_optional_text(row, 1, "links.to_note_id")?,
            target: row_text(row, 2, "links.target")?,
        });
    }
    Ok(out)
}

fn row_text(row: &Row, index: usize, field: &str) -> anyhow::Result<String> {
    match row.get(index) {
        Some(Value::Text(value)) => Ok(value.clone()),
        other => anyhow::bail!("图谱读取 {field} 类型异常：{other:?}"),
    }
}

fn row_optional_text(row: &Row, index: usize, field: &str) -> anyhow::Result<Option<String>> {
    match row.get(index) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::Text(value)) => Ok(Some(value.clone())),
        other => anyhow::bail!("图谱读取 {field} 类型异常：{other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 单元测试（纯模型：不建库、不碰 SQLite）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn note(node_id: &str, note_id: &str, title: &str, parent: Option<&str>) -> GraphNoteRow {
        GraphNoteRow {
            node_id: node_id.to_owned(),
            note_id: note_id.to_owned(),
            title: title.to_owned(),
            parent_id: parent.map(str::to_owned),
            tags: Vec::new(),
        }
    }

    fn tagged_note(
        node_id: &str,
        note_id: &str,
        title: &str,
        parent: Option<&str>,
        tags: &[&str],
    ) -> GraphNoteRow {
        GraphNoteRow {
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            ..note(node_id, note_id, title, parent)
        }
    }

    fn folder(node_id: &str, title: &str, parent: Option<&str>) -> GraphFolderRow {
        GraphFolderRow {
            node_id: node_id.to_owned(),
            title: title.to_owned(),
            parent_id: parent.map(str::to_owned),
        }
    }

    fn link(from: &str, to: Option<&str>) -> GraphLinkRow {
        GraphLinkRow {
            from_note_id: from.to_owned(),
            to_note_id: to.map(str::to_owned),
            target: to.unwrap_or("未解析").to_owned(),
        }
    }

    fn build(notes: Vec<GraphNoteRow>, links: Vec<GraphLinkRow>) -> GraphModel {
        GraphModel::from_rows(notes, links, GraphLimits::unlimited())
    }

    fn build_with(
        notes: Vec<GraphNoteRow>,
        folders: Vec<GraphFolderRow>,
        links: Vec<GraphLinkRow>,
        scope: GraphScope,
        options: GraphOptions,
    ) -> GraphModel {
        GraphModel::from_rows_with(
            notes,
            folders,
            links,
            GraphLimits::unlimited(),
            scope,
            options,
        )
    }

    fn titles(model: &GraphModel) -> Vec<&str> {
        model
            .nodes()
            .iter()
            .map(|node| node.title.as_str())
            .collect()
    }

    #[test]
    fn empty_graph_is_empty_and_has_no_layout() {
        let model = build(Vec::new(), vec![link("a", Some("b"))]);
        assert!(model.is_empty());
        assert_eq!(model.node_count(), 0);
        assert_eq!(model.edge_count(), 0);
        assert_eq!(model.layout_radius(), None);
        assert_eq!(
            model.unresolved_links(),
            0,
            "没有活节点的库不该因为脏 links 行改变统计"
        );
        assert!(model.neighbors("a").is_empty());
        assert!(hint_line(&model).contains("空的"));
        assert_eq!(model.ghost_count(), 0);
        assert_eq!(model.hidden_nodes(), 0);
    }

    #[test]
    fn single_note_sits_at_origin_without_panic() {
        let model = build(vec![note("t1", "n1", "只有一篇", None)], Vec::new());
        assert_eq!(model.node_count(), 1);
        assert_eq!(model.edge_count(), 0);
        let node = model.node("n1").expect("节点存在");
        assert_eq!(node.degree(), 0);
        assert_eq!(node.path, "只有一篇");
        assert_eq!(node.folder, "");
        assert_eq!(node.depth, 0);
        assert_eq!(node.pos, point(0.0, 0.0), "单节点必须落在正中");
        // 单节点也不能把视口算炸。
        let mut viewport = GraphViewport::default();
        viewport.fit_to(&model, Size::new(px(800.0), px(600.0)));
        assert!((viewport.zoom - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn cycle_is_kept_as_triangle_without_self_loops() {
        // a → b → c → a 的环：三条边都要在。
        let model = build(
            vec![
                note("t1", "a", "A", None),
                note("t2", "b", "B", None),
                note("t3", "c", "C", None),
            ],
            vec![
                link("a", Some("b")),
                link("b", Some("c")),
                link("c", Some("a")),
            ],
        );
        assert_eq!(model.node_count(), 3);
        assert_eq!(model.edge_count(), 3);
        assert_eq!(model.self_links(), 0, "环不是自环");
        for note_id in ["a", "b", "c"] {
            let node = model.node(note_id).expect(note_id);
            assert_eq!(node.degree(), 2, "{note_id} 在环里入 1 出 1");
            assert_eq!(node.in_degree, 1);
            assert_eq!(node.out_degree, 1);
            assert_eq!(model.neighbors(note_id).len(), 2);
        }
        // 邻接关系对称：a 的邻居里必须有 b、c。
        let mut neighbors: Vec<&str> = model
            .neighbors("a")
            .iter()
            .map(|node| node.note_id.as_str())
            .collect();
        neighbors.sort_unstable();
        assert_eq!(neighbors, ["b", "c"]);
        assert!(model.are_adjacent(
            model.node_index("a").expect("a"),
            model.node_index("b").expect("b")
        ));
    }

    #[test]
    fn parallel_links_are_deduped_with_weight_and_direction_kept() {
        // a → b 两条（同对多链接）+ b → a 一条：两条有向边，权重分别 2 / 1。
        let model = build(
            vec![note("t1", "a", "A", None), note("t2", "b", "B", None)],
            vec![
                link("a", Some("b")),
                link("a", Some("b")),
                link("b", Some("a")),
            ],
        );
        assert_eq!(model.edge_count(), 2, "同对去重，反方向另算一条");
        let a = model.node_index("a").expect("a");
        let b = model.node_index("b").expect("b");
        let mut weights = Vec::new();
        for edge in model.edges() {
            if edge.from == a && edge.to == b {
                weights.push(edge.weight);
            }
        }
        assert_eq!(weights, [2], "同一对的多条链接合并成一条、权重 = 条数");
        let reverse = model
            .edges()
            .iter()
            .find(|edge| edge.from == b && edge.to == a)
            .expect("反方向是另一条边");
        assert_eq!(reverse.weight, 1);
        assert_eq!(model.node("a").expect("a").degree(), 2);
        assert_eq!(model.node("b").expect("b").degree(), 2);
        assert_eq!(model.neighbors("a").len(), 1, "邻居去重");
    }

    #[test]
    fn self_loops_are_excluded_and_counted() {
        // `[[#标题]]` 解析到自己：排除出正式边，但要留统计。
        let model = build(
            vec![note("t1", "a", "A", None), note("t2", "b", "B", None)],
            vec![link("a", Some("a")), link("a", Some("b"))],
        );
        assert_eq!(model.edge_count(), 1);
        assert_eq!(model.self_links(), 1);
        let a = model.node("a").expect("a");
        assert_eq!(a.out_degree, 1);
        assert_eq!(a.in_degree, 0, "自环不进度数");
        assert_eq!(model.neighbors("a").len(), 1);
        assert!(!model.are_adjacent(0, 0));
        assert!(hint_line(&model).contains("自环 1"));
    }

    #[test]
    fn unresolved_and_dangling_links_are_statistics_only() {
        let model = build(
            vec![note("t1", "a", "A", None), note("t2", "b", "B", None)],
            vec![
                link("a", None),            // 未解析
                link("a", Some("missing")), // 悬空（目标不存在）
                link("a", Some("b")),
            ],
        );
        assert_eq!(model.edge_count(), 1);
        assert_eq!(model.unresolved_links(), 1);
        assert_eq!(model.dangling_links(), 1);
        assert_eq!(model.ghost_count(), 0, "默认不画未解析占位（G1 口径）");
        let hint = hint_line(&model);
        assert!(hint.contains("未解析 1"), "{hint}");
        assert!(hint.contains("悬空 1"), "{hint}");
    }

    #[test]
    fn links_from_deleted_sources_are_ignored() {
        let model = build(
            vec![note("t1", "a", "A", None)],
            vec![link("gone", None), link("gone", Some("a"))],
        );
        assert_eq!(model.node_count(), 1);
        assert_eq!(model.edge_count(), 0);
        assert_eq!(model.unresolved_links(), 0);
        assert_eq!(model.dangling_links(), 0);
    }

    #[test]
    fn layout_is_deterministic_and_grouped_by_folder() {
        let notes = vec![
            note("t1", "z", "Z", None),
            note("t2", "f1", "文件夹", None),
            note("t3", "z1", "子篇", Some("t2")),
            note("t4", "a", "A", None),
        ];
        let links = vec![link("z", Some("z1"))];
        let first = build(notes.clone(), links.clone());
        let second = build(notes, links);
        // 排序键 = (folder, path, title, note_id)：根级笔记 folder 为空串排前面。
        let paths: Vec<&str> = first
            .nodes()
            .iter()
            .map(|node| node.path.as_str())
            .collect();
        assert_eq!(paths, ["A", "Z", "文件夹", "文件夹/子篇"]);
        let positions: Vec<GraphPoint> = first.nodes().iter().map(|node| node.pos).collect();
        let again: Vec<GraphPoint> = second.nodes().iter().map(|node| node.pos).collect();
        assert_eq!(positions, again, "同一输入两次构建坐标必须一致");
        // 圆周：到原点的距离相等，且不在原点。
        let radii: Vec<f32> = positions
            .iter()
            .map(|pos| (pos.x * pos.x + pos.y * pos.y).sqrt())
            .collect();
        let radius = radii[0];
        assert!(radius > 1.0);
        for value in radii {
            assert!((value - radius).abs() < 0.01, "所有节点应在同一圆周上");
        }
        assert_ne!(positions[0], positions[1]);
        // 输入顺序无关：打乱 nodes 后构建结果不变。
        let shuffled = vec![
            note("t3", "z1", "子篇", Some("t2")),
            note("t1", "z", "Z", None),
            note("t4", "a", "A", None),
            note("t2", "f1", "文件夹", None),
        ];
        let third = build(shuffled, Vec::new());
        let third_paths: Vec<&str> = third
            .nodes()
            .iter()
            .map(|node| node.path.as_str())
            .collect();
        assert_eq!(third_paths, paths, "输入顺序不得影响模型顺序");
    }

    #[test]
    fn path_and_folder_are_built_from_the_tree() {
        let model = build(
            vec![
                note("t1", "root", "顶层", None),
                note("t2", "folder", "资料", None),
                note("t3", "sub", "子目录", Some("t2")),
                note("t4", "deep", "深篇", Some("t3")),
            ],
            Vec::new(),
        );
        let deep = model.node("deep").expect("deep");
        assert_eq!(deep.path, "资料/子目录/深篇");
        assert_eq!(deep.folder, "资料", "folder 取顶层文件夹");
        assert_eq!(deep.depth, 2);
        let root = model.node("root").expect("root");
        assert_eq!(root.folder, "");
        assert_eq!(root.depth, 0);
    }

    #[test]
    fn path_crosses_real_folder_rows() {
        // 真实库里笔记的父节点是**目录**（tree.note_id IS NULL）：必须能穿过。
        let model = build_with(
            vec![
                note("n1", "a", "工作笔记", Some("dir-work")),
                note("n2", "b", "生活笔记", Some("dir-life")),
                note("n3", "c", "根笔记", None),
            ],
            vec![
                folder("dir-work", "工作", None),
                folder("dir-life", "生活", None),
            ],
            Vec::new(),
            GraphScope::full(),
            GraphOptions::default(),
        );
        let work = model.node("a").expect("工作笔记");
        assert_eq!(work.path, "工作/工作笔记");
        assert_eq!(work.folder, "工作");
        assert_eq!(work.depth, 1);
        let root = model.node("c").expect("根笔记");
        assert_eq!(root.path, "根笔记");
        assert_eq!(root.folder, "");
        assert_eq!(model.folders(), ["工作", "生活"]);
    }

    #[test]
    fn limits_truncate_nodes_and_edges_with_counts() {
        let notes = (0..4)
            .map(|index| {
                note(
                    &format!("t{index}"),
                    &format!("n{index}"),
                    &format!("N{index}"),
                    None,
                )
            })
            .collect();
        let links = vec![
            link("n0", Some("n1")),
            link("n1", Some("n2")),
            link("n2", Some("n3")),
        ];
        let limits = GraphLimits {
            max_nodes: 2,
            max_edges: 1,
        };
        let model = GraphModel::from_rows(notes, links, limits);
        assert_eq!(model.node_count(), 2);
        assert_eq!(model.edge_count(), 1);
        let truncation = model.truncation().expect("必须报告截断");
        assert_eq!(truncation.total_nodes, 4);
        assert_eq!(truncation.total_edges, 3);
        assert_eq!(truncation.shown_nodes, 2);
        assert_eq!(truncation.shown_edges, 1);
        let hint = hint_line(&model);
        assert!(hint.contains("超过显示上限"), "{hint}");
        assert!(hint.contains("整库 4 / 3"), "{hint}");
        // 度数只算保留下来的边（画面与数字一致）。
        assert_eq!(model.node("n0").expect("n0").out_degree, 1);
        assert_eq!(model.node("n1").expect("n1").in_degree, 1);
    }

    #[test]
    fn viewport_hit_test_matches_screen_positions() {
        let model = build(
            vec![note("t1", "a", "A", None), note("t2", "b", "B", None)],
            vec![link("a", Some("b"))],
        );
        let size = Size::new(px(800.0), px(600.0));
        let mut viewport = GraphViewport::default();
        viewport.fit_to(&model, size);
        assert!(viewport.zoom >= MIN_ZOOM && viewport.zoom <= MAX_ZOOM);
        for (index, node) in model.nodes().iter().enumerate() {
            let center = viewport.screen_pos(node.pos, size);
            assert_eq!(
                viewport.hit_test(&model, center, size),
                Some(index),
                "命中必须与绘制的中心一致"
            );
            // 中心外 1px 也算命中（半径 + 容差）。
            let near = point(center.x + px(1.0), center.y);
            assert_eq!(viewport.hit_test(&model, near, size), Some(index));
        }
        // 远离任何节点 = 没命中（点在画布外/正中都不该误命中）。
        let corner = point(px(5.0), px(5.0));
        assert_eq!(viewport.hit_test(&model, corner, size), None);
    }

    #[test]
    fn zoom_is_bounded_and_keeps_the_anchor_fixed() {
        let size = Size::new(px(800.0), px(600.0));
        let anchor = point(px(600.0), px(200.0));
        let mut viewport = GraphViewport::default();
        let model_before = viewport.model_pos(anchor, size);
        for _ in 0..40 {
            viewport.zoom_by(1.5, anchor, size);
        }
        assert!(
            (viewport.zoom - MAX_ZOOM).abs() < f32::EPSILON,
            "上限必须夹住"
        );
        let model_after = viewport.model_pos(anchor, size);
        assert!((model_after.x - model_before.x).abs() < 0.5);
        assert!((model_after.y - model_before.y).abs() < 0.5);
        for _ in 0..80 {
            viewport.zoom_by(0.5, anchor, size);
        }
        assert!(
            (viewport.zoom - MIN_ZOOM).abs() < f32::EPSILON,
            "下限必须夹住"
        );
        assert!(viewport.zoom > 0.0, "缩放不能出现 0/负数（除零防护）");
        let model_after = viewport.model_pos(anchor, size);
        assert!((model_after.x - model_before.x).abs() < 0.5);
        assert!((model_after.y - model_before.y).abs() < 0.5);
    }

    #[test]
    fn pan_offsets_every_node_by_the_same_delta() {
        let model = build(
            vec![note("t1", "a", "A", None), note("t2", "b", "B", None)],
            Vec::new(),
        );
        let size = Size::new(px(800.0), px(600.0));
        let mut viewport = GraphViewport::default();
        let before: Vec<Point<Pixels>> = model
            .nodes()
            .iter()
            .map(|node| viewport.screen_pos(node.pos, size))
            .collect();
        viewport.pan_by(point(37.0, -11.0));
        for (index, node) in model.nodes().iter().enumerate() {
            let after = viewport.screen_pos(node.pos, size);
            assert!((after.x - before[index].x - px(37.0)).as_f32().abs() < 0.01);
            assert!((after.y - before[index].y + px(11.0)).as_f32().abs() < 0.01);
        }
    }

    #[test]
    fn scroll_zoom_factor_is_bounded() {
        assert!((scroll_zoom_factor(0.0) - 1.0).abs() < f32::EPSILON);
        assert!(scroll_zoom_factor(10_000.0) <= 2.0);
        assert!(scroll_zoom_factor(-10_000.0) >= 0.5);
        assert!(scroll_zoom_factor(60.0) > 1.0);
        assert!(scroll_zoom_factor(-60.0) < 1.0);
    }

    #[test]
    fn empty_model_hit_test_and_fit_do_not_panic() {
        let model = GraphModel::default();
        let size = Size::new(px(800.0), px(600.0));
        let mut viewport = GraphViewport::default();
        viewport.fit_to(&model, size);
        assert!((viewport.zoom - 1.0).abs() < f32::EPSILON);
        assert_eq!(
            viewport.hit_test(&model, point(px(400.0), px(300.0)), size),
            None
        );
        viewport.zoom_by(0.1, point(px(0.0), px(0.0)), size);
        assert!(viewport.zoom >= MIN_ZOOM);
        assert_eq!(
            viewport.ghost_hit_test(&model, point(px(0.0), px(0.0)), size),
            None
        );
    }

    // ---- G2：局部图 / 过滤 / 搜索 / LOD / 箭头 ----

    #[test]
    fn local_scope_keeps_undirected_neighborhood_within_depth() {
        // 链 a → b → c → d → e（无向 BFS 也应能往回走）。
        let notes = vec![
            note("t1", "a", "A", None),
            note("t2", "b", "B", None),
            note("t3", "c", "C", None),
            note("t4", "d", "D", None),
            note("t5", "e", "E", None),
        ];
        let links = vec![
            link("a", Some("b")),
            link("b", Some("c")),
            link("c", Some("d")),
            link("d", Some("e")),
        ];
        let at = |depth: usize| {
            build_with(
                notes.clone(),
                Vec::new(),
                links.clone(),
                GraphScope::local("b", depth),
                GraphOptions::default(),
            )
        };
        assert_eq!(titles(&at(1)), ["A", "B", "C"], "1 跳 = 两个直接邻居");
        assert_eq!(titles(&at(2)), ["A", "B", "C", "D"], "2 跳 = 邻居的邻居");
        assert_eq!(titles(&at(3)), ["A", "B", "C", "D", "E"]);
        // 深度按 1–3 夹取（0 → 1，99 → 3）。
        assert_eq!(titles(&at(0)), ["A", "B", "C"]);
        assert_eq!(titles(&at(99)), ["A", "B", "C", "D", "E"]);
        // 局部图的边只保留两端都在范围内的。
        let depth_one = at(1);
        assert_eq!(depth_one.edge_count(), 2);
        assert_eq!(depth_one.hidden_nodes(), 2);
        assert_eq!(depth_one.scope().clamped_depth(), 1);
        assert!(hint_line(&depth_one).contains("局部 1 跳"));
        // 全库图不受影响。
        let full = build(notes, links);
        assert_eq!(full.node_count(), 5);
        assert_eq!(full.hidden_nodes(), 0);
    }

    #[test]
    fn local_scope_missing_center_or_filtered_out_is_empty() {
        let notes = vec![note("t1", "a", "A", None), note("t2", "b", "B", None)];
        let links = vec![link("a", Some("b"))];
        // 中心不存在 → 空图，不 panic。
        let missing = build_with(
            notes.clone(),
            Vec::new(),
            links.clone(),
            GraphScope::local("missing", 2),
            GraphOptions::default(),
        );
        assert!(missing.is_empty());
        assert_eq!(missing.hidden_nodes(), 2);
        assert!(hint_line(&missing).contains("没有可显示的节点"));
        // 中心被文件夹过滤掉 → 空图（口径：过滤先于 BFS）。
        let filtered = build_with(
            notes,
            Vec::new(),
            links,
            GraphScope::local("a", 2),
            GraphOptions {
                folder: Some("工作".to_owned()),
                ..GraphOptions::default()
            },
        );
        assert!(filtered.is_empty());
        assert_eq!(filtered.hidden_nodes(), 2);
    }

    #[test]
    fn folder_filter_keeps_top_level_folder() {
        let notes = vec![
            note("n1", "a", "工作笔记", Some("dir-work")),
            note("n2", "b", "生活笔记", Some("dir-life")),
            note("n3", "c", "根笔记", None),
        ];
        let folders = vec![
            folder("dir-work", "工作", None),
            folder("dir-life", "生活", None),
        ];
        let work = build_with(
            notes.clone(),
            folders.clone(),
            Vec::new(),
            GraphScope::full(),
            GraphOptions {
                folder: Some("工作".to_owned()),
                ..GraphOptions::default()
            },
        );
        assert_eq!(titles(&work), ["工作笔记"]);
        assert_eq!(work.hidden_nodes(), 2);
        assert!(hint_line(&work).contains("已隐藏 2 个节点"));
        // `Some("")` = 根级笔记。
        let root = build_with(
            notes,
            folders,
            Vec::new(),
            GraphScope::full(),
            GraphOptions {
                folder: Some(String::new()),
                ..GraphOptions::default()
            },
        );
        assert_eq!(titles(&root), ["根笔记"]);
    }

    #[test]
    fn tag_filter_keeps_tagged_notes() {
        let notes = vec![
            tagged_note("n1", "a", "工作笔记", None, &["工作", "重要"]),
            tagged_note("n2", "b", "生活笔记", None, &["生活"]),
            note("n3", "c", "无标签", None),
        ];
        // 整库模型（未过滤）暴露全部标签，覆盖层的标签 chips 用它。
        let full = build(notes.clone(), Vec::new());
        assert_eq!(full.tags(), ["工作", "生活", "重要"]);
        let model = build_with(
            notes,
            Vec::new(),
            Vec::new(),
            GraphScope::full(),
            GraphOptions {
                tag: Some("工作".to_owned()),
                ..GraphOptions::default()
            },
        );
        assert_eq!(titles(&model), ["工作笔记"]);
        assert_eq!(model.hidden_nodes(), 2);
        assert_eq!(model.tags(), ["工作", "重要"], "视图内的标签");
    }

    #[test]
    fn unresolved_ghosts_are_optional_and_deterministic() {
        let notes = vec![note("t1", "a", "A", None), note("t2", "b", "B", None)];
        let links = vec![
            link("a", Some("b")),
            link("a", None),
            link("a", None), // 同一 target 两条 → 一个占位、权重 2
        ];
        let hidden = build_with(
            notes.clone(),
            Vec::new(),
            links.clone(),
            GraphScope::full(),
            GraphOptions::default(),
        );
        assert_eq!(hidden.ghost_count(), 0, "默认关：只统计不画");
        assert_eq!(hidden.unresolved_links(), 2);
        let options = GraphOptions {
            show_unresolved: true,
            ..GraphOptions::default()
        };
        let shown = build_with(
            notes.clone(),
            Vec::new(),
            links.clone(),
            GraphScope::full(),
            options.clone(),
        );
        assert_eq!(shown.ghost_count(), 1, "同一 (来源, target) 只占一个位");
        assert_eq!(shown.ghosts()[0].weight, 2);
        assert_eq!(shown.ghosts()[0].target, "未解析");
        assert_eq!(shown.edge_count(), 1, "占位不是正式边");
        assert_eq!(shown.node_count(), 2, "占位不是正式节点");
        assert!(hint_line(&shown).contains("未解析占位 1"));
        // 确定性：同一输入两次派生坐标一致。
        let again = build_with(notes, Vec::new(), links, GraphScope::full(), options);
        assert_eq!(
            shown
                .ghosts()
                .iter()
                .map(|ghost| ghost.pos)
                .collect::<Vec<_>>(),
            again
                .ghosts()
                .iter()
                .map(|ghost| ghost.pos)
                .collect::<Vec<_>>()
        );
        // 占位命中：点它不会误命中正式节点。
        let size = Size::new(px(800.0), px(600.0));
        let mut viewport = GraphViewport::default();
        viewport.fit_to(&shown, size);
        let ghost_center = viewport.screen_pos(shown.ghosts()[0].pos, size);
        assert_eq!(viewport.ghost_hit_test(&shown, ghost_center, size), Some(0));
        assert_eq!(viewport.hit_test(&shown, ghost_center, size), None);
    }

    #[test]
    fn search_matches_title_and_path_case_insensitively() {
        let notes = vec![
            note("n1", "a", "Alpha", None),
            note("n2", "b", "Beta", Some("dir")),
        ];
        let folders = vec![folder("dir", "Folder", None)];
        let model = build_with(
            notes,
            folders,
            Vec::new(),
            GraphScope::full(),
            GraphOptions::default(),
        );
        assert_eq!(model.search("alpha"), [0], "标题大小写不敏感");
        assert_eq!(model.search("ALPHA"), [0]);
        assert_eq!(model.search("folder"), [1], "路径也能搜到");
        assert_eq!(model.search("Folder/Beta"), [1]);
        assert!(model.search("").is_empty(), "空查询不匹配");
        assert!(model.search("   ").is_empty(), "纯空白不匹配");
        assert!(model.search("zzz").is_empty());
        // 搜索状态：默认选中第一个匹配。
        let search = GraphSearch::apply(&model, "a");
        assert!(search.is_active());
        assert_eq!(search.selected_node(), search.matches.first().copied());
        assert!(!GraphSearch::default().is_active());
        assert_eq!(GraphSearch::default().selected_node(), None);
    }

    #[test]
    fn graph_search_selection_wraps() {
        let mut search = GraphSearch {
            query: "x".to_owned(),
            matches: vec![2, 5, 9],
            selected: 0,
        };
        assert_eq!(search.selected_node(), Some(2));
        search.select_next();
        assert_eq!(search.selected_node(), Some(5));
        search.select_next();
        search.select_next();
        assert_eq!(search.selected_node(), Some(2), "循环回第一个");
        search.select_prev();
        assert_eq!(search.selected_node(), Some(9), "反向循环");
        let mut empty = GraphSearch::default();
        empty.select_next();
        empty.select_prev();
        assert_eq!(empty.selected_node(), None, "空匹配不 panic");
    }

    #[test]
    fn lod_hides_labels_and_thins_edges_at_low_zoom() {
        let min = GraphLod::for_zoom(MIN_ZOOM);
        assert!(!min.show_labels, "低缩放不画标签");
        assert!(!min.show_arrows, "低缩放不画箭头");
        assert!((min.edge_width - 0.5).abs() < f32::EPSILON, "低缩放画细边");
        let mid = GraphLod::for_zoom(0.5);
        assert!(!mid.show_labels, "0.5 < 标签阈值");
        assert!(mid.show_arrows, "0.5 ≥ 箭头阈值");
        assert!((mid.edge_width - 1.0).abs() < f32::EPSILON);
        let normal = GraphLod::for_zoom(1.0);
        assert!(normal.show_labels);
        assert!(normal.show_arrows);
        assert!((normal.edge_width - 1.0).abs() < f32::EPSILON);
        assert!(GraphLod::for_zoom(MAX_ZOOM).show_labels);
        // 阈值本身是闭区间。
        assert!(GraphLod::for_zoom(LABEL_ZOOM_THRESHOLD).show_labels);
        assert!(!GraphLod::for_zoom(LABEL_ZOOM_THRESHOLD - 0.01).show_labels);
        assert!(GraphLod::for_zoom(ARROW_ZOOM_THRESHOLD).show_arrows);
        assert!(!GraphLod::for_zoom(ARROW_ZOOM_THRESHOLD - 0.01).show_arrows);
    }

    #[test]
    fn arrow_head_geometry_is_deterministic() {
        // 水平边：尖端在目标点左侧 tip_offset 处，两翼对称。
        let head = arrow_head(point(px(0.0), px(0.0)), point(px(100.0), px(0.0)), 10.0)
            .expect("足够长的边必须画箭头");
        let [tip, left, right] = head;
        assert!((tip.x.as_f32() - 90.0).abs() < 0.01);
        assert!(tip.y.as_f32().abs() < 0.01);
        assert!((left.x.as_f32() - 83.656).abs() < 0.05, "{left:?}");
        assert!((left.y.as_f32() + 2.958).abs() < 0.05, "{left:?}");
        assert!((right.x.as_f32() - 83.656).abs() < 0.05, "{right:?}");
        assert!((right.y.as_f32() - 2.958).abs() < 0.05, "{right:?}");
        // 太短的边不画（避免箭头糊住节点）。
        assert!(arrow_head(point(px(0.0), px(0.0)), point(px(5.0), px(0.0)), 10.0).is_none());
        // 同输入两次结果一致。
        let again =
            arrow_head(point(px(0.0), px(0.0)), point(px(100.0), px(0.0)), 10.0).expect("确定性");
        assert_eq!(head, again);
    }

    #[test]
    fn center_on_puts_model_point_at_viewport_center() {
        let size = Size::new(px(800.0), px(600.0));
        let mut viewport = GraphViewport {
            zoom: 1.7,
            ..GraphViewport::default()
        };
        viewport.center_on(point(30.0, -20.0));
        let screen = viewport.screen_pos(point(30.0, -20.0), size);
        assert!((screen.x.as_f32() - 400.0).abs() < 0.01);
        assert!((screen.y.as_f32() - 300.0).abs() < 0.01);
        // 只改平移、不改缩放。
        assert!((viewport.zoom - 1.7).abs() < f32::EPSILON);
    }

    #[test]
    fn derive_applies_limits_after_filtering() {
        let notes = vec![
            note("n1", "a", "工作一", Some("dir")),
            note("n2", "b", "工作二", Some("dir")),
            note("n3", "c", "工作三", Some("dir")),
            note("n4", "d", "根笔记", None),
        ];
        let folders = vec![folder("dir", "工作", None)];
        let limits = GraphLimits {
            max_nodes: 2,
            max_edges: 10,
        };
        let model = GraphModel::from_rows_with(
            notes,
            folders,
            Vec::new(),
            limits,
            GraphScope::full(),
            GraphOptions {
                folder: Some("工作".to_owned()),
                ..GraphOptions::default()
            },
        );
        assert_eq!(model.node_count(), 2, "过滤后仍受节点上限约束");
        let truncation = model.truncation().expect("范围内截断");
        assert_eq!(truncation.total_nodes, 3, "total 是过滤后的范围内总数");
        assert_eq!(truncation.shown_nodes, 2);
        assert_eq!(model.hidden_nodes(), 1, "根笔记被过滤隐藏");
        let hint = hint_line(&model);
        assert!(hint.contains("整库 3 / 0"), "{hint}");
        assert!(hint.contains("已隐藏 1 个节点"), "{hint}");
    }

    #[test]
    fn node_offscreen_culls_only_far_away_nodes() {
        let size = Size::new(px(800.0), px(600.0));
        assert!(!node_offscreen(point(px(400.0), px(300.0)), size));
        assert!(
            !node_offscreen(point(px(-100.0), px(700.0)), size),
            "余量内仍画"
        );
        assert!(node_offscreen(point(px(-200.0), px(300.0)), size));
        assert!(node_offscreen(point(px(400.0), px(900.0)), size));
    }
}
