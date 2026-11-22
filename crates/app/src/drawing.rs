//! 画布笔记（`.excalidraw`）+ Excalidraw 编辑器覆盖层。
//!
//! ## 产品模型（已确认的架构 A）
//!
//! 画布**就是普通笔记**：标题以 `.excalidraw` 结尾（大小写不敏感）即被识别，
//! 正文存 `.excalidraw` JSON 信封（SQLite/Loro 仍是唯一真源）。因此：
//!
//! - **不新增** note kind / schema 列 / 同步消息——标题就是文件名语义；
//! - 打开画布笔记时照常创建 [`crate::editor::view::EditorView`]（正文是 JSON 文本），
//!   再在内容区叠一层 Excalidraw 覆盖层；vendor 命中已按画布原点校正，侧栏保持可见；
//! - 保存 = 把 [`gpui_excalidraw::Editor::scene_json`] 一次性写回活动编辑器的源文
//!   （一条 replace-all 编辑 / 一个撤销组），然后关闭覆盖层；
//! - 取消 = 丢弃覆盖层，正文一个字节都不动。
//!
//! ## 为什么覆盖层是独立实体
//!
//! Excalidraw 编辑器自己持有场景、撤销栈、工具态与图片缓存（都在
//! [`gpui_excalidraw::Editor`] 里）。把它塞进 `EditorView` 会让两个编辑内核共享
//! 生命周期；独立实体则让"打开/关闭画布"= 创建/丢弃一个实体，互不污染。
//!
//! ## Markdown 内嵌预览（ADR 0025 后续切片）
//!
//! 普通 Markdown 笔记里的整行 `![[目标.excalidraw]]` 会投影成一个图片式 Object
//! Block，预览图**不在主线程解析/渲染**：编辑器只按合成资产 key
//! （[`drawing_asset_key`]，带当前主题 id）向已有的 `ImageService` 登记请求；
//! 宿主注入的资产 provider 识别 `drawing://` key，读目标笔记正文并调用
//! [`scene_json_to_png`] 在后台线程生成 PNG。本模块只提供**纯 helper + key 协议**，
//! 不做库读取、不做线程调度。
//!
//! ## 已知限制（本轮明确不做）
//!
//! - 覆盖层**不**处理 `Esc`/点外部关闭：误触会丢画布改动，必须点「保存并关闭」或
//!   「关闭（不保存）」；
//! - vendor 的命中用**窗口坐标**（`MouseDownEvent.position` 直接当画布坐标），
//!   覆盖层挂在编辑器内容区；保存/关闭动作放在 shell 状态栏，不用悬浮 bar；
//! - 画布预览不跟随缩略图缩放/分页缓存（`ImageService` 的 LRU 预算内按主题缓存）。

use gpui::prelude::*;
use gpui::{div, App, Context, Entity, Render, Window};
use gpui_excalidraw::core::scene::{AppState, Scene};
use gpui_excalidraw::core::types::Theme as ExcalidrawTheme;
use gpui_excalidraw::Editor;

use crate::editor::theme;

/// 画布笔记的标题后缀（大小写不敏感）。
pub const DRAWING_SUFFIX: &str = ".excalidraw";

/// 画布预览的**合成资产 key** 前缀（不是真实 `asset://` 内容寻址哈希）。
///
/// 协议：`drawing://<theme>/<target>`。
///
/// - `<theme>`：当前主题 id（`light` / `dark`）；深/浅色的画布渲染结果不同，
///   必须各自缓存，否则切主题会拿到反色的旧图；
/// - `<target>`：以 `.excalidraw` 结尾的目标笔记标题（原样，不 URL 编码；可能
///   包含 `/` 表示子目录，因此按**第一个** `/` 分割主题与目标）。
///
/// 宿主注入的 `AssetBytesProvider` 用 [`parse_drawing_asset_key`] 识别本协议并
/// 生成 PNG；`EditorView` 只负责 `ImageService::request(key)` 与绘制。
pub const DRAWING_ASSET_PREFIX: &str = "drawing://";

/// 生成画布预览的合成资产 key（含**当前**主题 id）。
///
/// 只做字符串拼接，不读库、不渲染；`target` 两侧空白会被去掉（解析器已经 trim
/// 过一次，这里再防御）。目标为空时仍返回 key（调用方应先判空；provider 会按
/// 缺失占位处理）。
#[must_use]
pub fn drawing_asset_key(target: &str) -> String {
    let target = target.trim();
    format!(
        "{DRAWING_ASSET_PREFIX}{}/{target}",
        crate::theme::theme_id().as_str()
    )
}

