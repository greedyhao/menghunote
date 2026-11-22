//! 「未知自定义 scheme 记住」的**本机偏好**（不同步、不写正文）。
//!
//! # 为什么单独一个文件
//!
//! 记住的只是「这个 scheme 允许打开」这一条本机决定：它既不属于笔记正文（不能写进
//! CRDT / 同步），也不该进 vault（换设备后应重新确认）。因此落在平台配置目录下的
//! 一个纯文本文件里，格式极简（每行一个 scheme，`#` 开头为注释）。
//!
//! # 路径
//!
//! 1. `MENGHUNOTE_LINK_SCHEMES_PATH`（测试/便携用，显式覆盖）；
//! 2. Windows：`%LOCALAPPDATA%\menghunote\link-schemes.txt`；
//! 3. macOS：`~/Library/Application Support/menghunote/link-schemes.txt`；
//! 4. 其它（Linux）：`$XDG_CONFIG_HOME/menghunote/link-schemes.txt`，缺省
//!    `~/.config/menghunote/link-schemes.txt`。
//!
//! 解析不到路径或写失败时**退化为进程内记忆**：打开流程不因为偏好文件不可写而失败。
//!
//! # 安全
//!
//! 只存 scheme 名（小写、不含 `:`），**不存 URL / 路径 / 查询串**；写失败只打一条
//! 不含目标的错误。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// 进程内的偏好存储（懒加载；`Mutex` 只护内存，不跨 await）。
static STORE: OnceLock<Mutex<SchemeStore>> = OnceLock::new();

/// 测试/便携用的路径覆盖（`None` = 按平台默认路径）。
static PATH_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Debug, Default)]
struct SchemeStore {
    /// 已记住的 scheme（小写、无 `:`；有序便于稳定写文件）。
    schemes: BTreeSet<String>,
    /// 是否已尝试从磁盘加载（失败也算加载过，避免每次点击都读盘）。
    loaded: bool,
}

impl SchemeStore {
    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(path) = settings_path() else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            // 文件不存在是正常情况（第一次记住之前没有文件）。
            return;
        };
        for line in text.lines() {
            let scheme = line.trim();
            if scheme.is_empty() || scheme.starts_with('#') {
                continue;
            }
            self.schemes.insert(scheme.to_ascii_lowercase());
        }
    }

    fn save(&self) {
        let Some(path) = settings_path() else {
            return;
        };
        let mut text =
            String::from("# menghunote：允许打开的自定义 scheme（每行一个；本机偏好，不同步）\n");
        for scheme in &self.schemes {
            text.push_str(scheme);
            text.push('\n');
        }
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                eprintln!("[menghunote] 创建链接偏好目录失败：{error}");
                return;
            }
        }
        // 先写临时文件再改名：写一半崩溃不会留下半行 scheme。
        let temp = path.with_extension("txt.tmp");
        if let Err(error) = std::fs::write(&temp, text.as_bytes()) {
            eprintln!("[menghunote] 写入链接偏好失败：{error}");
            return;
        }
        if std::fs::rename(&temp, &path).is_err() {
            // Windows 上目标存在时 rename 可能失败：退化为直接覆盖写。
            if let Err(error) = std::fs::write(&path, text.as_bytes()) {
                eprintln!("[menghunote] 写入链接偏好失败：{error}");
            }
            let _ = std::fs::remove_file(&temp);
        }
    }
}

/// 平台配置目录下的偏好文件路径；解析不到（没有 HOME/LOCALAPPDATA）时返回 `None`。
#[must_use]
fn settings_path() -> Option<PathBuf> {
    if let Some(path) = PATH_OVERRIDE.lock().ok().and_then(|guard| guard.clone()) {
        return Some(path);
    }
    if let Some(path) = std::env::var_os("MENGHUNOTE_LINK_SCHEMES_PATH") {
        return Some(PathBuf::from(path));
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("LOCALAPPDATA").map(|base| {
            PathBuf::from(base)
                .join("menghunote")
                .join("link-schemes.txt")
        })
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library/Application Support/menghunote")
                .join("link-schemes.txt")
        })
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("menghunote").join("link-schemes.txt"))
    }
}

fn store() -> &'static Mutex<SchemeStore> {
    STORE.get_or_init(|| Mutex::new(SchemeStore::default()))
}

fn with_store<T>(f: impl FnOnce(&mut SchemeStore) -> T) -> T {
    // 这里不做跨 await 的持锁；即使上一次持有者 panic 也不该让链接永久打不开。
    let mut guard = store().lock().unwrap_or_else(|error| error.into_inner());
    f(&mut guard)
}

/// 这个自定义 scheme 是否已被本机记住（允许直接打开）。
#[must_use]
pub fn is_remembered(scheme: &str) -> bool {
    let scheme = scheme.to_ascii_lowercase();
    with_store(|store| {
        store.ensure_loaded();
        store.schemes.contains(&scheme)
    })
}

/// 记住一个自定义 scheme（本机偏好；写盘失败只影响下次启动）。
///
/// 返回 `true` 表示这次是新记住的（之前没有）。
pub fn remember(scheme: &str) -> bool {
    let scheme = scheme.to_ascii_lowercase();
    with_store(|store| {
        store.ensure_loaded();
        let inserted = store.schemes.insert(scheme);
        if inserted {
            store.save();
        }
        inserted
    })
}

/// 测试用：清空并重新指向一个偏好文件（`None` = 回到平台默认路径）。
///
/// 只给 `crates/app/tests/**` 的链接测试用；生产代码不调用。
#[doc(hidden)]
pub fn reset_for_tests(path: Option<PathBuf>) {
    if let Ok(mut guard) = PATH_OVERRIDE.lock() {
        *guard = path;
    }
    with_store(|store| {
        store.schemes.clear();
        store.loaded = false;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个单测共享进程级偏好存储：串行执行，避免互相清空。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn temp_path(tag: &str) -> PathBuf {
        // lib 单测拿不到 `CARGO_TARGET_TMPDIR`（只有集成测试有），按 AGENTS §6
        // 统一落到仓库 `target/tmp` 下。
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("link-schemes-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建 target/tmp 失败");
        root.join("link-schemes.txt")
    }

    #[test]
    fn remember_is_persisted_and_reloaded() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let path = temp_path("persist");
        reset_for_tests(Some(path.clone()));
        assert!(!is_remembered("obsidian"));
        assert!(remember("obsidian"));
        assert!(is_remembered("OBSIDIAN"), "scheme 大小写不敏感");
        assert!(!remember("obsidian"), "重复记住返回 false");

        // 模拟重启：清内存后从文件重新加载。
        reset_for_tests(Some(path.clone()));
        assert!(is_remembered("obsidian"), "偏好必须跨重启保留");
        let raw = std::fs::read_to_string(&path).expect("读回偏好文件");
        assert!(raw.contains("obsidian"));
        assert!(!raw.contains("://"), "只存 scheme，不存 URL");
        reset_for_tests(None);
    }

    #[test]
    fn unreadable_path_degrades_to_memory() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        // 指向一个目录（写入必失败）：记住仍在本进程内生效，不 panic。
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("link-schemes-dir-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        reset_for_tests(Some(dir.clone()));
        assert!(remember("my-scheme"));
        assert!(is_remembered("my-scheme"));
        reset_for_tests(None);
    }
}
