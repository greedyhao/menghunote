//! Mermaid 预览的后台渲染、光栅化与 LRU 缓存（ADR 0019 M2）。
//!
//! ## 边界
//!
//! - **不解析 Mermaid**：源码由 editor 的 [`menghunote_editor::MarkdownMermaid::content`]
//!   给出，本模块只调用 M1 的 [`crate::mermaid::render_svg_for_preview`]（纯 Rust、
//!   无 Node/WebView/远程渲染），再交给 GPUI 的 [`gpui::SvgRenderer`]（`resvg`/`usvg`）
//!   光栅化成 [`gpui::RenderImage`]；
//! - **主线程不渲染、不光栅化**：全部工作在**一个**后台线程（串行，结构性并发 1）；
//!   主线程只登记请求、读状态；
//! - **失败/超限不 panic、不改正文**：只把状态降级为 [`MermaidPreviewState::Failed`]，
//!   编辑器显示「Mermaid 渲染失败」占位并保留源码回退；
//! - **缓存**：key = 源码 hash + `MERMAN_VERSION` + 主题 id + 渲染宽度桶；LRU 有界
//!   （[`MERMAID_CACHE_BUDGET_BYTES`] + 最大条目数）。同一 key 只渲染一次。
//!
//! ## 为什么用 GPUI 的 `SvgRenderer` 而不是新加光栅化依赖
//!
//! `gpui-pre 0.3.12` 已经通过 `gpui::SvgRenderer` 暴露了 `resvg`/`usvg` 光栅化
//! （与 `gpui-kit` 同一份依赖图、同一套版本），所以**不新增任何依赖**。Mermaid 默认
//! 把标签放在 `<foreignObject>` HTML 里，`usvg` 会忽略它；M1 因此另提供
//! `render_svg_for_preview`（merman 的 `ResvgSafe` 管线，把标签投影成 SVG `<text>`），
//! 预览路径用那个 SVG 光栅化。
//!
//! ## 线程模型（与图片解码同构）
//!
//! ```text
//! 主线程（prepaint）: request(key, source) → 只登记 inflight，立即返回 Loading
//! 后台 worker:        render_svg_for_preview → SvgRenderer::render_single_frame → 缓存 → completed += 1
//! 主线程（pump）:     take_completed() > 0 → 重绘；wait_idle() 是测试用收敛点
//! ```
//!
//! `Mutex` 只保护小表，不跨渲染/光栅化（锁内不做重活）。
//!
//! ## 资源上限
//!
//! | 边界 | 上限 | 行为 |
//! | --- | --- | --- |
//! | 源码 | M1 的 256 KiB | 超限 → `Failed(Render(...))` |
//! | SVG | M1 的 8 MiB | 超限 → `Failed(Render(...))` |
//! | 光栅边长 | 4096 device px | 等比缩小到预算内 |
//! | 光栅总像素 | 8 MP（约 32 MiB BGRA） | 等比缩小；仍超（异常 SVG）→ `TooLarge` |
//! | 已解码缓存 | 64 MiB / 最多 64 项 | 按 LRU 淘汰 |
//!
//! 光栅化按 GPUI 的平滑策略渲染到 **2×** 逻辑尺寸（`SvgRenderer` 的
//! `SMOOTH_SVG_SCALE_FACTOR`），因此高 DPI 下文字/线条仍清晰；逻辑尺寸记为
//! `自然尺寸 × scale`，绘制层据此按内容宽度等比缩放。

use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gpui::{AssetSource, RenderImage, SvgRenderer};

use crate::mermaid::{render_svg_for_preview, MermaidError};
use crate::theme::ThemeId;

/// M1 固定使用的 merman 版本；进缓存 key，升级渲染器时旧条目自然失效。
///
/// 必须与 `crates/app/Cargo.toml` 的 `merman = "=0.10.0"` 一致（升级时两边一起改）。
pub const MERMAN_VERSION: &str = "0.10.0";

/// 已光栅化图片的缓存预算（BGRA 字节）。
pub const MERMAID_CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// 光栅化单边上限（device px）；超过时等比缩小。
pub const MAX_RASTER_EDGE: u32 = 4096;

/// 光栅化总像素上限（约 32 MiB BGRA）；超过时等比缩小。
pub const MAX_RASTER_PIXELS: u64 = 8 * 1024 * 1024;

/// 渲染宽度桶的最小值（逻辑像素）。
const MIN_RENDER_WIDTH: u32 = 128;
/// 渲染宽度桶的最大值（逻辑像素）。
const MAX_RENDER_WIDTH: u32 = 2048;
/// 宽度桶粒度：窗口拖动时避免每一帧都换 key 重新渲染。
const WIDTH_BUCKET: u32 = 64;
/// 缓存最大条目数（含失败终态；失败是零字节，但也要防病态笔记刷爆表）。
const MAX_CACHE_ENTRIES: usize = 64;
/// GPUI `SvgRenderer::render_single_frame` 内部固定按 2× 渲染。
const GPUI_SVG_SCALE: f32 = 2.0;

/// 渲染服务最多排队的请求数（超过时请求方看到 `Closed`；正常笔记远小于它）。
const JOB_QUEUE: usize = 256;

/// Mermaid 预览缓存 key。
///
/// 包含：源码 hash + 源码字节长度 + merman 版本 + 主题 + 渲染宽度桶。宽度桶把
/// “内容宽度每帧微调”折叠到 64px 粒度，避免拖动窗口时反复渲染。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MermaidCacheKey {
    source_hash: u64,
    source_len: usize,
    merman_version: &'static str,
    theme: &'static str,
    width: u32,
}

