//! 平台相关的小工具：目前只有「读回进程 DPI 感知级别」。
//!
//! ## 为什么会有它（task-12 的结论）
//!
//! 切片 5a 收尾时怀疑过：exe 没有 DPI 清单 → Windows 做 DPI 虚拟化 → 150% 缩放下窗口
//! 被整体位图放大、状态栏掉到屏幕外。**核实后的结论是这个怀疑不成立**：
//!
//! - `gpui` 的 `windows-manifest` 特性（默认开启）经 `embed-resource` 产出带
//!   `RT_MANIFEST` 资源的 `gpui.lib`，而 cargo 会把**依赖** build script 的
//!   `rustc-link-arg-bins` 传播到最终 bin —— 所以 `menghunote-app.exe` 本来就带着
//!   同一份 PerMonitorV2 清单（实测可执行文件里有 `PerMonitorV2` / `dpiAwareness` 字符串；
//!   链接命令行里能看到 gpui 的 `out/gpui.lib`）；
//! - 运行时读回：`DPI_AWARENESS_PER_MONITOR_AWARE(2)`、`system_dpi = 144`；
//!   GPUI `window.scale_factor() = 1.5`，显示器 `2560×1440` 是**逻辑**像素
//!   （物理 3840×2160），请求 1024×768 逻辑 = 1536×1152 物理，整窗都在屏内；
//! - 当时"状态栏出屏"是**测量错误**：截图用的 PowerShell 进程自身没有 DPI 感知，
//!   `PrimaryScreen.Bounds` 报的是虚拟化后的 2560×1440，于是只截到了 4K 屏的左上角。
//!
//! 也不能在 app 里**再**嵌一份清单：RT_MANIFEST 只能有一份，两份会让链接直接失败
//! （`CVTRES: fatal error CVT1100: duplicate resource. type:MANIFEST`）。因此这里
//! 不加 `build.rs`，只保留读回校验：
//!
//! 1. 证明清单确实生效（`scale_factor` 只是下游表现，感知级别才是根）；
//! 2. 一旦 gpui 关掉 `windows-manifest`、或将来 link-arg 传播规则变化，启动日志会立刻
//!    给出明确警告，而不是又退化成"窗口莫名其妙变大"这种只能靠肉眼发现的问题。
//!
//! ## 分层
//!
//! - [`DpiAwareness`] / [`DpiReport`]：纯数据 + 纯函数（映射、判定、日志文案），可单测；
//! - `imp`：Win32 调用，只有 `cfg(windows)` 才编译，业务层看不到平台分支（AGENTS.md §4）。

/// Windows `DPI_AWARENESS`（`GetAwarenessFromDpiAwarenessContext` 的返回值）的语义化封装。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpiAwareness {
    /// `DPI_AWARENESS_UNAWARE (0)`：窗口被当位图整体缩放（最坏情况，必须告警）。
    Unaware,
    /// `DPI_AWARENESS_SYSTEM_AWARE (1)`：只认主显示器 DPI，跨屏拖动会再次被位图缩放。
    System,
    /// `DPI_AWARENESS_PER_MONITOR_AWARE (2)`：`PerMonitorV2` 清单生效后落在这里。
    PerMonitor,
    /// 查询失败或系统返回了未知取值（原值保留，便于排查）。
    Unknown(i32),
}

impl DpiAwareness {
    /// 从 `GetAwarenessFromDpiAwarenessContext` 的原始值映射。
    #[must_use]
    pub fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::Unaware,
            1 => Self::System,
            2 => Self::PerMonitor,
            other => Self::Unknown(other),
        }
    }

    /// 是否达到了本切片要求的级别（每显示器 DPI 感知）。
    #[must_use]
    pub fn is_per_monitor(self) -> bool {
        matches!(self, Self::PerMonitor)
    }

    /// 日志用的名字（带 Windows 常量名与原始值，便于对照文档）。
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::Unaware => "DPI_AWARENESS_UNAWARE(0)".to_owned(),
            Self::System => "DPI_AWARENESS_SYSTEM_AWARE(1)".to_owned(),
            Self::PerMonitor => "DPI_AWARENESS_PER_MONITOR_AWARE(2)".to_owned(),
            Self::Unknown(value) => format!("未知({value})"),
        }
    }
}