/// 解析画布预览的合成资产 key → `(主题 id, 目标标题)`。
///
/// 不是 `drawing://` 前缀 / 缺少分隔 `/` / 主题或目标为空时返回 `None`
/// （provider 据此把**非**画布 key 交回原有资产路径）。
#[must_use]
pub fn parse_drawing_asset_key(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix(DRAWING_ASSET_PREFIX)?;
    let (theme, target) = rest.split_once('/')?;
    if theme.is_empty() || target.is_empty() {
        return None;
    }
    Some((theme, target))
}

/// 主题 id 是否深色（画布预览背景/渲染参数用；未知 id 按浅色处理）。
#[must_use]
pub fn drawing_theme_is_dark(theme: &str) -> bool {
    theme == crate::theme::ThemeId::DARK.as_str()
}

/// 画布预览 PNG 的导出倍率（1.0 = 逻辑尺寸；控制内存，不随 DPI 放大）。
pub const DRAWING_PREVIEW_SCALE: f32 = 1.0;
/// 深色画布的预览背景（与 Excalidraw 深色画布一致）。
pub const DRAWING_BACKGROUND_DARK: &str = "#1e1e1e";
/// 浅色画布的预览背景（与文档底色一致）。
pub const DRAWING_BACKGROUND_LIGHT: &str = "#ffffff";

/// `.excalidraw` 场景 JSON → PNG（**纯 CPU helper**，不要求 GPUI 上下文）。
///
/// 供宿主资产 provider 在后台线程调用：
///
/// 1. `gpui_excalidraw::Editor::new()` 是纯结构（不依赖 `App`/窗口），
///    `load_scene_json` 解析信封；空串 / 纯空白 / 非 JSON / 结构损坏都返回
///    `Err`（**不 panic**），调用方降级成「画布笔记不存在 / 预览失败」占位；
/// 2. 用 `render::export::export_png` 把场景光栅化成 PNG；背景按深/浅色固定
///    （[`DRAWING_BACKGROUND_DARK`] / [`DRAWING_BACKGROUND_LIGHT`]），倍率
///    [`DRAWING_PREVIEW_SCALE`]。
///
/// # Errors
///
/// 场景 JSON 不合法，或 vendor 导出失败时返回可读错误文本；调用方只做降级，
/// 不把错误写进正文。
pub fn scene_json_to_png(scene_json: &str, dark: bool) -> Result<Vec<u8>, String> {
    let mut editor = Editor::new();
    editor.load_scene_json(scene_json)?;
    let background = if dark {
        DRAWING_BACKGROUND_DARK
    } else {
        DRAWING_BACKGROUND_LIGHT
    };
    gpui_excalidraw::render::export::export_png(
        &editor.document.scene,
        background,
        DRAWING_PREVIEW_SCALE,
    )
}

/// 标题是否是画布笔记：`.excalidraw` 后缀，**大小写不敏感**。
///
/// 只做后缀判定，不要求前面有字符：标题恰好是 `.excalidraw` 也算画布（与
/// "文件名语义"一致，storage 允许任意标题）。
#[must_use]
pub fn is_drawing_title(title: &str) -> bool {
    // `to_ascii_lowercase` 只折叠 ASCII 字母，非 ASCII 标题原样保留；后缀本身是
    // ASCII，因此判定与语言/编码无关。
    title.to_ascii_lowercase().ends_with(DRAWING_SUFFIX)
}

/// 侧栏/标题栏展示用的标题。
///
/// 当前**保留原样**（含 `.excalidraw` 后缀）：用户能一眼看出这是画布笔记，
/// 与 Obsidian 的文件名语义一致。留这个函数是为了将来"隐藏后缀"只改一处。
#[must_use]
pub fn display_title(title: &str) -> &str {
    title
}

/// 画布会话：Excalidraw 编辑器实体 + 打开时的场景基准。
///
/// `initial_json` 是**打开时**的场景序列化结果，用于：
///
/// - `dirty` 判定（initial vs current 的 JSON 比较，不依赖 vendor 内部状态）；
/// - 诊断/日志（"打开时有多少元素"）。
pub struct DrawingSession {
    /// Excalidraw 编辑器实体（场景 + 撤销栈 + 工具态都在里面）。
    editor: Entity<Editor>,
    /// 打开时的场景 JSON（脏判定基准）。
    initial_json: String,
}