impl MermaidCacheKey {
    /// 由 Mermaid 源码（**content**，不含围栏行）与可用内容宽度构造 key。
    ///
    /// 主题取**全局当前主题 id**（ADR 0021 P2）：切主题后 key 自然变化，旧缓存不再
    /// 命中并重新渲染一次。SVG 内容本身的暗色配色属于 M3，本切片只保证缓存失效。
    #[must_use]
    pub fn new(source: &str, content_width: f32) -> Self {
        Self::new_for_theme(source, content_width, crate::theme::theme_id())
    }

    /// 显式指定主题 id 构造 key（测试/后台渲染用；不读全局状态）。
    #[must_use]
    pub fn new_for_theme(source: &str, content_width: f32, theme: crate::theme::ThemeId) -> Self {
        Self {
            source_hash: fnv1a64(source.as_bytes()),
            source_len: source.len(),
            merman_version: MERMAN_VERSION,
            theme: theme.as_str(),
            width: width_bucket(content_width),
        }
    }

    /// 源码 hash（FNV-1a 64；仅用于缓存身份，不作为安全哈希）。
    #[must_use]
    pub const fn source_hash(self) -> u64 {
        self.source_hash
    }

    /// 源码字节长度（与 hash 一起进一步降低碰撞概率）。
    #[must_use]
    pub const fn source_len(self) -> usize {
        self.source_len
    }

    /// 渲染宽度桶（逻辑像素，64 的倍数）。
    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    /// merman 版本（缓存失效用）。
    #[must_use]
    pub const fn merman_version(self) -> &'static str {
        self.merman_version
    }

    /// 主题 id（缓存失效用）。
    #[must_use]
    pub const fn theme(self) -> &'static str {
        self.theme
    }

    /// 解析后的主题 id（worker 按它选择 SVG 主题；只认识内置浅/深色）。
    #[must_use]
    pub fn theme_id(self) -> ThemeId {
        ThemeId::parse(self.theme).unwrap_or(ThemeId::LIGHT)
    }
}

/// 把内容宽度折叠到 [`MIN_RENDER_WIDTH`, `MAX_RENDER_WIDTH`] 之间的 64px 桶。
#[must_use]
pub fn width_bucket(content_width: f32) -> u32 {
    if !content_width.is_finite() || content_width <= 0.0 {
        return MIN_RENDER_WIDTH;
    }
    let rounded = (content_width.ceil() as u32).saturating_add(WIDTH_BUCKET - 1) / WIDTH_BUCKET
        * WIDTH_BUCKET;
    rounded.clamp(MIN_RENDER_WIDTH, MAX_RENDER_WIDTH)
}

/// 已光栅化的 Mermaid 图（BGRA，首帧）。
#[derive(Debug, Clone)]
pub struct MermaidImage {
    /// GPUI 可绘制的图片（`Arc` 便于跨帧/跨视图共享）。
    pub image: Arc<RenderImage>,
    /// 逻辑宽度（用于布局/等比，不是像素宽）。
    pub width: f32,
    /// 逻辑高度。
    pub height: f32,
    /// 光栅化占用字节（`w * h * 4`，LRU 记账用）。
    pub byte_len: usize,
}

/// 渲染失败的分类（全部可降级为「Mermaid 渲染失败」占位，不写正文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MermaidPreviewError {
    /// M1 渲染失败（空/非图/语法/超限/panic/非 SVG）。文案只用于诊断展示。
    Render(String),
    /// GPUI/usvg 解析/光栅化失败。
    Raster(String),
    /// 光栅化结果超过像素预算（异常 SVG；正常图会被等比缩小）。
    TooLarge {
        /// 像素宽。
        width: u32,
        /// 像素高。
        height: u32,
    },
    /// 后台线程已关闭（视图关闭中）。
    Closed,
}

impl std::fmt::Display for MermaidPreviewError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Render(reason) => write!(formatter, "Mermaid 渲染失败：{reason}"),
            Self::Raster(reason) => write!(formatter, "Mermaid 光栅化失败：{reason}"),
            Self::TooLarge { width, height } => {
                write!(formatter, "Mermaid 光栅化 {width}×{height} 超过像素预算")
            }
            Self::Closed => formatter.write_str("Mermaid 渲染线程已关闭"),
        }
    }
}

impl std::error::Error for MermaidPreviewError {}

/// Mermaid 预览的当前状态（app 绘制占位/图片用；克隆只增加一个 `Arc`）。
#[derive(Debug, Clone)]
pub enum MermaidPreviewState {
    /// **等待光标移出 block**（或内容为空）：不提交渲染请求。
    ///
    /// 这是 view 层的派生状态，`MermaidService` 自己不会返回它；它只用于绘制
    /// 「等待光标移出后渲染…」占位，避免输入 ` ```mermaid ` 时立刻把半成品渲染成
    /// 失败。缓存/在途请求不受影响。
    Waiting,
    /// 正在后台渲染/光栅化（或尚未请求）。
    Loading,
    /// 已光栅化。
    Ready(Arc<MermaidImage>),
    /// 失败（渲染、光栅化或超限）；调用方显示占位并保留源码回退。
    Failed(MermaidPreviewError),
}

/// 一个已渲染图片的缓存项。
struct CacheEntry {
    image: Arc<MermaidImage>,
    /// LRU 序号（越大越新）。
    tick: u64,
}

impl CacheEntry {
    fn byte_len(&self) -> usize {
        self.image.byte_len
    }
}

/// 一个 key 的终态（失败也是终态：确定性渲染器对同一输入不会变好，避免每帧重试）。
enum CacheSlot {
    Ready(CacheEntry),
    Failed(MermaidPreviewError),
}