/// 一次读回的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DpiReport {
    /// 当前线程的 DPI 感知级别（清单生效时进程/线程一致）。
    pub awareness: DpiAwareness,
    /// `GetDpiForSystem()`：系统 DPI（96 = 100%，144 = 150%）。
    pub system_dpi: u32,
}

impl DpiReport {
    /// 感知级别是否达标。
    #[must_use]
    pub fn is_per_monitor(self) -> bool {
        self.awareness.is_per_monitor()
    }

    /// 一行摘要（启动日志用）。
    #[must_use]
    pub fn summary(self) -> String {
        format!(
            "awareness={} · system_dpi={}（{}%）",
            self.awareness.describe(),
            self.system_dpi,
            self.system_dpi * 100 / 96
        )
    }

    /// 不达标时的警告文案；达标返回 `None`。
    #[must_use]
    pub fn warning(self) -> Option<&'static str> {
        match self.awareness {
            DpiAwareness::PerMonitor => None,
            DpiAwareness::Unaware => Some(
                "进程 DPI 未感知：窗口会被 Windows 当位图整体缩放、文字发虚。\
                 检查 gpui 的 windows-manifest 特性是否还在（清单经 gpui.lib 的 \
                 rustc-link-arg-bins 传播到本 exe）",
            ),
            DpiAwareness::System => Some(
                "进程只有系统级 DPI 感知：跨显示器拖动时窗口会被位图缩放，\
                 需要 PerMonitorV2 清单（gpui 自带那份是 PerMonitorV2）",
            ),
            DpiAwareness::Unknown(_) => {
                Some("DPI 感知级别未知：GetAwarenessFromDpiAwarenessContext 返回了未预期取值")
            }
        }
    }
}

/// 读回当前线程的 DPI 感知级别；Windows 之外的平台返回 `None`。
#[must_use]
pub fn read() -> Option<DpiReport> {
    imp::read()
}

/// 启动时打印 DPI 感知读回结果；不达标时额外打一条明确警告（**不 panic**）。
pub fn log_startup_dpi_report() {
    match read() {
        Some(report) => {
            println!("[menghunote] DPI 感知读回：{}", report.summary());
            if let Some(warning) = report.warning() {
                eprintln!("[menghunote] 警告：{warning}");
            }
        }
        None => {
            eprintln!("[menghunote] 提示：本平台不读 DPI 感知级别（只有 Windows 需要清单）");
        }
    }
}

#[cfg(windows)]
#[allow(
    unsafe_code,
    reason = "只为读回 DPI 感知级别调用三个无副作用的 Win32 查询"
)]
mod imp {
    use windows_sys::Win32::UI::HiDpi::{
        GetAwarenessFromDpiAwarenessContext, GetDpiForSystem, GetThreadDpiAwarenessContext,
    };

    use super::{DpiAwareness, DpiReport};

