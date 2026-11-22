//! 图片资产加载与受控解码（ADR 0019 §2 / §5，S2a）。
//!
//! ## 边界
//!
//! - **只读取本地资产字节**：通过 [`AssetBytesProvider`] 注入（`AppShell` 用
//!   storage 的只读 `asset_bytes` 实现），本模块**不依赖 storage**，也绝不走
//!   HTTP / file 加载器；
//! - **不执行 SVG**：只解码 PNG / JPEG / WebP / GIF，其余格式降级为占位；
//! - **动画只取首帧**：GIF/WebP 动画不播，只解第一帧（原字节仍在 storage 里）；
//! - **预算**（ADR 0019 §2）：边长 ≤ 8192、总像素 ≤ 16MP、单图解码内存 ≤ 128MiB、
//!   解码并发 1、已解码缓存 LRU ≤ 128MiB；
//! - 超限 / 损坏 / 缺失**只降级占位**：不 panic、不删正文、不阻塞主线程。
//!
//! ## 线程模型
//!
//! 解码在**一个**后台线程上串行进行（[`ImageService`] 的 worker；并发 1 是结构性的）：
//!
//! ```text
//! 主线程（render/prepaint）: request(hash) → 只登记 inflight，立即返回 Loading
//! 后台 worker:  读 asset 字节 → 校验预算 → 解码 → 写回缓存 → completed += 1
//! 主线程（pump / wait）: take_completed() > 0 → 重绘；等 inflight 清空即可断言
//! ```
//!
//! 主线程永远不读 SQLite、不解码图片；`Mutex` 只保护小表，不跨 IO。

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use gpui::RenderImage;
use image::{DynamicImage, Frame, ImageDecoder as _, ImageFormat};

/// 单边最大像素（ADR 0019 §2）。
pub const MAX_IMAGE_EDGE: u32 = 8192;
/// 单图最大总像素（ADR 0019 §2）。
pub const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
/// 单图解码内存上限（RGBA → BGRA 后约 `w * h * 4` 字节）。
pub const MAX_IMAGE_ALLOC_BYTES: usize = 128 * 1024 * 1024;
/// 已解码图片缓存预算（LRU）。
pub const IMAGE_CACHE_BUDGET_BYTES: usize = 128 * 1024 * 1024;
/// `asset://<64 小写 hex>` 的哈希长度。
pub const ASSET_HASH_HEX_LEN: usize = 64;
/// content-addressed 资产链接前缀。
pub const ASSET_URL_PREFIX: &str = "asset://";

/// 只读资产字节的最小能力（**不含** storage 类型，EditorView 不反向依赖 storage）。
///
/// `AppShell` 用 `Vault` 的只读 `asset_bytes` 实现它；测试可以直接用内存里的
/// `HashMap` 或闭包。实现必须是 `Send + Sync`（worker 线程会调用它）。
pub trait AssetBytesProvider: Send + Sync + 'static {
    /// 按 64 位小写 hex 哈希取资产原始字节；不存在 / 只有元数据占位时返回 `None`。
    fn read_asset(&self, hash: &str) -> Option<Vec<u8>>;
}

impl<F> AssetBytesProvider for F
where
    F: Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static,
{
    fn read_asset(&self, hash: &str) -> Option<Vec<u8>> {
        self(hash)
    }
}

/// 共享的资产字节提供者（`Arc` 便于注入 worker 线程）。
pub type SharedAssetProvider = Arc<dyn AssetBytesProvider>;

/// 永远没有字节的 provider（默认构建 / 无 vault 的测试用；图片显示占位）。
#[must_use]
pub fn empty_asset_provider() -> SharedAssetProvider {
    Arc::new(|_hash: &str| None)
}

/// 从 Markdown 目标 URL 里取出 `asset://` 的哈希；不是合法资产链接返回 `None`。
///
/// 只接受 `asset://` + **64 个 ASCII 小写 hex**（ADR 0019 §3）；上限严格，
/// 不做大小写归一、不猜其它 scheme。
#[must_use]
pub fn parse_asset_hash(dest: &str) -> Option<&str> {
    let hash = dest.strip_prefix(ASSET_URL_PREFIX)?;
    if hash.len() != ASSET_HASH_HEX_LEN {
        return None;
    }
    if hash
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Some(hash)
    } else {
        None
    }
}

