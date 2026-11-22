//! Wiki 关系图谱（ADR 0019 backlog `backlog-wiki-links-graph.md` 的 G0 + G1）。
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
//! 为什么先有 `from_rows`：G0 要"纯模型 + 纯单元测试"。SQL 只负责把行取出来
//! （[`GraphNoteRow`] / [`GraphLinkRow`]），`from_rows` 不依赖 SQLite，环、多边、
//! 自环、超大库截断这些边界可以在没有数据库时钉住；真实库路径由
//! `crates/app/tests/headless_graph.rs` 覆盖。
//!
//! ## 口径（G0 决策，测试钉住）
//!
//! - **自环排除**：`[[#某标题]]`（空 target 解析到自己）会解析成 `from == to`，
//!   它不成边、不进度数，只计入 [`GraphModel::self_links`]（与"未解析"同样只是统计）；
//! - **未解析不画边**：`to_note_id IS NULL` 只计入 [`GraphModel::unresolved_links`]；
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

use std::collections::{BTreeMap, HashMap};

use gpui::{point, px, Pixels, Point, Size};
use menghunote_storage::{Row, Value, Vault};

/// 模型坐标里的一个点（逻辑像素；只在内存里，不落库）。
pub type GraphPoint = Point<f32>;

// ---------------------------------------------------------------------------
// 常量（布局 / 视口）
// ---------------------------------------------------------------------------

/// 默认节点上限（超出只画前 N 个，并提示）。
pub const DEFAULT_MAX_NODES: usize = 800;
/// 默认边上限（超出只画前 N 条，并提示）。
pub const DEFAULT_MAX_EDGES: usize = 3000;
/// 环上相邻节点的弧长（模型单位）：节点越多环越大，直径随节点数线性增长。
const NODE_ARC_SPACING: f32 = 76.0;
/// 环的最小半径（模型单位）：节点很少时也不要挤在一点。
const MIN_RING_RADIUS: f32 = 96.0;
/// `zoom = 1.0` 时的节点半径（屏幕像素）。
pub const NODE_RADIUS: f32 = 9.0;
/// 缩放后节点半径的下限/上限（屏幕像素）：低缩放仍能看见一个点，高缩放也不盖满屏幕。
const NODE_RADIUS_RANGE: (f32, f32) = (3.0, 26.0);
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
}

/// 一条候选链接（`links` 的一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphLinkRow {
    /// 来源笔记 ID。
    pub from_note_id: String,
    /// 目标笔记 ID（`None` = 未解析）。
    pub to_note_id: Option<String>,
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

/// 节点/边上限的截断信息（`None` = 没截断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphTruncation {
    /// 整库节点总数（截断前）。
    pub total_nodes: usize,
    /// 整库去重边总数（截断前）。
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

/// 关系图谱的纯模型（节点/边/度数/布局）。
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
    /// 未解析链接条数（`to_note_id IS NULL`，只统计存活来源）。
    unresolved_links: usize,
    /// 自环链接条数（解析到自己；不成边、不进度数）。
    self_links: usize,
    /// 指向已删除/不存在笔记的链接条数。
    dangling_links: usize,
    /// 截断信息（`None` = 没截断）。
    truncation: Option<GraphTruncation>,
}

impl GraphModel {
    /// 从真实库构建（默认上限 800 / 3000）。
    ///
    /// 只用 `vault.query` 读现有表，**不新增 storage API**：图谱是派生视图。
    pub fn build(vault: &Vault) -> anyhow::Result<Self> {
        Self::build_with_limits(vault, GraphLimits::default())
    }

    /// 从真实库构建（显式上限；测试/将来 G2 过滤用）。
    pub fn build_with_limits(vault: &Vault, limits: GraphLimits) -> anyhow::Result<Self> {
        let notes = query_note_rows(vault)?;
        let links = query_link_rows(vault)?;
        Ok(Self::from_rows(notes, links, limits))
    }