    pub fn read() -> Option<DpiReport> {
        // SAFETY: 三个函数都是只读查询——不接受指针参数、不修改全局状态、
        // 任意线程可调用；不满足前提时最坏结果是返回 0/None，不会破坏内存。
        let context = unsafe { GetThreadDpiAwarenessContext() };
        if context.is_null() {
            return None;
        }
        let awareness = unsafe { GetAwarenessFromDpiAwarenessContext(context) };
        let system_dpi = unsafe { GetDpiForSystem() };
        Some(DpiReport {
            awareness: DpiAwareness::from_raw(awareness),
            system_dpi,
        })
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn read() -> Option<super::DpiReport> {
        None
    }
}

// ---------------------------------------------------------------------------
// 首帧就绪标记（task-36）：给 UI 自动化一个明确的"可以开始点击"信号
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

/// 进程启动时刻（`main` 第一行设置）：所有 `t=+…ms` 日志都以它为基准。
static APP_START: OnceLock<Instant> = OnceLock::new();
/// 首帧日志是否已经打过（只打一次）。
static FIRST_FRAME_LOGGED: AtomicBool = AtomicBool::new(false);

/// 记录进程启动时刻（`main` 里尽早调用，重复调用无副作用）。
pub fn mark_app_start() {
    APP_START.get_or_init(Instant::now);
}

/// 进程启动到现在经过的时间；`mark_app_start` 没调用过时返回 `None`。
#[must_use]
pub fn since_app_start() -> Option<std::time::Duration> {
    APP_START.get().map(|start| start.elapsed())
}

/// **第一帧真正画完**时调用一次：打一行就绪标记，供 UI 自动化轮询。
///
/// 之所以放在 paint 里而不是 render 里：render 只是"算出元素树"，paint 才是真正
/// 往窗口提交绘制；自动化要等的是"窗口已经有内容可交互"。
///
/// 重复调用无副作用（只有第一次会打印）。
pub fn log_first_frame_ready() {
    if FIRST_FRAME_LOGGED.swap(true, Ordering::SeqCst) {
        return;
    }
    match since_app_start() {
        Some(elapsed) => eprintln!(
            "[menghunote] 首帧就绪 t=+{:.0}ms",
            elapsed.as_secs_f64() * 1000.0
        ),
        None => eprintln!("[menghunote] 首帧就绪"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_windows_awareness_constants() {
        assert_eq!(DpiAwareness::from_raw(0), DpiAwareness::Unaware);
        assert_eq!(DpiAwareness::from_raw(1), DpiAwareness::System);
        assert_eq!(DpiAwareness::from_raw(2), DpiAwareness::PerMonitor);
        // 保留原值：新枚举值要能出现在日志里
        assert_eq!(DpiAwareness::from_raw(-1), DpiAwareness::Unknown(-1));
        assert_eq!(DpiAwareness::from_raw(7), DpiAwareness::Unknown(7));
    }

    #[test]
    fn only_per_monitor_counts_as_fixed() {
        assert!(DpiAwareness::PerMonitor.is_per_monitor());
        assert!(!DpiAwareness::System.is_per_monitor());
        assert!(!DpiAwareness::Unaware.is_per_monitor());
        assert!(!DpiAwareness::Unknown(3).is_per_monitor());
    }

    #[test]
    fn every_non_per_monitor_level_produces_a_warning() {
        let fixed = DpiReport {
            awareness: DpiAwareness::PerMonitor,
            system_dpi: 144,
        };
        assert!(fixed.is_per_monitor());
        assert!(fixed.warning().is_none());
        assert!(fixed.summary().contains("system_dpi=144"));
        assert!(fixed.summary().contains("150%"), "摘要要能看出缩放比例");

        for awareness in [
            DpiAwareness::Unaware,
            DpiAwareness::System,
            DpiAwareness::Unknown(9),
        ] {
            let report = DpiReport {
                awareness,
                system_dpi: 96,
            };
            assert!(!report.is_per_monitor(), "{awareness:?} 不能被当作已修复");
            assert!(report.warning().is_some(), "{awareness:?} 必须给警告");
        }
    }

    #[test]
    fn summary_keeps_the_raw_value_for_unknown_levels() {
        let report = DpiReport {
            awareness: DpiAwareness::Unknown(42),
            system_dpi: 120,
        };
        let summary = report.summary();
        assert!(summary.contains("42"), "未知取值要原样显示：{summary}");
        assert!(summary.contains("system_dpi=120"), "{summary}");
    }

    #[test]
    fn summary_is_percent_of_96_dpi() {
        for (dpi, percent) in [(96, "100%"), (120, "125%"), (144, "150%"), (192, "200%")] {
            let report = DpiReport {
                awareness: DpiAwareness::PerMonitor,
                system_dpi: dpi,
            };
            assert!(
                report.summary().contains(percent),
                "system_dpi={dpi} 应显示 {percent}：{}",
                report.summary()
            );
        }
    }

    /// 真机读回，并**断言清单确实生效**。
    ///
    /// Windows 的测试 exe 与应用 exe 一样都会链接 gpui，因此都会拿到 gpui 经
    /// `rustc-link-arg-bins` 传播的 PerMonitorV2 清单（实测测试产物里也含 `PerMonitorV2`）。
    /// 这条断言就是"DPI 感知"的机器可验证回归保护：一旦清单丢了（例如有人关掉 gpui 的
    /// `windows-manifest`），它会直接变红，而不是等到肉眼发现"窗口变大了"。
    #[cfg(windows)]
    #[test]
    fn windows_binaries_are_per_monitor_dpi_aware() {
        let report = read().expect("Windows 上必须能读回 DPI 感知级别");
        assert!(report.system_dpi >= 96, "system_dpi 不合理：{report:?}");
        assert!(
            report.is_per_monitor(),
            "Windows 二进制必须是 PerMonitorV2 感知，实际读到 {}",
            report.awareness.describe()
        );
    }
}