/// 缓存 + inflight + 完成计数（单把锁，锁内不做渲染）。
#[derive(Default)]
struct Inner {
    cache: HashMap<MermaidCacheKey, CacheSlot>,
    inflight: HashSet<MermaidCacheKey>,
    bytes: usize,
    tick: u64,
    completed: usize,
    requests: usize,
    renders: usize,
}

impl Inner {
    fn lock(inner: &Mutex<Self>) -> MutexGuard<'_, Self> {
        // 锁中毒也不能 panic：其它线程 panic 不该把预览整个带崩。
        inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 写入一个终态；`Ready` 超预算时按 LRU 淘汰（**总是保留刚写入的那一项**）。
    fn insert(&mut self, key: MermaidCacheKey, slot: CacheSlot, budget: usize) {
        self.cache.remove(&key);
        let ready_bytes = match &slot {
            CacheSlot::Ready(entry) => Some(entry.byte_len()),
            CacheSlot::Failed(_) => None,
        };
        if let Some(byte_len) = ready_bytes {
            self.bytes = self.bytes.saturating_add(byte_len);
        }
        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        let slot = match slot {
            CacheSlot::Ready(entry) => CacheSlot::Ready(CacheEntry { tick, ..entry }),
            other => other,
        };
        self.cache.insert(key, slot);
        self.evict(budget, key);
    }

    /// 淘汰：先按字节预算淘汰 Ready，再按条目数淘汰任意终态（都从最旧开始）。
    fn evict(&mut self, budget: usize, keep: MermaidCacheKey) {
        while self.bytes > budget {
            let Some(oldest) = self
                .cache
                .iter()
                .filter(|(key, slot)| **key != keep && matches!(slot, CacheSlot::Ready(_)))
                .min_by_key(|(_, slot)| match slot {
                    CacheSlot::Ready(entry) => entry.tick,
                    CacheSlot::Failed(_) => u64::MAX,
                })
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(CacheSlot::Ready(entry)) = self.cache.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(entry.byte_len());
            }
        }
        while self.cache.len() > MAX_CACHE_ENTRIES {
            let Some(oldest) = self
                .cache
                .iter()
                .filter(|(key, _)| **key != keep)
                .min_by_key(|(_, slot)| match slot {
                    CacheSlot::Ready(entry) => entry.tick,
                    CacheSlot::Failed(_) => 0,
                })
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(CacheSlot::Ready(entry)) = self.cache.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(entry.byte_len());
            }
        }
    }

    fn state(&self, key: MermaidCacheKey) -> MermaidPreviewState {
        match self.cache.get(&key) {
            Some(CacheSlot::Ready(entry)) => MermaidPreviewState::Ready(Arc::clone(&entry.image)),
            Some(CacheSlot::Failed(error)) => MermaidPreviewState::Failed(error.clone()),
            None if self.inflight.contains(&key) => MermaidPreviewState::Loading,
            // 对 Mermaid 来说“未请求”也应显示占位（对象已经存在，渲染是异步后续）。
            None => MermaidPreviewState::Loading,
        }
    }
}

enum Job {
    Render {
        key: MermaidCacheKey,
        source: String,
        /// 由 `key.theme_id()` 解析：worker 不读全局主题，避免 request 与渲染线程不一致。
        theme: ThemeId,
    },
    Shutdown,
}

/// 渲染函数（生产 = M1 的 [`render_svg_for_preview`]；测试可注入 fake）。
///
/// 第二个参数是主题：后台 worker 只按任务携带的主题渲染，不依赖主线程的全局状态。
type RenderFn = dyn Fn(&str, ThemeId) -> Result<String, MermaidError> + Send + Sync + 'static;

/// Mermaid 预览服务：一个后台渲染线程 + LRU 缓存（`Clone` 共享同一 worker）。
#[derive(Clone)]
pub struct MermaidService {
    inner: Arc<ServiceInner>,
}

struct ServiceInner {
    tx: SyncSender<Job>,
    handle: Mutex<Option<JoinHandle<()>>>,
    shared: Arc<Mutex<Inner>>,
    wake: Arc<Condvar>,
    budget: usize,
}

impl std::fmt::Debug for MermaidService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MermaidService")
            .field("budget", &self.inner.budget)
            .finish()
    }
}

impl MermaidService {
    /// 用 app 的资产源创建服务（启动一个后台渲染线程）。
    ///
    /// 资产源只用于 `SvgRenderer` 解析 SVG 里引用的字体（`gpui-kit` 的 bundled fonts）；
    /// 不读笔记/不访问网络。
    #[must_use]
    pub fn new(asset_source: Arc<dyn AssetSource>) -> Self {
        Self::with_budget(asset_source, MERMAID_CACHE_BUDGET_BYTES)
    }

    /// 指定缓存预算（测试用：极小预算验证 LRU 淘汰）。
    #[must_use]
    pub fn with_budget(asset_source: Arc<dyn AssetSource>, budget: usize) -> Self {
        Self::build(asset_source, Arc::new(render_svg_for_preview), budget)
    }

    /// 不依赖 app 资产源的实例（直接构造视图的测试用；字体用 GPUI 的默认解析）。
    #[must_use]
    pub fn headless() -> Self {
        let asset_source: Arc<dyn AssetSource> = Arc::new(());
        Self::new(asset_source)
    }

