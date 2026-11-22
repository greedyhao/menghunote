//! 应用内 i18n 基础设施（ADR 0020 P1）。
//!
//! 本 crate 是**纯 Rust**：不依赖 GPUI、storage、sync，也没有外部依赖。
//! 它只负责四件事：
//!
//! 1. [`Locale`]（中文 / 英文）与系统语言解析（`LC_ALL` / `LANG`）；
//! 2. 用户偏好 [`LocaleMode`]（固定中文 / 固定英文 / 跟随系统）与解析；
//! 3. 静态 key-value catalog（[`catalog`]）与消息 id 常量（[`msg`]）；
//! 4. 翻译入口 [`t`] / [`t_with`] / [`t_count`]，以及进程级当前语言状态。
//!
//! ## 回退规则（固定、可测试）
//!
//! 查找顺序：**当前语言 → 英文 → 中文 → key 本身**。
//! 也就是说缺中文 key 时先拿英文，缺英文 key 时退回中文；两边都没有时
//! [`t`] 原样返回 key（便于测试/诊断发现漏翻），**不** panic、不写日志。
//!
//! ## 插值与复数
//!
//! - [`t_with`] 只做 `{name}` 逐字替换；未知占位符原样保留（不报错）。
//! - [`t_count`] 用**两条 key**（`*.one` / `*.other`）表达单复数：`count == 1`
//!   用 `one`，其它（含 0）用 `other`。本切片只支持这一条英文规则；中文两条
//!   key 的文案相同（单复数同形）。`zero`/`few`/`many` 等 CLDR 分类后置。
//! - 业务代码**禁止**把中文/英文片段拼接起来再判断语言；需要组合时用带
//!   占位符的模板 + [`t_with`]（例如 `settings.instance_row` 的 `{role}`）。
//!
//! ## 持久化边界
//!
//! 本 crate **不读写磁盘**：`language.json` 的加载/原子写/损坏回退在 app 层
//! （`crates/app/src/language_settings.rs`）完成，因为"平台配置目录"是 app 的
//! 职责。这里只保存内存中的当前语言。
//!
//! ## 测试钩子
//!
//! 当前语言是进程级状态，改它的测试必须串行：[`lock_for_test`] 返回的守卫
//! 同时提供 [`LocaleTestGuard::reset`]；守卫 `Drop` 时自动复位到默认
//! （`System` + 中文）。只给 `crates/app/tests/**` 与 crate 内测试使用。
//!
//! ## 未做（明确后置）
//!
//! 日期/数字/相对时间格式化、RTL、字体与 CJK 换行、复数 `zero/few/many`、
//! 插件命名空间 `plugin.<plugin_id>.*` 的运行时装载——均见 ADR 0020 §9。

pub mod catalog;
pub mod locale;
pub mod msg;
pub mod translate;

pub use catalog::{catalog_keys, has_key, lookup, missing_translations, CatalogStats};
pub use locale::{Locale, LocaleMode};
pub use translate::{
    current_locale, interpolate, lock_for_test, mode, set_locale, set_mode, t, t_count, t_with,
    LocaleTestGuard,
};
