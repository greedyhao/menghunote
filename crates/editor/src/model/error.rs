//! 编辑器纯模型的错误类型。
//!
//! 原则（ADR 0003 §5.3）：镜像**只按确认 delta 更新一次**。任何无法安全对齐的
//! 状态（基准过期、命令被拒、live_rev 跳跃、回声对不上乐观编辑）都不做"猜测式
//! 修补"，而是显式返回错误并让控制器进入 [`EditorError::ResyncRequired`]，
//! 等调用方按新的 `OpenSnapshot` 重建。

use std::error::Error;
use std::fmt;

use menghunote_storage::session::{CommandId, LiveRev, RejectReason};
use menghunote_text::CoordError;

use crate::model::markdown::MarkdownError;
use crate::model::projection::ProjectionError;

/// 编辑器纯模型的全部错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorError {
    /// 坐标 / 单位换算失败（越界、非 scalar 边界、代理对内部等）。
    Coord(CoordError),
    /// 投影映射失败。
    Projection(ProjectionError),
    /// Markdown 语义扫描失败（正常文本不会触发；防御性分支）。
    Markdown(MarkdownError),
    /// source 标量序号越界（控制器内部校验）。
    ScalarOutOfBounds { index: usize, len: usize },
    /// 还有一条本地命令在等确认（S1 单飞行；切片 4 再接批量与补偿）。
    CommandInFlight(CommandId),
    /// 正在 IME 组合（preedit）时不允许生成正文编辑。
    PreeditActive,
    /// 没有进行中的组合，却收到 `commit_preedit`。
    NoPreedit,
    /// 编辑集合为空。
    EmptyEditSet,
    /// 编辑集合未按 `start` 升序排序。
    EditsNotSorted {
        previous_start: usize,
        next_start: usize,
    },
    /// 编辑区间重叠（协议要求排序且互不重叠）。
    OverlappingEdits {
        previous_end: usize,
        next_start: usize,
    },
    /// 单条编辑区间起止倒序。
    ReversedEdit { start: usize, end: usize },
    /// `local_echo = true` 的确认没有对应的本地乐观编辑：镜像可能缺文本。
    LocalEchoWithoutPending(CommandId),
    /// 本 crate 已乐观写入，但协议声明"未本地写入"：两种权威冲突，不能猜。
    MirrorDiverged(CommandId),
    /// 协议确认 / 远端 delta 与当前 `live_rev` 不连续，镜像无法安全前进。
    RevGap { expected: LiveRev, actual: LiveRev },
    /// 本地命令尚未确认时收到远端 live delta：S1 不做并发 rebase，需重建。
    ConcurrentDeltaWithPending(CommandId),
    /// 文档 ID 不符。
    WrongDoc { expected: String, actual: String },
    /// 会话基准过期：请求未被应用，控制器已标记需要重建。
    StaleBase {
        command_id: CommandId,
        current_live_rev: LiveRev,
    },
    /// 请求被拒（含背压）：控制器已标记需要重建。
    Rejected {
        command_id: CommandId,
        reason: RejectReason,
    },
    /// 背压：请求**未被受理**（Loro / journal 均未改），可用同一个 `EditRequest`
    /// 与同一个 `command_id` 重试；本地乐观输入与 pending 命令都保留。
    Backpressured(CommandId),
    /// 序号换算溢出（防御性分支，正常文档不会触发）。
    ArithmeticOverflow,
    /// 控制器需要按新的 `OpenSnapshot` 重建后才能继续编辑。
    ResyncRequired,
}

impl EditorError {
    /// 该错误是否意味着"镜像已不可信，必须按新快照重建"。
    #[must_use]
    pub fn is_resync_required(&self) -> bool {
        matches!(
            self,
            Self::LocalEchoWithoutPending(_)
                | Self::MirrorDiverged(_)
                | Self::RevGap { .. }
                | Self::ConcurrentDeltaWithPending(_)
                | Self::StaleBase { .. }
                | Self::Rejected { .. }
                | Self::ResyncRequired
        )
    }
}

impl fmt::Display for EditorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Coord(err) => write!(f, "坐标换算失败：{err}"),
            Self::Projection(err) => write!(f, "投影映射失败：{err}"),
            Self::Markdown(err) => write!(f, "Markdown 语义扫描失败：{err}"),
            Self::ScalarOutOfBounds { index, len } => {
                write!(f, "标量序号 {index} 越界（source 共 {len} 个 scalar）")
            }
            Self::CommandInFlight(command_id) => {
                write!(f, "命令 {command_id} 尚未确认，S1 不允许并发提交")
            }
            Self::PreeditActive => write!(f, "IME 组合期间不允许生成正文编辑"),
            Self::NoPreedit => write!(f, "当前没有进行中的 IME 组合"),
            Self::EmptyEditSet => write!(f, "编辑集合为空"),
            Self::EditsNotSorted {
                previous_start,
                next_start,
            } => write!(
                f,
                "编辑集合未按 start 升序：前一条 start={previous_start}，后一条 start={next_start}"
            ),
            Self::OverlappingEdits {
                previous_end,
                next_start,
            } => write!(
                f,
                "编辑区间重叠：前一条 end={previous_end}，后一条 start={next_start}"
            ),
            Self::ReversedEdit { start, end } => {
                write!(f, "编辑区间倒序：start={start} 晚于 end={end}")
            }
            Self::LocalEchoWithoutPending(command_id) => write!(
                f,
                "命令 {command_id} 声明 local_echo=true，但本控制器没有对应的乐观编辑"
            ),
            Self::MirrorDiverged(command_id) => write!(
                f,
                "命令 {command_id} 声明 local_echo=false，但本控制器已乐观写入，无法判定权威状态"
            ),
            Self::RevGap { expected, actual } => write!(
                f,
                "live_rev 不连续：期望从 {} 前进，但收到 {}",
                expected.0, actual.0
            ),
            Self::ConcurrentDeltaWithPending(command_id) => write!(
                f,
                "命令 {command_id} 未确认时收到远端 delta，S1 不做并发 rebase，需重建"
            ),
            Self::WrongDoc { expected, actual } => {
                write!(f, "文档不符：期望 {expected}，收到 {actual}")
            }
            Self::StaleBase {
                command_id,
                current_live_rev,
            } => write!(
                f,
                "命令 {command_id} 基准过期（当前 live_rev={}），请求未应用",
                current_live_rev.0
            ),
            Self::Rejected { command_id, reason } => {
                write!(f, "命令 {command_id} 被拒：{reason:?}")
            }
            Self::Backpressured(command_id) => write!(
                f,
                "命令 {command_id} 因背压暂未受理（Loro 未改），可用同一命令重试"
            ),
            Self::ArithmeticOverflow => write!(f, "序号换算溢出"),
            Self::ResyncRequired => write!(f, "控制器已失效，需要按新的 OpenSnapshot 重建"),
        }
    }
}

impl Error for EditorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Coord(err) => Some(err),
            Self::Projection(err) => Some(err),
            Self::Markdown(err) => Some(err),
            _ => None,
        }
    }
}

impl From<CoordError> for EditorError {
    fn from(error: CoordError) -> Self {
        Self::Coord(error)
    }
}

impl From<ProjectionError> for EditorError {
    fn from(error: ProjectionError) -> Self {
        Self::Projection(error)
    }
}

impl From<MarkdownError> for EditorError {
    fn from(error: MarkdownError) -> Self {
        Self::Markdown(error)
    }
}
