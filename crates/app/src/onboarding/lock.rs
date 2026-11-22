//! 每个实例目录的**进程级独占锁**（ADR 0011「每目录持有进程级 OS 独占锁」）。
//!
//! ## 为什么需要
//!
//! 同一个实例目录被两个进程同时打开（GUI + CLI，或两份 GUI）会共享同一个 SQLite 库与
//! `dev-identity.bin`；ADR 0009 的整套"单写者"假设都会被打破。向导/CLI 共用本锁，
//! 覆盖 worker 的真实生命周期（从开始 Create 到窗口关闭）。
//!
//! ## MVP 实现（明确局限，见 ADR 0011 的"锁"段）
//!
//! 现有依赖里没有跨平台文件锁 crate（`fs2`/`fd-lock` 都不在依赖图里，ADR 要求优先零
//! 新增依赖），因此 MVP 用**同目录锁文件 + `create_new`**：
//!
//! - 获取 = 在实例目录里 `create_new` 一个 `.dev-sync-instance.lock`，内容写
//!   `pid=` / `created=`（Unix 秒）；创建成功即视为持锁；
//! - 释放 = **我们自己**持锁的进程退出/Drop 时删除该文件；
//! - 冲突 = 文件已存在 → 明确报错 + 提示"确认没有进程在跑时可手工删除锁文件"。
//!
//! 局限（必须诚实）：进程被硬杀时锁文件会残留（**不会**自动偷锁），需要人工清理；
//! 锁文件被第三方删除/改名后两个进程可能同时进入。它挡的是"正常人两次启动"，不是
//! 恶意/异常并发。产品化时应换成真正的 OS 文件锁（`fd-lock`/`fs2`），这把锁的 API
//! 形状（[`InstanceLock::acquire`] + `Drop`）不需要变。
//!
//! ## 纪律
//!
//! - **绝不删除实例目录**，也**绝不删除不是本次进程创建的锁文件**（Drop 前比对内容）；
//! - 错误文案只带路径/pid/时间戳（公开信息）。

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

/// 实例目录里的锁文件名。
pub const LOCK_FILE_NAME: &str = ".dev-sync-instance.lock";

/// 锁冲突/IO 错误。
#[derive(Debug)]
pub enum LockError {
    /// 目录已被另一个进程占用。
    Conflict {
        /// 锁文件路径。
        path: PathBuf,
        /// 锁文件里的持有者信息（pid/时间戳；可能为空）。
        holder: String,
    },
    /// 目录不可创建/不可写。
    Io {
        /// 出错路径。
        path: PathBuf,
        /// 底层错误。
        source: std::io::Error,
    },
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { path, holder } => write!(
                f,
                "实例目录已被另一个进程占用：{}（持有者 {holder}）；\
                 若确认没有进程在使用它，请手工删除该锁文件后重试——\
                 本程序不会自动删除别人的锁，也不会删除实例目录",
                path.display()
            ),
            Self::Io { path, source } => {
                write!(f, "实例目录不可用：{}（{source}）", path.display())
            }
        }
    }
}

impl std::error::Error for LockError {}

/// 已持有的实例目录锁（Drop 时释放）。
///
/// 不实现 `Clone`：一份锁只能有一个所有者，复制会让"谁负责释放"变得暧昧。
pub struct InstanceLock {
    /// 锁文件路径。
    path: PathBuf,
    /// 本进程写入的内容（Drop 前比对，避免删掉别人的锁）。
    content: String,
    /// 打开的文件句柄：存活期间让锁文件至少有一个打开的写句柄（诊断/延长语义用）。
    ///
    /// `Option` 只为 Drop 顺序：先在 `Drop` 里关掉句柄，再删锁文件（Windows 上更稳）。
    file: Option<File>,
}

impl std::fmt::Debug for InstanceLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只打印路径：内容里没有秘密，但也没必要进日志。
        f.debug_struct("InstanceLock")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl InstanceLock {
    /// 获取实例目录锁（目录不存在时先创建）。
    pub fn acquire(instance_dir: &Path) -> Result<Self, LockError> {
        fs::create_dir_all(instance_dir).map_err(|source| LockError::Io {
            path: instance_dir.to_path_buf(),
            source,
        })?;
        let path = instance_dir.join(LOCK_FILE_NAME);
        let content = format!(
            "pid={}\ncreated={}\n",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs())
        );
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // 锁文件只有当前用户可写；它不含秘密，但也不该被别人改。
            options.mode(0o600);
        }
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let holder = fs::read_to_string(&path)
                    .ok()
                    .map(|text| text.trim().replace('\n', " "))
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| "（内容不可读）".to_owned());
                return Err(LockError::Conflict { path, holder });
            }
            Err(source) => return Err(LockError::Io { path, source }),
        };
        if let Err(source) = file
            .write_all(content.as_bytes())
            .and_then(|()| file.sync_all())
        {
            // 写失败：锁文件是本次刚 create_new 出来的，清掉再报错（不算删别人的锁）。
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(LockError::Io { path, source });
        }
        Ok(Self {
            path,
            content,
            file: Some(file),
        })
    }

    /// 锁文件路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // 先关句柄再删文件（Windows 上避免共享冲突；锁语义由 create_new 决定，不靠句柄）。
        drop(self.file.take());
        // 只删"内容仍是本次写入"的锁文件：人工介入过（内容变了）就原样保留并提示。
        match fs::read_to_string(&self.path) {
            Ok(current) if current == self.content => {
                if let Err(error) = fs::remove_file(&self.path) {
                    eprintln!(
                        "[menghunote] 释放实例锁失败：{}（{error}；可手工删除）",
                        self.path.display()
                    );
                }
            }
            Ok(_) => eprintln!(
                "[menghunote] 实例锁内容已被外部修改，保留不删：{}",
                self.path.display()
            ),
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(prefix: &str) -> PathBuf {
        // lib 单测没有 `CARGO_TARGET_TMPDIR`（只有集成测试有），统一落仓库 target/tmp。
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!(
                "onboarding-lock-{prefix}-{}-{}",
                std::process::id(),
                super::super::layout::generate_instance_id()
            ));
        std::fs::create_dir_all(&dir).expect("创建测试目录失败");
        dir
    }

    #[test]
    fn second_acquisition_conflicts_and_release_removes_the_file() {
        let dir = temp_dir("conflict");
        let first = InstanceLock::acquire(&dir).expect("第一次获取锁失败");
        assert!(first.path().exists(), "锁文件必须落盘");
        let error = InstanceLock::acquire(&dir).expect_err("第二次必须冲突");
        let message = error.to_string();
        assert!(message.contains("已被另一个进程占用"), "{message}");
        assert!(
            message.contains("手工删除"),
            "必须给出人工清理提示：{message}"
        );
        drop(first);
        assert!(!dir.join(LOCK_FILE_NAME).exists(), "释放后锁文件应被删除");
        // 释放之后可以重新获取（模拟重启）。
        let again = InstanceLock::acquire(&dir).expect("释放后应能重新获取");
        drop(again);
    }

    #[test]
    fn foreign_lock_content_is_never_deleted() {
        let dir = temp_dir("foreign");
        let lock_path = dir.join(LOCK_FILE_NAME);
        std::fs::write(&lock_path, "pid=999\ncreated=1\n").expect("写外来锁失败");
        let error = InstanceLock::acquire(&dir).expect_err("已存在锁必须冲突");
        assert!(matches!(error, LockError::Conflict { .. }), "{error}");
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("外来锁必须原样保留"),
            "pid=999\ncreated=1\n"
        );
    }
}
