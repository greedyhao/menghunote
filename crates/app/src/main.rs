#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

//! menghunote 桌面端入口（ADR 0003 §7 切片 5a；ADR 0011 起带同步向导入口）。
//!
//! 启动有三条互不混淆的路径（顺序 = 优先级）：
//!
//! 1. **显式 dev-sync CLI**（仅 `dev-sync` 构建：`--dev-sync*` / `MENGHUNOTE_DEV_SYNC=1`，
//!    含 Create/Join/Info/Publish）：沿用 ADR 0009 的启动生命周期，开窗后根视图是
//!    `AppShell`——自动化/测试兼容，行为不变。
//! 2. **产品同步 UI**（`sync-ui`，**默认 feature**：`cargo build/run -p menghunote-app`）：
//!    **不需要**任何 env/flag——先开窗，根视图是 [`AppRoot`]（Welcome/Create 表单/
//!    备份页/错误页/主界面），已有有效公开配置 + 身份时启动直接进 Main。产品构建只
//!    允许 `remote_tls`；`local_dev` 仍由 `dev-sync` 单独控制。
//! 3. **无同步 UI**（`--no-default-features`，或只开 `sync`）：打开 dev vault
//!    （`target/tmp/menghunote-dev/`，可用 `MENGHUNOTE_DEV_VAULT` 覆盖），根视图
//!    `AppShell`，行为与 P1 之前的默认构建完全一致（保留给纯开发/演示路径）。
//!
//! 第 2 条是 ADR 0011 的产品入口：真实用户不再需要 `MENGHUNOTE_DEV_VAULT`、
//! `--dev-sync` 或命令行就能创建同步实例。CLI/env 只保留给自动化。

use anyhow::Result;
use gpui::{
    px, size, App, AppContext as _, Bounds, SharedString, TitlebarOptions, WindowBounds,
    WindowOptions,
};
#[cfg(any(not(feature = "sync-ui"), feature = "dev-sync"))]
use menghunote_app::dev_vault;
// 应用统一资产源（画布手绘图标 + GPUI Kit 默认资产）：三条启动路径都要注册，
// 否则 Excalidraw 工具栏/菜单渲染成空白字形。
use menghunote_app::assets::AppAssets;
use menghunote_app::platform;
#[cfg(any(not(feature = "sync-ui"), feature = "dev-sync"))]
use menghunote_app::shell::AppShell;

fn main() -> Result<()> {
    // 第一行就记启动时刻：后面所有 `t=+…ms`（含「首帧就绪」）都以它为基准。
    platform::mark_app_start();
    // 应用级主题（ADR 0021 P2）：读本机偏好（只含 mode/theme_id）。文件缺失/损坏
    // 一律回退 `System`（无窗口时解析为浅色），**不阻塞启动**；根视图第一次渲染后
    // 由 `observe_window_appearance` 按真实 OS 外观校正。
    menghunote_app::theme::init_from_disk();
    // 应用语言（ADR 0020 P1）：读本机 `language.json`（只含 locale 选择）。缺失/损坏
    // 回退「跟随系统」（`LC_ALL`/`LANG`，未知语言回退中文）；不阻塞启动、不同步。
    menghunote_app::language_settings::init_from_disk();
    // 快捷键（设置页「快捷键」分类）：读本机 `shortcuts.json`（数据根下）。缺失/损坏/
    // 未知命令/非法组合回退默认值，**不阻塞启动**；改绑后立即生效。
    menghunote_app::shortcuts::init_from_disk();
    // 先读 DPI 感知：清单没生效时这里会打一条明确警告（不 panic），
    // 后面 `scale_factor` / 显示器尺寸的数值就是它的下游表现。
    platform::log_startup_dpi_report();

    // ---- 启动路径：编译期门禁决定入口（P1：sync-ui 是产品路径，dev-sync 是 dev 路径）----
    //
    // 1. `dev-sync`：保留旧的运行时门禁（ADR 0009 / ADR 0011）。`gate::resolve` 的语义
    //    分两支：
    //    - **请求启用**（显式 `--dev-sync*` 或 `MENGHUNOTE_DEV_SYNC=1`）→ 旧 CLI 生命周期，
    //      配置不合法（缺 vault / relay 非 loopback）仍然**明确报错退出**，不静默回退；
    //    - **没有请求启用** → 不是错误：进入 GUI 向导（`AppRoot`），由公开配置（而不是
    //      环境变量）决定打开哪个实例。
    #[cfg(feature = "dev-sync")]
    {
        let args: Vec<String> = std::env::args().collect();
        let gate = menghunote_app::dev_sync::gate::resolve(args.iter().cloned(), |key| {
            std::env::var(key).ok()
        })
        .map_err(|error| anyhow::anyhow!("dev-sync 门禁拒绝：{error}"))?;
        match gate {
            menghunote_app::dev_sync::gate::Gate::Disabled => run_onboarding_window(),
            menghunote_app::dev_sync::gate::Gate::Enabled(config) => {
                run_dev_sync_window(config, &args)
            }
        }
    }
    // 2. 产品同步 UI（默认构建）：没有 `--dev-sync*` 入口，直接进 AppRoot 向导。
    #[cfg(all(feature = "sync-ui", not(feature = "dev-sync")))]
    {
        run_onboarding_window()
    }
    // 3. 无同步 UI（`--no-default-features` / 只开 `sync`）：保持 P1 之前的 dev vault 入口。
    #[cfg(not(feature = "sync-ui"))]
    {
        run_default_window()
    }
}

