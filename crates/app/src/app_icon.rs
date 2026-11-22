//! 应用图标（窗口/任务栏）。
//!
//! 源文件是用户提供的 `assets/app-icon.svg`（512×512）；运行时用
//! `assets/app-icon-256.png` 解码成 GPUI `WindowOptions.icon` 需要的
//! `Arc<image::RgbaImage>`。不引入 SVG 运行时渲染依赖。
//!
//! `assets/app-icon.ico` 目前只作为**打包资产**保留（16/24/32/48/64/128/256），
//! 本轮不改 Windows 资源文件；exe 文件图标等安装包阶段再接。

use std::sync::Arc;

use image::{ImageFormat, RgbaImage};

/// 窗口图标的 PNG 字节（256×256，RGBA）。
pub const WINDOW_ICON_PNG: &[u8] = include_bytes!("../assets/app-icon-256.png");

/// 解码窗口图标；解码失败返回 `None`，不阻塞窗口创建。
#[must_use]
pub fn window_icon() -> Option<Arc<RgbaImage>> {
    let image = image::load_from_memory_with_format(WINDOW_ICON_PNG, ImageFormat::Png).ok()?;
    Some(Arc::new(image.to_rgba8()))
}

#[cfg(test)]
mod tests {
    use super::{window_icon, WINDOW_ICON_PNG};

    #[test]
    fn window_icon_decodes_to_256_square_rgba() {
        let icon = window_icon().expect("窗口图标必须可解码");
        assert_eq!(icon.width(), 256);
        assert_eq!(icon.height(), 256);
        assert_eq!(icon.as_raw().len(), 256 * 256 * 4);
        // 至少有可见像素；全透明说明 SVG 栅格化/导出坏了。
        assert!(
            icon.pixels().any(|pixel| pixel.0[3] > 0),
            "窗口图标不能全透明"
        );
    }

    #[test]
    fn embedded_png_is_not_empty() {
        assert!(WINDOW_ICON_PNG.len() > 1024, "图标 PNG 明显过小");
        assert_eq!(&WINDOW_ICON_PNG[..8], b"\x89PNG\r\n\x1a\n");
    }
}