/// 图片解码 / 预算错误（全部可降级为占位，不影响正文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageLoadError {
    /// 超过边长 / 总像素 / 解码内存预算之一。
    TooLarge {
        /// 解码器报告的宽（像素）。
        width: u32,
        /// 解码器报告的高（像素）。
        height: u32,
    },
    /// 不支持的格式（SVG / BMP / TIFF… 首版不栅格化）。
    Unsupported,
    /// 字节损坏 / 解码失败 / 空图。
    Corrupt,
    /// 解码线程已关闭（视图关闭中）。
    Closed,
}

impl std::fmt::Display for ImageLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { width, height } => write!(
                formatter,
                "图片 {width}×{height} 超出预算（边长 ≤ {MAX_IMAGE_EDGE}、总像素 ≤ 16MP）"
            ),
            Self::Unsupported => {
                formatter.write_str("不支持的图片格式（首版只解码 PNG/JPEG/WebP/GIF）")
            }
            Self::Corrupt => formatter.write_str("图片字节损坏或为空"),
            Self::Closed => formatter.write_str("图片解码线程已关闭"),
        }
    }
}

impl std::error::Error for ImageLoadError {}

/// 已解码的图片（BGRA，供 GPUI 直接绘制；首帧）。
#[derive(Debug)]
pub struct DecodedImage {
    /// GPUI 可绘制的图片（`Arc` 便于跨帧共享）。
    pub image: Arc<RenderImage>,
    /// 像素宽。
    pub width: u32,
    /// 像素高。
    pub height: u32,
    /// 解码占用字节（`w * h * 4`，LRU 预算记账用）。
    pub byte_len: usize,
}

/// 图片的当前状态（app 绘制占位/图片用；克隆开销只有一次 `Arc` 增加）。
#[derive(Debug, Clone)]
pub enum ImageState {
    /// 资产不存在 / `data IS NULL`（待同步或缺失）——显示 alt / 待同步占位。
    Missing,
    /// 正在后台解码。
    Loading,
    /// 已解码（首帧）。
    Ready(Arc<DecodedImage>),
    /// 解码失败（损坏 / 超预算 / 不支持的格式）——显示降级占位。
    Failed(ImageLoadError),
}

/// 检查解码器报告的尺寸是否在预算内。
///
/// # Errors
///
/// 零尺寸（非法头）、超过边长/像素/内存预算时返回 [`ImageLoadError::TooLarge`]。
pub fn check_dimensions(width: u32, height: u32) -> Result<(), ImageLoadError> {
    if width == 0 || height == 0 {
        return Err(ImageLoadError::Corrupt);
    }
    if width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
        return Err(ImageLoadError::TooLarge { width, height });
    }
    let pixels = u64::from(width) * u64::from(height);
    let bytes = pixels.saturating_mul(4);
    if pixels > MAX_IMAGE_PIXELS || bytes > MAX_IMAGE_ALLOC_BYTES as u64 {
        return Err(ImageLoadError::TooLarge { width, height });
    }
    Ok(())
}

/// 解码一张图片的**第一帧**（ADR 0019 §2 的预算与格式边界）。
///
/// # Errors
///
/// 格式不支持 / 字节损坏 / 超预算时返回 [`ImageLoadError`]；调用方降级为占位。
pub fn decode_image(bytes: &[u8]) -> Result<DecodedImage, ImageLoadError> {
    let format = image::guess_format(bytes).map_err(|_| ImageLoadError::Corrupt)?;
    match format {
        ImageFormat::Png | ImageFormat::Jpeg => decode_static(bytes, format),
        ImageFormat::Gif => decode_animated_first_frame(bytes, AnimatedKind::Gif),
        ImageFormat::WebP => decode_webp(bytes),
        _ => Err(ImageLoadError::Unsupported),
    }
}

fn decode_static(bytes: &[u8], format: ImageFormat) -> Result<DecodedImage, ImageLoadError> {
    let reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut decoder = reader.into_decoder().map_err(|_| ImageLoadError::Corrupt)?;
    let (width, height) = decoder.dimensions();
    check_dimensions(width, height)?;
    // EXIF 方向只在解码前读取一次；像素旋转不改变尺寸预算的保守判定。
    let orientation = decoder.orientation().map_err(|_| ImageLoadError::Corrupt)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(|_| ImageLoadError::Corrupt)?;
    image.apply_orientation(orientation);
    let rgba = image.into_rgba8();
    finish(rgba)
}