/// 无同步 UI 路径（`--no-default-features` / 只开 `sync`）：打开 dev vault → `AppShell`
/// （与 P1 之前逐字节一致，保留给纯开发/演示）。
#[cfg(not(feature = "sync-ui"))]
fn run_default_window() -> Result<()> {
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
    let vault_label = short_vault_label(&dev.path);
    let vault = dev.vault.clone();
    gpui::platform::application()
        .with_assets(AppAssets)
        .run(move |cx: &mut App| {
            gpui::init(cx);
            // 手绘字体（Excalifont / Virgil）：画布笔记的文本元素用它们；
            // 只在启动时注册一次（`add_fonts` 幂等）。
            gpui_excalidraw::register_hand_drawn_fonts(cx);
            let window_bounds = initial_window_bounds(cx);
            let options = window_options("menghunote · 切片 7b dev vault", window_bounds);
            let opened = cx.open_window(options, move |window, cx| {
                println!("[menghunote] 窗口 scale_factor = {}", window.scale_factor());
                let shell = cx.new(|cx| AppShell::new(vault.clone(), vault_label.clone(), cx));
                let weak = shell.downgrade();
                // 关窗只在这里排空一次，且时长有界（1500ms）；切换文档另有自己的排空。
                window.on_window_should_close(cx, move |_window, cx| {
                    weak.update(cx, |shell, cx| shell.close_all(cx))
                        .unwrap_or(true)
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

/// ADR 0011 GUI 入口：先开窗，根视图 `AppRoot`（不需要 env/flag）。
///
/// 目录解析失败（HOME/LOCALAPPDATA 都没有）是**错误**：不静默回退 cwd/tmp。此时还没
/// 有窗口，直接带着明确原因退出。
#[cfg(feature = "sync-ui")]
fn run_onboarding_window() -> Result<()> {
    use menghunote_app::onboarding::{AppRoot, AppRootOptions};
    let options = AppRootOptions::discover(|key| std::env::var(key).ok()).map_err(|error| {
        anyhow::anyhow!("同步向导无法确定数据目录（不静默回退临时目录）：{error}")
    })?;
    println!(
        "[menghunote] 同步向导：配置 = {}；实例目录 = {}",
        options.layout.config_path.display(),
        options.layout.instances_dir.display()
    );
    gpui::platform::application()
        .with_assets(AppAssets)
        .run(move |cx: &mut App| {
            gpui::init(cx);
            // 手绘字体（Excalifont / Virgil）：画布笔记的文本元素用它们；
            // 只在启动时注册一次（`add_fonts` 幂等）。
            gpui_excalidraw::register_hand_drawn_fonts(cx);
            let window_bounds = initial_window_bounds(cx);
            let options_for_window = options.clone();
            let window_options = window_options("menghunote · 同步", window_bounds);
            let opened = cx.open_window(window_options, move |window, cx| {
                let root = cx.new(|cx| AppRoot::new(options_for_window, cx));
                // 关窗分派：Working 拦截、Main 有界排空（见 `AppRoot::should_close`）。
                AppRoot::install_close_handler(window, root.clone(), cx);
                root
            });
            if let Err(error) = opened {
                eprintln!("[menghunote] 无法创建主窗口：{error}");
                return;
            }
            cx.activate(true);
        });
    Ok(())
}

/// 显式 dev-sync CLI 路径（ADR 0009 切片 4c-1/4c-3）：沿用既有启动生命周期。
///
/// 顺序是硬约束：
/// 1. 五种模式互斥：`--dev-sync-create`（新路径 + env setup secret）、
///    `--dev-sync-join`（env vault_id + 恢复码）、普通 `--dev-sync`（**只 load** 身份），
///    以及 `--dev-sync-info` / `--dev-sync-publish`（打印完即退出，不开窗）；
/// 2. secret/恢复码只从 env 读；启动日志只报模式/路径/是否已存在；
///    **例外**：Create 成功后把一次性恢复码块打到 stdout（切片 4c-3），
///    不写日志/文件/配置/meta；
/// 3. `prepare` 在 Create/Join 成功后已把身份**受保护落盘**；失败会清掉本次新建的
///    库/身份文件并 fail-fast；
/// 4. 窗口句柄必须用身份映射出的 peer 重建（`open_vault_at_with_peer`）。
#[cfg(feature = "dev-sync")]
fn run_dev_sync_window(
    config: menghunote_app::dev_sync::gate::DevSyncConfig,
    args: &[String],
) -> Result<()> {
    let mode = menghunote_app::dev_sync::setup::resolve_mode(args.iter().cloned())
        .map_err(|error| anyhow::anyhow!("dev-sync 启动模式非法：{error:#}"))?;
    let secrets = menghunote_app::dev_sync::setup::secrets_from_env(|key| std::env::var(key).ok());
    println!(
        "[menghunote] dev-sync 模式 = {}；实例库 = {}（启动前{}）",
        mode.label(),
        config.vault.display(),
        if config.vault.exists() {
            "已存在"
        } else {
            "不存在"
        }
    );
    // 切片 4c-3：`--dev-sync-info` / `--dev-sync-publish` 是**不开窗**的子命令。
    if let Some(cli_mode) = mode.cli_mode() {
        run_dev_sync_cli(&config, cli_mode, secrets)
            .map_err(|error| anyhow::anyhow!("dev-sync {} 失败：{error:#}", mode.label()))?;
        return Ok(());
    }
    let outcome = menghunote_app::dev_sync::setup::prepare(&config, mode, secrets)
        .map_err(|error| anyhow::anyhow!("dev-sync 启动初始化失败：{error:#}"))?;
    // 切片 4c-3：Create 是恢复码**唯一一次**可见的机会（不落盘、不写日志/配置/meta）。
    if mode == menghunote_app::dev_sync::setup::SetupMode::Create {
        if let Some(recovery_code) = outcome.recovery_code.as_deref() {
            println!(
                "{}",
                menghunote_app::dev_sync::cli::create_handoff_block(
                    &outcome.vault_id,
                    recovery_code
                )
            );
        }
    }
    let peer = outcome.identity.loro_peer_id();
    let epoch = outcome.identity.current_epoch;
    // 打印完立即丢弃：恢复码不随 outcome 继续持有，也不进任何后续输出。
    drop(outcome.recovery_code);
    // `prepare` 的库句柄只用于启动阶段；窗口句柄由生产入口重建。
    // dev-sync 路径**不得**播种演示树。
    drop(outcome.vault);
    let dev = dev_vault::open_vault_at_with_peer_opts(&config.vault, peer, false)?;
    println!(
        "[menghunote] dev vault = {}（{}）",
        dev.path.display(),
        if dev.seeded {
            "本次已播种演示笔记"
        } else {
            "复用已有内容"
        }
    );
    let vault_label = short_vault_label(&dev.path);
    let vault = dev.vault.clone();
    let sync_context = menghunote_storage::SyncContext::new(peer, epoch);
    gpui::platform::application()
        .with_assets(AppAssets)
        .run(move |cx: &mut App| {
            gpui::init(cx);
            // 手绘字体（Excalifont / Virgil）：画布笔记的文本元素用它们；
            // 只在启动时注册一次（`add_fonts` 幂等）。
            gpui_excalidraw::register_hand_drawn_fonts(cx);
            let window_bounds = initial_window_bounds(cx);
            if let Some(display) = cx.primary_display() {
                println!(
                    "[menghunote] 显示器 bounds = {:?}，visible = {:?}，初始窗口 = {:?}",
                    display.bounds(),
                    display.visible_bounds(),
                    window_bounds
                );
            }
            let options = window_options("menghunote · 切片 7b dev vault", window_bounds);
            let config_for_window = config.clone();
            let opened = cx.open_window(options, move |window, cx| {
                println!("[menghunote] 窗口 scale_factor = {}", window.scale_factor());
                let shell = cx.new(|cx| {
                    AppShell::new_with_dev_sync_with_autosync(
                        vault.clone(),
                        vault_label.clone(),
                        config_for_window.clone(),
                        sync_context,
                        cx,
                    )
                });
                let weak = shell.downgrade();
                window.on_window_should_close(cx, move |_window, cx| {
                    weak.update(cx, |shell, cx| shell.close_all(cx))
                        .unwrap_or(true)
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

/// 切片 4c-3：两个**不开窗**的 dev-sync 子命令（打印公开事实后由 `main` 直接退出）。
#[cfg(feature = "dev-sync")]
fn run_dev_sync_cli(
    config: &menghunote_app::dev_sync::gate::DevSyncConfig,
    mode: menghunote_app::dev_sync::setup::CliMode,
    secrets: menghunote_app::dev_sync::setup::SetupSecrets,
) -> Result<()> {
    use menghunote_app::dev_sync::cli;
    use menghunote_app::dev_sync::setup::CliMode;
    match mode {
        CliMode::Info => {
            let info = cli::inspect(config)?;
            println!("{}", cli::info_block(&info));
        }
        CliMode::Publish => {
            let summary = cli::publish_baseline(config, secrets.recovery_code)?;
            println!("{}", cli::publish_block(&summary));
        }
    }
    Ok(())
}

/// 统一的窗口选项（标题 + 初始 bounds）。
fn window_options(title: &'static str, bounds: Bounds<gpui::Pixels>) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(SharedString::from(title)),
            ..Default::default()
        }),
        // 应用图标（用户提供的猛虎下山 SVG 栅格化）：窗口/任务栏。
        icon: menghunote_app::app_icon::window_icon(),
        ..Default::default()
    }
}

/// 状态栏用的短标签：`<父目录>/<文件名>`（不打印完整绝对路径）。
#[cfg(any(not(feature = "sync-ui"), feature = "dev-sync"))]
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
