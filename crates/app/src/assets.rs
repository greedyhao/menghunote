//! 应用统一资产源（`gpui::platform::application().with_assets(AppAssets)`）。
//!
//! ## 为什么需要这一层
//!
//! 应用同时用到两套嵌入式资源：
//!
//! 1. **GPUI Kit 默认资产**（`gpui::assets::Assets`，即 `gpui-kit-assets` 的
//!    Lucide 图标集）——组件库/框架的 `svg()` 路径都从这里取；
//! 2. **gpui-excalidraw 的手绘图标集**（[`IconAssets`]）——画布工具栏、菜单、
//!    属性面板的 140 个上游图标，**不在** GPUI Kit 的资产里。
//!
//! 只注册其中一个，另一套就会渲染成空白字形（vendor README 的 "Registry note"
//! 就是提醒这一点）。[`AppAssets`] 先查手绘图标、未命中再回退到 GPUI Kit 默认
//! 资产，两套资源因此可以共存于同一个 `AssetSource`。
//!
//! ## 为什么不是 `IconAssets` 单独注册
//!
//! `IconAssets` 自己已经回退到 `gpui_kit::assets::Assets`（见其 `load` 实现），
//! 但应用侧仍需要一个**本 crate 的类型**：`with_assets` 是泛型参数，直接传
//! `IconAssets` 会把 vendor 类型泄漏到 `main.rs` 的启动路径里，而且后续要叠加
//! 应用自己的图标时没有落点。这里的包装是零成本的（`Copy` 空结构体）。

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_excalidraw::ui::icons::IconAssets;

/// 应用资产源：gpui-excalidraw 手绘图标优先，未命中回退 GPUI Kit 默认资产。
#[derive(Clone, Copy, Debug, Default)]
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        // `IconAssets::load_with` 先查手绘图标（大小写不敏感回退），未命中才调
        // `inner`；`inner` 的错误（GPUI Kit 的 `RustEmbed` 找不到路径）原样上抛，
        // 与只注册默认资产时的行为一致。
        IconAssets.load_with(path, |path| gpui::assets::Assets.load(path))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        IconAssets.list_with(path, |path| gpui::assets::Assets.list(path))
    }
}

#[cfg(test)]
mod tests {
    use super::AppAssets;
    use gpui::AssetSource;

    /// 手绘图标（`icons/*.svg`）必须能从应用资产源里取到；这是画布工具栏不空白的
    /// 唯一保证。
    #[test]
    fn serves_hand_drawn_icons() {
        let bytes = AppAssets
            .load("icons/SelectionIcon.svg")
            .expect("加载手绘图标失败")
            .expect("手绘图标必须由 AppAssets 提供");
        assert!(
            bytes.starts_with(b"<svg") || bytes.starts_with(b"<?xml"),
            "手绘图标必须是 SVG 文本"
        );
    }

    /// 大小写不敏感回退：vendor 资产里同时存在 `HelpIcon.svg` / `helpIcon.svg`，
    /// 大小写不敏感文件系统只能存一份，另一份必须仍能取到。
    #[test]
    fn falls_back_case_insensitively() {
        let upper = AppAssets.load("icons/helpIcon.svg").expect("加载失败");
        let lower = AppAssets.load("icons/HelpIcon.svg").expect("加载失败");
        assert!(upper.is_some(), "helpIcon.svg 必须可加载");
        assert!(lower.is_some(), "HelpIcon.svg 必须可加载");
    }

    /// 未知路径必须回退到 GPUI Kit 默认资产（而不是 panic 或凭空返回字节）。
    #[test]
    fn unknown_path_does_not_panic() {
        // 默认资产里没有这个路径：`RustEmbed` 返回错误，`AppAssets` 原样上抛，
        // 调用方（GPUI 的 svg 加载器）按缺失处理。
        assert!(
            !matches!(
                AppAssets.load("icons/definitely-not-an-icon.svg"),
                Ok(Some(_))
            ),
            "未知路径不能凭空返回字节"
        );
        assert!(AppAssets.load("").expect("空路径不报错").is_none());
    }

    /// `list` 必须同时包含两套资源（否则 GPUI 的资产枚举会漏）。
    #[test]
    fn lists_hand_drawn_icons() {
        let names = AppAssets.list("icons/").expect("枚举资产失败");
        assert!(
            names.iter().any(|name| name == "icons/SelectionIcon.svg"),
            "list 必须包含手绘图标：{names:?}"
        );
    }
}