fn decode_webp(bytes: &[u8]) -> Result<DecodedImage, ImageLoadError> {
    let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
        .map_err(|_| ImageLoadError::Corrupt)?;
    if decoder.has_animation() {
        return decode_animated_first_frame(bytes, AnimatedKind::Webp);
    }
    decode_static(bytes, ImageFormat::WebP)
}

#[derive(Clone, Copy)]
enum AnimatedKind {
    Gif,
    Webp,
}

/// 动画图片只取首帧（ADR 0019 §2）：一次解码不把整段动画读进内存。
fn decode_animated_first_frame(
    bytes: &[u8],
    kind: AnimatedKind,
) -> Result<DecodedImage, ImageLoadError> {
    use image::AnimationDecoder as _;
    let first = match kind {
        AnimatedKind::Gif => {
            let decoder = image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
                .map_err(|_| ImageLoadError::Corrupt)?;
            let (width, height) = decoder.dimensions();
            check_dimensions(width, height)?;
            decoder
                .into_frames()
                .next()
                .transpose()
                .map_err(|_| ImageLoadError::Corrupt)?
        }
        AnimatedKind::Webp => {
            let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(bytes))
                .map_err(|_| ImageLoadError::Corrupt)?;
            let (width, height) = decoder.dimensions();
            check_dimensions(width, height)?;
            let _ = decoder.set_background_color(image::Rgba([0, 0, 0, 0]));
            decoder
                .into_frames()
                .next()
                .transpose()
                .map_err(|_| ImageLoadError::Corrupt)?
        }
    };
    let Some(frame) = first else {
        return Err(ImageLoadError::Corrupt);
    };
    finish(frame.into_buffer())
}

/// RGBA → BGRA（GPUI 的 `RenderImage` 约定）并包成首帧。
fn finish(rgba: image::RgbaImage) -> Result<DecodedImage, ImageLoadError> {
    let (width, height) = rgba.dimensions();
    check_dimensions(width, height)?;
    let byte_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ImageLoadError::TooLarge { width, height })?;
    if byte_len > MAX_IMAGE_ALLOC_BYTES {
        return Err(ImageLoadError::TooLarge { width, height });
    }
    let mut bgra = rgba;
    // RGBA → BGRA：每 4 字节一个像素（`as_chunks_mut` 让长度不变量由类型保证）。
    let (pixels, remainder) = bgra.as_chunks_mut::<4>();
    debug_assert!(remainder.is_empty(), "RGBA 缓冲长度必然是 4 的倍数");
    for pixel in pixels {
        pixel.swap(0, 2);
    }
    let image = Arc::new(RenderImage::new([Frame::new(bgra)]));
    Ok(DecodedImage {
        image,
        width,
        height,
        byte_len,
    })
}

/// 一张已解码图片的缓存项。
struct CacheEntry {
    decoded: Arc<DecodedImage>,
    /// LRU 序号（越大越新）。
    tick: u64,
}

impl CacheEntry {
    fn byte_len(&self) -> usize {
        self.decoded.byte_len
    }
}

/// 一个哈希在缓存里的最终状态（`Ready` / `Failed` / `Missing` 都是终态，不重试）。
enum CacheSlot {
    Ready(CacheEntry),
    Failed(ImageLoadError),
    Missing,
}

/// 缓存 + inflight + 完成计数（单把锁）。
#[derive(Default)]
struct Inner {
    cache: HashMap<String, CacheSlot>,
    inflight: HashSet<String>,
    bytes: usize,
    tick: u64,
    completed: usize,
}

