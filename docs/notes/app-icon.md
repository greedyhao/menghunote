# 应用图标（窗口 / 任务栏）

> 2026-11-19。用户提供 `猛虎下山笔记-512.svg`（512×512，圆角底、猛虎 + 书本 +
> 太阳），作为应用图标。

## 资产

| 文件 | 用途 |
| --- | --- |
| `crates/app/assets/app-icon.svg` | 设计母版（512×512，保留原始 SVG） |
| `crates/app/assets/app-icon-256.png` | 运行时窗口/任务栏图标（RGBA PNG） |
| `crates/app/assets/app-icon-512.png` | 大图/未来安装包 |
| `crates/app/assets/app-icon.ico` | Windows 图标资源：16/24/32/48/64/128/256（PNG 帧） |

RFC：不把 SVG 运行时渲染依赖带进 app。图标是固定资产，构建前已用
`gpui-excalidraw` 的 `render::export::rasterize_svg` 栅格化；运行时只解码 PNG。

## 运行时接入

- `crates/app/src/app_icon.rs`：`window_icon() -> Option<Arc<image::RgbaImage>>`，
  解码 `app-icon-256.png`；失败返回 `None`，不阻塞窗口创建。
- `main.rs` 的 `window_options()` 填 `WindowOptions.icon`；三条启动路径共用。
- 平台：Windows 标题栏/任务栏；Linux 由窗口管理器决定是否显示。

## exe 文件图标

`crates/app/build.rs` 在 **Windows 目标**上调用 `embed-resource`，把
`crates/app/app-icon.rc`（引用 `assets/app-icon.ico`）编译进可执行文件。
**资源 ID 必须是数字 `1`**：GPUI 的 Windows 后端用
`LoadImageW(module, PCWSTR(1 as _), IMAGE_ICON, ...)` 取 exe 图标，`IDI_ICON1`
这类字符串名不会被找到。

- MSVC：自动查找 Windows SDK 的 `rc.exe`；
- GNU 工具链：回退 `windres`；
- Linux/macOS：build.rs 直接跳过，不需要资源编译器。

因此**外部 Windows 构建会自动嵌入**，前提是本机有 Windows SDK（或 MinGW 的 `windres`）。
找不到资源编译器时构建会**明确失败**，不会静默生成无图标 exe。

## 未做

- macOS `.icns`、Linux 桌面文件图标未做。

## 验证

`cargo test -p menghunote-app` 中 `app_icon` 单测 2 个：PNG 解码为 256×256 RGBA、
至少一个非透明像素；PNG 头/大小。应用默认测试 **682**。