    fn build(asset_source: Arc<dyn AssetSource>, render: Arc<RenderFn>, budget: usize) -> Self {
        let (tx, rx) = sync_channel::<Job>(JOB_QUEUE);
        let shared = Arc::new(Mutex::new(Inner::default()));
        let wake = Arc::new(Condvar::new());
        let worker_shared = Arc::clone(&shared);
        let worker_wake = Arc::clone(&wake);
        let handle = std::thread::Builder::new()
            .name("menghunote-mermaid".to_owned())
            .spawn(move || {
                worker_loop(rx, asset_source, render, worker_shared, worker_wake, budget);
            })
            .ok();
        Self {
            inner: Arc::new(ServiceInner {
                tx,
                handle: Mutex::new(handle),
                shared,
                wake,
                budget,
            }),
        }
    }

    /// 请求渲染一个 key（幂等：已缓存 / 在途都不会重复提交）。
    ///
    /// `source` 是 fenced 围栏之间的 Mermaid 源码（不含围栏行）。
    pub fn request(&self, key: MermaidCacheKey, source: &str) {
        {
            let mut inner = Inner::lock(&self.inner.shared);
            if inner.cache.contains_key(&key) || inner.inflight.contains(&key) {
                return;
            }
            inner.inflight.insert(key);
            inner.requests = inner.requests.saturating_add(1);
        }
        self.submit(key, source);
    }

    /// 强制重新渲染一个 key（工具条「重新渲染」）：丢弃成功/失败终态后重新入队。
    ///
    /// 同一 key 已在途时返回 `false`（不重复排队；等它完成即可）。返回是否真的入队。
    pub fn retry(&self, key: MermaidCacheKey, source: &str) -> bool {
        {
            let mut inner = Inner::lock(&self.inner.shared);
            if inner.inflight.contains(&key) {
                return false;
            }
            // `remove` 无论命中 Ready 还是 Failed 都会把终态摘掉；只有 Ready 需要减字节。
            if let Some(CacheSlot::Ready(entry)) = inner.cache.remove(&key) {
                inner.bytes = inner.bytes.saturating_sub(entry.byte_len());
            }
            inner.inflight.insert(key);
            inner.requests = inner.requests.saturating_add(1);
        }
        self.submit(key, source);
        true
    }

    /// 提交一个已经登记过 inflight 的渲染任务。
    fn submit(&self, key: MermaidCacheKey, source: &str) {
        let job = Job::Render {
            key,
            source: source.to_owned(),
            theme: key.theme_id(),
        };
        if self.inner.tx.send(job).is_err() {
            let mut inner = Inner::lock(&self.inner.shared);
            inner.inflight.remove(&key);
            inner.insert(
                key,
                CacheSlot::Failed(MermaidPreviewError::Closed),
                self.inner.budget,
            );
            inner.completed = inner.completed.saturating_add(1);
            self.inner.wake.notify_all();
        }
    }

    /// 当前状态（`Loading` 也包含“尚未请求”）。
    #[must_use]
    pub fn state(&self, key: MermaidCacheKey) -> MermaidPreviewState {
        Inner::lock(&self.inner.shared).state(key)
    }

    /// 取走“自上次调用以来完成的渲染数”（`pump` 用它触发一次重绘）。
    pub fn take_completed(&self) -> usize {
        std::mem::take(&mut Inner::lock(&self.inner.shared).completed)
    }

    /// 实际提交给 worker 的渲染请求数（诊断/测试：验证等待/缓存命中不重复请求）。
    #[must_use]
    pub fn request_count(&self) -> usize {
        Inner::lock(&self.inner.shared).requests
    }

    /// 实际执行过的渲染次数（诊断/测试：验证缓存命中不重复渲染）。
    #[must_use]
    pub fn render_count(&self) -> usize {
        Inner::lock(&self.inner.shared).renders
    }

    /// 当前缓存条目数（测试/诊断）。
    #[must_use]
    pub fn cache_entries(&self) -> usize {
        Inner::lock(&self.inner.shared).cache.len()
    }

    /// 当前缓存字节数（测试/诊断）。
    #[must_use]
    pub fn cache_bytes(&self) -> usize {
        Inner::lock(&self.inner.shared).bytes
    }

    /// 等待所有在途渲染结束（测试用收敛点；有界，**不 sleep 轮询**）。
    ///
    /// 生产代码不需要它：完成由 `take_completed()` + pump 重绘驱动。
    #[must_use]
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut inner = Inner::lock(&self.inner.shared);
        while !inner.inflight.is_empty() {
            let Some(deadline) = deadline else {
                return false;
            };
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, wait) = self
                .inner
                .wake
                .wait_timeout(inner, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner = guard;
            if wait.timed_out() && !inner.inflight.is_empty() {
                return false;
            }
        }
        true
    }
}

