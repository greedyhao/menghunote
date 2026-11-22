//! 翻译入口与进程级当前语言（ADR 0020 P1）。
//!
//! - [`t`]：按固定回退链取文案（当前语言 → 英文 → 中文 → key 本身）；
//! - [`t_with`]：在 [`t`] 的基础上做 `{name}` 插值；
//! - [`t_count`]：两条 key 的简单单复数（`count == 1` → `one`，其它 → `other`）；
//! - [`set_mode`] / [`current_locale`] / [`mode`]：进程级语言状态。
//!
//! ## 为什么当前语言是全局的
//!
//! 和主题一样，语言是**整个窗口**的偏好：侧栏、菜单、设置页、状态栏必须同时
//! 切换。绘制函数直接调用 [`t`]（不把 locale 一路塞进每个视图的构造参数），
//! 切换时由 app 层 `refresh_windows()` 触发重绘。全局状态只有几个 `Copy` 字段，
//! 读路径是读锁 + 一次查表，不跨渲染/IO 持锁。
//!
//! ## `System` 的解析时机
//!
//! [`set_mode`] / [`set_locale`] 在**调用时**解析系统语言（`LC_ALL`/`LANG`）。
//! 本切片不订阅 OS 语言切换（GPUI 没有跨平台的语言变化回调）；用户改完系统
//! 语言重启应用或重新选择「跟随系统」即可，见 ADR 0020 §9「已知限制」。

use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::catalog;
use crate::locale::{Locale, LocaleMode};

/// 进程级语言状态。
struct LocaleState {
    /// 用户选择的模式（`System` / 固定中文 / 固定英文）。
    mode: LocaleMode,
    /// 解析后的输出语言。
    locale: Locale,
}

/// 默认：模式 `System`、输出中文。
///
/// **启动时没有调用 app 的 `init_from_disk()` 就保持中文**（测试/嵌入场景
/// 需要确定性）；生产入口 `main.rs` 总会先调一次 `init_from_disk()`，由它按
/// 系统语言解析 `System`。
static STATE: RwLock<LocaleState> = RwLock::new(LocaleState {
    mode: LocaleMode::System,
    locale: Locale::ZhCn,
});

fn read_state() -> RwLockReadGuard<'static, LocaleState> {
    STATE.read().unwrap_or_else(|error| error.into_inner())
}

fn write_state() -> RwLockWriteGuard<'static, LocaleState> {
    STATE.write().unwrap_or_else(|error| error.into_inner())
}

/// 当前输出语言。
#[must_use]
pub fn current_locale() -> Locale {
    read_state().locale
}

/// 当前用户选择的模式。
#[must_use]
pub fn mode() -> LocaleMode {
    read_state().mode
}

/// 设置语言模式并立即生效（解析 `System` + 更新当前语言）。
///
/// 返回 `true` 表示模式或输出语言真的变了（调用方据此决定是否持久化/重绘）。
/// 持久化由 app 层负责：本 crate 不碰磁盘。
pub fn set_mode(mode: LocaleMode) -> bool {
    let locale = mode.resolve(Locale::detect());
    let mut state = write_state();
    let changed = state.mode != mode || state.locale != locale;
    state.mode = mode;
    state.locale = locale;
    changed
}

/// 切到**固定**语言（`Locale::ZhCn` / `Locale::En`）。
///
/// 这是 [`set_mode`] 的便捷包装：会把模式一起改成对应的固定模式，避免出现
/// "模式还是 System、输出语言却是固定值"的中间态。测试/嵌入方用它来获得
/// 确定性。
pub fn set_locale(locale: Locale) -> bool {
    set_mode(LocaleMode::from_locale(locale))
}

/// 回退链（当前语言 → 英文 → 中文）。
///
/// 抽成接受 `lookup` 的函数，测试可以注入不对称 catalog 验证回退顺序；
/// 生产路径传入 [`catalog::lookup`]。
fn select_text(
    current: Locale,
    id: &str,
    lookup: impl Fn(Locale, &str) -> Option<&'static str>,
) -> Option<&'static str> {
    lookup(current, id)
        .or_else(|| lookup(Locale::En, id))
        .or_else(|| lookup(Locale::ZhCn, id))
}