impl Inner {
    fn lock(inner: &Mutex<Self>) -> MutexGuard<'_, Self> {
        // 锁中毒也不能 panic：其它线程 panic 不该把图片加载整个带崩。
        inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 写入一个终态；`Ready` 超预算时按 LRU 淘汰（**总是保留刚写入的那一项**）。
    fn insert(&mut self, hash: String, slot: CacheSlot, budget: usize) {
        self.cache.remove(&hash);
        let ready_bytes = match &slot {
            CacheSlot::Ready(entry) => Some(entry.byte_len()),
            CacheSlot::Failed(_) | CacheSlot::Missing => None,
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
        self.cache.insert(hash.clone(), slot);
        // 只有写入已解码图片才会让缓存变大；Failed/Missing 是零字节终态，不触发淘汰。
        if ready_bytes.is_some() {
            self.evict(budget, &hash);
        }
    }

    /// LRU 淘汰：`bytes > budget` 或条目数超上限时，从最旧的开始删。
    fn evict(&mut self, budget: usize, keep: &str) {
        const MAX_ENTRIES: usize = 256;
        while self.bytes > budget || self.cache.len() > MAX_ENTRIES {
            let Some(oldest) = self
                .cache
                .iter()
                .filter(|(hash, slot)| hash.as_str() != keep && matches!(slot, CacheSlot::Ready(_)))
                .min_by_key(|(_, slot)| match slot {
                    CacheSlot::Ready(entry) => entry.tick,
                    _ => u64::MAX,
                })
                .map(|(hash, _)| hash.clone())
            else {
                break;
            };
            if let Some(CacheSlot::Ready(entry)) = self.cache.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(entry.byte_len());
            }
        }
    }

    fn state(&self, hash: &str) -> ImageState {
        match self.cache.get(hash) {
            Some(CacheSlot::Ready(entry)) => ImageState::Ready(Arc::clone(&entry.decoded)),
            Some(CacheSlot::Failed(error)) => ImageState::Failed(*error),
            Some(CacheSlot::Missing) => ImageState::Missing,
            None if self.inflight.contains(hash) => ImageState::Loading,
            None => ImageState::Missing,
        }
    }
}

enum Job {
    Load(String),
    Shutdown,
}

/// 图片加载服务：一个后台解码线程 + LRU 缓存（`Send + Sync`，可被视图共享）。
pub struct ImageService {
    tx: SyncSender<Job>,
    handle: Mutex<Option<JoinHandle<()>>>,
    inner: Arc<Mutex<Inner>>,
    wake: Arc<Condvar>,
    budget: usize,
}

impl std::fmt::Debug for ImageService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ImageService")
            .field("budget", &self.budget)
            .finish()
    }
}

impl ImageService {
    /// 用资产字节 provider 创建服务（启动一个后台解码线程）。
    #[must_use]
    pub fn new(provider: SharedAssetProvider) -> Self {
        Self::with_budget(provider, IMAGE_CACHE_BUDGET_BYTES)
    }

    /// 指定缓存预算（测试用：用极小预算验证 LRU 淘汰）。
    #[must_use]
    pub fn with_budget(provider: SharedAssetProvider, budget: usize) -> Self {
        let (tx, rx) = sync_channel::<Job>(256);
        let inner = Arc::new(Mutex::new(Inner::default()));
        let wake = Arc::new(Condvar::new());
        let worker_inner = Arc::clone(&inner);
        let worker_wake = Arc::clone(&wake);
        let worker_budget = budget;
        let handle = std::thread::Builder::new()
            .name("menghunote-image".to_owned())
            .spawn(move || worker_loop(rx, provider, worker_inner, worker_wake, worker_budget))
            .ok();
        Self {
            tx,
            handle: Mutex::new(handle),
            inner,
            wake,
            budget,
        }
    }

    /// 请求加载一个哈希（幂等：已缓存 / 在途 / 已失败都不会重复提交）。
    pub fn request(&self, hash: &str) {
        let mut inner = Inner::lock(&self.inner);
        if inner.cache.contains_key(hash) || inner.inflight.contains(hash) {
            return;
        }
        inner.inflight.insert(hash.to_owned());
        drop(inner);
        if self.tx.send(Job::Load(hash.to_owned())).is_err() {
            let mut inner = Inner::lock(&self.inner);
            inner.inflight.remove(hash);
            inner.insert(
                hash.to_owned(),
                CacheSlot::Failed(ImageLoadError::Closed),
                self.budget,
            );
            inner.completed = inner.completed.saturating_add(1);
            self.wake.notify_all();
        }
    }

    /// 当前状态（`Missing` 也包含“尚未请求”，调用方应先 [`Self::request`]）。
    #[must_use]
    pub fn state(&self, hash: &str) -> ImageState {
        Inner::lock(&self.inner).state(hash)
    }