    /// 纯构建：不碰数据库、不读时钟、不用随机数（单元测试的唯一入口）。
    ///
    /// 输入顺序**不影响**结果：内部先按 `(folder, path, title, note_id)` 排序，
    /// 再按排序后的下标建边/算布局。
    #[must_use]
    pub fn from_rows(
        notes: Vec<GraphNoteRow>,
        links: Vec<GraphLinkRow>,
        limits: GraphLimits,
    ) -> Self {
        let by_node: HashMap<&str, usize> = notes
            .iter()
            .enumerate()
            .map(|(index, note)| (note.node_id.as_str(), index))
            .collect();

        // 1) 结构信息：path / folder / depth（标题按父链拼接，缺失父节点当根）。
        let mut sorted: Vec<(String, String, usize, &GraphNoteRow)> =
            Vec::with_capacity(notes.len());
        for index in 0..notes.len() {
            let (path, folder, depth) = resolve_path(&notes, index, &by_node);
            let note = &notes[index];
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
        let mut unresolved_links = 0usize;
        let mut self_links = 0usize;
        let mut dangling_links = 0usize;
        for link in &links {
            // 来源必须是活节点：已删除笔记的链接整行跳过（不参与任何统计）。
            let Some(&from) = full_index.get(link.from_note_id.as_str()) else {
                continue;
            };
            match link.to_note_id.as_deref() {
                None => unresolved_links += 1,
                Some(to) => match full_index.get(to).copied() {
                    None => dangling_links += 1,
                    // 自环：解析到自己（`[[#标题]]` 的常态）。选"排除并统计"。
                    Some(to) if to == from => self_links += 1,
                    Some(to) => *pairs.entry((from, to)).or_insert(0) += 1,
                },
            }
        }
        let total_edges = pairs.len();

        // 3) 上限：节点取排序后的前 N 个（前缀），边取 `(from, to)` 最小的前 M 条。
        let shown_nodes = total_nodes.min(limits.max_nodes);
        let kept_pairs: Vec<((usize, usize), usize)> = pairs
            .into_iter()
            .filter(|((from, to), _)| *from < shown_nodes && *to < shown_nodes)
            .collect();
        let shown_edges = kept_pairs.len().min(limits.max_edges);
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
            unresolved_links,
            self_links,
            dangling_links,
            truncation,
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

    /// 空图（库里没有笔记，或全被上限截掉）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 未解析链接条数（不画边，只统计）。
    #[must_use]
    pub const fn unresolved_links(&self) -> usize {
        self.unresolved_links
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
/// 空图 / 正常 / 被上限截断三种形态都写清楚：**只提示，不假装全部画完了**。
#[must_use]
pub fn hint_line(model: &GraphModel) -> String {
    if model.is_empty() {
        return "图谱是空的：先写一篇笔记，再用 [[双链]] 连起来".to_owned();
    }
    let mut line = format!(
        "节点 {} · 边 {} · 未解析 {}",
        model.node_count(),
        model.edge_count(),
        model.unresolved_links()
    );
    if model.self_links() > 0 {
        line.push_str(&format!(" · 自环 {}（不画）", model.self_links()));
    }
    if model.dangling_links() > 0 {
        line.push_str(&format!(" · 悬空 {}（不画）", model.dangling_links()));
    }
    if let Some(truncation) = model.truncation() {
        line.push_str(&format!(
            " · 超过显示上限，只画前 {} 个节点 / {} 条边（整库 {} / {}）",
            truncation.shown_nodes,
            truncation.shown_edges,
            truncation.total_nodes,
            truncation.total_edges
        ));
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

    /// 拖拽平移（屏幕像素增量）。
    pub fn pan_by(&mut self, delta: Point<f32>) {
        self.pan = point(self.pan.x + delta.x, self.pan.y + delta.y);
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

// ---------------------------------------------------------------------------
// 内部：结构树 → path/folder/depth；确定性圆周布局
// ---------------------------------------------------------------------------

/// 沿父链拼 `path` / 顶层 `folder` / `depth`（父节点缺失或有环时停在当前层）。
fn resolve_path(
    notes: &[GraphNoteRow],
    index: usize,
    by_node: &HashMap<&str, usize>,
) -> (String, String, usize) {
    let mut titles = vec![notes[index].title.clone()];
    let mut current = notes[index].parent_id.as_deref();
    let mut guard = 0usize;
    while let Some(parent_id) = current {
        guard += 1;
        if guard > MAX_DEPTH_WALK || guard > notes.len() {
            break;
        }
        let Some(&parent) = by_node.get(parent_id) else {
            break;
        };
        titles.push(notes[parent].title.clone());
        current = notes[parent].parent_id.as_deref();
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
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(GraphNoteRow {
            node_id: row_text(row, 0, "tree.node_id")?,
            note_id: row_text(row, 1, "tree.note_id")?,
            title: row_text(row, 2, "tree.title")?,
            parent_id: row_optional_text(row, 3, "tree.parent_id")?,
        });
    }
    Ok(out)
}

fn query_link_rows(vault: &Vault) -> anyhow::Result<Vec<GraphLinkRow>> {
    let rows = vault.query(
        "SELECT from_note_id, to_note_id FROM links WHERE kind = 'wiki'",
        &[],
    )?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(GraphLinkRow {
            from_note_id: row_text(row, 0, "links.from_note_id")?,
            to_note_id: row_optional_text(row, 1, "links.to_note_id")?,
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
        }
    }

    fn link(from: &str, to: Option<&str>) -> GraphLinkRow {
        GraphLinkRow {
            from_note_id: from.to_owned(),
            to_note_id: to.map(str::to_owned),
        }
    }

    fn build(notes: Vec<GraphNoteRow>, links: Vec<GraphLinkRow>) -> GraphModel {
        GraphModel::from_rows(notes, links, GraphLimits::unlimited())
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
    }
}
