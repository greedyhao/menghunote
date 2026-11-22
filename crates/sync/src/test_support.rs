//! 仅测试用的共享辅助：确定性随机源、内存槽存储、本地身份封装初始化。

use crate::error::SyncError;
use crate::random::RandomSource;
use std::sync::OnceLock;

/// 让本测试进程具备可用的本地身份封装（仅 Linux 需要主密钥文件）。
///
/// - 在测试进程首次调用时设置**独立、vault 之外**的 `XDG_DATA_HOME`
///   （`CARGO_TARGET_TMPDIR/xdg-<pid>`），然后调用生产入口
///   [`crate::identity_wrap::initialize_user_key`]；
/// - 之后既有的 `identity.protect/unprotect` 调用完全不变，**没有**测试旁路；
/// - `OnceLock` 保证多测试线程并发调用只初始化一次；Windows 上 `initialize_user_key`
///   是 no-op（DPAPI 不需要文件）。
pub fn ensure_local_wrap() {
    static READY: OnceLock<()> = OnceLock::new();
    READY.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            // lib 单测目标不一定有编译期 CARGO_TARGET_TMPDIR：运行时回退到仓库 target/tmp。
            let root = std::env::var_os("CARGO_TARGET_TMPDIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp")
                });
            let dir = root.join(format!("xdg-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME");
            std::env::set_var("XDG_DATA_HOME", &dir);
        }
        crate::identity_wrap::initialize_user_key().expect("初始化测试用户主密钥");
    });
}

/// 确定性随机源（SplitMix64）。
///
/// **只用于测试**：让"随机"可复现，断言能稳定命中。不要在生产路径使用。
pub struct TestRng {
    state: u64,
}

impl TestRng {
    /// 用种子构造（不同种子产生不同流）。
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1),
        }
    }
}

impl RandomSource for TestRng {
    fn fill(&mut self, dst: &mut [u8]) -> Result<(), SyncError> {
        for chunk in dst.chunks_mut(8) {
            self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            let bytes = z.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        Ok(())
    }
}