impl Drop for ServiceInner {
    fn drop(&mut self) {
        let _ = self.tx.send(Job::Shutdown);
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

fn worker_loop(
    rx: Receiver<Job>,
    asset_source: Arc<dyn AssetSource>,
    render: Arc<RenderFn>,
    shared: Arc<Mutex<Inner>>,
    wake: Arc<Condvar>,
    budget: usize,
) {
    // SvgRenderer 不是 Send：它只在 worker 线程内构造/使用，不跨线程移动。
    let renderer = SvgRenderer::new(asset_source);
    while let Ok(job) = rx.recv() {
        let Job::Render { key, source, theme } = job else {
            break;
        };
        let slot = render_slot(&renderer, render.as_ref(), key, theme, &source);
        let mut inner = Inner::lock(&shared);
        inner.inflight.remove(&key);
        inner.renders = inner.renders.saturating_add(1);
        inner.insert(key, slot, budget);
        inner.completed = inner.completed.saturating_add(1);
        drop(inner);
        wake.notify_all();
    }
}

/// 一次完整的“源码 → SVG → 位图”渲染；任何失败都变成终态，不 panic。
fn render_slot(
    renderer: &SvgRenderer,
    render: &RenderFn,
    key: MermaidCacheKey,
    theme: ThemeId,
    source: &str,
) -> CacheSlot {
    let svg = match std::panic::catch_unwind(AssertUnwindSafe(|| render(source, theme))) {
        Ok(Ok(svg)) => svg,
        Ok(Err(error)) => return CacheSlot::Failed(MermaidPreviewError::Render(error.to_string())),
        Err(payload) => {
            return CacheSlot::Failed(MermaidPreviewError::Render(format!(
                "渲染器 panic（已捕获）：{}",
                panic_message(payload.as_ref())
            )));
        }
    };
    match std::panic::catch_unwind(AssertUnwindSafe(|| {
        rasterize_svg(renderer, &svg, key.width())
    })) {
        Ok(Ok(image)) => CacheSlot::Ready(CacheEntry {
            image: Arc::new(image),
            tick: 0,
        }),
        Ok(Err(error)) => CacheSlot::Failed(error),
        Err(payload) => CacheSlot::Failed(MermaidPreviewError::Raster(format!(
            "光栅化 panic（已捕获）：{}",
            panic_message(payload.as_ref())
        ))),
    }
}

/// 把一个 ResvgSafe SVG 光栅化成 GPUI 图片。
///
/// 自然尺寸来自 SVG 根元素的 `viewBox`（Merman 输出恒有）；按目标宽度桶与光栅预算
/// 等比缩小，再由 `SvgRenderer` 以 2× 平滑渲染。返回值里的宽高是**逻辑尺寸**
/// （自然尺寸 × scale），绘制层用它做布局。
fn rasterize_svg(
    renderer: &SvgRenderer,
    svg: &str,
    max_width: u32,
) -> Result<MermaidImage, MermaidPreviewError> {
    let (natural_width, natural_height) = svg_intrinsic_size(svg)
        .ok_or_else(|| MermaidPreviewError::Raster("SVG 缺少可用的 viewBox/宽高".to_owned()))?;
    if !(natural_width.is_finite()
        && natural_height.is_finite()
        && natural_width > 0.0
        && natural_height > 0.0)
    {
        return Err(MermaidPreviewError::Raster(format!(
            "SVG 自然尺寸非法：{natural_width}×{natural_height}"
        )));
    }

    // 1) 按内容宽度等比（小图不放大）。
    let target_width = natural_width.min(max_width as f32);
    let mut scale = target_width / natural_width;
    // 2) 单边上限（GPUI 内部还按 2× 渲染，所以 device px 再乘 2）。
    let max_edge_scale =
        MAX_RASTER_EDGE as f32 / (GPUI_SVG_SCALE * natural_width.max(natural_height));
    scale = scale.min(max_edge_scale);
    // 3) 总像素预算：device px = natural × scale × 2。
    let max_pixels_scale = (MAX_RASTER_PIXELS as f32
        / (GPUI_SVG_SCALE * GPUI_SVG_SCALE * natural_width * natural_height))
        .sqrt();
    scale = scale.min(max_pixels_scale);
    if !scale.is_finite() || scale <= 0.0 {
        return Err(MermaidPreviewError::Raster(
            "无法为 SVG 计算有效缩放".to_owned(),
        ));
    }

    let image = renderer
        .render_single_frame(svg.as_bytes(), scale)
        .map_err(|error| MermaidPreviewError::Raster(error.to_string()))?;
    let pixel_size = image.size(0);
    let pixel_width = u32::try_from(i32::from(pixel_size.width)).unwrap_or(0);
    let pixel_height = u32::try_from(i32::from(pixel_size.height)).unwrap_or(0);
    if pixel_width == 0 || pixel_height == 0 {
        return Err(MermaidPreviewError::Raster("光栅化结果是空图".to_owned()));
    }
    let pixels = u64::from(pixel_width) * u64::from(pixel_height);
    if pixels > MAX_RASTER_PIXELS {
        return Err(MermaidPreviewError::TooLarge {
            width: pixel_width,
            height: pixel_height,
        });
    }
    let byte_len = usize::try_from(pixels)
        .ok()
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(MermaidPreviewError::TooLarge {
            width: pixel_width,
            height: pixel_height,
        })?;
    let width = natural_width * scale;
    let height = natural_height * scale;
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(MermaidPreviewError::Raster("逻辑尺寸非法".to_owned()));
    }
    Ok(MermaidImage {
        image,
        width,
        height,
        byte_len,
    })
}

/// 从独立 SVG 文档的根元素取自然尺寸（逻辑单位）。
///
/// 优先 `viewBox="minX minY width height"`（Merman 的输出恒有），否则退回数值型
/// `width`/`height` 属性。百分比宽高（`width="100%"`）不算自然尺寸：没有 viewBox 的
/// 百分比 SVG 无法确定宽高，按 `None` 处理（预览显示失败占位，而不是猜一个尺寸）。
fn svg_intrinsic_size(svg: &str) -> Option<(f32, f32)> {
    let tag = svg_root_tag(svg)?;
    if let Some(view_box) = attribute(tag, "viewBox") {
        let mut values = view_box
            .split(|character: char| character == ',' || character.is_whitespace())
            .filter(|value| !value.is_empty());
        let _min_x = values.next()?.parse::<f32>().ok()?;
        let _min_y = values.next()?.parse::<f32>().ok()?;
        let width = values.next()?.parse::<f32>().ok()?;
        let height = values.next()?.parse::<f32>().ok()?;
        if width > 0.0 && height > 0.0 {
            return Some((width, height));
        }
    }
    let width = parse_length(attribute(tag, "width")?)?;
    let height = parse_length(attribute(tag, "height")?)?;
    Some((width, height))
}

/// 取 SVG 根元素的**开口标签**（`<svg …>`），尊重引号里的 `>`。
fn svg_root_tag(svg: &str) -> Option<&str> {
    let start = svg.find("<svg")?;
    let bytes = svg.as_bytes();
    let mut quote = None;
    let mut index = start + 4;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(active) => {
                if byte == active {
                    quote = None;
                }
            }
            None => match byte {
                b'"' | b'\'' => quote = Some(byte),
                b'>' => return svg.get(start..=index),
                _ => {}
            },
        }
        index += 1;
    }
    None
}