    /// 已解码图片的显示尺寸：按内容宽度等比，最大不超过 `max_width`。
    ///
    /// 返回 `None` 表示还没解码好（加载中/失败/缺失）。
    #[must_use]
    pub fn display_size(&self, hash: &str, max_width: f32) -> Option<(f32, f32)> {
        let ImageState::Ready(decoded) = self.state(hash) else {
            return None;
        };
        fit_size(decoded.width, decoded.height, max_width)
    }

    /// 取走“自上次调用以来完成的解码数”（`pump` 用它触发一次重绘）。
    pub fn take_completed(&self) -> usize {
        std::mem::take(&mut Inner::lock(&self.inner).completed)
    }

    /// 等待所有在途解码结束（测试用收敛点；有界，**不 sleep 轮询**）。
    ///
    /// 生产代码不需要它：解码完成由 `take_completed()` + pump 重绘驱动。
    #[must_use]
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut inner = Inner::lock(&self.inner);
        while !inner.inflight.is_empty() {
            let Some(deadline) = deadline else {
                return false;
            };
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, wait) = self
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

impl Drop for ImageService {
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
    provider: SharedAssetProvider,
    inner: Arc<Mutex<Inner>>,
    wake: Arc<Condvar>,
    budget: usize,
) {
    while let Ok(job) = rx.recv() {
        let Job::Load(hash) = job else {
            break;
        };
        let slot = match provider.read_asset(&hash) {
            None => CacheSlot::Missing,
            Some(bytes) => match decode_image(&bytes) {
                Ok(decoded) => CacheSlot::Ready(CacheEntry {
                    decoded: Arc::new(decoded),
                    tick: 0,
                }),
                Err(error) => CacheSlot::Failed(error),
            },
        };
        let mut inner = Inner::lock(&inner);
        inner.inflight.remove(&hash);
        inner.insert(hash, slot, budget);
        inner.completed = inner.completed.saturating_add(1);
        drop(inner);
        wake.notify_all();
    }
}

/// 等比缩放到 `max_width` 内（不放大超过原始内容宽度）。
#[must_use]
pub fn fit_size(width: u32, height: u32, max_width: f32) -> Option<(f32, f32)> {
    if width == 0 || height == 0 || !max_width.is_finite() || max_width <= 0.0 {
        return None;
    }
    let intrinsic = width as f32;
    let display_width = intrinsic.min(max_width);
    let display_height = display_width * (height as f32) / intrinsic;
    if !display_height.is_finite() || display_height <= 0.0 {
        return None;
    }
    Some((display_width, display_height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let buffer = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(buffer)
            .write_to(&mut out, ImageFormat::Png)
            .expect("测试 PNG 编码失败");
        out.into_inner()
    }

    #[test]
    fn asset_hash_parsing_is_strict() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(parse_asset_hash(&format!("asset://{hash}")), Some(hash));
        assert_eq!(parse_asset_hash("asset://abc"), None);
        assert_eq!(parse_asset_hash(&format!("ASSET://{hash}")), None);
        assert_eq!(
            parse_asset_hash(&format!("asset://{}", hash.to_uppercase())),
            None,
            "只接受小写 hex"
        );
        assert_eq!(parse_asset_hash(&format!("file://{hash}")), None);
        assert_eq!(
            parse_asset_hash(&format!("asset://{hash}g")),
            None,
            "长度必须正好 64"
        );
    }

    #[test]
    fn dimension_budget_rejects_oversize_and_degenerate_images() {
        assert!(check_dimensions(1, 1).is_ok());
        assert_eq!(
            check_dimensions(0, 10),
            Err(ImageLoadError::Corrupt),
            "零尺寸是坏头"
        );
        assert!(matches!(
            check_dimensions(MAX_IMAGE_EDGE + 1, 1),
            Err(ImageLoadError::TooLarge { .. })
        ));
        // 8192×8192 = 67MP > 16MP。
        assert!(matches!(
            check_dimensions(8192, 8192),
            Err(ImageLoadError::TooLarge { .. })
        ));
        // 4000×4000 = 16MP，正好在预算内。
        assert!(check_dimensions(4000, 4000).is_ok());
    }

    #[test]
    fn decoding_a_real_png_yields_bgra_first_frame() {
        let bytes = png_bytes(4, 2);
        let decoded = decode_image(&bytes).expect("PNG 必须能解码");
        assert_eq!((decoded.width, decoded.height), (4, 2));
        assert_eq!(decoded.byte_len, 4 * 2 * 4);
        // 原像素 (10, 20, 30) → BGRA 首像素 (30, 20, 10, 255)。
        let raw = decoded.image.as_bytes(0).expect("首帧必须有字节");
        assert_eq!(&raw[..4], &[30, 20, 10, 255]);
    }

    #[test]
    fn corrupt_and_unsupported_bytes_degrade_instead_of_panicking() {
        assert_eq!(
            decode_image(b"not an image").err(),
            Some(ImageLoadError::Corrupt)
        );
        assert_eq!(decode_image(b"").err(), Some(ImageLoadError::Corrupt));
        // BMP 不在首版支持列表里（不栅格化未知格式）。
        let mut bmp = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([1, 2, 3, 255]),
        ))
        .write_to(&mut bmp, ImageFormat::Bmp)
        .expect("测试 BMP 编码失败");
        assert_eq!(
            decode_image(&bmp.into_inner()).err(),
            Some(ImageLoadError::Unsupported)
        );
    }

    #[test]
    fn fit_size_scales_down_to_the_content_width_only() {
        assert_eq!(fit_size(100, 50, 500.0), Some((100.0, 50.0)));
        assert_eq!(fit_size(1000, 500, 400.0), Some((400.0, 200.0)));
        assert_eq!(fit_size(0, 10, 400.0), None);
        assert_eq!(fit_size(10, 10, f32::NAN), None);
    }

    /// 缓存 LRU：超预算时淘汰最旧的 Ready 项，且**总是保留新写入的那一项**。
    #[test]
    fn cache_evicts_oldest_entries_to_stay_within_budget() {
        let bytes = png_bytes(2, 2);
        let decoded = Arc::new(decode_image(&bytes).expect("PNG 必须能解码"));
        let entry = || {
            CacheSlot::Ready(CacheEntry {
                decoded: Arc::clone(&decoded),
                tick: 0,
            })
        };
        let mut inner = Inner::default();
        inner.insert("a".to_owned(), entry(), 0);
        assert!(matches!(inner.state("a"), ImageState::Ready(_)));
        inner.insert("b".to_owned(), entry(), 0);
        // 预算为 0：每写入一项就淘汰其它项，只剩刚写入的。
        assert!(matches!(inner.state("b"), ImageState::Ready(_)));
        assert!(matches!(inner.state("a"), ImageState::Missing));
        assert_eq!(inner.bytes, decoded.byte_len);
        // Failed / Missing 不被计入字节数，也不会被 LRU 当成淘汰对象。
        inner.insert("missing".to_owned(), CacheSlot::Missing, 0);
        assert!(matches!(inner.state("missing"), ImageState::Missing));
        assert_eq!(inner.bytes, decoded.byte_len);
    }

    #[test]
    fn service_loads_missing_and_ready_states_off_the_main_thread() {
        let bytes = png_bytes(3, 3);
        let provider: SharedAssetProvider = Arc::new(move |hash: &str| {
            if hash == "present" {
                Some(bytes.clone())
            } else {
                None
            }
        });
        let service = ImageService::new(provider);
        service.request("present");
        service.request("absent");
        assert!(service.wait_idle(Duration::from_secs(10)), "解码必须收敛");
        assert!(matches!(service.state("present"), ImageState::Ready(_)));
        assert!(matches!(service.state("absent"), ImageState::Missing));
        assert_eq!(service.display_size("present", 100.0), Some((3.0, 3.0)));
        assert_eq!(service.display_size("absent", 100.0), None);
        assert!(service.take_completed() >= 1);
    }

    #[test]
    fn service_degrades_corrupt_assets_to_failed() {
        let provider: SharedAssetProvider = Arc::new(|_: &str| Some(b"broken".to_vec()));
        let service = ImageService::new(provider);
        service.request("bad");
        assert!(service.wait_idle(Duration::from_secs(10)));
        assert!(matches!(
            service.state("bad"),
            ImageState::Failed(ImageLoadError::Corrupt)
        ));
    }
}