impl DrawingSession {
    /// 用源文（`.excalidraw` JSON 文本；空/空白/非 JSON 都合法）新建会话。
    pub fn new(source: &str, cx: &mut App) -> Self {
        let editor = cx.new(|_| {
            let mut editor = Editor::new();
            editor.set_theme(editor_theme());
            editor
        });
        let mut session = Self {
            editor,
            initial_json: String::new(),
        };
        session.load_scene_json(source, cx);
        session
    }

    /// 用源文加载场景。
    ///
    /// 空串 / 纯空白 / 非 JSON / 结构不合法一律**回退空场景**（不 panic、不弹错）：
    /// 新建画布笔记的正文就是空串，损坏的正文也不该让覆盖层打不开。
    pub fn load_scene_json(&mut self, source: &str, cx: &mut App) {
        let loaded = self
            .editor
            .update(cx, |editor, _| editor.load_scene_json(source));
        if let Err(error) = loaded {
            eprintln!("[menghunote] 画布场景 JSON 无效（{error}）：回退空场景");
            self.editor.update(cx, |editor, _| {
                editor.replace_scene(Scene::new(), AppState::default());
            });
        }
        // 基准必须取**规范化后**的 JSON：源文是空串时，基准是空场景信封而不是空串，
        // 否则"打开即脏"。
        self.initial_json = self.scene_json(cx).unwrap_or_default();
    }

    /// 当前场景的 `.excalidraw` JSON 信封。
    ///
    /// 失败只可能是序列化错误（`serde_json` 对内存里的场景几乎不可能失败），
    /// 调用方按"保存失败"提示，**不关闭**覆盖层。
    pub fn scene_json(&self, cx: &App) -> Result<String, String> {
        self.editor.read(cx).scene_json()
    }

    /// 场景是否与打开时不同（initial vs current 的 JSON 比较）。
    #[must_use]
    pub fn dirty(&self, cx: &App) -> bool {
        self.scene_json(cx)
            .map_or(true, |current| current != self.initial_json)
    }

    /// Excalidraw 编辑器实体（渲染用）。
    #[must_use]
    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }
}

/// 当前应用主题 → Excalidraw 主题（深色模式下画布也跟着深色）。
fn editor_theme() -> ExcalidrawTheme {
    if crate::theme::theme_id() == crate::theme::ThemeId::DARK {
        ExcalidrawTheme::Dark
    } else {
        ExcalidrawTheme::Light
    }
}

/// 覆盖层事件：宿主（`AppShell`）负责真正的保存/关闭。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawingOverlayEvent {
    /// 「保存并关闭」：把 `scene_json()` 写回活动编辑器。
    Save,
    /// 「关闭（不保存）」：丢弃覆盖层改动。
    Close,
}

impl gpui::EventEmitter<DrawingOverlayEvent> for DrawingOverlay {}

/// 画布覆盖层：内容区的 Excalidraw 编辑器。保存/关闭动作放在 shell 状态栏，
/// 不再用悬浮 bar 遮挡 Excalidraw 自己的工具栏。
///
/// vendor 的鼠标命中已按 `canvas` 元素在窗口里的原点校正；覆盖层只占编辑器
/// 内容区，侧栏保持可见。`Esc`/点外部**不**关闭（避免误丢画布改动）。
pub struct DrawingOverlay {
    /// 笔记标题（bar 上显示 `画布 · <标题>`）。
    title: String,
    /// 场景会话。
    session: DrawingSession,
}

impl DrawingOverlay {
    /// 用笔记标题 + 正文源文创建覆盖层。
    pub fn new(title: impl Into<String>, source: &str, cx: &mut App) -> Self {
        Self {
            title: title.into(),
            session: DrawingSession::new(source, cx),
        }
    }

    /// 笔记标题。
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// 当前场景 JSON（保存路径用）。
    pub fn scene_json(&self, cx: &App) -> Result<String, String> {
        self.session.scene_json(cx)
    }

    /// Excalidraw 编辑器实体（headless 测试模拟画布命中用；生产不依赖）。
    #[must_use]
    pub fn editor_entity(&self) -> Entity<Editor> {
        self.session.editor().clone()
    }

