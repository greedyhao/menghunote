//! menghunote 桌面端入口（ADR 0003 §7 切片 5a：第一次把编辑器接进 GPUI 窗口）。
//!
//! 启动顺序（每步失败都带上下文，不 `unwrap`）：
//!
//! 0. 读回 DPI 感知级别（`app.manifest` 里的 PerMonitorV2 是否生效的唯一客观证据）；
//! 1. 打开 dev vault（`target/tmp/menghunote-dev/vault.sqlite3`，可用
//!    `MENGHUNOTE_DEV_VAULT` 覆盖），需要时播种演示笔记；
//! 2. 开窗，根视图是 `AppShell`（切片 7b：左侧结构树 + 右侧编辑器）；
//!    `AppShell` 自己按 `list_tree` 的结果打开第一篇笔记（`SessionDriver` → `EditorController`），
//!    关窗时做一次**有界**排空。

use anyhow::Result;
use gpui::{
    px, size, App, AppContext as _, Bounds, SharedString, TitlebarOptions, WindowBounds,
    WindowOptions,
};
use menghunote_app::dev_vault;
use menghunote_app::platform;
use menghunote_app::shell::AppShell;

fn main() -> Result<()> {
    // 第一行就记启动时刻：后面所有 `t=+…ms`（含「首帧就绪」）都以它为基准。
    platform::mark_app_start();
    // 先读 DPI 感知：清单没生效时这里会打一条明确警告（不 panic），
    // 后面 `scale_factor` / 显示器尺寸的数值就是它的下游表现。
    platform::log_startup_dpi_report();

    let dev = dev_vault::open_dev_vault()?;
    println!(
        "[menghunote] dev vault = {}（{}）",
        dev.path.display(),
        if dev.seeded {
            "本次已播种演示笔记"
        } else {
            "复用已有内容"
        }
    );
    // 状态栏只放"文件所在目录 + 文件名"，完整路径在启动日志里；否则长路径会把提示挤掉。
    let vault_label = short_vault_label(&dev.path);
    // 切片 7b：根视图（侧栏 + 编辑器）与侧栏共用同一个 vault 句柄；
    // 具体打开哪篇笔记由结构树决定，不再硬编码文档 ID。
    let vault = dev.vault.clone();

    gpui::platform::application().run(move |cx: &mut App| {
        let window_bounds = initial_window_bounds(cx);
        if let Some(display) = cx.primary_display() {
            // 修复前后对比用：DPI 未感知时这里是**物理像素**且窗口 scale=1.0；
            // 清单生效后这里是**逻辑像素**（物理 / scale）。
            println!(
                "[menghunote] 显示器 bounds = {:?}，visible = {:?}，初始窗口 = {:?}",
                display.bounds(),
                display.visible_bounds(),
                window_bounds
            );
        }
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(window_bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from("menghunote · 切片 7b dev vault")),
                ..Default::default()
            }),
            ..Default::default()
        };

        let opened = cx.open_window(options, move |window, cx| {
            // 期望 1.5（150% 缩放，清单生效）；若仍是 1.0，说明 exe 没有清单。
            println!("[menghunote] 窗口 scale_factor = {}", window.scale_factor());
            let shell = cx.new(|cx| AppShell::new(vault.clone(), vault_label.clone(), cx));
            // 关窗只在这里排空一次，且时长有界（1500ms）；切换文档另有自己的排空。
            let weak = shell.downgrade();
            window.on_window_should_close(cx, move |_window, cx| {
                weak.update(cx, |shell, cx| shell.close_all(cx)).ok();
                true
            });
            shell
        });

        if let Err(error) = opened {
            eprintln!("[menghunote] 无法创建主窗口：{error}");
            return;
        }
        cx.activate(true);
    });
    Ok(())
}

/// 状态栏用的短标签：`<父目录>/<文件名>`（不打印完整绝对路径）。
fn short_vault_label(path: &std::path::Path) -> String {
    let file = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.parent()
        .and_then(std::path::Path::file_name)
        .map_or(file.clone(), |parent| {
            format!("{}/{}", parent.to_string_lossy(), file)
        })
}

/// 初始窗口：居中，但不超过所在显示器可见区域的 82%。
///
/// ## 与 DPI 的关系（task-12 核实过）
///
/// 本机是 4K（3840×2160 物理）主屏 + 150% 缩放：进程是 PerMonitorV2 感知（清单由 gpui
/// 经 `rustc-link-arg-bins` 传播到本 exe，启动日志有读回值），GPUI 报的
/// `visible_bounds` 是**逻辑**像素 2560×1392、`window.scale_factor() = 1.5`。
/// 请求 1024×768 逻辑 = 1536×1152 物理，整窗（含状态栏）都在屏内，所以本机上这条
/// 82% 夹取不会触发。
///
/// 它仍然保留，原因与 DPI 无关：**任何** DPI 下都可能遇到小屏（例如 1280×800 逻辑的
/// 笔记本屏），窗口不能一开就超出可用区域。因此这条逻辑**不需要跟着 DPI 调整**。
fn initial_window_bounds(cx: &App) -> Bounds<gpui::Pixels> {
    let preferred = size(px(1024.0), px(768.0));
    let Some(display) = cx.primary_display() else {
        return Bounds::centered(None, preferred, cx);
    };
    let visible = display.visible_bounds();
    let limit = 0.82;
    let width = preferred.width.min(px(visible.size.width.as_f32() * limit));
    let height = preferred
        .height
        .min(px(visible.size.height.as_f32() * limit));
    Bounds::centered_at(visible.center(), size(width, height))
}