/// 取开口标签里的属性值（只支持带引号/不带引号的基本写法；找不到返回 `None`）。
fn attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut index = 4usize; // 跳过 "<svg"
    while index < bytes.len() {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] == b'>' || bytes[index] == b'/' {
            break;
        }
        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && !matches!(bytes[index], b'=' | b'>' | b'/')
        {
            index += 1;
        }
        let attribute_name = tag.get(name_start..index)?;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() || bytes[index] != b'=' {
            continue;
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            return None;
        }
        let (value_start, value_end) = match bytes[index] {
            quote @ (b'"' | b'\'') => {
                index += 1;
                let start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                (start, index)
            }
            _ => {
                let start = index;
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && bytes[index] != b'>'
                {
                    index += 1;
                }
                (start, index)
            }
        };
        if attribute_name == name {
            return tag.get(value_start..value_end);
        }
    }
    None
}

/// 解析不带单位的长度（`168` / `168px`）；百分比/其它单位返回 `None`。
fn parse_length(value: &str) -> Option<f32> {
    let value = value.trim();
    let numeric = value.strip_suffix("px").unwrap_or(value);
    if numeric.ends_with('%') {
        return None;
    }
    let parsed = numeric.parse::<f32>().ok()?;
    (parsed.is_finite() && parsed > 0.0).then_some(parsed)
}

