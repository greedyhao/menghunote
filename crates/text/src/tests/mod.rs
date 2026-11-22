//! `menghunote-text` 的坐标 / 单位测试：单位夹具 + 固定种子属性测试。
//!
//! 全部测试都是纯 `&str` 计算，**不依赖 GPUI / 窗口 / 文件系统**，因此可在无窗口
//! 的 CI 环境运行。依据 ADR 0002 §11.1 第 1、2 条；按 ADR 0003 §6 第 5 条，
//! 这些通用 Unicode 测试随实现一起从 `crates/editor` 迁入本 crate（语义未改）。

mod fixtures;
mod property;
mod range;