/// 取翻译；缺 key 时回退英文 → 中文 → **key 本身**（不 panic、不打印）。
#[must_use]
pub fn t(id: &str) -> String {
    select_text(current_locale(), id, catalog::lookup)
        .unwrap_or(id)
        .to_owned()
}

/// 取翻译并做 `{name}` 插值。
///
/// - 占位符按 `args` 顺序逐字替换；同名多次出现全部替换；
/// - 未知占位符原样保留（不报错，便于诊断模板拼写错误）；
/// - 缺 key 时先回退（同 [`t`]），再对取到的模板插值。
///
/// 业务代码只允许用**完整模板**组合文案，禁止先拼接中文/英文片段再判断语言。
#[must_use]
pub fn t_with(id: &str, args: &[(&str, &str)]) -> String {
    // 缺 key 时**原样返回 key**，不再对 key 文本做插值：回退语义是"把 key
    // 显示出来便于发现漏翻"，不是"拿 key 当模板"。
    match select_text(current_locale(), id, catalog::lookup) {
        Some(template) => interpolate(template, args),
        None => id.to_owned(),
    }
}

/// 简单单复数：`count == 1` 用 `id_one`，其它（含 0）用 `id_other`。
///
/// 只支持这一条英文规则；中文 catalog 两条 key 文案相同（单复数同形）。
/// `zero`/`few`/`many` 等 CLDR 分类后置到消息格式选型（ADR 0020 §8/§9）。
#[must_use]
pub fn t_count(id_one: &str, id_other: &str, count: u64) -> String {
    let id = if count == 1 { id_one } else { id_other };
    let count = count.to_string();
    t_with(id, &[("count", &count)])
}

/// 纯插值：把模板里的 `{name}` 替换成对应值。
///
/// 未知占位符/多余大括号原样保留；这是**纯函数**，catalog 测试也直接用它。
#[must_use]
pub fn interpolate(template: &str, args: &[(&str, &str)]) -> String {
    let mut text = template.to_owned();
    for (name, value) in args {
        let placeholder = format!("{{{name}}}");
        if text.contains(&placeholder) {
            text = text.replace(&placeholder, value);
        }
    }
    text
}

// ---------------------------------------------------------------------------
// 测试钩子
// ---------------------------------------------------------------------------

/// 进程级语言测试锁：语言是全局状态，改它的测试必须串行。
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// 语言测试守卫：持有期间串行；[`LocaleTestGuard::reset`] 把状态固定到给定
/// 语言；`Drop` 时复位到默认（`System` + 中文）。
pub struct LocaleTestGuard {
    _lock: MutexGuard<'static, ()>,
}

impl LocaleTestGuard {
    /// 把全局语言固定到 `locale`（模式同步改成对应的固定模式，结果确定）。
    pub fn reset(&self, locale: Locale) {
        let mut state = write_state();
        state.mode = LocaleMode::from_locale(locale);
        state.locale = locale;
    }
}

impl Drop for LocaleTestGuard {
    fn drop(&mut self) {
        let mut state = write_state();
        state.mode = LocaleMode::System;
        state.locale = Locale::ZhCn;
    }
}