    /// 用源文重新加载场景（headless 测试注入矩形/损坏 JSON 用）。
    ///
    /// 生产路径只在创建时加载一次；暴露它是为了让集成测试不必模拟画布点击
    /// （vendor 的命中依赖窗口坐标，headless 下不可靠）。
    pub fn load_scene_json(&mut self, source: &str, cx: &mut App) {
        self.session.load_scene_json(source, cx);
    }

    /// 场景是否与打开时不同。
    #[must_use]
    pub fn dirty(&self, cx: &App) -> bool {
        self.session.dirty(cx)
    }
}

impl Render for DrawingOverlay {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.session.editor().clone();
        // 覆盖层只占编辑器内容区；保存/关闭动作在 shell 状态栏，避免遮挡
        // Excalidraw 自己的工具栏。
        div()
            .id("drawing-overlay")
            // 测试元数据（只读断言用）：覆盖层存在 = 打开；`debug_bounds` 为 `None` = 关闭。
            .debug_selector(|| "drawing-overlay".to_string())
            .absolute()
            .inset_0()
            .bg(theme::window_bg())
            // 覆盖层语义：填满编辑器内容区，不把点击漏给下面的编辑器。
            .occlude()
            .child(
                div()
                    // 测试元数据（只读断言用）：Excalidraw 编辑器铺满整个窗口。
                    .debug_selector(|| "drawing-editor".to_string())
                    .size_full()
                    .relative()
                    .child(editor),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{display_title, is_drawing_title, DrawingSession, DRAWING_SUFFIX};
    use gpui::TestAppContext;

    #[test]
    fn suffix_matches_case_insensitively() {
        assert!(is_drawing_title("未命名画布.excalidraw"));
        assert!(is_drawing_title("未命名画布.EXCALIDRAW"));
        assert!(is_drawing_title("未命名画布.Excalidraw"));
        assert!(is_drawing_title("a.b.excalidraw"));
        assert!(is_drawing_title(DRAWING_SUFFIX));
        assert!(!is_drawing_title("未命名画布"));
        assert!(!is_drawing_title("未命名画布.excalidraw.md"));
        assert!(!is_drawing_title("excalidraw"));
        assert!(!is_drawing_title(""));
        // 非 ASCII 标题不受 ASCII 折叠影响。
        assert!(is_drawing_title("画布.excalidraw"));
        assert!(!is_drawing_title("画布.画布"));
    }

    #[test]
    fn display_title_keeps_suffix() {
        assert_eq!(
            display_title("未命名画布.excalidraw"),
            "未命名画布.excalidraw"
        );
        assert_eq!(display_title("普通笔记"), "普通笔记");
    }

    #[gpui::test]
    fn empty_source_loads_empty_scene(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let session = DrawingSession::new("", cx);
            let json = session.scene_json(cx).expect("空场景必须能序列化");
            assert!(
                json.contains("\"elements\""),
                "空场景也要是完整信封：{json}"
            );
            assert!(
                !session.dirty(cx),
                "刚打开（空正文）不能算脏：{}",
                session.dirty(cx)
            );
        });
    }

    #[gpui::test]
    fn whitespace_and_garbage_sources_fall_back_to_empty_scene(cx: &mut TestAppContext) {
        cx.update(|cx| {
            for source in ["   \n\t ", "not json at all", "{\"elements\": 42}"] {
                let session = DrawingSession::new(source, cx);
                let json = session.scene_json(cx).expect("回退后必须能序列化");
                assert!(
                    json.contains("\"elements\""),
                    "损坏源文 {source:?} 必须回退空场景：{json}"
                );
                assert!(!session.dirty(cx), "回退后的基准必须等于当前场景");
            }
        });
    }

    #[gpui::test]
    fn valid_json_roundtrips(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // 先用 vendor 的工厂造一个最小场景，再序列化成源文。
            let mut scene = gpui_excalidraw::core::scene::Scene::new();
            scene.add(gpui_excalidraw::core::factory::new_rectangle(
                10.0,
                20.0,
                30.0,
                40.0,
                &gpui_excalidraw::core::factory::ElementOptions::default(),
            ));
            let source = gpui_excalidraw::scene_to_json(
                &scene,
                &gpui_excalidraw::core::scene::AppState::default(),
            )
            .expect("序列化场景失败");

            let session = DrawingSession::new(&source, cx);
            let roundtrip = session.scene_json(cx).expect("合法场景必须能序列化");
            assert!(
                roundtrip.contains("\"type\": \"excalidraw\""),
                "必须是 .excalidraw 信封：{roundtrip}"
            );
            assert!(
                roundtrip.contains("\"rectangle\""),
                "矩形必须保留：{roundtrip}"
            );
            assert!(!session.dirty(cx), "刚打开的合法场景不能算脏");
        });
    }

    #[gpui::test]
    fn dirty_tracks_scene_changes(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let session = DrawingSession::new("", cx);
            assert!(!session.dirty(cx));
            // 直接改场景（模拟用户在画布上画了一个矩形）。
            let editor = session.editor().clone();
            editor.update(cx, |editor, _| {
                editor
                    .document
                    .scene
                    .add(gpui_excalidraw::core::factory::new_rectangle(
                        0.0,
                        0.0,
                        10.0,
                        10.0,
                        &gpui_excalidraw::core::factory::ElementOptions::default(),
                    ));
            });
            assert!(session.dirty(cx), "场景变了必须算脏");
        });
    }

    // ---- Markdown 内嵌预览（ADR 0025 后续切片）----

    #[test]
    fn drawing_asset_key_roundtrips_with_theme_and_target() {
        let key = super::drawing_asset_key("流程图.excalidraw");
        let (theme, target) = super::parse_drawing_asset_key(&key).expect("key 必须可解析");
        assert_eq!(theme, crate::theme::theme_id().as_str());
        assert_eq!(target, "流程图.excalidraw");
        // 目标可能带子目录：只按第一个 `/` 分割主题与目标。
        let key = super::drawing_asset_key("文件夹/子目录/图.excalidraw");
        assert_eq!(
            super::parse_drawing_asset_key(&key),
            Some((
                crate::theme::theme_id().as_str(),
                "文件夹/子目录/图.excalidraw"
            ))
        );
        // target 两侧空白归一。
        assert_eq!(
            super::parse_drawing_asset_key(&super::drawing_asset_key(" a.excalidraw ")),
            Some((crate::theme::theme_id().as_str(), "a.excalidraw"))
        );
    }

    #[test]
    fn parse_drawing_asset_key_rejects_foreign_or_malformed_keys() {
        for key in [
            "",
            "asset://deadbeef",
            "drawing://",
            "drawing://light",
            "drawing://light/",
            "drawing:///a.excalidraw",
            "DRAWING://light/a.excalidraw",
        ] {
            assert!(
                super::parse_drawing_asset_key(key).is_none(),
                "非法 key 必须拒绝：{key:?}"
            );
        }
        assert!(!super::drawing_theme_is_dark("light"));
        assert!(super::drawing_theme_is_dark("dark"));
        assert!(!super::drawing_theme_is_dark("unknown"));
    }

    /// 一个最小可序列化的矩形场景（`.excalidraw` 信封）。
    fn rectangle_scene_json() -> String {
        let mut scene = gpui_excalidraw::core::scene::Scene::new();
        scene.add(gpui_excalidraw::core::factory::new_rectangle(
            10.0,
            20.0,
            120.0,
            80.0,
            &gpui_excalidraw::core::factory::ElementOptions::default(),
        ));
        gpui_excalidraw::scene_to_json(&scene, &gpui_excalidraw::core::scene::AppState::default())
            .expect("序列化测试场景失败")
    }

    #[test]
    fn scene_json_to_png_exports_a_real_png_for_both_themes() {
        let scene = rectangle_scene_json();
        for dark in [false, true] {
            let png = super::scene_json_to_png(&scene, dark)
                .unwrap_or_else(|error| panic!("导出必须成功（dark={dark}）：{error}"));
            assert!(
                png.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
                "必须是 PNG 字节（dark={dark}）"
            );
            assert!(png.len() > 64, "PNG 不能是空图（dark={dark}）");
        }
    }

    #[test]
    fn scene_json_to_png_rejects_broken_scene_json_without_panicking() {
        for source in ["", "   ", "not json", r#"{"elements": 42}"#] {
            let result = super::scene_json_to_png(source, false);
            assert!(
                result.is_err(),
                "损坏/空场景必须返回 Err（{source:?}）：{result:?}"
            );
        }
        // vendor 的 `json_to_scene` 把“没有 elements 键”当成空场景：导出成空白 PNG
        // 比拒绝更好（新建画布笔记的正文就是空串，用户在覆盖层里画之前不该报错）。
        let empty = super::scene_json_to_png("{}", false);
        assert!(empty.is_ok(), "空场景信封应导出空白 PNG：{empty:?}");
    }
}