/// FNV-1a 64：仅用于缓存身份（不是安全哈希）。
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 提取 panic 载荷的人类可读文案（与 `crate::mermaid` 的同名私有函数同语义）。
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "非字符串 panic 载荷".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 一个最小的合法 SVG（无文字，避免测试依赖系统字体）。
    const TINY_SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"50\" viewBox=\"0 0 100 50\"><rect x=\"0\" y=\"0\" width=\"100\" height=\"50\" fill=\"#abc\"/></svg>";

    /// 深色版最小 SVG：只为区分“worker 用了哪个主题”。
    const DARK_TINY_SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"50\" viewBox=\"0 0 100 50\"><rect x=\"0\" y=\"0\" width=\"100\" height=\"50\" fill=\"#102030\"/></svg>";

    fn blocked_renderer(
        gate: Arc<(Mutex<bool>, Condvar)>,
        calls: Arc<AtomicUsize>,
    ) -> Arc<RenderFn> {
        Arc::new(move |_source: &str, _theme: ThemeId| {
            calls.fetch_add(1, Ordering::SeqCst);
            let (lock, wake) = &*gate;
            let mut released = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while !*released {
                released = wake
                    .wait(released)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            Ok(TINY_SVG.to_owned())
        })
    }

    fn release(gate: &Arc<(Mutex<bool>, Condvar)>) {
        let (lock, wake) = &**gate;
        *lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        wake.notify_all();
    }

    #[test]
    fn width_buckets_are_clamped_and_rounded() {
        assert_eq!(width_bucket(0.0), MIN_RENDER_WIDTH);
        assert_eq!(width_bucket(f32::NAN), MIN_RENDER_WIDTH);
        assert_eq!(width_bucket(1.0), MIN_RENDER_WIDTH);
        assert_eq!(width_bucket(129.0), 192);
        assert_eq!(width_bucket(700.0), 704);
        assert_eq!(width_bucket(100_000.0), MAX_RENDER_WIDTH);
    }

    #[test]
    fn cache_key_includes_source_version_theme_and_width() {
        use crate::theme::ThemeId;

        let source = "flowchart TD\nA-->B\n";
        let base = MermaidCacheKey::new_for_theme(source, 700.0, ThemeId::LIGHT);
        assert_eq!(base.merman_version(), MERMAN_VERSION);
        assert_eq!(base.theme(), "light");
        assert_eq!(base.source_len(), source.len());
        assert_ne!(
            base,
            MermaidCacheKey::new_for_theme("flowchart TD\nA-->C\n", 700.0, ThemeId::LIGHT)
        );
        assert_ne!(
            base,
            MermaidCacheKey::new_for_theme(source, 900.0, ThemeId::LIGHT)
        );
        assert_eq!(
            base,
            MermaidCacheKey::new_for_theme(source, 701.0, ThemeId::LIGHT),
            "同桶宽度必须命中"
        );
        // ADR 0021 P2：浅色/深色必须是**不同** key（切主题后旧缓存失效）。
        assert_ne!(
            base,
            MermaidCacheKey::new_for_theme(source, 700.0, ThemeId::DARK)
        );
        assert_eq!(
            MermaidCacheKey::new_for_theme(source, 700.0, ThemeId::DARK).theme(),
            "dark"
        );
        // worker 直接按 key 的主题渲染：不需要读全局状态。
        assert_eq!(base.theme_id(), ThemeId::LIGHT);
        assert_eq!(
            MermaidCacheKey::new_for_theme(source, 700.0, ThemeId::DARK).theme_id(),
            ThemeId::DARK
        );
    }

    #[test]
    fn svg_intrinsic_size_reads_view_box_then_numeric_attributes() {
        assert_eq!(
            svg_intrinsic_size(
                "<svg width=\"100%\" viewBox=\"0 0 168 156\" xmlns=\"http://www.w3.org/2000/svg\"><g/></svg>"
            ),
            Some((168.0, 156.0))
        );
        assert_eq!(
            svg_intrinsic_size("<svg width=\"24px\" height=\"12\" xmlns=\"x\"></svg>"),
            Some((24.0, 12.0))
        );
        assert_eq!(
            svg_intrinsic_size("<svg width=\"100%\" height=\"100%\"></svg>"),
            None
        );
        assert_eq!(svg_intrinsic_size("<svg viewBox=\"0 0 0 10\"></svg>"), None);
        assert_eq!(svg_intrinsic_size("not an svg"), None);
    }

    #[test]
    fn attribute_parser_does_not_confuse_similar_names() {
        let tag = "<svg stroke-width=\"4\" width=\"12\" height='7' viewBox=\"0 0 12 7\">";
        assert_eq!(attribute(tag, "width"), Some("12"));
        assert_eq!(attribute(tag, "height"), Some("7"));
        assert_eq!(attribute(tag, "viewBox"), Some("0 0 12 7"));
        assert_eq!(attribute(tag, "stroke-width"), Some("4"));
    }

    #[test]
    fn rasterize_produces_a_gpui_image_at_two_x_inside_budget() {
        let renderer = SvgRenderer::new(Arc::new(()));
        let image = rasterize_svg(&renderer, TINY_SVG, 800).expect("最小 SVG 必须光栅化");
        assert_eq!((image.width, image.height), (100.0, 50.0), "小图不放大");
        let size = image.image.size(0);
        // GPUI 的平滑策略固定 2×；逻辑尺寸 × 2 = device px。
        assert_eq!((i32::from(size.width), i32::from(size.height)), (200, 100));
        assert_eq!(image.byte_len, 200 * 100 * 4);
    }

    /// M3a：真实 GPUI `SvgRenderer`（resvg/usvg）把深色 CJK 标签光栅化成字形墨迹。
    ///
    /// 用**同一图的空标签**作对照：没有文字时该区域不应出现文字色像素；中文标签必须
    /// 出现足量 `#e5e7eb` 文字色像素（说明显示层确实选中了可覆盖 CJK 的系统字体，
    /// 而不是空白/整块丢失）。是否“无可读方框”仍要 Windows 真机看观感。
    #[test]
    fn dark_cjk_labels_rasterize_to_visible_glyph_ink() {
        let renderer = SvgRenderer::new(Arc::new(()));
        let ink = |source: &str| {
            let svg = render_svg_for_preview(source, ThemeId::DARK).expect("深色 SVG");
            let image = rasterize_svg(&renderer, &svg, 800).expect("真实光栅化必须成功");
            let bytes = image.image.as_bytes(0).expect("首帧像素");
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[0] > 220 && pixel[1] > 220 && pixel[2] > 220)
                .count()
        };
        let empty = ink("flowchart TD\n  A[ ] --> B[ ]\n");
        let cjk = ink("flowchart TD\n  开始[开始] --> 结束[结束]\n");
        assert_eq!(empty, 0, "空标签对照不应有文字色像素");
        assert!(
            cjk >= 100,
            "深色 CJK 标签必须画出文字墨迹（实际 {cjk} 像素）"
        );
    }

    #[test]
    fn service_is_loading_then_ready_without_sleep() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let service = MermaidService::build(
            Arc::new(()),
            blocked_renderer(Arc::clone(&gate), Arc::clone(&calls)),
            MERMAID_CACHE_BUDGET_BYTES,
        );
        let key = MermaidCacheKey::new("flowchart TD\nA-->B\n", 700.0);
        service.request(key, "flowchart TD\nA-->B\n");
        assert!(matches!(service.state(key), MermaidPreviewState::Loading));
        // 同一请求不会重复入队：worker 可能还没跑到渲染，但渲染计数最终必须只有 1。
        service.request(key, "flowchart TD\nA-->B\n");
        release(&gate);
        assert!(service.wait_idle(Duration::from_secs(10)), "渲染必须收敛");
        let state = service.state(key);
        assert!(matches!(state, MermaidPreviewState::Ready(_)), "{state:?}");
        assert_eq!(service.request_count(), 1, "同一 key 只能请求一次");
        assert_eq!(service.render_count(), 1, "同一 key 只能渲染一次");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "worker 只应收到一次渲染");
        assert!(service.take_completed() >= 1);
        // 缓存命中：再请求同一 key 不产生第二次渲染。
        service.request(key, "flowchart TD\nA-->B\n");
        assert_eq!(service.request_count(), 1, "缓存命中不得再请求");
        assert_eq!(service.render_count(), 1);
        assert_eq!(service.cache_entries(), 1);
    }

    #[test]
    fn light_and_dark_render_once_each_and_cache_per_theme() {
        let calls: Arc<Mutex<Vec<ThemeId>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_worker = Arc::clone(&calls);
        let render: Arc<RenderFn> = Arc::new(move |_source: &str, theme: ThemeId| {
            calls_worker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(theme);
            if theme == ThemeId::DARK {
                Ok(DARK_TINY_SVG.to_owned())
            } else {
                Ok(TINY_SVG.to_owned())
            }
        });
        let service = MermaidService::build(Arc::new(()), render, MERMAID_CACHE_BUDGET_BYTES);
        let source = "flowchart TD\nA-->B\n";
        let light = MermaidCacheKey::new_for_theme(source, 700.0, ThemeId::LIGHT);
        let dark = MermaidCacheKey::new_for_theme(source, 700.0, ThemeId::DARK);
        assert_ne!(light, dark, "浅/深色必须是不同 key");

        service.request(light, source);
        service.request(dark, source);
        assert!(service.wait_idle(Duration::from_secs(10)), "渲染必须收敛");
        assert_eq!(service.render_count(), 2, "浅/深色各渲染一次");
        assert_eq!(service.cache_entries(), 2);
        {
            let themes = calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            assert_eq!(themes.len(), 2, "worker 只应收到两次渲染：{themes:?}");
            assert!(themes.contains(&ThemeId::LIGHT), "{themes:?}");
            assert!(themes.contains(&ThemeId::DARK), "{themes:?}");
        }
        assert!(matches!(
            service.state(light),
            MermaidPreviewState::Ready(_)
        ));
        assert!(matches!(service.state(dark), MermaidPreviewState::Ready(_)));

        // 同主题重复请求：命中缓存，不再渲染。
        service.request(light, source);
        service.request(dark, source);
        assert_eq!(service.request_count(), 2, "缓存命中不得再请求");
        assert_eq!(service.render_count(), 2, "缓存命中不得再渲染");
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len(),
            2
        );

        // 新 key（例如窗口宽度跨桶 / 切主题）才再渲染一次；同 key 永远命中。
        let dark_wider = MermaidCacheKey::new_for_theme(source, 900.0, ThemeId::DARK);
        service.request(dark_wider, source);
        assert!(service.wait_idle(Duration::from_secs(10)));
        assert_eq!(service.render_count(), 3, "新 key 必须再渲染一次");
        assert_eq!(service.request_count(), 3);
    }

    #[test]
    fn service_caches_failures_and_never_panics() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_worker = Arc::clone(&calls);
        let render: Arc<RenderFn> = Arc::new(move |_source: &str, _theme: ThemeId| {
            calls_worker.fetch_add(1, Ordering::SeqCst);
            Err(MermaidError::Render("注入的语法错误".to_owned()))
        });
        let service = MermaidService::build(Arc::new(()), render, MERMAID_CACHE_BUDGET_BYTES);
        let key = MermaidCacheKey::new("bad", 700.0);
        service.request(key, "bad");
        assert!(service.wait_idle(Duration::from_secs(10)));
        assert!(matches!(
            service.state(key),
            MermaidPreviewState::Failed(MermaidPreviewError::Render(_))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        service.request(key, "bad");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "失败也是终态，不反复重试");
    }

    #[test]
    fn retry_discards_the_terminal_state_and_renders_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_worker = Arc::clone(&calls);
        let render: Arc<RenderFn> = Arc::new(move |_source: &str, _theme: ThemeId| {
            calls_worker.fetch_add(1, Ordering::SeqCst);
            Ok(TINY_SVG.to_owned())
        });
        let service = MermaidService::build(Arc::new(()), render, MERMAID_CACHE_BUDGET_BYTES);
        let key = MermaidCacheKey::new("flowchart TD\nA-->B\n", 700.0);
        service.request(key, "flowchart TD\nA-->B\n");
        assert!(service.wait_idle(Duration::from_secs(10)));
        assert_eq!(service.render_count(), 1);
        assert!(
            service.retry(key, "flowchart TD\nA-->B\n"),
            "终态必须可重渲染"
        );
        assert!(
            !service.retry(key, "flowchart TD\nA-->B\n"),
            "在途不重复排队"
        );
        assert!(service.wait_idle(Duration::from_secs(10)));
        assert_eq!(service.request_count(), 2, "重新渲染必须再请求一次");
        assert_eq!(service.render_count(), 2, "重新渲染必须再执行一次");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn service_catches_renderer_panics_as_failures() {
        let render: Arc<RenderFn> = Arc::new(|_source: &str, _theme: ThemeId| panic!("注入 panic"));
        let service = MermaidService::build(Arc::new(()), render, MERMAID_CACHE_BUDGET_BYTES);
        let key = MermaidCacheKey::new("boom", 700.0);
        service.request(key, "boom");
        assert!(service.wait_idle(Duration::from_secs(10)));
        match service.state(key) {
            MermaidPreviewState::Failed(MermaidPreviewError::Render(reason)) => {
                assert!(reason.contains("注入 panic"), "{reason}");
            }
            other => panic!("预期 Failed(Render)，实际 {other:?}"),
        }
    }

    #[test]
    fn cache_evicts_oldest_entries_to_stay_within_budget() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_worker = Arc::clone(&calls);
        let render: Arc<RenderFn> = Arc::new(move |_source: &str, _theme: ThemeId| {
            calls_worker.fetch_add(1, Ordering::SeqCst);
            Ok(TINY_SVG.to_owned())
        });
        let service = MermaidService::build(Arc::new(()), render, 1);
        let keys: Vec<MermaidCacheKey> = (0..4)
            .map(|index| MermaidCacheKey::new(&format!("source-{index}"), 700.0))
            .collect();
        for (index, key) in keys.iter().enumerate() {
            service.request(*key, &format!("source-{index}"));
        }
        assert!(service.wait_idle(Duration::from_secs(10)));
        // 预算 1 字节：每写入一张图都会淘汰其它 Ready，只剩最后写入的一项。
        assert_eq!(service.cache_entries(), 1);
        assert!(matches!(
            service.state(*keys.last().expect("最后一项")),
            MermaidPreviewState::Ready(_)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