/// 串行化语言测试并自动复位（crate 内测试与 `crates/app/tests/**` 共用）。
#[doc(hidden)]
#[must_use]
pub fn lock_for_test() -> LocaleTestGuard {
    LocaleTestGuard {
        _lock: TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::msg;

    #[test]
    fn fallback_chain_is_current_then_english_then_chinese() {
        // 模拟：en 有 a/b，zh 只有 b。
        let fake = |locale: Locale, id: &str| -> Option<&'static str> {
            match (locale, id) {
                (Locale::En, "a") => Some("A-en"),
                (Locale::En, "b") => Some("B-en"),
                (Locale::ZhCn, "b") => Some("B-zh"),
                _ => None,
            }
        };

        assert_eq!(select_text(Locale::En, "a", fake), Some("A-en"));
        assert_eq!(
            select_text(Locale::En, "b", fake),
            Some("B-en"),
            "当前语言命中时优先当前语言"
        );
        assert_eq!(
            select_text(Locale::ZhCn, "a", fake),
            Some("A-en"),
            "中文缺 key 时回退英文"
        );
        assert_eq!(
            select_text(Locale::En, "b", fake),
            Some("B-en"),
            "英文命中时不再看中文"
        );
        assert_eq!(select_text(Locale::En, "missing", fake), None);
    }

    #[test]
    fn missing_key_returns_the_key_itself() {
        let _guard = lock_for_test();
        _guard.reset(Locale::En);
        assert_eq!(t("sidebar.not-a-real-key"), "sidebar.not-a-real-key");
        assert_eq!(
            t_with("missing.{x}", &[("x", "1")]),
            "missing.{x}",
            "缺 key 也必须先回退到 key 本身，不插值出意外文案"
        );
    }

    #[test]
    fn interpolation_replaces_repeated_and_leaves_unknown_placeholders() {
        assert_eq!(
            interpolate("{a}-{b}-{a}", &[("a", "1"), ("b", "2")]),
            "1-2-1"
        );
        assert_eq!(
            interpolate("x {unknown} y", &[("known", "1")]),
            "x {unknown} y"
        );
        assert_eq!(interpolate("no placeholders", &[]), "no placeholders");
        assert_eq!(interpolate("{raw}", &[("raw", "{nested}")]), "{nested}");
    }

    #[test]
    fn translations_cover_both_languages_and_templates() {
        let _guard = lock_for_test();

        _guard.reset(Locale::ZhCn);
        assert_eq!(t(msg::sidebar::NOTES), "笔记");
        assert_eq!(
            t_with(
                msg::settings::INSTANCE_ROW,
                &[("role", "实例"), ("id", "abc")]
            ),
            "实例 · 本机实例 abc"
        );
        assert_eq!(
            t_with(msg::settings::VAULT_PATH, &[("path", "/tmp/a.db")]),
            "库文件：/tmp/a.db"
        );

        _guard.reset(Locale::En);
        assert_eq!(t(msg::sidebar::NOTES), "Notes");
        assert_eq!(t(msg::editor_menu::COPY_MARKDOWN), "Copy Markdown");
        assert_eq!(
            t_with(
                msg::settings::INSTANCE_ROW,
                &[("role", "Active"), ("id", "abc")]
            ),
            "Active · local instance abc"
        );
        assert_eq!(
            t_with(msg::settings::VAULT_PATH, &[("path", "/tmp/a.db")]),
            "Vault file: /tmp/a.db"
        );
    }

    #[test]
    fn count_plural_uses_one_key_for_exactly_one() {
        let _guard = lock_for_test();

        _guard.reset(Locale::En);
        assert_eq!(
            t_count(
                msg::sidebar::BACKLINKS_ONE,
                msg::sidebar::BACKLINKS_OTHER,
                1
            ),
            "1 backlink"
        );
        assert_eq!(
            t_count(
                msg::sidebar::BACKLINKS_ONE,
                msg::sidebar::BACKLINKS_OTHER,
                0
            ),
            "0 backlinks"
        );
        assert_eq!(
            t_count(
                msg::sidebar::BACKLINKS_ONE,
                msg::sidebar::BACKLINKS_OTHER,
                2
            ),
            "2 backlinks"
        );

        _guard.reset(Locale::ZhCn);
        for count in [0_u64, 1, 2] {
            assert_eq!(
                t_count(
                    msg::status::BACKLINKS_ONE,
                    msg::status::BACKLINKS_OTHER,
                    count
                ),
                format!("反向链接 {count}"),
                "中文单复数同形"
            );
        }
    }

    #[test]
    fn set_mode_updates_state_and_reports_changes() {
        let _guard = lock_for_test();
        _guard.reset(Locale::ZhCn);
        assert_eq!(mode(), LocaleMode::ZhCn);
        assert_eq!(current_locale(), Locale::ZhCn);

        assert!(set_mode(LocaleMode::En), "切到英文必须报告变化");
        assert_eq!(mode(), LocaleMode::En);
        assert_eq!(current_locale(), Locale::En);

        assert!(!set_mode(LocaleMode::En), "重复设置同一模式不算变化");

        assert!(set_locale(Locale::ZhCn));
        assert_eq!(mode(), LocaleMode::ZhCn);
        assert!(!set_locale(Locale::ZhCn));

        // System 的解析结果依赖真实环境变量，这里只断言"模式确实变成 System"。
        set_mode(LocaleMode::System);
        assert_eq!(mode(), LocaleMode::System);
    }
}
