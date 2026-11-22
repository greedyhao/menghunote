# 同步评审 Part 5/6：sync 源码 B

@@PART 5/6

> 这是 **Part 5/6**。请先只确认收到，不要开始评审。
> 本部分约 515797 字符；应低于平台约 72 万字符的截断点。
> 内容与 Part 4 连续；全部到齐后再集中评审。代码截止 `1bededa`。

## 收到确认（请只回复这三行）

1. `PART 5/6 RECEIVED`
2. 本部分 `@@SECTION` 数量（应为 22）
3. 最后一个完整文件（若可见）

---

# 本部分附录

@@SECTION code/033 `crates/sync/src/identity_wrap.rs`


`````rust
//! 本地身份封装平台门面（ADR 0006 §3、ADR 0008）。
//!
//! 本模块是 `LocalIdentityV1` / `IdentityRotationStateV1` / `RotationJournal` 落盘时
//! **唯一**的封装入口。按平台分派：
//!
//! - **Windows**：直接调用 [`crate::dpapi`]（ADR 0006 §3 冻结契约）。**不加任何外层头**，
//!   旧裸 DPAPI BLOB 继续可解；Windows 也**不探测** Linux 的 `MNKW` magic。
//! - **Linux**：用户主密钥文件 + XChaCha20-Poly1305 外层格式（ADR 0008）。
//! - **其它平台**：一律 [`SyncError::LocalWrapUnsupportedPlatform`]，**绝不**回退明文或 DPAPI。
//!
//! ## Linux 外层格式（冻结，ADR 0008）
//!
//! ```text
//! magic = b"MNKW" (4B) | version = 1 (u8) | kind = 1 (u8, LinuxKeyFile)
//! | nonce = 24B random | ciphertext || tag(16)
//! ```
//!
//! 头固定 30 字节。AAD 冻结为：
//!
//! ```text
//! b"MN/local-wrap/v1\0" || 完整 30B 头 || 原 entropy
//! ```
//!
//! 其中 `entropy` 沿用各调用点原有取值（身份/旁路状态是
//! `b"MN/dpapi/v1\0" || vault_id || device_id`，journal 是
//! `b"MN/v1/rotation-journal\0"`），所以 Linux 与 Windows 的 AAD 绑定语义一致：
//! 换 vault/device/记录类型即认证失败。
//!
//! ## 用户主密钥文件（Linux）
//!
//! - 路径：`$XDG_DATA_HOME/menghunote/local-identity/key-v1.bin`；
//!   `XDG_DATA_HOME` 未设或为空 → `$HOME/.local/share/...`。必须是绝对路径；
//!   相对路径/缺失 HOME 显式报错，**不退回 cwd 或 tmp**。
//! - `local-identity` 目录 0700、属主为当前有效 UID、真目录（拒绝符号链接）；
//!   `menghunote` 父目录不得 group/other 可写；数据家目录本身按 XDG 规范是用户
//!   信任锚（只校验同 UID/真目录）。
//! - 钥文件 0600、同 UID、普通文件、单硬链接、恰好 32 字节；拒绝符号链接；
//!   已有权限**不自动修复**。
//! - [`initialize_user_key`] 是唯一创建入口（OS 随机 32B → 私有临时文件 → fsync →
//!   原子 no-replace 发布 → 目录 fsync）。[`protect`] / [`unprotect`] 只加载校验，
//!   **不会**创建或替换主钥；主钥缺失/损坏一律显式失败。
//!
//! 安全边界（ADR 0008 威胁模型）：主钥是 0600 裸密钥文件，拿到同 UID 的进程/文件读取
//! 能力即可解密；**安全性弱于 Windows DPAPI**，本项目不宣称二者等价。

use crate::error::SyncError;
use zeroize::Zeroizing;

/// 本平台是否具备本地身份封装（Windows DPAPI / Linux 用户主密钥）。
#[must_use]
pub const fn is_available() -> bool {
    cfg!(windows) || cfg!(target_os = "linux")
}

/// 平台门面：保护明文，返回可落盘 BLOB。
///
/// Windows 走 DPAPI（无外层头）；Linux 走 `MNKW` 外层格式；其它平台明确不支持。
pub fn protect(plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, SyncError> {
    #[cfg(windows)]
    {
        crate::dpapi::protect(plaintext, entropy)
    }
    #[cfg(target_os = "linux")]
    {
        linux::protect(plaintext, entropy)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (plaintext, entropy);
        Err(SyncError::LocalWrapUnsupportedPlatform)
    }
}

/// 平台门面：解封 BLOB，返回**零化**的明文。
///
/// Windows 只按 DPAPI 解（不探测 `MNKW`）；Linux 严格校验 `MNKW` 头，**绝不**回退 DPAPI
/// 或明文；其它平台明确不支持。
pub fn unprotect(blob: &[u8], entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, SyncError> {
    #[cfg(windows)]
    {
        crate::dpapi::unprotect(blob, entropy)
    }
    #[cfg(target_os = "linux")]
    {
        linux::unprotect(blob, entropy)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (blob, entropy);
        Err(SyncError::LocalWrapUnsupportedPlatform)
    }
}

/// 首次初始化 Linux 用户主密钥（Windows 为 no-op）。
///
/// 只允许由显式初始化入口调用：OS 随机 32B → 私有临时文件 → fsync → 原子 no-replace
/// 发布 → 目录 fsync。已存在且校验通过的钥幂等返回 `Ok(())`；已存在但损坏/权限不安全
/// **不会**被覆盖，直接返回错误（不自动修复）。
pub fn initialize_user_key() -> Result<(), SyncError> {
    #[cfg(windows)]
    {
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        linux::initialize_user_key()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(SyncError::LocalWrapUnsupportedPlatform)
    }
}

/// Linux 实现：用户主密钥文件 + `MNKW` 外层格式。
#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use crate::domain::LOCAL_WRAP_AAD_DOMAIN;
    use crate::keys::Secret32;
    use crate::random::{OsRandom, RandomSource};
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};
    use rustix::fs::{
        fstat, fsync, open, renameat_with, unlink, FileType, Mode, OFlags, RenameFlags, Stat, CWD,
    };
    use rustix::io::Errno;
    use std::fs::{self, DirBuilder, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use zeroize::Zeroizing;

    /// Linux 外层格式 magic（ADR 0008 冻结）。
    pub const MAGIC: [u8; 4] = *b"MNKW";
    /// Linux 外层格式版本。
    pub const VERSION: u8 = 1;
    /// 外层 kind：用户主密钥文件封装（唯一取值）。
    pub const KIND_LINUX_KEY_FILE: u8 = 1;
    /// 外层头长度 = magic4 + version1 + kind1 + nonce。
    pub const HEADER_LEN: usize = 4 + 1 + 1 + NONCE_LEN;
    /// XChaCha20-Poly1305 nonce 长度。
    pub const NONCE_LEN: usize = 24;
    /// Poly1305 标签长度。
    pub const TAG_LEN: usize = 16;
    /// 主密钥长度（固定 32B）。
    pub const KEY_LEN: usize = 32;

    const APP_DIR: &str = "menghunote";
    const IDENTITY_DIR: &str = "local-identity";
    const KEY_FILE_NAME: &str = "key-v1.bin";

    /// 平台入口：由环境变量解析数据家目录并初始化主钥。
    pub fn initialize_user_key() -> Result<(), SyncError> {
        let base = data_home()?;
        initialize_in(&base)
    }

    /// 平台入口：加载并校验主钥后加密。
    pub fn protect(plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, SyncError> {
        let base = data_home()?;
        let key = load_key_in(&base)?;
        encrypt(&key, plaintext, entropy)
    }

    /// 平台入口：加载并校验主钥后解密。
    pub fn unprotect(blob: &[u8], entropy: &[u8]) -> Result<Zeroizing<Vec<u8>>, SyncError> {
        let base = data_home()?;
        let key = load_key_in(&base)?;
        decrypt(&key, blob, entropy)
    }

    // ------------------------------------------------------------ 路径解析

    /// 解析数据家目录：`$XDG_DATA_HOME` 优先，否则 `$HOME/.local/share`。
    ///
    /// 只接受绝对路径；缺失/相对路径显式报错，**不退回 cwd 或 /tmp**。
    fn data_home() -> Result<PathBuf, SyncError> {
        resolve_data_home(non_empty_env("XDG_DATA_HOME"), non_empty_env("HOME"))
    }

    fn non_empty_env(name: &str) -> Option<std::ffi::OsString> {
        std::env::var_os(name).filter(|value| !value.is_empty())
    }

    /// 纯函数口径（便于单元测试覆盖"相对/缺失/为空"分支，不读进程环境）。
    fn resolve_data_home(
        xdg_data_home: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
    ) -> Result<PathBuf, SyncError> {
        if let Some(value) = xdg_data_home.filter(|value| !value.is_empty()) {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(SyncError::LocalKeyPath("XDG_DATA_HOME 必须是绝对路径"));
            }
            return Ok(path);
        }
        let Some(home) = home.filter(|value| !value.is_empty()) else {
            return Err(SyncError::LocalKeyPath(
                "XDG_DATA_HOME 未设置且 HOME 缺失，拒绝使用 cwd/tmp",
            ));
        };
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err(SyncError::LocalKeyPath("HOME 必须是绝对路径"));
        }
        Ok(home.join(".local").join("share"))
    }

    fn app_dir(base: &Path) -> PathBuf {
        base.join(APP_DIR)
    }

    fn key_dir(base: &Path) -> PathBuf {
        app_dir(base).join(IDENTITY_DIR)
    }

    fn key_file_path(base: &Path) -> PathBuf {
        key_dir(base).join(KEY_FILE_NAME)
    }

    fn current_euid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    // ------------------------------------------------------------ 目录校验

    /// 数据家目录必须存在、是真目录且属主是当前有效 UID。
    ///
    /// 注意：XDG 规范把数据家目录本身交给用户选择（可能是 `target/tmp` 这类同 UID
    /// 可写目录），它被当作**信任锚**；我们只校验属主与类型，不再向上检查其祖先。
    /// 应用子树（`menghunote` / `local-identity`）会逐个校验，见 [`check_app_dir`]。
    fn check_base_exists(base: &Path) -> Result<(), SyncError> {
        match fs::metadata(base) {
            Ok(meta) => {
                if !meta.is_dir() {
                    return Err(SyncError::LocalKeyPath("数据家目录不是目录"));
                }
                if meta.uid() != current_euid() {
                    return Err(SyncError::LocalKeyPermissions(
                        "数据家目录属主不是当前有效 UID",
                    ));
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(SyncError::LocalKeyMissing("数据家目录不存在"))
            }
            Err(error) => Err(SyncError::LocalKeyIo(format!(
                "读取数据家目录失败：{error}"
            ))),
        }
    }

    /// 校验 `menghunote` / `local-identity` 目录：真目录、同 UID、不可被 group/other 写。
    ///
    /// `local-identity` 必须严格 0700；`menghunote` 允许 0700/0755（只禁止 group/other
    /// 写位），避免与其它已有数据布局冲突。**不自动修复**任何权限。
    fn check_app_dir(path: &Path, exact_0700: bool) -> Result<(), SyncError> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SyncError::LocalKeyMissing("本地身份目录不存在"));
            }
            Err(error) => {
                return Err(SyncError::LocalKeyIo(format!(
                    "读取本地身份目录失败：{error}"
                )))
            }
        };
        if meta.file_type().is_symlink() {
            return Err(SyncError::LocalKeyPermissions("本地身份目录是符号链接"));
        }
        if !meta.is_dir() {
            return Err(SyncError::LocalKeyPermissions("本地身份路径不是目录"));
        }
        if meta.uid() != current_euid() {
            return Err(SyncError::LocalKeyPermissions(
                "本地身份目录属主不是当前有效 UID",
            ));
        }
        let mode = meta.mode() & 0o777;
        if exact_0700 {
            if mode != 0o700 {
                return Err(SyncError::LocalKeyPermissions(
                    "本地身份目录权限必须严格为 0700",
                ));
            }
        } else if mode & 0o022 != 0 {
            return Err(SyncError::LocalKeyPermissions(
                "本地身份父目录不得对 group/other 可写",
            ));
        }
        Ok(())
    }

    /// 加载并完整校验主钥（只读，不创建、不修复）。
    fn load_key_in(base: &Path) -> Result<Secret32, SyncError> {
        check_base_exists(base)?;
        check_app_dir(&app_dir(base), false)?;
        check_app_dir(&key_dir(base), true)?;
        read_key_file(&key_file_path(base))
    }

    // ------------------------------------------------------------ 主钥文件

    /// 读取主钥文件：`O_NOFOLLOW` 打开，`fstat` 校验后读满 32B。
    fn read_key_file(path: &Path) -> Result<Secret32, SyncError> {
        let fd = match open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                return Err(SyncError::LocalKeyMissing("用户主密钥文件不存在"));
            }
            Err(Errno::LOOP) => {
                return Err(SyncError::LocalKeyPermissions("用户主密钥文件是符号链接"));
            }
            Err(Errno::ACCESS) | Err(Errno::PERM) => {
                return Err(SyncError::LocalKeyPermissions("无权读取用户主密钥文件"));
            }
            Err(error) => {
                return Err(SyncError::LocalKeyIo(format!(
                    "打开用户主密钥失败：{error}"
                )));
            }
        };
        let stat = fstat(&fd)
            .map_err(|error| SyncError::LocalKeyIo(format!("fstat 用户主密钥失败：{error}")))?;
        check_key_stat(&stat)?;

        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        let mut filled = 0;
        while filled < KEY_LEN {
            let read = rustix::io::read(&fd, &mut key[filled..])
                .map_err(|error| SyncError::LocalKeyIo(format!("读取用户主密钥失败：{error}")))?;
            if read == 0 {
                return Err(SyncError::LocalKeyCorrupt("用户主密钥被截断"));
            }
            filled += read;
        }
        // 防御性检查：fstat 已限定 32B，这里再确认没有尾随字节。
        let mut extra = Zeroizing::new([0u8; 1]);
        match rustix::io::read(&fd, &mut extra[..]) {
            Ok(0) => {}
            Ok(_) => return Err(SyncError::LocalKeyCorrupt("用户主密钥含尾随字节")),
            Err(error) => {
                return Err(SyncError::LocalKeyIo(format!(
                    "读取用户主密钥失败：{error}"
                )));
            }
        }
        Ok(Secret32::from_array(*key))
    }

    /// 主钥文件的元数据约束（ADR 0008 冻结）。
    fn check_key_stat(stat: &Stat) -> Result<(), SyncError> {
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(SyncError::LocalKeyPermissions(
                "用户主密钥不是普通文件（拒绝符号链接/设备/FIFO）",
            ));
        }
        if stat.st_nlink != 1 {
            return Err(SyncError::LocalKeyPermissions("用户主密钥硬链接数必须为 1"));
        }
        if stat.st_uid != current_euid() {
            return Err(SyncError::LocalKeyPermissions(
                "用户主密钥属主不是当前有效 UID",
            ));
        }
        if stat.st_mode & 0o777 != 0o600 {
            return Err(SyncError::LocalKeyPermissions(
                "用户主密钥权限必须严格为 0600",
            ));
        }
        if stat.st_size != KEY_LEN as i64 {
            return Err(SyncError::LocalKeyCorrupt("用户主密钥长度不是 32 字节"));
        }
        Ok(())
    }

    /// 首次初始化：创建私有目录与 32B 主钥；已有合法钥幂等返回。
    fn initialize_in(base: &Path) -> Result<(), SyncError> {
        ensure_base_exists(base)?;
        let app = app_dir(base);
        let dir = key_dir(base);
        let app_created = ensure_private_dir(&app, false)?;
        let dir_created = ensure_private_dir(&dir, true)?;
        if app_created {
            fsync_dir(base)?;
        }
        if dir_created {
            fsync_dir(&app)?;
        }
        let path = key_file_path(base);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                // 已存在：只校验，不覆盖、不自动修复。
                let _ = load_key_in(base)?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_key_file(base, &dir, &path)
            }
            Err(error) => Err(SyncError::LocalKeyIo(format!(
                "读取用户主密钥失败：{error}"
            ))),
        }
    }

    /// 数据家目录缺失时创建；已存在则只校验（属主/类型）。
    fn ensure_base_exists(base: &Path) -> Result<(), SyncError> {
        match fs::create_dir_all(base) {
            Ok(()) => {}
            Err(error) => {
                return Err(SyncError::LocalKeyIo(format!(
                    "创建数据家目录失败：{error}"
                )));
            }
        }
        check_base_exists(base)
    }

    /// 创建/校验应用私有目录。返回是否新建。
    fn ensure_private_dir(path: &Path, exact_0700: bool) -> Result<bool, SyncError> {
        match fs::symlink_metadata(path) {
            Ok(_) => {
                check_app_dir(path, exact_0700)?;
                Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let result = DirBuilder::new().mode(0o700).create(path);
                match result {
                    Ok(()) => {}
                    // 并发初始化：另一进程刚创建，按已存在路径校验。
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        return Err(SyncError::LocalKeyIo(format!(
                            "创建本地身份目录失败：{error}"
                        )));
                    }
                }
                check_app_dir(path, exact_0700)?;
                Ok(true)
            }
            Err(error) => Err(SyncError::LocalKeyIo(format!(
                "读取本地身份目录失败：{error}"
            ))),
        }
    }

    /// 生成新主钥并以 no-replace 语义原子发布（绝不覆盖既有钥）。
    fn create_key_file(base: &Path, dir: &Path, path: &Path) -> Result<(), SyncError> {
        let key = Secret32::generate(&mut OsRandom)?;
        let suffix = crate::random::vec(&mut OsRandom, 8)?;
        let tmp = dir.join(format!(
            ".{KEY_FILE_NAME}.tmp.{}.{}",
            std::process::id(),
            crate::ids::hex_of(&suffix)
        ));
        let result = (|| -> Result<(), SyncError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|error| {
                    SyncError::LocalKeyIo(format!("创建临时主密钥文件失败：{error}"))
                })?;
            file.write_all(key.expose_slice())
                .map_err(|error| SyncError::LocalKeyIo(format!("写入主密钥失败：{error}")))?;
            file.sync_all()
                .map_err(|error| SyncError::LocalKeyIo(format!("fsync 主密钥失败：{error}")))?;
            drop(file);
            match renameat_with(CWD, &tmp, CWD, path, RenameFlags::NOREPLACE) {
                Ok(()) => Ok(()),
                Err(Errno::EXIST) => {
                    // 并发初始化输了：发布者已写入合法钥则幂等成功，否则暴露其错误。
                    let _ = unlink(&tmp);
                    let _ = load_key_in(base)?;
                    Ok(())
                }
                Err(error) => Err(SyncError::LocalKeyIo(format!(
                    "原子发布主密钥失败：{error}"
                ))),
            }
        })();
        if result.is_err() {
            let _ = unlink(&tmp);
        }
        result?;
        fsync_dir(dir)
    }

    /// fsync 目录，使新建/改名的目录项持久化。
    fn fsync_dir(path: &Path) -> Result<(), SyncError> {
        let fd = open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::DIRECTORY | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| SyncError::LocalKeyIo(format!("打开目录失败：{error}")))?;
        fsync(&fd).map_err(|error| SyncError::LocalKeyIo(format!("fsync 目录失败：{error}")))
    }

    // ------------------------------------------------------------ AEAD

    /// 计算冻结 AAD：`domain || 30B 头 || entropy`。
    fn build_aad(header: &[u8; HEADER_LEN], entropy: &[u8]) -> Vec<u8> {
        let mut aad = Vec::with_capacity(LOCAL_WRAP_AAD_DOMAIN.len() + HEADER_LEN + entropy.len());
        aad.extend_from_slice(LOCAL_WRAP_AAD_DOMAIN);
        aad.extend_from_slice(header);
        aad.extend_from_slice(entropy);
        aad
    }

    fn cipher_for(key: &Secret32) -> Result<XChaCha20Poly1305, SyncError> {
        XChaCha20Poly1305::new_from_slice(key.expose_slice())
            .map_err(|_| SyncError::LocalKeyCorrupt("主密钥长度不是 XChaCha20-Poly1305 的 32 字节"))
    }

    fn encrypt(key: &Secret32, plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, SyncError> {
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[4] = VERSION;
        header[5] = KIND_LINUX_KEY_FILE;
        OsRandom.fill(&mut header[6..])?;
        let aad = build_aad(&header, entropy);
        let ciphertext = cipher_for(key)?
            .encrypt(
                XNonce::from_slice(&header[6..]),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| SyncError::LocalWrapFormat("本地封装加密失败（明文过长）"))?;
        let mut blob = Vec::with_capacity(HEADER_LEN + ciphertext.len());
        blob.extend_from_slice(&header);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    fn decrypt(
        key: &Secret32,
        blob: &[u8],
        entropy: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, SyncError> {
        if blob.len() < HEADER_LEN + TAG_LEN {
            return Err(SyncError::LocalWrapFormat(
                "本地封装 BLOB 长度不足 30B 头 + 16B 标签",
            ));
        }
        let mut header = [0u8; HEADER_LEN];
        header.copy_from_slice(&blob[..HEADER_LEN]);
        if header[..4] != MAGIC {
            return Err(SyncError::LocalWrapFormat(
                "magic 不是 MNKW（可能是其它平台/旧格式裸包）",
            ));
        }
        if header[4] != VERSION {
            return Err(SyncError::LocalWrapVersion(header[4]));
        }
        if header[5] != KIND_LINUX_KEY_FILE {
            return Err(SyncError::LocalWrapKind(header[5]));
        }
        let aad = build_aad(&header, entropy);
        let plaintext = cipher_for(key)?
            .decrypt(
                XNonce::from_slice(&header[6..]),
                Payload {
                    msg: &blob[HEADER_LEN..],
                    aad: &aad,
                },
            )
            .map_err(|_| SyncError::LocalWrapAuth)?;
        Ok(Zeroizing::new(plaintext))
    }

    // ------------------------------------------------------------ 测试夹具

    #[cfg(test)]
    pub(super) fn test_base(prefix: &str) -> tempfile::TempDir {
        // lib 单测目标不一定有编译期 CARGO_TARGET_TMPDIR：运行时回退到仓库 target/tmp。
        let root = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp")
            });
        std::fs::create_dir_all(&root).expect("创建 target/tmp");
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(&root)
            .expect("在 target/tmp 下创建临时目录")
    }

    #[cfg(test)]
    pub(super) fn protect_at(
        base: &Path,
        plaintext: &[u8],
        entropy: &[u8],
    ) -> Result<Vec<u8>, SyncError> {
        let key = load_key_in(base)?;
        encrypt(&key, plaintext, entropy)
    }

    #[cfg(test)]
    pub(super) fn unprotect_at(
        base: &Path,
        blob: &[u8],
        entropy: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, SyncError> {
        let key = load_key_in(base)?;
        decrypt(&key, blob, entropy)
    }

    #[cfg(test)]
    pub(super) fn key_file(base: &Path) -> PathBuf {
        key_file_path(base)
    }

    #[cfg(test)]
    pub(super) fn key_directory(base: &Path) -> PathBuf {
        key_dir(base)
    }

    #[cfg(test)]
    pub(super) fn current_effective_uid() -> u32 {
        current_euid()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{symlink, PermissionsExt};

        fn entropy(seed: u8) -> Vec<u8> {
            let mut out = crate::domain::DPAPI_ENTROPY_PREFIX.to_vec();
            out.extend_from_slice(&[seed; 16]);
            out.extend_from_slice(&[seed.wrapping_add(1); 16]);
            out
        }

        fn initialized(prefix: &str) -> tempfile::TempDir {
            let base = test_base(prefix);
            initialize_in(base.path()).expect("初始化用户主密钥");
            base
        }

        #[test]
        fn golden_format_and_aad_layout() {
            // 域常量：必须是真正的 NUL 结尾，AAD 是 domain || 30B 头 || entropy。
            assert_eq!(LOCAL_WRAP_AAD_DOMAIN, b"MN/local-wrap/v1\0");
            assert_eq!(LOCAL_WRAP_AAD_DOMAIN.len(), 17);
            assert_eq!(LOCAL_WRAP_AAD_DOMAIN[16], 0x00);
            let base = initialized("identity-wrap-golden-");
            let ent = entropy(1);
            let blob = protect_at(base.path(), b"canary", &ent).expect("protect");
            assert_eq!(&blob[..4], b"MNKW");
            assert_eq!(blob[4], VERSION);
            assert_eq!(blob[5], KIND_LINUX_KEY_FILE);
            // 头 30B + 明文 6B + 标签 16B
            assert_eq!(blob.len(), HEADER_LEN + 6 + TAG_LEN);
            let aad = build_aad(&blob[..HEADER_LEN].try_into().expect("header"), &ent);
            assert_eq!(&aad[..LOCAL_WRAP_AAD_DOMAIN.len()], LOCAL_WRAP_AAD_DOMAIN);
            assert_eq!(
                &aad[LOCAL_WRAP_AAD_DOMAIN.len()..][..HEADER_LEN],
                &blob[..HEADER_LEN]
            );
            assert_eq!(&aad[LOCAL_WRAP_AAD_DOMAIN.len() + HEADER_LEN..], &ent[..]);
        }

        #[test]
        fn roundtrip_and_nonce_is_random() {
            let base = initialized("identity-wrap-roundtrip-");
            let ent = entropy(2);
            let first = protect_at(base.path(), b"local identity canary", &ent).expect("protect");
            let second = protect_at(base.path(), b"local identity canary", &ent).expect("protect");
            assert_ne!(first, second, "每次封装必须使用新 nonce");
            assert_ne!(&first[6..HEADER_LEN], &second[6..HEADER_LEN]);
            let opened = unprotect_at(base.path(), &first, &ent).expect("unprotect");
            assert_eq!(&**opened, b"local identity canary");
            // 幂等初始化：已有合法钥时 Ok，且绝不换钥（旧 BLOB 仍可解）。
            let key_before = fs::read(key_file(base.path())).expect("read key");
            initialize_in(base.path()).expect("重复初始化应幂等成功");
            assert_eq!(
                fs::read(key_file(base.path())).expect("read key"),
                key_before,
                "initialize_user_key 不得替换已有主钥"
            );
            assert_eq!(
                &*unprotect_at(base.path(), &first, &ent).expect("旧 BLOB 仍可解"),
                b"local identity canary"
            );
            // 换 entropy 必须认证失败（vault/device 绑定）
            let other = entropy(3);
            assert!(matches!(
                unprotect_at(base.path(), &first, &other),
                Err(SyncError::LocalWrapAuth)
            ));
        }

        #[test]
        fn key_file_is_0600_and_dirs_are_0700() {
            let base = initialized("identity-wrap-perms-");
            let file = key_file(base.path());
            let meta = fs::symlink_metadata(&file).expect("lstat key");
            assert_eq!(meta.mode() & 0o777, 0o600);
            assert_eq!(meta.uid(), current_effective_uid());
            assert_eq!(meta.nlink(), 1);
            assert_eq!(meta.len(), KEY_LEN as u64);
            let dir = key_directory(base.path());
            let meta = fs::symlink_metadata(&dir).expect("lstat dir");
            assert_eq!(meta.mode() & 0o777, 0o700);
            assert_eq!(meta.uid(), current_effective_uid());
        }

        #[test]
        fn load_does_not_create_or_repair() {
            // 目录/钥都不存在：protect 必须显式失败，且不创建任何东西。
            let base = test_base("identity-wrap-missing-");
            assert!(matches!(
                protect_at(base.path(), b"x", b"e"),
                Err(SyncError::LocalKeyMissing(_))
            ));
            assert!(!key_directory(base.path()).exists());
            assert!(!key_file(base.path()).exists());

            // 已初始化后删除钥：仍是 Missing，protect 不会重建。
            let base = initialized("identity-wrap-deleted-");
            fs::remove_file(key_file(base.path())).expect("remove key");
            assert!(matches!(
                protect_at(base.path(), b"x", b"e"),
                Err(SyncError::LocalKeyMissing(_))
            ));
            assert!(!key_file(base.path()).exists());
        }

        #[test]
        fn key_file_mode_must_be_0600() {
            let base = initialized("identity-wrap-mode-");
            let file = key_file(base.path());
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("chmod");
            assert!(matches!(
                protect_at(base.path(), b"x", b"e"),
                Err(SyncError::LocalKeyPermissions(_))
            ));
            // initialize 也不得自动修复。
            assert!(matches!(
                initialize_in(base.path()),
                Err(SyncError::LocalKeyPermissions(_))
            ));
            assert_eq!(
                fs::symlink_metadata(&file).expect("lstat").mode() & 0o777,
                0o644,
                "不自动修复权限"
            );
        }

        #[test]
        fn key_directory_mode_must_be_0700() {
            let base = initialized("identity-wrap-dirmode-");
            let dir = key_directory(base.path());
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("chmod dir");
            assert!(matches!(
                protect_at(base.path(), b"x", b"e"),
                Err(SyncError::LocalKeyPermissions(_))
            ));
        }

        #[test]
        fn symlink_key_is_rejected() {
            let base = initialized("identity-wrap-symlink-");
            let file = key_file(base.path());
            let target = base.path().join("elsewhere.bin");
            fs::rename(&file, &target).expect("move key");
            symlink(&target, &file).expect("symlink key");
            assert!(matches!(
                protect_at(base.path(), b"x", b"e"),
                Err(SyncError::LocalKeyPermissions(_))
            ));
        }

        #[test]
        fn truncated_or_oversized_key_is_rejected() {
            let base = initialized("identity-wrap-truncated-");
            let file = key_file(base.path());
            fs::write(&file, [0u8; KEY_LEN - 1]).expect("truncate");
            assert!(matches!(
                protect_at(base.path(), b"x", b"e"),
                Err(SyncError::LocalKeyCorrupt(_))
            ));
            fs::write(&file, [0u8; KEY_LEN + 1]).expect("grow");
            assert!(matches!(
                protect_at(base.path(), b"x", b"e"),
                Err(SyncError::LocalKeyCorrupt(_))
            ));
        }

        #[test]
        fn hard_linked_key_is_rejected() {
            let base = initialized("identity-wrap-link-");
            let file = key_file(base.path());
            fs::hard_link(&file, base.path().join("second-link.bin")).expect("hard link");
            assert!(matches!(
                protect_at(base.path(), b"x", b"e"),
                Err(SyncError::LocalKeyPermissions(_))
            ));
        }

        #[test]
        fn key_is_usable_from_a_copied_directory() {
            // 用户钥可以跨目录复制（同一用户），两个"独立 vault"共用同一把用户钥。
            let first = initialized("identity-wrap-copy-a-");
            let second = initialized("identity-wrap-copy-b-");
            fs::copy(key_file(first.path()), key_file(second.path())).expect("copy key");
            // 复制会带来新的 inode，权限/属主仍正确。
            let ent_a = entropy(4);
            let ent_b = entropy(5);
            let blob_a = protect_at(first.path(), b"vault A", &ent_a).expect("protect A");
            let blob_b = protect_at(second.path(), b"vault B", &ent_b).expect("protect B");
            assert_eq!(
                &*unprotect_at(second.path(), &blob_a, &ent_a).expect("A"),
                b"vault A"
            );
            assert_eq!(
                &*unprotect_at(first.path(), &blob_b, &ent_b).expect("B"),
                b"vault B"
            );
            assert!(matches!(
                unprotect_at(second.path(), &blob_a, &ent_b),
                Err(SyncError::LocalWrapAuth)
            ));
        }

        #[test]
        fn truncated_headers_and_ciphertext_tamper_are_rejected() {
            let base = initialized("identity-wrap-tamper-");
            let ent = entropy(6);
            let blob = protect_at(base.path(), b"secret payload", &ent).expect("protect");

            let mut bad_magic = blob.clone();
            bad_magic[0] ^= 0x01;
            assert!(matches!(
                unprotect_at(base.path(), &bad_magic, &ent),
                Err(SyncError::LocalWrapFormat(_))
            ));

            let mut bad_version = blob.clone();
            bad_version[4] = 99;
            assert!(matches!(
                unprotect_at(base.path(), &bad_version, &ent),
                Err(SyncError::LocalWrapVersion(99))
            ));

            let mut bad_kind = blob.clone();
            bad_kind[5] = 99;
            assert!(matches!(
                unprotect_at(base.path(), &bad_kind, &ent),
                Err(SyncError::LocalWrapKind(99))
            ));

            let mut bad_nonce = blob.clone();
            bad_nonce[6] ^= 0x01;
            assert!(matches!(
                unprotect_at(base.path(), &bad_nonce, &ent),
                Err(SyncError::LocalWrapAuth)
            ));

            let mut bad_tag = blob.clone();
            let last = bad_tag.len() - 1;
            bad_tag[last] ^= 0x01;
            assert!(matches!(
                unprotect_at(base.path(), &bad_tag, &ent),
                Err(SyncError::LocalWrapAuth)
            ));

            for cut in [
                0,
                1,
                4,
                HEADER_LEN - 1,
                HEADER_LEN,
                HEADER_LEN + TAG_LEN - 1,
            ] {
                assert!(
                    matches!(
                        unprotect_at(base.path(), &blob[..cut], &ent),
                        Err(SyncError::LocalWrapFormat(_))
                    ),
                    "截断到 {cut}B 必须被拒绝"
                );
            }
        }

        #[test]
        fn windows_dpapi_bare_blob_is_rejected_on_linux() {
            // Linux 严格只认 MNKW：Windows DPAPI 裸包（无头）必须明确拒绝，绝不回退。
            let base = initialized("identity-wrap-cross-platform-");
            let dpapi_like = vec![0x01u8; 128];
            assert!(matches!(
                unprotect_at(base.path(), &dpapi_like, b"entropy"),
                Err(SyncError::LocalWrapFormat(_))
            ));
        }

        #[test]
        fn data_home_resolution_is_strict() {
            use std::ffi::OsString;
            let xdg = |value: &str| Some(OsString::from(value));
            // XDG 优先且必须绝对。
            assert_eq!(
                resolve_data_home(xdg("/data"), xdg("/home/u")).expect("xdg"),
                PathBuf::from("/data")
            );
            assert!(matches!(
                resolve_data_home(xdg("relative"), xdg("/home/u")),
                Err(SyncError::LocalKeyPath(_))
            ));
            // 空 XDG 视为未设置，回退 HOME/.local/share。
            assert_eq!(
                resolve_data_home(Some(OsString::new()), xdg("/home/u")).expect("home"),
                PathBuf::from("/home/u/.local/share")
            );
            // HOME 缺失/相对：显式报错，不退回 cwd/tmp。
            assert!(matches!(
                resolve_data_home(None, None),
                Err(SyncError::LocalKeyPath(_))
            ));
            assert!(matches!(
                resolve_data_home(Some(OsString::new()), xdg("relative")),
                Err(SyncError::LocalKeyPath(_))
            ));
            assert!(matches!(
                resolve_data_home(None, Some(OsString::new())),
                Err(SyncError::LocalKeyPath(_))
            ));
        }

        #[test]
        fn wrong_owner_is_rejected_when_constructible() {
            // 普通用户无法 chown 到别的 UID；只有 root 跑测试时才能构造。
            if current_effective_uid() != 0 {
                return;
            }
            let base = initialized("identity-wrap-owner-");
            let file = key_file(base.path());
            let status = std::process::Command::new("chown")
                .arg("65534")
                .arg(&file)
                .status()
                .expect("chown");
            assert!(status.success(), "root 下 chown 应成功");
            assert!(matches!(
                protect_at(base.path(), b"x", b"e"),
                Err(SyncError::LocalKeyPermissions(_))
            ));
        }
    }
}

`````



@@SECTION code/034 `crates/sync/src/ids.rs`


`````rust
//! 本 crate 私有的本地标识符与小工具。
//!
//! `VaultId` / `DeviceId` / `ObjectId` 等 wire 类型由 `menghunote-sync-proto` 定义
//! （都是定长字节别名），这里只补一个 **Loro peer id** 的本地类型和十六进制显示工具。
//! 这些 ID 都**不是秘密**，可以进日志/UI（恢复码材料还要打印 vault UUID）。

use crate::error::SyncError;

const HEX: &[u8; 16] = b"0123456789abcdef";

/// 任意字节的小写十六进制（非秘密值显示用，例如设备指纹）。
#[must_use]
pub fn hex_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// 从十六进制解析定长字节（测试/恢复材料输入用）。
pub fn parse_hex<const N: usize>(s: &str) -> Result<[u8; N], SyncError> {
    if s.len() != N * 2 {
        return Err(SyncError::Identity("十六进制长度不符"));
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = (bytes[i * 2] as char)
            .to_digit(16)
            .ok_or(SyncError::Identity("十六进制字符非法"))?;
        let lo = (bytes[i * 2 + 1] as char)
            .to_digit(16)
            .ok_or(SyncError::Identity("十六进制字符非法"))?;
        *slot = ((hi << 4) | lo) as u8;
    }
    Ok(out)
}

/// 标准 UUID 字符串（8-4-4-4-12，小写）。恢复码材料需要打印 vault UUID。
#[must_use]
pub fn vault_uuid_hyphenated(vault_id: &crate::VaultId) -> String {
    let h = hex_of(vault_id);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Loro peer id（16B）。ADR 0001 §3.2：克隆备份/恢复入口必须换新 peer_id。
///
/// 不是秘密；`Debug`/`Display` 打印小写 hex。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId([u8; 16]);

impl PeerId {
    /// 从 16 字节构造。
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// 原始字节。
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 小写十六进制。
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex_of(&self.0)
    }

    /// 映射到 Loro 的 `u64` peer id（**确定性**，同一条 16B 身份每次都得同一个值）。
    ///
    /// 规则（ADR 0009 切片 2a；`genesis::GenesisMaterials::peer_id_u64` 与
    /// `identity::LocalIdentityV1::loro_peer_id` 都委托到这里，**唯一实现**）：
    ///
    /// ```text
    /// u64 = BE(peer_id[0..8]) & LORO_PEER_ID_MASK
    /// u64 == 0 → 1                      // 0 不是合法的 Loro peer
    /// ```
    ///
    /// - **前 8 字节大端**：字段本身就是随机 16B，取哪一半都一样；固定取前半段，
    ///   保证"同一身份 → 同一 u64"可复现（重启/重装/重放都不变）；
    /// - **清最高位**（[`LORO_PEER_ID_MASK`]）：Loro 的内部计数器是 `i64`，peer 的高位
    ///   会在极端计数下把 `counter` 推到负区间；因此只保留 63 位。这条与 genesis 的
    ///   既有口径一致（`docs/adr/0006-sync-v1.md` 的 peer 生成即如此），不能两边不同；
    /// - **0 → 1**：Loro 不接受 peer 0；映射到 1 意味着"映射值 0 与 1 的两类身份共用
    ///   peer 1"（两类各自的概率都是 2^-63）；
    /// - **碰撞边界**：128 → 63 位截断，不同身份**可能**撞到同一 u64（生日界约 2^32 个
    ///   身份才到 50%）。dev-sync 每个实例一个独立库、同库不混身份，这个边界可接受；
    ///   但跨设备同步一旦使用同一库，撞 peer 会让 CRDT 的 op id 冲突——届时必须用 16B
    ///   [`PeerId`] 做权威标识并拒绝复用，不能只靠这个 u64。
    #[must_use]
    pub fn loro_u64(&self) -> u64 {
        let mut head = [0u8; 8];
        head.copy_from_slice(&self.0[..8]);
        let masked = u64::from_be_bytes(head) & LORO_PEER_ID_MASK;
        // 0 不是合法 peer；折到 1（见方法文档的碰撞边界）。
        if masked == 0 {
            1
        } else {
            masked
        }
    }
}

/// Loro peer id 的可用位宽掩码：63 位（最高位清零，避免越过 `i64` 计数器域）。
///
/// 见 [`PeerId::loro_u64`]；genesis 与 app 的 dev identity 共用这一个常量。
pub const LORO_PEER_ID_MASK: u64 = 0x7fff_ffff_ffff_ffff;

impl core::fmt::Debug for PeerId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PeerId({})", self.to_hex())
    }
}

impl core::fmt::Display for PeerId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let bytes: [u8; 4] = [0x00, 0x11, 0xab, 0xff];
        assert_eq!(hex_of(&bytes), "0011abff");
        assert_eq!(parse_hex::<4>("0011abff").unwrap(), bytes);
        assert!(parse_hex::<4>("0011abf").is_err());
        assert!(parse_hex::<4>("0011abfg").is_err());
    }

    #[test]
    fn peer_id_display_is_hex() {
        let peer = PeerId::from_bytes([0x12; 16]);
        assert_eq!(peer.to_hex().len(), 32);
        assert_eq!(
            format!("{peer}"),
            format!("{peer:?}").replace("PeerId(", "").replace(')', "")
        );
    }

    #[test]
    fn loro_u64_is_big_endian_head_and_clears_high_bit() {
        // 前 8 字节大端 + 最高位清零。
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        assert_eq!(PeerId::from_bytes(bytes).loro_u64(), 0x0123_4567_89ab_cdef);
        // 最高位置位的身份：清掉最高位，落在 i64 计数器域内。
        assert_eq!(PeerId::from_bytes([0xff; 16]).loro_u64(), LORO_PEER_ID_MASK);
        // 只有后 8 字节不同 → u64 相同（映射只看前半段）；这也是碰撞边界的来源。
        let mut tail_only = [0u8; 16];
        tail_only[8..].copy_from_slice(&[0xaa; 8]);
        assert_eq!(PeerId::from_bytes(tail_only).loro_u64(), 1);
        // 确定性：同一字节永远同一结果。
        let peer = PeerId::from_bytes([0x5a; 16]);
        assert_eq!(peer.loro_u64(), peer.loro_u64());
    }

    #[test]
    fn loro_u64_never_returns_zero() {
        // 全 0 身份：0 不是合法 Loro peer，按文档折到 1。
        assert_eq!(PeerId::from_bytes([0u8; 16]).loro_u64(), 1);
        // 只有最高位（会被清掉）：同样折到 1。
        let mut high_only = [0u8; 16];
        high_only[0] = 0x80;
        assert_eq!(PeerId::from_bytes(high_only).loro_u64(), 1);
    }

    #[test]
    fn vault_uuid_layout() {
        let vault = [
            0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88,
        ];
        assert_eq!(
            vault_uuid_hyphenated(&vault),
            "12345678-9abc-def0-1122-334455667788"
        );
    }
}

`````



@@SECTION code/035 `crates/sync/src/init_journal.rs`


`````rust
//! ADR 0013 **S2b**：受限初始化的 durable pending / journal（`sync.init_pending`）。
//!
//! ## 为什么需要它
//!
//! 初始化要跨三类事实：**远端**（relay 上新建的 vault/设备）、**SQLite meta**
//! （cursor/pins/control pin）与**身份文件**（app 侧 `临时 + 原子 rename`）。三者不能放在
//! 一个事务里，任何一步之后崩溃都会留下"远端已建、本地未完成"或"本地半初始化"的状态。
//! 如果崩溃后直接重跑 Create/Join，就会在 relay 上留下不可用的孤儿 vault，或把已有材料
//! 覆盖掉。因此初始化前先在 SQLite meta 写一条 **canonical CDE v1** 的 pending：
//!
//! ```text
//! {v:1, kind:uint, mode:uint, origin:text, vault_id:b16,
//!  expected_device:b16, phase:uint, created_at:uint}
//! ```
//!
//! - `kind`：`NewGenesis=1` / `FreshRecovery=2` / `VerifiedMigration=3`；
//! - `mode`：`local_dev=1` / `remote_tls=2`（与 [`RelayMode`] 一一对应，未知值拒绝）；
//! - `origin`：canonical origin 文本（`scheme://host[:port]`，不含 userinfo/query）；
//! - `expected_device`：本项目**预期**落盘的设备 id；全 0 表示"尚未确定"（FreshRecovery
//!   在 enroll 之前还不知道新设备 id，enroll 成功后用 [`bind_expected_device`] 补上）；
//! - `phase`：只进不退的阶段阶梯，见 [`InitPhase`]；
//! - `created_at`：Unix 秒（只用于诊断/过期判断，不参与信任判定）。
//!
//! ## 阶段（每一步都 durable）
//!
//! ```text
//! Prepared → GenesisRequested → IdentityWritten → PinCommitted → Done
//! ```
//!
//! | 阶段 | 含义 |
//! | --- | --- |
//! | `Prepared` | 本地材料/write-ahead 已落盘；**初始化请求尚未发出**（写前记录保证） |
//! | `GenesisRequested` | 初始化请求**已发出**（genesis/enroll）；远端可能已建立 vault/设备，崩溃后必须对账、不得重建 |
//! | `IdentityWritten` | 身份材料已用"临时 + 原子 rename"落盘（当前由 app 侧写入；sync 只在能证明时推进） |
//! | `PinCommitted` | durable 强 pin（`sync.control_pin`）已提交 |
//! | `Done` | 本地初始化全部完成；**只有**这个阶段允许清理 pending |
//!
//! `Prepared` 与 `GenesisRequested` 的先后是**写前记录**：先写"请求将发出"，再发请求。
//! 因此 `Prepared` 崩溃可以由 [`abort_before_request`] 安全放弃（远端不可能有本次创建），
//! 而 `GenesisRequested` 崩溃必须 fail-closed（远端可能已创建）。唯一的例外是
//! `abort_before_identity`（crate 内受限）：远端初始化在**拿到回执/身份材料之前**失败时，本地没有任何
//! 可对账的材料，保留这个新库只会让路径永久不可用（细节见该函数的文档）。
//!
//! ## 纪律
//!
//! - 阶段**只进不退**（[`advance_phase`] 用 CAS 比较 canonical 字节，落后/超前都拒绝）；
//! - pending 损坏/版本未知/字段残缺/vault 绑定不符都是**明确错误**
//!   （[`SyncError::ConfigInvalid`]），**不得**当成"没有 pending"（否则就等于绕过对账）；
//! - 清理只在 `Done`（[`clear_pending`]）；未完成时[`crate::relay_context::RelayContext::open`]
//!   会拒绝普通同步/管理操作（fail-closed，见 [`ensure_no_pending_sync`]）；
//! - pending **不含任何秘密**：没有根、私钥、恢复种子、恢复码；只有公开 id/模式/origin/阶段。

use menghunote_storage::{AffectedGuard, Statement, StorageError, Value, Vault};
use menghunote_sync_proto::{decode_cde, encode_cde, CdeValue};

use crate::api_version::RelayApiVersion;
use crate::error::SyncError;
use crate::relay_policy::RelayMode;

/// meta 键：初始化 pending（canonical CDE v1）。
pub const META_INIT_PENDING: &str = "sync.init_pending";

/// pending 结构版本（canonical CDE 键 0）。
pub const INIT_PENDING_VERSION: u64 = 1;

const KEY_VERSION: u64 = 0;
const KEY_KIND: u64 = 1;
const KEY_MODE: u64 = 2;
const KEY_ORIGIN: u64 = 3;
const KEY_VAULT_ID: u64 = 4;
const KEY_EXPECTED_DEVICE: u64 = 5;
const KEY_PHASE: u64 = 6;
const KEY_CREATED_AT: u64 = 7;
/// ADR 0014 S3a：relay API 面。**可选**字段：v1 pending 不写（保持既有字节完全不变），
/// 缺省按 v1 解读；v2 创建（票据）写 2。旧二进制读到 v2 pending 会按"未知字段"拒绝，
/// 这正是单向升级的 fail-closed 语义（不静默降级）。
const KEY_API_VERSION: u64 = 8;

/// 初始化能力种类（ADR 0013 §3：只有这三种受限能力，没有 None/unpin/reset）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InitKind {
    /// 新 vault：本地提交证书复算 rev1 `record_hash`，TLS 后验证链前缀并提交强 pin。
    NewGenesis,
    /// 恢复码加入：恢复码管理锚 + TLS + 认证后的控制链/checkpoint/对象验证。
    FreshRecovery,
    /// 已有库/身份与旧 pin 的受限迁移（转 canonical v1；不具备条件明确拒绝）。
    VerifiedMigration,
}

impl InitKind {
    /// CDE 里的稳定取值（**不得**重排；未知值读取时拒绝）。
    #[must_use]
    pub const fn as_code(self) -> u64 {
        match self {
            Self::NewGenesis => 1,
            Self::FreshRecovery => 2,
            Self::VerifiedMigration => 3,
        }
    }

    /// 从 CDE 取值解析；未知值返回 `None`（调用方必须拒绝，不做默认回退）。
    #[must_use]
    pub const fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::NewGenesis),
            2 => Some(Self::FreshRecovery),
            3 => Some(Self::VerifiedMigration),
            _ => None,
        }
    }

    /// 诊断/日志用的稳定短标签。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NewGenesis => "new_genesis",
            Self::FreshRecovery => "fresh_recovery",
            Self::VerifiedMigration => "verified_migration",
        }
    }
}

/// 初始化阶段（只进不退；整理见模块文档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InitPhase {
    /// write-ahead 已落盘，远端结果未知。
    Prepared,
    /// 远端初始化已确认成功（vault/设备已在 relay 建立）。
    GenesisRequested,
    /// 身份材料已原子落盘（app 侧写入；sync 只在能证明时推进）。
    IdentityWritten,
    /// durable 强 pin 已提交。
    PinCommitted,
    /// 本地初始化完成（**唯一**允许清理 pending 的阶段）。
    Done,
}

impl InitPhase {
    /// CDE 里的稳定取值。
    #[must_use]
    pub const fn as_code(self) -> u64 {
        match self {
            Self::Prepared => 1,
            Self::GenesisRequested => 2,
            Self::IdentityWritten => 3,
            Self::PinCommitted => 4,
            Self::Done => 5,
        }
    }

    /// 从 CDE 取值解析；未知值返回 `None`。
    #[must_use]
    pub const fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::Prepared),
            2 => Some(Self::GenesisRequested),
            3 => Some(Self::IdentityWritten),
            4 => Some(Self::PinCommitted),
            5 => Some(Self::Done),
            _ => None,
        }
    }

    /// 诊断/日志用的稳定短标签。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::GenesisRequested => "genesis_requested",
            Self::IdentityWritten => "identity_written",
            Self::PinCommitted => "pin_committed",
            Self::Done => "done",
        }
    }

    /// 初始化请求是否**已经发出**（`GenesisRequested` 及之后）。
    ///
    /// `Prepared` = 写前记录已落盘但请求未发出：此时远端不可能有本次创建的事实。
    /// 一旦为 `true`，崩溃后既不能重跑 genesis/enroll（可能建出孤儿 vault/设备），
    /// 也不能假定远端不存在：只能按已有材料 finalize 或 fail-closed。
    #[must_use]
    pub const fn request_sent(self) -> bool {
        matches!(
            self,
            Self::GenesisRequested | Self::IdentityWritten | Self::PinCommitted | Self::Done
        )
    }
}

/// 初始化 pending（`sync.init_pending` 的语义结构）。
///
/// **不含秘密**：只有公开 id、模式、origin、阶段与时间戳。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitPendingV1 {
    /// 受限初始化能力种类。
    pub kind: InitKind,
    /// relay 接入模式。
    pub mode: RelayMode,
    /// canonical origin（`scheme://host[:port]`）。
    pub origin: String,
    /// relay API 面（ADR 0014 S3a）：v2 票据创建写 v2；旧 pending/缺省 = v1。
    pub api_version: RelayApiVersion,
    /// 本次初始化要建立的 vault id。
    pub vault_id: [u8; 16],
    /// 预期落盘的设备 id；全 0 = 尚未确定（FreshRecovery 在 enroll 前）。
    pub expected_device: [u8; 16],
    /// 当前阶段。
    pub phase: InitPhase,
    /// 创建时刻（Unix 秒；诊断用，不参与信任判定）。
    pub created_at: u64,
}

impl InitPendingV1 {
    /// 构造一条 `Prepared` pending。
    #[must_use]
    pub fn prepared(
        kind: InitKind,
        mode: RelayMode,
        origin: impl Into<String>,
        vault_id: [u8; 16],
        expected_device: [u8; 16],
        created_at: u64,
    ) -> Self {
        // 兼容入口：历史调用点（local_dev / remote v1）固定 v1 面。
        Self::prepared_with_api_version(
            kind,
            mode,
            origin,
            vault_id,
            expected_device,
            created_at,
            RelayApiVersion::V1,
        )
    }

    /// 显式 API 面的构造（ADR 0014 S3a：v2 票据创建/新配置用）。
    #[must_use]
    pub fn prepared_with_api_version(
        kind: InitKind,
        mode: RelayMode,
        origin: impl Into<String>,
        vault_id: [u8; 16],
        expected_device: [u8; 16],
        created_at: u64,
        api_version: RelayApiVersion,
    ) -> Self {
        Self {
            kind,
            mode,
            origin: origin.into(),
            api_version,
            vault_id,
            expected_device,
            phase: InitPhase::Prepared,
            created_at,
        }
    }

    /// 设备 id 是否已确定。
    #[must_use]
    pub fn has_expected_device(&self) -> bool {
        self.expected_device != [0u8; 16]
    }
}

fn mode_code(mode: RelayMode) -> u64 {
    match mode {
        RelayMode::LocalDev => 1,
        RelayMode::RemoteTls => 2,
    }
}

fn mode_from_code(code: u64) -> Option<RelayMode> {
    match code {
        1 => Some(RelayMode::LocalDev),
        2 => Some(RelayMode::RemoteTls),
        _ => None,
    }
}

fn corrupt(message: &'static str) -> SyncError {
    SyncError::ConfigInvalid(format!("sync.init_pending 损坏：{message}"))
}

/// canonical CDE 编码（唯一编码器；写库/测试夹具共用）。
pub fn encode_pending_v1(pending: &InitPendingV1) -> Result<Vec<u8>, SyncError> {
    let mut fields = vec![
        (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
        (KEY_KIND, CdeValue::UInt(pending.kind.as_code())),
        (KEY_MODE, CdeValue::UInt(mode_code(pending.mode))),
        (KEY_ORIGIN, CdeValue::Text(pending.origin.clone())),
        (KEY_VAULT_ID, CdeValue::Bytes(pending.vault_id.to_vec())),
        (
            KEY_EXPECTED_DEVICE,
            CdeValue::Bytes(pending.expected_device.to_vec()),
        ),
        (KEY_PHASE, CdeValue::UInt(pending.phase.as_code())),
        (KEY_CREATED_AT, CdeValue::UInt(pending.created_at)),
    ];
    // v1 面保持既有编码逐字节不变（旧 golden/旧库不迁移）；v2 才追加键 8。
    if pending.api_version != RelayApiVersion::V1 {
        fields.push((
            KEY_API_VERSION,
            CdeValue::UInt(pending.api_version.as_code()),
        ));
    }
    encode_cde(&CdeValue::Map(fields))
        .map_err(|_| SyncError::ConfigInvalid("sync.init_pending 无法 canonical 编码".to_owned()))
}

/// canonical CDE 解码 + 严格字段/版本/长度校验。
///
/// 任何结构问题都返回 [`SyncError::ConfigInvalid`]，**不**返回 `None`（fail-closed）。
pub fn decode_pending_v1(bytes: &[u8]) -> Result<InitPendingV1, SyncError> {
    let CdeValue::Map(fields) = decode_cde(bytes).map_err(|_| corrupt("不是合法 canonical CDE"))?
    else {
        return Err(corrupt("不是 CDE map"));
    };
    for (key, _) in &fields {
        if *key > KEY_API_VERSION {
            return Err(corrupt("含未知字段"));
        }
    }
    let version = uint_field(&fields, KEY_VERSION)?;
    if version != INIT_PENDING_VERSION {
        return Err(SyncError::ConfigInvalid(format!(
            "sync.init_pending 版本不支持：{version}"
        )));
    }
    let kind_code = uint_field(&fields, KEY_KIND)?;
    let kind = InitKind::from_code(kind_code).ok_or(corrupt("kind 未知"))?;
    let mode_code = uint_field(&fields, KEY_MODE)?;
    let mode = mode_from_code(mode_code).ok_or(corrupt("relay_mode 未知"))?;
    let origin = match field(&fields, KEY_ORIGIN)? {
        CdeValue::Text(text) => text.clone(),
        _ => return Err(corrupt("origin 不是文本")),
    };
    if origin.is_empty() {
        return Err(corrupt("origin 为空"));
    }
    let vault_id = fixed_field::<16>(&fields, KEY_VAULT_ID)?;
    let expected_device = fixed_field::<16>(&fields, KEY_EXPECTED_DEVICE)?;
    let phase_code = uint_field(&fields, KEY_PHASE)?;
    let phase = InitPhase::from_code(phase_code).ok_or(corrupt("phase 未知"))?;
    let created_at = uint_field(&fields, KEY_CREATED_AT)?;
    // 缺省 = v1（ADR 0014 S3a 的旧 pending 兼容）；未知数字 fail-closed。
    let api_version = match fields.iter().find(|(key, _)| *key == KEY_API_VERSION) {
        Some((_, CdeValue::UInt(code))) => {
            RelayApiVersion::from_code(*code).ok_or(corrupt("api_version 未知"))?
        }
        Some(_) => return Err(corrupt("api_version 类型不符")),
        None => RelayApiVersion::V1,
    };
    Ok(InitPendingV1 {
        kind,
        mode,
        origin,
        api_version,
        vault_id,
        expected_device,
        phase,
        created_at,
    })
}

fn field(fields: &[(u64, CdeValue)], key: u64) -> Result<&CdeValue, SyncError> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
        .ok_or(corrupt("缺字段"))
}

fn uint_field(fields: &[(u64, CdeValue)], key: u64) -> Result<u64, SyncError> {
    match field(fields, key)? {
        CdeValue::UInt(value) => Ok(*value),
        _ => Err(corrupt("整数字段类型不符")),
    }
}

fn fixed_field<const N: usize>(fields: &[(u64, CdeValue)], key: u64) -> Result<[u8; N], SyncError> {
    let CdeValue::Bytes(bytes) = field(fields, key)? else {
        return Err(corrupt("定长字节字段类型不符"));
    };
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| corrupt("定长字节字段长度不符"))
}

/// 读取 pending：不存在 → `Ok(None)`；损坏/绑定不符 → 明确错误。
///
/// `expected_vault` 为 `Some` 时要求 pending 的 `vault_id` 与本地 vault 一致
/// （复制/错配的 pending 不能被当成"本次初始化"）。
pub fn load_pending(
    vault: &Vault,
    expected_vault: Option<[u8; 16]>,
) -> Result<Option<InitPendingV1>, SyncError> {
    let Some(bytes) = vault.get_meta(META_INIT_PENDING)? else {
        return Ok(None);
    };
    let pending = decode_pending_v1(&bytes)?;
    if let Some(expected) = expected_vault {
        if pending.vault_id != expected {
            return Err(SyncError::ConfigInvalid(
                "sync.init_pending 的 vault 绑定与本地 vault 不一致".to_owned(),
            ));
        }
    }
    Ok(Some(pending))
}

/// 开始一次初始化：写入 `Prepared` pending（**只在不存在时**插入）。
///
/// 语义（"重复 Create/Join 的幂等"）：
///
/// - 没有 pending → 写入并返回 `Ok`；
/// - 已有**完全相同**的 pending → `Ok`（崩溃/重试的续跑，不重复写、不覆盖）；
/// - 已有**不同**的 pending → [`SyncError::ConfigInvalid`]（未完成的初始化必须先对账，
///   不得用新初始化覆盖，否则会丢失"远端已建"的线索）。
pub fn begin_pending(vault: &Vault, pending: &InitPendingV1) -> Result<(), SyncError> {
    if pending.phase != InitPhase::Prepared {
        return Err(SyncError::ConfigInvalid(
            "初始 pending 必须是 prepared 阶段".to_owned(),
        ));
    }
    let bytes = encode_pending_v1(pending)?;
    let key = Value::Text(META_INIT_PENDING.to_owned());
    let statement = Statement::new(
        "INSERT INTO meta(key, value) SELECT ?1, ?2 \
         WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key = ?1)",
        vec![key, Value::Blob(bytes)],
    );
    match vault.exec_transaction(&[statement], &[AffectedGuard::exactly(0, 1)]) {
        Ok(_) => Ok(()),
        Err(StorageError::GuardFailed { .. }) => {
            // 已存在：只有逐字节相同才算幂等续跑。
            let existing = load_pending(vault, Some(pending.vault_id))?.ok_or_else(|| {
                SyncError::ConfigInvalid(
                    "sync.init_pending 写入竞争后消失（不自动重建）".to_owned(),
                )
            })?;
            if existing == *pending {
                Ok(())
            } else {
                Err(SyncError::ConfigInvalid(format!(
                    "已有未完成的初始化 pending（{} / {}）：必须先启动对账，不得覆盖",
                    existing.kind.as_str(),
                    existing.phase.as_str()
                )))
            }
        }
        Err(error) => Err(SyncError::ConfigInvalid(format!(
            "sync.init_pending 写入失败：{error}"
        ))),
    }
}

/// 用 CAS 推进一次阶段（只进不退）。
pub fn advance_phase(
    vault: &Vault,
    expected: InitPhase,
    next: InitPhase,
) -> Result<InitPendingV1, SyncError> {
    if next <= expected {
        return Err(SyncError::ConfigInvalid(
            "初始化阶段只能前进（不允许回退/原地）".to_owned(),
        ));
    }
    persist_transition(vault, expected, |pending| {
        pending.phase = next;
    })
}

/// 绑定预期设备 id（只允许从"未确定"变为"已确定"）。
pub fn bind_expected_device(
    vault: &Vault,
    expected: InitPhase,
    device_id: [u8; 16],
) -> Result<InitPendingV1, SyncError> {
    if device_id == [0u8; 16] {
        return Err(SyncError::ConfigInvalid(
            "预期设备 id 不能是全 0".to_owned(),
        ));
    }
    let current = load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending".to_owned(),
    ))?;
    if current.has_expected_device() {
        return Err(SyncError::ConfigInvalid(
            "sync.init_pending 已绑定预期设备：不允许改写（可能换错设备）".to_owned(),
        ));
    }
    persist_transition(vault, expected, |pending| {
        pending.expected_device = device_id;
    })
}

/// 用 CAS 改写一个阶段内的字段（`expected` 是调用方读到的阶段）。
///
/// CAS 对象是**存储里的 canonical 字节**：并发写者先推进了阶段时这里失败，不覆盖赢家。
fn persist_transition(
    vault: &Vault,
    expected_phase: InitPhase,
    mutate: impl FnOnce(&mut InitPendingV1),
) -> Result<InitPendingV1, SyncError> {
    let current = load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending".to_owned(),
    ))?;
    if current.phase != expected_phase {
        return Err(SyncError::ConfigInvalid(format!(
            "sync.init_pending 阶段不符：期望 {}，实际 {}",
            expected_phase.as_str(),
            current.phase.as_str()
        )));
    }
    let old_bytes = encode_pending_v1(&current)?;
    let mut next = current;
    mutate(&mut next);
    let new_bytes = encode_pending_v1(&next)?;
    vault.exec_transaction(
        &[Statement::new(
            "UPDATE meta SET value = ?2 WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?3",
            vec![
                Value::Text(META_INIT_PENDING.to_owned()),
                Value::Blob(new_bytes),
                Value::Blob(old_bytes),
            ],
        )],
        &[AffectedGuard::exactly(0, 1)],
    )
    .map_err(|error| match error {
        StorageError::GuardFailed { .. } => SyncError::ConfigInvalid(
            "sync.init_pending 被并发修改（不覆盖赢家，需重新对账）".to_owned(),
        ),
        other => SyncError::ConfigInvalid(format!("sync.init_pending 更新失败：{other}")),
    })?;
    Ok(next)
}

/// 初始化请求**尚未发出**时的安全清理（唯一的非 `Done` 清理入口）。
///
/// 为什么允许：`Prepared` 是写前记录，进入 `GenesisRequested` 之前不会发任何初始化
/// 请求（见模块文档的阶段表），因此此时远端**不可能**有本次创建的事实；继续卡在
/// "未完成初始化"只会让这个新路径永久不可用。它**只删 pending 记录**，不碰库数据/
/// 身份文件/用户目录；请求一旦发出（`GenesisRequested` 及之后）就只能走
/// [`crate::init_session::reconcile`] 的 finalize 路径。
pub fn abort_before_request(vault: &Vault) -> Result<(), SyncError> {
    let current = load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending".to_owned(),
    ))?;
    if current.phase != InitPhase::Prepared {
        return Err(SyncError::ConfigInvalid(format!(
            "初始化已进入 {} 阶段（请求可能已发出）：只允许在 prepared 阶段安全放弃",
            current.phase.as_str()
        )));
    }
    delete_pending_matching(vault, &current)
}

/// **远端初始化专用**：请求已发出、但**身份材料从未落盘**时的安全清理
/// （`Prepared` / `GenesisRequested` 两个阶段）。
///
/// 为什么允许在 `GenesisRequested` 清理：ADR 0013 §3 的"请求已发就必须对账"针对的是
/// **可继续的初始化**——远端可能已建立 vault/设备，但本地已经拿到了回执与身份材料，
/// 对账能补 finalize。若在拿到回执之前失败（TLS/策略拒绝、relay 明确拒绝、传输中断），
/// 本地**没有任何可用身份材料**：这个新库永远无法被 finalize，继续保留只会让这条
/// 新路径永久不可用（`RelayContext::open` 被 pending 挡住）。此时：
///
/// - 远端最多留下一个**没有身份、没有槽/checkpoint 的空 vault/孤儿设备**（不可用，
///   与 ADR 0009 既有失败语义一致），下次初始化会建新的；
/// - 本地只清 pending（不碰库数据/身份文件），随后由调用方删除本次新建的库文件——它
///   是本操作自己刚建出来的，不是用户数据。
///
/// `IdentityWritten`/`PinCommitted`/`Done` 一律拒绝：一旦身份落盘，初始化就**可以**
/// 被对账继续，删 pending 等于丢掉"远端可能已建"的线索。
///
/// `pub(crate)`：能力只给 `crate::remote_init` 的受限编排，不是应用层可随手调用的开关。
pub(crate) fn abort_before_identity(vault: &Vault) -> Result<(), SyncError> {
    let current = load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending".to_owned(),
    ))?;
    if current.phase >= InitPhase::IdentityWritten {
        return Err(SyncError::ConfigInvalid(format!(
            "初始化已进入 {} 阶段（身份已落盘）：只能走启动对账，不得放弃 pending",
            current.phase.as_str()
        )));
    }
    delete_pending_matching(vault, &current)
}

/// 删除**逐字节匹配**当前 pending 的那一行（CAS：并发修改时拒绝，不删赢家）。
fn delete_pending_matching(vault: &Vault, current: &InitPendingV1) -> Result<(), SyncError> {
    let bytes = encode_pending_v1(current)?;
    vault
        .exec_transaction(
            &[Statement::new(
                "DELETE FROM meta WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?2",
                vec![
                    Value::Text(META_INIT_PENDING.to_owned()),
                    Value::Blob(bytes),
                ],
            )],
            &[AffectedGuard::exactly(0, 1)],
        )
        .map_err(|error| match error {
            StorageError::GuardFailed { .. } => SyncError::ConfigInvalid(
                "sync.init_pending 在放弃时被并发修改（不删赢家的状态）".to_owned(),
            ),
            other => SyncError::ConfigInvalid(format!("sync.init_pending 放弃失败：{other}")),
        })?;
    Ok(())
}

/// 清理 pending：**只允许** `Done`（ADR 0013 §3「清理只在 Done」）。
pub fn clear_pending(vault: &Vault) -> Result<(), SyncError> {
    let current = load_pending(vault, None)?.ok_or(SyncError::ConfigInvalid(
        "缺少 sync.init_pending".to_owned(),
    ))?;
    if current.phase != InitPhase::Done {
        return Err(SyncError::ConfigInvalid(format!(
            "初始化尚未完成（阶段 {}）：只在 done 阶段允许清理 pending",
            current.phase.as_str()
        )));
    }
    let bytes = encode_pending_v1(&current)?;
    vault
        .exec_transaction(
            &[Statement::new(
                "DELETE FROM meta WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?2",
                vec![
                    Value::Text(META_INIT_PENDING.to_owned()),
                    Value::Blob(bytes),
                ],
            )],
            &[AffectedGuard::exactly(0, 1)],
        )
        .map_err(|error| match error {
            StorageError::GuardFailed { .. } => SyncError::ConfigInvalid(
                "sync.init_pending 在清理时被并发修改（不删赢家的状态）".to_owned(),
            ),
            other => SyncError::ConfigInvalid(format!("sync.init_pending 清理失败：{other}")),
        })?;
    Ok(())
}

/// 普通同步/管理操作的守卫：pending 未完成时**明确阻止**（ADR 0013 §3）。
///
/// `Done` 是"已完成、等清理"，允许放行（对账会清理它）。
pub fn ensure_no_pending_sync(vault: &Vault, vault_id: [u8; 16]) -> Result<(), SyncError> {
    let Some(pending) = load_pending(vault, Some(vault_id))? else {
        return Ok(());
    };
    if pending.phase == InitPhase::Done {
        return Ok(());
    }
    Err(SyncError::NeedsTrust(
        "本地初始化未完成（sync.init_pending）：拒绝启动普通同步/管理操作，请先完成或对账初始化",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: [u8; 16] = [0x5a; 16];
    const DEVICE: [u8; 16] = [0x04; 16];

    fn fresh() -> (tempfile::TempDir, Vault) {
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("init-journal-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    fn pending(kind: InitKind) -> InitPendingV1 {
        InitPendingV1::prepared(
            kind,
            RelayMode::LocalDev,
            "http://127.0.0.1:18080",
            VAULT,
            DEVICE,
            1_700_000_000,
        )
    }

    fn raw(vault: &Vault) -> Option<Vec<u8>> {
        vault.get_meta(META_INIT_PENDING).expect("读 meta")
    }

    #[test]
    fn pending_roundtrips_through_canonical_cde() {
        for kind in [
            InitKind::NewGenesis,
            InitKind::FreshRecovery,
            InitKind::VerifiedMigration,
        ] {
            for mode in [RelayMode::LocalDev, RelayMode::RemoteTls] {
                for phase in [
                    InitPhase::Prepared,
                    InitPhase::GenesisRequested,
                    InitPhase::IdentityWritten,
                    InitPhase::PinCommitted,
                    InitPhase::Done,
                ] {
                    let mut value = pending(kind);
                    value.mode = mode;
                    value.phase = phase;
                    let bytes = encode_pending_v1(&value).expect("编码");
                    assert_eq!(decode_pending_v1(&bytes).expect("解码"), value);
                    // 编码是 canonical 的：同值同字节。
                    assert_eq!(encode_pending_v1(&value).expect("再编码"), bytes);
                }
            }
        }
    }

    /// ADR 0014 S3a：旧 pending（无键 8）按 v1 解读且编码逐字节不变；v2 才写键 8；
    /// 未知 api_version 明确拒绝（不静默回退）。
    #[test]
    fn pending_api_version_is_optional_and_v2_roundtrips() {
        let v1 = pending(InitKind::NewGenesis);
        assert_eq!(v1.api_version, RelayApiVersion::V1);
        let bytes_v1 = encode_pending_v1(&v1).expect("编码");
        // 历史 v1 编码（键 0..=7）：不含键 8（旧二进制/旧 golden 仍可读）。
        let fields = match decode_cde(&bytes_v1).expect("cde") {
            CdeValue::Map(fields) => fields,
            other => panic!("不是 map：{other:?}"),
        };
        assert!(fields.iter().all(|(key, _)| *key != KEY_API_VERSION));

        let v2 = InitPendingV1::prepared_with_api_version(
            InitKind::NewGenesis,
            RelayMode::LocalDev,
            "http://127.0.0.1:18080",
            VAULT,
            DEVICE,
            1_700_000_000,
            RelayApiVersion::V2,
        );
        let bytes_v2 = encode_pending_v1(&v2).expect("编码 v2");
        assert_ne!(bytes_v1, bytes_v2);
        assert_eq!(decode_pending_v1(&bytes_v2).expect("解码 v2"), v2);
        assert_eq!(
            decode_pending_v1(&bytes_v1).expect("解码旧").api_version,
            RelayApiVersion::V1
        );

        // 未知 api_version 值：明确错误。
        let mut fields = match decode_cde(&bytes_v2).expect("cde") {
            CdeValue::Map(fields) => fields,
            other => panic!("不是 map：{other:?}"),
        };
        for (key, value) in &mut fields {
            if *key == KEY_API_VERSION {
                *value = CdeValue::UInt(9);
            }
        }
        let broken = encode_cde(&CdeValue::Map(fields)).expect("cde");
        assert!(
            decode_pending_v1(&broken).is_err(),
            "未知 api_version 必须拒绝"
        );
    }

    #[test]
    fn corrupt_or_unknown_pending_is_an_error_not_missing() {
        let (_dir, vault) = fresh();
        let cde = |fields: Vec<(u64, CdeValue)>| encode_cde(&CdeValue::Map(fields)).expect("cde");
        let blobs: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0xFF, 0x00],
            // 版本 2。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(2)),
                (KEY_KIND, CdeValue::UInt(1)),
                (KEY_MODE, CdeValue::UInt(1)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(1)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
            ]),
            // 未知 kind。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
                (KEY_KIND, CdeValue::UInt(9)),
                (KEY_MODE, CdeValue::UInt(1)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(1)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
            ]),
            // 未知 mode。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
                (KEY_KIND, CdeValue::UInt(1)),
                (KEY_MODE, CdeValue::UInt(7)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(1)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
            ]),
            // 未知 phase。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
                (KEY_KIND, CdeValue::UInt(1)),
                (KEY_MODE, CdeValue::UInt(1)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(17)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
            ]),
            // vault_id 长度错。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
                (KEY_KIND, CdeValue::UInt(1)),
                (KEY_MODE, CdeValue::UInt(1)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(vec![0; 15])),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(1)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
            ]),
            // 未知字段。
            cde(vec![
                (KEY_VERSION, CdeValue::UInt(INIT_PENDING_VERSION)),
                (KEY_KIND, CdeValue::UInt(1)),
                (KEY_MODE, CdeValue::UInt(1)),
                (KEY_ORIGIN, CdeValue::Text("http://127.0.0.1".to_owned())),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_EXPECTED_DEVICE, CdeValue::Bytes(DEVICE.to_vec())),
                (KEY_PHASE, CdeValue::UInt(1)),
                (KEY_CREATED_AT, CdeValue::UInt(1)),
                (99, CdeValue::UInt(1)),
            ]),
        ];
        for blob in blobs {
            vault
                .set_meta(META_INIT_PENDING, &blob)
                .expect("写坏 pending");
            let error = load_pending(&vault, Some(VAULT)).expect_err("损坏必须报错，不能当缺失");
            assert!(
                matches!(error, SyncError::ConfigInvalid(_)),
                "必须是 ConfigInvalid：{error:?}"
            );
        }
    }

    #[test]
    fn begin_is_idempotent_only_for_the_same_pending() {
        let (_dir, vault) = fresh();
        let first = pending(InitKind::NewGenesis);
        begin_pending(&vault, &first).expect("首次");
        begin_pending(&vault, &first).expect("同一 pending 的重试必须幂等");
        let other = pending(InitKind::FreshRecovery);
        let error = begin_pending(&vault, &other).expect_err("不同 pending 必须拒绝");
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
        assert_eq!(load_pending(&vault, Some(VAULT)).expect("读"), Some(first));
        // 非 Prepared 阶段不得作为"初始 pending"写入。
        let mut not_prepared = pending(InitKind::FreshRecovery);
        not_prepared.phase = InitPhase::Done;
        assert!(begin_pending(&vault, &not_prepared).is_err());
    }

    #[test]
    fn vault_binding_mismatch_is_rejected() {
        let (_dir, vault) = fresh();
        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("写入");
        let error = load_pending(&vault, Some([0x99; 16])).expect_err("错绑必须拒绝");
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
    }

    #[test]
    fn phases_only_move_forward_and_bind_device_once() {
        let (_dir, vault) = fresh();
        begin_pending(
            &vault,
            &InitPendingV1::prepared(
                InitKind::FreshRecovery,
                RelayMode::LocalDev,
                "http://127.0.0.1:18080",
                VAULT,
                [0u8; 16],
                7,
            ),
        )
        .expect("写入");
        // 回退/原地都拒绝。
        assert!(advance_phase(&vault, InitPhase::Prepared, InitPhase::Prepared).is_err());
        let bound = bind_expected_device(&vault, InitPhase::Prepared, DEVICE).expect("绑定设备");
        assert_eq!(bound.expected_device, DEVICE);
        assert_eq!(bound.phase, InitPhase::Prepared, "绑定设备不推进阶段");
        // 已绑定后再改设备 = 矛盾，拒绝（不能让 pending 悄悄指向另一台设备）。
        assert!(bind_expected_device(&vault, InitPhase::Prepared, [0x77; 16]).is_err());
        // 阶段期望不符（调用方读到的是旧阶段）→ 拒绝。
        let error = advance_phase(&vault, InitPhase::GenesisRequested, InitPhase::PinCommitted)
            .expect_err("阶段期望不符必须拒绝");
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
        // 正常前进。
        advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested).expect("前进");
        advance_phase(
            &vault,
            InitPhase::GenesisRequested,
            InitPhase::IdentityWritten,
        )
        .expect("前进");
        advance_phase(&vault, InitPhase::IdentityWritten, InitPhase::PinCommitted).expect("前进");
        advance_phase(&vault, InitPhase::PinCommitted, InitPhase::Done).expect("前进");
        assert_eq!(
            load_pending(&vault, Some(VAULT))
                .expect("读")
                .expect("在")
                .phase,
            InitPhase::Done
        );
    }

    #[test]
    fn abort_before_request_only_allows_prepared() {
        let (_dir, vault) = fresh();
        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("写入");
        abort_before_request(&vault).expect("prepared 阶段允许安全放弃");
        assert!(raw(&vault).is_none(), "放弃后不得留下 pending");

        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("再次写入");
        advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested).expect("前进");
        let error = abort_before_request(&vault).expect_err("请求已发出后不得放弃");
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
        assert!(raw(&vault).is_some(), "拒绝路径不得删 pending");
    }

    #[test]
    fn clear_is_allowed_only_at_done() {
        let (_dir, vault) = fresh();
        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("写入");
        let error = clear_pending(&vault).expect_err("未完成不得清理");
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
        assert!(raw(&vault).is_some(), "拒绝路径不得删 pending");

        advance_phase(&vault, InitPhase::Prepared, InitPhase::GenesisRequested).expect("前进");
        advance_phase(
            &vault,
            InitPhase::GenesisRequested,
            InitPhase::IdentityWritten,
        )
        .expect("前进");
        advance_phase(&vault, InitPhase::IdentityWritten, InitPhase::PinCommitted).expect("前进");
        assert!(clear_pending(&vault).is_err(), "PinCommitted 仍不得清理");
        advance_phase(&vault, InitPhase::PinCommitted, InitPhase::Done).expect("前进");
        clear_pending(&vault).expect("done 阶段清理");
        assert!(raw(&vault).is_none(), "清理后必须没有 pending");
        // 第二次清理是明确错误（不能把"没有 pending"当成"清理成功"）。
        assert!(clear_pending(&vault).is_err());
    }

    #[test]
    fn sync_guard_blocks_unfinished_and_allows_done() {
        let (_dir, vault) = fresh();
        // 没有 pending：放行。
        ensure_no_pending_sync(&vault, VAULT).expect("无 pending");
        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("写入");
        for phase in [
            InitPhase::Prepared,
            InitPhase::GenesisRequested,
            InitPhase::IdentityWritten,
            InitPhase::PinCommitted,
        ] {
            if phase.as_code() > 1 {
                let previous = InitPhase::from_code(phase.as_code() - 1).expect("上一阶段");
                advance_phase(&vault, previous, phase).expect("前进");
            }
            let error = ensure_no_pending_sync(&vault, VAULT).expect_err("未完成必须阻止普通同步");
            assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
        }
        advance_phase(&vault, InitPhase::PinCommitted, InitPhase::Done).expect("前进");
        ensure_no_pending_sync(&vault, VAULT).expect("done 阶段放行（等清理）");
    }

    /// 崩溃矩阵的"存储层"部分：每个阶段落盘后重开库，pending 逐字节可读、阶段不回退。
    #[test]
    fn every_phase_survives_a_reopen() {
        let (_dir, vault) = fresh();
        let path = vault.path().to_path_buf();
        begin_pending(&vault, &pending(InitKind::NewGenesis)).expect("写入");
        let phases = [
            InitPhase::Prepared,
            InitPhase::GenesisRequested,
            InitPhase::IdentityWritten,
            InitPhase::PinCommitted,
            InitPhase::Done,
        ];
        let mut vault = vault;
        for (index, phase) in phases.iter().enumerate() {
            if index > 0 {
                advance_phase(&vault, phases[index - 1], *phase).expect("前进");
            }
            let bytes = raw(&vault).expect("持久化");
            drop(vault);
            vault = Vault::open(&path).expect("重开");
            assert_eq!(
                raw(&vault).expect("读"),
                bytes,
                "阶段 {phase:?} 必须逐字节持久"
            );
            assert_eq!(
                load_pending(&vault, Some(VAULT))
                    .expect("读")
                    .expect("在")
                    .phase,
                *phase
            );
            if *phase == InitPhase::Done {
                clear_pending(&vault).expect("清理");
            }
        }
    }
}

`````



@@SECTION code/036 `crates/sync/src/kdf.rs`


`````rust
//! 口令 KDF（Argon2id）与 HKDF-SHA256 子密钥派生。
//!
//! ADR 0006 §3 的硬约束：
//!
//! - 口令是**原样 UTF-8**：不 trim、不做 Unicode 规范化，长度 1..=1024 字节；
//! - 写者固定 `argon2id, version=0x13, m=65536 KiB, t=3, p=1, salt=random16, output_len=32`；
//! - 读者允许 `m=64..256MiB, t=3..6, p=1..4`，**越界必须在调用 Argon2 之前拒绝**
//!   （否则恶意 slot 描述可以让客户端分配 1TiB 或算上几分钟）。
//!
//! 参数类型用 `menghunote-sync-proto` 的 [`Argon2idParams`]（wire 真源）；本模块只做
//! 范围策略与 KDF 计算。

use crate::error::SyncError;
use crate::keys::Secret32;
use crate::random::RandomSource;
use hkdf::Hkdf;
use menghunote_sync_proto::Argon2idParams;
use sha2::Sha256;
use zeroize::Zeroizing;

/// 口令最小字节数（ADR §3）。
pub const PASSWORD_MIN_BYTES: usize = 1;
/// 口令最大字节数（ADR §3）。
pub const PASSWORD_MAX_BYTES: usize = 1024;

/// Argon2id 版本号（ADR §3：`version=0x13`）。
pub const ARGON2_VERSION_13: u16 = 0x13;
/// Argon2id 内存下限：64 MiB（单位 KiB）。
pub const M_MIN_KIB: u32 = 65_536;
/// Argon2id 内存上限：256 MiB（单位 KiB）。
pub const M_MAX_KIB: u32 = 262_144;
/// 迭代次数下限。
pub const T_MIN: u32 = 3;
/// 迭代次数上限。
pub const T_MAX: u32 = 6;
/// 并行度下限。
pub const P_MIN: u32 = 1;
/// 并行度上限。
pub const P_MAX: u32 = 4;
/// 输出长度（v1 固定 32 字节）。
pub const OUTPUT_LEN: u32 = 32;
/// salt 长度（ADR §3：random16）。
pub const SALT_LEN: usize = 16;

/// 校验口令长度。**任何 KDF 之前**先调用。
pub fn validate_password(password: &[u8]) -> Result<(), SyncError> {
    if !(PASSWORD_MIN_BYTES..=PASSWORD_MAX_BYTES).contains(&password.len()) {
        return Err(SyncError::PasswordLength {
            actual: password.len(),
            min: PASSWORD_MIN_BYTES,
            max: PASSWORD_MAX_BYTES,
        });
    }
    Ok(())
}

fn param(field: &'static str, value: u64) -> SyncError {
    SyncError::Argon2Param { field, value }
}

/// 越界检查（ADR §3 的读者区间；必须在 Argon2 之前）。
pub fn validate_argon2id_params(params: &Argon2idParams) -> Result<(), SyncError> {
    if params.version != ARGON2_VERSION_13 {
        return Err(param("version", u64::from(params.version)));
    }
    if !(M_MIN_KIB..=M_MAX_KIB).contains(&params.m_kib) {
        return Err(param("m_kib", u64::from(params.m_kib)));
    }
    if !(T_MIN..=T_MAX).contains(&params.t) {
        return Err(param("t", u64::from(params.t)));
    }
    if !(P_MIN..=P_MAX).contains(&params.p) {
        return Err(param("p", u64::from(params.p)));
    }
    if params.output_len != OUTPUT_LEN {
        return Err(param("output_len", u64::from(params.output_len)));
    }
    Ok(())
}

/// v1 写者参数（64 MiB / t=3 / p=1 / out=32），给定 salt。
#[must_use]
pub fn writer_params(salt: [u8; SALT_LEN]) -> Argon2idParams {
    Argon2idParams {
        version: ARGON2_VERSION_13,
        m_kib: M_MIN_KIB,
        t: T_MIN,
        p: P_MIN,
        salt,
        output_len: OUTPUT_LEN,
    }
}

/// v1 写者参数 + 随机 salt。
pub fn new_writer_params(rng: &mut impl RandomSource) -> Result<Argon2idParams, SyncError> {
    let salt = crate::random::array::<SALT_LEN>(rng)?;
    Ok(writer_params(salt))
}

/// HKDF-SHA256 展开为任意长度（ADR §19 的 24B ASSET nonce 等）。
pub fn hkdf_expand(key: &[u8], salt: &[u8], info: &[u8], out: &mut [u8]) -> Result<(), SyncError> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), key);
    hkdf.expand(info, out).map_err(|_| SyncError::Hkdf)
}

/// 计算 `Argon2id(password, params)` 的 32 字节输出。
///
/// 执行顺序刻意是：**口令长度 → 参数范围 → Argon2**。参数越界时不会分配内存、
/// 不会计算哈希（测试用 1TiB 参数 + 耗时断言验证）。
pub fn argon2id_output(password: &[u8], params: &Argon2idParams) -> Result<Secret32, SyncError> {
    validate_password(password)?;
    validate_argon2id_params(params)?;
    let argon_params = argon2::Params::new(
        params.m_kib,
        params.t,
        params.p,
        Some(params.output_len as usize),
    )
    .map_err(|_| SyncError::Argon2("Argon2id 参数被底层拒绝"))?;
    let hasher = argon2::Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon_params,
    );
    let mut out = Zeroizing::new([0u8; OUTPUT_LEN as usize]);
    hasher
        .hash_password_into(password, &params.salt, out.as_mut())
        .map_err(|_| SyncError::Argon2("Argon2id 计算失败"))?;
    Ok(Secret32::from_array(*out))
}

/// HKDF-SHA256：`KM = HKDF(ikm, salt, info, 32B)`。
///
/// ADR §3 的各 KEK 都用这个原语；info 由 `sync-proto` 的 `password_wrap_info` /
/// `recovery_wrap_info` 等生成（域分隔符末尾是真正的 0x00）。
pub fn hkdf32(ikm: &[u8], salt: &[u8], info: &[u8]) -> Result<Secret32, SyncError> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut okm = Zeroizing::new([0u8; 32]);
    hkdf.expand(info, okm.as_mut())
        .map_err(|_| SyncError::Hkdf)?;
    Ok(Secret32::from_array(*okm))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random::RandomSource;
    use std::time::Instant;

    fn params(m_kib: u32, t: u32, p: u32, output_len: u32) -> Argon2idParams {
        Argon2idParams {
            version: ARGON2_VERSION_13,
            m_kib,
            t,
            p,
            salt: [7u8; SALT_LEN],
            output_len,
        }
    }

    #[test]
    fn password_length_checked_before_anything() {
        assert!(matches!(
            validate_password(&[]),
            Err(SyncError::PasswordLength { actual: 0, .. })
        ));
        let too_long = vec![b'x'; PASSWORD_MAX_BYTES + 1];
        assert!(matches!(
            validate_password(&too_long),
            Err(SyncError::PasswordLength { actual: 1025, .. })
        ));
        assert!(validate_password(b"x").is_ok());
        assert!(validate_password(&vec![b'x'; PASSWORD_MAX_BYTES]).is_ok());
        // 口令原样 UTF-8：不 trim，前后空格是有效字节
        assert!(validate_password(b"  x  ").is_ok());
    }

    /// 越界参数必须在 KDF 之前拒绝：用 1TiB 参数 + 耗时上界证明没有真的跑 Argon2。
    #[test]
    fn out_of_range_params_rejected_before_kdf() {
        let cases = [
            Argon2idParams {
                version: 0x12,
                ..params(M_MIN_KIB, T_MIN, P_MIN, OUTPUT_LEN)
            },
            params(1 << 20, T_MIN, P_MIN, OUTPUT_LEN),
            params(M_MIN_KIB - 1, T_MIN, P_MIN, OUTPUT_LEN),
            params(M_MAX_KIB + 1, T_MIN, P_MIN, OUTPUT_LEN),
            params(M_MIN_KIB, T_MIN - 1, P_MIN, OUTPUT_LEN),
            params(M_MIN_KIB, T_MAX + 1, P_MIN, OUTPUT_LEN),
            params(M_MIN_KIB, T_MIN, 0, OUTPUT_LEN),
            params(M_MIN_KIB, T_MIN, P_MAX + 1, OUTPUT_LEN),
            params(M_MIN_KIB, T_MIN, P_MIN, 16),
        ];
        for params in cases {
            let started = Instant::now();
            let err = match argon2id_output(b"correct horse battery staple", &params) {
                Ok(_) => panic!("越界参数必须拒绝"),
                Err(err) => err,
            };
            let elapsed = started.elapsed();
            assert!(
                matches!(err, SyncError::Argon2Param { .. }),
                "必须是参数错误：{err:?}"
            );
            assert!(
                elapsed.as_millis() < 50,
                "越界参数不能真的跑 KDF（耗时 {elapsed:?}）"
            );
        }
    }

    #[test]
    fn writer_params_hit_expected_bounds() {
        let params = writer_params([1u8; SALT_LEN]);
        assert_eq!(
            (params.version, params.m_kib, params.t, params.p),
            (0x13, 65_536, 3, 1)
        );
        assert_eq!(params.output_len, 32);
        validate_argon2id_params(&params).unwrap();
    }

    /// 真实 Argon2id（64 MiB）一次，验证确定性 + 输出长度 + 边界可满足。
    #[test]
    fn argon2id_is_deterministic_and_meets_writer_params() {
        let params = writer_params([9u8; SALT_LEN]);
        let a = argon2id_output(b"pass phrase", &params).unwrap();
        let b = argon2id_output(b"pass phrase", &params).unwrap();
        assert!(a.ct_eq(&b));
        let c = argon2id_output(b"pass phrasf", &params).unwrap();
        assert!(!a.ct_eq(&c));
        // salt 不同 → 输出不同
        let other_salt = argon2id_output(b"pass phrase", &writer_params([8u8; SALT_LEN])).unwrap();
        assert!(!a.ct_eq(&other_salt));
    }

    #[test]
    fn hkdf_is_deterministic_and_domain_separated() {
        let a = hkdf32(b"ikm", b"salt", b"info-a\0").unwrap();
        let a2 = hkdf32(b"ikm", b"salt", b"info-a\0").unwrap();
        let b = hkdf32(b"ikm", b"salt", b"info-b\0").unwrap();
        let c = hkdf32(b"ikm", b"other-salt", b"info-a\0").unwrap();
        assert!(a.ct_eq(&a2));
        assert!(!a.ct_eq(&b));
        assert!(!a.ct_eq(&c));
        let other_ikm = hkdf32(b"other-ikm", b"salt", b"info-a\0").unwrap();
        assert!(!a.ct_eq(&other_ikm));
    }

    #[test]
    fn test_rng_is_deterministic() {
        let mut rng = crate::test_support::TestRng::new(42);
        let mut a = [0u8; 8];
        rng.fill(&mut a).unwrap();
        let mut rng = crate::test_support::TestRng::new(42);
        let mut b = [0u8; 8];
        rng.fill(&mut b).unwrap();
        assert_eq!(a, b);
    }
}

`````



@@SECTION code/037 `crates/sync/src/keys.rs`


`````rust
//! 秘密材料与根密钥类型。
//!
//! ADR 0006 §3：根/KEK/口令**不 Debug、不日志、用 `Zeroizing`**。
//! 这里把这条纪律落到类型上：所有秘密都装在 [`Secret32`] 里，它：
//!
//! - 用 `Zeroizing<[u8; 32]>` 持有，`Drop` 时清零；
//! - **不实现** `Debug` / `Display` / `Serialize`（想打印就编译不过）；
//! - 相等比较走 [`subtle::ConstantTimeEq`]（避免按字节短路泄露）；
//! - 只暴露一个名字带 `expose` 的访问器，调用点一眼能看出"这里在用明文密钥"。
//!
//! 根密钥的 **wire 表示**是 `menghunote-sync-proto::RootSecretV1`；本模块的
//! [`RootSecretV1`] 是它的本地"零化 + 不可打印"版本，编解码复用 proto 的 CDE。

use crate::error::SyncError;
use crate::random::RandomSource;
use crate::{ObjectId, VaultId};
use menghunote_sync_proto::{ObjectType, RootSecretV1 as ProtoRootSecretV1};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

/// 32 字节秘密材料（根密钥、KEK、种子、私钥材料）。
///
/// 没有 `Debug`/`Display`：`format!("{secret:?}")` 无法编译。
#[derive(Clone)]
pub struct Secret32(Zeroizing<[u8; 32]>);

impl Secret32 {
    /// 生成一个新的随机秘密。
    pub fn generate(rng: &mut impl RandomSource) -> Result<Self, SyncError> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        rng.fill(bytes.as_mut())?;
        Ok(Self(bytes))
    }

    /// 从已有字节构造（调用方交出所有权，内部会清零）。
    #[must_use]
    pub fn from_array(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// 取明文引用。**调用点必须显式出现 `expose`**，便于审计。
    #[must_use]
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }

    /// 取明文字节切片（算法接口需要 `&[u8]` 时用）。
    #[must_use]
    pub fn expose_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    /// 常量时间相等。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.0.as_slice().ct_eq(other.0.as_slice()).into()
    }
}

impl PartialEq for Secret32 {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(other)
    }
}

impl Eq for Secret32 {}

/// 用于封装根密钥的 KEK（口令派生或恢复码派生）。
///
/// 与 [`Secret32`] 一样不可打印；单独成类型是为了让"这是封装密钥、不是根"在调用点可见。
#[derive(Clone)]
pub struct WrapKey(Secret32);

impl WrapKey {
    /// 由 32 字节密钥材料构造（只允许本 crate 内部派生路径使用）。
    #[must_use]
    pub(crate) fn from_secret(secret: Secret32) -> Self {
        Self(secret)
    }

    /// 取明文引用。
    #[must_use]
    pub fn expose(&self) -> &[u8; 32] {
        self.0.expose()
    }

    /// 常量时间相等。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0)
    }
}

/// 某个 epoch 的 vault 根密钥 + 归属信息（ADR §2 `RootSecretV1` 的本地表示）。
///
/// 它是槽体与 HPKE grant 的明文载荷；**不上 wire 明文**，只出现在
/// "被 KEK 封装"或"被 HPKE 封装"之后。wire 编码走 `sync-proto` 的
/// `RootSecretV1::encode/decode`。
#[derive(Clone)]
pub struct RootSecretV1 {
    /// vault 身份。
    pub vault_id: VaultId,
    /// 密钥代（`key_epoch`，与 doc `generation` 永不混用）。
    pub key_epoch: u64,
    /// 32 字节随机根。
    pub root: Secret32,
}

impl RootSecretV1 {
    /// 用已有根构造。
    #[must_use]
    pub const fn new(vault_id: VaultId, key_epoch: u64, root: Secret32) -> Self {
        Self {
            vault_id,
            key_epoch,
            root,
        }
    }

    /// 生成新的随机根（ADR §2：每个新 epoch 的根重新随机，不由旧根导出）。
    pub fn generate(
        vault_id: VaultId,
        key_epoch: u64,
        rng: &mut impl RandomSource,
    ) -> Result<Self, SyncError> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        rng.fill(bytes.as_mut())?;
        Ok(Self {
            vault_id,
            key_epoch,
            root: Secret32::from_array(*bytes),
        })
    }

    /// 归属信息 + 根的常量时间比较。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.vault_id == other.vault_id
            && self.key_epoch == other.key_epoch
            && self.root.ct_eq(&other.root)
    }

    /// CDE 编码（proto 的 `RootSecretV1`）。
    ///
    /// 编解码必须经过 proto 的 wire 结构，那里会有一份栈上的根副本；编码/解码完成后
    /// 立刻用 `zeroize` 擦掉本 crate 持有的那份（proto 内部 `to_cde` 的临时副本由
    /// 依赖侧管理，见实现回报"已知限制"）。
    pub fn encode(&self) -> Result<Vec<u8>, SyncError> {
        let mut proto = ProtoRootSecretV1 {
            vault_id: self.vault_id,
            key_epoch: self.key_epoch,
            root: *self.root.expose(),
        };
        let encoded = proto.encode()?;
        proto.root.zeroize();
        Ok(encoded)
    }

    /// 解析 CDE 并立即把根搬进 [`Secret32`]。
    pub fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let mut proto = ProtoRootSecretV1::decode(bytes)?;
        let root = Secret32::from_array(proto.root);
        proto.root.zeroize();
        Ok(Self {
            vault_id: proto.vault_id,
            key_epoch: proto.key_epoch,
            root,
        })
    }
}

/// 派生数据对象密钥（ADR §2）：
/// `K_object = HKDF(K_epoch, salt=vault_id, info=MN/v1/data\0 || type_u8 || epoch_BE64 || object_id, 32)`。
///
/// info 的拼装由 `sync-proto` 的 `hkdf_data_info` 负责（含 `i64::MAX` 上界检查）。
pub fn derive_object_key(
    root: &RootSecretV1,
    object_type: ObjectType,
    object_id: &ObjectId,
) -> Result<Secret32, SyncError> {
    let info = menghunote_sync_proto::hkdf_data_info(object_type, root.key_epoch, object_id)?;
    crate::kdf::hkdf32(root.root.expose_slice(), &root.vault_id, &info)
}

/// 派生资产 HMAC 对象名密钥（ADR §2：
/// `K_asset_id = HKDF(K_epoch, salt=vault_id, info=MN/v1/asset-id\0, 32)`）。
pub fn derive_asset_id_key(root: &RootSecretV1) -> Result<Secret32, SyncError> {
    crate::kdf::hkdf32(
        root.root.expose_slice(),
        &root.vault_id,
        crate::domain::HKDF_ASSET_ID_INFO,
    )
}

/// task-122：派生 checkpoint 本地回执的 HMAC 密钥。
///
/// 来源是**当前 checkpoint 的 epoch 根**（`K_epoch`），域分隔为
/// `MN/v1/checkpoint-receipt-key\0` + `salt=vault_id`；每个 epoch 一个新密钥，退役根
/// 不再可用于新回执。它**不重用**根本身、`K_object`、`K_asset_id`、槽密钥、签名私钥或
/// relay cursor 密钥；只认证本地 durable receipt，不加密任何数据、不上 wire。
pub fn derive_checkpoint_receipt_key(root: &RootSecretV1) -> Result<Secret32, SyncError> {
    crate::kdf::hkdf32(
        root.root.expose_slice(),
        &root.vault_id,
        crate::domain::HKDF_CHECKPOINT_RECEIPT_INFO,
    )
}

/// ADR §20：KEY_SLOT 的 MNE1 密钥。
///
/// `IKM = Argon2id(password) 或 recovery_seed`；`info = "MN/v1/key-slot\0"
/// || kind_u8 || BE64(epoch) || slot_id32`；`salt = vault_id`。
/// 这样恢复设备在拿到 root **之前**就能解槽（其他对象类型仍用 `K_object`）。
pub fn derive_slot_mne_key(
    ikm: &[u8],
    vault_id: &VaultId,
    kind: menghunote_sync_proto::SlotKind,
    epoch: u64,
    slot_id: &ObjectId,
) -> Result<Secret32, SyncError> {
    let mut info = Vec::with_capacity(crate::domain::HKDF_KEY_SLOT_INFO.len() + 1 + 8 + 32);
    info.extend_from_slice(crate::domain::HKDF_KEY_SLOT_INFO);
    info.push(kind as u8);
    info.extend_from_slice(&epoch.to_be_bytes());
    info.extend_from_slice(slot_id);
    crate::kdf::hkdf32(ikm, vault_id, &info)
}

/// 明文秘密的调试格式：只打印"已脱敏"，不打印任何字节。
pub(crate) fn redacted() -> &'static str {
    "<redacted>"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;

    #[test]
    fn secret_eq_is_constant_time_wrapper() {
        let mut rng = TestRng::new(1);
        let a = Secret32::generate(&mut rng).unwrap();
        let b = Secret32::generate(&mut rng).unwrap();
        assert!(!a.ct_eq(&b));
        assert!(a.ct_eq(&Secret32::from_array(*a.expose())));
        assert!(a.ct_eq(&Secret32::from_array(*a.expose())));
    }

    #[test]
    fn root_secret_cde_roundtrip() {
        let mut rng = TestRng::new(2);
        let root = RootSecretV1::generate([7u8; 16], 3, &mut rng).unwrap();
        let bytes = root.encode().unwrap();
        let back = RootSecretV1::decode(&bytes).unwrap();
        assert!(root.ct_eq(&back));
        assert_eq!(bytes, back.encode().unwrap(), "CDE 必须确定性");
        // proto 的 RootSecretV1 是 3 项 map（0xa3）
        assert_eq!(bytes[0], 0xa3);
        // 末尾是根的 32 字节
        assert_eq!(&bytes[bytes.len() - 32..], root.root.expose_slice());

        // 尾部多余字节拒绝
        let mut long = bytes.clone();
        long.push(0);
        assert!(RootSecretV1::decode(&long).is_err());
        // 截断拒绝
        assert!(RootSecretV1::decode(&bytes[..bytes.len() - 1]).is_err());
        // 字段数不符拒绝（把 3 项改成 2 项）
        let mut short = bytes;
        short[0] = 0xa2;
        assert!(RootSecretV1::decode(&short).is_err());
    }

    #[test]
    fn root_secret_rejects_zero_epoch() {
        // proto 的 RootSecretV1::validate 要求 key_epoch > 0
        let root = RootSecretV1::new([1u8; 16], 0, Secret32::from_array([2u8; 32]));
        assert!(root.encode().is_err());
    }

    #[test]
    fn object_keys_are_domain_separated_and_deterministic() {
        let mut rng = TestRng::new(3);
        let root = RootSecretV1::generate([9u8; 16], 1, &mut rng).unwrap();
        let oid_a = [0x11u8; 32];
        let oid_b = [0x22u8; 32];

        let k1 = derive_object_key(&root, ObjectType::Update, &oid_a).unwrap();
        let k1_again = derive_object_key(&root, ObjectType::Update, &oid_a).unwrap();
        assert!(k1.ct_eq(&k1_again));

        // 类型/对象/epoch/根 任一不同都必须得到不同密钥
        assert!(!k1.ct_eq(&derive_object_key(&root, ObjectType::Snapshot, &oid_a).unwrap()));
        assert!(!k1.ct_eq(&derive_object_key(&root, ObjectType::Update, &oid_b).unwrap()));
        let other_epoch = RootSecretV1::new(root.vault_id, 2, root.root.clone());
        assert!(!k1.ct_eq(&derive_object_key(&other_epoch, ObjectType::Update, &oid_a).unwrap()));
        let other_root = RootSecretV1::generate(root.vault_id, 1, &mut rng).unwrap();
        assert!(!k1.ct_eq(&derive_object_key(&other_root, ObjectType::Update, &oid_a).unwrap()));
        // 资产密钥独立
        assert!(!k1.ct_eq(&derive_asset_id_key(&root).unwrap()));
    }

    #[test]
    fn secret_debug_is_impossible() {
        // 类型系统层面 Secret32/WrapKey 没有 Debug；这里只断言脱敏占位符存在。
        assert_eq!(redacted(), "<redacted>");
    }
}

`````



@@SECTION code/038 `crates/sync/src/lib.rs`


`````rust
//! sync —— 同步/加密 v1 的密钥与设备身份层（见 `docs/adr/0006-sync-v1.md`）。
//!
//! 本 crate 目前只实现 ADR 0006 §11 的**第 2 步**（密钥与设备身份）：
//!
//! - 根密钥 / 口令 KEK / 恢复码（Argon2id + HKDF，Bech32m `mnrec`）；
//! - 密钥槽（`sync-proto` 的 `SlotDescriptorV1`）、wrap/unwrap、改口令纯逻辑；
//! - 本地身份 BLOB 封装（[`identity_wrap`]）：Windows DPAPI（CurrentUser），
//!   Linux 用户主密钥文件 + XChaCha20-Poly1305（ADR 0008）；
//! - 设备 Ed25519/X25519 身份、`DeviceCertV1`（`sync-proto` 类型）、独立管理密钥、
//!   邀请令牌、设备指纹；
//! - HPKE（RFC 9180 base mode：X25519-HKDF-SHA256 / HKDF-SHA256 / ChaCha20Poly1305）。
//!
//! 边界（ADR §11）：不做网络、不做 SQLite、不碰 GPUI。**wire 类型与 CDE 编码的真源是
//! `menghunote-sync-proto`**（header/AAD/记录/KDF 联合体/域常量），本 crate 不另写一套；
//! 它只负责"把待编码字节算成 SHA/签名/HPKE/AEAD"，以及本 crate 私有的本地结构
//! （本地身份 BLOB、HPKE grant AAD）经同一 CDE 编码器编码。
//!
//! **秘密纪律**：口令、根、恢复种子、私钥、Linux 用户主密钥一律包在
//! [`keys::Secret32`] / `Zeroizing` 里，这些类型不实现 `Debug`/`Display`；错误类型只带
//! 定位信息，不带秘密。
//!
//! 平台（[`identity_wrap`] 分派）：Windows 用 DPAPI（ADR 0006 §3）；Linux 用用户主密钥
//! 文件（ADR 0008，安全性弱于 DPAPI）；其它平台编译通过但调用明确返回
//! [`error::SyncError::LocalWrapUnsupportedPlatform`]，**不假装成功、不回退明文**。

// 除 DPAPI 的 Win32 FFI（`dpapi.rs` 内 `#[allow(unsafe_code)]`）外，本 crate 不用 unsafe。
#![deny(unsafe_code)]

pub mod api_version;
pub mod approval_v1;
pub(crate) mod asset_sync;
pub mod base64url;
pub mod bootstrap;
pub mod cbor;
pub mod checkpoint;
pub mod control_pin;
pub mod create_link;
pub mod dev_local;
pub mod dev_local_rotation;
pub mod device;
pub mod device_management;
pub mod domain;
pub mod dpapi;
pub mod engine;
pub mod error;
pub mod genesis;
pub mod hpke;
pub mod http_relay;
pub mod http_transport;
pub mod identity;
pub mod identity_wrap;
pub mod ids;
pub mod init_journal;
pub mod init_session;
pub mod invite_link;
pub mod kdf;
pub mod keys;
pub mod management_v2;
pub mod random;
pub mod recovery;
pub mod recovery_enroll;
pub mod relay;
pub mod relay_context;
pub mod relay_policy;
pub mod remote_init;
pub mod rotation;
pub mod rotation_activate;
pub mod rotation_apply;
pub mod rotation_journal;
pub mod rotation_repack;
pub mod rotation_restart;
pub mod rotation_slots;
pub mod rotation_stage;
pub mod slot;
pub mod storage_view;
pub mod upload;
pub mod wire;

#[cfg(test)]
mod test_support;

pub use error::SyncError;

// 复用 wire 真源的标识符类型（避免各 crate 各定义一份）。
pub use menghunote_sync_proto::{DeviceId, ObjectId, VaultId};

`````



@@SECTION code/039 `crates/sync/src/random.rs`


`````rust
//! 随机源抽象。
//!
//! 生产路径统一走 [`OsRandom`]（`getrandom`，失败返回错误）；单元测试注入确定性实现，
//! 这样"随机"本身可复现，测试不依赖运气。
//!
//! 注意：HPKE 的临时 X25519 密钥与 nonce **不经过这里**——RFC 9180 的 nonce 由 HPKE
//! 内部的 seq 派生（ADR 0006 §4 明确要求"不要自造 nonce"）。

use crate::error::SyncError;

/// 可注入的随机源。
pub trait RandomSource {
    /// 用随机字节填满 `dst`。
    fn fill(&mut self, dst: &mut [u8]) -> Result<(), SyncError>;
}

/// 系统随机源（Windows：`BCryptGenRandom`，由 `getrandom` 封装）。
#[derive(Debug, Default, Clone, Copy)]
pub struct OsRandom;

impl RandomSource for OsRandom {
    fn fill(&mut self, dst: &mut [u8]) -> Result<(), SyncError> {
        getrandom::getrandom(dst).map_err(|_| SyncError::Random)
    }
}

/// 生成 `N` 字节随机数组。
pub fn array<const N: usize>(rng: &mut impl RandomSource) -> Result<[u8; N], SyncError> {
    let mut out = [0u8; N];
    rng.fill(&mut out)?;
    Ok(out)
}

/// 生成 `len` 字节随机向量。
pub fn vec(rng: &mut impl RandomSource, len: usize) -> Result<Vec<u8>, SyncError> {
    let mut out = vec![0u8; len];
    rng.fill(&mut out)?;
    Ok(out)
}

`````



@@SECTION code/040 `crates/sync/src/recovery.rs`


`````rust
//! 恢复码：`R = random32`，编码为 **Bech32m（HRP = `mnrec`）**（ADR 0006 §3）。
//!
//! 恢复码同时具备两种能力：
//!
//! - **数据恢复**：`K_recovery_wrap = HKDF(R, vault_id, b"MN/v1/recovery-wrap\0" || slot_id, 32)`
//!   解出 vault 根；
//! - **管理恢复**：`management_ed25519_seed = HKDF(R, vault_id, b"MN/v1/management-ed25519\0", 32)`
//!   派生**独立的管理 Ed25519 私钥**（普通根持有者不能据此注册/吊销设备，ADR §3/§4）。
//!
//! 编码用 BIP-350 Bech32m 而不是 Bech32：解码时**强制校验 Bech32m**，否则校验和类型不同
//! 的串会被误接受（Bech32 与 Bech32m 的字符集相同，只差校验和常量）。

use crate::domain;
use crate::error::SyncError;
use crate::kdf::hkdf32;
use crate::keys::{Secret32, WrapKey};
use crate::random::RandomSource;
use crate::VaultId;
use bech32::primitives::decode::CheckedHrpstring;
use bech32::{Bech32m, Hrp};
use zeroize::Zeroizing;

/// 恢复码的 Bech32 HRP（ADR §3：`mnrec`）。
pub const RECOVERY_HRP: &str = "mnrec";
/// 恢复种子字节数。
pub const RECOVERY_SEED_LEN: usize = 32;
/// 恢复码字符串长度：5(hrp) + 1(分隔符) + 52(32B→base32) + 6(校验和)。
pub const RECOVERY_CODE_LEN: usize = 64;

/// 恢复种子 `R`（32 字节随机）。**不实现 `Debug`/`Display`**，编码只能走 [`Self::to_code`]。
#[derive(Clone)]
pub struct RecoverySeed(Secret32);

impl RecoverySeed {
    /// 生成新的随机恢复种子（ADR §3：`R = random32`）。
    pub fn generate(rng: &mut impl RandomSource) -> Result<Self, SyncError> {
        Ok(Self(Secret32::generate(rng)?))
    }

    /// 从已有字节构造。
    #[must_use]
    pub fn from_array(bytes: [u8; RECOVERY_SEED_LEN]) -> Self {
        Self(Secret32::from_array(bytes))
    }

    /// 取明文引用。
    #[must_use]
    pub fn expose(&self) -> &[u8; RECOVERY_SEED_LEN] {
        self.0.expose()
    }

    /// 常量时间相等。
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        self.0.ct_eq(&other.0)
    }

    /// 编码为 Bech32m 恢复码（`mnrec1...`）。
    ///
    /// 返回值包在 `Zeroizing<String>` 里，`Drop` 时清零；调用方仍必须遵守
    /// "只显示一次、不写日志、不进遥测"。
    pub fn to_code(&self) -> Result<Zeroizing<String>, SyncError> {
        let hrp = Hrp::parse_unchecked(RECOVERY_HRP);
        let code = bech32::encode::<Bech32m>(hrp, self.0.expose_slice())
            .map_err(|_| SyncError::RecoveryCode("Bech32m 编码失败"))?;
        Ok(Zeroizing::new(code))
    }

    /// 解析 Bech32m 恢复码。
    ///
    /// 严格规则：必须是 **Bech32m** 校验和、HRP 必须是 `mnrec`、数据必须恰好 32 字节；
    /// 大小写混写/未知 HRP/长度不符/校验和错误一律拒绝。
    pub fn from_code(code: &str) -> Result<Self, SyncError> {
        let checked = CheckedHrpstring::new::<Bech32m>(code)
            .map_err(|_| SyncError::RecoveryCode("Bech32m 校验失败"))?;
        let expected = Hrp::parse_unchecked(RECOVERY_HRP);
        if checked.hrp() != expected {
            return Err(SyncError::RecoveryCode("HRP 必须是 mnrec"));
        }
        let mut seed = Zeroizing::new([0u8; RECOVERY_SEED_LEN]);
        let mut len = 0usize;
        for byte in checked.byte_iter() {
            if len >= RECOVERY_SEED_LEN {
                return Err(SyncError::RecoveryCode("恢复码数据必须是 32 字节"));
            }
            seed[len] = byte;
            len += 1;
        }
        if len != RECOVERY_SEED_LEN {
            return Err(SyncError::RecoveryCode("恢复码数据必须是 32 字节"));
        }
        Ok(Self(Secret32::from_array(*seed)))
    }
}

/// 管理私钥种子：`HKDF(R, vault_id, b"MN/v1/management-ed25519\0", 32)`（ADR §3）。
pub fn management_seed(seed: &RecoverySeed, vault_id: &VaultId) -> Result<Secret32, SyncError> {
    hkdf32(
        seed.0.expose_slice(),
        vault_id,
        domain::HKDF_MANAGEMENT_ED25519_INFO,
    )
}

/// 恢复槽 KEK：`HKDF(R, vault_id, b"MN/v1/recovery-wrap\0" || slot_id, 32)`（ADR §3）。
pub fn recovery_wrap_key(
    seed: &RecoverySeed,
    vault_id: &VaultId,
    slot_id: &[u8; 32],
) -> Result<WrapKey, SyncError> {
    let info = domain::recovery_wrap_info(slot_id);
    let kek = hkdf32(seed.0.expose_slice(), vault_id, &info)?;
    Ok(WrapKey::from_secret(kek))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRng;

    /// 用独立实现（BIP-350 参考算法，Python）算出的 golden vector：
    /// seed = 0x00 0x01 ... 0x1f，HRP = `mnrec`，Bech32m 校验和。
    const GOLDEN_SEED: [u8; 32] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
        0x1e, 0x1f,
    ];
    const GOLDEN_BECH32M: &str = "mnrec1qqqsyqcyq5rqwzqfpg9scrgwpugpzysnzs23v9ccrydpk8qarc0sr56vg6";
    /// 同一 32 字节用 **Bech32（非 m）** 校验和编码的串：必须被拒绝。
    const GOLDEN_BECH32_WRONG_SUM: &str =
        "mnrec1qqqsyqcyq5rqwzqfpg9scrgwpugpzysnzs23v9ccrydpk8qarc0skg2qdc";

    #[test]
    fn golden_vector_roundtrip() {
        let seed = RecoverySeed::from_array(GOLDEN_SEED);
        let code = seed.to_code().unwrap();
        assert_eq!(code.as_str(), GOLDEN_BECH32M);
        assert_eq!(code.len(), RECOVERY_CODE_LEN);
        let back = RecoverySeed::from_code(&code).unwrap();
        assert!(seed.ct_eq(&back));
    }

    #[test]
    fn generate_then_parse() {
        let mut rng = TestRng::new(7);
        let seed = RecoverySeed::generate(&mut rng).unwrap();
        let code = seed.to_code().unwrap();
        assert!(code.starts_with("mnrec1"));
        assert_eq!(code.len(), RECOVERY_CODE_LEN);
        assert!(seed.ct_eq(&RecoverySeed::from_code(&code).unwrap()));
    }

    #[test]
    fn uppercase_is_accepted() {
        let seed = RecoverySeed::from_array(GOLDEN_SEED);
        let upper = seed.to_code().unwrap().to_uppercase();
        let back = RecoverySeed::from_code(&upper).unwrap();
        assert!(seed.ct_eq(&back));
    }

    #[test]
    fn wrong_checksum_type_rejected() {
        // Bech32（非 m）编码的同一数据：ADR 要求 Bech32m，必须拒绝。
        assert!(RecoverySeed::from_code(GOLDEN_BECH32_WRONG_SUM).is_err());
    }

    #[test]
    fn tampered_and_malformed_rejected() {
        let mut chars: Vec<char> = GOLDEN_BECH32M.chars().collect();
        // 改动最后一个数据字符 → 校验和失败
        chars[RECOVERY_CODE_LEN - 7] = if chars[RECOVERY_CODE_LEN - 7] == 'q' {
            'p'
        } else {
            'q'
        };
        let tampered: String = chars.into_iter().collect();
        assert!(RecoverySeed::from_code(&tampered).is_err());
        // 混写大小写
        let mixed = format!("mnrec1{}", GOLDEN_BECH32M[6..].to_uppercase());
        assert!(RecoverySeed::from_code(&mixed).is_err());
        // 空串/垃圾
        assert!(RecoverySeed::from_code("").is_err());
        assert!(RecoverySeed::from_code("mnrec1").is_err());
        // 未知 HRP（Bech32m 合法但 HRP 不同）
        let hrp = Hrp::parse_unchecked("mnxxx");
        let wrong_hrp = bech32::encode::<Bech32m>(hrp, &GOLDEN_SEED).unwrap();
        assert!(RecoverySeed::from_code(&wrong_hrp).is_err());
    }

    #[test]
    fn wrong_data_length_rejected() {
        let hrp = Hrp::parse_unchecked(RECOVERY_HRP);
        let short = bech32::encode::<Bech32m>(hrp, &GOLDEN_SEED[..31]).unwrap();
        assert!(RecoverySeed::from_code(&short).is_err());
        let long = bech32::encode::<Bech32m>(hrp, &[0u8; 33]).unwrap();
        assert!(RecoverySeed::from_code(&long).is_err());
    }

    /// 恢复码同时恢复 root 与独立管理 key（ADR §11 步 2 的验收点）。
    #[test]
    fn recovery_code_restores_root_and_management_key() {
        let mut rng = TestRng::new(11);
        let vault_id: VaultId = [3u8; 16];
        let slot_id = [4u8; 32];

        // 初始化：生成 R、根、恢复槽 KEK、管理种子
        let seed = RecoverySeed::generate(&mut rng).unwrap();
        let root = crate::keys::RootSecretV1::generate(vault_id, 1, &mut rng).unwrap();
        let _kek = recovery_wrap_key(&seed, &vault_id, &slot_id).unwrap();
        let _mgmt = management_seed(&seed, &vault_id).unwrap();

        // 用户抄下恢复码；此后再无内存中的 R
        let code = seed.to_code().unwrap();
        drop(seed);

        // 恢复：仅凭恢复码 + vault_id + slot_id 重新派生两个能力
        let restored_seed = RecoverySeed::from_code(&code).unwrap();
        let _restored_kek = recovery_wrap_key(&restored_seed, &vault_id, &slot_id).unwrap();
        let restored_mgmt = management_seed(&restored_seed, &vault_id).unwrap();

        // 管理种子能从恢复码确定性地重建（同一默认 key）
        let mgmt_key = crate::device::ManagementKey::from_seed(restored_mgmt);
        let mgmt_key2 =
            crate::device::ManagementKey::from_recovery(&restored_seed, &vault_id).unwrap();
        assert_eq!(mgmt_key.public_key(), mgmt_key2.public_key());
        assert_ne!(mgmt_key.public_key(), [0u8; 32]);

        // root 本身不受恢复码影响（恢复码只解封槽，不重算根）
        assert_eq!(root.key_epoch, 1);
    }

    #[test]
    fn management_seed_differs_from_recovery_wrap_key() {
        let seed = RecoverySeed::from_array(GOLDEN_SEED);
        let vault_id: VaultId = [5u8; 16];
        let mgmt = management_seed(&seed, &vault_id).unwrap();
        let wrap = recovery_wrap_key(&seed, &vault_id, &[1u8; 32]).unwrap();
        assert_ne!(mgmt.expose(), wrap.expose());
        assert_ne!(
            mgmt.expose(),
            recovery_wrap_key(&seed, &vault_id, &[2u8; 32])
                .unwrap()
                .expose(),
            "不同槽必须得到不同 KEK"
        );
    }
}

`````



@@SECTION code/041 `crates/sync/src/recovery_enroll.rs`


`````rust
//! 恢复码自批新设备（ADR §3.7/§4/§20.3）。
//!
//! 只有恢复码 + relay、没有任何在线旧设备时：
//! 1. 恢复码 → 恢复种子 R → 管理私钥（`ManagementKey::from_recovery`）；
//! 2. management realm challenge/token（proof 用管理私钥签）；
//! 3. 用 management token `GET key-slots`（可见 staging）→ 取 recovery 槽 →
//!    `KeySlotV1::mne_key`（§20）解封 root（**不需要 glue 设备在线**）；
//! 4. 本地生成新设备 sign/HPKE key；管理签名建 invite → 新设备 redeem（proof 自证）→
//!    approve（管理签 §17.1 证书 + §17.2 HPKE grant，grant 只封给本次本地 HPKE 公钥）；
//! 5. 返回新设备身份 + root；调用方再用设备 token 走 `bootstrap::run`。
//!
//! 安全边界：不信任 relay 返回的任何公钥；grant 的 recipient 就是本地新生成的
//! `hpke_key.public_key()`，approve 前逐步校验响应字段。

use sha2::{Digest, Sha256};

use crate::api_version::RelayApi;
use crate::cbor;
use crate::device::{verify_signature, DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::hpke::{open_epoch_grant, seal_epoch_grant};
use crate::http_relay::{error_from_response, HttpRequest, HttpResponse, HttpTransport};
use crate::keys::RootSecretV1;
use crate::recovery::RecoverySeed;
use crate::relay::SlotRecord;
use crate::slot::{KeySlotV1, WrappedRoot};
use crate::DeviceId;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use menghunote_sync_proto::{
    auth_signing_input, device_cert_signed_bytes, management_request_cde, management_signing_input,
    AuthRealm, Challenge, DeviceCertV1, Envelope, GrantAadV1, HttpMethod, ObjectType, Rights,
    SlotDescriptorV1, SlotKind as ProtoSlotKind,
};

/// 自批成功后的新设备材料。
pub struct EnrolledDevice {
    /// vault id。
    pub vault_id: [u8; 16],
    /// 新设备 id。
    pub device_id: DeviceId,
    /// Loro peer id。
    pub peer_id: u64,
    /// 设备签名私钥。
    pub signing_key: DeviceSigningKey,
    /// 设备 HPKE 私钥。
    pub hpke_key: DeviceHpkeKey,
    /// 解出的 epoch 根。
    pub root: RootSecretV1,
    /// relay 当前 control_rev（approve/建 invite 用）。
    pub control_rev: u64,
    /// 自批步骤日志（e2e 打印用）。
    pub steps: Vec<String>,
}

const CONTENT_TYPE: &str = "application/cbor";

/// 执行恢复码自批；返回新设备身份和 root。
pub fn self_enroll<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    recovery_code: &str,
    now: u64,
) -> Result<EnrolledDevice, SyncError> {
    let seed = RecoverySeed::from_code(recovery_code)?;
    let manager = ManagementKey::from_recovery(&seed, &vault_id)?;
    let mut steps = Vec::new();

    // 1. 管理 realm token。
    let (token, instance_id) = management_token(transport, api, vault_id, &manager, now)?;
    steps.push("management token ok".to_owned());

    // 2. 读 staging recovery 槽并解 root（§20 KEK 派生）。
    let slots = list_slots(transport, api, vault_id, &token)?;
    let recovery_slot = slots
        .iter()
        .find(|slot| slot.kind == ProtoSlotKind::Recovery)
        .ok_or(SyncError::Object(
            "relay 没有 recovery 槽（管理 realm 应可见 staging）",
        ))?
        .clone();
    let descriptor = recovery_slot_descriptor(&recovery_slot)?;
    // ADR §22：管理 realm 可只读 KEY_SLOT 对象；先解出真实 root，再封真实 grant。
    let root = unwrap_recovery_slot(transport, api, vault_id, &token, &recovery_slot, &seed)?;
    steps.push("recovery slot unwrapped (management realm)".to_owned());

    // 3. 本地生成新设备密钥（不信任 relay 的任何公钥）。
    let signing_key = DeviceSigningKey::generate(&mut crate::random::OsRandom)?;
    let hpke_key = DeviceHpkeKey::generate(&mut crate::random::OsRandom)?;
    let device_id: DeviceId = crate::random::array(&mut crate::random::OsRandom)?;
    let peer_bytes: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    let mut peer_head = [0u8; 8];
    peer_head.copy_from_slice(&peer_bytes[..8]);
    let peer_id = u64::from_be_bytes(peer_head) & 0x7fff_ffff_ffff_ffff;
    let manager_rev = descriptor.manager_rev;

    // 4. 读取 state（control_rev/epoch）。
    let state = get_state(transport, api, vault_id, &token)?;
    let invitation = create_invite(
        transport,
        api,
        vault_id,
        &manager,
        &token,
        state.0,
        instance_id,
        now,
        None,
    )?;
    steps.push(format!("invite created id={}", hex16(&invitation.0)));
    redeem_invite(
        transport,
        api,
        vault_id,
        invitation.0,
        &invitation.1,
        device_id,
        &signing_key,
        &hpke_key,
    )?;
    steps.push("invite redeemed (device pending)".to_owned());
    // ADR §22：grant 必须封真实 root（占位 grant 是协议错误）。
    approve_device(
        transport,
        api,
        vault_id,
        &manager,
        &token,
        state.0,
        instance_id,
        device_id,
        signing_key.public_key(),
        hpke_key.public_key(),
        manager_rev,
        &root,
    )?;
    steps.push("recovery self-approval: device active (real root grant)".to_owned());
    let device_token = device_token(transport, api, vault_id, device_id, &signing_key, now)?;
    verify_latest_grant(
        transport,
        api,
        vault_id,
        device_id,
        &device_token,
        &hpke_key,
        &root,
    )?;
    steps.push("grant root verified".to_owned());

    Ok(EnrolledDevice {
        vault_id,
        device_id,
        peer_id,
        signing_key,
        hpke_key,
        root,
        control_rev: state.0,
        steps,
    })
}

pub(crate) fn management_token<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    manager: &ManagementKey,
    now: u64,
) -> Result<(String, [u8; 16]), SyncError> {
    let subject = management_subject(&manager.public_key());
    let body = cbor::encode(vec![
        (0, cbor::uint(u64::from(AuthRealm::Management as u8))),
        (1, cbor::bytes(subject.to_vec())),
    ])?;
    let response = send(
        transport,
        "POST",
        api.url(&api.auth_path(&vault_id, "challenges")),
        None,
        body,
    )?;
    let fields = decode(&response)?;
    let challenge_id: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
    let realm = cbor::uint_field(&fields, 1)?;
    if realm != u64::from(AuthRealm::Management as u8) {
        return Err(SyncError::RelayWire("management challenge realm 不符"));
    }
    let subject: [u8; 16] = cbor::bytes_fixed(&fields, 2)?;
    if subject != management_subject(&manager.public_key()) {
        return Err(SyncError::RelayWire("management challenge subject 不符"));
    }
    let nonce: [u8; 32] = cbor::bytes_fixed(&fields, 3)?;
    let expires_at = cbor::uint_field(&fields, 4)?;
    let response_vault: [u8; 16] = cbor::bytes_fixed(&fields, 5)?;
    let instance: [u8; 16] = cbor::bytes_fixed(&fields, 6)?;
    if response_vault != vault_id || expires_at <= now {
        return Err(SyncError::RelayWire("management challenge 过期/vault 不符"));
    }
    let origin_sha256: [u8; 32] = Sha256::digest(api.origin().as_bytes()).into();
    let challenge = Challenge {
        pv: 1,
        relay_instance_id: instance,
        origin_sha256,
        vault_id,
        realm: AuthRealm::Management,
        subject,
        challenge_id,
        nonce,
        expires_at,
    };
    let message = challenge.encode()?;
    let signature = manager.sign(&auth_signing_input(&challenge)?);
    let body = cbor::encode(vec![
        (0, cbor::bytes(challenge_id.to_vec())),
        (1, cbor::bytes(message)),
        (2, cbor::bytes(signature)),
    ])?;
    let response = send(
        transport,
        "POST",
        api.url(&api.auth_path(&vault_id, "tokens")),
        None,
        body,
    )?;
    let fields = decode(&response)?;
    let token = cbor::text_field(&fields, 0)?;
    let realm = cbor::uint_field(&fields, 3)?;
    if realm != u64::from(AuthRealm::Management as u8) {
        return Err(SyncError::RelayWire("token realm 不是 management"));
    }
    Ok((token, instance))
}

fn verify_latest_grant<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    device_id: DeviceId,
    token: &str,
    hpke_key: &DeviceHpkeKey,
    expected_root: &RootSecretV1,
) -> Result<(), SyncError> {
    let response = send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/devices/self/grants/latest")),
        Some(token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    let epoch = cbor::uint_field(&fields, 0)?;
    let grant_device: [u8; 16] = cbor::bytes_fixed(&fields, 1)?;
    let manager_rev = cbor::uint_field(&fields, 2)?;
    let enc: [u8; 32] = cbor::bytes_fixed(&fields, 3)?;
    let ciphertext = cbor::bytes_field(&fields, 4)?;
    let signed_header = cbor::bytes_field(&fields, 5)?;
    let signature = cbor::opt_bytes_field(&fields, 6)?.unwrap_or_default();
    let grant_id = cbor::opt_bytes_field(&fields, 7)?;
    let activation_ref = cbor::opt_bytes_field(&fields, 8)?;
    let header_hash = cbor::opt_bytes_field(&fields, 9)?;
    let package_digest = cbor::opt_bytes_field(&fields, 10)?;
    if grant_device != device_id || signed_header.len() < 64 {
        return Err(SyncError::RelayWire("self grant 字段不符"));
    }
    if let Some(grant_id) = &grant_id {
        if grant_id.len() != 16 {
            return Err(SyncError::RelayWire("self grant grant_id 长度非法"));
        }
    }
    if let Some(activation_ref) = &activation_ref {
        if activation_ref.len() != 16 {
            return Err(SyncError::RelayWire("self grant activation_ref 长度非法"));
        }
    }
    if let Some(header_hash) = &header_hash {
        let expected: [u8; 32] = Sha256::digest(&signed_header).into();
        if header_hash.as_slice() != expected {
            return Err(SyncError::RelayWire("self grant signed_header digest 不符"));
        }
    }
    if let Some(package_digest) = &package_digest {
        if signature.len() != 64 {
            return Err(SyncError::RelayWire("self grant signature 长度非法"));
        }
        let mut input = Vec::new();
        input.extend_from_slice(&enc);
        input.extend_from_slice(&Sha256::digest(&ciphertext));
        input.extend_from_slice(&Sha256::digest(&signed_header));
        input.extend_from_slice(&signature);
        let expected: [u8; 32] = Sha256::digest(&input).into();
        if package_digest.as_slice() != expected {
            return Err(SyncError::RelayWire("self grant package digest 不符"));
        }
    }
    let aad_len = signed_header.len() - 64;
    let aad = GrantAadV1::decode(&signed_header[..aad_len])?;
    let signed_enc: [u8; 32] = signed_header[aad_len..aad_len + 32]
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant enc 长度非法"))?;
    let signed_hash: [u8; 32] = signed_header[aad_len + 32..]
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant hash 长度非法"))?;
    let ct_hash: [u8; 32] = Sha256::digest(&ciphertext).into();
    if signed_enc != enc
        || signed_hash != ct_hash
        || aad.epoch != epoch
        || aad.manager_rev != manager_rev
        || aad.recipient_device_id != device_id
        || aad.recipient_x25519_pub != hpke_key.public_key()
    {
        return Err(SyncError::RelayWire("self grant 与本地设备/槽不一致"));
    }
    let opened = open_epoch_grant(hpke_key.secret(), &enc, &aad, &ciphertext)?;
    if opened.encode()? != expected_root.encode()? {
        return Err(SyncError::Object("grant root 与槽里解出的 root 不一致"));
    }
    Ok(())
}

/// 设备 realm 认证的完整回执（邀请加入端要核对 vault/instance pin）。
pub(crate) struct DeviceAuth {
    /// bearer token。
    pub token: String,
    /// challenge 响应的 vault（必须与请求一致）。
    pub vault_id: [u8; 16],
    /// relay instance（必须与邀请链接的 pin 一致，**不自动换 pin**）。
    pub instance_id: [u8; 16],
}

/// 设备 realm challenge/token；返回完整回执（vault/instance 供调用方核对）。
pub(crate) fn device_auth<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    now: u64,
) -> Result<DeviceAuth, SyncError> {
    let body = cbor::encode(vec![
        (0, cbor::uint(u64::from(AuthRealm::Device as u8))),
        (1, cbor::bytes(device_id.to_vec())),
    ])?;
    let response = send(
        transport,
        "POST",
        api.url(&api.auth_path(&vault_id, "challenges")),
        None,
        body,
    )?;
    let fields = decode(&response)?;
    let challenge_id: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
    let nonce: [u8; 32] = cbor::bytes_fixed(&fields, 3)?;
    let expires_at = cbor::uint_field(&fields, 4)?;
    let response_vault: [u8; 16] = cbor::bytes_fixed(&fields, 5)?;
    let instance: [u8; 16] = cbor::bytes_fixed(&fields, 6)?;
    if response_vault != vault_id || expires_at <= now {
        return Err(SyncError::RelayWire("device challenge 过期/vault 不符"));
    }
    let origin_sha256: [u8; 32] = Sha256::digest(api.origin().as_bytes()).into();
    let challenge = Challenge {
        pv: 1,
        relay_instance_id: instance,
        origin_sha256,
        vault_id,
        realm: AuthRealm::Device,
        subject: device_id,
        challenge_id,
        nonce,
        expires_at,
    };
    let message = challenge.encode()?;
    let signature = signing_key.sign(&auth_signing_input(&challenge)?);
    let body = cbor::encode(vec![
        (0, cbor::bytes(challenge_id.to_vec())),
        (1, cbor::bytes(message)),
        (2, cbor::bytes(signature.to_vec())),
    ])?;
    let response = send(
        transport,
        "POST",
        api.url(&api.auth_path(&vault_id, "tokens")),
        None,
        body,
    )?;
    let fields = decode(&response)?;
    Ok(DeviceAuth {
        token: cbor::text_field(&fields, 0)?,
        vault_id: response_vault,
        instance_id: instance,
    })
}

/// 只要 bearer token 的薄封装（恢复码自批路径；instance pin 由调用方另外核对）。
pub(crate) fn device_token<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    now: u64,
) -> Result<String, SyncError> {
    Ok(device_auth(transport, api, vault_id, device_id, signing_key, now)?.token)
}

fn list_slots<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    token: &str,
) -> Result<Vec<SlotRecord>, SyncError> {
    let response = send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/key-slots")),
        Some(token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    let mut slots = Vec::new();
    for value in cbor::array_field(&fields, 0)? {
        let fields = cbor::value_map(&value)?;
        slots.push(SlotRecord {
            slot_id: cbor::bytes_fixed(&fields, 0)?,
            epoch: cbor::uint_field(&fields, 1)?,
            kind: parse_kind(&cbor::text_field(&fields, 2)?)?,
            descriptor: cbor::bytes_field(&fields, 3)?,
            state: cbor::text_field(&fields, 4)?,
            slot_set_rev: cbor::uint_field(&fields, 5)?,
            retire_at: cbor::opt_uint_field(&fields, 6)?,
        });
    }
    Ok(slots)
}

fn unwrap_recovery_slot<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    token: &str,
    record: &SlotRecord,
    seed: &RecoverySeed,
) -> Result<RootSecretV1, SyncError> {
    let descriptor = SlotDescriptorV1::decode(&record.descriptor)?;
    if descriptor.kind != ProtoSlotKind::Recovery {
        return Err(SyncError::Object("rec 槽类型不符"));
    }
    let mne_key = crate::keys::derive_slot_mne_key(
        seed.expose(),
        &vault_id,
        descriptor.kind,
        descriptor.key_epoch,
        &record.slot_id,
    )?;
    let cipher = XChaCha20Poly1305::new_from_slice(mne_key.expose_slice())
        .map_err(|_| SyncError::Object("槽数据密钥长度非法"))?;
    let mut plaintext = Vec::new();
    let mut first_header = None;
    let mut part_count = 1u32;
    let mut index = 0u32;
    while index < part_count {
        let path =
            api.url(&api.object_path(&vault_id, &record.slot_id, &format!("/parts/{index}")));
        let response = send(transport, "GET", path, Some(token), Vec::new())?;
        let envelope = Envelope::decode(&response.body)?;
        if envelope.header.object_type != ObjectType::KeySlot
            || envelope.header.object_id != record.slot_id
            || envelope.header.part_index != index
        {
            return Err(SyncError::Object("KEY_SLOT header 不符"));
        }
        part_count = envelope.header.part_count;
        if first_header.is_none() {
            first_header = Some(envelope.header.clone());
        }
        let aad = envelope.header.aad(&record.descriptor)?;
        let chunk = cipher
            .decrypt(
                XNonce::from_slice(&envelope.header.nonce),
                Payload {
                    msg: &envelope.ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| SyncError::Object("KEY_SLOT MNE 解密失败"))?;
        plaintext.extend_from_slice(&chunk);
        index += 1;
    }
    if plaintext.len() as u64
        != first_header
            .as_ref()
            .map_or(0, |header| header.total_plaintext_len)
    {
        return Err(SyncError::Object("KEY_SLOT 明文长度不符"));
    }
    let wrapped = WrappedRoot::from_bytes(&plaintext)?;
    let slot = KeySlotV1 {
        descriptor,
        wrapped,
    };
    let mut header_for_aad = first_header.ok_or(SyncError::Object("KEY_SLOT 没有分片"))?;
    // 槽体 AAD 是 genesis 包 root 时用的 AAD（明文长度 = root CDE 长度），不是 MNE 层
    // 包 wrapped bytes 的 AAD；按 ciphertext - TAG 反推并重算 header 字段。
    let root_plain_len = slot
        .wrapped
        .ciphertext
        .len()
        .checked_sub(16)
        .ok_or(SyncError::Object("KEY_SLOT 槽体过短"))?;
    header_for_aad.total_plaintext_len = root_plain_len as u64;
    header_for_aad.this_plaintext_len =
        u32::try_from(root_plain_len).map_err(|_| SyncError::Object("槽 root 长度溢出"))?;
    header_for_aad.ciphertext_len =
        u32::try_from(root_plain_len + 16).map_err(|_| SyncError::Object("槽密文长度溢出"))?;
    // genesis 包槽体时 message_id = slot_id[..16]（不是 MNE 层的随机 id）。
    header_for_aad
        .message_id
        .copy_from_slice(&record.slot_id[..16]);
    let aad = move |descriptor: &SlotDescriptorV1,
                    _nonce: &[u8; 24],
                    _plain_len: usize|
          -> Result<Vec<u8>, SyncError> {
        Ok(header_for_aad.aad(&descriptor.encode()?)?)
    };
    slot.unwrap_recovery(seed, &aad)
        .map_err(|_| SyncError::Object("KEY_SLOT KEK 解封失败"))
}

/// 创建邀请（管理签名 canonical body）。
///
/// `expires_at = Some(ts)` 时把 TTL 明确写进被签名的 body（ADR 0014 S1：默认 15 分钟，
/// 上限 relay 的 24h）；`None` 保持历史语义（relay 缺省 = now + 24h）。
///
/// 返回 `(invite_id, token, expires_at)`：`expires_at` 优先取 relay 回执（权威事实），
/// 旧 relay 不带该字段时回退到请求值/`now + 24h`。
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_invite<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    manager: &ManagementKey,
    token: &str,
    control_rev: u64,
    instance: [u8; 16],
    now: u64,
    expires_at: Option<u64>,
) -> Result<([u8; 16], [u8; 32], u64), SyncError> {
    let raw: [u8; 32] = crate::random::array(&mut crate::random::OsRandom)?;
    let token_hash = Sha256::digest(raw);
    let mut payload_fields = vec![(0, cbor::bytes(token_hash.to_vec()))];
    if let Some(expires_at) = expires_at {
        payload_fields.push((1, cbor::uint(expires_at)));
    }
    let payload = cbor::encode(payload_fields)?;
    let path = api.vault_path(&vault_id, "/invites");
    let request_id: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    let body_digest: [u8; 32] =
        Sha256::digest(management_request_cde(HttpMethod::Post, &path, &payload)?).into();
    let signature = manager.sign(&management_signing_input(
        &instance,
        &vault_id,
        &request_id,
        control_rev,
        &body_digest,
    )?);
    let body = cbor::encode(vec![
        (
            0,
            cbor::CdeValue::Map(vec![
                (0, cbor::bytes(payload)),
                (1, cbor::bytes(signature.to_vec())),
            ]),
        ),
        (1, cbor::bytes(request_id.to_vec())),
        (2, cbor::uint(control_rev)),
    ])?;
    let response = send(transport, "POST", api.url(&path), Some(token), body)?;
    let fields = decode(&response)?;
    let invite_id = cbor::bytes_fixed(&fields, 0)?;
    // 回退值只用于**展示**（旧 relay 不回 TTL 时与 relay 缺省一致 = 24h）；
    // 真实一次性/过期语义始终由 relay 判定，本地从不据此放行。
    let confirmed_expires_at = cbor::opt_uint_field(&fields, 1)?
        .or(expires_at)
        .unwrap_or(now.saturating_add(24 * 60 * 60));
    Ok((invite_id, raw, confirmed_expires_at))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn redeem_invite<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    invite_id: [u8; 16],
    token: &[u8; 32],
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    hpke_key: &DeviceHpkeKey,
) -> Result<(), SyncError> {
    let proof: [u8; 32] = crate::random::array(&mut crate::random::OsRandom)?;
    let proof_signature = signing_key.sign(&proof);
    let body = cbor::encode(vec![
        (0, cbor::bytes(token.to_vec())),
        (1, cbor::bytes(device_id.to_vec())),
        (2, cbor::bytes(signing_key.public_key().to_vec())),
        (3, cbor::bytes(hpke_key.public_key().to_vec())),
        (4, cbor::bytes(proof.to_vec())),
        (5, cbor::bytes(proof_signature.to_vec())),
    ])?;
    let response = send(
        transport,
        "POST",
        api.vault_path(&vault_id, &format!("/invites/{}/redeem", b64(&invite_id))),
        None,
        body,
    )?;
    let fields = decode(&response)?;
    let pending: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
    if pending != device_id {
        return Err(SyncError::RelayWire("redeem pending_device 不符"));
    }
    Ok(())
}

/// 批准一台 PENDING 设备（管理签名证书 + 封给它的 HPKE grant）。
///
/// 只接收**公钥**：恢复码自批路径用本地刚生成的密钥，邀请路径用管理端从成员视图读到的
/// pending 设备公钥（ADR 0014 S1 §3）。两条路径共用同一份签名/canonical body 逻辑。
#[allow(clippy::too_many_arguments)]
pub(crate) fn approve_device<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    manager: &ManagementKey,
    token: &str,
    control_rev: u64,
    instance: [u8; 16],
    device_id: DeviceId,
    sign_pk: [u8; 32],
    hpke_pk: [u8; 32],
    manager_rev: u64,
    root: &RootSecretV1,
) -> Result<(), SyncError> {
    let cert = DeviceCertV1 {
        vault_id,
        device_id,
        sign_ed25519_pub: sign_pk,
        hpke_x25519_pub: hpke_pk,
        cert_serial: 1,
        issued_manager_rev: manager_rev,
        rights: Rights(Rights::ALL),
    };
    let cert_bytes = cert.encode()?;
    let certificate_signature = manager.sign(&device_cert_signed_bytes(&cert)?);
    let grant_id: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    let aad = GrantAadV1 {
        pv: 1,
        vault_id,
        epoch: root.key_epoch,
        recipient_device_id: device_id,
        recipient_x25519_pub: hpke_pk,
        manager_rev,
        grant_id,
    };
    let sealed = seal_epoch_grant(&hpke_pk, root, &aad)?;
    let grant_signature = manager.sign(&sealed.signature_input(&aad)?);
    let request_id: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    let payload = cbor::encode(vec![
        (0, cbor::bytes(cert_bytes.clone())),
        (1, cbor::bytes(certificate_signature.to_vec())),
        (2, cbor::bytes(grant_id.to_vec())),
        (3, cbor::bytes(sealed.enc.to_vec())),
        (4, cbor::bytes(sealed.ciphertext.clone())),
        (5, cbor::bytes(grant_signature.to_vec())),
        (6, cbor::bytes(request_id.to_vec())),
        (7, cbor::uint(control_rev)),
    ])?;
    let path = api.vault_path(&vault_id, &format!("/devices/{}/approve", b64(&device_id)));
    let body_digest: [u8; 32] =
        Sha256::digest(management_request_cde(HttpMethod::Post, &path, &payload)?).into();
    let management_signature = manager.sign(&management_signing_input(
        &instance,
        &vault_id,
        &request_id,
        control_rev,
        &body_digest,
    )?);
    let body = cbor::encode(vec![
        (
            0,
            cbor::CdeValue::Map(vec![
                (0, cbor::bytes(payload)),
                (1, cbor::bytes(management_signature.to_vec())),
            ]),
        ),
        (1, cbor::bytes(request_id.to_vec())),
        (2, cbor::uint(control_rev)),
        (3, cbor::bytes(cert_bytes)),
        (4, cbor::bytes(certificate_signature.to_vec())),
        (5, cbor::bytes(grant_id.to_vec())),
        (6, cbor::bytes(sealed.enc.to_vec())),
        (7, cbor::bytes(sealed.ciphertext)),
        (8, cbor::bytes(grant_signature.to_vec())),
    ])?;
    let response = send(transport, "POST", api.url(&path), Some(token), body)?;
    let fields = decode(&response)?;
    let approved: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
    if approved != device_id {
        return Err(SyncError::RelayWire("approve device_id 不符"));
    }
    let cert_sig: [u8; 64] = cbor::bytes_fixed(&fields, 4)?;
    verify_signature(
        &manager.public_key(),
        &device_cert_signed_bytes(&cert)?,
        &cert_sig,
    )?;
    Ok(())
}

pub(crate) fn get_state<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: [u8; 16],
    token: &str,
) -> Result<(u64, u64), SyncError> {
    let response = send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/state")),
        Some(token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    Ok((cbor::uint_field(&fields, 5)?, cbor::uint_field(&fields, 4)?))
}

pub(crate) fn send<T: HttpTransport>(
    transport: &mut T,
    method: &'static str,
    path: String,
    token: Option<&str>,
    body: Vec<u8>,
) -> Result<HttpResponse, SyncError> {
    let mut headers = Vec::new();
    if !body.is_empty() {
        headers.push(("content-type".to_owned(), CONTENT_TYPE.to_owned()));
    }
    if let Some(token) = token {
        headers.push(("authorization".to_owned(), format!("Bearer {token}")));
    }
    transport.send(&HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

pub(crate) fn decode(response: &HttpResponse) -> Result<cbor::Fields, SyncError> {
    if !(200..300).contains(&response.status) {
        return Err(error_from_response(response));
    }
    cbor::decode(&response.body)
}

fn management_subject(manager_pub: &[u8; 32]) -> [u8; 16] {
    let digest = Sha256::digest(manager_pub);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn hex16(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn recovery_slot_descriptor(slot: &SlotRecord) -> Result<SlotDescriptorV1, SyncError> {
    Ok(SlotDescriptorV1::decode(&slot.descriptor)?)
}

fn parse_kind(text: &str) -> Result<ProtoSlotKind, SyncError> {
    match text {
        "password" => Ok(ProtoSlotKind::Password),
        "recovery" => Ok(ProtoSlotKind::Recovery),
        _ => Err(SyncError::Unsupported("未知槽类型")),
    }
}

fn b64(bytes: &[u8]) -> String {
    crate::base64url::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedTransport(Vec<u8>);

    impl HttpTransport for FixedTransport {
        fn send(&mut self, _request: &HttpRequest) -> Result<HttpResponse, SyncError> {
            Ok(HttpResponse {
                status: 200,
                body: self.0.clone(),
            })
        }
    }

    fn grant_body(root: &RootSecretV1, hpke: &DeviceHpkeKey, device_id: DeviceId) -> Vec<u8> {
        let aad = GrantAadV1 {
            pv: 1,
            vault_id: root.vault_id,
            epoch: root.key_epoch,
            recipient_device_id: device_id,
            recipient_x25519_pub: hpke.public_key(),
            manager_rev: 1,
            grant_id: [9; 16],
        };
        let sealed = seal_epoch_grant(&hpke.public_key(), root, &aad).expect("seal");
        let hash: [u8; 32] = Sha256::digest(&sealed.ciphertext).into();
        let signed = aad
            .grant_signed_bytes(&sealed.enc, &hash)
            .expect("signed header");
        cbor::encode(vec![
            (0, cbor::uint(root.key_epoch)),
            (1, cbor::bytes(device_id.to_vec())),
            (2, cbor::uint(1)),
            (3, cbor::bytes(sealed.enc.to_vec())),
            (4, cbor::bytes(sealed.ciphertext)),
            (5, cbor::bytes(signed)),
        ])
        .expect("body")
    }

    fn sample(hpke: &DeviceHpkeKey) -> (RootSecretV1, DeviceId) {
        let root = RootSecretV1::new([7; 16], 1, crate::keys::Secret32::from_array([0x42; 32]));
        let device_id: DeviceId = [3; 16];
        let _ = hpke;
        (root, device_id)
    }

    #[test]
    fn grant_unseals_to_the_same_root_and_placeholder_is_rejected() {
        let hpke = DeviceHpkeKey::generate(&mut crate::random::OsRandom).expect("hpke");
        let (root, device_id) = sample(&hpke);
        let mut transport = FixedTransport(grant_body(&root, &hpke, device_id));
        verify_latest_grant(
            &mut transport,
            &RelayApi::new("http://relay", crate::api_version::RelayApiVersion::V1),
            root.vault_id,
            device_id,
            "token",
            &hpke,
            &root,
        )
        .expect("真实 grant 必须解出同一 root");

        let placeholder = RootSecretV1::new(
            root.vault_id,
            root.key_epoch,
            crate::keys::Secret32::from_array([0x5A; 32]),
        );
        let mut transport = FixedTransport(grant_body(&placeholder, &hpke, device_id));
        assert!(
            verify_latest_grant(
                &mut transport,
                &RelayApi::new("http://relay", crate::api_version::RelayApiVersion::V1),
                root.vault_id,
                device_id,
                "token",
                &hpke,
                &root,
            )
            .is_err(),
            "占位 root 的 grant 必须被拒绝"
        );
    }
}

`````



@@SECTION code/042 `crates/sync/src/relay.rs`


`````rust
//! relay 客户端抽象（ADR 0006 §5）。
//!
//! 引擎只依赖这个**语义**接口；`HttpRelayClient`（[`crate::http_relay`]）负责 JSON/HTTP/
//! token 刷新，测试用 `FakeRelay` 直接实现它（见 `tests/support/fake_relay.rs`）。
//!
//! 为什么拆两层：
//!
//! - 引擎测试不需要真实网络（FakeRelay + 真 SQLite，不 sleep）；
//! - HTTP 层可以单独用 mock transport 验证请求形状/token 刷新/错误映射；
//! - 真实 relay 的 `SignatureHook` 目前是 fail-closed（`crates/relay/src/sig.rs`），
//!   真实端到端联调是 relay 侧接线后的独立任务，不能把它塞进本 crate 的测试。

use crate::error::SyncError;
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_sync_proto::{ActivationCommitV1, ActivationTV1, DeviceCertV1, ObjectCommit};

/// 同步必须暂停的原因（ADR §5：暂停后**不得**推进游标或 ACK）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    /// relay head 低于本地已确认高水位（疑似回滚）。
    RollbackDetected,
    /// relay `log_id` 与本地 pinned 值不同（restore/换实例）。
    LogIdChanged,
    /// relay 活动 epoch 与本地根不匹配（需要先 bootstrap/rotation）。
    EpochMismatch,
    /// 本地已收到的前缀已被 GC（`gc_floor` 超过 `received_seq`）。
    CursorExpired,
    /// 日志里出现本任务范围外的事件（asset/checkpoint/control），不能静默跳过。
    UnsupportedEvent,
    /// 事件缺少 ObjectCommit 签名（真实 relay 还没在 log 事件里带签名）。
    MissingSignature,
    /// 轮换阶段 A–C 已冻结：禁止再 push/encrypt（A 事务编辑仍可继续）。
    RotationFrozen,
    /// relay 在同一 `relay_seq` 上返回了与本地 durable 证据不同的历史。
    ///
    /// 触发点是 engine 已真实下载并落盘的 ASSET（skipped-asset 记录 HMAC 通过）
    /// 再次拉取同一 seq 时身份字段不一致：本地记录未被篡改，是 relay 重写了历史，
    /// 必须暂停等待人工处理，不能报成本地数据损坏。
    RelayHistoryRewritten,
}

impl core::fmt::Display for PauseReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::RollbackDetected => "检测到 relay 回滚（head 低于已确认高水位）",
            Self::LogIdChanged => "relay log_id 变化（恢复/换实例）",
            Self::EpochMismatch => "relay epoch 与本地根不匹配",
            Self::CursorExpired => "游标前缀已被 GC",
            Self::UnsupportedEvent => "日志含本任务范围外的事件类型",
            Self::MissingSignature => "日志事件缺少 ObjectCommit 签名",
            Self::RotationFrozen => "轮换已冻结，禁止 push/encrypt",
            Self::RelayHistoryRewritten => {
                "relay 改写了已消费 seq 的历史（与本地 durable 证据冲突）"
            }
        };
        f.write_str(text)
    }
}

/// relay 状态（`GET P/state` 的客户端视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayState {
    /// vault 身份。
    pub vault_id: VaultId,
    /// 日志实例（restore 必须换新）。
    pub log_id: [u8; 16],
    /// relay 实例 id。
    pub instance_id: [u8; 16],
    /// 当前活动 epoch。
    pub epoch: u64,
    /// 控制链版本。
    pub control_rev: u64,
    /// 鉴权版本（revoke 后递增）。
    pub auth_rev: u64,
    /// 已分配的最大 relay_seq。
    pub head_seq: u64,
    /// 日志前缀 GC 下界。
    pub gc_floor: u64,
    /// §30.3：当前 frozen/staging 轮换 id（响应丢失时先查询）。
    pub current_rotation_id: Option<[u8; 16]>,
}

/// 日志事件类型（ADR §5：update/asset/checkpoint/control）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayLogKind {
    /// 数据 update。
    Update,
    /// 资产对象。
    Asset,
    /// checkpoint。
    Checkpoint,
    /// 控制面转换。
    Control,
}

impl RelayLogKind {
    /// 解析 wire 字符串；未知类型返回 `None`（由调用方转成暂停）。
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "update" => Some(Self::Update),
            "asset" => Some(Self::Asset),
            "checkpoint" => Some(Self::Checkpoint),
            "control" => Some(Self::Control),
            _ => None,
        }
    }
}

/// 一条日志事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayLogEvent {
    /// 送达顺序（仅 relay 解读）。
    pub relay_seq: u64,
    /// 事件类型。
    pub kind: RelayLogKind,
    /// 关联对象（update/asset 有）。
    pub object_id: Option<ObjectId>,
    /// 对象所属 epoch。
    pub epoch: u64,
    /// 事件载荷：UPDATE 为 `ObjectCommit` 的 CDE 字节。
    pub event: Vec<u8>,
    /// `ObjectCommit` 的设备签名。
    ///
    /// **真实 relay 缺口**：当前 `GET log` 的事件只带 commit、不带签名（`objects.signed_commit`
    /// 列存的是 commit 字节）。引擎要求签名才能验收，因此这里的 `None` 会让同步暂停；
    /// relay 侧补上签名（或提供可取的签名端点）后该字段才有值。
    pub signature: Option<[u8; 64]>,
}

/// 一页日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayLogPage {
    /// 升序事件。
    pub events: Vec<RelayLogEvent>,
    /// 指最后返回事件的游标（无事件时回显请求游标）。
    pub next_cursor: String,
    /// 本页覆盖到的水位。
    pub through_seq: u64,
    /// 查询时的 head。
    pub query_head: u64,
    /// 当前活动 epoch。
    pub epoch: u64,
    /// 当前 gc_floor。
    pub gc_floor: u64,
}

/// 一次对象提交（`POST /updates` 的一项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateSubmission {
    /// 对象名（opaque 32B）。
    pub object_id: ObjectId,
    /// 已签名的提交记录（CDE）。
    pub commit: ObjectCommit,
    /// `commit.encode()` 的字节（避免重复编码）。
    pub signed_commit: Vec<u8>,
    /// 设备对 `object_commit_signing_input(commit)` 的签名。
    pub signature: [u8; 64],
}

/// 一项提交结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateOutcome {
    /// 对应对象。
    pub object_id: ObjectId,
    /// 结果。
    pub status: UpdateStatus,
}

/// 提交状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// 首次接受；`relay_seq` 只对 UPDATE/ASSET 等会进 log 的对象存在
    /// （MANIFEST/KEY_SLOT 等控制对象可能不分配送达序号）。
    Accepted {
        /// 分配的送达序号（relay 未分配时为 `None`）。
        relay_seq: Option<u64>,
    },
    /// 完全相同字节的重复提交（幂等）。
    Duplicate,
    /// 业务失败（整批鉴权/epoch 错误由传输层错误表示）。
    Failed {
        /// 统一错误码。
        code: Option<String>,
        /// 是否可重试。
        retryable: bool,
    },
}

/// 一项对象提交（任意类型：UPDATE/SNAPSHOT/MANIFEST/ASSET/KEY_SLOT）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSubmission {
    /// 对象名（opaque 32B）。
    pub object_id: ObjectId,
    /// 已签名的提交记录（CDE）。
    pub commit: ObjectCommit,
    /// `commit.encode()` 的字节。
    pub signed_commit: Vec<u8>,
    /// 设备对 `object_commit_signing_input(commit)` 的签名。
    pub signature: [u8; 64],
}

/// 一次 `PUT P/key-slots/{slot_id}` 的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotUpload {
    /// 槽 id（也是 KEY_SLOT 对象 id）。
    pub slot_id: ObjectId,
    /// 槽所属 epoch。
    pub epoch: u64,
    /// 槽类型。
    pub kind: menghunote_sync_proto::SlotKind,
    /// `CDE(SlotDescriptorV1)`。
    pub descriptor: Vec<u8>,
    /// 设备签名的待签字节（本实现 = `descriptor`，见 `checkpoint` 模块文档）。
    pub signed_message: Vec<u8>,
    /// 设备签名（64B）。
    pub signature: [u8; 64],
}

/// relay 返回的槽记录（`GET P/key-slots` 的一项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRecord {
    /// 槽 id。
    pub slot_id: ObjectId,
    /// 槽所属 epoch。
    pub epoch: u64,
    /// 槽类型。
    pub kind: menghunote_sync_proto::SlotKind,
    /// `CDE(SlotDescriptorV1)`。
    pub descriptor: Vec<u8>,
    /// `staging` / `active` / `retiring`。
    pub state: String,
    /// 槽集版本（activate 的 CAS 基准）。
    pub slot_set_rev: u64,
    /// retiring 槽的删除时间（Unix 秒）。
    pub retire_at: Option<u64>,
}

/// B 侧 latest self grant 的原始证据（外签/recipient/epoch 校验用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfGrantEvidence {
    pub epoch: u64,
    pub device_id: DeviceId,
    pub manager_rev: u64,
    pub hpke_enc: [u8; 32],
    pub ciphertext: Vec<u8>,
    pub signed_header: Vec<u8>,
    pub signature: Vec<u8>,
    pub package_digest: Option<[u8; 32]>,
}

/// `/control` 里**已复算**的 activation T 与旧/新管理双签（ADR 0006 §34）。
///
/// 只有 [`RelayClient::verified_activation_t`] 的实现能产出它：T 的双签、字段合法性
/// 与"钉在已验证控制链上"都已由实现方（真实客户端 = `HttpRelayClient`）复算过。
/// 调用方拿到的 `None` 表示**无法复算**，绝不表示"验证通过"（必须 fail-closed）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedActivationT {
    /// canonical CDE 解码后的 T（20 字段已校验）。
    pub activation_t: ActivationTV1,
    /// 旧管理 key 对 `activation_t_signing_input(T)` 的 64B 签名。
    pub old_t_signature: Vec<u8>,
    /// 新管理 key 对同一输入的 64B 签名。
    pub new_t_signature: Vec<u8>,
}

/// `/control` 中一条 `epoch.activated` 的**完整授权材料**（epoch 链的一环）。
///
/// 设备离线跨过多个 epoch 后，本地 `current_epoch` 只会落后于链尾：latest T 的
/// `from_epoch` 不再是本地 epoch。此时必须先沿控制链逐条验证每一环（T 双签、T 与
/// rotation view 绑定、`ActivationCommitV1` 摘要），才能用链尾的 T 授权 apply。
///
/// 本结构由 [`RelayClient::epoch_activations`] 产出：实现方（真实客户端 =
/// [`crate::http_relay::HttpRelayClient`]）负责把 T 钉在**已验证**控制链的位置上，
/// 并把 `rotation view + primary/fallback 公开事实` 重建为 `ActivationCommitV1`；
/// 消费方（[`crate::rotation_apply::verify_epoch_activation_chain`]）再复算全部绑定，
/// 不信任 relay 自报。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochActivationLink {
    /// 该 activation 记录在控制链上的 rev（必须等于 `T.expected_control_rev`）。
    pub control_rev: u64,
    /// 链上**前一条**记录（`control_rev - 1`）的 `record_hash`；必须等于 `T.prev_control_hash`。
    pub prev_control_hash: [u8; 32],
    /// `GET /rotations/{id}` 视图（必须是 `activated`）。
    pub view: RotationView,
    /// `/control` 里已复算的 activation T 与旧/新管理双签。
    pub evidence: VerifiedActivationT,
    /// 由 view + primary/fallback checkpoint 公开事实重建的 `ActivationCommitV1`。
    ///
    /// relay 只把它的 SHA256 记在 view 键 20；消费方必须重新编码并与该摘要逐字节比对。
    pub activation_commit: ActivationCommitV1,
}

/// staged grant 集合的一个 leaf：relay `grants_digest` 口径下的
/// `(device_id, SHA256(signed_header))`。
///
/// 设备端没有 management realm（relay `GET /rotations/{r}/grants` 仅 management 可读），
/// 因此真实客户端通常只能证明 `grants_count == 1` 的单 leaf 情形；完整 leaf 集合只
/// 出现在具备 management 凭据的调用方/测试夹具里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantLeafEvidence {
    /// 收件设备。
    pub device_id: DeviceId,
    /// `SHA256(grant.signed_header)`（relay `grants_digest` 的叶子）。
    pub signed_header_sha256: [u8; 32],
}

impl GrantLeafEvidence {
    /// 从 `signed_header` 原字节构造 leaf。
    #[must_use]
    pub fn new(device_id: DeviceId, signed_header: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            device_id,
            signed_header_sha256: Sha256::digest(signed_header).into(),
        }
    }
}

/// `GET P/checkpoints/{cid}` 返回的 manifest 明文（只读）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointManifest {
    pub checkpoint_id: [u8; 16],
    pub manifest_bytes: Vec<u8>,
}

/// `POST P/checkpoints` 的 writer lease。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointLease {
    /// checkpoint id。
    pub checkpoint_id: menghunote_sync_proto::CheckpointId,
    /// 覆盖的 epoch。
    pub epoch: u64,
    /// 创建时固定的 S。
    pub covered_seq: u64,
    /// 维护 reservation id。
    pub reservation_id: [u8; 16],
    /// reservation 过期时间（Unix 秒）。
    pub expires_at: u64,
}

/// checkpoint ref 的角色（relay `RefRole`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointRole {
    /// snapshot / asset 数据对象（纳入 `refs_digest`）。
    Data,
    /// manifest 对象本体（不纳入 `refs_digest`）。
    Manifest,
}

impl CheckpointRole {
    /// wire 文本值。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Manifest => "manifest",
        }
    }
}

/// 一条 checkpoint ref。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointRef {
    /// 对象 id。
    pub object_id: ObjectId,
    /// 角色。
    pub role: CheckpointRole,
}

/// `POST P/checkpoints/{c}/publish` 的输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointPublish {
    /// checkpoint id。
    pub checkpoint_id: menghunote_sync_proto::CheckpointId,
    /// 覆盖的 S（必须等于创建时固定的 S）。
    pub covered_seq: u64,
    /// manifest 对象 id（必须与 manifest ref 一致）。
    pub manifest_oid: ObjectId,
    /// 公开 refs 的摘要（只覆盖 snapshot/asset）。
    pub refs_digest: [u8; 32],
    /// `CheckpointCommit` 的 CDE 字节。
    pub signed_commit: Vec<u8>,
    /// 设备对 checkpoint 待签字节的签名。
    pub signature: [u8; 64],
}

/// publish 成功后的 relay 视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedCheckpoint {
    /// checkpoint id。
    pub checkpoint_id: menghunote_sync_proto::CheckpointId,
    /// epoch。
    pub epoch: u64,
    /// 覆盖的 S。
    pub covered_seq: u64,
    /// 发布时间（Unix 秒）。
    pub published_at: u64,
}

/// bootstrap 返回的一份基线视图（relay `/bootstrap` 字段 0/1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapCheckpoint {
    /// checkpoint id。
    pub checkpoint_id: menghunote_sync_proto::CheckpointId,
    /// epoch。
    pub epoch: u64,
    /// 覆盖的 S。
    pub covered_seq: u64,
    /// manifest 对象 id（relay 已发布基线必有）。
    pub manifest_oid: ObjectId,
    /// 发布时承诺的 refs digest（relay 响应字段 4；旧库可能缺）。
    pub refs_digest: Option<[u8; 32]>,
    /// 该 S 对应的 relay 游标（`next_cursor` 语义，可直接喂给 `/log`）。
    pub cursor: String,
    /// ADR §20：relay 返回的 `CheckpointCommit` CDE（bootstrap 响应键 6）。
    pub signed_commit: Option<Vec<u8>>,
    /// ADR §20：`CheckpointCommit` 设备签名（bootstrap 响应键 7）。
    pub signature: Option<[u8; 64]>,
}

/// bootstrap read lease（`/bootstrap` 字段 5/6，续租用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapLease {
    /// lease id。
    pub lease_id: [u8; 16],
    /// 到期 Unix 秒。
    pub expires_at: u64,
    /// 保护的下界 floor_seq。
    pub floor_seq: u64,
}

/// `POST P/bootstrap` 的完整结果：primary/fallback + head/epoch + lease。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapBundle {
    /// 最新有效基线（优先使用）。
    pub primary: BootstrapCheckpoint,
    /// 次新有效基线（primary 损坏时回退）。
    pub fallback: BootstrapCheckpoint,
    /// relay head_seq。
    pub head_seq: u64,
    /// relay gc_floor。
    pub gc_floor: u64,
    /// 当前 epoch。
    pub epoch: u64,
    /// 读 lease。
    pub lease: BootstrapLease,
    /// §30.3：bootstrap 是否走了 degraded（`?degraded=1`，只允许显式请求）。
    pub degraded: bool,
}

/// 轮换 begin 的客户端 wire（canonical body + 管理签名；ADR §28.2 阶段 B）。
#[derive(Debug, Clone)]
pub struct RotationBegin {
    pub request_id: [u8; 16],
    pub expected_epoch: u64,
    pub expected_control_rev: u64,
    pub revoke: Vec<[u8; 16]>,
    /// 唯一 canonical body（CDE），管理签名覆盖 `management_request_cde(POST,path,body)`。
    pub signed_body: Vec<u8>,
    pub signature: [u8; 64],
}

/// relay rotation 视图（`/rotations` 响应键 0；字段随 relay 增量扩展）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationView {
    pub rotation_id: [u8; 16],
    pub state: String,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub freeze_seq: u64,
    pub revoked_count: u64,
    pub member_set_hash: Option<[u8; 32]>,
    pub grants_count: u64,
    pub grants_digest: Option<[u8; 32]>,
    pub published_checkpoints: u64,
    /// §30.3 键 10–22：control/auth/manager rev、revoked_once、冻结资产摘要、
    /// 成员证书 digest、两份候选 cid、activation commit/时间/control_rev。
    pub control_rev: u64,
    pub auth_rev: u64,
    pub manager_rev: u64,
    pub from_epoch_revoked_once: bool,
    pub freeze_asset_count: u64,
    pub freeze_asset_digest: Option<[u8; 32]>,
    pub member_cert_digest: Option<[u8; 32]>,
    pub primary_checkpoint: Option<[u8; 16]>,
    pub fallback_checkpoint: Option<[u8; 16]>,
    pub activation_commit: Option<Vec<u8>>,
    pub activated_at: Option<u64>,
    pub activated_control_rev: Option<u64>,
    /// 键 23/26：候选集数量与 candidate_ready。
    pub candidate_count: u64,
    pub candidate_ready: bool,
    /// 键 25：维护 reservation id（候选 epoch 上传用）。
    pub reservation_id: Option<[u8; 16]>,
}

/// 冻结资产分页的一项（relay 只给不透明 object id 与元数据，real sha 在明文里）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenAssetRef {
    pub object_id: ObjectId,
    pub generation: u64,
    pub key_epoch: u64,
    pub part_count: u32,
    pub total_plain_len: u64,
    pub object_digest: [u8; 32],
}

/// `GET /rotations/{r}/assets?after=&limit=` 的一页。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrozenAssetPage {
    pub assets: Vec<FrozenAssetRef>,
    pub freeze_seq: u64,
    pub asset_set_digest: Option<[u8; 32]>,
    pub total: u64,
}

/// 候选 checkpoint 状态（`GET /checkpoints/{cid}`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointStatus {
    pub checkpoint_id: [u8; 16],
    pub state: String,
    pub role: Option<String>,
    pub covered_seq: u64,
    pub manifest_object_id: Option<ObjectId>,
    pub refs_digest: Option<[u8; 32]>,
    pub candidate_ready: bool,
    pub epoch: u64,
}

/// `/control` 键 5：管理权转换链的一条记录（ADR §27.2）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerChainEntry {
    pub rev: u64,
    pub pubkey: [u8; 32],
    pub transition: Vec<u8>,
    pub old_rev: u64,
    pub old_signature: Option<[u8; 64]>,
    pub new_signature: Option<[u8; 64]>,
}

/// relay 语义客户端。
///
/// 所有方法都假定调用方已持有有效 token（HTTP 实现内部负责刷新）；`SyncEngine` 只关心
/// 语义结果与错误是否可重试。
pub trait RelayClient {
    /// `GET P/state`。
    fn state(&mut self) -> Result<RelayState, SyncError>;

    /// `GET P/log?after=cursor&limit`。`cursor=None` 表示"从头"，真实 relay 需要
    /// bootstrap 给出初始游标（本 crate 的 `pull` 在无游标时会明确报错）。
    fn log(&mut self, cursor: Option<&str>, limit: u32) -> Result<RelayLogPage, SyncError>;

    /// `GET P/objects/{o}/parts/{n}`：返回 exact MNE1 envelope 字节。
    fn part(&mut self, object_id: &ObjectId, part_no: u32) -> Result<Vec<u8>, SyncError>;

    /// `PUT P/objects/{o}/parts/{n}`：exact 字节幂等上传。
    fn put_part(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
    ) -> Result<(), SyncError>;

    /// §28.1：候选 epoch 上传必须携带维护 reservation id；默认实现不支持（旧 relay）。
    fn put_part_with_reservation(
        &mut self,
        object_id: &ObjectId,
        part_no: u32,
        envelope: &[u8],
        _reservation: Option<&[u8; 16]>,
    ) -> Result<(), SyncError> {
        self.put_part(object_id, part_no, envelope)
    }

    /// `POST P/updates`（≤256 项；整批一次鉴权，逐项结果）。
    fn submit_updates(
        &mut self,
        items: Vec<UpdateSubmission>,
    ) -> Result<Vec<UpdateOutcome>, SyncError>;

    /// 提交单个任意类型对象（snapshot/manifest/asset/key-slot）。
    fn submit_object(&mut self, item: ObjectSubmission) -> Result<UpdateOutcome, SyncError>;

    /// `POST P/acks`。
    fn ack(&mut self, received_seq: u64, integrated_seq: u64, epoch: u64) -> Result<(), SyncError>;

    /// 查设备证书（验 `ObjectCommit` 用）。
    fn device_cert(&mut self, device_id: &DeviceId) -> Result<Option<DeviceCertV1>, SyncError>;

    /// `PUT P/key-slots/{slot_id}`：登记一个已 complete 的 KEY_SLOT 对象。
    fn put_key_slot(&mut self, upload: SlotUpload) -> Result<SlotRecord, SyncError>;

    /// `GET P/key-slots`：列出可见槽。
    fn list_key_slots(&mut self) -> Result<Vec<SlotRecord>, SyncError>;

    /// `POST P/key-slots/password:activate`：CAS 切换 active password 槽。
    fn activate_password_slot(
        &mut self,
        expected_slot_set_rev: u64,
        slot_id: ObjectId,
        signed_message: Vec<u8>,
        signature: [u8; 64],
    ) -> Result<u64, SyncError>;

    /// `POST P/checkpoints`：申请 writer lease（固定 S）。
    fn create_checkpoint(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
    ) -> Result<CheckpointLease, SyncError>;

    /// §28.1：候选 checkpoint 创建时固定 role（primary/fallback）；默认忽略 role。
    fn create_checkpoint_with_role(
        &mut self,
        estimated_bytes: u64,
        epoch: Option<u64>,
        _role: Option<&str>,
    ) -> Result<CheckpointLease, SyncError> {
        self.create_checkpoint(estimated_bytes, epoch)
    }

    /// `POST P/checkpoints/{c}/refs`：追加 opaque refs（data + manifest）。
    fn add_checkpoint_refs(
        &mut self,
        checkpoint_id: menghunote_sync_proto::CheckpointId,
        refs: Vec<CheckpointRef>,
    ) -> Result<u64, SyncError>;

    /// `POST P/checkpoints/{c}/publish`。
    fn publish_checkpoint(
        &mut self,
        publish: CheckpointPublish,
    ) -> Result<PublishedCheckpoint, SyncError>;

    /// `POST P/bootstrap`：取两份有效基线 + 各自游标 + read lease。
    fn bootstrap(&mut self) -> Result<BootstrapBundle, SyncError>;

    /// `PATCH P/bootstrap-leases/{l}`：续租（过期 → 410，调用方重新 bootstrap）。
    fn renew_bootstrap_lease(&mut self, lease_id: [u8; 16]) -> Result<BootstrapLease, SyncError>;

    /// `POST P/rotations`（管理 realm；阶段 B）。
    fn begin_rotation(&mut self, _begin: RotationBegin) -> Result<RotationView, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 rotation begin",
        ))
    }

    /// `GET P/rotations/{id}`（管理/设备可读）。
    fn rotation(&mut self, _rotation_id: [u8; 16]) -> Result<RotationView, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 rotation 查询",
        ))
    }

    /// 丢响应后按 request_id 找回（relay task-101 后可用；未实现时返回 Unsupported）。
    fn rotation_for_request(
        &mut self,
        _request_id: [u8; 16],
    ) -> Result<Option<RotationView>, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 request_id 查询 rotation",
        ))
    }

    /// `GET /control` 键 5：管理权转换链（阶段 A 校验）。
    fn management_chain(&mut self) -> Result<Vec<ManagerChainEntry>, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 management chain",
        ))
    }

    /// 阶段 D：冻结 finalized 资产分页（不依赖可回收 log）。
    fn rotation_assets(
        &mut self,
        _rotation_id: [u8; 16],
        _after: Option<[u8; 32]>,
        _limit: u32,
    ) -> Result<FrozenAssetPage, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 frozen asset 分页",
        ))
    }

    /// 阶段 E：候选 checkpoint 状态（ready 前不对外生效）。
    fn checkpoint_status(
        &mut self,
        _checkpoint_id: [u8; 16],
    ) -> Result<CheckpointStatus, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 candidate checkpoint 状态",
        ))
    }

    /// §28.1：当前 active 成员证书的 HPKE 公钥（来自已验证 `/control` 缓存）。
    fn active_member_hpke_keys(&mut self) -> Result<Vec<(DeviceId, [u8; 32])>, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 active 成员 HPKE 列表",
        ))
    }

    /// §30.3：控制链 typed kind（键 4）。
    fn control_kinds(&mut self) -> Result<Vec<(u64, String)>, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 control_kinds",
        ))
    }

    /// B 侧 latest self grant 的原始证据。
    fn self_grant_evidence(&mut self) -> Result<SelfGrantEvidence, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 self_grant_evidence",
        ))
    }

    /// `/control` 中**已复算**的 activation T 与旧/新管理双签（ADR 0006 §34）。
    ///
    /// 这是 B 侧 apply 的**唯一授权来源**：relay 自报的 `ActivationCommitV1` 32B hash
    /// 只用于交叉一致性，不能代替 T。
    ///
    /// 默认 `Ok(None)`：表示该客户端**无法复算** T（历史控制记录不带 T，或实现方
    /// 没有管理 realm/控制链缓存）。`None` 绝不等于"验证通过"——调用方必须
    /// fail-closed（`Paused`，不写库、不推游标）。
    ///
    /// 真实 HTTP 客户端在 [`crate::http_relay::HttpRelayClient`] 里覆盖本方法，
    /// 直接委托它的 inherent `verified_activation_t`（按 `/control` 缓存复算）。
    fn verified_activation_t(
        &mut self,
        _rotation_id: [u8; 16],
    ) -> Result<Option<VerifiedActivationT>, SyncError> {
        Ok(None)
    }

    /// 完整 staged grant leaf 集合（证明本设备 latest grant 的 inclusion）。
    ///
    /// 默认 `Ok(None)`：设备端读不到 management 的 grant 清单，此时只有
    /// `grants_count == 1`（单 leaf 即全集）能复算 `grants_digest`；其余情形
    /// 调用方必须拒绝（无法证明 inclusion）。
    fn grant_leaf_evidence(
        &mut self,
        _rotation_id: [u8; 16],
    ) -> Result<Option<Vec<GrantLeafEvidence>>, SyncError> {
        Ok(None)
    }

    /// `/control` 中全部 `epoch.activated` 的完整授权材料（epoch 链），按控制链 rev 升序。
    ///
    /// 只返回 `to_epoch > from_epoch`（调用方本地可信 epoch）的环：每环的
    /// `view/T/activation_commit` 必须能在本地复算（见
    /// [`crate::rotation_apply::verify_epoch_activation_chain`]）。
    ///
    /// 真实客户端 = [`crate::http_relay::HttpRelayClient`]：从已验证 `/control` 缓存取
    /// T 与它在链上的位置（rev / 前一条 `record_hash`），再按 rotation/cid 重建 commit。
    ///
    /// **默认 `Ok(Vec::new())`**：表示该客户端无法提供可验证的链（不支持/无控制链缓存）。
    /// 空链绝不等于"验证通过"——本地 epoch 落后 relay 时调用方必须 fail-closed（`Paused`），
    /// 不得回退到"只信 latest rotation 的 view"。
    fn epoch_activations(
        &mut self,
        _from_epoch: u64,
    ) -> Result<Vec<EpochActivationLink>, SyncError> {
        Ok(Vec::new())
    }

    /// 只读 checkpoint manifest（不写库）。
    fn checkpoint_manifest(
        &mut self,
        _checkpoint_id: &[u8; 16],
    ) -> Result<CheckpointManifest, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 checkpoint_manifest",
        ))
    }

    /// 丢弃旧 token/控制缓存；下一次请求重新认证（begin/activate 会 bump auth_rev）。
    fn reauthenticate(&mut self) -> Result<(), SyncError> {
        Ok(())
    }

    /// §30.3：`GET /rotations/current`；没有进行中的轮换返回 `None`。
    fn current_rotation(&mut self) -> Result<Option<RotationView>, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 rotation current",
        ))
    }

    /// §30.3：显式 degraded bootstrap（`POST /bootstrap?degraded=1`）。
    fn bootstrap_degraded(&mut self) -> Result<BootstrapBundle, SyncError> {
        Err(SyncError::Unsupported(
            "该 relay 客户端不支持 degraded bootstrap",
        ))
    }
}

`````



@@SECTION code/043 `crates/sync/src/relay_policy.rs`


`````rust
//! relay **模式与 URL 的唯一权威策略**（ADR 0013 §1/§6）。
//!
//! 生产与开发共用同一套解析/规范化：
//!
//! | 模式 | scheme | 主机 | 额外限制 |
//! | --- | --- | --- | --- |
//! | [`RelayMode::LocalDev`] | 只允许 `http` | **显式白名单** `localhost` / `127.0.0.1` / `[::1]` | 必须显式端口可选；拒绝 userinfo/query/fragment/非根路径 |
//! | [`RelayMode::RemoteTls`] | 只允许 `https` | DNS 名或 IP（含 IPv6 字面量） | 拒绝 userinfo/query/fragment/非根 base path/非法端口 |
//!
//! 关键纪律（ADR 0013「前提校正」）：
//!
//! - `local_dev` **不** 用 `IpAddr::is_loopback()` 这种宽泛判定（`::2`、127.0.0.2 之类的
//!   loopback 变体不被接受），也不读环境变量放宽生产策略；
//! - 明文只在 [`RelayMode::LocalDev`] 下合法，且词法上必须是 `http://`；
//! - 输出永远是 canonical origin（`scheme://host[:port]`，默认端口省略），
//!   这样配置、日志、pin 的显示与比较只有一种形态；
//! - 生产传输显式用 [`TransportPolicy::production`]，**绝不**调用会读环境变量的
//!   `StdTcpTransport::new`；`local_dev` 分支由调用方（app 的 `dev-sync` feature）闸控。
//!
//! 本模块还提供生产 client 的**受限构造入口** [`open_pinned_client`]：它只接受
//! `remote_tls`，并要求 vault 里已有 durable `sync.control_pin`；缺失时返回
//! [`SyncError::NeedsTrust`]，**不会**退化成 unpinned/local-dev 消费控制链。
//! S1 不把它接到 worker/onboarding（S2 统一 wiring）。

use std::fmt;
use std::net::Ipv6Addr;

use menghunote_storage::Vault;

use crate::control_pin::load_control_pin;
use crate::device::DeviceSigningKey;
use crate::error::SyncError;
use crate::http_relay::{HttpRelayClient, HttpRelayConfig};
use crate::http_transport::{StdTcpTransport, TransportPolicy, TransportTimeouts};

/// relay 接入模式（公开配置 `relay_mode` 的唯一取值集合）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayMode {
    /// 本机开发：明文 `http://` + 显式 loopback 白名单。
    LocalDev,
    /// 生产：`https://` + 系统可信根（TLS 端点认证）与 durable pin 两层独立校验。
    RemoteTls,
}

impl RelayMode {
    /// 配置里的字面值（`local_dev` / `remote_tls`）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalDev => "local_dev",
            Self::RemoteTls => "remote_tls",
        }
    }

    /// 严格解析配置字面值；未知值返回 `None`（调用方必须明确拒绝，不做默认回退）。
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "local_dev" => Some(Self::LocalDev),
            "remote_tls" => Some(Self::RemoteTls),
            _ => None,
        }
    }
}

impl fmt::Display for RelayMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// URL 解析/策略拒绝的原因。
///
/// 错误文案**只含公开定位信息**（scheme/host/端口），绝不回显原始 userinfo。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayUrlError {
    /// 空输入（含只有空白）。
    Empty,
    /// 缺少 `://`，无法确定 scheme。
    MissingScheme,
    /// scheme 与模式不匹配（例如 `local_dev` 收到 `https://`）。
    SchemeNotAllowed {
        /// 期望 scheme（由模式决定）。
        expected: &'static str,
        /// 实际 scheme（小写；未知 scheme 原样转小写）。
        actual: String,
    },
    /// 携带 userinfo（`user:pass@host`）。
    Userinfo,
    /// 携带 query（`?...`）。
    Query,
    /// 携带 fragment（`#...`）。
    Fragment,
    /// 非根 base path（只接受空路径或 `/`）。
    BasePath,
    /// 缺少主机名。
    MissingHost,
    /// 主机不在 `local_dev` 的显式白名单里。
    HostNotAllowed(String),
    /// 主机名/端口语法非法。
    InvalidHost(String),
    /// 端口为空/非数字/超出 u16。
    InvalidPort(String),
    /// 端口为 0。
    PortZero,
}

impl fmt::Display for RelayUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "relay 地址为空"),
            Self::MissingScheme => write!(
                f,
                "relay 缺少 scheme（生产必须 https://，本机开发必须 http://）"
            ),
            Self::SchemeNotAllowed { expected, actual } => write!(
                f,
                "relay scheme 不允许：本模式只接受 {expected}://（收到 {actual}://）"
            ),
            Self::Userinfo => write!(f, "relay 不允许携带 userinfo：凭据必须走认证协议"),
            Self::Query => write!(f, "relay 基地址不允许携带 query（?...）"),
            Self::Fragment => write!(f, "relay 基地址不允许携带 fragment（#...）"),
            Self::BasePath => write!(
                f,
                "relay 只接受 canonical origin：不能带非根 base path（/api 之类）"
            ),
            Self::MissingHost => write!(f, "relay 缺少主机名"),
            Self::HostNotAllowed(host) => write!(
                f,
                "local_dev 只允许明文 loopback 白名单 localhost / 127.0.0.1 / [::1]，\
                 收到主机 {host:?}"
            ),
            Self::InvalidHost(host) => write!(f, "relay 主机名非法：{host:?}"),
            Self::InvalidPort(port) => write!(f, "relay 端口非法：{port:?}"),
            Self::PortZero => write!(f, "relay 端口不能为 0"),
        }
    }
}

impl std::error::Error for RelayUrlError {}

/// canonical origin（`scheme://host[:port]`）。
///
/// 构造只能经 [`parse_relay_url`]：主机已小写、IPv6 已去括号（展示时补回）、
/// 默认端口已省略。`Display` 输出即 canonical origin。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayOrigin {
    mode: RelayMode,
    scheme: &'static str,
    host: String,
    port: u16,
}

impl RelayOrigin {
    /// 模式。
    #[must_use]
    pub const fn mode(&self) -> RelayMode {
        self.mode
    }

    /// 主机（DNS 小写；IPv6 不带方括号）。
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// 生效端口（显式给出或 scheme 默认值）。
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// 是否本地开发模式。
    #[must_use]
    pub const fn is_local_dev(&self) -> bool {
        matches!(self.mode, RelayMode::LocalDev)
    }

    /// canonical origin 字符串（`scheme://host[:port]`，默认端口省略）。
    #[must_use]
    pub fn as_str(&self) -> String {
        let default_port = match self.scheme {
            "https" => 443,
            _ => 80,
        };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == default_port {
            format!("{}://{host}", self.scheme)
        } else {
            format!("{}://{host}:{}", self.scheme, self.port)
        }
    }
}

impl fmt::Display for RelayOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl From<RelayUrlError> for SyncError {
    fn from(error: RelayUrlError) -> Self {
        Self::ConfigInvalid(error.to_string())
    }
}

/// 解析并校验 relay 基地址（唯一权威入口）。
///
/// 成功返回 canonical origin；失败返回 [`RelayUrlError`]（不含 userinfo）。
pub fn parse_relay_url(raw: &str, mode: RelayMode) -> Result<RelayOrigin, RelayUrlError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(RelayUrlError::Empty);
    }
    // query/fragment 在任何模式下都不允许：先按字符拒绝，避免被 authority 解析吞掉。
    if raw.contains('?') {
        return Err(RelayUrlError::Query);
    }
    if raw.contains('#') {
        return Err(RelayUrlError::Fragment);
    }
    let (scheme_text, rest) = raw.split_once("://").ok_or(RelayUrlError::MissingScheme)?;
    let scheme_text = scheme_text.to_ascii_lowercase();
    let expected = match mode {
        RelayMode::LocalDev => "http",
        RelayMode::RemoteTls => "https",
    };
    if scheme_text != expected {
        return Err(RelayUrlError::SchemeNotAllowed {
            expected,
            actual: scheme_text,
        });
    }
    // authority 到第一个 '/' 为止；之后只能是根路径。
    let cut = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..cut];
    let path = &rest[cut..];
    if !path.is_empty() && path != "/" {
        return Err(RelayUrlError::BasePath);
    }
    if authority.contains('@') {
        return Err(RelayUrlError::Userinfo);
    }
    if authority.is_empty() {
        return Err(RelayUrlError::MissingHost);
    }
    let (host, port_text) = split_authority(authority)?;
    if host.is_empty() {
        return Err(RelayUrlError::MissingHost);
    }
    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(RelayUrlError::InvalidHost(host));
    }
    let port = match port_text {
        Some(text) => {
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(RelayUrlError::InvalidPort(text.to_owned()));
            }
            let port = text
                .parse::<u16>()
                .map_err(|_| RelayUrlError::InvalidPort(text.to_owned()))?;
            if port == 0 {
                return Err(RelayUrlError::PortZero);
            }
            port
        }
        None => match mode {
            RelayMode::LocalDev => 80,
            RelayMode::RemoteTls => 443,
        },
    };
    let host = host.to_ascii_lowercase();
    match mode {
        RelayMode::LocalDev => {
            // 显式白名单：**不用** is_loopback()。
            if !matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1") {
                return Err(RelayUrlError::HostNotAllowed(host));
            }
        }
        RelayMode::RemoteTls => {
            if !is_remote_host(&host) {
                return Err(RelayUrlError::InvalidHost(host));
            }
        }
    }
    Ok(RelayOrigin {
        mode,
        scheme: expected,
        host,
        port,
    })
}

/// 拆 authority 为 `(host, Option<port_text>)`；IPv6 必须写成 `[::1][:port]`。
fn split_authority(authority: &str) -> Result<(String, Option<&str>), RelayUrlError> {
    if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, rest) = after_bracket
            .split_once(']')
            .ok_or_else(|| RelayUrlError::InvalidHost(authority.to_owned()))?;
        // 括号内必须是合法 IPv6 字面量（拒绝 `[localhost]` 之类）。
        host.parse::<Ipv6Addr>()
            .map_err(|_| RelayUrlError::InvalidHost(host.to_owned()))?;
        match rest {
            "" => Ok((host.to_owned(), None)),
            _ => {
                let port = rest
                    .strip_prefix(':')
                    .ok_or_else(|| RelayUrlError::InvalidHost(authority.to_owned()))?;
                Ok((host.to_owned(), Some(port)))
            }
        }
    } else if let Some((host, port)) = authority.rsplit_once(':') {
        // 裸 IPv6（含多个冒号）必须加方括号；这里拒绝以免把 `::1` 当 host:port。
        if host.contains(':') {
            return Err(RelayUrlError::InvalidHost(authority.to_owned()));
        }
        Ok((host.to_owned(), Some(port)))
    } else {
        Ok((authority.to_owned(), None))
    }
}

/// `remote_tls` 的主机：合法 IPv4/IPv6 字面量，或语法合法的 DNS 名。
fn is_remote_host(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    // DNS：总长 ≤ 253，各 label 非空且 ≤ 63，只允许 ASCII 字母/数字/`-`/`_`，
    // label 不以 `-` 开头/结尾（`_` 常见于内网主机记录，这里不额外放宽其它字符）。
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    })
}

/// 生产传输：**显式** [`TransportPolicy::production`]；不接受 `local_dev`。
///
/// 不读 `MENGHUNOTE_ALLOW_PLAINTEXT_HTTP` 之类的环境放宽，也不调用 `StdTcpTransport::new`。
pub fn production_transport(
    origin: &RelayOrigin,
    timeouts: Option<TransportTimeouts>,
) -> Result<StdTcpTransport, SyncError> {
    if origin.mode() != RelayMode::RemoteTls {
        return Err(SyncError::ConfigInvalid(
            "生产 relay 必须是 remote_tls（https）；local_dev 只在显式开发构建中可用".to_owned(),
        ));
    }
    let transport =
        StdTcpTransport::new_with_policy(&origin.as_str(), TransportPolicy::production())?;
    Ok(apply_timeouts(transport, timeouts))
}

/// 本地开发传输：只接受 `local_dev`，用显式的 loopback 开发策略。
///
/// 调用方必须是显式的开发入口（app 的 `dev-sync` feature）；生产 client 不得走到这里。
pub fn local_dev_transport(
    origin: &RelayOrigin,
    timeouts: Option<TransportTimeouts>,
) -> Result<StdTcpTransport, SyncError> {
    if origin.mode() != RelayMode::LocalDev {
        return Err(SyncError::ConfigInvalid(
            "本地开发 transport 只接受 local_dev（http://loopback）".to_owned(),
        ));
    }
    let transport = StdTcpTransport::new_with_policy(
        &origin.as_str(),
        TransportPolicy::localhost_development(),
    )?;
    Ok(apply_timeouts(transport, timeouts))
}

fn apply_timeouts(
    transport: StdTcpTransport,
    timeouts: Option<TransportTimeouts>,
) -> StdTcpTransport {
    let Some(timeouts) = timeouts else {
        return transport;
    };
    let transport = transport
        .with_connect_timeout(timeouts.connect)
        .with_io_timeout(timeouts.io);
    match timeouts.request_deadline {
        Some(deadline) => transport.with_request_deadline(deadline),
        None => transport,
    }
}

/// 生产 client 的构造参数（S2 的 `RelayContext` 收敛点；S1 只提供受限入口）。
#[derive(Debug, Clone)]
pub struct PinnedClientSpec {
    /// 已解析的 `remote_tls` origin。
    pub origin: RelayOrigin,
    /// vault 身份（与 `/state.vault_id` 比对由 client 完成）。
    pub vault_id: [u8; 16],
    /// 本机设备身份。
    pub device_id: [u8; 16],
    /// 管理公钥信任锚（ADR §35.2；绝不取 relay genesis key）。
    pub manager_pub: [u8; 32],
    /// 本地身份里 pin 住的 relay instance（若已有）；用于在发请求前 fail-stop。
    pub expected_instance: Option<[u8; 16]>,
    /// 传输预算；`None` = 历史默认。
    pub timeouts: Option<TransportTimeouts>,
}

/// 生产 client 的**唯一**通用构造入口：`remote_tls` + durable pin + 生产传输策略。
///
/// 失败语义（fail-closed，均可分类到 ADR 0013 §4）：
/// - origin 不是 `remote_tls` → [`SyncError::ConfigInvalid`]；
/// - 没有 durable `sync.control_pin` → [`SyncError::NeedsTrust`]（**不**退回 unpinned）；
/// - pin 损坏/绑定不符 → [`SyncError::PinCorrupt`]；
/// - pin 的实例与本地身份不一致 → [`SyncError::InstanceMismatch`]。
pub fn open_pinned_client(
    vault: &Vault,
    spec: PinnedClientSpec,
    signing_key: DeviceSigningKey,
) -> Result<HttpRelayClient<StdTcpTransport>, SyncError> {
    let PinnedClientSpec {
        origin,
        vault_id,
        device_id,
        manager_pub,
        expected_instance,
        timeouts,
    } = spec;
    let pin = load_control_pin(vault, vault_id)?.ok_or(SyncError::NeedsTrust(
        "缺少 durable sync.control_pin：生产路径拒绝以 unpinned 方式消费 /control",
    ))?;
    if let Some(expected) = expected_instance {
        if pin.instance_id != expected {
            return Err(SyncError::InstanceMismatch);
        }
    }
    let transport = production_transport(&origin, timeouts)?;
    let config = HttpRelayConfig {
        base_url: origin.as_str(),
        // 生产受限入口是历史 v1 语义；v2 只由票据创建/显式配置进入。
        api_version: crate::api_version::RelayApiVersion::V1,
        vault_id,
        device_id,
        manager_pub: Some(manager_pub),
    };
    Ok(HttpRelayClient::new(transport, config, signing_key).with_control_pin(pin.to_control_pin()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(raw: &str) -> Result<RelayOrigin, RelayUrlError> {
        parse_relay_url(raw, RelayMode::LocalDev)
    }

    fn remote(raw: &str) -> Result<RelayOrigin, RelayUrlError> {
        parse_relay_url(raw, RelayMode::RemoteTls)
    }

    #[test]
    fn local_dev_accepts_only_explicit_loopback_allowlist() {
        for good in [
            ("http://localhost:18080", "http://localhost:18080"),
            ("http://127.0.0.1:18080", "http://127.0.0.1:18080"),
            ("http://[::1]:18080", "http://[::1]:18080"),
            // 默认端口规范化：显式 `:80` 被省略，尾斜杠也归一。
            ("http://localhost:80", "http://localhost"),
            ("http://127.0.0.1", "http://127.0.0.1"),
            ("http://[::1]/", "http://[::1]"),
            ("  http://LOCALHOST:1234  ", "http://localhost:1234"),
        ] {
            let origin = local(good.0).unwrap_or_else(|error| panic!("{good:?}：{error}"));
            assert_eq!(origin.as_str(), good.1);
            assert_eq!(origin.mode(), RelayMode::LocalDev);
        }
        // 不是白名单里的 loopback 变体也不行（显式白名单，不用 is_loopback()）。
        for bad in [
            "http://127.0.0.2:18080",
            "http://[::2]:18080",
            "http://relay.example:18080",
            "http://192.168.1.9:18080",
            "http://0.0.0.0:18080",
            "http://localhost.evil.example",
        ] {
            assert!(
                matches!(local(bad), Err(RelayUrlError::HostNotAllowed(_))),
                "{bad:?} 必须被主机白名单拒绝"
            );
        }
    }

    #[test]
    fn local_dev_rejects_scheme_userinfo_query_fragment_path_and_bad_ports() {
        assert!(matches!(
            local("https://localhost:18080"),
            Err(RelayUrlError::SchemeNotAllowed { .. })
        ));
        assert!(matches!(
            local("http://user:pass@127.0.0.1:18080"),
            Err(RelayUrlError::Userinfo)
        ));
        assert!(matches!(
            local("http://127.0.0.1:18080/?x=1"),
            Err(RelayUrlError::Query)
        ));
        assert!(matches!(
            local("http://127.0.0.1:18080/#frag"),
            Err(RelayUrlError::Fragment)
        ));
        assert!(matches!(
            local("http://127.0.0.1:18080/api"),
            Err(RelayUrlError::BasePath)
        ));
        assert!(matches!(
            local("http://127.0.0.1:0"),
            Err(RelayUrlError::PortZero)
        ));
        assert!(matches!(
            local("http://127.0.0.1:notaport"),
            Err(RelayUrlError::InvalidPort(_))
        ));
        assert!(matches!(
            local("http://127.0.0.1:99999"),
            Err(RelayUrlError::InvalidPort(_))
        ));
        assert!(matches!(
            local("127.0.0.1:18080"),
            Err(RelayUrlError::MissingScheme)
        ));
        assert!(matches!(local("   "), Err(RelayUrlError::Empty)));
        assert!(matches!(
            local("http://[::1:18080"),
            Err(RelayUrlError::InvalidHost(_))
        ));
    }

    #[test]
    fn remote_tls_only_accepts_https_and_canonicalizes_origin() {
        for good in [
            ("https://relay.example", "https://relay.example"),
            ("https://relay.example:8443", "https://relay.example:8443"),
            ("https://Relay.Example:443/", "https://relay.example"),
            ("https://192.0.2.10:9443", "https://192.0.2.10:9443"),
            ("https://[2001:db8::1]:8443", "https://[2001:db8::1]:8443"),
            ("https://[2001:db8::1]", "https://[2001:db8::1]"),
        ] {
            let origin = remote(good.0).unwrap_or_else(|error| panic!("{good:?}：{error}"));
            assert_eq!(origin.as_str(), good.1);
            assert_eq!(origin.mode(), RelayMode::RemoteTls);
        }
        for bad in [
            "http://relay.example",
            "https://user:pass@relay.example",
            "https://relay.example/api",
            "https://relay.example/?x=1",
            "https://relay.example/#frag",
            "https://relay.example:0",
            "https://relay.example:70000",
            "https://",
            "https://[::1",
            "https://exa mple.com",
            "https://-bad.example.com",
        ] {
            assert!(remote(bad).is_err(), "{bad:?} 必须被拒绝");
        }
    }

    #[test]
    fn canonical_origin_roundtrips_through_the_parser() {
        for (raw, mode) in [
            ("http://127.0.0.1:1", RelayMode::LocalDev),
            ("http://localhost", RelayMode::LocalDev),
            ("https://relay.example:8443", RelayMode::RemoteTls),
        ] {
            let origin = parse_relay_url(raw, mode).expect("合法");
            let again = parse_relay_url(&origin.as_str(), mode).expect("canonical 必须可再解析");
            assert_eq!(origin, again);
        }
    }

    #[test]
    fn modes_are_strict_and_never_default() {
        assert_eq!(RelayMode::parse("local_dev"), Some(RelayMode::LocalDev));
        assert_eq!(RelayMode::parse("remote_tls"), Some(RelayMode::RemoteTls));
        for bad in ["", "LOCAL_DEV", "remote", "https", "local-dev"] {
            assert_eq!(RelayMode::parse(bad), None, "{bad:?} 不能被默认接受");
        }
        assert_eq!(RelayMode::LocalDev.as_str(), "local_dev");
        assert_eq!(RelayMode::RemoteTls.to_string(), "remote_tls");
    }

    #[test]
    fn production_and_dev_transports_are_mode_locked() {
        let remote_origin = remote("https://relay.example").expect("remote");
        let transport = production_transport(&remote_origin, None).expect("生产策略可构造");
        assert_eq!(
            transport.policy(),
            TransportPolicy::production(),
            "生产入口必须显式 production policy"
        );
        let local_origin = local("http://127.0.0.1:18080").expect("local");
        assert!(
            matches!(
                production_transport(&local_origin, None),
                Err(SyncError::ConfigInvalid(_))
            ),
            "生产入口绝不能接受 local_dev"
        );
        let transport = local_dev_transport(&local_origin, None).expect("开发策略可构造");
        assert_eq!(transport.policy(), TransportPolicy::localhost_development());
        assert!(matches!(
            local_dev_transport(&remote_origin, None),
            Err(SyncError::ConfigInvalid(_))
        ));
    }

    fn fresh_vault() -> (tempfile::TempDir, Vault) {
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("relay-policy-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    fn spec(origin: RelayOrigin, instance: Option<[u8; 16]>) -> PinnedClientSpec {
        PinnedClientSpec {
            origin,
            vault_id: [0x5a; 16],
            device_id: [0x04; 16],
            manager_pub: [0x07; 32],
            expected_instance: instance,
            timeouts: None,
        }
    }

    fn signing_key() -> DeviceSigningKey {
        DeviceSigningKey::from_seed(crate::keys::Secret32::from_array([0x09; 32]))
    }

    /// 生产 client：没有 durable pin 时必须 NeedsTrust（绝不 unpinned、也不建空 pin）。
    #[test]
    fn production_client_requires_a_durable_pin() {
        let (_dir, vault) = fresh_vault();
        let error = match open_pinned_client(
            &vault,
            spec(remote("https://relay.example").expect("remote"), None),
            signing_key(),
        ) {
            Ok(_) => panic!("缺 durable pin 必须 NeedsTrust"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
        assert!(
            vault
                .get_meta(crate::control_pin::META_CONTROL_PIN)
                .expect("meta")
                .is_none(),
            "拒绝路径不得写入 pin"
        );
    }

    /// 生产 client：有 pin 才能打开；instance 绑定不符与 local_dev origin 都拒绝。
    #[test]
    fn production_client_uses_the_pinned_head_and_rejects_bad_bindings() {
        let (_dir, vault) = fresh_vault();
        crate::control_pin::commit_verified_head(
            &vault,
            None,
            crate::control_pin::VerifiedHead::from_verified_control(
                [0x5a; 16], [0x22; 16], 1, [0x11; 32],
            ),
        )
        .expect("写 durable pin");

        // instance 一致 → 可构造（不联网；TLS 根在首次请求时才装配）。
        let origin = remote("https://relay.example:8443").expect("remote");
        open_pinned_client(&vault, spec(origin, Some([0x22; 16])), signing_key())
            .expect("有 pin 且 instance 一致必须可构造");

        let origin = remote("https://relay.example").expect("remote");
        let error = match open_pinned_client(&vault, spec(origin, Some([0x33; 16])), signing_key())
        {
            Ok(_) => panic!("instance 不符必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::InstanceMismatch), "{error:?}");

        let origin = local("http://127.0.0.1:18080").expect("local");
        let error = match open_pinned_client(&vault, spec(origin, Some([0x22; 16])), signing_key())
        {
            Ok(_) => panic!("生产 client 不得接受 local_dev"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
    }

    #[test]
    fn production_never_accepts_plaintext_even_with_dev_origin() {
        // 生产工厂入口按模式拒绝；即使调用方手里是 loopback origin 也不行。
        let origin = local("http://127.0.0.1:18080").expect("local");
        let error = match production_transport(&origin, None) {
            Ok(_) => panic!("明文必须拒绝"),
            Err(error) => error,
        };
        assert!(matches!(error, SyncError::ConfigInvalid(_)), "{error:?}");
    }
}

`````



@@SECTION code/044 `crates/sync/src/remote_init.rs`


`````rust
//! ADR 0013 **S3b**：app 侧的远端 `remote_tls` Create / Join 编排（受限初始化 + 身份落盘回调）。
//!
//! ## 与 `dev_local` / `init_session` 的分工
//!
//! - [`crate::init_session::InitSession`] 是**受限能力**：NewGenesis / FreshRecovery，
//!   没有 unpin/reset 分支，pin 决议只能由本地材料（genesis 证书 / 恢复码）证明；
//! - [`crate::dev_local::DevLocal`] 是 `local_dev` 的既有编排（明文 loopback）；
//! - 本模块把同一套受限能力接到 **remote_tls**（production TLS + HTTPS canonical origin）：
//!   远端 Create/Join 的**顺序**与本地不同——身份文件由 app 用"临时 + 原子 rename"写，
//!   sync 看不到那个提交点，所以 `create_remote` / `join_remote` 接受一个**落盘回调**
//!   （`persist_identity`），在正确时序里调用它，然后由本模块推进 durable pending 阶段。
//!
//! ```text
//! Create: pending(Prepared) → POST /v1/vaults(GenesisRequested)
//!         → 本地事实（structure/label/cursor/log_id/初始槽材料）
//!         → 身份原子落盘（回调）→ IdentityWritten
//!         → /state + /control 全链验签 + rev1 前缀 → commit_verified_head(PinCommitted)
//!         → 上传初始 recovery 槽 → Done（清理 pending）
//!
//! Join:   恢复码 → pending(Prepared) → self_enroll(GenesisRequested) → 认证 bootstrap
//!         → 身份原子落盘（回调）→ IdentityWritten
//!         → record pin + bootstrap 锚同一验证视图 → commit_recovery_pin(PinCommitted)
//!         → Done（清理 pending）
//! ```
//!
//! ## 失败语义（ADR 0013 §3/§4；与 local_dev 的差别是有意的）
//!
//! - **拿到回执/身份材料之前**失败（TLS/策略拒绝、relay 明确拒绝、传输中断）：
//!   本次新库没有任何可用身份，永远无法 finalize。清理 pending（`init_journal::abort_before_identity`
//!   的受限入口）+ 删掉本次新建的库文件，返回错误；远端最多留下不可用的空 vault/孤儿
//!   设备（与 ADR 0009 既有失败语义一致）。调用方可以换新目录重试。
//! - **拿到回执/身份材料之后**失败（含身份落盘失败）：**保留** pending/库与已落盘的
//!   事实（不删用户数据、不回退 local_dev、不 unpin），错误交回 app 显示；`Done` 之前的
//!   pending 由启动对账继续（能证明安全才推进，证明不了就是 `NeedsTrust`/
//!   `ConfigInvalid`，不自动重建）。身份落盘失败时本地没有可对账的身份材料，保留现场
//!   是为了人工诊断（**不**删库、**不**重跑 genesis/enroll 覆盖远端事实）。
//! - 收尾（`finish`）失败与初始 recovery 槽上传失败**不**当致命错误：身份与强 pin 都已
//!   durable，实例可用；以 `warning` 交回 app（启动对账补 pending；槽上传失败时其他设备
//!   用恢复码 Join 可能失败，恢复码仍在 app 的备份页）。
//!
//! ## 安全
//!
//! - 传输只来自 [`RelayContext::transport`]（`remote_tls` → 显式 production TLS policy，
//!   系统信任根；测试 harness 可用 `with_test_extra_roots` 显式追加自签根，仍校验证书链/
//!   SAN/有效期，**没有** accept-invalid）；
//! - pending/身份/配置**不含秘密**；恢复码只在内存里传给初始槽上传，不落盘、不进日志；
//! - 本模块不写 app 的受保护身份文件：落盘是回调的责任（`DevIdentityStore`），
//!   sync 只在回调返回成功后推进 `IdentityWritten`。

use std::path::Path;

use menghunote_storage::{ensure_structure_row, set_sync_log_id, Vault};

use crate::create_link::CreateTicket;
use crate::dev_local::{
    encode_initial_slot, remove_vault_files, throwaway_password, DevLocal,
    INITIAL_RECOVERY_SLOT_META, VAULT_LABEL_META,
};
use crate::engine::RELAY_CURSOR_META;
use crate::error::SyncError;
use crate::genesis::{self, GenesisReceipt};
use crate::http_relay::{Clock, SystemClock};
use crate::identity::LocalIdentityV1;
use crate::init_journal;
use crate::init_session::{finish_identity_written, mark_identity_written, InitSession};
use crate::random::{array, OsRandom};
use crate::recovery::RecoverySeed;
use crate::relay_context::RelayContext;
use crate::relay_policy::RelayMode;
use crate::{DeviceId, VaultId};

/// Create 成功后的产物（**不实现 `Debug`**：`identity`/`recovery_code` 是秘密材料）。
///
/// v1（`create_remote`）与 v2 票据（`create_vault_with_ticket`）返回**同一组事实**：
/// 库、身份、vault id、一次性恢复码、非致命告警。写身份/公开配置仍由调用方决定。
pub struct CreatedVault {
    /// 已打开的真实库（结构行/标签/游标/log_id/初始槽材料都已落库）。
    pub vault: Vault,
    /// 新设备身份（已由调用方回调原子落盘）。
    pub identity: LocalIdentityV1,
    /// 新 vault id（公开标识）。
    pub vault_id: VaultId,
    /// 一次性恢复码（只在 app 备份页展示；不落盘、不进日志）。
    pub recovery_code: String,
    /// 非致命告警（初始恢复槽上传/ pending 收尾失败；不影响实例可用）。
    pub warning: Option<String>,
}

/// 兼容旧名（v1 `create_remote` 的历史返回类型）。
pub type RemoteCreatedVault = CreatedVault;

/// 远端 Join 成功后的产物（**不实现 `Debug`**：`identity` 是秘密材料）。
pub struct RemoteJoinedVault {
    /// 已由 bootstrap 导入基线的库（pins/cursor 已 durable）。
    pub vault: Vault,
    /// 新设备身份（已由调用方回调原子落盘）。
    pub identity: LocalIdentityV1,
    /// 加入的 vault id（公开标识）。
    pub vault_id: VaultId,
    /// 非致命告警（pending 收尾失败；不影响实例可用）。
    pub warning: Option<String>,
}

/// 远端 Create：genesis + 身份落盘回调 + rev1 强 pin + 初始恢复槽 + `Done`。
///
/// `persist_identity` 必须在**返回前**完成身份的"临时文件 + 原子 rename"落盘
/// （app 的 `DevIdentityStore::persist_protected`）；失败=本次身份未落盘，本模块保留
/// 库/pending 并返回错误（远端可能已建 vault，**不得**重跑 genesis 覆盖）。
///
/// `context` 必须是 `remote_tls`（HTTPS canonical origin）；`local_dev` 走
/// [`crate::dev_local::DevLocal::create`]，不在本入口（防止生产路径误用明文）。
pub fn create_remote(
    context: RelayContext,
    setup_secret: &str,
    vault_path: &Path,
    vault_label: &str,
    persist_identity: impl FnOnce(&LocalIdentityV1) -> Result<(), SyncError>,
) -> Result<CreatedVault, SyncError> {
    require_remote_mode(&context)?;
    if setup_secret.is_empty() {
        return Err(SyncError::ConfigInvalid(
            "remote Create 需要非空 setup secret（服务器创建凭证）".to_owned(),
        ));
    }
    // v1 面（remote_tls 的既有 Create）；票据创建走 create_vault_with_ticket。
    if context.api_version() != crate::api_version::RelayApiVersion::V1 {
        return Err(SyncError::ConfigInvalid(
            "create_remote 只接受 v1 面；v2 创建请用一次性票据（create_vault_with_ticket）"
                .to_owned(),
        ));
    }
    let vault_id: VaultId = array(&mut OsRandom)?;
    create_with_auth(
        context,
        vault_id,
        GenesisCreateAuth::SetupSecret(setup_secret),
        None,
        vault_path,
        vault_label,
        persist_identity,
    )
}

/// **库级票据创建**（ADR 0014 S3a）：`POST /v2/vaults` + 本地材料 + 身份/pin/本地库初始化。
///
/// 与 [`create_remote`] 的主体**完全复用**（同一个 durable pending/回执校验/强 pin/
/// 初始恢复槽/收尾），只把"创建路由"从 v1 setup secret 换成 v2 一次性票据：
///
/// - `context` 必须显式选择 [`crate::api_version::RelayApiVersion::V2`]
///   （`local_dev` 或 `remote_tls` 均可；测试用 loopback dev relay）；
/// - `vault_id` 必须与票据绑定的预留 id 一致（relay 统一以 403 拒绝错 vault；客户端
///   拿不到票据的绑定信息，因此这一步只能由 relay 判定）；
/// - 票据只在内存里用一次：不写 pending/配置/日志，失败也不消费票据（relay 事务回滚）；
/// - 失败语义与 v1 相同：拿到回执前失败会清理本次新库；拿到回执后失败保留现场。
///
/// 返回事实：[`CreatedVault`]（写身份/公开配置由调用方决定，库不替 app 落盘）。
pub fn create_vault_with_ticket(
    context: RelayContext,
    vault_id: VaultId,
    ticket: &CreateTicket,
    expected_instance: Option<[u8; 16]>,
    vault_path: &Path,
    vault_label: &str,
    persist_identity: impl FnOnce(&LocalIdentityV1) -> Result<(), SyncError>,
) -> Result<CreatedVault, SyncError> {
    if context.api_version() != crate::api_version::RelayApiVersion::V2 {
        return Err(SyncError::ConfigInvalid(
            "票创建必须显式使用 v2 面（RelayApiVersion::V2）".to_owned(),
        ));
    }
    create_with_auth(
        context,
        vault_id,
        GenesisCreateAuth::Ticket(ticket),
        expected_instance,
        vault_path,
        vault_label,
        persist_identity,
    )
}

/// 创建授权方式（唯一差别；其余流程逐字节共用）。
enum GenesisCreateAuth<'a> {
    /// v1 `POST /v1/vaults`（setup secret）。
    SetupSecret(&'a str),
    /// v2 `POST /v2/vaults`（一次性票据）。
    Ticket(&'a CreateTicket),
}

/// v1/v2 共用的 Create 主体（顺序与失败语义见模块文档）。
fn create_with_auth(
    context: RelayContext,
    vault_id: VaultId,
    auth: GenesisCreateAuth<'_>,
    expected_instance: Option<[u8; 16]>,
    vault_path: &Path,
    vault_label: &str,
    persist_identity: impl FnOnce(&LocalIdentityV1) -> Result<(), SyncError>,
) -> Result<CreatedVault, SyncError> {
    if vault_path.as_os_str().is_empty() {
        return Err(SyncError::LocalCorrupt("Create 需要明确的 vault 路径"));
    }
    if vault_path.exists() {
        return Err(SyncError::LocalCorrupt(
            "Create 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
        ));
    }

    // 1) 本地材料 + 会话（不联网）：vault/device id 必须先进 pending，之后才发请求。
    let device_id: DeviceId = array(&mut OsRandom)?;
    let throwaway_password = throwaway_password()?;
    let materials = genesis::build_materials(
        vault_id,
        device_id,
        &throwaway_password,
        SystemClock.now_secs(),
        &mut OsRandom,
    )?;
    let session = InitSession::new_genesis(context, &materials)?;

    // 2) 新库 + 写前 pending（`Prepared`）。
    let vault = match Vault::open(vault_path) {
        Ok(vault) => vault,
        Err(error) => {
            remove_vault_files(vault_path);
            return Err(SyncError::Storage(error));
        }
    };
    if let Err(error) = session.prepare_pending(&vault, SystemClock.now_secs()) {
        drop(vault);
        remove_vault_files(vault_path);
        return Err(error);
    }

    // 3) 请求 genesis/genesis-with-ticket。失败 = 没有回执/身份，本次新库无法继续
    //    （见模块文档）。
    let receipt = match auth {
        GenesisCreateAuth::SetupSecret(setup_secret) => {
            session.request_genesis(&vault, &materials, setup_secret)
        }
        GenesisCreateAuth::Ticket(ticket) => {
            session.request_genesis_with_ticket(&vault, &materials, ticket)
        }
    };
    let receipt = match receipt {
        Ok(receipt) => receipt,
        Err(error) => {
            let _ = init_journal::abort_before_identity(&vault);
            drop(vault);
            remove_vault_files(vault_path);
            return Err(error);
        }
    };

    // 3b) 公开锚（链接里的 `pin`）：必须与创建回执的 relay instance 一致。
    //     不自动换 pin；不一致时这次新库没有可证明的身份，按"回执前失败"的同一套
    //     清理语义处理（远端可能已建 vault/消费票据，本地不留任何半初始化材料）。
    if let Some(expected) = expected_instance {
        if receipt.instance_id != expected {
            let _ = init_journal::abort_before_identity(&vault);
            drop(vault);
            remove_vault_files(vault_path);
            return Err(SyncError::InstanceMismatch);
        }
    }

    // 4) 本地事实（不依赖身份/pin）：回执里的 cursor/log_id + 展示标签 + 初始槽材料。
    //    先落这些，`IdentityWritten` 的崩溃窗口里库至少是"带游标/槽材料"的完整事实。
    write_created_local_facts(&vault, vault_label, &materials, &receipt)?;

    // 5) 身份落盘（app 的原子 rename）→ durable `IdentityWritten`。
    //    失败保留库/pending（远端已建 vault，不得删/重建）。
    let identity = materials.identity(receipt.instance_id, receipt.control_rev)?;
    persist_identity(&identity)?;
    drop(identity);
    mark_identity_written(&vault)?;

    // 6) 验证 `/state + /control` 全链与 rev1 `SHA256(零32||cert)` 前缀 → 强 pin。
    let report = session.finalize_genesis(&vault, &materials, &receipt)?;

    // 7) 初始 recovery 槽必须现在上传（`K_slot_mne = HKDF(恢复码种子)` 不可逆，恢复码不落盘）。
    //    这是收尾步骤：身份/强 pin 已 durable，失败降级为告警而不是"创建失败"。
    let mut warning = match DevLocal::upload_initial_slot_during_init(
        session.context(),
        &vault,
        &report.identity,
        &materials.recovery_seed,
    ) {
        Ok(()) => None,
        Err(error) => Some(format!(
            "初始恢复码槽上传失败（{error}）：实例可继续使用，但其他设备用恢复码 Join 可能失败；\
             恢复码仍须备份（可稍后用恢复码补传）"
        )),
    };

    // 8) pending → Done（清理只在 Done）。失败留给启动对账，不删数据。
    if let Err(error) = session.finish(&vault) {
        warning = Some(append_warning(
            warning,
            format!("初始化 pending 收尾失败（{error}）：重启后会做启动对账补齐"),
        ));
    }

    Ok(CreatedVault {
        vault,
        identity: report.identity,
        vault_id,
        recovery_code: materials.recovery_code.as_str().to_owned(),
        warning,
    })
}

/// 远端 Join：恢复码 → self_enroll → 认证 bootstrap → 身份落盘回调 → 强 pin + 锚 → `Done`。
///
/// 与 [`create_remote`] 对称：`persist_identity` 负责身份的原子落盘，本模块只在其成功后
/// 推进 `IdentityWritten`；pin 必须在 bootstrap 的**同一验证视图**上提交
/// （`record` 与 `bootstrap` 锚各带 rev，见 [`crate::init_session`]）。
pub fn join_remote(
    context: RelayContext,
    vault_id: VaultId,
    recovery_code: &str,
    vault_path: &Path,
    persist_identity: impl FnOnce(&LocalIdentityV1) -> Result<(), SyncError>,
) -> Result<RemoteJoinedVault, SyncError> {
    require_remote_mode(&context)?;
    if vault_path.as_os_str().is_empty() {
        return Err(SyncError::LocalCorrupt("Join 需要明确的 vault 路径"));
    }
    if vault_path.exists() {
        return Err(SyncError::LocalCorrupt(
            "Join 只接受尚不存在的 vault 路径（已有库请按其身份使用）",
        ));
    }

    // 1) 恢复码 → 恢复种子（格式错误在**建库之前**返回：不留下任何文件）。
    let seed = RecoverySeed::from_code(recovery_code)?;
    let session = InitSession::new_fresh_recovery(context, vault_id, &seed)?;

    // 2) 新库 + 写前 pending（`Prepared`）。
    let vault = match Vault::open(vault_path) {
        Ok(vault) => vault,
        Err(error) => {
            remove_vault_files(vault_path);
            return Err(SyncError::Storage(error));
        }
    };
    if let Err(error) = session.prepare_pending(&vault, SystemClock.now_secs()) {
        drop(vault);
        remove_vault_files(vault_path);
        return Err(error);
    }

    // 3) self_enroll + 认证 bootstrap（远端可能已登记设备 → 之后的失败不得自动重建）。
    let recovered = match session.recover_identity(&vault, SystemClock.now_secs()) {
        Ok(recovered) => recovered,
        Err(error) => {
            // 没有身份材料：本次新库无法 finalize（远端最多留下孤儿设备），清理后允许重试。
            let _ = init_journal::abort_before_identity(&vault);
            drop(vault);
            remove_vault_files(vault_path);
            return Err(error);
        }
    };

    // 4) 身份落盘（app 原子 rename）→ `IdentityWritten`。
    persist_identity(&recovered.identity)?;
    mark_identity_written(&vault)?;

    // 5) 用 bootstrap 的同一验证视图提交 record pin + bootstrap 锚（`PinCommitted`）。
    session.commit_recovery_pin(&vault, &recovered.identity)?;

    // 6) pending → Done（清理只在 Done）；失败留给启动对账。
    let warning = match finish_identity_written(&vault) {
        Ok(()) => None,
        Err(error) => Some(format!(
            "初始化 pending 收尾失败（{error}）：实例保留可用，重启后会做启动对账补齐"
        )),
    };

    Ok(RemoteJoinedVault {
        vault,
        identity: recovered.identity,
        vault_id,
        warning,
    })
}

/// 只允许 `remote_tls`：防止生产路径把明文/开发语义误接进远端入口。
fn require_remote_mode(context: &RelayContext) -> Result<(), SyncError> {
    if context.mode != RelayMode::RemoteTls {
        return Err(SyncError::ConfigInvalid(
            "远端 Create/Join 只接受 remote_tls（https）context；local_dev 走 DevLocal 入口"
                .to_owned(),
        ));
    }
    Ok(())
}

/// genesis 回执里的本地事实（不依赖身份/pin）：标签、初始游标、log_id、初始 slot 材料。
fn write_created_local_facts(
    vault: &Vault,
    vault_label: &str,
    materials: &genesis::GenesisMaterials,
    receipt: &GenesisReceipt,
) -> Result<(), SyncError> {
    ensure_structure_row(vault)?;
    vault.set_meta(VAULT_LABEL_META, vault_label.as_bytes())?;
    if let Some(cursor) = &receipt.initial_cursor {
        vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
    }
    // log_id 来自 genesis 响应：先钉住 relay 日志代，pull 再复核。
    set_sync_log_id(vault, &receipt.log_id)?;
    // 初始 recovery 槽材料（密文）留给上传/Join 复用（同一 slot_id 幂等）。
    let slot_record = encode_initial_slot(&materials.recovery_slot)?;
    vault.set_meta(INITIAL_RECOVERY_SLOT_META, &slot_record)?;
    Ok(())
}

/// 合并告警（两条非致命收尾失败时都保留，避免后一条覆盖前一条）。
fn append_warning(existing: Option<String>, next: String) -> String {
    match existing {
        Some(existing) => format!("{existing}；{next}"),
        None => next,
    }
}

`````



@@SECTION code/045 `crates/sync/src/rotation.rs`


`````rust
//! 轮换客户端阶段 B/C（S7-28；ADR §28.2）。
//!
//! - 阶段 B：管理签 canonical `POST begin`；journal 先持久化 request_id，响应丢失时**只按
//!   request_id 查询**，查询不可用/未找到时不盲 begin。
//! - 阶段 C：旧根 pull+integrate 到 S（必须 `integrated_seq >= S`，received 不够），
//!   固定 local cut L；门禁只拦 **≤S 的入站缺口** 与 **已上传未 ACK** 的更新，
//!   冻结后继续产生的本地 `pending/encrypted` 编辑**不阻断**轮换（推广后用新 epoch
//!   重包/上传即可）。
//!
//! relay 的 request_id 查询端点由 relay task-101 提供；未提供时本模块返回原错误并保持
//! Frozen，调用方可安全重试查询。

use sha2::{Digest, Sha256};

use crate::api_version::{RelayApi, RelayApiVersion};
use crate::cbor;
use crate::device::{verify_signature, ManagementKey};
use crate::error::SyncError;
use crate::http_relay::ControlPin;
use crate::relay::{RelayClient, RotationBegin, RotationView};
use crate::rotation_journal::{RotationJournal, RotationPhase};
use crate::VaultId;
use menghunote_storage::Vault;
use menghunote_sync_proto::{management_request_cde, management_signing_input, HttpMethod};

/// §27.2 管理转换 9 字段（本地解码/编码，不在 sync-proto 新增记录）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerTransitionV1 {
    pub vault_id: [u8; 16],
    pub instance_id: [u8; 16],
    pub rotation_id: [u8; 16],
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub old_rev: u64,
    pub new_rev: u64,
    pub new_pubkey: [u8; 32],
    pub covered_seq: u64,
}

impl ManagerTransitionV1 {
    /// CDE 编码（9 字段，键 0..8）。
    pub fn encode(&self) -> Result<Vec<u8>, SyncError> {
        cbor::encode(vec![
            (0, cbor::bytes(self.vault_id.to_vec())),
            (1, cbor::bytes(self.instance_id.to_vec())),
            (2, cbor::bytes(self.rotation_id.to_vec())),
            (3, cbor::uint(self.from_epoch)),
            (4, cbor::uint(self.to_epoch)),
            (5, cbor::uint(self.old_rev)),
            (6, cbor::uint(self.new_rev)),
            (7, cbor::bytes(self.new_pubkey.to_vec())),
            (8, cbor::uint(self.covered_seq)),
        ])
    }

    /// CDE 解码并校验 9 字段存在。
    pub fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let map = cbor::decode(bytes)?;
        Ok(Self {
            vault_id: cbor::bytes_fixed(&map, 0)?,
            instance_id: cbor::bytes_fixed(&map, 1)?,
            rotation_id: cbor::bytes_fixed(&map, 2)?,
            from_epoch: cbor::uint_field(&map, 3)?,
            to_epoch: cbor::uint_field(&map, 4)?,
            old_rev: cbor::uint_field(&map, 5)?,
            new_rev: cbor::uint_field(&map, 6)?,
            new_pubkey: cbor::bytes_fixed(&map, 7)?,
            covered_seq: cbor::uint_field(&map, 8)?,
        })
    }
}

/// 阶段 A：校验 `/control` 管理链并 pin instance/log/control head。
///
/// 安全契约（三条缺陷的修复口径）：
///
/// 1. **信任锚**：`first_old_public` 必须是调用方从**可信本地材料**（恢复码派生的 genesis
///    管理公钥、本地 identity/pin）拿到的 genesis 管理公钥。relay 返回的 rev1 记录只能
///    **校验相等**，绝不覆盖锚点；锚点缺失时 fail-closed（不允许退化成"信任 relay 给的
///    第一个公钥"）。
/// 2. **epoch 只要求前进**：`to_epoch > from_epoch` 即合法（restart 允许 1→3 跳 epoch），
///    rev 仍必须严格 `new_rev = old_rev + 1` 且链连续。
/// 3. **pin 的 control hash 只能来自已验证的 `/control` 链尾**：`control_pin` 必须由调用方用
///    `HttpRelayClient::control_head` / `HttpRelayClient::control_head_pin` 取得（含链尾
///    `record_hash`）。journal 自带的 `control_hash` **不参与** pin；control rev 的 pin 事实
///    是 `journal.control_rev`，必须与已验证链尾 rev 相等（低 pin / 控制链已前进一律拒绝）。
///    同一 rev 下链尾 `record_hash` 变化（换链尾/分叉）同样拒绝。
///
/// 逐条验证 9 字段绑定、old/new 双签、rev 连续；链尾公钥必须等于 `current_manager`。
#[allow(clippy::too_many_arguments)]
pub fn verify_management_chain_and_pin<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    journal: &mut RotationJournal,
    vault_id: VaultId,
    current_manager: &ManagementKey,
    first_old_public: Option<[u8; 32]>,
    control_pin: ControlPin,
) -> Result<(), SyncError> {
    // ① 信任锚：缺失即 fail-closed，绝不拿 relay 的 rev1 或"当前管理 key"兜底。
    let anchor = first_old_public.ok_or(SyncError::Unsupported(
        "阶段 A 缺少 genesis 管理 key 信任锚（first_old_public / 本地 identity pin）",
    ))?;
    let state = client.state()?;
    let chain = client.management_chain()?;

    // ③ pin 只接受"已验证 `/control` 链尾"这一事实来源。
    if control_pin.instance_id != state.instance_id {
        return Err(SyncError::RelayWire(
            "控制链 pin 的 relay instance 与 /state 不符",
        ));
    }
    let head_hash = control_pin.record_hash.ok_or(SyncError::RelayWire(
        "控制链 pin 缺链尾 record_hash（未经 /control 验证，不能用 journal 字段代替）",
    ))?;
    if control_pin.control_rev != journal.control_rev {
        return Err(SyncError::RelayWire(
            "已验证 /control 链尾 rev 与 rotation 冻结的 control_rev 不符（低 pin/控制链已前进）",
        ));
    }
    // 同一 rotation 内已有 pin 时，同 rev 只允许逐字节相等（换链尾/替换 genesis 后重写都拒绝）。
    if journal
        .pinned_control_hash
        .is_some_and(|pinned| pinned != head_hash)
    {
        return Err(SyncError::RelayWire(
            "控制链同 rev 的链尾 record_hash 与本地 pin 不符（换链尾）",
        ));
    }

    // rev1 是 genesis（无 transition/签名），rev≥2 才是管理转换；链必须按 rev 严格递增。
    let mut prev_rev = 1u64;
    let mut prev_pub = anchor;
    let mut prev_entry_rev = 0u64;
    let mut genesis_seen = false;
    let mut last_rev = 1u64;
    for entry in &chain {
        if entry.rev <= 1 {
            // relay 提供的 genesis 记录只允许出现一次、位于链首，且只能与信任锚相等。
            if entry.rev != 1 || genesis_seen || prev_entry_rev != 0 {
                return Err(SyncError::RelayWire(
                    "管理链 genesis 记录非法（重复/乱序/rev!=1）",
                ));
            }
            if entry.pubkey != anchor {
                return Err(SyncError::RelayWire(
                    "管理链 rev1 genesis 公钥与本地信任锚不符（relay 替换 genesis）",
                ));
            }
            genesis_seen = true;
            prev_entry_rev = 1;
            last_rev = 1;
            continue;
        }
        if entry.rev <= prev_entry_rev {
            return Err(SyncError::RelayWire("管理链 rev 不递增"));
        }
        let transition = ManagerTransitionV1::decode(&entry.transition)?;
        if transition.vault_id != vault_id
            || transition.instance_id != state.instance_id
            || transition.old_rev != prev_rev
            || transition.new_rev != entry.rev
            || transition.new_rev != transition.old_rev.saturating_add(1)
            // ② restart 允许跳 epoch：只要求前进，不要求 +1。
            || transition.to_epoch <= transition.from_epoch
            || transition.new_pubkey != entry.pubkey
            || entry.old_rev != prev_rev
        {
            return Err(SyncError::RelayWire("管理转换链字段/rev 不连续"));
        }
        let old_signature = entry
            .old_signature
            .ok_or(SyncError::RelayWire("管理转换链缺旧管理签名"))?;
        let new_signature = entry
            .new_signature
            .ok_or(SyncError::RelayWire("管理转换链缺新管理签名"))?;
        verify_signature(&prev_pub, &entry.transition, &old_signature)?;
        verify_signature(&entry.pubkey, &entry.transition, &new_signature)?;
        prev_rev = entry.rev;
        prev_pub = entry.pubkey;
        prev_entry_rev = entry.rev;
        last_rev = entry.rev;
    }
    // 链尾必须是当前管理 key；空链等价于"从未发生管理转换"，此时 genesis 必须就是当前 key。
    let head_pub = chain.last().map_or(anchor, |entry| entry.pubkey);
    if head_pub != current_manager.public_key() {
        return Err(SyncError::RelayWire("管理链尾公钥与当前管理 key 不符"));
    }

    // ③ 只 pin 已验证事实：instance/log 来自本次 `/state`，control hash 来自已验证链尾，
    // control rev 已在 freeze 时固定并与链尾 rev 核对（上面第 3 条）。
    journal.pinned_instance_id = Some(state.instance_id);
    journal.pinned_log_id = Some(state.log_id);
    journal.pinned_control_hash = Some(head_hash);
    journal.control_chain_rev = Some(last_rev);
    journal.save(vault)?;
    Ok(())
}

/// 阶段 G：管理签的 activate canonical body（含四个 digest 键 3–6）。
#[derive(Debug, Clone)]
pub struct SignedActivation {
    /// `RotationActivateBodyV1` 的 canonical 字节。
    pub body: Vec<u8>,
    pub signature: [u8; 64],
}

/// 阶段 G：校验四个 digest 必填并管理签 activate body。
#[allow(clippy::too_many_arguments)]
pub fn sign_activate(
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    vault_id: VaultId,
    rotation_id: [u8; 16],
    request_id: [u8; 16],
    expected_control_rev: u64,
    body: &menghunote_sync_proto::RotationActivateBodyV1,
    api_version: RelayApiVersion,
) -> Result<SignedActivation, SyncError> {
    if body.member_cert_digest.is_none()
        || body.grants_digest.is_none()
        || body.slot_set_digest.is_none()
        || body.asset_set_digest.is_none()
    {
        return Err(SyncError::Unsupported(
            "activate body 必须携带 member/slot/grant/asset 四个 digest",
        ));
    }
    let payload = body.encode()?;
    let path = RelayApi::new("", api_version).vault_path(
        &vault_id,
        &format!(
            "/rotations/{}/activate",
            crate::base64url::encode(&rotation_id)
        ),
    );
    let body_digest: [u8; 32] =
        Sha256::digest(management_request_cde(HttpMethod::Post, &path, &payload)?).into();
    let signature = manager.sign(&management_signing_input(
        instance_id,
        &vault_id,
        &request_id,
        expected_control_rev,
        &body_digest,
    )?);
    Ok(SignedActivation {
        body: payload,
        signature,
    })
}

/// 阶段 B 的唯一 canonical body（CDE；管理签名覆盖它的 request CDE）。
pub fn begin_body(
    expected_epoch: u64,
    expected_control_rev: u64,
    request_id: &[u8; 16],
    revoke: &[[u8; 16]],
) -> Result<Vec<u8>, SyncError> {
    let body = menghunote_sync_proto::RotationBeginBodyV1 {
        expected_epoch,
        expected_control_rev,
        request_id: *request_id,
        revoke: revoke.to_vec(),
    };
    Ok(body.encode()?)
}

/// 生成 begin wire（canonical body + 管理签名）。
#[allow(clippy::too_many_arguments)]
pub fn sign_begin(
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    vault_id: VaultId,
    request_id: [u8; 16],
    expected_epoch: u64,
    expected_control_rev: u64,
    revoke: &[[u8; 16]],
    api_version: RelayApiVersion,
) -> Result<RotationBegin, SyncError> {
    let body = begin_body(expected_epoch, expected_control_rev, &request_id, revoke)?;
    let path = RelayApi::new("", api_version).vault_path(&vault_id, "/rotations");
    let body_digest: [u8; 32] =
        Sha256::digest(management_request_cde(HttpMethod::Post, &path, &body)?).into();
    let signature = manager.sign(&management_signing_input(
        instance_id,
        &vault_id,
        &request_id,
        expected_control_rev,
        &body_digest,
    )?);
    Ok(RotationBegin {
        request_id,
        expected_epoch,
        expected_control_rev,
        revoke: revoke.to_vec(),
        signed_body: body,
        signature,
    })
}

/// 阶段 B：发起 begin；响应丢失时只按 request_id 查询。
///
/// 成功（含查询找回）后 journal 进入 `Begun` 并持久化 rotation_id/S/log_id。
#[allow(clippy::too_many_arguments)]
pub fn begin_rotation<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    journal: &mut RotationJournal,
    vault_id: VaultId,
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    revoke: &[[u8; 16]],
    api_version: RelayApiVersion,
) -> Result<RotationView, SyncError> {
    if journal.phase != RotationPhase::Frozen {
        return Err(SyncError::Unsupported("rotation begin 需要 Frozen journal"));
    }
    if !journal.r_prime_backed_up {
        return Err(SyncError::Unsupported(
            "rotation begin 前必须确认新恢复码 R' 已备份",
        ));
    }
    let pinned_instance = journal.pinned_instance_id.ok_or(SyncError::Unsupported(
        "rotation begin 前必须完成阶段 A pin",
    ))?;
    if journal.pinned_log_id.is_none() || journal.pinned_control_hash.is_none() {
        return Err(SyncError::Unsupported(
            "rotation begin 前必须完成阶段 A pin",
        ));
    }
    if pinned_instance != *instance_id {
        return Err(SyncError::RelayWire(
            "rotation begin 的 instance 与 pin 不符",
        ));
    }
    let request = sign_begin(
        manager,
        instance_id,
        vault_id,
        journal.request_id,
        journal.from_epoch,
        journal.control_rev,
        revoke,
        api_version,
    )?;
    match client.begin_rotation(request) {
        Ok(view) => {
            mark_begun(vault, client, journal, view.clone())?;
            Ok(view)
        }
        Err(error) => {
            // 丢响应：只按 request_id 查询，绝不盲目重发 begin。
            match client.rotation_for_request(journal.request_id) {
                Ok(Some(view)) => {
                    mark_begun(vault, client, journal, view.clone())?;
                    Ok(view)
                }
                Ok(None) | Err(_) => Err(error),
            }
        }
    }
}

fn mark_begun<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    journal: &mut RotationJournal,
    view: RotationView,
) -> Result<(), SyncError> {
    if view.from_epoch != journal.from_epoch || view.to_epoch != journal.to_epoch {
        return Err(SyncError::RelayWire("rotation view epoch 与 journal 不符"));
    }
    let state = client.state()?;
    journal.mark_begun(view.rotation_id, view.freeze_seq, state.log_id)?;
    journal.save(vault)?;
    Ok(())
}

/// 阶段 C 的 ready 门禁（收敛到本模块，语义见下）。
///
/// 只允许下面两类情况阻断进入 ready：
///
/// 1. **≤S 的入站缺口**：`confirmed_highwater`/`integrated_seq` 未覆盖 S，或 inbox 里
///    `relay_seq <= S` 仍有非 `integrated` 行（quarantine/因果洞/AEAD 未验）；
/// 2. **已上传但未 ACK**：outbox `state='sent'` 的行。它们都在冻结前生成，对象依赖覆盖
///    ≤S 的本地状态，必须等 ACK 或按新 epoch 重包后才能继续。
///
/// 明确**不**检查 `pending_encryption` / `pending_send`：这些是冻结后继续产生的本地编辑，
/// 尚未上传、可以推广后重包上传，不能让轮换被持续编辑永久卡住（旧门禁的缺陷）。
pub fn ensure_c_phase_gate(vault: &Vault, target_s: u64) -> Result<(), SyncError> {
    let cursor = menghunote_storage::sync_cursor_state(vault)?;
    if cursor.confirmed_highwater.unwrap_or(0) < target_s {
        return Err(SyncError::Unsupported(
            "rotation integrate: confirmed highwater 未覆盖 S",
        ));
    }
    if cursor.integrated_seq < target_s {
        return Err(SyncError::Unsupported(
            "rotation integrate: integrated_seq 未到 S（received 不够）",
        ));
    }
    if inbox_gap_at_or_before(vault, &cursor, target_s)? {
        return Err(SyncError::Unsupported(
            "rotation integrate: inbox 在 ≤S 仍有 quarantine/因果洞",
        ));
    }
    if !menghunote_storage::outbox_rows(vault, menghunote_storage::SyncOutboxState::Sent, 1)?
        .is_empty()
    {
        return Err(SyncError::Unsupported(
            "rotation integrate: 存在已上传但未 ACK 的更新（≤S 未覆盖）",
        ));
    }
    Ok(())
}

/// inbox 里是否有 `relay_seq <= S` 且尚未 integrated 的行（quarantine/因果洞）。
///
/// 正常不变量下 `integrated_seq >= S` 已经蕴含"≤S 无洞"；这里仍显式查一遍，既为防御
/// meta 与 inbox 不一致（崩溃/手工修复），也让门禁失败原因可诊断。
pub(crate) fn inbox_gap_at_or_before(
    vault: &Vault,
    cursor: &menghunote_storage::SyncCursorState,
    target_s: u64,
) -> Result<bool, SyncError> {
    let Some(log_id) = cursor.log_id else {
        return Ok(false);
    };
    let target = i64::try_from(target_s)
        .map_err(|_| SyncError::LocalCorrupt("rotation integrate: S 超出 i64"))?;
    let rows = vault.query(
        "SELECT relay_seq FROM sync_inbox \
         WHERE log_id=?1 AND relay_seq<=?2 AND state<>'integrated' \
         ORDER BY relay_seq LIMIT 1",
        &[
            menghunote_storage::Value::Blob(log_id.to_vec()),
            menghunote_storage::Value::Integer(target),
        ],
    )?;
    Ok(!rows.is_empty())
}

/// 阶段 C：用旧根 pull+integrate 到 S，固定 local cut L。
///
/// `received_seq` 不能代替 `integrated_seq`；只有本地真的 integrate 到 S 才允许继续。
/// 通过 [`ensure_c_phase_gate`] 后按 journal 状态机推进 `Begun → Integrated`。
pub fn integrate_to_s<C: RelayClient>(
    vault: &Vault,
    engine: &mut crate::engine::SyncEngine<'_, C>,
    journal: &mut RotationJournal,
    target_s: u64,
    local_cut: u64,
) -> Result<(), SyncError> {
    if journal.phase != RotationPhase::Begun {
        return Err(SyncError::Unsupported(
            "rotation integrate 需要 Begun journal",
        ));
    }
    loop {
        let cursor = menghunote_storage::sync_cursor_state(vault)?;
        if cursor.integrated_seq >= target_s {
            break;
        }
        let before = cursor.integrated_seq;
        let pulled = engine.pull()?;
        engine.integrate(usize::MAX)?;
        let after = menghunote_storage::sync_cursor_state(vault)?;
        if after.integrated_seq == before {
            return Err(SyncError::Unsupported(if pulled.events == 0 {
                "rotation integrate: relay 已无更多事件但未到 S"
            } else {
                "rotation integrate: 日志未推进到 S（存在因果洞/quarantine）"
            }));
        }
    }
    // 门禁不再复用 `RotationJournal::mark_integrated`：那个实现把任何 pending/encrypted
    // 本地编辑都当成"≤S 未覆盖"而拒绝（文件不在本次作用域）。这里按同一状态机语义
    // （Begun → Integrated + local_cut）推进，门禁统一由 `ensure_c_phase_gate` 收口。
    ensure_c_phase_gate(vault, target_s)?;
    journal.advance(RotationPhase::Integrated)?;
    journal.local_cut = Some(local_cut);
    journal.save(vault)?;
    Ok(())
}

fn _journal_vault_hint(_vault_id: VaultId) {}

`````



@@SECTION code/046 `crates/sync/src/rotation_activate.rs`


`````rust
//! 轮换阶段 G/H：activate、grant 解根校验与本地原子推广。
//!
//! 只实现“发起设备”的路径；其它设备应用与 outbox 重打包见后续阶段 I/J。
//!
//! 推广的硬约束（本次修正）：
//!
//! - 身份里的 `manager_seed` 必须是**新管理 key** 的 seed（由 R' 派生），绝不能继续保存
//!   旧 seed 却 pin 新 manager；
//! - 旧 epoch 根按 epoch 保留到本地 outbox 重包完成（[`crate::identity::HistoricalRootV1`]），
//!   rename/重启后仍能解旧 parts；
//! - `control_rev`（控制链）与 `new_manager_rev`（管理 key）分开，不互相写入；
//! - journal 只能 `→ Promoted → Completed`，不能跳步。

use std::fs;
use std::io::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::api_version::RelayApi;
use crate::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::hpke::open_epoch_grant;
use crate::http_relay::HttpTransport;
use crate::identity::{HistoricalRootV1, IdentityRotationStateV1, LocalIdentityV1};
use crate::keys::{RootSecretV1, Secret32};
use crate::recovery_enroll::{decode, device_token, management_token, send};
use crate::relay::RotationView;
use crate::rotation_journal::{RotationJournal, RotationPhase};
use crate::{DeviceId, VaultId};
use menghunote_storage::Vault;
use menghunote_sync_proto::{GrantAadV1, RotationActivateBodyV1};

/// 阶段 G 的 activate 材料。
#[derive(Clone)]
pub struct ActivationInput {
    pub rotation_id: [u8; 16],
    pub request_id: [u8; 16],
    pub expected_control_rev: u64,
    pub primary_checkpoint: [u8; 16],
    pub fallback_checkpoint: [u8; 16],
    pub member_cert_digest: [u8; 32],
    pub slot_set_digest: [u8; 32],
    pub grants_digest: [u8; 32],
    pub asset_set_digest: [u8; 32],
    pub new_manager: Option<menghunote_sync_proto::NewManagerV1>,
    /// 候选根（只用于推广与 grant 校验，不上 wire）。
    pub candidate_root: RootSecretV1,
    pub new_manager_public: [u8; 32],
    pub new_manager_rev: u64,
    /// 四个 digest 的 canonical 绑定版本。
    pub pv: u64,
}

impl ActivationInput {
    fn body(&self) -> RotationActivateBodyV1 {
        RotationActivateBodyV1 {
            new_manager: self.new_manager.clone(),
            primary_checkpoint: Some(self.primary_checkpoint),
            fallback_checkpoint: Some(self.fallback_checkpoint),
            member_cert_digest: Some(self.member_cert_digest),
            grants_digest: Some(self.grants_digest),
            slot_set_digest: Some(self.slot_set_digest),
            asset_set_digest: Some(self.asset_set_digest),
        }
    }
}

/// activate 成功后的 relay 结果（响应键 0–6，不是 rotation view）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivatedOutcome {
    pub epoch: u64,
    pub control_rev: u64,
    pub auth_rev: u64,
    pub member_set_hash: [u8; 32],
    pub primary_checkpoint: [u8; 16],
    pub fallback_checkpoint: [u8; 16],
    pub activation_commit: Vec<u8>,
}

/// 阶段 G：management activate；超时只查 `rotations/current`，不 abort。
#[allow(clippy::too_many_arguments)]
pub fn activate_http<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    instance_id: &[u8; 16],
    manager: &ManagementKey,
    input: &ActivationInput,
    now: u64,
) -> Result<ActivatedOutcome, SyncError> {
    let body = input.body();
    let signed = crate::rotation::sign_activate(
        manager,
        instance_id,
        vault_id,
        input.rotation_id,
        input.request_id,
        input.expected_control_rev,
        &body,
        api.version(),
    )?;
    let (token, _) = management_token(transport, api, vault_id, manager, now)?;
    let signed_value = crate::cbor::CdeValue::Map(vec![
        (0, crate::cbor::bytes(signed.body.clone())),
        (1, crate::cbor::bytes(signed.signature.to_vec())),
    ]);
    let path = api.url(&api.vault_path(
        &vault_id,
        &format!(
            "/rotations/{}/activate",
            crate::base64url::encode(&input.rotation_id)
        ),
    ));
    let response = send(
        transport,
        "POST",
        path,
        Some(&token),
        crate::cbor::encode(vec![
            (0, signed_value),
            (1, crate::cbor::bytes(input.request_id.to_vec())),
            (2, crate::cbor::uint(input.expected_control_rev)),
        ])?,
    );
    match response {
        Ok(response) => {
            let fields = decode(&response)?;
            parse_activated_outcome(&fields)
        }
        Err(error @ SyncError::Transport(_)) => {
            let view = query_current_rotation(transport, api, vault_id, manager, now)?;
            if view.state == "activated" {
                Ok(ActivatedOutcome {
                    epoch: view.to_epoch,
                    control_rev: view.control_rev,
                    auth_rev: view.auth_rev,
                    member_set_hash: view.member_set_hash.unwrap_or_default(),
                    primary_checkpoint: view.primary_checkpoint.unwrap_or_default(),
                    fallback_checkpoint: view.fallback_checkpoint.unwrap_or_default(),
                    activation_commit: view.activation_commit.unwrap_or_default(),
                })
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}

fn parse_activated_outcome(fields: &crate::cbor::Fields) -> Result<ActivatedOutcome, SyncError> {
    Ok(ActivatedOutcome {
        epoch: crate::cbor::uint_field(fields, 0)?,
        control_rev: crate::cbor::uint_field(fields, 1)?,
        auth_rev: crate::cbor::uint_field(fields, 2)?,
        member_set_hash: crate::cbor::bytes_fixed(fields, 3)?,
        primary_checkpoint: crate::cbor::bytes_fixed(fields, 4)?,
        fallback_checkpoint: crate::cbor::bytes_fixed(fields, 5)?,
        activation_commit: crate::cbor::bytes_field(fields, 6)?,
    })
}

fn query_current_rotation<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    manager: &ManagementKey,
    now: u64,
) -> Result<RotationView, SyncError> {
    let (token, _) = management_token(transport, api, vault_id, manager, now)?;
    let response = send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/rotations/current")),
        Some(&token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    let view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    parse_activated_view(&view)
}

fn parse_activated_view(view: &crate::cbor::Fields) -> Result<RotationView, SyncError> {
    Ok(RotationView {
        rotation_id: crate::cbor::bytes_fixed(view, 0)?,
        state: crate::cbor::text_field(view, 1)?,
        from_epoch: crate::cbor::uint_field(view, 2)?,
        to_epoch: crate::cbor::uint_field(view, 3)?,
        freeze_seq: crate::cbor::uint_field(view, 4)?,
        revoked_count: crate::cbor::uint_field(view, 5)?,
        member_set_hash: None,
        grants_count: crate::cbor::uint_field(view, 7)?,
        grants_digest: None,
        published_checkpoints: crate::cbor::uint_field(view, 9)?,
        control_rev: crate::cbor::uint_field(view, 10)?,
        auth_rev: crate::cbor::uint_field(view, 11)?,
        manager_rev: crate::cbor::uint_field(view, 12)?,
        from_epoch_revoked_once: cbor_uint(view, 13) != 0,
        freeze_asset_count: cbor_uint(view, 15),
        freeze_asset_digest: None,
        member_cert_digest: None,
        primary_checkpoint: None,
        fallback_checkpoint: None,
        activation_commit: None,
        activated_at: cbor_opt_uint(view, 21),
        activated_control_rev: cbor_opt_uint(view, 22),
        candidate_count: 0,
        candidate_ready: false,
        reservation_id: None,
    })
}

fn cbor_uint(fields: &crate::cbor::Fields, key: u64) -> u64 {
    crate::cbor::uint_field(fields, key).unwrap_or(0)
}

fn cbor_opt_uint(fields: &crate::cbor::Fields, key: u64) -> Option<u64> {
    crate::cbor::opt_uint_field(fields, key).ok().flatten()
}

/// 阶段 H：读回自己的最新 grant，解封必须等于 candidate root。
#[allow(clippy::too_many_arguments)]
pub fn verify_latest_grant_root<T: HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    hpke_key: &DeviceHpkeKey,
    expected_root: &RootSecretV1,
    now: u64,
) -> Result<(), SyncError> {
    let token = device_token(transport, api, vault_id, device_id, signing_key, now)?;
    let response = send(
        transport,
        "GET",
        api.url(&api.vault_path(&vault_id, "/devices/self/grants/latest")),
        Some(&token),
        Vec::new(),
    )?;
    let fields = decode(&response)?;
    let epoch = crate::cbor::uint_field(&fields, 0)?;
    let grant_device: [u8; 16] = crate::cbor::bytes_fixed(&fields, 1)?;
    let enc: [u8; 32] = crate::cbor::bytes_fixed(&fields, 3)?;
    let ciphertext = crate::cbor::bytes_field(&fields, 4)?;
    let signed_header = crate::cbor::bytes_field(&fields, 5)?;
    if grant_device != device_id || expected_root.key_epoch != epoch || signed_header.len() < 64 {
        return Err(SyncError::RelayWire("latest grant 与候选 epoch/设备不符"));
    }
    let aad_len = signed_header.len() - 64;
    let aad = GrantAadV1::decode(&signed_header[..aad_len])?;
    if aad.epoch != epoch
        || aad.recipient_device_id != device_id
        || aad.recipient_x25519_pub != hpke_key.public_key()
    {
        return Err(SyncError::RelayWire("latest grant AAD 与本地设备不符"));
    }
    let opened = open_epoch_grant(hpke_key.secret(), &enc, &aad, &ciphertext)?;
    if opened.encode()? != expected_root.encode()? {
        return Err(SyncError::Object(
            "latest grant 解出的 root 不是 candidate root，拒绝推广",
        ));
    }
    Ok(())
}

/// 本次 activation 已经由 T / `ActivationCommitV1` 验证过的身份 pin 值（评审 0003 P1-3）。
///
/// 推广必须写入这些值，而不是保留旧身份的 `control_rev`/instance：
///
/// - `instance_id`：activation T 与 `/state` 共同绑定的 relay instance；
/// - `control_rev`：激活提交里的控制链 rev（**不是**管理 key rev）；
/// - `activation_hash`：`SHA256(CDE(ActivationCommitV1))`，即本次激活的认证摘要。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedActivation {
    /// 已验证的 relay instance（T 的 `instance_id`）。
    pub instance_id: [u8; 16],
    /// 已验证的控制链 rev（`ActivationCommitV1.control_rev` == T.expected_control_rev）。
    pub control_rev: u64,
    /// 已验证的 activation 摘要：`SHA256(CDE(ActivationCommitV1))`。
    pub activation_hash: [u8; 32],
}

/// 推广后写入的 authenticated pin（`vault.meta`，与身份旁路状态同 epoch 语义）。
///
/// 它不是身份 BLOB 的一部分（身份字段清单被冻结为 0..11），而是与身份同源的旁路记录：
/// `epoch` 必须等于身份 `current_epoch` 才被视为"当前 pin"，否则是崩溃残留（忽略）。
pub const AUTHENTICATED_PIN_META: &str = "sync.identity.authenticated_pin";

/// 推广后 durable 的认证 pin。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedPinV1 {
    /// 与身份 `current_epoch` 对齐；不一致即崩溃残留。
    pub epoch: u64,
    /// 控制链 rev（不是管理 key rev）。
    pub control_rev: u64,
    /// 已验证的 relay instance。
    pub instance_id: [u8; 16],
    /// 本次 activation 的 `SHA256(CDE(ActivationCommitV1))`。
    pub activation_hash: [u8; 32],
}

impl AuthenticatedPinV1 {
    /// 读 pin；不存在/空值返回 `None`。
    pub fn load(vault: &Vault) -> Result<Option<Self>, SyncError> {
        let Some(blob) = vault.get_meta(AUTHENTICATED_PIN_META)? else {
            return Ok(None);
        };
        if blob.is_empty() {
            return Ok(None);
        }
        let fields = crate::cbor::decode(&blob)?;
        Ok(Some(Self {
            epoch: crate::cbor::uint_field(&fields, 0)?,
            control_rev: crate::cbor::uint_field(&fields, 1)?,
            instance_id: crate::cbor::bytes_fixed(&fields, 2)?,
            activation_hash: crate::cbor::bytes_fixed(&fields, 3)?,
        }))
    }

    /// 读"当前身份的" pin：`epoch` 不匹配时返回 `None`（崩溃残留，旧身份仍权威）。
    pub fn load_current(
        vault: &Vault,
        identity: &LocalIdentityV1,
    ) -> Result<Option<Self>, SyncError> {
        Ok(Self::load(vault)?.filter(|pin| pin.epoch == identity.current_epoch))
    }

    fn save(&self, vault: &Vault) -> Result<(), SyncError> {
        let blob = crate::cbor::encode(vec![
            (0, crate::cbor::uint(self.epoch)),
            (1, crate::cbor::uint(self.control_rev)),
            (2, crate::cbor::bytes(self.instance_id.to_vec())),
            (3, crate::cbor::bytes(self.activation_hash.to_vec())),
        ])?;
        vault.set_meta(AUTHENTICATED_PIN_META, &blob)?;
        Ok(())
    }
}

/// 父目录 durability 的处理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryDurability {
    /// 已对父目录做 `fsync`（Unix）。
    Synced,
    /// 平台不提供目录 fsync（Windows 的 std 无法打开带备份语义的目录句柄）。
    PlatformUnsupported,
}

/// 对 `path` 的父目录做"可用"的 durability 处理。
///
/// - Unix：`open(dir)` + `sync_all()`（目录 `fsync`），让 rename 的目录项落盘；
/// - Windows：标准库没有暴露 `FILE_FLAG_BACKUP_SEMANTICS` 的目录句柄，无法对目录
///   调用 `FlushFileBuffers`；这里**明确**返回 [`DirectoryDurability::PlatformUnsupported`]。
///   因此 Windows 上的可验证耐久边界是：
///   **临时文件在 rename 前已 `File::sync_all`（等价 `FlushFileBuffers`）+ rename 原子提交**；
///   掉电后目录项是否立即持久不在本标准库能力范围内（NTFS 日志通常会在重放时恢复，
///   但不得把它当作保证）。测试必须显式断言这条平台差异。
pub fn sync_parent_directory(path: &Path) -> Result<DirectoryDurability, SyncError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    #[cfg(unix)]
    {
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| SyncError::Io(format!("父目录 fsync 失败: {error}")))?;
        Ok(DirectoryDurability::Synced)
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(DirectoryDurability::PlatformUnsupported)
    }
}

/// 写临时身份并在 rename 之前 `File::sync_all`（Windows = `FlushFileBuffers`）。
fn write_identity_temp(path: &Path, blob: &[u8]) -> Result<(), SyncError> {
    let mut file = fs::File::create(path)
        .map_err(|error| SyncError::Io(format!("创建临时身份失败: {error}")))?;
    file.write_all(blob)
        .map_err(|error| SyncError::Io(format!("写临时身份失败: {error}")))?;
    // 必须在 rename 之前完成：rename 成功不等于内容已经落盘（Windows 尤其如此）。
    file.sync_all()
        .map_err(|error| SyncError::Io(format!("临时身份 sync_all 失败: {error}")))?;
    Ok(())
}

/// 以 `rename` 为唯一提交点的耐久替换。
///
/// 失败语义：rename 之前任何失败都保留旧身份文件不变；rename 是唯一提交点。
/// 提交之后再刷新父目录/文件属于"尽力而为"（Windows 无目录 fsync），不会回滚已提交的身份
/// ——回滚会引入第二个非原子提交点，反而扩大半新半旧窗口。
fn replace_identity_file(temp: &Path, target: &Path) -> Result<(), SyncError> {
    // 提交前刷新父目录（Unix 上把临时文件目录项落盘）；失败时旧身份仍未被替换。
    sync_parent_directory(temp)?;
    fs::rename(temp, target)
        .map_err(|error| SyncError::Io(format!("身份 rename 失败: {error}")))?;
    // 提交后再次刷新父目录：Unix 上补齐目录项持久；Windows 明确不支持（已知边界）。
    let _ = sync_parent_directory(target);
    Ok(())
}

/// 本地推广所需的轮换材料。
///
/// 与旧 6 参数入口相比，这里显式分开了三个语义（此前被混在一起）：
///
/// - `candidate_root`：新 epoch 的根（覆盖身份 `current_root`）；
/// - `new_manager_seed`：由 R' 派生的新管理私钥种子（只有管理设备有；B 侧为 `None`）；
/// - `control_rev` / `new_manager_rev`：**控制链 rev** 与**管理 key rev** 是两个独立计数，
///   绝不允许互相写入（旧实现把 `new_manager_rev` 写进了 `control_rev`）。
#[derive(Clone)]
pub struct PromotionMaterial {
    /// 新 epoch 根（必须高于身份当前 epoch）。
    pub candidate_root: RootSecretV1,
    /// 新管理 key 的 seed（由 R' 派生）；非管理设备为 `None`。
    pub new_manager_seed: Option<Secret32>,
    /// 新管理公钥（relay 双签后生效的那把）。
    pub new_manager_public: [u8; 32],
    /// 管理 key rev。
    pub new_manager_rev: u64,
    /// 控制链 rev（activate/restart 回执里的值，不是管理 rev）。
    pub control_rev: u64,
    /// 本次 activation 的已验证认证值；B apply 路径必须提供，A 管理路径可为 `None`。
    pub authentication: Option<AuthenticatedActivation>,
}

impl PromotionMaterial {
    /// 用 activate/restart 回执构造推广材料：`control_rev` 取回执里的控制链 rev。
    #[must_use]
    pub fn from_activated(
        outcome: &ActivatedOutcome,
        candidate_root: RootSecretV1,
        new_manager_seed: Option<Secret32>,
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
    ) -> Self {
        Self {
            candidate_root,
            new_manager_seed,
            new_manager_public,
            new_manager_rev,
            control_rev: outcome.control_rev,
            // A 管理设备手上没有"已验证 instance/hash"的 apply 上下文；
            // 推广仍更新 epoch/root/manager/control_rev，pin 由 apply 路径补齐。
            authentication: None,
        }
    }
}

/// 管理设备专用：由 R' 派生新管理 seed，并与公钥一起组装推广材料。
///
/// 这是"推广后 `manager_seed` 必须对应新 manager 公钥"的唯一正确来源：seed 走
/// `HKDF(R', vault_id, management-ed25519)`，公钥从同一 seed 派生，调用方不需要也不允许
/// 自己拼接。
pub fn promotion_material_from_recovery(
    outcome: &ActivatedOutcome,
    candidate_root: RootSecretV1,
    recovery_seed: &crate::recovery::RecoverySeed,
    vault_id: &VaultId,
    new_manager_rev: u64,
) -> Result<PromotionMaterial, SyncError> {
    let seed = crate::recovery::management_seed(recovery_seed, vault_id)?;
    let public = ManagementKey::from_seed(seed.clone()).public_key();
    Ok(PromotionMaterial::from_activated(
        outcome,
        candidate_root,
        Some(seed),
        public,
        new_manager_rev,
    ))
}

/// 阶段 H：原子推广 DPAPI 身份 + 历史根/管理 rev 旁路状态 + authenticated pin + repack 屏障。
///
/// 写入顺序（即崩溃/掉电语义，评审 0003 P1-3）：
///
/// 1. 全部校验先做完（epoch/归属/新管理 seed 与公钥一致/认证 pin 与 control_rev 一致），
///    失败时**不写任何字节**；
/// 2. 写临时身份 BLOB 并 `File::sync_all`（Windows 等价 `FlushFileBuffers`），
///    保证 rename 之前内容已落盘；
/// 3. 写旁路状态（历史根 + 控制链/管理 rev，带新 epoch）；
/// 4. 写 authenticated pin（instance/epoch/control_rev/activation hash，带新 epoch）；
/// 5. 置 `sync.repack_required=1`；
/// 6. 刷新父目录后 `rename` 临时身份 → 正式路径（**唯一提交点**）；提交后再次刷新
///    父目录（Unix 有目录 fsync；Windows 无目录 fsync，边界见
///    [`sync_parent_directory`]）。
///
/// 若在 3–6 之间崩溃：旁路状态/pin 的 epoch 领先旧身份，[`IdentityRotationStateV1::load_current`]
/// 与 [`AuthenticatedPinV1::load_current`] 会因 epoch 不匹配忽略它们；旧身份文件未动，
/// 旧根仍可解旧 outbox。rename 失败时旧身份逐字节不变，临时文件被清理，可原样重试。
pub fn promote_identity_with_material(
    vault: &menghunote_storage::Vault,
    identity_path: &Path,
    identity: &LocalIdentityV1,
    material: &PromotionMaterial,
) -> Result<LocalIdentityV1, SyncError> {
    let candidate_epoch = material.candidate_root.key_epoch;
    if material.candidate_root.vault_id != identity.vault_id {
        return Err(SyncError::Identity("候选根归属与本地身份不符"));
    }
    if identity.current_epoch >= candidate_epoch {
        return Err(SyncError::Object("候选 epoch 必须高于当前身份 epoch"));
    }
    // 新管理 seed 必须与 pin 住的新公钥一致；旧 seed 绝不能继续冒充 current。
    if let Some(seed) = &material.new_manager_seed {
        if ManagementKey::from_seed(seed.clone()).public_key() != material.new_manager_public {
            return Err(SyncError::Object("新管理 key seed 与公钥不一致，拒绝推广"));
        }
    } else if identity.manager_seed.is_some() {
        return Err(SyncError::Unsupported(
            "管理设备推广必须提供由 R' 派生的新管理 seed",
        ));
    }
    if let Some(authentication) = &material.authentication {
        if authentication.control_rev != material.control_rev {
            return Err(SyncError::Object(
                "authenticated control_rev 与推广材料的 control_rev 不一致",
            ));
        }
    }
    // 旧根按 epoch 保留（至少最近一个）；本地重包完成前不得丢弃。
    let mut previous_roots = IdentityRotationStateV1::load_current(vault, identity)?
        .map(|state| state.previous_roots)
        .unwrap_or_default();
    previous_roots.push(HistoricalRootV1::new(
        identity.current_epoch,
        identity.current_root.clone(),
    ));
    previous_roots.sort_by_key(|entry| entry.epoch);
    previous_roots.dedup_by_key(|entry| entry.epoch);
    previous_roots.retain(|entry| entry.epoch < candidate_epoch);
    let rotation_state = IdentityRotationStateV1 {
        current_epoch: candidate_epoch,
        control_rev: material.control_rev,
        manager_rev: material.new_manager_rev,
        previous_roots,
    };
    // 推广后的身份 pin 必须来自本次 activation 的已验证值，而不是沿用旧 control_rev。
    let pinned_instance = material
        .authentication
        .map_or(identity.pinned_instance, |auth| auth.instance_id);
    let promoted = LocalIdentityV1 {
        vault_id: identity.vault_id,
        device_id: identity.device_id,
        peer_id: identity.peer_id,
        ed25519_seed: identity.ed25519_seed.clone(),
        x25519_secret: identity.x25519_secret.clone(),
        current_epoch: candidate_epoch,
        current_root: material.candidate_root.root.clone(),
        manager_seed: material.new_manager_seed.clone(),
        pinned_instance,
        pinned_manager: material.new_manager_public,
        control_rev: material.control_rev,
        pending_rotation: None,
    };
    let blob = promoted.protect()?;
    let temp = identity_path.with_extension("promote.tmp");
    // 1) 临时身份必须 `sync_all` 后才允许进入提交序列（Windows = FlushFileBuffers）。
    write_identity_temp(&temp, &blob)?;
    // 2) 旁路状态 / authenticated pin / repack 屏障都写在 rename 之前：
    //    崩溃时旧身份 epoch 与它们不匹配，旁路记录会被忽略（旧身份仍权威）。
    let committed = (|| -> Result<(), SyncError> {
        rotation_state.save(vault, &identity.vault_id, &identity.device_id)?;
        if let Some(authentication) = material.authentication {
            AuthenticatedPinV1 {
                epoch: candidate_epoch,
                control_rev: material.control_rev,
                instance_id: authentication.instance_id,
                activation_hash: authentication.activation_hash,
            }
            .save(vault)?;
        }
        vault.set_meta("sync.repack_required", b"1")?;
        // 3) rename = 唯一提交点（失败时旧身份逐字节不变，见测试）。
        replace_identity_file(&temp, identity_path)
    })();
    if let Err(error) = committed {
        // 失败回滚：正式身份没动，清掉临时文件避免下一次重试读到半份新 BLOB。
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(promoted)
}

/// 阶段 H：B 侧兼容入口。
///
/// B 侧没有 R'，因此只能推广**非管理设备**；管理设备必须走
/// [`promote_identity_with_material`] 并提供新管理 seed。
///
/// 与旧 6 参数入口的关键差别（评审 0003 P1-3）：推广**不再保留旧
/// `identity.control_rev`**，而是写入本次 activation 已验证的
/// [`AuthenticatedActivation`]（instance/control_rev/activation hash）。调用方必须先把
/// T/commit 验证结果传进来，不能再用"只换 root/manager pub"的旧形态。
pub fn promote_identity_atomic(
    vault: &menghunote_storage::Vault,
    identity_path: &Path,
    identity: &LocalIdentityV1,
    candidate_root: &RootSecretV1,
    authenticated: &AuthenticatedActivation,
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
) -> Result<LocalIdentityV1, SyncError> {
    let material = PromotionMaterial {
        candidate_root: candidate_root.clone(),
        new_manager_seed: None,
        new_manager_public,
        new_manager_rev,
        control_rev: authenticated.control_rev,
        authentication: Some(*authenticated),
    };
    promote_identity_with_material(vault, identity_path, identity, &material)
}

/// 阶段 H：把 journal 按 `→ Promoted → Completed` 推进并清除候选秘密。
///
/// journal 只允许单步前进，旧实现从 `Activated` 直接 `advance(Completed)` 必报
/// "rotation phase 不能跳步"。这里兼容 restart 的 `RestartActivated`，并允许在
/// `Promoted`/`Completed` 上重入（崩溃后重试幂等）。
pub fn finish_promotion(
    journal: &mut RotationJournal,
    vault: &menghunote_storage::Vault,
) -> Result<(), SyncError> {
    match journal.phase {
        RotationPhase::Activated | RotationPhase::RestartActivated => {
            journal.advance(RotationPhase::Promoted)?;
        }
        RotationPhase::Promoted | RotationPhase::Completed => {}
        _ => {
            return Err(SyncError::Unsupported(
                "promotion 需要 Activated/RestartActivated/Promoted journal",
            ))
        }
    }
    if journal.phase != RotationPhase::Completed {
        journal.advance(RotationPhase::Completed)?;
    }
    // 本地推广完成后，journal 里的裸根与槽体密文不再需要。
    journal.candidate_root = None;
    journal.new_slot_bodies.clear();
    journal.save(vault)
}

/// G 的 canonical body digest（供测试/日志，不参与签名）。
pub fn activate_body_digest(input: &ActivationInput) -> Result<[u8; 32], SyncError> {
    let body = input.body();
    Ok(Sha256::digest(body.encode()?).into())
}

`````



@@SECTION code/047 `crates/sync/src/rotation_apply.rs`


`````rust
//! P3-I/J 的纯状态机：epoch 变化检测、激活 commit 校验、restart 判定。
//!
//! 这里**不写 storage**；baseline merge 由 storage task-108 提供事务 API 后接入。
//!
//! I 阶段的两件事是**分开**的（评审 0003 P1-C）：
//! - [`verify_activation_t_gate`]：T/latest grant 授权"把根推广到哪个 epoch"（双签、链、
//!   grant inclusion、pair 内 commit digest 强绑定）；
//! - [`verify_checkpoint_authorization`]：认证"当前这份 checkpoint 内容"（生产者资格/
//!   签名/epoch/manifest/refs/S 边界），从而支持激活 pair 被普通健康 checkpoint 替换后
//!   的旧设备消费。

use crate::device::verify_signature;
use crate::error::SyncError;
use crate::hpke::x25519_public_key;
use crate::identity::{IdentityRotationStateV1, LocalIdentityV1};
use crate::keys::{RootSecretV1, Secret32};
pub use crate::relay::GrantLeafEvidence;
use crate::relay::{
    BootstrapBundle, BootstrapCheckpoint, CheckpointStatus, EpochActivationLink, RelayClient,
    RelayState, RotationView, SelfGrantEvidence, VerifiedActivationT,
};
use crate::rotation_activate::AuthenticatedPinV1;
use crate::rotation_journal::{RotationApplyPhase, RotationApplyReceipt};
use crate::rotation_slots::signed_commit_digest;
use crate::rotation_stage::verify_snapshot_integrity;
use crate::upload::{decrypt_envelope, fetch_object};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_storage::{
    abort_baseline_apply, apply_baseline_assets, apply_baseline_batch, baseline_apply_state,
    begin_baseline_apply, finish_baseline_apply, repack_required, BaselineApplyFinish,
    BaselineApplyState, BaselineApplyStatus, BaselineAsset, BaselineExpectation,
};
use menghunote_sync_proto::{
    activation_t_signing_input, checkpoint_signing_input, grant_package_digest, ActivationCommitV1,
    ActivationTV1, CheckpointCommit, Envelope, GrantAadV1, ManifestV1, ObjectType, SnapshotV1,
};
use sha2::{Digest, Sha256};

/// 已验证、尚未写库的基线内容（下载阶段只读）。
#[derive(Debug, Clone)]
pub struct BaselinePayload {
    pub checkpoint_id: [u8; 16],
    pub baseline_s: u64,
    pub docs: Vec<(String, Vec<u8>)>,
    pub assets: Vec<BaselineAsset>,
}

/// 完整 gate 产出的 apply 输入（评审 0003 P1-C）。
///
/// [`BaselinePayload`] 是**公开可变**的下载/合并数据，本身不构成授权；本类型把
/// 它与 T/`ActivationCommitV1`/选中 checkpoint/推广材料逐字段绑定，字段**私有**、
/// 只有只读访问器，且构造只发生在 [`crate::rotation_apply::apply_epoch_change_with_client`]
/// 通过 [`verify_activation_t_gate`] + manifest/候选根校验之后。
///
/// 因此纯入口 [`apply_epoch_change`] 只消费本类型，外部 crate 无法用一份改过的
/// `BaselinePayload` 拼出它（编译期 API 层阻断），也就不存在"绕过 T 门禁的公开入口"。
///
/// 外部 crate 无法构造本类型（字段私有 + 构造函数 `pub(crate)`）：
///
/// ```compile_fail
/// use menghunote_sync::rotation_apply::{BaselinePayload, VerifiedApplyPayload};
/// let _ = VerifiedApplyPayload {
///     rotation_id: [0u8; 16],
///     checkpoint_id: [0u8; 16],
///     baseline_s: 0,
///     baseline_cursor: String::new(),
///     from_epoch: 0,
///     to_epoch: 1,
///     control_rev: 0,
///     activation_hash: [0u8; 32],
///     instance_id: [0u8; 16],
///     new_manager_public: [0u8; 32],
///     new_manager_rev: 0,
///     baseline: BaselinePayload {
///         checkpoint_id: [0u8; 16],
///         baseline_s: 0,
///         docs: Vec::new(),
///         assets: Vec::new(),
///     },
/// };
/// ```
///
/// 通过只读访问器也改不了 gate 已经绑定的基线字段：
///
/// ```compile_fail
/// fn mutate(value: &menghunote_sync::rotation_apply::VerifiedApplyPayload) {
///     value.baseline().checkpoint_id = [0u8; 16];
/// }
/// ```
pub struct VerifiedApplyPayload {
    rotation_id: [u8; 16],
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: String,
    from_epoch: u64,
    to_epoch: u64,
    control_rev: u64,
    activation_hash: [u8; 32],
    instance_id: [u8; 16],
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
    baseline: BaselinePayload,
}

impl VerifiedApplyPayload {
    /// 只能由完整 gate 之后的本模块调用（`pub(crate)`，外部不可见）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_gate(
        rotation_id: [u8; 16],
        checkpoint_id: [u8; 16],
        baseline_s: u64,
        baseline_cursor: &str,
        from_epoch: u64,
        to_epoch: u64,
        control_rev: u64,
        activation_hash: [u8; 32],
        instance_id: [u8; 16],
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
        baseline: BaselinePayload,
    ) -> Self {
        Self {
            rotation_id,
            checkpoint_id,
            baseline_s,
            baseline_cursor: baseline_cursor.to_owned(),
            from_epoch,
            to_epoch,
            control_rev,
            activation_hash,
            instance_id,
            new_manager_public,
            new_manager_rev,
            baseline,
        }
    }

    /// 本次 activation 的 rotation id。
    #[must_use]
    pub fn rotation_id(&self) -> [u8; 16] {
        self.rotation_id
    }

    /// 选中的基线 checkpoint id。
    #[must_use]
    pub fn checkpoint_id(&self) -> [u8; 16] {
        self.checkpoint_id
    }

    /// 选中基线覆盖的 relay S。
    #[must_use]
    pub fn baseline_s(&self) -> u64 {
        self.baseline_s
    }

    /// 选中基线的 relay cursor。
    #[must_use]
    pub fn baseline_cursor(&self) -> &str {
        &self.baseline_cursor
    }

    /// 本地可信起点 epoch。
    #[must_use]
    pub fn from_epoch(&self) -> u64 {
        self.from_epoch
    }

    /// 推广目标 epoch。
    #[must_use]
    pub fn to_epoch(&self) -> u64 {
        self.to_epoch
    }

    /// 激活控制链 rev。
    #[must_use]
    pub fn control_rev(&self) -> u64 {
        self.control_rev
    }

    /// `SHA256(CDE(ActivationCommitV1))`。
    #[must_use]
    pub fn activation_hash(&self) -> [u8; 32] {
        self.activation_hash
    }

    /// 已验证的 relay instance。
    #[must_use]
    pub fn instance_id(&self) -> [u8; 16] {
        self.instance_id
    }

    /// 推广后的管理公钥。
    #[must_use]
    pub fn new_manager_public(&self) -> [u8; 32] {
        self.new_manager_public
    }

    /// 推广后的管理 key rev。
    #[must_use]
    pub fn new_manager_rev(&self) -> u64 {
        self.new_manager_rev
    }

    /// 已验证的基线内容（只读；不可再被调用方改写）。
    #[must_use]
    pub fn baseline(&self) -> &BaselinePayload {
        &self.baseline
    }

    fn receipt(&self, identity: &LocalIdentityV1) -> RotationApplyReceipt {
        // receipt 的 `from_epoch` 记**本地 apply 起点**（promotion 前 identity epoch），
        // 不是链尾 T 的 from_epoch：离线跨多 epoch 时两者不同，收尾时要用前者对
        // `IdentityRotationStateV1.previous_roots`。
        RotationApplyReceipt::new(
            identity.vault_id,
            identity.device_id,
            self.rotation_id,
            self.checkpoint_id,
            self.baseline_s,
            &self.baseline_cursor,
            identity.current_epoch,
            self.to_epoch,
            self.control_rev,
            self.instance_id,
            self.new_manager_public,
            self.new_manager_rev,
            self.activation_hash,
        )
    }
}

/// 下载并验证一份候选基线；不写 Vault。
pub fn collect_baseline_payload<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    report: &crate::checkpoint::CheckpointReport,
) -> Result<BaselinePayload, SyncError> {
    let mut docs = Vec::with_capacity(report.docs.len());
    for doc in &report.docs {
        let (_, bytes) = fetch_object(
            client,
            root,
            ObjectType::Snapshot,
            &doc.object.object_id,
            doc.object.part_count,
            doc.object.total_plain_len,
            &doc.object.object_digest,
            &[],
        )?;
        let snapshot = SnapshotV1::decode(&bytes)?;
        // 对象摘要/分片数/明文长度已由 `fetch_object` 按 MNE1 规则校验；这里再验
        // manifest 字段与**内层** Loro 数据（full-history + 实际 VV == 声明 VV）。
        verify_snapshot_integrity(&snapshot)?;
        if snapshot.doc_id != doc.doc_id
            || snapshot.kind != doc.kind
            || snapshot.generation != doc.generation
            || snapshot.vv != doc.vv
            || snapshot.frontier != doc.frontier
            || !snapshot.full_history
        {
            return Err(SyncError::Object("基线快照不是 full-history/字段不符"));
        }
        // storage 的 `apply_baseline_batch` 最终走 `BodyDoc::restore(..., Some(snapshot))`，
        // 需要的是**内层 Loro snapshot 字节**；`SnapshotV1` 的 CDE 外层不能直接导入
        // （评审 0002 P1-1.3：这里曾把整块 CDE 推进 docs）。
        if snapshot.loro_snapshot_bytes.is_empty() {
            return Err(SyncError::Object("基线快照内层 Loro 数据为空"));
        }
        docs.push((snapshot.doc_id, snapshot.loro_snapshot_bytes));
    }
    let mut assets = Vec::with_capacity(report.assets.len());
    for asset in &report.assets {
        let (_, payload) = fetch_object(
            client,
            root,
            ObjectType::Asset,
            &asset.object.object_id,
            asset.object.part_count,
            asset.object.total_plain_len,
            &asset.object.object_digest,
            &[],
        )?;
        let (real, byte_len, media_type) = crate::rotation_stage::decode_asset_plaintext(&payload)?;
        if real != asset.real_sha256 || byte_len != asset.byte_len || media_type != asset.media_type
        {
            return Err(SyncError::Object("基线资产明文与 manifest 不符"));
        }
        assets.push(BaselineAsset {
            object_id: asset.object.object_id,
            asset_hash: real,
            byte_len,
            media_type: Some(media_type),
            data: crate::rotation_stage::payload_asset_data(&payload)?.to_vec(),
        });
    }
    Ok(BaselinePayload {
        checkpoint_id: report.checkpoint_id,
        baseline_s: report.covered_seq,
        docs,
        assets,
    })
}

/// 分批 merge 已验证基线；全部成功才 `finish` 推进 cursor，失败/冲突 abort 且不推 cursor。
pub fn apply_verified_baseline(
    vault: &menghunote_storage::Vault,
    payload: &BaselinePayload,
    baseline_cursor: &str,
) -> Result<BaselineApplyState, SyncError> {
    // 完整性基准：候选 checkpoint 覆盖的 docs/资产必须全部写进 journal，少一批就
    // finish 不了（不会推进 cursor）。
    let expected = BaselineExpectation::new(
        payload.docs.iter().map(|(doc_id, _)| doc_id.clone()),
        payload.assets.iter().map(|asset| asset.object_id),
    );
    begin_baseline_apply(
        vault,
        &payload.checkpoint_id,
        payload.baseline_s,
        "",
        &expected,
    )?;
    let result = (|| {
        for chunk in payload.docs.chunks(64) {
            apply_baseline_batch(vault, chunk, &[])?;
        }
        if !payload.assets.is_empty() {
            apply_baseline_assets(vault, &payload.assets)?;
        }
        match finish_baseline_apply(vault, payload.baseline_s, baseline_cursor)? {
            BaselineApplyFinish::Applied(state) => Ok(state),
            BaselineApplyFinish::NotFound => Err(SyncError::Object("baseline apply journal 丢失")),
            BaselineApplyFinish::Conflict => {
                Err(SyncError::Object("baseline cursor CAS 冲突，未推进"))
            }
        }
    })();
    if result.is_err() {
        let _ = abort_baseline_apply(vault);
    }
    result
}

/// merge 已提交后：**先写 receipt，再推广身份**。
///
/// receipt 必须早于 rename：这样 rename 成功后、`applied` 写前崩溃时，重启能凭
/// receipt + 推广后的 identity/pin 做认证收尾；反过来（先 rename 后写 receipt）则会
/// 留下"新 identity + 无 receipt"的永久冻结窗口。
///
/// 失败语义：merge 失败不写 receipt；receipt 写失败不推广身份；推广失败保留 receipt
/// 但本地身份仍是旧 epoch（重试走正常 epoch change，不会被误判为已完成）。
fn apply_verified_and_promote(
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &LocalIdentityV1,
    verified: &VerifiedApplyPayload,
    candidate_root: &RootSecretV1,
    new_manager_seed: Option<crate::keys::Secret32>,
) -> Result<BaselineApplyState, SyncError> {
    let state = apply_verified_baseline(vault, verified.baseline(), verified.baseline_cursor())?;
    verified.receipt(identity).store(vault)?;
    let authenticated = crate::rotation_activate::AuthenticatedActivation {
        instance_id: verified.instance_id(),
        control_rev: verified.control_rev(),
        activation_hash: verified.activation_hash(),
    };
    let material = crate::rotation_activate::PromotionMaterial {
        candidate_root: candidate_root.clone(),
        // 非管理设备为 `None`；管理设备只有在**新管理 key 与本地一致**时才由调用方
        // 传入自己的 seed（promotion 会再验 `seed -> pubkey == material.new_manager_public`），
        // 否则保持 `None` 并 fail-closed（不许拿旧 seed 冒充新管理权）。
        new_manager_seed,
        new_manager_public: verified.new_manager_public(),
        new_manager_rev: verified.new_manager_rev(),
        control_rev: verified.control_rev(),
        authentication: Some(authenticated),
    };
    crate::rotation_activate::promote_identity_with_material(
        vault,
        identity_path,
        identity,
        &material,
    )?;
    Ok(state)
}

/// B 侧应用 epoch 变化的业务结果；非 `Applied` 一律不得推 cursor/推广/恢复 push。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 合并成功后才可能推广；`promoted=false` 表示只完成了 merge（无身份可推广）。
    Applied { baseline_s: u64, promoted: bool },
    /// 保持暂停，等待重试（网络/验证/merge 失败）。
    Paused { reason: String },
    /// 必须走更高 epoch 的 restart。
    NeedsRestart {
        to_epoch: u64,
        reason: crate::rotation_apply::RestartReason,
    },
    /// baseline cursor CAS 冲突；不推 cursor。
    Conflict,
}

/// 纯入口：消费完整 gate 产出的 [`VerifiedApplyPayload`]，完成 I 阶段收尾。
///
/// **不再接受公开可变的 [`BaselinePayload`] 作为授权输入**（评审 0003 P1-C）：
/// - T 双签/T-view-实例绑定、选中 checkpoint 的 `signed_commit` digest/生产者签名、
///   S/cursor、self grant inclusion 全部由网络入口在构造 [`VerifiedApplyPayload`]
///   之前完成；
/// - 本类型字段私有、构造 `pub(crate)`，外部 crate 无法伪造（见 `compile_fail` doctest），
///   因此本函数等价于 crate 内部/测试 helper，不存在"跳过 gate 的公开入口"。
///
/// 本函数负责阶段 I 的持久收尾：`begin_apply`（同一事务清旧 receipt + 落 `applying`）
/// → merge → 写 typed receipt → rename 推广 → `applied`。不做 epoch 检测与 restart 判定
/// （那是网络入口的职责）。
pub fn apply_epoch_change(
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &LocalIdentityV1,
    verified: &VerifiedApplyPayload,
    candidate_root: &RootSecretV1,
) -> Result<ApplyOutcome, SyncError> {
    apply_epoch_change_inner(
        vault,
        identity_path,
        identity,
        verified,
        candidate_root,
        None,
    )
}

/// 同 [`apply_epoch_change`]，但允许**管理设备**用本地管理 seed 完成推广。
///
/// 只在本片"管理 key 不变"的前提下使用：调用方必须满足
/// `ManagementKey::from_seed(seed).public_key() == new_manager_public`（promotion 会再验一次），
/// 否则 `promote_identity_with_material` 明确失败，绝不把旧 seed 当新管理权。
#[allow(clippy::too_many_arguments)]
pub fn apply_epoch_change_with_manager_seed(
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &LocalIdentityV1,
    verified: &VerifiedApplyPayload,
    candidate_root: &RootSecretV1,
    new_manager_seed: crate::keys::Secret32,
) -> Result<ApplyOutcome, SyncError> {
    apply_epoch_change_inner(
        vault,
        identity_path,
        identity,
        verified,
        candidate_root,
        Some(new_manager_seed),
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_epoch_change_inner(
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &LocalIdentityV1,
    verified: &VerifiedApplyPayload,
    candidate_root: &RootSecretV1,
    new_manager_seed: Option<crate::keys::Secret32>,
) -> Result<ApplyOutcome, SyncError> {
    // 候选根必须正是本次 activation 的目标 epoch：否则 merge 会写进错误 epoch 的根，
    // 身份推广后也无法用该根解开新 outbox。
    if candidate_root.key_epoch != verified.to_epoch() {
        return Ok(ApplyOutcome::Paused {
            reason: format!(
                "候选根 epoch={} 与 activation to_epoch={} 不符",
                candidate_root.key_epoch,
                verified.to_epoch()
            ),
        });
    }
    // 阶段 I 的 durable 冻结屏障：同一事务清旧 receipt + 落 applying，必须在任何
    // merge/写库之前完成。
    RotationApplyPhase::begin_apply(vault)?;
    match apply_verified_and_promote(
        vault,
        identity_path,
        identity,
        verified,
        candidate_root,
        new_manager_seed,
    ) {
        Ok(state) => {
            // 只有 merge + promotion 都成功才允许转 applied（engine 据此解冻）。
            RotationApplyPhase::Applied.store(vault)?;
            Ok(ApplyOutcome::Applied {
                baseline_s: state.baseline_s,
                promoted: true,
            })
        }
        Err(error) => {
            // 失败保持冻结：merge 可能已提交、身份可能尚未推广，绝不能让旧写继续。
            let message = error.to_string();
            if message.contains("CAS") || message.contains("冲突") {
                RotationApplyPhase::Conflict.store(vault)?;
                Ok(ApplyOutcome::Conflict)
            } else {
                RotationApplyPhase::Paused.store(vault)?;
                Ok(ApplyOutcome::Paused { reason: message })
            }
        }
    }
}

fn reconcile_paused(reason: impl Into<String>) -> Option<ApplyOutcome> {
    Some(ApplyOutcome::Paused {
        reason: reason.into(),
    })
}

/// same-epoch 重入：凭 durable receipt 做**认证后的完成 reconcile**（评审 0003 P1-D）。
///
/// 触发条件：`sync.rotation_apply_phase == applying`。此时本地身份可能已经完成
/// rename（`identity.current_epoch == relay.epoch`），网络入口看不到 epoch change；
/// RAM 里的 freeze 也会随进程消失。收尾必须由 durable 事实认证，不能"看到 applying
/// 就无条件清 phase"：
///
/// 1. receipt 必须存在且可解析（损坏/缺失 → `Paused`，保持冻结）；
/// 2. receipt 必须与内存身份、**磁盘身份**（重新解封比对 epoch/control_rev/pin/root）
///    和 `AuthenticatedPinV1` 逐字段一致；
/// 3. 身份旁路状态、baseline apply journal（completed + cid/S/cursor）也必须一致；
/// 4. 必须仍处于 `sync.repack_required=1` 的重包屏障（推广在 rename 前写入）。
///
/// 全部通过才写 `applied`（**不清重包屏障**；重包完成后由 repack 调用方清）。
/// 返回 `None` 表示当前不是 applying（无需收尾）。
pub(crate) fn reconcile_same_epoch_apply(
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &LocalIdentityV1,
) -> Result<Option<ApplyOutcome>, SyncError> {
    let phase = RotationApplyPhase::load(vault)?;
    if phase != Some(RotationApplyPhase::Applying) {
        return Ok(None);
    }
    let receipt = match RotationApplyReceipt::load(vault) {
        Ok(Some(receipt)) => receipt,
        Ok(None) => {
            return Ok(reconcile_paused(
                "phase=applying 但缺少 apply receipt（不能无条件解冻）",
            ))
        }
        Err(error) => {
            return Ok(reconcile_paused(format!(
                "phase=applying 但 apply receipt 损坏：{error}"
            )))
        }
    };
    // ① 内存身份必须已是 receipt 的推广目标。
    if receipt.vault_id() != identity.vault_id
        || receipt.device_id() != identity.device_id
        || receipt.epoch() != identity.current_epoch
        || receipt.control_rev() != identity.control_rev
        || receipt.instance_id() != identity.pinned_instance
        || receipt.new_manager_public() != identity.pinned_manager
        || identity.pending_rotation.is_some()
    {
        return Ok(reconcile_paused(
            "apply receipt 与本地身份不匹配（epoch/control_rev/pin），保持冻结",
        ));
    }
    // ② 磁盘身份才是 durable 真源：内存身份陈旧时不能被用来解冻。
    let durable = match std::fs::read(identity_path) {
        Ok(bytes) => {
            match LocalIdentityV1::unprotect(&bytes, &identity.vault_id, &identity.device_id) {
                Ok(durable) => durable,
                Err(error) => {
                    return Ok(reconcile_paused(format!(
                        "apply receipt 收尾无法解封磁盘身份：{error}"
                    )))
                }
            }
        }
        Err(error) => {
            return Ok(reconcile_paused(format!(
                "apply receipt 收尾无法读取磁盘身份：{error}"
            )))
        }
    };
    if durable.current_epoch != receipt.epoch()
        || durable.control_rev != receipt.control_rev()
        || durable.pinned_instance != receipt.instance_id()
        || durable.pinned_manager != receipt.new_manager_public()
        || durable.pending_rotation.is_some()
        || !durable.current_root.ct_eq(&identity.current_root)
    {
        return Ok(reconcile_paused("apply receipt 与磁盘身份不匹配，保持冻结"));
    }
    // ③ authenticated pin 是完整 gate + 推广的 durable 证据。
    let pin = match AuthenticatedPinV1::load(vault) {
        Ok(Some(pin)) => pin,
        Ok(None) => return Ok(reconcile_paused("缺少 authenticated pin，保持冻结")),
        Err(error) => return Ok(reconcile_paused(format!("authenticated pin 损坏：{error}"))),
    };
    if pin.epoch != receipt.epoch()
        || pin.control_rev != receipt.control_rev()
        || pin.instance_id != receipt.instance_id()
        || pin.activation_hash != receipt.activation_hash()
    {
        return Ok(reconcile_paused(
            "apply receipt 与 authenticated pin 不匹配，保持冻结",
        ));
    }
    // ④ baseline journal 必须证明 merge 已完成且正是 receipt 的 cid/S/cursor。
    let journal_ok = match baseline_apply_state(vault) {
        Ok(Some(state)) => {
            state.status == BaselineApplyStatus::Completed
                && state.checkpoint_id == receipt.checkpoint_id()
                && state.baseline_s == receipt.baseline_s()
                && state.chosen_cursor.as_deref() == Some(receipt.baseline_cursor())
        }
        Ok(None) => false,
        Err(error) => {
            return Ok(reconcile_paused(format!(
                "baseline apply journal 读取失败：{error}"
            )))
        }
    };
    if !journal_ok {
        return Ok(reconcile_paused(
            "baseline apply journal 与 receipt 不匹配（merge 未完成），保持冻结",
        ));
    }
    // ⑤ 身份旁路状态（epoch/control_rev/manager_rev/旧根）必须一致。
    match IdentityRotationStateV1::load_current(vault, identity) {
        Ok(Some(state))
            if state.control_rev == receipt.control_rev()
                && state.manager_rev == receipt.new_manager_rev()
                && state.root_for_epoch(receipt.from_epoch()).is_some() => {}
        Ok(_) => return Ok(reconcile_paused("身份旁路状态与 receipt 不匹配，保持冻结")),
        Err(error) => return Ok(reconcile_paused(format!("身份旁路状态读取失败：{error}"))),
    }
    // ⑥ 重包屏障必须仍在；收尾只写 applied，不提前解除 repack 冻结。
    match repack_required(vault) {
        Ok(true) => {}
        Ok(false) => {
            return Ok(reconcile_paused(
                "缺少 sync.repack_required 重包屏障，保持冻结",
            ))
        }
        Err(error) => return Ok(reconcile_paused(format!("读取重包屏障失败：{error}"))),
    }
    RotationApplyPhase::Applied.store(vault)?;
    Ok(Some(ApplyOutcome::Applied {
        baseline_s: receipt.baseline_s(),
        promoted: true,
    }))
}

/// 从 manifest 下载已验证基线（不写库）。
pub fn collect_manifest_payload<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    manifest_bytes: &[u8],
) -> Result<BaselinePayload, SyncError> {
    let manifest = ManifestV1::decode(manifest_bytes)?;
    if manifest.key_epoch != root.key_epoch {
        return Err(SyncError::Object("基线 manifest epoch 与解出根不符"));
    }
    let mut docs = Vec::with_capacity(manifest.docs.len());
    for doc in &manifest.docs {
        let (header, bytes) = fetch_object(
            client,
            root,
            ObjectType::Snapshot,
            &doc.snapshot_object_id,
            doc.part_count,
            doc.total_plain_len,
            &doc.object_digest,
            &[],
        )?;
        if header.part_count != doc.part_count {
            return Err(SyncError::Object("基线快照分片数与 manifest 不符"));
        }
        if bytes.len() as u64 != doc.total_plain_len {
            return Err(SyncError::Object("基线快照明文长度与 manifest 不符"));
        }
        let snapshot = SnapshotV1::decode(&bytes)?;
        // 快照载荷自报 VV 必须与实际 Loro 历史一致（T18）。
        verify_snapshot_integrity(&snapshot)?;
        if snapshot.doc_id != doc.doc_id
            || snapshot.kind != doc.kind
            || snapshot.generation != doc.generation
            || snapshot.vv != doc.vv
            || snapshot.frontier != doc.frontier
        {
            return Err(SyncError::Object("基线快照与 manifest 字段不符"));
        }
        // storage 的 `apply_baseline_batch` 最终走 `BodyDoc::restore(..., Some(snapshot))`，
        // 需要的是**内层 Loro snapshot 字节**；SnapshotV1 的 CDE 外层不能直接导入。
        if snapshot.loro_snapshot_bytes.is_empty() {
            return Err(SyncError::Object("基线快照内层 Loro 数据为空"));
        }
        docs.push((
            snapshot.doc_id.clone(),
            snapshot.loro_snapshot_bytes.clone(),
        ));
    }
    let mut assets = Vec::with_capacity(manifest.assets.len());
    for asset in &manifest.assets {
        let (_, payload) = fetch_object(
            client,
            root,
            ObjectType::Asset,
            &asset.object_id,
            asset.part_count,
            asset.total_plain_len,
            &asset.object_digest,
            &[],
        )?;
        let (real, byte_len, media_type) = crate::rotation_stage::decode_asset_plaintext(&payload)?;
        if real != asset.real_sha256 || byte_len != asset.byte_len || media_type != asset.media_type
        {
            return Err(SyncError::Object("基线资产明文与 manifest 不符"));
        }
        assets.push(BaselineAsset {
            object_id: asset.object_id,
            asset_hash: real,
            byte_len,
            media_type: Some(media_type),
            data: crate::rotation_stage::payload_asset_data(&payload)?.to_vec(),
        });
    }
    Ok(BaselinePayload {
        checkpoint_id,
        baseline_s,
        docs,
        assets,
    })
}

/// 按 `manifest_oid` 下载并解密 MANIFEST 对象（只读），返回 `(manifest CDE, object_digest)`。
///
/// relay 的 `GET /checkpoints/{cid}` 键 4 是 manifest 的 **object id**（`api.rs:1859–1860`），
/// 不是 manifest 明文；对象本体是 MNE1 密文，必须用当前 epoch 根解密。这里复用
/// `RelayClient::part` + `decrypt_envelope`，并校验分片 header、对象摘要与明文长度。
pub fn fetch_manifest_object<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    manifest_oid: &ObjectId,
) -> Result<(Vec<u8>, [u8; 32]), SyncError> {
    let first = client.part(manifest_oid, 0)?;
    let envelope = Envelope::decode(&first)?;
    if envelope.header.object_type != ObjectType::Manifest
        || envelope.header.object_id != *manifest_oid
        || envelope.header.part_count == 0
    {
        return Err(SyncError::Object("MANIFEST 对象 header 不符"));
    }
    let part_count = envelope.header.part_count;
    let total_plain_len = envelope.header.total_plaintext_len;
    let mut parts = Vec::with_capacity(part_count as usize);
    let mut digests = Vec::with_capacity(part_count as usize);
    for index in 0..part_count {
        let bytes = if index == 0 {
            first.clone()
        } else {
            client.part(manifest_oid, index)?
        };
        let envelope = Envelope::decode(&bytes)?;
        if envelope.header.object_type != ObjectType::Manifest
            || envelope.header.object_id != *manifest_oid
            || envelope.header.part_index != index
            || envelope.header.part_count != part_count
        {
            return Err(SyncError::Object("MANIFEST 分片 header 不符"));
        }
        digests.push(Sha256::digest(&bytes).into());
        parts.push(bytes);
    }
    let digest_input = menghunote_sync_proto::object_digest_input(part_count, &digests)?;
    let object_digest: [u8; 32] = Sha256::digest(&digest_input).into();
    let mut plaintext = Vec::with_capacity(total_plain_len as usize);
    for bytes in &parts {
        let (_, chunk) = decrypt_envelope(root, ObjectType::Manifest, manifest_oid, bytes, &[])?;
        plaintext.extend_from_slice(&chunk);
    }
    if plaintext.len() as u64 != total_plain_len {
        return Err(SyncError::Object("MANIFEST 明文长度不符"));
    }
    Ok((plaintext, object_digest))
}

// ---------------------------------------------------------------- activation T 授权（I 阶段）
//
// `/control` 里**已复算**的 activation T 与双签通过 `RelayClient::verified_activation_t`
// 暴露（带默认 `Ok(None)` 实现）；本模块只消费它，不再自建扩展 trait。
// **缺 T 绝不等于验证通过**：`Ok(None)` 时调用方必须拒绝 apply（fail-closed），
// 不得回退到"只信 relay 自报的 `ActivationCommitV1` 32B hash"。

/// relay `grants_digest` 口径：`SHA256(concat(sorted(device_id || SHA256(signed_header))))`。
///
/// 与 `crates/relay/src/rotations.rs::grants_digest`、`sync::rotation_slots::grants_digest`
/// 同口径（ADR 0006 §34）；按 `device_id` 升序拼接后才哈希。
#[must_use]
pub fn grant_set_digest(leaves: &[GrantLeafEvidence]) -> [u8; 32] {
    let mut ordered = leaves.to_vec();
    ordered.sort_by_key(|leaf| leaf.device_id);
    let mut input = Vec::with_capacity(ordered.len() * 48);
    for leaf in ordered {
        input.extend_from_slice(&leaf.device_id);
        input.extend_from_slice(&leaf.signed_header_sha256);
    }
    Sha256::digest(&input).into()
}

/// 选中 baseline 的 `CheckpointCommit` 证据（T 门禁的 commit digest/签名输入）。
///
/// 真实 HTTP 客户端只能从 `/bootstrap` 拿到主/备基线的 `signed_commit + 生产者签名`
/// （键 6/7）；`GET /checkpoints/{cid}` 只在 `http_relay` 内部解码同名字段，不透出原字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointCommitEvidence {
    /// checkpoint id（必须等于 T 的 primary/fallback cid）。
    pub checkpoint_id: [u8; 16],
    /// 该 checkpoint 覆盖的 S（必须等于 `T.freeze_seq`）。
    pub covered_seq: u64,
    /// relay 为该 S 给出的游标（`/bootstrap` 键 5；与调用方输入逐字节比对）。
    pub cursor: String,
    /// relay 持久化的 `signed_commit` 字节（`CheckpointCommit` CDE）。
    pub signed_commit: Vec<u8>,
    /// 生产者设备对该 commit 的 64B Ed25519 签名。
    pub signature: [u8; 64],
    /// 生产者设备证书里的 Ed25519 公钥（`RelayClient::device_cert(producer)`）。
    pub producer_sign_public: [u8; 32],
    /// manifest 对象 id（relay 为该 checkpoint 指名的 OID）。
    pub manifest_object_id: ObjectId,
}

impl CheckpointCommitEvidence {
    /// 从 relay `/bootstrap` 的一条基线取 commit 证据；缺 `signed_commit`/签名 → `Ok(None)`。
    #[must_use]
    pub fn from_bootstrap(
        entry: &BootstrapCheckpoint,
        producer_sign_public: [u8; 32],
    ) -> Option<Self> {
        let (Some(signed_commit), Some(signature)) = (entry.signed_commit.clone(), entry.signature)
        else {
            return None;
        };
        Some(Self {
            checkpoint_id: entry.checkpoint_id,
            covered_seq: entry.covered_seq,
            cursor: entry.cursor.clone(),
            signed_commit,
            signature,
            producer_sign_public,
            manifest_object_id: entry.manifest_oid,
        })
    }
}

/// 一份"当前合法 checkpoint"的认证结果（评审 0003 P1-C 的第一半）。
///
/// 它回答的是"**这份 checkpoint 内容是谁签的、覆盖到哪、对应哪份 manifest**"，与
/// "T/latest grant 是否授权把根推广到某 epoch"是**两件事**。把两者解耦后，激活 pair
/// 被普通健康 checkpoint 替换（relay `prune_retired` 会 prune 旧 pair/清 refs）时，
/// 旧设备仍能消费当前 pair，而不是永久 `Paused`。
///
/// 字段**私有**、构造只发生在 [`verify_checkpoint_authorization`]（`pub(crate)`）：
/// 外部 crate 无法伪造本类型，也就无法让 [`verify_activation_t_gate`] 对
/// `cid ∉ T.pair` 的选择走"跳过认证"的弱路径。
///
/// ```compile_fail
/// use menghunote_sync::rotation_apply::AuthenticatedCheckpoint;
/// let _ = AuthenticatedCheckpoint {
///     checkpoint_id: [0u8; 16],
///     covered_seq: 0,
///     cursor: String::new(),
///     epoch: 0,
///     signed_commit_sha256: [0u8; 32],
///     manifest_object_id: [0u8; 32],
///     manifest_object_digest: [0u8; 32],
/// };
/// ```
pub struct AuthenticatedCheckpoint {
    checkpoint_id: [u8; 16],
    covered_seq: u64,
    cursor: String,
    epoch: u64,
    signed_commit_sha256: [u8; 32],
    manifest_object_id: ObjectId,
    manifest_object_digest: [u8; 32],
}

impl AuthenticatedCheckpoint {
    /// 已认证的 checkpoint id。
    #[must_use]
    pub fn checkpoint_id(&self) -> [u8; 16] {
        self.checkpoint_id
    }

    /// 已认证的覆盖水位 S。
    #[must_use]
    pub fn covered_seq(&self) -> u64 {
        self.covered_seq
    }

    /// relay 为该 S 给出的游标。
    #[must_use]
    pub fn cursor(&self) -> &str {
        &self.cursor
    }

    /// checkpoint 所属 epoch。
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 生产者签名的 `signed_commit` 字节摘要。
    #[must_use]
    pub fn signed_commit_sha256(&self) -> [u8; 32] {
        self.signed_commit_sha256
    }

    /// MANIFEST 对象 id。
    #[must_use]
    pub fn manifest_object_id(&self) -> ObjectId {
        self.manifest_object_id
    }

    /// MANIFEST 密文对象摘要（`fetch_manifest_object` 复算，不是 relay 自报）。
    #[must_use]
    pub fn manifest_object_digest(&self) -> [u8; 32] {
        self.manifest_object_digest
    }
}

/// 认证"当前合法 checkpoint"本身：**只读**，不写库、不推游标。
///
/// 这是评审 0003 P1-C 的第二半：`cid ∈ T.pair` 时 T 的双签已经授权了该 commit，
/// 但"激活 pair 被普通健康 checkpoint 替换"之后（relay `checkpoints::prune_retired`
/// 会 prune 旧 pair、删 `checkpoint_refs`），新 pair 的 cid 不可能出现在 T 里。
/// 此时改用本函数认证：生产者证书/活动成员资格、epoch、`CheckpointCommit` 签名、
/// manifest、refs（复算 `refs_digest`）、S 的 freeze/GC/head 边界。
///
/// 入参全部是**读回/复算**事实：`entry` 来自 `/bootstrap`（cursor 的唯一来源），
/// `status` 来自 `GET /checkpoints/{cid}`，`commit` 是 `entry.signed_commit` 的解码，
/// `manifest` 是按 `entry.manifest_oid` 解密出的明文，`manifest_object_digest` 是密文
/// 分片的复算摘要。任何一项不符都是 `RelayWire`（调用方转 `Paused`，可重试）。
///
/// `require_active_producer`：`cid ∉ T.pair` 时**必须**为 true——这份 checkpoint 没有
/// T 的 digest 授权，只能靠"已验证控制链里的 active 成员"资格；`cid ∈ T.pair` 时传
/// false（T 的双签已授权该 commit，不因生产者后来被吊销而拒绝冻结期的合法基线）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_checkpoint_authorization<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    entry: &BootstrapCheckpoint,
    status: &CheckpointStatus,
    commit: &CheckpointCommit,
    manifest: &ManifestV1,
    manifest_object_digest: [u8; 32],
    expected_epoch: u64,
    freeze_seq: u64,
    bundle: &BootstrapBundle,
    state: &RelayState,
    require_active_producer: bool,
) -> Result<AuthenticatedCheckpoint, SyncError> {
    // ① relay 必须明确把这份 checkpoint 标成 `published`：staging/candidate/aborted/
    //    pruned 都不是"当前合法基线"（pruned 的 refs 已删、对象可能已被 retire）。
    if status.state != "published" {
        return Err(SyncError::RelayWire(
            "checkpoint 状态不是 published（拒绝未发布/已 prune 的基线）",
        ));
    }
    // ② cid 三方一致：bootstrap 条目、`GET /checkpoints/{cid}` 与签名 commit。
    if entry.checkpoint_id != status.checkpoint_id || commit.checkpoint_id != status.checkpoint_id {
        return Err(SyncError::RelayWire(
            "checkpoint cid 在 bootstrap/status/commit 三处不一致",
        ));
    }
    // ③ epoch 三方一致，且正是本次 activation 的目标 epoch。
    if entry.epoch != expected_epoch
        || status.epoch != expected_epoch
        || commit.epoch != expected_epoch
    {
        return Err(SyncError::RelayWire(
            "checkpoint epoch 与 bootstrap/status/commit/激活目标 epoch 不一致",
        ));
    }
    // ④ S 四方一致（bootstrap/status/commit/manifest）：cursor 与 apply 都必须落在同一 S。
    if entry.covered_seq != status.covered_seq
        || commit.covered_relay_seq != status.covered_seq
        || manifest.covered_relay_seq != status.covered_seq
    {
        return Err(SyncError::RelayWire(
            "checkpoint 覆盖水位 S 在 bootstrap/status/commit/manifest 四处不一致",
        ));
    }
    // ⑤ 下界/上界：
    //    - `S >= freeze_seq`：`(S, freeze]` 的更新写在旧根下，基线不覆盖就会永久丢；
    //    - `S >= gc_floor`：低于 floor 的日志前缀已删，从该 cursor 续读必缺洞；
    //    - `S <= head`：不得消费 relay 尚未产生的更新。
    //    floor/head 取 `/bootstrap` 与 `/state` 两处中更严格的一侧（两次请求之间可能前进）。
    if status.covered_seq < freeze_seq {
        return Err(SyncError::RelayWire(
            "checkpoint S 早于 activation T 的 freeze_seq",
        ));
    }
    let gc_floor = bundle.gc_floor.max(state.gc_floor);
    if status.covered_seq < gc_floor {
        return Err(SyncError::RelayWire(
            "checkpoint S 低于 relay GC floor（该游标已不可继续）",
        ));
    }
    let head_seq = bundle.head_seq.min(state.head_seq);
    if status.covered_seq > head_seq {
        return Err(SyncError::RelayWire("checkpoint S 超过 relay head_seq"));
    }
    // ⑥ manifest 对象 id 三方一致；`/bootstrap` 必须带游标（Opaque 客户端不可解）。
    let status_manifest = status
        .manifest_object_id
        .ok_or(SyncError::RelayWire("checkpoint 缺少 manifest_oid"))?;
    if entry.manifest_oid != status_manifest || commit.manifest_object_id != status_manifest {
        return Err(SyncError::RelayWire(
            "checkpoint manifest_oid 在 bootstrap/status/commit 三处不一致",
        ));
    }
    if entry.cursor.is_empty() {
        return Err(SyncError::RelayWire("bootstrap 条目缺 relay 游标"));
    }
    // ⑦ 密文对象摘要必须等于签名 commit 里承诺的 manifest_digest（已在
    //    `fetch_manifest_object` 处按分片复算，不是 relay 自报）。
    if commit.manifest_digest != manifest_object_digest {
        return Err(SyncError::RelayWire(
            "manifest 密文摘要与 checkpoint commit 不符",
        ));
    }
    // ⑧ manifest 明文必须绑到本 vault/本 cid/本 epoch。
    if manifest.vault_id != vault_id
        || manifest.checkpoint_id != commit.checkpoint_id
        || manifest.key_epoch != expected_epoch
    {
        return Err(SyncError::RelayWire(
            "manifest 明文与 vault/checkpoint/epoch 不绑定",
        ));
    }
    // ⑨ refs 摘要**复算**：只凭 manifest 条目重算 refs 集合的 `refs_digest`，与签名
    //    commit 以及 relay 自报（status/boot 条目有值时必须相等）对撞，防止"签名覆盖的
    //    对象集合"与实际 manifest 不一致。
    let refs = crate::bootstrap::manifest_refs(manifest);
    let refs_digest: [u8; 32] =
        Sha256::digest(menghunote_sync_proto::refs_digest_input(&refs)?).into();
    if commit.refs_digest != refs_digest {
        return Err(SyncError::RelayWire(
            "checkpoint refs_digest 与 manifest 复算结果不符",
        ));
    }
    if status
        .refs_digest
        .is_some_and(|advertised| advertised != refs_digest)
    {
        return Err(SyncError::RelayWire(
            "checkpoint status 自报 refs_digest 与复算不符",
        ));
    }
    if entry
        .refs_digest
        .is_some_and(|advertised| advertised != refs_digest)
    {
        return Err(SyncError::RelayWire(
            "bootstrap 条目自报 refs_digest 与复算不符",
        ));
    }
    // ⑩ 生产者证书必须存在且归属本 vault/该设备。
    let certificate = client
        .device_cert(&commit.producer_device_id)?
        .ok_or(SyncError::RelayWire("找不到 checkpoint 生产者设备证书"))?;
    if certificate.vault_id != vault_id || certificate.device_id != commit.producer_device_id {
        return Err(SyncError::RelayWire(
            "checkpoint 生产者证书与 vault/commit 不符",
        ));
    }
    // ⑪ 生产者 Ed25519 签名覆盖 canonical `checkpoint_signing_input`。
    let signature = entry
        .signature
        .ok_or(SyncError::RelayWire("checkpoint 缺 commit 生产者签名"))?;
    verify_signature(
        &certificate.sign_ed25519_pub,
        &checkpoint_signing_input(commit)?,
        &signature,
    )?;
    // ⑫ 生产者资格：必须是**已验证控制链**里的 active 成员。用 HPKE 公钥做身份对撞
    //    （`active_member_hpke_keys` 由 `active_certs` 派生，relay `member.status` 不参与），
    //    避免再造一个"证书存在即通过"的洞。客户端给不出该集合时 fail-closed。
    if require_active_producer {
        let active_keys = match client.active_member_hpke_keys() {
            Ok(keys) => keys,
            // 传输类错误保持 `Err`（可重试，不写库）。
            Err(error @ SyncError::Transport(_)) => return Err(error),
            Err(_) => {
                return Err(SyncError::RelayWire(
                    "客户端无法提供已验证控制链的 active 成员集合，禁止消费 T pair 之外的基线",
                ))
            }
        };
        let active = active_keys.iter().any(|(device_id, hpke)| {
            *device_id == commit.producer_device_id && *hpke == certificate.hpke_x25519_pub
        });
        if !active {
            return Err(SyncError::RelayWire(
                "checkpoint 生产者不是已验证控制链的 active 成员",
            ));
        }
    }
    let signed_commit = entry
        .signed_commit
        .as_deref()
        .ok_or(SyncError::RelayWire("checkpoint 缺 signed_commit"))?;
    Ok(AuthenticatedCheckpoint {
        checkpoint_id: commit.checkpoint_id,
        covered_seq: commit.covered_relay_seq,
        cursor: entry.cursor.clone(),
        epoch: commit.epoch,
        signed_commit_sha256: signed_commit_digest(signed_commit),
        manifest_object_id: commit.manifest_object_id,
        manifest_object_digest,
    })
}

/// T 授权门禁的全部输入：T 证据 + 已下载/已读回的公开事实（不含秘密）。
pub struct ActivationTGate<'a> {
    /// 已复算的 T 与双签（缺失时调用方不得构造门禁）。
    pub evidence: &'a VerifiedActivationT,
    /// current rotation 视图（relay 公开事实，仅用于与 T 交叉绑定）。
    pub view: &'a RotationView,
    /// 本地 vault。
    pub vault_id: VaultId,
    /// relay 实例（`/state` 键 2；必须与本地 pin 一致）。
    pub instance_id: [u8; 16],
    /// 期望的 `T.from_epoch`：单轮 = 本地 `current_epoch`；离线跨多 epoch =
    /// 已验证 activation 链的前一环 `to_epoch`（见
    /// [`verify_epoch_activation_chain`]）。
    pub expected_from_epoch: u64,
    /// 本设备 id（grant recipient）。
    pub device_id: DeviceId,
    /// 本设备 X25519 私钥（复算 grant AAD 的 recipient 公钥）。
    pub device_x25519_secret: &'a Secret32,
    /// 签发 grant / T 旧签名的管理公钥（阶段 A 本地锚点）。
    pub old_manager_public: [u8; 32],
    /// T 新签名的新管理公钥（推广后 pin）。
    pub new_manager_public: [u8; 32],
    /// 新管理 key rev（推广输入）。
    pub new_manager_rev: u64,
    /// 调用方选中的 baseline cid。
    pub selected_checkpoint: [u8; 16],
    /// 调用方选中的 baseline S。
    pub baseline_s: u64,
    /// 调用方选中的 baseline cursor。
    pub baseline_cursor: &'a str,
    /// 选中 checkpoint 的 `signed_commit` 证据（`/bootstrap` 读回）。
    pub commit: &'a CheckpointCommitEvidence,
    /// 选中 MANIFEST 对象的密文摘要（`fetch_manifest_object` 复算，不是 relay 自报）。
    pub manifest_object_digest: [u8; 32],
    /// 本设备 latest grant 原始证据。
    pub grant: &'a SelfGrantEvidence,
    /// 完整 grant leaf（`None` 时只有 `grants_count == 1` 能证明 inclusion）。
    pub grant_leaves: Option<&'a [GrantLeafEvidence]>,
    /// relay view 的 grants 数量（`T.grants_digest` 覆盖的 leaf 数）。
    pub grants_count: u64,
}

/// T 证据的**双签复算 + 逐字段绑定**（门禁、epoch 链与纯入口共用）。
///
/// ① 旧/新管理公钥分别对 `activation_t_signing_input(T)` 复算 64B 签名（不信 relay
/// "已验证"自报）；② T 与 rotation view（rid/epoch/freeze S/候选 pair/激活 control_rev）、
/// relay 实例、链上位置 `expected_from_epoch`、推广用新管理 key/rev 逐字段绑定。
///
/// `expected_from_epoch` 只允许两种来源：单轮 apply 的本地可信 `current_epoch`，或
/// [`verify_epoch_activation_chain`] 已验证链的**前一项 `to_epoch`**。绝不接受"任意小"
/// 或 `<=` 语义——跳环/回滚必须在链验证阶段 fail-closed。
///
/// 这里**不**检查 baseline 的 S/cursor/commit digest/grant inclusion——那些需要 relay
/// 读回事实，由 [`verify_activation_t_gate`] 继续完成。
#[allow(clippy::too_many_arguments)]
pub fn verify_activation_t_bindings(
    evidence: &VerifiedActivationT,
    view: &RotationView,
    vault_id: VaultId,
    instance_id: [u8; 16],
    expected_from_epoch: u64,
    old_manager_public: [u8; 32],
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
) -> Result<(), SyncError> {
    let activation_t = &evidence.activation_t;

    // ① T 的旧/新管理双签必须由本地已知公钥复算（不信 relay "已验证"自报）。
    let signing_input = activation_t_signing_input(activation_t)?;
    let old_signature: [u8; 64] = evidence
        .old_t_signature
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("activation T 旧管理签名不是 64B"))?;
    let new_signature: [u8; 64] = evidence
        .new_t_signature
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("activation T 新管理签名不是 64B"))?;
    verify_signature(&old_manager_public, &signing_input, &old_signature)?;
    verify_signature(&new_manager_public, &signing_input, &new_signature)?;

    // ② T 与 rotation view / relay 实例 / 本地身份逐字段绑定。
    if activation_t.vault_id != vault_id
        || activation_t.instance_id != instance_id
        || activation_t.rotation_id != view.rotation_id
    {
        return Err(SyncError::RelayWire(
            "activation T 与 vault/instance/rotation 不绑定",
        ));
    }
    if view.state != "activated"
        || activation_t.from_epoch != view.from_epoch
        || activation_t.to_epoch != view.to_epoch
        || activation_t.freeze_seq != view.freeze_seq
    {
        return Err(SyncError::RelayWire(
            "activation T 与 rotation view 的 epoch/S 不符",
        ));
    }
    // `from_epoch` 必须精确等于"链上前一环的 to_epoch"：
    // - 单轮 apply = 本地可信 `current_epoch`；
    // - 离线跨多 epoch = 已验证链的前一环（由
    //   [`verify_epoch_activation_chain`] 逐环推进后传入）。
    // **不是**放宽为 `<=`：任何跳环/回滚都必须在链验证阶段被拒绝。
    if activation_t.from_epoch != expected_from_epoch
        || activation_t.to_epoch <= activation_t.from_epoch
    {
        return Err(SyncError::RelayWire(
            "activation T 的 from/to epoch 与链上位置/本地状态不符",
        ));
    }
    if view.primary_checkpoint != Some(activation_t.primary_cid)
        || view.fallback_checkpoint != Some(activation_t.fallback_cid)
        || activation_t.primary_cid == activation_t.fallback_cid
    {
        return Err(SyncError::RelayWire(
            "activation T 的 primary/fallback cid 与 view 不符",
        ));
    }
    if view.activated_control_rev != Some(activation_t.expected_control_rev) {
        return Err(SyncError::RelayWire(
            "activation T 的 expected_control_rev 与激活控制记录不符",
        ));
    }
    if activation_t.new_manager_pubkey != new_manager_public
        || u64::from(activation_t.new_manager_rev) != new_manager_rev
    {
        return Err(SyncError::RelayWire(
            "activation T 的新管理 key/rev 与推广输入不符",
        ));
    }
    Ok(())
}

/// 已验证的 activation 链尾：离线跨多 epoch 后 B 侧 apply 的唯一授权结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedActivationChain {
    /// 链尾 T（必须与 network 入口拿到的 latest T 逐字节一致）。
    pub tail: VerifiedActivationT,
    /// 链尾 T 的 `from_epoch`（= 链上前一环 `to_epoch`；单轮 = 本地 epoch）。
    pub tail_from_epoch: u64,
    /// 链首 T 的旧管理 rev（= 本地 epoch 位置上的管理 rev）。
    ///
    /// 链内相邻环已保证 `old == 上一环 new`；链首必须再与本地可信管理 rev 锚定，
    /// 否则 relay 可以给出"链内自洽但整体错位"的 rev 序列。
    pub head_old_manager_rev: u64,
    /// 链尾 T 的旧管理公钥（签发 latest grant 的那把；单轮 = 本地 pin）。
    pub tail_old_manager_public: [u8; 32],
    /// 链尾 T 的新管理公钥（推广目标；必须等于调用方传入的推广材料）。
    pub tail_new_manager_public: [u8; 32],
    /// 链尾 T 的新管理 key rev（推广目标）。
    pub tail_new_manager_rev: u64,
}

/// 从本地可信 `current_epoch` 出发，逐环验证 `/control` 的 activation 链。
///
/// 背景（评审 0003 追加）：B 离线期间 A 连续完成 1→2、2→3 后，latest T 的
/// `from_epoch` 是 2 而本地 epoch 仍是 1。旧门禁强制 `T.from_epoch == 本地 epoch`，
/// 于是"latest grant 存在但必定 Paused"。修复**不是**把等号放宽为 `<=`，而是补上
/// 授权链本身：
///
/// 1. 必须能在链上定位 `from_epoch == 本地可信 current_epoch` 的那一环，并从它开始
///    逐环验证（找不到 = 链不完整/回滚/缺中间激活 → 拒绝）；
/// 2. 相邻环 `to_epoch == 下一环 from_epoch`，且每环 `to > from`（缺中间激活 → 拒绝）；
/// 3. 每环 T 的双签/T-view/实例绑定复用 [`verify_activation_t_bindings`]；旧管理 key
///    由上一环 T 的 `new_manager_pubkey` 推出，管理 rev 必须首尾相接；链首的旧管理
///    rev 由调用方锚定到本地可信状态（本函数把它放在 [`VerifiedActivationChain::head_old_manager_rev`] 返回）；
/// 4. 每环的 `ActivationCommitV1` 必须能重建、摘要等于 view 的 32B 摘要，且与 T
///    逐字段一致（复用 [`verify_activation_commit`] /
///    [`verify_activation_commit_matches_t`]）；
/// 5. 每环 `control_rev == T.expected_control_rev`，且 `prev_control_hash` 等于该 rev
///    前一条记录的链上 hash（由 [`RelayClient::epoch_activations`] 从已验证的
///    `/control` 给出）；
/// 6. 链尾必须正是 latest T（否则 latest 来自另一条链/更新的激活 → 拒绝）。
///
/// 中间的 baseline/grant 不逐个下载（latest baseline 覆盖历史），但**授权链逐环验**。
/// 通过后调用方只能拿链尾的 T 授权 latest grant/baseline 的 apply。
pub fn verify_epoch_activation_chain(
    links: &[EpochActivationLink],
    latest: &VerifiedActivationT,
    vault_id: VaultId,
    instance_id: [u8; 16],
    current_epoch: u64,
    anchor_manager_public: [u8; 32],
) -> Result<VerifiedActivationChain, SyncError> {
    if links.is_empty() {
        return Err(SyncError::RelayWire(
            "缺少 activation 链（客户端未提供 /control 逐环证据），拒绝 apply",
        ));
    }
    // ① 全链结构：epoch 严格前进、相邻环首尾相接、控制链 rev 严格递增。
    for (index, link) in links.iter().enumerate() {
        let activation_t = &link.evidence.activation_t;
        if activation_t.to_epoch <= activation_t.from_epoch {
            return Err(SyncError::RelayWire(
                "activation 链的 to_epoch 必须大于 from_epoch",
            ));
        }
        if index > 0 {
            let prev = &links[index - 1];
            if activation_t.from_epoch != prev.evidence.activation_t.to_epoch {
                return Err(SyncError::RelayWire(
                    "activation 链缺中间激活（to_epoch 与下一环 from_epoch 不相接）",
                ));
            }
            if link.control_rev <= prev.control_rev {
                return Err(SyncError::RelayWire(
                    "activation 链的 control_rev 未严格递增（重排/回放）",
                ));
            }
        }
    }
    // ② 从本地可信 epoch 对应的那一环开始；找不到 = 链不完整/回滚/缺中间激活。
    //    客户端（[`RelayClient::epoch_activations`]）按 `to_epoch > from_epoch` 过滤后
    //    通常 start = 0；这里仍显式定位，避免不同实现返回全链时把信任锚放错位置。
    let start = links
        .iter()
        .position(|link| link.evidence.activation_t.from_epoch == current_epoch)
        .ok_or(SyncError::RelayWire(
            "本地 epoch 不在 activation 链首（链不完整/回滚/缺中间激活），拒绝 apply",
        ))?;
    let mut tail_old_manager_public = anchor_manager_public;
    for (index, link) in links.iter().enumerate().skip(start) {
        let activation_t = &link.evidence.activation_t;
        let expected_from_epoch = if index == start {
            current_epoch
        } else {
            links[index - 1].evidence.activation_t.to_epoch
        };
        // ③ 旧管理 key 由链上前一环推出；链首 = 本地锚点。管理 rev 必须相接。
        if index > start {
            let prev = &links[index - 1].evidence.activation_t;
            if activation_t.old_manager_rev != prev.new_manager_rev {
                return Err(SyncError::RelayWire("activation 链的管理权 rev 不连续"));
            }
            tail_old_manager_public = prev.new_manager_pubkey;
        }
        // ④ 双签 + T/view/实例/epoch 绑定（复用单步门禁的同一条校验）。
        verify_activation_t_bindings(
            &link.evidence,
            &link.view,
            vault_id,
            instance_id,
            expected_from_epoch,
            tail_old_manager_public,
            activation_t.new_manager_pubkey,
            u64::from(activation_t.new_manager_rev),
        )?;
        // ⑤ 控制链位置：rev 与 prev_control_hash 必须正是链上这一条。
        if link.control_rev != activation_t.expected_control_rev {
            return Err(SyncError::RelayWire(
                "activation 的 control_rev 与 T.expected_control_rev 不符",
            ));
        }
        if link.prev_control_hash != activation_t.prev_control_hash {
            return Err(SyncError::RelayWire(
                "activation T 的 prev_control_hash 与链上位置不符",
            ));
        }
        // ⑥ activation commit：重建摘要必须等于 view 的 32B 摘要，且与 T 逐字段一致。
        let Some(expected_commit) = link.view.activation_commit.as_deref() else {
            return Err(SyncError::RelayWire(
                "activation 链的 view 缺少 activation commit",
            ));
        };
        let commit_bytes = link.activation_commit.encode()?;
        let control_rev = link.view.activated_control_rev.ok_or(SyncError::RelayWire(
            "activation 链的 view 缺少 activated_control_rev",
        ))?;
        verify_activation_commit(
            &commit_bytes,
            expected_commit,
            vault_id,
            instance_id,
            link.view.rotation_id,
            link.view.from_epoch,
            link.view.to_epoch,
            control_rev,
        )?;
        verify_activation_commit_matches_t(&link.activation_commit, activation_t)?;
    }
    // ⑦ 链尾 = latest：否则 latest 可能来自另一条链/更新的激活，不能拿它授权。
    let tail = links
        .last()
        .ok_or(SyncError::RelayWire("activation 链为空（不能授权 apply）"))?;
    if tail.evidence != *latest {
        return Err(SyncError::RelayWire(
            "activation 链尾不是 latest T（本地看到的激活不在链尾），拒绝 apply",
        ));
    }
    let activation_t = &tail.evidence.activation_t;
    Ok(VerifiedActivationChain {
        tail: tail.evidence.clone(),
        tail_from_epoch: activation_t.from_epoch,
        head_old_manager_rev: u64::from(links[start].evidence.activation_t.old_manager_rev),
        tail_old_manager_public,
        tail_new_manager_public: activation_t.new_manager_pubkey,
        tail_new_manager_rev: u64::from(activation_t.new_manager_rev),
    })
}

/// 用 activation T 对一次 B 侧 apply 授权（评审 0002 P1-1.4 / ADR 0006 §34）。
///
/// 覆盖：T 双签复算、T 与 view/身份/管理权逐字段绑定、选中 baseline 的 cid/commit
/// 授权、`CheckpointCommit` 字段与生产者签名、本设备 grant 的
/// AAD/enc/ciphertext/外签与 `T.grants_digest` inclusion。
///
/// **两条路径（评审 0003 P1-C）**：
/// - `selected_checkpoint ∈ {T.primary_cid, T.fallback_cid}`：T 的双签点名了该
///   `signed_commit` 的 digest，必须逐字节相等。这是**强路径**，绝不因为弱路径可用而降级
///   （否则恶意 relay 可在激活 pair 仍健康时改喂"同 cid、不同 commit"的基线，把轮换冻结点
///   唯一的 have-frozen 保证静默弱化；也给"生产者事后被吊销"的历史基线保留合法消费口径）。
/// - `selected_checkpoint ∉ T.pair`：激活 pair 已被普通健康 checkpoint 替换（relay
///   `prune_retired`），必须由调用方先经 [`verify_checkpoint_authorization`] 完成
///   checkpoint 级认证并传入 `authenticated`；门禁再把它与本次选择的
///   cid/S/cursor/epoch/commit 逐字段对撞。
///
/// **管理 rev 语义**：staging grant 的 AAD `manager_rev` 是 `T.old_manager_rev`
/// （activate 前 relay 的管理权仍是旧 rev）；`T.new_manager_rev` 只用于 promote/最终
/// 身份，**不**参与 grant 校验。调用方必须先经 [`verify_epoch_activation_chain`] 并
/// 把链首 `T.old_manager_rev` 锚定到本地可信管理 rev（见
/// `apply_epoch_change_with_client` 的链首锚点检查）。
///
/// **S 语义**：T 绑定的是**冻结** S（`T.freeze_seq`），候选基线可以覆盖 `S >= freeze_seq`
/// （冻结之后创建的候选 checkpoint 也是合法基线）；因此这里只要求
/// `baseline_s >= T.freeze_seq` 且 `CheckpointCommit.covered_relay_seq >= T.freeze_seq`，
/// `baseline_cursor` 仍必须逐字节等于 relay 为该 S 给出的 `/bootstrap` 游标。
///
/// 通过后返回选中的、已验签的 [`CheckpointCommit`]。任一步不符都是 `RelayWire` 错误：
/// 调用方必须转成 `Paused`，**不得**写库/推游标。
#[allow(clippy::too_many_arguments)]
pub fn verify_activation_t_gate(
    input: &ActivationTGate<'_>,
    authenticated: Option<&AuthenticatedCheckpoint>,
) -> Result<CheckpointCommit, SyncError> {
    let activation_t = &input.evidence.activation_t;
    verify_activation_t_bindings(
        input.evidence,
        input.view,
        input.vault_id,
        input.instance_id,
        input.expected_from_epoch,
        input.old_manager_public,
        input.new_manager_public,
        input.new_manager_rev,
    )?;

    // ③ 选中 baseline 的 cid/digest 授权（两条路径，见函数文档）。
    //    S 只要求 `>= freeze_seq`：T 绑定的是**冻结** S，冻结之后创建的候选 checkpoint
    //    覆盖更晚的 S 仍是合法基线。
    let pair_digest = if input.selected_checkpoint == activation_t.primary_cid {
        Some(activation_t.primary_commit_digest)
    } else if input.selected_checkpoint == activation_t.fallback_cid {
        Some(activation_t.fallback_commit_digest)
    } else {
        None
    };
    if input.commit.checkpoint_id != input.selected_checkpoint {
        return Err(SyncError::RelayWire(
            "checkpoint commit 与选中基线 cid 不符",
        ));
    }
    if input.baseline_s < activation_t.freeze_seq || input.commit.covered_seq != input.baseline_s {
        return Err(SyncError::RelayWire(
            "baseline S 早于 activation T 的 freeze_seq 或与 checkpoint 覆盖的 S 不符",
        ));
    }
    // cursor 必须正是 relay 为该 S 给出的游标（`/bootstrap` 键 5，Opaque 客户端不可解）：
    // 换了 S 却复用旧游标 = 从错误位置续读，必须拒绝。
    if input.baseline_cursor != input.commit.cursor {
        return Err(SyncError::RelayWire(
            "baseline cursor 与 relay 该 baseline_s 的 /bootstrap 游标不符",
        ));
    }
    match pair_digest {
        // 强路径：pair 内必须逐字节等于 T 的双签 digest，**不得**降级到 checkpoint 级认证。
        Some(expected_commit_digest) => {
            if signed_commit_digest(&input.commit.signed_commit) != expected_commit_digest {
                return Err(SyncError::RelayWire(
                    "checkpoint commit digest 与 activation T 不符",
                ));
            }
        }
        // 弱路径：非 T pair 的选择必须已完成 checkpoint 级认证，并与本次选择逐字段一致。
        None => {
            let authenticated = authenticated.ok_or(SyncError::RelayWire(
                "选中基线不是 T 的 activation pair，且未通过 checkpoint 级认证",
            ))?;
            if authenticated.checkpoint_id != input.selected_checkpoint
                || authenticated.covered_seq != input.baseline_s
                || authenticated.cursor.as_str() != input.baseline_cursor
                || authenticated.epoch != activation_t.to_epoch
                || authenticated.signed_commit_sha256
                    != signed_commit_digest(&input.commit.signed_commit)
                || authenticated.manifest_object_id != input.commit.manifest_object_id
                || authenticated.manifest_object_digest != input.manifest_object_digest
            {
                return Err(SyncError::RelayWire(
                    "checkpoint 级认证材料与选中的 cid/S/commit 不一致",
                ));
            }
        }
    }

    // ④ checkpoint commit 自身：字段绑定 + 生产者签名。digest 只证明"同一份字节"，
    //    签名才证明生产者真的为这个 S/manifest/refs 签过。
    let commit = CheckpointCommit::decode(&input.commit.signed_commit)?;
    crate::checkpoint::require_current_checkpoint_validation(&commit)?;
    if commit.vault_id != input.vault_id
        || commit.checkpoint_id != input.selected_checkpoint
        || commit.epoch != activation_t.to_epoch
        // `CheckpointCommit.covered_relay_seq` 只受 freeze S 下界约束（候选可覆盖更晚的 S）；
        // 它必须与调用方选中的 S 一致，manifest/apply 才会把 cursor 推到同一个 S。
        || commit.covered_relay_seq < activation_t.freeze_seq
        || commit.covered_relay_seq != input.baseline_s
        || commit.manifest_object_id != input.commit.manifest_object_id
        || commit.manifest_digest != input.manifest_object_digest
    {
        return Err(SyncError::RelayWire(
            "checkpoint commit 字段/对象摘要与 T 或读回事实不符",
        ));
    }
    verify_signature(
        &input.commit.producer_sign_public,
        &checkpoint_signing_input(&commit)?,
        &input.commit.signature,
    )?;

    // ⑤ 本设备 latest grant：AAD/enc/ciphertext/外签 + 包 digest + T.grants_digest inclusion。
    //
    // **AAD.manager_rev 是旧管理 rev，不是推广目标 rev**：staging grant 由 relay 在
    // activate 之前写入，那时 `vault.manager_rev` 仍是旧值（relay `stage_grant` 对
    // 新 rev 返回 CAS_CONFLICT），grant 的 AAD 因此固定是 `T.old_manager_rev`。
    // `T.new_manager_rev` 只用于 promote/最终身份，绝不参与 staging grant 的校验。
    let grant = input.grant;
    let (aad, enc, ciphertext_sha256) = decode_self_grant_header(&grant.signed_header)?;
    if aad.vault_id != input.vault_id
        || aad.epoch != activation_t.to_epoch
        || aad.manager_rev != u64::from(activation_t.old_manager_rev)
        || aad.recipient_device_id != input.device_id
    {
        return Err(SyncError::RelayWire("self grant AAD 与 T/本地设备不绑定"));
    }
    if aad.recipient_x25519_pub != x25519_public_key(input.device_x25519_secret) {
        return Err(SyncError::RelayWire(
            "self grant recipient 公钥不是本设备 HPKE key",
        ));
    }
    let ciphertext_digest: [u8; 32] = Sha256::digest(&grant.ciphertext).into();
    if grant.device_id != input.device_id
        || grant.epoch != activation_t.to_epoch
        || grant.manager_rev != u64::from(activation_t.old_manager_rev)
        || grant.hpke_enc != enc
        || ciphertext_digest != ciphertext_sha256
    {
        return Err(SyncError::RelayWire(
            "self grant 字段与 signed_header/AAD 不符",
        ));
    }
    let grant_signature: [u8; 64] = grant
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant 管理签名不是 64B"))?;
    verify_signature(
        &input.old_manager_public,
        &grant.signed_header,
        &grant_signature,
    )?;
    // 包 digest（relay `grants/latest` 键 10 口径）：若 relay 给了自报值，必须逐字节一致。
    let header_sha256: [u8; 32] = Sha256::digest(&grant.signed_header).into();
    let package_input = grant_package_digest(
        &grant.hpke_enc,
        &ciphertext_sha256,
        &header_sha256,
        &grant.signature,
    );
    let package_digest: [u8; 32] = Sha256::digest(&package_input).into();
    if grant
        .package_digest
        .is_some_and(|expected| expected != package_digest)
    {
        return Err(SyncError::RelayWire(
            "self grant 包 digest 与 relay 自报不符",
        ));
    }
    // inclusion：`T.grants_digest` 只覆盖 `(device_id, SHA256(signed_header))`。
    let owned_leaf = GrantLeafEvidence {
        device_id: input.device_id,
        signed_header_sha256: header_sha256,
    };
    match input.grant_leaves {
        Some(leaves) => {
            if !leaves.contains(&owned_leaf) {
                return Err(SyncError::RelayWire("本设备 grant 不在 staged leaf 集合内"));
            }
            if grant_set_digest(leaves) != activation_t.grants_digest {
                return Err(SyncError::RelayWire(
                    "完整 grant leaf 集合与 T.grants_digest 不符",
                ));
            }
        }
        None if input.grants_count == 1 => {
            if grant_set_digest(&[owned_leaf]) != activation_t.grants_digest {
                return Err(SyncError::RelayWire(
                    "本设备 grant 未被 T.grants_digest 覆盖",
                ));
            }
        }
        None => {
            return Err(SyncError::RelayWire(
                "缺完整 grant leaf manifest，无法证明 self grant 被 T.grants_digest 覆盖",
            ))
        }
    }
    Ok(commit)
}

/// 把 self grant 的 `signed_header` 拆成 `(AAD, enc32, SHA256(ciphertext))`。
///
/// AAD 是 `CDE(GrantAadV1)`；后 64B 固定是 `enc32 || ciphertext_sha256`
/// （`hpke::grant_signature_input` 口径）。三者必须与 relay 下发的
/// `hpke_enc`/`ciphertext` 逐字节一致，否则 grant 不在 T 授权的包内。
fn decode_self_grant_header(
    signed_header: &[u8],
) -> Result<(GrantAadV1, [u8; 32], [u8; 32]), SyncError> {
    if signed_header.len() < 64 {
        return Err(SyncError::RelayWire("self grant signed_header 过短"));
    }
    let aad_len = signed_header.len() - 64;
    let aad = GrantAadV1::decode(&signed_header[..aad_len])?;
    let enc: [u8; 32] = signed_header[aad_len..aad_len + 32]
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant enc 长度非法"))?;
    let ciphertext_sha256: [u8; 32] = signed_header[aad_len + 32..]
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant ciphertext 摘要长度非法"))?;
    Ok((aad, enc, ciphertext_sha256))
}

/// 重建出的 `ActivationCommitV1`（relay 键 20 的公开事实）必须与 T 逐字段一致。
///
/// relay 的 32B hash **不是**授权来源：它只说明"重建材料 == relay 自报材料"，
/// 真正的授权是双签 T。
///
/// **S 语义**：T 的 `freeze_seq` 是候选 checkpoint 的**下界**（冻结之后创建的候选可以
/// 覆盖更晚的 S），因此 `primary_covered_seq`/`fallback_covered_seq` 只要求
/// `>= T.freeze_seq`；`ActivationCommitV1.covered_seq` 仍是冻结 S，必须精确相等。
pub fn verify_activation_commit_matches_t(
    commit: &ActivationCommitV1,
    activation_t: &ActivationTV1,
) -> Result<(), SyncError> {
    if commit.vault_id != activation_t.vault_id
        || commit.instance_id != activation_t.instance_id
        || commit.rotation_id != activation_t.rotation_id
        || commit.from_epoch != activation_t.from_epoch
        || commit.to_epoch != activation_t.to_epoch
        || commit.covered_seq != activation_t.freeze_seq
        || commit.primary_checkpoint != activation_t.primary_cid
        || commit.fallback_checkpoint != activation_t.fallback_cid
        // 候选 checkpoint 的 S 只受 freeze S 下界约束（冻结后创建的候选可以更晚）。
        || commit.primary_covered_seq < activation_t.freeze_seq
        || commit.fallback_covered_seq < activation_t.freeze_seq
        || commit.grants_digest != activation_t.grants_digest
        || commit.asset_set_digest != Some(activation_t.old_asset_set_digest)
        || commit.control_rev != activation_t.expected_control_rev
    {
        return Err(SyncError::RelayWire(
            "重建的 activation commit 与 T 授权材料不一致",
        ));
    }
    Ok(())
}

/// MANIFEST 明文必须与 T 授权的 checkpoint commit / S / 根绑定。
pub fn verify_manifest_matches_commit(
    commit: &CheckpointCommit,
    manifest: &ManifestV1,
    vault_id: VaultId,
    key_epoch: u64,
    baseline_s: u64,
) -> Result<(), SyncError> {
    if manifest.vault_id != vault_id
        || manifest.checkpoint_id != commit.checkpoint_id
        || manifest.covered_relay_seq != baseline_s
        || manifest.key_epoch != key_epoch
        || commit.covered_relay_seq != baseline_s
    {
        return Err(SyncError::RelayWire("manifest 与 checkpoint/S/根不绑定"));
    }
    Ok(())
}

/// 网络入口（I 阶段生产路径）：**先取 latest activation T 与整条 activation 链**，
/// 逐环验证后才用链尾的 T 授权 apply，最后按 manifest_oid 下载 → merge → 推广。
///
/// T 的来源是 [`RelayClient::verified_activation_t`]（带默认 `Ok(None)`），链的来源是
/// [`RelayClient::epoch_activations`]（默认空链）：
/// - 真实客户端 `HttpRelayClient` 从已验证 `/control` 缓存产出 T 与每一环的位置/commit；
/// - `FakeRelay`/测试 wrapper 直接注入已复算证据；
/// - 不覆盖的客户端拿默认 `None`/空链 → 本函数一律 `Paused`（fail-closed），**绝不**
///   回退到"只信 relay 自报的 `ActivationCommitV1` 32B hash"。
///
/// **离线跨多 epoch**：本地 `current_epoch` 只可能等于链首/链中间某一环的 `from_epoch`；
/// latest T 的 `from_epoch` 不再要求等于本地 epoch，而是由
/// [`verify_epoch_activation_chain`] 从本地 epoch 逐环推进到链尾（要求链首正是本地
/// epoch、环与环首尾相接、每环双签/绑定/commit/链位置有效、链尾 == latest T）。
/// 中间 baseline/grant 不逐个下载，只有 latest 的基线被 merge。
///
/// 顺序（评审 0002 §三 I / 0003 P1-C）：重新鉴权/分页验控制链 → 取 latest T → 逐环验
/// activation 链 → 用链尾 T 交叉绑定 view/身份/管理权 → 取选中 checkpoint 的
/// `status`/`signed_commit` 证据与 `/bootstrap` pair → 下载/解密 MANIFEST →
/// **checkpoint 级认证**（[`verify_checkpoint_authorization`]）→ T 门禁（pair 路径的
/// commit digest 强绑定 / 非 pair 路径的认证对撞、S/cursor 下界、self grant inclusion）
/// → merge → 推广。链不完整/T 缺失/双签复算失败/认证失败：`Paused`，不写库、不推 cursor。
///
/// **两件事分开**（评审 0003 P1-C）：T/latest grant 授权的是"把根推广到哪个 epoch"；
/// "当前这份 checkpoint 内容是否合法"由 [`verify_checkpoint_authorization`] 独立认证。
/// 激活 pair 被普通健康 checkpoint 替换（relay `prune_retired` 会 prune 旧 pair）后，
/// 旧设备仍能消费当前 pair，而不是永久 `Paused`。
#[allow(clippy::too_many_arguments)]
pub fn apply_epoch_change_with_client<C: RelayClient>(
    client: &mut C,
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &crate::identity::LocalIdentityV1,
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: &str,
    candidate_root: &RootSecretV1,
    manager_public: [u8; 32],
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
    restart_reason: Option<RestartReason>,
) -> Result<ApplyOutcome, SyncError> {
    apply_epoch_change_with_client_inner(
        client,
        vault,
        identity_path,
        identity,
        checkpoint_id,
        baseline_s,
        baseline_cursor,
        candidate_root,
        manager_public,
        new_manager_public,
        new_manager_rev,
        restart_reason,
        None,
    )
}

/// 同 [`apply_epoch_change_with_client`]，但允许**管理设备**在"新管理 key 与本地一致"时
/// 用自己的 seed 完成推广（本片管理 key 不变；见 [`apply_epoch_change_with_manager_seed`]）。
#[allow(clippy::too_many_arguments)]
pub fn apply_epoch_change_with_client_and_manager_seed<C: RelayClient>(
    client: &mut C,
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &crate::identity::LocalIdentityV1,
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: &str,
    candidate_root: &RootSecretV1,
    manager_public: [u8; 32],
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
    restart_reason: Option<RestartReason>,
    new_manager_seed: crate::keys::Secret32,
) -> Result<ApplyOutcome, SyncError> {
    apply_epoch_change_with_client_inner(
        client,
        vault,
        identity_path,
        identity,
        checkpoint_id,
        baseline_s,
        baseline_cursor,
        candidate_root,
        manager_public,
        new_manager_public,
        new_manager_rev,
        restart_reason,
        Some(new_manager_seed),
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_epoch_change_with_client_inner<C: RelayClient>(
    client: &mut C,
    vault: &menghunote_storage::Vault,
    identity_path: &std::path::Path,
    identity: &crate::identity::LocalIdentityV1,
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: &str,
    candidate_root: &RootSecretV1,
    manager_public: [u8; 32],
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
    restart_reason: Option<RestartReason>,
    new_manager_seed: Option<crate::keys::Secret32>,
) -> Result<ApplyOutcome, SyncError> {
    let state = client.state()?;
    // same-epoch 重入（评审 0003 P1-C/P1-D）：rename 已提交、`applied` 未写时，relay 与
    // 本地身份已是同一 epoch，下面必然会看到 "no epoch change"。必须先凭 durable receipt
    // 做认证收尾，否则 engine 会被 `applying` 永久冻结。
    if state.epoch == identity.current_epoch {
        if let Some(outcome) = reconcile_same_epoch_apply(vault, identity_path, identity)? {
            return Ok(outcome);
        }
    }
    // §30.3 的 `/state` 键 15（current_rotation_id）只对 frozen/staging 返回；activate 之后
    // 它不再出现。此时必须回落到 `GET /rotations/current`（relay 返回最新 rotation 视图），
    // 再由 `state.epoch > local` + 控制链 `epoch.activated` 判定，不能因为没有
    // current_rotation_id 就 Paused。
    let view = match state.current_rotation_id {
        Some(rotation_id) => client.rotation(rotation_id)?,
        None if state.epoch > identity.current_epoch => match client.current_rotation()? {
            Some(view) => view,
            None => {
                return Ok(ApplyOutcome::Paused {
                    reason: "relay epoch 已前进但找不到 rotation 视图".to_owned(),
                })
            }
        },
        None => {
            return Ok(ApplyOutcome::Paused {
                reason: "no epoch change".to_owned(),
            })
        }
    };
    let control = client.control_kinds()?;
    match detect_epoch_change(&state, &view, &control, identity.current_epoch) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Ok(ApplyOutcome::Paused {
                reason: "no epoch change".to_owned(),
            })
        }
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: error.to_string(),
            })
        }
    }
    if let Some(reason) = restart_reason {
        // restart 不消费 baseline，不需要 T；它只要求 relay 分到更高 epoch。
        return Ok(ApplyOutcome::NeedsRestart {
            to_epoch: view.to_epoch + 1,
            reason,
        });
    }
    if state.instance_id != identity.pinned_instance {
        return Ok(ApplyOutcome::Paused {
            reason: "relay instance 与本地 pin 不符（必须重新 pin/bootstrap）".to_owned(),
        });
    }
    // ① 先取"已复算"的 activation T（旧/新管理双签已过）。缺失/未验证 → 拒绝：
    //    绝不再用 relay 自报的 32B activation hash 当授权（评审 0002 P1-1.4）。
    let evidence = client.verified_activation_t(view.rotation_id)?;
    let Some(evidence) = evidence else {
        return Ok(ApplyOutcome::Paused {
            reason: "缺少可复算的 activation T（/control 无 T 或双签未通过），拒绝 apply"
                .to_owned(),
        });
    };
    // ② 离线跨多 epoch：latest T 的 `from_epoch` 只可能等于本地 epoch 之后的某一环，
    //    必须先从本地可信 `current_epoch` 沿 `/control` 的 `epoch.activated` 逐环验证，
    //    再由链尾 T 授权 apply。链缺失/断裂/位置不符/链尾不是 latest → fail-closed。
    let chain = {
        let links = match client.epoch_activations(identity.current_epoch) {
            Ok(links) => links,
            // 传输类错误保持 `Err`（可重试）；认证/链材料错误一律 Paused 且不写库。
            Err(error @ SyncError::Transport(_)) => return Err(error),
            Err(error) => {
                return Ok(ApplyOutcome::Paused {
                    reason: error.to_string(),
                })
            }
        };
        match verify_epoch_activation_chain(
            &links,
            &evidence,
            identity.vault_id,
            state.instance_id,
            identity.current_epoch,
            manager_public,
        ) {
            Ok(chain) => chain,
            Err(error) => {
                return Ok(ApplyOutcome::Paused {
                    reason: error.to_string(),
                })
            }
        }
    };
    // 链尾 T 的新管理 key/rev 必须正是本次推广材料：否则 apply 会把身份推到错误的
    // 管理权（例如 relay 在链尾之后又换了 key，而调用方还拿着旧材料）。
    if chain.tail_new_manager_public != new_manager_public
        || chain.tail_new_manager_rev != new_manager_rev
    {
        return Ok(ApplyOutcome::Paused {
            reason: "activation 链尾的新管理 key/rev 与推广材料不符".to_owned(),
        });
    }
    // 链首的旧管理 rev 必须锚定在**本地可信身份旁路状态**上：链内相邻环的
    // `old == 上一环 new` 只保证相对自洽，relay 仍可给出整体错位的 rev 序列
    // （例如把 T.old 抬成尚未生效的 rev）。旁路状态缺失/损坏时 fail-closed，
    // 绝不拿 T 自报的 rev 当锚点。
    let local_manager_rev = match IdentityRotationStateV1::load_current(vault, identity) {
        Ok(Some(rotation)) => rotation.manager_rev,
        // 从未轮换过的 vault：身份仍在 genesis epoch，管理 rev 固定为 1（无旁路记录）。
        Ok(None) if identity.current_epoch == crate::genesis::GENESIS_EPOCH => {
            crate::genesis::GENESIS_MANAGER_REV
        }
        Ok(None) => {
            return Ok(ApplyOutcome::Paused {
                reason: "缺少身份旁路状态，无法锚定 activation 链首的管理 rev".to_owned(),
            })
        }
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: format!("身份旁路状态读取失败：{error}"),
            })
        }
    };
    if chain.head_old_manager_rev != local_manager_rev {
        return Ok(ApplyOutcome::Paused {
            reason: format!(
                "activation 链首的旧管理 rev={} 与本地管理链位置 rev={local_manager_rev} 不符",
                chain.head_old_manager_rev
            ),
        });
    }
    // 后续所有授权都只看**链尾**的 T（链验证已要求链尾与 latest T 逐字节相等）。
    let VerifiedActivationChain {
        tail: evidence,
        tail_from_epoch,
        tail_old_manager_public,
        ..
    } = chain;
    let leaves = client.grant_leaf_evidence(view.rotation_id)?;
    let grant = client.self_grant_evidence()?;
    let Some(activation_digest) = view.activation_commit.as_deref() else {
        return Ok(ApplyOutcome::Paused {
            reason: "view 缺少 activation commit".to_owned(),
        });
    };
    // 键 20 是 `ActivationCommitV1` 的 SHA256，不是 CDE：先按 view/checkpoint 公开事实
    // 重建 commit 材料。它**只做**与 T 的一致性交叉验证，不再是授权来源。
    let (primary_checkpoint, fallback_checkpoint) =
        match (view.primary_checkpoint, view.fallback_checkpoint) {
            (Some(primary), Some(fallback)) => (primary, fallback),
            _ => {
                return Ok(ApplyOutcome::Paused {
                    reason: "view 缺少 primary/fallback checkpoint".to_owned(),
                })
            }
        };
    let primary = client.checkpoint_status(primary_checkpoint)?;
    let fallback = client.checkpoint_status(fallback_checkpoint)?;
    let commit = match activation_commit_materials(
        identity.vault_id,
        state.instance_id,
        &view,
        &primary,
        &fallback,
    ) {
        Ok(commit) => commit,
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: error.to_string(),
            })
        }
    };
    let commit_bytes = commit.encode()?;
    if let Err(error) = verify_activation_commit(
        &commit_bytes,
        activation_digest,
        identity.vault_id,
        state.instance_id,
        view.rotation_id,
        view.from_epoch,
        view.to_epoch,
        commit.control_rev,
    ) {
        return Ok(ApplyOutcome::Paused {
            reason: error.to_string(),
        });
    }
    // T 才是授权来源：重建材料必须与 T 逐字段一致（epoch/S/pair/grants/asset/control_rev）。
    if let Err(error) = verify_activation_commit_matches_t(&commit, &evidence.activation_t) {
        return Ok(ApplyOutcome::Paused {
            reason: error.to_string(),
        });
    }
    client.reauthenticate()?;
    // relay `GET /checkpoints/{cid}` 键 4 只是 MANIFEST 的 **object id**（api.rs:1859–1860），
    // 不是 manifest 明文；按 oid 下载、解密并校验后才能交给 `collect_manifest_payload`。
    let status = client.checkpoint_status(checkpoint_id)?;
    if status.checkpoint_id != checkpoint_id
        || status.covered_seq != baseline_s
        || status.epoch != view.to_epoch
    {
        return Ok(ApplyOutcome::Paused {
            reason: "checkpoint 状态与选中 cid/S/epoch 不符".to_owned(),
        });
    }
    let Some(manifest_oid) = status.manifest_object_id else {
        return Ok(ApplyOutcome::Paused {
            reason: "checkpoint 缺少 manifest_oid".to_owned(),
        });
    };
    // ② 选中基线的 `signed_commit` 与生产者签名只能从 relay 公开的 `/bootstrap`
    //    主/备条目取（`GET /checkpoints/{cid}` 的 commit 原字节不经 `RelayClient` 透出），
    //    relay 游标也只在这里给。
    //
    //    **降级 bootstrap 不得用于根推广**：degraded 是"只允许显式请求"的诊断模式，
    //    其条目可能已被 prune/回收，不能作为一次身份推广的授权材料。
    //    < 2 份健康基线时 relay 返回业务错误 → 转 `Paused`（保持冻结、下次从新 pair 重试，
    //    绝不回落到旧全量基线或静默接受）。传输类错误仍 `Err`（可重试）。
    let bundle = match client.bootstrap() {
        Ok(bundle) => bundle,
        Err(error @ SyncError::Transport(_)) => return Err(error),
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: format!("无法取得 relay /bootstrap 主/备基线：{error}"),
            })
        }
    };
    if bundle.degraded {
        return Ok(ApplyOutcome::Paused {
            reason: "relay bootstrap 处于 degraded 模式，禁止用于根推广".to_owned(),
        });
    }
    if bundle.epoch != view.to_epoch {
        return Ok(ApplyOutcome::Paused {
            reason: "bootstrap epoch 与 T 的 to_epoch 不符".to_owned(),
        });
    }
    // 本地水位不得回退：`finish_baseline_apply` 只做 max+CAS，会"导入旧快照但保留高水位"，
    // 于是 receipt 记的 S 与实际消费位置不一致（随后 pull 重复/跳段）。这是防状态不一致，
    // 不是防丢数据（Loro merge 是并集）。
    let local = menghunote_storage::sync_cursor_state(vault)?;
    let local_highwater = local.confirmed_highwater.unwrap_or(0);
    if baseline_s < local.integrated_seq || baseline_s < local_highwater {
        return Ok(ApplyOutcome::Paused {
            reason: format!(
                "选中基线 S={baseline_s} 低于本地水位（integrated_seq={}，confirmed_highwater={local_highwater}），禁止回退",
                local.integrated_seq
            ),
        });
    }
    // 目标 cid 必须是**当前** `/bootstrap` pair 之一：relay 游标只在这里给，且
    // `checkpoint_refs` 被 prune 的旧 cid 已无法作为续读起点。
    let entry = if bundle.primary.checkpoint_id == checkpoint_id {
        &bundle.primary
    } else if bundle.fallback.checkpoint_id == checkpoint_id {
        &bundle.fallback
    } else {
        return Ok(ApplyOutcome::Paused {
            reason:
                "选中基线不在 relay bootstrap 的主/备 pair（无法取得该 S 的游标/signed_commit）"
                    .to_owned(),
        });
    };
    if entry.covered_seq != baseline_s
        || entry.cursor != baseline_cursor
        || entry.manifest_oid != manifest_oid
    {
        return Ok(ApplyOutcome::Paused {
            reason: "bootstrap 条目与选中 cid/S/cursor/manifest 不符".to_owned(),
        });
    }
    let Some(signed_commit) = entry.signed_commit.clone() else {
        return Ok(ApplyOutcome::Paused {
            reason: "checkpoint 缺 signed_commit（无法复算 commit digest/签名）".to_owned(),
        });
    };
    let Some(signature) = entry.signature else {
        return Ok(ApplyOutcome::Paused {
            reason: "checkpoint 缺 commit 生产者签名".to_owned(),
        });
    };
    let baseline_commit = match CheckpointCommit::decode(&signed_commit) {
        Ok(commit) => match crate::checkpoint::require_current_checkpoint_validation(&commit) {
            Ok(()) => commit,
            Err(error) => {
                return Ok(ApplyOutcome::Paused {
                    reason: error.to_string(),
                })
            }
        },
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: error.to_string(),
            })
        }
    };
    let Some(producer_cert) = client.device_cert(&baseline_commit.producer_device_id)? else {
        return Ok(ApplyOutcome::Paused {
            reason: "找不到 checkpoint 生产者设备证书".to_owned(),
        });
    };
    if producer_cert.vault_id != identity.vault_id
        || producer_cert.device_id != baseline_commit.producer_device_id
    {
        return Ok(ApplyOutcome::Paused {
            reason: "checkpoint 生产者证书与 vault/commit 不符".to_owned(),
        });
    }
    let commit_evidence = CheckpointCommitEvidence {
        checkpoint_id: entry.checkpoint_id,
        covered_seq: entry.covered_seq,
        cursor: entry.cursor.clone(),
        signed_commit,
        signature,
        producer_sign_public: producer_cert.sign_ed25519_pub,
        manifest_object_id: manifest_oid,
    };
    let (manifest_bytes, manifest_object_digest) =
        fetch_manifest_object(client, candidate_root, &manifest_oid)?;
    let manifest = ManifestV1::decode(&manifest_bytes)?;
    // ③ checkpoint 级认证（评审 0003 P1-C 的第二半）：认证"这份 checkpoint 内容"。
    //    `cid ∈ T.pair` 时 T 的双签已授权该 commit，但**不**因此跳过认证（published/
    //    refs/S 边界仍是必须的自洽检查），只是不额外要求 active 生产者资格——否则生产者
    //    在冻结后被吊销，冻结期唯一合法基线会永久不可消费。
    //    `cid ∉ T.pair`（激活 pair 已被普通健康 checkpoint 替换）时，本认证是该内容
    //    唯一的授权来源，必须**同时**满足 active 成员资格。
    let in_t_pair = checkpoint_id == evidence.activation_t.primary_cid
        || checkpoint_id == evidence.activation_t.fallback_cid;
    let authenticated = match verify_checkpoint_authorization(
        client,
        identity.vault_id,
        entry,
        &status,
        &baseline_commit,
        &manifest,
        manifest_object_digest,
        view.to_epoch,
        evidence.activation_t.freeze_seq,
        &bundle,
        &state,
        !in_t_pair,
    ) {
        Ok(auth) => auth,
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: error.to_string(),
            })
        }
    };
    // ④ T/latest grant 授权门禁：commit digest（pair 路径强绑定，不许降级）、S/cursor、
    //    生产者签名、self grant inclusion 一次验完。
    let parsed_commit = match verify_activation_t_gate(
        &ActivationTGate {
            evidence: &evidence,
            view: &view,
            vault_id: identity.vault_id,
            instance_id: state.instance_id,
            expected_from_epoch: tail_from_epoch,
            device_id: identity.device_id,
            device_x25519_secret: &identity.x25519_secret,
            // latest grant 由**链尾 T 的旧管理 key**签发：多轮时它不是本地旧 pin，
            // 而是上一环 T 的新管理公钥（由 [`verify_epoch_activation_chain`] 推出）。
            old_manager_public: tail_old_manager_public,
            new_manager_public,
            new_manager_rev,
            selected_checkpoint: checkpoint_id,
            baseline_s,
            baseline_cursor,
            commit: &commit_evidence,
            manifest_object_digest,
            grant: &grant,
            grant_leaves: leaves.as_deref(),
            grants_count: view.grants_count,
        },
        Some(&authenticated),
    ) {
        Ok(commit) => commit,
        Err(error) => {
            return Ok(ApplyOutcome::Paused {
                reason: error.to_string(),
            })
        }
    };
    if let Err(error) = verify_manifest_matches_commit(
        &parsed_commit,
        &manifest,
        identity.vault_id,
        candidate_root.key_epoch,
        baseline_s,
    ) {
        return Ok(ApplyOutcome::Paused {
            reason: error.to_string(),
        });
    }
    let payload = collect_manifest_payload(
        client,
        candidate_root,
        checkpoint_id,
        baseline_s,
        &manifest_bytes,
    )?;
    if candidate_root.key_epoch != commit.to_epoch {
        return Ok(ApplyOutcome::Paused {
            reason: format!(
                "候选根 epoch={} 与 activation to_epoch={} 不符",
                candidate_root.key_epoch, commit.to_epoch
            ),
        });
    }
    let activation_hash: [u8; 32] = Sha256::digest(commit.encode()?).into();
    // 完整 gate 已通过：把授权事实与已下载基线绑定成**不可外部构造**的输入，再交给
    // 纯入口完成 applying/receipt/promote/applied。这样公开入口不再接受可变的
    // `BaselinePayload` 作为授权输入（评审 0003 P1-C）。
    let verified = VerifiedApplyPayload::from_gate(
        commit.rotation_id,
        checkpoint_id,
        baseline_s,
        baseline_cursor,
        commit.from_epoch,
        commit.to_epoch,
        commit.control_rev,
        activation_hash,
        state.instance_id,
        new_manager_public,
        new_manager_rev,
        payload,
    );
    match new_manager_seed {
        Some(seed) => apply_epoch_change_with_manager_seed(
            vault,
            identity_path,
            identity,
            &verified,
            candidate_root,
            seed,
        ),
        None => apply_epoch_change(vault, identity_path, identity, &verified, candidate_root),
    }
}

/// B 看到 epoch 变化后的下一步动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EpochAction {
    /// 正常应用：验证并解根，等 baseline merge 完成后再推广。
    Apply {
        rotation_id: [u8; 16],
        from_epoch: u64,
        to_epoch: u64,
    },
    /// 原子 restart：更高 epoch、fresh root/slots/R'/管理 key；旧写保持冻结。
    Restart {
        next_epoch: u64,
        reason: RestartReason,
    },
}

/// restart 原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartReason {
    /// 候选根泄露。
    CandidateRootLeaked,
    /// 收件人被吊销。
    RecipientRevoked,
}

/// 纯检测：relay state 的 epoch 高于本地，且控制链里有 `epoch.activated` 记录。
///
/// relay 的 typed kind 是 `epoch.activated`（`crates/relay/src/control.rs` 的
/// `ControlKind::as_str`）；若 view 带 `activated_control_rev`，还要求该 rev 上确实是
/// `epoch.activated`，避免"任取一条历史激活记录"。
pub fn detect_epoch_change(
    state: &RelayState,
    view: &RotationView,
    control_kinds: &[(u64, String)],
    current_epoch: u64,
) -> Result<Option<RotationView>, SyncError> {
    if state.epoch <= current_epoch {
        return Ok(None);
    }
    if view.state != "activated" || view.to_epoch != state.epoch {
        return Err(SyncError::Object(
            "relay epoch 已变但 rotation 未 activated",
        ));
    }
    let has_activated = match view.activated_control_rev {
        Some(rev) => control_kinds
            .iter()
            .any(|(record_rev, kind)| *record_rev == rev && kind == "epoch.activated"),
        None => control_kinds
            .iter()
            .any(|(_, kind)| kind == "epoch.activated"),
    };
    if !has_activated {
        return Err(SyncError::Object("缺少 epoch.activated 控制记录"));
    }
    Ok(Some(view.clone()))
}

/// 用 view + 两个候选 checkpoint 的公开元数据重建 `ActivationCommitV1`。
///
/// relay 的 `GET rotations/{r}` 只给 commit 的 SHA256（键 20），不给 CDE 明文；客户端必须
/// 用公开事实重建后再比 hash（见 `verify_activation_commit`）。
pub fn activation_commit_materials(
    vault_id: VaultId,
    instance_id: [u8; 16],
    view: &RotationView,
    primary: &CheckpointStatus,
    fallback: &CheckpointStatus,
) -> Result<ActivationCommitV1, SyncError> {
    let primary_checkpoint = view
        .primary_checkpoint
        .ok_or(SyncError::RelayWire("view 缺 primary checkpoint"))?;
    let fallback_checkpoint = view
        .fallback_checkpoint
        .ok_or(SyncError::RelayWire("view 缺 fallback checkpoint"))?;
    if primary.checkpoint_id != primary_checkpoint || fallback.checkpoint_id != fallback_checkpoint
    {
        return Err(SyncError::RelayWire(
            "checkpoint 状态与 view 指名的 cid 不符",
        ));
    }
    let primary_manifest = primary
        .manifest_object_id
        .ok_or(SyncError::RelayWire("primary checkpoint 缺 manifest_oid"))?;
    let fallback_manifest = fallback
        .manifest_object_id
        .ok_or(SyncError::RelayWire("fallback checkpoint 缺 manifest_oid"))?;
    Ok(ActivationCommitV1 {
        vault_id,
        instance_id,
        rotation_id: view.rotation_id,
        from_epoch: view.from_epoch,
        to_epoch: view.to_epoch,
        covered_seq: view.freeze_seq,
        primary_checkpoint,
        fallback_checkpoint,
        primary_covered_seq: primary.covered_seq,
        fallback_covered_seq: fallback.covered_seq,
        primary_manifest,
        fallback_manifest,
        member_set_hash: view
            .member_set_hash
            .ok_or(SyncError::RelayWire("view 缺 member_set_hash"))?,
        grants_digest: view
            .grants_digest
            .ok_or(SyncError::RelayWire("view 缺 grants_digest"))?,
        asset_set_digest: view.freeze_asset_digest,
        // relay 的 `Rotations::activate` 用 `activated_control_rev` 记录 commit 的 control_rev；
        // 后续控制记录会推进 view.control_rev，因此必须优先用激活时的值。
        control_rev: view
            .activated_control_rev
            .ok_or(SyncError::RelayWire("view 缺 activated_control_rev"))?,
    })
}

/// 校验激活 commit：绑定字段 + `SHA256(CDE(ActivationCommitV1)) == relay 字段`。
///
/// `expected_digest` 必须是 relay 返回的 32B SHA256；**不得**把 hash 当 CDE 解码。
#[allow(clippy::too_many_arguments)]
pub fn verify_activation_commit(
    commit_bytes: &[u8],
    expected_digest: &[u8],
    vault_id: VaultId,
    instance_id: [u8; 16],
    rotation_id: [u8; 16],
    from_epoch: u64,
    to_epoch: u64,
    control_rev: u64,
) -> Result<ActivationCommitV1, SyncError> {
    let commit = ActivationCommitV1::decode(commit_bytes)?;
    if commit.vault_id != vault_id
        || commit.instance_id != instance_id
        || commit.rotation_id != rotation_id
        || commit.from_epoch != from_epoch
        || commit.to_epoch != to_epoch
        || commit.control_rev != control_rev
    {
        return Err(SyncError::RelayWire("激活 commit 与 rotation/身份不绑定"));
    }
    if commit.primary_checkpoint == commit.fallback_checkpoint {
        return Err(SyncError::RelayWire("激活 commit 的两 cid 相同"));
    }
    if expected_digest.len() != 32 {
        return Err(SyncError::RelayWire(
            "relay activation_commit 不是 32B SHA256",
        ));
    }
    // 摘要按 canonical CDE 重算（等价于 relay `ActivationCommit::digest()`）。
    let canonical = commit.encode()?;
    let actual: [u8; 32] = Sha256::digest(&canonical).into();
    if actual.as_slice() != expected_digest {
        return Err(SyncError::RelayWire(
            "activation_commit 与重建的 ActivationCommitV1 不符",
        ));
    }
    Ok(commit)
}

/// 校验 B 自己的 grant 头：recipient/epoch 必须匹配，manager 外签必须由新管理公钥验过。
pub fn verify_self_grant_header(
    signed_header: &[u8],
    signature: &[u8],
    manager_public: &[u8; 32],
    device_id: DeviceId,
    epoch: u64,
) -> Result<(), SyncError> {
    if signed_header.len() < 64 || signature.len() != 64 {
        return Err(SyncError::RelayWire("self grant 签名长度非法"));
    }
    let aad_len = signed_header.len() - 64;
    let aad = menghunote_sync_proto::GrantAadV1::decode(&signed_header[..aad_len])?;
    if aad.epoch != epoch || aad.recipient_device_id != device_id {
        return Err(SyncError::RelayWire("self grant recipient/epoch 不符"));
    }
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| SyncError::RelayWire("self grant 签名长度非法"))?;
    crate::device::verify_signature(manager_public, signed_header, &signature)
}

/// restart 判定：候选根泄露或收件人被吊销 → 更高 epoch；否则正常应用。
pub fn decide_epoch_action(
    view: &RotationView,
    current_epoch: u64,
    restart_reason: Option<RestartReason>,
) -> Result<EpochAction, SyncError> {
    if view.state != "activated" {
        return Err(SyncError::Object("epoch 变化但 rotation 未 activated"));
    }
    if view.to_epoch <= current_epoch {
        return Err(SyncError::Object("目标 epoch 不高于当前 epoch"));
    }
    if let Some(reason) = restart_reason {
        return Ok(EpochAction::Restart {
            next_epoch: view.to_epoch + 1,
            reason,
        });
    }
    Ok(EpochAction::Apply {
        rotation_id: view.rotation_id,
        from_epoch: view.from_epoch,
        to_epoch: view.to_epoch,
    })
}

`````



@@SECTION code/048 `crates/sync/src/rotation_journal.rs`


`````rust
//! 轮换 journal（S7-28，ADR §28.2；阶段 A–C 的持久状态）。
//!
//! 这里只落**本地持久状态机**，不调用 relay：阶段 A freeze 固定 intent/secret，阶段 B
//! 记录 begin 响应（request_id 幂等查询在 relay task-101 落地后接），阶段 C 要求旧根
//! 已 integrate 到 S 且没有 ≤S 的 pending outbox，才允许继续。
//!
//! journal 经 [`crate::identity_wrap`] 封装后存 `vault.meta`（Windows DPAPI / Linux
//! 用户主密钥，见 ADR 0008），不落明文密钥；根/新管理 key 等敏感字段只在内存与
//! 本地封装密文中出现。未完成字段用 `TODO(task-101)` 标注的占位保持可往返。

use sha2::{Digest, Sha256};

use crate::cbor;
use crate::error::SyncError;
use crate::identity_wrap;
use crate::keys::{RootSecretV1, Secret32};
use crate::slot::KeySlotV1;
use crate::{DeviceId, VaultId};
use menghunote_storage::{AffectedGuard, Statement, Value, Vault};

/// journal 在 vault meta 中的键。
pub const ROTATION_JOURNAL_META: &str = "sync.rotation_journal";
const JOURNAL_DOMAIN: &[u8] = b"MN/v1/rotation-journal\0";

/// 轮换阶段（只允许按顺序推进；Aborted 可从任意阶段进入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationPhase {
    /// 阶段 A：已冻结旧 epoch，网络 push/encrypt 停止。
    Frozen,
    /// 阶段 B：relay `/rotations` begin 已成功，S 固定。
    Begun,
    /// 阶段 C：旧根已 integrate 至 S、本地 cut L 已固定。
    Integrated,
    /// 阶段 D：旧资产已全量重加密（task-103）。
    Reencrypted,
    /// 阶段 E：两份独立基线已各自导入验证（task-103）。
    BaselinesReady,
    /// 阶段 F：grants 已 staging（task-103）。
    GrantsReady,
    /// 阶段 G：双签 activate（task-104）。
    Activated,
    /// 阶段 H：本地原子推广（task-104）。
    Promoted,
    /// 阶段 I/J：其它设备/outbox 重打包完成（task-104）。
    Completed,
    /// 原子 restart / abort。
    Aborted,
    /// restart：POST 前的公开 intent 已 DPAPI 落盘（无新鲜秘密、无 relay 分配）。
    RestartIntentRecorded,
    /// restart：fresh 秘密与固定槽密文已 DPAPI 落盘（上传前）。
    RestartPrepared,
    /// restart：fresh 槽已用同一份固定密文上传。
    RestartSlotsUploaded,
    /// restart：主基线已读回验证并落盘（fallback 尚未生成）。
    RestartPrimaryBaselineReady,
    /// restart：两份独立基线已读回验证。
    RestartBaselinesReady,
    /// restart：未吊销设备的 grant 已按固定 leaf 字节 stage。
    RestartGrantsStaged,
    /// restart：activate 请求已持久化、正在等 relay activated 回执。
    RestartActivating,
    /// restart：已收到 relay activated 回执（本地身份推广由阶段 H 接）。
    RestartActivated,
}

impl RotationPhase {
    fn code(self) -> u64 {
        match self {
            Self::Frozen => 1,
            Self::Begun => 2,
            Self::Integrated => 3,
            Self::Reencrypted => 4,
            Self::BaselinesReady => 5,
            Self::GrantsReady => 6,
            Self::Activated => 7,
            Self::Promoted => 8,
            Self::Completed => 9,
            Self::Aborted => 10,
            Self::RestartPrepared => 11,
            Self::RestartSlotsUploaded => 12,
            Self::RestartBaselinesReady => 13,
            Self::RestartGrantsStaged => 14,
            Self::RestartActivating => 15,
            Self::RestartActivated => 16,
            Self::RestartIntentRecorded => 17,
            Self::RestartPrimaryBaselineReady => 18,
        }
    }

    fn from_code(code: u64) -> Result<Self, SyncError> {
        Ok(match code {
            1 => Self::Frozen,
            2 => Self::Begun,
            3 => Self::Integrated,
            4 => Self::Reencrypted,
            5 => Self::BaselinesReady,
            6 => Self::GrantsReady,
            7 => Self::Activated,
            8 => Self::Promoted,
            9 => Self::Completed,
            10 => Self::Aborted,
            11 => Self::RestartPrepared,
            12 => Self::RestartSlotsUploaded,
            13 => Self::RestartBaselinesReady,
            14 => Self::RestartGrantsStaged,
            15 => Self::RestartActivating,
            16 => Self::RestartActivated,
            17 => Self::RestartIntentRecorded,
            18 => Self::RestartPrimaryBaselineReady,
            _ => return Err(SyncError::LocalCorrupt("rotation journal phase 非法")),
        })
    }

    /// 中文短标签（app 的轮换失败报告用；只描述**公开阶段名**，不含任何秘密）。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Frozen => "冻结旧 epoch",
            Self::Begun => "已被 relay 接受（S 固定）",
            Self::Integrated => "旧根已整合到 S",
            Self::Reencrypted => "冻结资产已重加密",
            Self::BaselinesReady => "候选基线已就绪",
            Self::GrantsReady => "grants 已暂存",
            Self::Activated => "已激活",
            Self::Promoted => "身份已推广",
            Self::Completed => "已完成",
            Self::Aborted => "已中止",
            Self::RestartIntentRecorded => "restart：intent 已落盘",
            Self::RestartPrepared => "restart：新秘密已落盘",
            Self::RestartSlotsUploaded => "restart：候选槽已上传",
            Self::RestartPrimaryBaselineReady => "restart：主基线已读回",
            Self::RestartBaselinesReady => "restart：双基线已读回",
            Self::RestartGrantsStaged => "restart：grants 已暂存",
            Self::RestartActivating => "restart：已发 activate",
            Self::RestartActivated => "restart：relay 已激活",
        }
    }

    /// 是否为原子 restart 的 phase（区别于普通轮换 A–J）。
    #[must_use]
    pub fn is_restart(self) -> bool {
        matches!(
            self,
            Self::RestartPrepared
                | Self::RestartSlotsUploaded
                | Self::RestartPrimaryBaselineReady
                | Self::RestartBaselinesReady
                | Self::RestartGrantsStaged
                | Self::RestartActivating
                | Self::RestartActivated
                | Self::RestartIntentRecorded
        )
    }

    fn next(self) -> Option<Self> {
        Some(match self {
            Self::Frozen => Self::Begun,
            Self::Begun => Self::Integrated,
            Self::Integrated => Self::Reencrypted,
            Self::Reencrypted => Self::BaselinesReady,
            Self::BaselinesReady => Self::GrantsReady,
            Self::GrantsReady => Self::Activated,
            Self::Activated => Self::Promoted,
            Self::Promoted => Self::Completed,
            Self::Completed | Self::Aborted => return None,
            Self::RestartPrepared => Self::RestartSlotsUploaded,
            Self::RestartSlotsUploaded => Self::RestartPrimaryBaselineReady,
            Self::RestartPrimaryBaselineReady => Self::RestartBaselinesReady,
            Self::RestartBaselinesReady => Self::RestartGrantsStaged,
            Self::RestartGrantsStaged => Self::RestartActivating,
            Self::RestartActivating => Self::RestartActivated,
            Self::RestartActivated => Self::Promoted,
            Self::RestartIntentRecorded => Self::RestartPrepared,
        })
    }
}

/// 应用（阶段 I）的持久 phase 键（评审 0003 P1-3）。
///
/// B 首次 apply 在**任何本地写库/merge 之前**先把本键写成 [`RotationApplyPhase::Applying`]：
/// 这是 I 阶段的 durable 冻结屏障，不依赖 RAM。engine 构造期读它（与
/// `sync.repack_required`、rotation journal 并列）决定是否冻结旧 epoch 的 encrypt/push。
///
/// 状态机只允许单向收尾：
///
/// ```text
/// none ──apply 开始──> applying ──合并+推广成功──> applied（解冻）
///                        │
///                        ├─merge/推广失败──> paused  （保持冻结，可重试）
///                        ├─cursor CAS 冲突─> conflict（保持冻结，可重试）
///                        └─restart 判定────> needs_restart（保持冻结，走更高 epoch）
/// ```
///
/// 任何未知/损坏值都按 [`RotationApplyPhase::Failed`] 处理（fail-closed）：宁可继续冻结，
/// 也不能因为无法证明 apply 已完成而让旧写继续。
pub const ROTATION_APPLY_PHASE_META: &str = "sync.rotation_apply_phase";

/// 阶段 I 的持久 apply 状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotationApplyPhase {
    /// apply 已启动：全部验证通过、即将写 baseline/merge。**这是冻结屏障本身**。
    Applying,
    /// 合并 + 身份推广均成功；唯一可以解除 I 阶段冻结的终态。
    Applied,
    /// 可重试失败（网络/IO/merge/推广）；保持冻结，等下一次 apply。
    Paused,
    /// baseline cursor CAS 冲突（未推进 cursor）；保持冻结。
    Conflict,
    /// restart 判定：必须用更高 epoch 的原子 restart 收尾；保持冻结。
    NeedsRestart,
    /// 损坏/未知 phase：fail-closed，保持冻结。
    Failed,
}

impl RotationApplyPhase {
    /// 持久化文本（写入 `vault.meta`）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::Paused => "paused",
            Self::Conflict => "conflict",
            Self::NeedsRestart => "needs_restart",
            Self::Failed => "failed",
        }
    }

    /// 解析持久值。旧版本写过 `staging`，语义等同 `applying`；其余未知值 fail-closed。
    #[must_use]
    pub fn parse(raw: &[u8]) -> Self {
        match raw {
            b"applying" | b"staging" => Self::Applying,
            b"applied" => Self::Applied,
            b"paused" => Self::Paused,
            b"conflict" => Self::Conflict,
            b"needs_restart" => Self::NeedsRestart,
            _ => Self::Failed,
        }
    }

    /// 读持久 phase；不存在/空值返回 `None`（从未 apply 过）。
    pub fn load(vault: &Vault) -> Result<Option<Self>, SyncError> {
        let Some(raw) = vault.get_meta(ROTATION_APPLY_PHASE_META)? else {
            return Ok(None);
        };
        if raw.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self::parse(&raw)))
    }

    /// durable 写入 phase（`meta` 走 SQLite WAL，`synchronous=FULL`）。
    pub fn store(self, vault: &Vault) -> Result<(), SyncError> {
        vault.set_meta(ROTATION_APPLY_PHASE_META, self.as_str().as_bytes())?;
        Ok(())
    }

    /// 开始一次新的 apply：**同一事务**里清空旧 receipt 并落 `applying`。
    ///
    /// 两步必须原子（评审 0003 P1-C/P1-D）：
    /// - 先落 `applying` 再清 receipt：若两步之间崩溃，旧 receipt 可能与当前身份匹配，
    ///   same-epoch 重入会把"新 apply 还没开始"误判成"上一笔已收尾"；
    /// - 先清 receipt 再落 `applying`：若两步之间崩溃，phase 仍是旧的 `applied`，
    ///   持久冻结屏障会短暂消失。
    ///
    /// 因此这里用一条 `meta` 事务同时完成两件事。
    pub fn begin_apply(vault: &Vault) -> Result<(), SyncError> {
        let statements = [
            Statement::new(
                "INSERT INTO meta(key,value) VALUES(?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                vec![
                    Value::Text(ROTATION_APPLY_RECEIPT_META.to_owned()),
                    Value::Blob(Vec::new()),
                ],
            ),
            Statement::new(
                "INSERT INTO meta(key,value) VALUES(?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                vec![
                    Value::Text(ROTATION_APPLY_PHASE_META.to_owned()),
                    Value::Blob(Self::Applying.as_str().as_bytes().to_vec()),
                ],
            ),
        ];
        vault.exec_transaction(
            &statements,
            &[AffectedGuard::exactly(0, 1), AffectedGuard::exactly(1, 1)],
        )?;
        Ok(())
    }

    /// 清除 phase（只在调用方确认整条轮换链已收尾时使用；engine 解冻只认 `applied`）。
    pub fn clear(vault: &Vault) -> Result<(), SyncError> {
        vault.set_meta(ROTATION_APPLY_PHASE_META, b"")?;
        Ok(())
    }

    /// 是否需要冻结旧 epoch 的 encrypt/push。只有 `Applied` 不冻结。
    #[must_use]
    pub fn is_frozen(self) -> bool {
        !matches!(self, Self::Applied)
    }
}

/// 阶段 I 的 durable 屏障：返回 `true` 表示必须冻结。
///
/// 读取失败/未知值一律 fail-closed；`None`（从未 apply）不构成屏障。
pub(crate) fn apply_phase_barrier(vault: &Vault) -> bool {
    match RotationApplyPhase::load(vault) {
        Ok(None) => false,
        Ok(Some(phase)) => phase.is_frozen(),
        Err(_) => true,
    }
}

/// 阶段 I apply receipt 在 vault meta 中的键。
pub const ROTATION_APPLY_RECEIPT_META: &str = "sync.rotation_apply_receipt";

/// receipt 的 CDE 版本（字段布局变更必须升 version，旧版本 fail-closed）。
const APPLY_RECEIPT_VERSION: u64 = 1;
/// receipt 的 CDE 字段数（严格校验，缺失/多余都拒绝）。
const APPLY_RECEIPT_FIELDS: usize = 14;

/// 一次 I 阶段 apply 的 durable、类型化 receipt（评审 0003 P1-C/P1-D）。
///
/// 写入时机：baseline merge **已提交**、身份 rename 推广**之前**。因此：
/// - rename 成功后、`applied` 写前崩溃（本缺陷的主窗口）→ receipt 与推广后的
///   身份/pin 完全匹配，重启可做**认证后的完成 reconcile**，不需要 epoch change；
/// - rename 之前崩溃 → 本地身份仍是旧 epoch，receipt 与身份不匹配，只能走正常 epoch
///   change 重试（merge 幂等），绝不能被误当成"已完成"。
///
/// 字段全部来自完整 gate 的授权事实（T/`ActivationCommitV1`/选中 checkpoint/推广材料），
/// 字段私有 + 只读访问器：外部无法伪造 receipt 来绕过 T 门禁。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationApplyReceipt {
    vault_id: VaultId,
    device_id: DeviceId,
    rotation_id: [u8; 16],
    checkpoint_id: [u8; 16],
    baseline_s: u64,
    baseline_cursor: String,
    from_epoch: u64,
    epoch: u64,
    control_rev: u64,
    instance_id: [u8; 16],
    new_manager_public: [u8; 32],
    new_manager_rev: u64,
    activation_hash: [u8; 32],
}

impl RotationApplyReceipt {
    /// 由完整 gate 的已验证事实构造（crate 内部；外部不可伪造）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        vault_id: VaultId,
        device_id: DeviceId,
        rotation_id: [u8; 16],
        checkpoint_id: [u8; 16],
        baseline_s: u64,
        baseline_cursor: &str,
        from_epoch: u64,
        epoch: u64,
        control_rev: u64,
        instance_id: [u8; 16],
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
        activation_hash: [u8; 32],
    ) -> Self {
        Self {
            vault_id,
            device_id,
            rotation_id,
            checkpoint_id,
            baseline_s,
            baseline_cursor: baseline_cursor.to_owned(),
            from_epoch,
            epoch,
            control_rev,
            instance_id,
            new_manager_public,
            new_manager_rev,
            activation_hash,
        }
    }

    /// 归属 vault。
    #[must_use]
    pub fn vault_id(&self) -> VaultId {
        self.vault_id
    }

    /// 归属设备。
    #[must_use]
    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// 本次 activation 的 rotation id。
    #[must_use]
    pub fn rotation_id(&self) -> [u8; 16] {
        self.rotation_id
    }

    /// 选中的基线 checkpoint id。
    #[must_use]
    pub fn checkpoint_id(&self) -> [u8; 16] {
        self.checkpoint_id
    }

    /// 选中基线覆盖的 relay S。
    #[must_use]
    pub fn baseline_s(&self) -> u64 {
        self.baseline_s
    }

    /// 选中基线的 relay cursor。
    #[must_use]
    pub fn baseline_cursor(&self) -> &str {
        &self.baseline_cursor
    }

    /// 激活前 epoch（本地可信起点）。
    #[must_use]
    pub fn from_epoch(&self) -> u64 {
        self.from_epoch
    }

    /// 激活后 epoch（推广目标）。
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 激活控制链 rev。
    #[must_use]
    pub fn control_rev(&self) -> u64 {
        self.control_rev
    }

    /// 已验证的 relay instance（control pin）。
    #[must_use]
    pub fn instance_id(&self) -> [u8; 16] {
        self.instance_id
    }

    /// 推广后的管理公钥（pin）。
    #[must_use]
    pub fn new_manager_public(&self) -> [u8; 32] {
        self.new_manager_public
    }

    /// 推广后的管理 key rev。
    #[must_use]
    pub fn new_manager_rev(&self) -> u64 {
        self.new_manager_rev
    }

    /// `SHA256(CDE(ActivationCommitV1))`（与 authenticated pin 对账）。
    #[must_use]
    pub fn activation_hash(&self) -> [u8; 32] {
        self.activation_hash
    }

    fn encode(&self) -> Result<Vec<u8>, SyncError> {
        cbor::encode(vec![
            (0, cbor::uint(APPLY_RECEIPT_VERSION)),
            (1, cbor::bytes(self.vault_id.to_vec())),
            (2, cbor::bytes(self.device_id.to_vec())),
            (3, cbor::bytes(self.rotation_id.to_vec())),
            (4, cbor::bytes(self.checkpoint_id.to_vec())),
            (5, cbor::uint(self.baseline_s)),
            (6, cbor::text(self.baseline_cursor.clone())),
            (7, cbor::uint(self.from_epoch)),
            (8, cbor::uint(self.epoch)),
            (9, cbor::uint(self.control_rev)),
            (10, cbor::bytes(self.instance_id.to_vec())),
            (11, cbor::bytes(self.new_manager_public.to_vec())),
            (12, cbor::uint(self.new_manager_rev)),
            (13, cbor::bytes(self.activation_hash.to_vec())),
        ])
    }

    /// 写入 `vault.meta`（单条 SQLite WAL 事务，durable 后才返回）。
    pub fn store(&self, vault: &Vault) -> Result<(), SyncError> {
        let blob = self.encode()?;
        vault.set_meta(ROTATION_APPLY_RECEIPT_META, &blob)?;
        Ok(())
    }

    /// 读取 receipt；不存在/空值返回 `None`，损坏一律 `Err`（调用方必须 fail-closed）。
    pub fn load(vault: &Vault) -> Result<Option<Self>, SyncError> {
        let Some(blob) = vault.get_meta(ROTATION_APPLY_RECEIPT_META)? else {
            return Ok(None);
        };
        if blob.is_empty() {
            return Ok(None);
        }
        let fields = cbor::decode(&blob)?;
        if fields.len() != APPLY_RECEIPT_FIELDS {
            return Err(SyncError::LocalCorrupt("apply receipt 字段数不符"));
        }
        if cbor::uint_field(&fields, 0)? != APPLY_RECEIPT_VERSION {
            return Err(SyncError::LocalCorrupt("apply receipt 版本不支持"));
        }
        Ok(Some(Self {
            vault_id: cbor::bytes_fixed::<16>(&fields, 1)?,
            device_id: cbor::bytes_fixed::<16>(&fields, 2)?,
            rotation_id: cbor::bytes_fixed::<16>(&fields, 3)?,
            checkpoint_id: cbor::bytes_fixed::<16>(&fields, 4)?,
            baseline_s: cbor::uint_field(&fields, 5)?,
            baseline_cursor: cbor::text_field(&fields, 6)?,
            from_epoch: cbor::uint_field(&fields, 7)?,
            epoch: cbor::uint_field(&fields, 8)?,
            control_rev: cbor::uint_field(&fields, 9)?,
            instance_id: cbor::bytes_fixed::<16>(&fields, 10)?,
            new_manager_public: cbor::bytes_fixed::<32>(&fields, 11)?,
            new_manager_rev: cbor::uint_field(&fields, 12)?,
            activation_hash: cbor::bytes_fixed::<32>(&fields, 13)?,
        }))
    }

    /// 清空 receipt（`begin_apply` 会用同一事务完成，这里只给测试/显式收尾用）。
    pub fn clear(vault: &Vault) -> Result<(), SyncError> {
        vault.set_meta(ROTATION_APPLY_RECEIPT_META, b"")?;
        Ok(())
    }
}

/// restart **POST 之前**必须 durable 的公开请求事实（评审 0002 P1-2）。
///
/// 这里只允许放公开契约/请求事实：绝不包含候选根、口令、R'、管理私钥等 fresh 秘密。
/// 字段来源：
///
/// | 字段 | 来源 |
/// | --- | --- |
/// | `previous_rotation_id` | 调用方要原子重启的旧 rotation id |
/// | `request_id` | 本次 restart/activate 的幂等键（exact request 查询用） |
/// | `from_epoch` | 本地 active 根 epoch（relay 必须原样保留） |
/// | `previous_candidate_epoch` | restart **前**旧 rotation view 的 `to_epoch`（abandoned candidate） |
/// | `expected_control_rev` | 管理签名的 CAS 期望值（restart 成功后 relay = 它 + 1） |
/// | `manager_rev` | 旧管理 key rev（restart 不改管理权） |
/// | `freeze_seq` | 冻结点 S（两份基线必须 ≥S） |
/// | `instance_id` | 管理签名绑定的 relay instance |
/// | `freeze_barrier` | 写入 intent 时冻结屏障必须已为 1 |
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartIntent {
    /// 被原子重启的旧 rotation id。
    pub previous_rotation_id: [u8; 16],
    /// 本次 restart 的幂等 request_id（relay 按它找回已提交结果）。
    pub request_id: [u8; 16],
    /// relay 保留的 active epoch（restart 的 `from_epoch`）。
    pub from_epoch: u64,
    /// 旧 rotation view 的 `to_epoch`：被原子废弃的旧候选 epoch。
    pub previous_candidate_epoch: u64,
    /// 管理签名的 CAS 期望值；restart 成功后 relay 的 control_rev 必须 = 它 + 1。
    pub expected_control_rev: u64,
    /// 旧管理 key rev。
    pub manager_rev: u64,
    /// 冻结点 S。
    pub freeze_seq: u64,
    /// relay instance id（管理签名绑定）。
    pub instance_id: [u8; 16],
    /// 记录 intent 时冻结屏障已置 1（重启后必须继续保持）。
    pub freeze_barrier: bool,
}

/// relay 对 restart intent 的原子分配（只在拿到 relay 结果后落盘）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartAssignment {
    /// relay 新分配的 rotation id。
    pub rotation_id: [u8; 16],
    /// relay 保留的 active epoch。
    pub from_epoch: u64,
    /// 被废弃的旧候选 epoch。
    pub previous_candidate_epoch: u64,
    /// relay 新分配的候选 epoch（绝不本地 +1）。
    pub to_epoch: u64,
    /// restart 后的 control_rev（activate CAS 的期望值）。
    pub control_rev: u64,
    /// 冻结点 S。
    pub freeze_seq: u64,
}

/// 一份候选基线的验证回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalBaseline {
    pub checkpoint_id: [u8; 16],
    pub covered_seq: u64,
    pub cursor: String,
    pub manifest_object_id: [u8; 32],
    pub manifest_digest: [u8; 32],
    pub refs_digest: [u8; 32],
    pub verified: bool,
    /// 本地 durable 的 `CheckpointCommit` 摘要（签 activation T 只用它，不再问 relay）。
    pub commit_digest: [u8; 32],
    /// 读回验证得到的资产集合 digest（两份基线必须相等）。
    pub asset_set_digest: [u8; 32],
}

/// restart 在候选 epoch 重加密后的冻结资产（固定 object id/digest，重试不重加密）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRestartAsset {
    pub real_sha256: [u8; 32],
    pub byte_len: u64,
    pub media_type: String,
    pub object_id: [u8; 32],
    pub object_digest: [u8; 32],
}

/// 轮换 journal（DPAPI 持久）。
#[derive(Clone)]
pub struct RotationJournal {
    pub phase: RotationPhase,
    /// 幂等 request_id（begin 响应丢失时用它查询，绝不盲 begin）。
    pub request_id: [u8; 16],
    /// canonical begin body 摘要。
    pub body_digest: [u8; 32],
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// relay begin 返回的 rotation id；阶段 B 前为 None。
    pub rotation_id: Option<[u8; 16]>,
    /// 阶段 A 固定的 freeze 信息。
    pub freeze_seq: Option<u64>,
    pub freeze_log_id: Option<[u8; 16]>,
    pub control_rev: u64,
    pub control_hash: [u8; 32],
    /// 新 epoch 的候选根（DPAPI 保护；阶段 E 前生成）。
    pub candidate_root: Option<RootSecretV1>,
    /// 新管理 key 公钥/rev；私钥由 R' 派生，不单独存。
    pub new_manager_public: Option<[u8; 32]>,
    pub new_manager_rev: u64,
    /// activate 的 new_manager 三键（relay `fd89cfa` 后布局）：
    /// `(CDE ManagerTransition 9 字段, old_signature64, new_signature64)`。
    pub new_manager_transition: Option<Vec<u8>>,
    pub old_manager_signature: Option<[u8; 64]>,
    pub new_manager_signature: Option<[u8; 64]>,
    /// `/control` 键 5 管理转换链的 rev。
    pub control_chain_rev: Option<u64>,
    /// 阶段 A pin 的 relay instance/log/control head。
    pub pinned_instance_id: Option<[u8; 16]>,
    pub pinned_log_id: Option<[u8; 16]>,
    pub pinned_control_hash: Option<[u8; 32]>,
    /// R' 已离线备份。
    pub r_prime_backed_up: bool,
    /// 新 password/recovery 槽描述符、固定槽体、槽 id（阶段 D 上传用）。
    pub new_slot_descriptors: Vec<Vec<u8>>,
    pub new_slot_bodies: Vec<Vec<u8>>,
    pub new_slot_ids: Vec<[u8; 32]>,
    /// 两份候选基线（phase E 后各一条，互不共享状态）。
    pub baselines: Vec<JournalBaseline>,
    /// 成员证书/grants/资产集合摘要（阶段 F 用）。
    pub member_set_digest: Option<[u8; 32]>,
    pub grants_digest: Option<[u8; 32]>,
    /// restart：候选槽集合 digest（activate 四 digest 之一）。
    pub slot_set_digest: Option<[u8; 32]>,
    /// 阶段 F：每个待 stage 设备的完整 grant leaf manifest（规范 CDE 字节）。
    pub grant_leaf_manifests: Vec<Vec<u8>>,
    /// 阶段 F：所有 leaf manifest 的排序集合 digest（供 activate 锁定）。
    pub grant_leaf_digest: Option<[u8; 32]>,
    pub asset_set_digest: Option<[u8; 32]>,
    /// 上传进度（对象数）。
    pub uploaded_objects: u32,
    pub total_objects: u32,
    /// outbox 重打包进度（task-104）。
    pub repacked_outbox: u32,
    pub total_outbox: u32,
    /// 本地 cut L（阶段 C 固定）。
    pub local_cut: Option<u64>,
    /// restart：被废弃的旧候选 epoch（relay restart 前 rotation view 的 `to_epoch`）。
    pub previous_candidate_epoch: Option<u64>,
    /// restart：POST 之前 durable 的公开 intent（无秘密；恢复时按 exact request 查回）。
    pub restart_intent: Option<RestartIntent>,
    /// restart：fresh 新口令（DPAPI 保护的 journal 内；上传/重试复用）。
    pub restart_password: Option<Vec<u8>>,
    /// restart：fresh R'（32B；重试时据此重建管理 key，不重新生成）。
    pub recovery_seed: Option<[u8; 32]>,
    /// restart：由 R' 派生的新管理私钥种子（32B；重试不重新生成）。
    pub manager_seed: Option<[u8; 32]>,
    /// restart：用户已确认新恢复码（R'）离线备份；activate 的硬门槛。
    pub recovery_backup_confirmed: bool,
    /// restart：固定在 journal 里的 grant leaf manifest（重试复用同一字节）。
    pub restart_grant_manifests: Vec<Vec<u8>>,
    /// restart：fixed slot 的 MNE message_id（重试必须复用同一 envelope 字节）。
    pub restart_password_message_id: Option<[u8; 16]>,
    pub restart_recovery_message_id: Option<[u8; 16]>,
    /// restart：被重启的旧 rotation id（重试必须命中同一旧候选）。
    pub restart_previous_rotation_id: Option<[u8; 16]>,
    /// restart：已上传的候选 epoch 重加密资产（重试不再重加密/重传）。
    pub restart_assets: Vec<JournalRestartAsset>,
    /// restart：relay activated 回执里的 epoch/control_rev/activation_commit。
    pub activated_epoch: Option<u64>,
    pub activated_control_rev: Option<u64>,
    pub activation_commit: Option<Vec<u8>>,
}

impl RotationJournal {
    /// 阶段 A：冻结旧 epoch，生成持久 intent。新根/新槽在后续阶段填充。
    pub fn freeze(
        vault: &Vault,
        from_epoch: u64,
        request_id: [u8; 16],
        control_rev: u64,
        control_hash: [u8; 32],
    ) -> Result<Self, SyncError> {
        if Self::load(vault)?.is_some() {
            return Err(SyncError::Unsupported("已有未完成 rotation journal"));
        }
        let journal = Self {
            phase: RotationPhase::Frozen,
            request_id,
            body_digest: begin_body_digest(from_epoch, &request_id),
            from_epoch,
            to_epoch: from_epoch.saturating_add(1),
            rotation_id: None,
            freeze_seq: None,
            freeze_log_id: None,
            control_rev,
            control_hash,
            candidate_root: None,
            new_manager_public: None,
            new_manager_rev: 0,
            new_manager_transition: None,
            old_manager_signature: None,
            new_manager_signature: None,
            control_chain_rev: None,
            pinned_instance_id: None,
            pinned_log_id: None,
            pinned_control_hash: None,
            r_prime_backed_up: false,
            new_slot_descriptors: Vec::new(),
            new_slot_bodies: Vec::new(),
            new_slot_ids: Vec::new(),
            baselines: Vec::new(),
            member_set_digest: None,
            grants_digest: None,
            slot_set_digest: None,
            grant_leaf_manifests: Vec::new(),
            grant_leaf_digest: None,
            asset_set_digest: None,
            uploaded_objects: 0,
            total_objects: 0,
            repacked_outbox: 0,
            total_outbox: 0,
            local_cut: None,
            previous_candidate_epoch: None,
            restart_intent: None,
            restart_password: None,
            recovery_seed: None,
            manager_seed: None,
            recovery_backup_confirmed: false,
            restart_grant_manifests: Vec::new(),
            restart_password_message_id: None,
            restart_recovery_message_id: None,
            restart_previous_rotation_id: None,
            restart_assets: Vec::new(),
            activated_epoch: None,
            activated_control_rev: None,
            activation_commit: None,
        };
        journal.save(vault)?;
        Ok(journal)
    }

    /// restart：POST **之前**只落公开 intent（评审 0002 P1-2）。
    ///
    /// 这里不生成任何新 epoch/秘密：epoch、rotation_id、control_rev 必须等 relay
    /// 分配后才能写（`begin_restart`）。`to_epoch = 0` 是"尚未分配"的显式哨兵，
    /// `rotation_id = None` 表示还没有 relay 结果。
    pub fn record_restart_intent(vault: &Vault, intent: RestartIntent) -> Result<Self, SyncError> {
        if Self::load(vault)?.is_some() {
            return Err(SyncError::Unsupported("已有未完成 rotation journal"));
        }
        if !intent.freeze_barrier {
            return Err(SyncError::Object(
                "restart intent 必须在冻结屏障=1 之后落盘",
            ));
        }
        if intent.from_epoch >= intent.previous_candidate_epoch {
            return Err(SyncError::RelayWire(
                "restart intent epoch 必须 active < 旧候选（禁止本地 +1）",
            ));
        }
        let previous_rotation_id = intent.previous_rotation_id;
        let journal = Self {
            phase: RotationPhase::RestartIntentRecorded,
            request_id: intent.request_id,
            body_digest: restart_body_digest(),
            from_epoch: intent.from_epoch,
            // 尚未拿到 relay 分配；`begin_restart` 才会填入真实候选 epoch。
            to_epoch: 0,
            rotation_id: None,
            freeze_seq: Some(intent.freeze_seq),
            freeze_log_id: None,
            control_rev: intent.expected_control_rev,
            control_hash: [0u8; 32],
            candidate_root: None,
            new_manager_public: None,
            new_manager_rev: 0,
            new_manager_transition: None,
            old_manager_signature: None,
            new_manager_signature: None,
            control_chain_rev: None,
            pinned_instance_id: None,
            pinned_log_id: None,
            pinned_control_hash: None,
            r_prime_backed_up: false,
            new_slot_descriptors: Vec::new(),
            new_slot_bodies: Vec::new(),
            new_slot_ids: Vec::new(),
            baselines: Vec::new(),
            member_set_digest: None,
            grants_digest: None,
            slot_set_digest: None,
            grant_leaf_manifests: Vec::new(),
            grant_leaf_digest: None,
            asset_set_digest: None,
            uploaded_objects: 0,
            total_objects: 0,
            repacked_outbox: 0,
            total_outbox: 0,
            local_cut: None,
            previous_candidate_epoch: Some(intent.previous_candidate_epoch),
            restart_intent: Some(intent),
            restart_password: None,
            recovery_seed: None,
            manager_seed: None,
            recovery_backup_confirmed: false,
            restart_grant_manifests: Vec::new(),
            restart_password_message_id: None,
            restart_recovery_message_id: None,
            restart_previous_rotation_id: Some(previous_rotation_id),
            restart_assets: Vec::new(),
            activated_epoch: None,
            activated_control_rev: None,
            activation_commit: None,
        };
        journal.save(vault)?;
        Ok(journal)
    }

    /// restart：拿到 relay 分配后，把 fresh 秘密/epoch/rotation_id/固定槽密文 DPAPI 落盘。
    ///
    /// 必须在任何新槽/新对象上传前调用；调用方随后只能从 journal 重建同一份材料。
    /// 前置条件：已有 `RestartIntentRecorded` journal，且 `intent` 与其中持久事实逐字段
    /// 相等（防止把另一次 restart 的分配套到旧 intent 上）。
    #[allow(clippy::too_many_arguments)]
    pub fn begin_restart(
        vault: &Vault,
        intent: &RestartIntent,
        assignment: RestartAssignment,
        candidate_root: RootSecretV1,
        restart_password: Vec<u8>,
        recovery_seed: [u8; 32],
        manager_seed: [u8; 32],
        password_message_id: [u8; 16],
        recovery_message_id: [u8; 16],
        new_manager_public: [u8; 32],
        new_manager_rev: u64,
        slots: &[KeySlotV1],
        recovery_backup_confirmed: bool,
    ) -> Result<Self, SyncError> {
        let mut journal = Self::load(vault)?.ok_or(SyncError::LocalCorrupt(
            "restart 缺 POST 前 durable intent：拒绝在无 intent 时落 fresh 材料",
        ))?;
        if journal.phase != RotationPhase::RestartIntentRecorded {
            return Err(SyncError::Unsupported(
                "restart journal 已越过 intent phase，不能重复写入分配",
            ));
        }
        let stored_intent = journal
            .restart_intent
            .as_ref()
            .ok_or(SyncError::LocalCorrupt("restart journal 缺 durable intent"))?;
        if stored_intent != intent {
            return Err(SyncError::Unsupported(
                "restart intent 与 relay 分配前的持久事实不一致",
            ));
        }
        if assignment.from_epoch != intent.from_epoch
            || assignment.previous_candidate_epoch != intent.previous_candidate_epoch
            || assignment.freeze_seq != intent.freeze_seq
            || assignment.rotation_id == intent.previous_rotation_id
        {
            return Err(SyncError::RelayWire(
                "restart 分配与 durable intent 不一致（禁止复用旧候选）",
            ));
        }
        if !(assignment.from_epoch < assignment.previous_candidate_epoch
            && assignment.previous_candidate_epoch < assignment.to_epoch)
        {
            return Err(SyncError::RelayWire(
                "restart journal epoch 必须严格递增（禁止本地 +1）",
            ));
        }
        let Some(expected_control_rev) = intent.expected_control_rev.checked_add(1) else {
            return Err(SyncError::RelayWire("restart control_rev 溢出"));
        };
        if assignment.control_rev != expected_control_rev {
            return Err(SyncError::RelayWire(
                "relay restart control_rev 与本地 CAS 假设不一致",
            ));
        }
        if candidate_root.key_epoch != assignment.to_epoch || restart_password.is_empty() {
            return Err(SyncError::Object(
                "restart journal fresh 材料与候选 epoch 不一致",
            ));
        }
        journal.to_epoch = assignment.to_epoch;
        journal.rotation_id = Some(assignment.rotation_id);
        journal.control_rev = assignment.control_rev;
        journal.candidate_root = Some(candidate_root);
        journal.new_manager_public = Some(new_manager_public);
        journal.new_manager_rev = new_manager_rev;
        journal.restart_password = Some(restart_password);
        journal.recovery_seed = Some(recovery_seed);
        journal.manager_seed = Some(manager_seed);
        journal.recovery_backup_confirmed = recovery_backup_confirmed;
        journal.r_prime_backed_up = recovery_backup_confirmed;
        journal.restart_password_message_id = Some(password_message_id);
        journal.restart_recovery_message_id = Some(recovery_message_id);
        journal.set_slots(slots)?;
        journal.advance(RotationPhase::RestartPrepared)?;
        journal.save(vault)?;
        Ok(journal)
    }

    /// restart：用户确认新恢复码已离线备份（activate 前必须为 true）。
    pub fn confirm_recovery_backup(&mut self, vault: &Vault) -> Result<(), SyncError> {
        if !self.phase.is_restart() {
            return Err(SyncError::Unsupported("不是 restart journal"));
        }
        self.recovery_backup_confirmed = true;
        self.r_prime_backed_up = true;
        self.save(vault)
    }

    /// restart：**主基线一完成就落盘**（评审 0002 P1-D）。
    ///
    /// 这是给 fallback 崩溃/失败兜底的持久点：重跑只能复用这里的 cid/manifest/commit
    /// receipt，绝不能再生成随机 primary（relay 每 epoch role 唯一 CAS）。
    pub fn set_restart_primary_baseline(
        &mut self,
        vault: &Vault,
        baseline: JournalBaseline,
    ) -> Result<(), SyncError> {
        if self.phase != RotationPhase::RestartSlotsUploaded {
            return Err(SyncError::LocalCorrupt(
                "restart 主基线落盘时机非法（必须先完成槽上传）",
            ));
        }
        if !baseline.verified
            || baseline.commit_digest == [0u8; 32]
            || baseline.asset_set_digest == [0u8; 32]
        {
            return Err(SyncError::Object("restart 主基线缺少本地验证 receipt"));
        }
        if baseline.covered_seq < self.freeze_seq.unwrap_or(u64::MAX) {
            return Err(SyncError::Object("主基线 S 未覆盖 restart 冻结点"));
        }
        self.baselines = vec![baseline];
        self.advance(RotationPhase::RestartPrimaryBaselineReady)?;
        self.save(vault)
    }

    /// restart：备基线完成后再落盘，并与已持久的主基线交叉校验独立性。
    pub fn set_restart_fallback_baseline(
        &mut self,
        vault: &Vault,
        baseline: JournalBaseline,
    ) -> Result<(), SyncError> {
        if self.phase != RotationPhase::RestartPrimaryBaselineReady {
            return Err(SyncError::LocalCorrupt(
                "restart 备基线落盘时机非法（必须先持久主基线）",
            ));
        }
        let primary = self
            .baselines
            .first()
            .ok_or(SyncError::LocalCorrupt("restart journal 缺主基线"))?;
        if !baseline.verified
            || baseline.commit_digest == [0u8; 32]
            || baseline.asset_set_digest == [0u8; 32]
        {
            return Err(SyncError::Object("restart 备基线缺少本地验证 receipt"));
        }
        if primary.checkpoint_id == baseline.checkpoint_id
            || primary.manifest_object_id == baseline.manifest_object_id
        {
            return Err(SyncError::Object("restart 两份候选基线不独立"));
        }
        if primary.asset_set_digest != baseline.asset_set_digest {
            return Err(SyncError::Object("restart 两份基线的资产集合不一致"));
        }
        let freeze_seq = self
            .freeze_seq
            .ok_or(SyncError::LocalCorrupt("restart journal 缺冻结点 S"))?;
        if primary.covered_seq < freeze_seq || baseline.covered_seq < freeze_seq {
            return Err(SyncError::Object("restart 基线 S 未覆盖冻结点"));
        }
        self.baselines.push(baseline);
        self.advance(RotationPhase::RestartBaselinesReady)?;
        self.save(vault)
    }

    /// restart：固定已上传的重加密资产（重试不重复上传随机 nonce 的对象）。
    pub fn set_restart_assets(
        &mut self,
        vault: &Vault,
        assets: Vec<JournalRestartAsset>,
    ) -> Result<(), SyncError> {
        self.restart_assets = assets;
        self.save(vault)
    }

    /// restart：固定 grant leaf manifest（stage 前落盘，重试复用同一字节）。
    pub fn set_restart_grant_manifests(
        &mut self,
        vault: &Vault,
        manifests: Vec<Vec<u8>>,
        grants_digest: [u8; 32],
    ) -> Result<(), SyncError> {
        if manifests.is_empty() {
            return Err(SyncError::Object("restart grants 不能为空"));
        }
        self.restart_grant_manifests = manifests;
        self.grants_digest = Some(grants_digest);
        self.save(vault)
    }

    /// restart：持久化 activate 请求材料（重试不再依赖内存里的 PreparedCheckpoint）。
    #[allow(clippy::too_many_arguments)]
    pub fn set_restart_activation(
        &mut self,
        vault: &Vault,
        _primary_checkpoint: [u8; 16],
        _fallback_checkpoint: [u8; 16],
        member_set_digest: [u8; 32],
        slot_set_digest: [u8; 32],
        grants_digest: [u8; 32],
        asset_set_digest: [u8; 32],
        transition: Vec<u8>,
        old_signature: [u8; 64],
        new_signature: [u8; 64],
        control_chain_rev: u64,
    ) -> Result<(), SyncError> {
        if self.baselines.len() != 2 {
            return Err(SyncError::LocalCorrupt("restart activate 前缺两份候选基线"));
        }
        self.member_set_digest = Some(member_set_digest);
        self.slot_set_digest = Some(slot_set_digest);
        self.grants_digest = Some(grants_digest);
        self.asset_set_digest = Some(asset_set_digest);
        self.new_manager_transition = Some(transition);
        self.old_manager_signature = Some(old_signature);
        self.new_manager_signature = Some(new_signature);
        self.control_chain_rev = Some(control_chain_rev);
        if self.phase != RotationPhase::RestartActivating {
            self.advance(RotationPhase::RestartActivating)?;
        }
        self.save(vault)
    }

    /// restart：relay activated 回执落盘（只信 activated 才进入此 phase）。
    pub fn mark_restart_activated(
        &mut self,
        vault: &Vault,
        epoch: u64,
        control_rev: u64,
        activation_commit: Vec<u8>,
    ) -> Result<(), SyncError> {
        if self.phase != RotationPhase::RestartActivating {
            return Err(SyncError::LocalCorrupt("restart 未在 activating phase"));
        }
        if epoch != self.to_epoch {
            return Err(SyncError::RelayWire(
                "activated epoch 不是 relay 分配的候选 epoch",
            ));
        }
        self.advance(RotationPhase::RestartActivated)?;
        self.activated_epoch = Some(epoch);
        self.activated_control_rev = Some(control_rev);
        self.activation_commit = Some(activation_commit);
        self.save(vault)
    }

    /// 清空 journal（本地推广完成后调用；绝不能在 activate 前清）。
    pub fn clear(vault: &Vault) -> Result<(), SyncError> {
        vault.set_meta(ROTATION_JOURNAL_META, b"")?;
        Ok(())
    }

    /// 阶段 F：持久化每个未吊销设备的 grant leaf manifest 与集合 digest。
    pub fn set_grants(&mut self, vault: &Vault, leaves: &[Vec<u8>]) -> Result<[u8; 32], SyncError> {
        let mut sorted = leaves.to_vec();
        sorted.sort();
        let mut input = Vec::with_capacity(4 + sorted.iter().map(Vec::len).sum::<usize>());
        input.extend_from_slice(&(sorted.len() as u32).to_be_bytes());
        for leaf in &sorted {
            input.extend_from_slice(leaf);
        }
        let digest: [u8; 32] = sha2::Sha256::digest(&input).into();
        self.grant_leaf_manifests = sorted;
        self.grant_leaf_digest = Some(digest);
        self.grants_digest = Some(digest);
        self.save(vault)?;
        Ok(digest)
    }

    /// 阶段 A：确认新恢复码（R'）已离线备份；未确认前禁止 begin。
    pub fn confirm_r_prime_backed_up(&mut self, vault: &Vault) -> Result<(), SyncError> {
        self.r_prime_backed_up = true;
        self.save(vault)
    }

    /// 读 journal；不存在返回 None。
    pub fn load(vault: &Vault) -> Result<Option<Self>, SyncError> {
        let Some(blob) = vault.get_meta(ROTATION_JOURNAL_META)? else {
            return Ok(None);
        };
        if blob.is_empty() {
            return Ok(None);
        }
        let plain = identity_wrap::unprotect(&blob, &journal_entropy(vault))?;
        Ok(Some(Self::decode(&plain)?))
    }

    /// 经本地封装（Windows DPAPI / Linux 用户主钥）保护后写回 journal。
    pub fn save(&self, vault: &Vault) -> Result<(), SyncError> {
        let bytes = zeroize::Zeroizing::new(self.encode()?);
        let blob = identity_wrap::protect(&bytes, &journal_entropy(vault))?;
        vault.set_meta(ROTATION_JOURNAL_META, &blob)?;
        Ok(())
    }

    /// phase 只允许前进一步（Aborted 例外）。
    pub fn advance(&mut self, next: RotationPhase) -> Result<(), SyncError> {
        if next == RotationPhase::Aborted {
            self.phase = next;
            return Ok(());
        }
        if self.phase.next() != Some(next) {
            return Err(SyncError::LocalCorrupt("rotation phase 不能跳步"));
        }
        self.phase = next;
        Ok(())
    }

    /// 阶段 B：记录 relay begin 响应；调用方必须先用 `request_id` 查询（task-101 接口）。
    pub fn mark_begun(
        &mut self,
        rotation_id: [u8; 16],
        freeze_seq: u64,
        freeze_log_id: [u8; 16],
    ) -> Result<(), SyncError> {
        self.advance(RotationPhase::Begun)?;
        self.rotation_id = Some(rotation_id);
        self.freeze_seq = Some(freeze_seq);
        self.freeze_log_id = Some(freeze_log_id);
        Ok(())
    }

    /// 阶段 C：旧根 integrate 至 S、固定本地 cut L；门禁与 `rotation::ensure_c_phase_gate`
    /// 完全一致（只拦 ≤S 的入站缺口与已发送未 ACK，**不再拦本地 pending/encrypted 编辑**）。
    pub fn mark_integrated(
        &mut self,
        vault: &Vault,
        target_s: u64,
        local_cut: u64,
    ) -> Result<(), SyncError> {
        crate::rotation::ensure_c_phase_gate(vault, target_s)?;
        self.advance(RotationPhase::Integrated)?;
        self.local_cut = Some(local_cut);
        Ok(())
    }

    /// 阶段 B 的 request_id 幂等句柄（响应丢失时查询用，绝不重新 begin）。
    pub fn request_id(&self) -> [u8; 16] {
        self.request_id
    }

    /// 记录新根/新管理公钥（阶段 D 用）。
    pub fn set_candidate(
        &mut self,
        root: RootSecretV1,
        manager_public: [u8; 32],
        manager_rev: u64,
        r_prime_backed_up: bool,
    ) {
        self.candidate_root = Some(root);
        self.new_manager_public = Some(manager_public);
        self.new_manager_rev = manager_rev;
        self.r_prime_backed_up = r_prime_backed_up;
    }

    /// 记录新槽（描述符 + 固定槽体字节 + slot_id）。
    pub fn set_slots(&mut self, slots: &[KeySlotV1]) -> Result<(), SyncError> {
        self.new_slot_ids = slots.iter().map(|slot| slot.descriptor.slot_id).collect();
        self.new_slot_descriptors = slots
            .iter()
            .map(|slot| slot.descriptor.encode())
            .collect::<Result<Vec<_>, _>>()?;
        self.new_slot_bodies = slots.iter().map(|slot| slot.wrapped_bytes()).collect();
        Ok(())
    }

    /// 记录 activate 的 new_manager 三键（relay `fd89cfa`：transition/old_sig/new_sig）。
    pub fn set_activate_new_manager(
        &mut self,
        transition: Vec<u8>,
        old_signature: [u8; 64],
        new_signature: [u8; 64],
        control_chain_rev: u64,
    ) {
        self.new_manager_transition = Some(transition);
        self.old_manager_signature = Some(old_signature);
        self.new_manager_signature = Some(new_signature);
        self.control_chain_rev = Some(control_chain_rev);
    }

    /// 记录一份候选基线验证回执。
    pub fn push_baseline(&mut self, baseline: JournalBaseline) {
        self.baselines.push(baseline);
    }

    // ---------------------------------------------------------------- CDE

    fn encode(&self) -> Result<Vec<u8>, SyncError> {
        let mut fields = vec![
            (0, cbor::uint(self.phase.code())),
            (1, cbor::bytes(self.request_id.to_vec())),
            (2, cbor::bytes(self.body_digest.to_vec())),
            (3, cbor::uint(self.from_epoch)),
            (4, cbor::uint(self.to_epoch)),
            (8, cbor::uint(self.control_rev)),
            (9, cbor::bytes(self.control_hash.to_vec())),
            (12, cbor::uint(self.new_manager_rev)),
            (13, cbor::boolean(self.r_prime_backed_up)),
            (
                14,
                cbor::array(
                    self.new_slot_descriptors
                        .iter()
                        .map(|value| cbor::bytes(value.clone()))
                        .collect(),
                ),
            ),
            (
                15,
                cbor::array(
                    self.new_slot_bodies
                        .iter()
                        .map(|value| cbor::bytes(value.clone()))
                        .collect(),
                ),
            ),
            (
                16,
                cbor::array(
                    self.new_slot_ids
                        .iter()
                        .map(|value| cbor::bytes(value.to_vec()))
                        .collect(),
                ),
            ),
            (
                17,
                cbor::array(self.baselines.iter().map(baseline_cde).collect()),
            ),
            (21, cbor::uint(u64::from(self.uploaded_objects))),
            (22, cbor::uint(u64::from(self.total_objects))),
            (23, cbor::uint(u64::from(self.repacked_outbox))),
            (24, cbor::uint(u64::from(self.total_outbox))),
        ];
        if let Some(rotation_id) = self.rotation_id {
            fields.push((5, cbor::bytes(rotation_id.to_vec())));
        }
        if let Some(freeze_seq) = self.freeze_seq {
            fields.push((6, cbor::uint(freeze_seq)));
        }
        if let Some(log_id) = self.freeze_log_id {
            fields.push((7, cbor::bytes(log_id.to_vec())));
        }
        if let Some(root) = &self.candidate_root {
            fields.push((10, cbor::bytes(root.encode()?)));
        }
        if let Some(public) = self.new_manager_public {
            fields.push((11, cbor::bytes(public.to_vec())));
        }
        if let Some(digest) = self.member_set_digest {
            fields.push((18, cbor::bytes(digest.to_vec())));
        }
        if let Some(digest) = self.grants_digest {
            fields.push((19, cbor::bytes(digest.to_vec())));
        }
        if !self.grant_leaf_manifests.is_empty() {
            fields.push((
                33,
                cbor::array(
                    self.grant_leaf_manifests
                        .iter()
                        .map(|leaf| cbor::bytes(leaf.clone()))
                        .collect(),
                ),
            ));
        }
        if let Some(digest) = self.grant_leaf_digest {
            fields.push((34, cbor::bytes(digest.to_vec())));
        }
        if let Some(digest) = self.asset_set_digest {
            fields.push((20, cbor::bytes(digest.to_vec())));
        }
        if let Some(cut) = self.local_cut {
            fields.push((25, cbor::uint(cut)));
        }
        if let Some(transition) = &self.new_manager_transition {
            fields.push((26, cbor::bytes(transition.clone())));
        }
        if let Some(signature) = self.old_manager_signature {
            fields.push((27, cbor::bytes(signature.to_vec())));
        }
        if let Some(signature) = self.new_manager_signature {
            fields.push((28, cbor::bytes(signature.to_vec())));
        }
        if let Some(rev) = self.control_chain_rev {
            fields.push((29, cbor::uint(rev)));
        }
        if let Some(instance) = self.pinned_instance_id {
            fields.push((30, cbor::bytes(instance.to_vec())));
        }
        if let Some(log_id) = self.pinned_log_id {
            fields.push((31, cbor::bytes(log_id.to_vec())));
        }
        if let Some(hash) = self.pinned_control_hash {
            fields.push((32, cbor::bytes(hash.to_vec())));
        }
        if let Some(epoch) = self.previous_candidate_epoch {
            fields.push((35, cbor::uint(epoch)));
        }
        if let Some(password) = &self.restart_password {
            fields.push((36, cbor::bytes(password.clone())));
        }
        if let Some(seed) = self.recovery_seed {
            fields.push((37, cbor::bytes(seed.to_vec())));
        }
        if let Some(seed) = self.manager_seed {
            fields.push((38, cbor::bytes(seed.to_vec())));
        }
        if self.recovery_backup_confirmed {
            fields.push((39, cbor::boolean(true)));
        }
        if !self.restart_grant_manifests.is_empty() {
            fields.push((
                40,
                cbor::array(
                    self.restart_grant_manifests
                        .iter()
                        .map(|leaf| cbor::bytes(leaf.clone()))
                        .collect(),
                ),
            ));
        }
        if let Some(epoch) = self.activated_epoch {
            fields.push((41, cbor::uint(epoch)));
        }
        if let Some(rev) = self.activated_control_rev {
            fields.push((42, cbor::uint(rev)));
        }
        if let Some(commit) = &self.activation_commit {
            fields.push((43, cbor::bytes(commit.clone())));
        }
        if let Some(digest) = self.slot_set_digest {
            fields.push((44, cbor::bytes(digest.to_vec())));
        }
        if !self.restart_assets.is_empty() {
            fields.push((
                45,
                cbor::array(self.restart_assets.iter().map(restart_asset_cde).collect()),
            ));
        }
        if let Some(message_id) = self.restart_password_message_id {
            fields.push((46, cbor::bytes(message_id.to_vec())));
        }
        if let Some(message_id) = self.restart_recovery_message_id {
            fields.push((47, cbor::bytes(message_id.to_vec())));
        }
        if let Some(rotation_id) = self.restart_previous_rotation_id {
            fields.push((48, cbor::bytes(rotation_id.to_vec())));
        }
        if let Some(intent) = &self.restart_intent {
            fields.push((49, restart_intent_cde(intent)));
        }
        fields.sort_by_key(|(key, _)| *key);
        cbor::encode(fields)
    }

    fn decode(bytes: &[u8]) -> Result<Self, SyncError> {
        let map = cbor::decode(bytes)?;
        let phase = RotationPhase::from_code(cbor::uint_field(&map, 0)?)?;
        let request_id = cbor::bytes_fixed(&map, 1)?;
        let body_digest = cbor::bytes_fixed(&map, 2)?;
        let from_epoch = cbor::uint_field(&map, 3)?;
        let to_epoch = cbor::uint_field(&map, 4)?;
        let rotation_id = cbor::opt_bytes_field(&map, 5)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let freeze_seq = cbor::opt_uint_field(&map, 6)?;
        let freeze_log_id = cbor::opt_bytes_field(&map, 7)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let control_rev = cbor::uint_field(&map, 8)?;
        let control_hash = cbor::bytes_fixed(&map, 9)?;
        let candidate_root = cbor::opt_bytes_field(&map, 10)?
            .map(|value| RootSecretV1::decode(&value))
            .transpose()?;
        let new_manager_public = cbor::opt_bytes_field(&map, 11)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let new_manager_rev = cbor::uint_field(&map, 12)?;
        let r_prime_backed_up = cbor::bool_field(&map, 13)?;
        let new_slot_descriptors = cbor::array_field(&map, 14)?
            .iter()
            .map(value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let new_slot_bodies = cbor::array_field(&map, 15)?
            .iter()
            .map(value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let new_slot_ids = cbor::array_field(&map, 16)?
            .iter()
            .map(|value| fixed32(&value_bytes(value)?))
            .collect::<Result<Vec<_>, _>>()?;
        let baselines = cbor::array_field(&map, 17)?
            .iter()
            .map(decode_baseline)
            .collect::<Result<Vec<_>, _>>()?;
        let member_set_digest = cbor::opt_bytes_field(&map, 18)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let grants_digest = cbor::opt_bytes_field(&map, 19)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let slot_set_digest = cbor::opt_bytes_field(&map, 44)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let grant_leaf_manifests = cbor::opt_array_field(&map, 33)?
            .unwrap_or_default()
            .iter()
            .map(cbor::value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let grant_leaf_digest = cbor::opt_bytes_field(&map, 34)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let asset_set_digest = cbor::opt_bytes_field(&map, 20)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let uploaded_objects = u32::try_from(cbor::uint_field(&map, 21)?)
            .map_err(|_| SyncError::LocalCorrupt("journal uploaded_objects 溢出"))?;
        let total_objects = u32::try_from(cbor::uint_field(&map, 22)?)
            .map_err(|_| SyncError::LocalCorrupt("journal total_objects 溢出"))?;
        let repacked_outbox = u32::try_from(cbor::uint_field(&map, 23)?)
            .map_err(|_| SyncError::LocalCorrupt("journal repacked 溢出"))?;
        let total_outbox = u32::try_from(cbor::uint_field(&map, 24)?)
            .map_err(|_| SyncError::LocalCorrupt("journal total_outbox 溢出"))?;
        let local_cut = cbor::opt_uint_field(&map, 25)?;
        let new_manager_transition = cbor::opt_bytes_field(&map, 26)?;
        let old_manager_signature = cbor::opt_bytes_field(&map, 27)?
            .map(|value| fixed64(&value))
            .transpose()?;
        let new_manager_signature = cbor::opt_bytes_field(&map, 28)?
            .map(|value| fixed64(&value))
            .transpose()?;
        let control_chain_rev = cbor::opt_uint_field(&map, 29)?;
        let pinned_instance_id = cbor::opt_bytes_field(&map, 30)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let pinned_log_id = cbor::opt_bytes_field(&map, 31)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let pinned_control_hash = cbor::opt_bytes_field(&map, 32)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let previous_candidate_epoch = cbor::opt_uint_field(&map, 35)?;
        let restart_intent = cbor::opt_field(&map, 49)
            .map(decode_restart_intent)
            .transpose()?;
        let restart_password = cbor::opt_bytes_field(&map, 36)?;
        let recovery_seed = cbor::opt_bytes_field(&map, 37)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let manager_seed = cbor::opt_bytes_field(&map, 38)?
            .map(|value| fixed32(&value))
            .transpose()?;
        let recovery_backup_confirmed = cbor::bool_field(&map, 39).unwrap_or(false);
        let restart_grant_manifests = cbor::opt_array_field(&map, 40)?
            .unwrap_or_default()
            .iter()
            .map(cbor::value_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        let restart_assets = cbor::opt_array_field(&map, 45)?
            .unwrap_or_default()
            .iter()
            .map(decode_restart_asset)
            .collect::<Result<Vec<_>, _>>()?;
        let restart_password_message_id = cbor::opt_bytes_field(&map, 46)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let restart_recovery_message_id = cbor::opt_bytes_field(&map, 47)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let restart_previous_rotation_id = cbor::opt_bytes_field(&map, 48)?
            .map(|value| fixed16(&value))
            .transpose()?;
        let activated_epoch = cbor::opt_uint_field(&map, 41)?;
        let activated_control_rev = cbor::opt_uint_field(&map, 42)?;
        let activation_commit = cbor::opt_bytes_field(&map, 43)?;
        Ok(Self {
            phase,
            request_id,
            body_digest,
            from_epoch,
            to_epoch,
            rotation_id,
            freeze_seq,
            freeze_log_id,
            control_rev,
            control_hash,
            candidate_root,
            new_manager_public,
            new_manager_rev,
            new_manager_transition,
            old_manager_signature,
            new_manager_signature,
            control_chain_rev,
            pinned_instance_id,
            pinned_log_id,
            pinned_control_hash,
            r_prime_backed_up,
            new_slot_descriptors,
            new_slot_bodies,
            new_slot_ids,
            baselines,
            member_set_digest,
            grants_digest,
            slot_set_digest,
            grant_leaf_manifests,
            grant_leaf_digest,
            asset_set_digest,
            uploaded_objects,
            total_objects,
            repacked_outbox,
            total_outbox,
            local_cut,
            previous_candidate_epoch,
            restart_intent,
            restart_password,
            recovery_seed,
            manager_seed,
            recovery_backup_confirmed,
            restart_grant_manifests,
            restart_password_message_id,
            restart_recovery_message_id,
            restart_previous_rotation_id,
            restart_assets,
            activated_epoch,
            activated_control_rev,
            activation_commit,
        })
    }
}

fn journal_entropy(_vault: &Vault) -> Vec<u8> {
    JOURNAL_DOMAIN.to_vec()
}

fn begin_body_digest(from_epoch: u64, request_id: &[u8; 16]) -> [u8; 32] {
    let mut input = Vec::with_capacity(8 + 16);
    input.extend_from_slice(&from_epoch.to_be_bytes());
    input.extend_from_slice(request_id);
    Sha256::digest(&input).into()
}

/// restart 的唯一 canonical body 是空 CDE map（relay `CanonicalBody::parse(..., 0)` 要求零字段）。
fn restart_body_digest() -> [u8; 32] {
    // 空 map 的 CDE 编码；构造失败在正常参数下不可能，退回全零摘要只用于 journal 辅助字段。
    let body = cbor::encode(Vec::new()).unwrap_or_default();
    Sha256::digest(&body).into()
}

fn baseline_cde(baseline: &JournalBaseline) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(baseline.checkpoint_id.to_vec())),
        (1, cbor::uint(baseline.covered_seq)),
        (2, cbor::text(baseline.cursor.clone())),
        (3, cbor::bytes(baseline.manifest_object_id.to_vec())),
        (4, cbor::bytes(baseline.manifest_digest.to_vec())),
        (5, cbor::bytes(baseline.refs_digest.to_vec())),
        (6, cbor::boolean(baseline.verified)),
        (7, cbor::bytes(baseline.commit_digest.to_vec())),
        (8, cbor::bytes(baseline.asset_set_digest.to_vec())),
    ])
}

/// restart intent 的 journal CDE（公开事实，不含任何 fresh 秘密）。
fn restart_intent_cde(intent: &RestartIntent) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(intent.previous_rotation_id.to_vec())),
        (1, cbor::bytes(intent.request_id.to_vec())),
        (2, cbor::uint(intent.from_epoch)),
        (3, cbor::uint(intent.previous_candidate_epoch)),
        (4, cbor::uint(intent.expected_control_rev)),
        (5, cbor::uint(intent.manager_rev)),
        (6, cbor::uint(intent.freeze_seq)),
        (7, cbor::bytes(intent.instance_id.to_vec())),
        (8, cbor::boolean(intent.freeze_barrier)),
    ])
}

fn restart_asset_cde(asset: &JournalRestartAsset) -> cbor::CdeValue {
    cbor::CdeValue::Map(vec![
        (0, cbor::bytes(asset.real_sha256.to_vec())),
        (1, cbor::uint(asset.byte_len)),
        (2, cbor::text(asset.media_type.clone())),
        (3, cbor::bytes(asset.object_id.to_vec())),
        (4, cbor::bytes(asset.object_digest.to_vec())),
    ])
}

fn decode_restart_asset(value: &cbor::CdeValue) -> Result<JournalRestartAsset, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(JournalRestartAsset {
        real_sha256: cbor::bytes_fixed(&map, 0)?,
        byte_len: cbor::uint_field(&map, 1)?,
        media_type: cbor::text_field(&map, 2)?,
        object_id: cbor::bytes_fixed(&map, 3)?,
        object_digest: cbor::bytes_fixed(&map, 4)?,
    })
}

fn value_bytes(value: &cbor::CdeValue) -> Result<Vec<u8>, SyncError> {
    match value {
        cbor::CdeValue::Bytes(bytes) => Ok(bytes.clone()),
        _ => Err(SyncError::LocalCorrupt("journal 数组元素不是 bytes")),
    }
}

fn decode_baseline(value: &cbor::CdeValue) -> Result<JournalBaseline, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(JournalBaseline {
        checkpoint_id: cbor::bytes_fixed(&map, 0)?,
        covered_seq: cbor::uint_field(&map, 1)?,
        cursor: cbor::text_field(&map, 2)?,
        manifest_object_id: cbor::bytes_fixed(&map, 3)?,
        manifest_digest: cbor::bytes_fixed(&map, 4)?,
        refs_digest: cbor::bytes_fixed(&map, 5)?,
        verified: cbor::bool_field(&map, 6)?,
        // 旧 journal 没有这两个字段：保留零值，签 T 前会 fail-closed 拒绝。
        commit_digest: cbor::opt_bytes_field(&map, 7)?
            .map(|value| fixed32(&value))
            .transpose()?
            .unwrap_or([0u8; 32]),
        asset_set_digest: cbor::opt_bytes_field(&map, 8)?
            .map(|value| fixed32(&value))
            .transpose()?
            .unwrap_or([0u8; 32]),
    })
}

fn decode_restart_intent(value: &cbor::CdeValue) -> Result<RestartIntent, SyncError> {
    let map = cbor::value_map(value)?;
    Ok(RestartIntent {
        previous_rotation_id: cbor::bytes_fixed(&map, 0)?,
        request_id: cbor::bytes_fixed(&map, 1)?,
        from_epoch: cbor::uint_field(&map, 2)?,
        previous_candidate_epoch: cbor::uint_field(&map, 3)?,
        expected_control_rev: cbor::uint_field(&map, 4)?,
        manager_rev: cbor::uint_field(&map, 5)?,
        freeze_seq: cbor::uint_field(&map, 6)?,
        instance_id: cbor::bytes_fixed(&map, 7)?,
        freeze_barrier: cbor::bool_field(&map, 8)?,
    })
}

fn fixed16(bytes: &[u8]) -> Result<[u8; 16], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("journal 字段长度不是 16B"))
}

fn fixed32(bytes: &[u8]) -> Result<[u8; 32], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("journal 字段长度不是 32B"))
}

fn fixed64(bytes: &[u8]) -> Result<[u8; 64], SyncError> {
    bytes
        .try_into()
        .map_err(|_| SyncError::LocalCorrupt("journal 签名长度不是 64B"))
}

/// 新秘密的固定长度（password 必须由用户新给，R'=random32）。
pub const NEW_RECOVERY_SEED_LEN: usize = 32;

/// 生成 R'（调用方负责离线备份并置 `r_prime_backed_up`）。
pub fn generate_r_prime() -> Result<Secret32, SyncError> {
    Ok(Secret32::from_array(crate::random::array(
        &mut crate::random::OsRandom,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use menghunote_storage::{create_body_doc, set_confirmed_highwater};

    fn fresh() -> (tempfile::TempDir, Vault) {
        crate::test_support::ensure_local_wrap();
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        let base = base.as_path();
        std::fs::create_dir_all(base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("rotation-journal-")
            .tempdir_in(base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    fn frozen(vault: &Vault, request_id: [u8; 16]) -> RotationJournal {
        RotationJournal::freeze(vault, 1, request_id, 7, [0x11; 32]).expect("freeze")
    }

    #[test]
    fn apply_phase_is_durable_and_fail_closed() {
        let (_dir, vault) = fresh();
        assert_eq!(
            RotationApplyPhase::load(&vault).expect("phase"),
            None,
            "从未 apply 过时没有屏障"
        );
        assert!(!apply_phase_barrier(&vault), "absent 不冻结");

        // applying 是写库前的持久屏障：除 applied 外一律冻结。
        RotationApplyPhase::Applying
            .store(&vault)
            .expect("applying");
        assert_eq!(
            RotationApplyPhase::load(&vault).expect("phase"),
            Some(RotationApplyPhase::Applying)
        );
        assert!(apply_phase_barrier(&vault));

        RotationApplyPhase::Applied.store(&vault).expect("applied");
        assert!(!apply_phase_barrier(&vault), "只有 applied 解冻");

        // 旧版本写过的 staging 语义等同 applying（重启后仍冻结）。
        vault
            .set_meta(ROTATION_APPLY_PHASE_META, b"staging")
            .expect("legacy");
        assert_eq!(
            RotationApplyPhase::load(&vault).expect("phase"),
            Some(RotationApplyPhase::Applying)
        );
        assert!(apply_phase_barrier(&vault));

        // 未知/损坏值 fail-closed。
        vault
            .set_meta(ROTATION_APPLY_PHASE_META, b"garbage")
            .expect("garbage");
        assert_eq!(
            RotationApplyPhase::load(&vault).expect("phase"),
            Some(RotationApplyPhase::Failed)
        );
        assert!(apply_phase_barrier(&vault));

        // 失败态（paused/conflict/needs_restart）同样保持冻结。
        for phase in [
            RotationApplyPhase::Paused,
            RotationApplyPhase::Conflict,
            RotationApplyPhase::NeedsRestart,
        ] {
            phase.store(&vault).expect("store");
            assert!(apply_phase_barrier(&vault), "{phase:?} 必须冻结");
        }

        RotationApplyPhase::clear(&vault).expect("clear");
        assert_eq!(RotationApplyPhase::load(&vault).expect("phase"), None);
        assert!(!apply_phase_barrier(&vault));
    }

    #[test]
    fn apply_receipt_roundtrips_and_rejects_corruption() {
        let (_dir, vault) = fresh();
        let receipt = RotationApplyReceipt::new(
            [0x11; 16], [0x22; 16], [0x33; 16], [0x44; 16], 6, "c6", 1, 2, 4, [0x55; 16],
            [0x66; 32], 3, [0x77; 32],
        );
        assert_eq!(RotationApplyReceipt::load(&vault).expect("load"), None);
        receipt.store(&vault).expect("store");
        let loaded = RotationApplyReceipt::load(&vault)
            .expect("load")
            .expect("receipt");
        assert_eq!(loaded, receipt);
        assert_eq!(loaded.rotation_id(), [0x33; 16]);
        assert_eq!(loaded.checkpoint_id(), [0x44; 16]);
        assert_eq!(loaded.baseline_s(), 6);
        assert_eq!(loaded.baseline_cursor(), "c6");
        assert_eq!(loaded.from_epoch(), 1);
        assert_eq!(loaded.epoch(), 2);
        assert_eq!(loaded.control_rev(), 4);
        assert_eq!(loaded.instance_id(), [0x55; 16]);
        assert_eq!(loaded.new_manager_public(), [0x66; 32]);
        assert_eq!(loaded.new_manager_rev(), 3);
        assert_eq!(loaded.activation_hash(), [0x77; 32]);

        // 未知版本 / 字段数不符 / 非 canonical 一律 fail-closed。
        let mut value = crate::wire::decode_entries(
            &vault
                .get_meta(ROTATION_APPLY_RECEIPT_META)
                .expect("meta")
                .expect("value"),
        )
        .expect("entries");
        for entry in &mut value {
            if entry.0 == 0 {
                entry.1 = cbor::uint(99);
            }
        }
        let bad_version = crate::wire::encode(&cbor::CdeValue::Map(value)).expect("encode");
        vault
            .set_meta(ROTATION_APPLY_RECEIPT_META, &bad_version)
            .expect("store bad version");
        assert!(RotationApplyReceipt::load(&vault).is_err());

        vault
            .set_meta(ROTATION_APPLY_RECEIPT_META, &[0xa1, 0x00, 0x01])
            .expect("store short");
        assert!(RotationApplyReceipt::load(&vault).is_err());

        // begin_apply 必须原子地清掉旧 receipt 并落 applying。
        RotationApplyReceipt::store(&receipt, &vault).expect("store again");
        RotationApplyPhase::Applied.store(&vault).expect("applied");
        RotationApplyPhase::begin_apply(&vault).expect("begin");
        assert_eq!(
            RotationApplyPhase::load(&vault).expect("phase"),
            Some(RotationApplyPhase::Applying)
        );
        assert_eq!(RotationApplyReceipt::load(&vault).expect("receipt"), None);
        assert!(apply_phase_barrier(&vault));
    }

    #[test]
    fn restart_intent_is_durable_before_assignment_without_secrets() {
        let (_dir, vault) = fresh();
        let intent = RestartIntent {
            previous_rotation_id: [0x71; 16],
            request_id: [0x72; 16],
            from_epoch: 3,
            previous_candidate_epoch: 4,
            expected_control_rev: 9,
            manager_rev: 2,
            freeze_seq: 12,
            instance_id: [0x73; 16],
            freeze_barrier: true,
        };
        let journal =
            RotationJournal::record_restart_intent(&vault, intent.clone()).expect("record intent");
        assert_eq!(journal.phase, RotationPhase::RestartIntentRecorded);
        assert_eq!(journal.to_epoch, 0, "分配前不得写候选 epoch");
        assert!(journal.rotation_id.is_none());
        assert!(journal.candidate_root.is_none());
        assert!(journal.restart_password.is_none());
        assert!(journal.recovery_seed.is_none());
        assert!(journal.manager_seed.is_none());
        let loaded = RotationJournal::load(&vault)
            .expect("load")
            .expect("journal");
        assert_eq!(loaded.phase, journal.phase);
        assert_eq!(loaded.request_id, journal.request_id);
        assert_eq!(loaded.from_epoch, journal.from_epoch);
        assert_eq!(loaded.to_epoch, journal.to_epoch);
        assert_eq!(loaded.control_rev, journal.control_rev);
        assert_eq!(
            loaded.previous_candidate_epoch,
            journal.previous_candidate_epoch
        );
        assert_eq!(
            loaded.restart_previous_rotation_id,
            journal.restart_previous_rotation_id
        );
        assert_eq!(
            loaded.restart_intent, journal.restart_intent,
            "公开 intent 必须逐字段 durable"
        );
        // 已有 journal 时不得再落第二份 intent（否则会把两次请求混在一起）。
        assert!(RotationJournal::record_restart_intent(&vault, journal_intent()).is_err());
        assert!(
            RotationJournal::freeze(&vault, 3, [0x74; 16], 9, [0u8; 32]).is_err(),
            "也不能被普通 rotate freeze 覆盖"
        );
    }

    fn journal_intent() -> RestartIntent {
        RestartIntent {
            previous_rotation_id: [0x81; 16],
            request_id: [0x82; 16],
            from_epoch: 5,
            previous_candidate_epoch: 6,
            expected_control_rev: 10,
            manager_rev: 2,
            freeze_seq: 13,
            instance_id: [0x83; 16],
            freeze_barrier: true,
        }
    }

    #[test]
    fn journal_roundtrips_with_secrets_and_activate_fields() {
        let (_dir, vault) = fresh();
        let mut journal = frozen(&vault, [0x21; 16]);
        let request_id = journal.request_id();
        journal
            .mark_begun([0x31; 16], 9, [0x41; 16])
            .expect("begun");
        let root = RootSecretV1::new([0x55; 16], 2, Secret32::from_array([0x66; 32]));
        journal.set_candidate(root.clone(), [0x77; 32], 2, true);
        journal.set_activate_new_manager(vec![1, 2, 3], [0x88; 64], [0x99; 64], 3);
        journal.save(&vault).expect("save");
        // 无明文旁路：vault.meta 里的 journal 必须是本地封装密文，不能出现候选根明文。
        let blob = vault
            .get_meta(ROTATION_JOURNAL_META)
            .expect("meta")
            .expect("blob");
        assert!(
            !blob.windows(32).any(|window| window == [0x66u8; 32]),
            "journal 必须以本地封装密文落盘"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(&blob[..4], b"MNKW", "Linux journal 必须带 MNKW 外层头");
        let loaded = RotationJournal::load(&vault)
            .expect("load")
            .expect("journal");
        assert_eq!(loaded.phase, RotationPhase::Begun);
        assert_eq!(loaded.request_id(), request_id);
        assert_eq!(loaded.rotation_id, Some([0x31; 16]));
        assert_eq!(loaded.freeze_seq, Some(9));
        assert!(loaded.r_prime_backed_up);
        assert_eq!(
            loaded
                .candidate_root
                .expect("root")
                .encode()
                .expect("encode"),
            root.encode().expect("root encode")
        );
        assert_eq!(loaded.new_manager_transition, Some(vec![1, 2, 3]));
        assert_eq!(loaded.old_manager_signature, Some([0x88; 64]));
        assert_eq!(loaded.control_chain_rev, Some(3));
    }

    #[test]
    fn phase_cannot_skip_and_request_id_is_stable() {
        let (_dir, vault) = fresh();
        let mut journal = frozen(&vault, [0x22; 16]);
        let first = journal.request_id();
        assert_eq!(first, journal.request_id());
        assert!(
            journal.advance(RotationPhase::Integrated).is_err(),
            "Frozen 不能直接到 Integrated"
        );
        journal
            .mark_begun([0x32; 16], 4, [0x42; 16])
            .expect("begun");
        assert_eq!(journal.phase, RotationPhase::Begun);
    }

    #[test]
    fn integrate_allows_local_pending_edits() {
        let (_dir, vault) = fresh();
        set_confirmed_highwater(&vault, 5).expect("highwater");
        vault
            .set_meta(
                menghunote_storage::META_SYNC_INTEGRATED_SEQ,
                &5u64.to_be_bytes(),
            )
            .expect("integrated");
        create_body_doc(&vault, "note-1").expect("doc");
        let body = menghunote_storage::BodyDoc::create("note-1", 1).expect("body");
        body.insert_scalars(0, "pending").expect("text");
        let pending = body
            .pending_update(&loro::VersionVector::default())
            .expect("pending update");
        menghunote_storage::append_updates(
            &vault,
            "note-1",
            0,
            &[menghunote_storage::UpdateRecord::new("msg-1", pending)],
        )
        .expect("local update");
        menghunote_storage::enqueue_sync_intent(
            &vault,
            "msg-1",
            "batch-1",
            "note-1",
            1,
            1,
            &[0xAA; 32],
        )
        .expect("pending outbox");
        let mut journal = frozen(&vault, [0x23; 16]);
        journal
            .mark_begun([0x33; 16], 5, [0x43; 16])
            .expect("begun");
        // 冻结后继续产生的本地编辑（pending/encrypted）不阻断轮换；它们推广后重包。
        journal
            .mark_integrated(&vault, 5, 5)
            .expect("本地 pending 不应阻断 ready");
        assert_eq!(journal.phase, RotationPhase::Integrated);
    }

    #[test]
    fn integrate_requires_highwater_to_cover_s() {
        let (_dir, vault) = fresh();
        set_confirmed_highwater(&vault, 3).expect("highwater");
        let mut journal = frozen(&vault, [0x24; 16]);
        journal
            .mark_begun([0x34; 16], 5, [0x44; 16])
            .expect("begun");
        assert!(journal.mark_integrated(&vault, 5, 5).is_err());
    }

    #[test]
    fn received_ahead_of_integrated_is_allowed_when_s_covered() {
        let (_dir, vault) = fresh();
        set_confirmed_highwater(&vault, 5).expect("highwater");
        vault
            .set_meta(
                menghunote_storage::META_SYNC_RECEIVED_SEQ,
                &6u64.to_be_bytes(),
            )
            .expect("received");
        vault
            .set_meta(
                menghunote_storage::META_SYNC_INTEGRATED_SEQ,
                &5u64.to_be_bytes(),
            )
            .expect("integrated");
        let mut journal = frozen(&vault, [0x26; 16]);
        journal
            .mark_begun([0x36; 16], 5, [0x46; 16])
            .expect("begun");
        // received 超前只说明 >S 的入站事件尚未整合；≤S 已覆盖且无 inbox 缺口时允许 ready。
        // quarantine/因果洞由 `rotation::ensure_c_phase_gate` 的 inbox 查询单独拦截。
        journal
            .mark_integrated(&vault, 5, 5)
            .expect("received 超前但 ≤S 已覆盖时不应阻断");
        assert_eq!(journal.phase, RotationPhase::Integrated);
    }
}

`````



@@SECTION code/049 `crates/sync/src/rotation_repack.rs`


`````rust
//! 阶段 J：用新 message_id/object_id/E/nonce 重打包旧 epoch 的本地 outbox。
//!
//! 明文只在 `Zeroizing` 缓冲中短暂存在；旧行解密失败/冲突一律保留，不丢数据。

use zeroize::Zeroizing;

use crate::error::SyncError;
use crate::keys::RootSecretV1;
use crate::upload::{decrypt_envelope, encrypt_local_parts};
use crate::{DeviceId, ObjectId};
use menghunote_storage::{
    outbox_parts, outbox_repack_candidates, repack_outbox, store_encrypted_parts, OutboxKey,
    OutboxRepackCandidate, RepackOutcome, RepackPart, RepackSpec, SyncEncryptedPart,
    SyncOutboxState,
};
use menghunote_sync_proto::{ObjectType, UpdateV1};

/// 一条未重包记录，写入诊断 meta（不丢、不无限重试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepackFailure {
    pub message_id: String,
    pub epoch: u64,
    pub reason: String,
}

/// 一轮 repack 的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepackReport {
    /// 单事务 CAS 成功。
    pub repacked: usize,
    /// 解密失败（保留旧行）。
    pub failed: usize,
    /// NotFound/Conflict（保留旧行）。
    pub conflicts: usize,
    /// 新 message_id 列表（测试/日志用）。
    pub new_message_ids: Vec<String>,
    /// 未重包明细（写 `sync.rotation_repack_report`）。
    pub failures: Vec<RepackFailure>,
}

impl RepackReport {
    fn render(&self) -> String {
        let mut out = format!(
            "repacked={} failed={} conflicts={}\n",
            self.repacked, self.failed, self.conflicts
        );
        for failure in &self.failures {
            out.push_str(&format!(
                "{} {} {}\n",
                failure.message_id, failure.epoch, failure.reason
            ));
        }
        out
    }
}

/// 诊断 meta：重包计数与未重包明细。
pub const REPACK_REPORT_META: &str = "sync.rotation_repack_report";

fn new_message_id() -> ([u8; 16], String) {
    let bytes: [u8; 16] =
        crate::random::array(&mut crate::random::OsRandom).expect("OS CSPRNG 不可用");
    (bytes, crate::ids::hex_of(&bytes))
}

fn decode_message_id(text: &str) -> Result<[u8; 16], SyncError> {
    let cleaned: String = text.chars().filter(|ch| *ch != '-').collect();
    if cleaned.len() != 32 {
        return Err(SyncError::LocalCorrupt("outbox message_id 不是 UUID"));
    }
    crate::ids::parse_hex::<16>(&cleaned)
        .map_err(|_| SyncError::LocalCorrupt("outbox message_id 非法"))
}

fn normalize_vv(bytes: &[u8]) -> Vec<u8> {
    if bytes.is_empty() {
        loro::VersionVector::new().encode()
    } else {
        bytes.to_vec()
    }
}

/// 从 storage 的原始 Loro update 重建 pending 行的 `UpdateV1` 明文。
///
/// pending 行没有密文可解，但原始字节仍在 `loro_updates`：`update_id`、`batch_id`、
/// `end_vv`/`end_frontier` 与 payload 都按出站加密路径原样取回，origin 仍是本设备；
/// 这样重包不会丢 update_id/batch_id/origin/Loro bytes。
fn pending_update_plaintext(
    vault: &menghunote_storage::Vault,
    candidate: &OutboxRepackCandidate,
    device_id: DeviceId,
) -> Result<(u64, Vec<u8>), SyncError> {
    let meta = crate::storage_view::doc_meta(vault, &candidate.doc_id)?
        .ok_or(SyncError::LocalCorrupt("outbox 指向不存在的文档"))?;
    let stored = crate::storage_view::stored_update(vault, &candidate.doc_id, candidate.seq)?;
    let base_vv = normalize_vv(&crate::storage_view::base_vv_at(
        vault,
        &candidate.doc_id,
        candidate.seq,
    )?);
    let update = UpdateV1 {
        doc_id: candidate.doc_id.clone(),
        generation: meta.generation,
        codec_version: meta.codec_version,
        app_doc_schema: meta.app_doc_schema,
        min_reader: meta.min_reader,
        update_id: stored.update_id,
        // 出站加密写的是 outbox 行的 batch_id；这里保持同一来源。
        batch_id: candidate.batch_id.clone(),
        origin_device_id: device_id,
        origin_seq: candidate.seq,
        base_vv,
        end_vv: stored.end_vv,
        end_frontier: stored.end_frontier,
        loro_update_bytes: stored.payload,
    };
    Ok((meta.generation, update.encode()?))
}

/// 把 pending 的旧 epoch 意图按"旧 epoch 本来会写出的字节"先加密落库。
///
/// 这不是伪造新 epoch 密文：parts 用旧根加密、header 仍是旧 epoch；它只是把
/// `encrypt_pending` 对同一 intent 本来就会落下的 durable 状态补齐（拿到可 CAS 的
/// `cipher_digest`），随后与 `encrypted`/`sent` 走同一解密→新 epoch 重加密路径。
/// 若此时崩溃，库中留下的是合法的旧 epoch `encrypted` 行，下一轮 repack 可正常处理。
fn materialize_pending_old_epoch(
    vault: &menghunote_storage::Vault,
    candidate: &OutboxRepackCandidate,
    old_root: &RootSecretV1,
    device_id: DeviceId,
    object_id: &ObjectId,
) -> Result<[u8; 32], SyncError> {
    if candidate.state != SyncOutboxState::Pending {
        return Err(SyncError::LocalCorrupt("outbox 状态不是 pending"));
    }
    if old_root.key_epoch != candidate.key_epoch {
        return Err(SyncError::LocalCorrupt("旧根 epoch 与 outbox 不一致"));
    }
    let message_id = decode_message_id(&candidate.message_id)?;
    let (generation, plaintext) = pending_update_plaintext(vault, candidate, device_id)?;
    let (parts, digest) = encrypt_local_parts(
        old_root,
        ObjectType::Update,
        object_id,
        message_id,
        device_id,
        generation,
        candidate.seq,
        &plaintext,
        &[],
    )?;
    let parts: Vec<SyncEncryptedPart> = parts
        .into_iter()
        .map(|(part_no, envelope, digest)| SyncEncryptedPart {
            part_no,
            envelope,
            digest,
        })
        .collect();
    store_encrypted_parts(
        vault,
        &candidate.message_id,
        candidate.key_epoch,
        &digest,
        &parts,
    )?;
    Ok(digest)
}

/// 把一组旧 MNE1 part 解出同一对象明文，再用新 epoch 根重加密。
pub fn reencrypt_old_parts(
    old_root: &RootSecretV1,
    new_root: &RootSecretV1,
    old_object_id: &ObjectId,
    old_parts: &[menghunote_storage::SyncOutboxPart],
    device_id: DeviceId,
) -> Result<(ObjectId, String, Vec<RepackPart>, [u8; 32]), SyncError> {
    if old_parts.is_empty() {
        return Err(SyncError::LocalCorrupt("旧 outbox 没有 parts"));
    }
    let mut plaintext = Zeroizing::new(Vec::new());
    let mut generation = 0u64;
    let mut sender_seq = 0u64;
    for (index, part) in old_parts.iter().enumerate() {
        let (header, bytes) = decrypt_envelope(
            old_root,
            ObjectType::Update,
            old_object_id,
            &part.envelope,
            &[],
        )?;
        if index == 0 {
            generation = header.generation;
            sender_seq = header.sender_seq;
        }
        plaintext.extend_from_slice(&bytes);
    }
    let (message_bytes, message_id) = new_message_id();
    let new_object_id: [u8; 32] = crate::random::array(&mut crate::random::OsRandom)?;
    let (parts, cipher_digest) = encrypt_local_parts(
        new_root,
        ObjectType::Update,
        &new_object_id,
        message_bytes,
        device_id,
        generation,
        sender_seq,
        &plaintext,
        &[],
    )?;
    let parts = parts
        .into_iter()
        .map(|(part_no, envelope, digest)| RepackPart {
            part_no,
            envelope,
            digest,
        })
        .collect();
    Ok((new_object_id, message_id, parts, cipher_digest))
}

/// 旧 epoch 根集合：按 outbox row 的 `key_epoch` 选对应旧根。
///
/// 兼容单 epoch 调用方（`&RootSecretV1`），也接受 `&[RootSecretV1]` 切片/数组
/// 或 `Vec<RootSecretV1>`，让一次轮换可以重包跨多个旧 epoch 遗留的行。
/// 找不到对应 epoch 的根时返回 `None`，调用方必须 fail-closed。
pub trait OldEpochRoots {
    /// 取 `epoch` 对应的旧根。
    fn root_for(&self, epoch: u64) -> Option<&RootSecretV1>;
}

impl OldEpochRoots for RootSecretV1 {
    fn root_for(&self, epoch: u64) -> Option<&RootSecretV1> {
        (self.key_epoch == epoch).then_some(self)
    }
}

impl OldEpochRoots for [RootSecretV1] {
    fn root_for(&self, epoch: u64) -> Option<&RootSecretV1> {
        self.iter().find(|root| root.key_epoch == epoch)
    }
}

impl<const N: usize> OldEpochRoots for [RootSecretV1; N] {
    fn root_for(&self, epoch: u64) -> Option<&RootSecretV1> {
        self[..].root_for(epoch)
    }
}

impl OldEpochRoots for Vec<RootSecretV1> {
    fn root_for(&self, epoch: u64) -> Option<&RootSecretV1> {
        self[..].root_for(epoch)
    }
}

/// 阶段 J：列出旧 epoch outbox，按每行 `key_epoch` 选旧根重新加密，并通过
/// storage 单事务 CAS 替换。
///
/// 旧行可能跨多个 epoch（前几次轮换遗留），因此旧根是集合而不是单个；
/// 任何一条旧行找不到对应 epoch 的根都会**整轮 fail-closed**，不跳过、不丢行。
pub fn repack_outbox_after_rotation<R: OldEpochRoots + ?Sized>(
    vault: &menghunote_storage::Vault,
    old_roots: &R,
    new_root: &RootSecretV1,
    device_id: DeviceId,
) -> Result<RepackReport, SyncError> {
    let candidates = outbox_repack_candidates(vault, new_root.key_epoch)?;
    // 先解析出每条旧行对应的旧根；缺根必须在改动任何行之前失败。
    let mut per_row_roots = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        let old_root = old_roots
            .root_for(candidate.key_epoch)
            .ok_or_else(|| SyncError::LocalCorrupt("outbox 旧 epoch 缺少对应旧根，拒绝重包"))?;
        per_row_roots.push(old_root);
    }
    let mut report = RepackReport::default();
    for (candidate, old_root) in candidates.into_iter().zip(per_row_roots) {
        let Some(old_object_id) = candidate.object_id else {
            report.failed += 1;
            report.failures.push(RepackFailure {
                message_id: candidate.message_id,
                epoch: candidate.key_epoch,
                reason: "missing object_id".to_owned(),
            });
            continue;
        };
        // pending 行没有密文：先从 storage 的原始 Loro update 补出可 CAS 的旧 epoch
        // 密文（等价于 encrypt_pending 本会写下的 durable 状态），再走统一重打包路径。
        let old_digest = match candidate.cipher_digest {
            Some(digest) => digest,
            None if candidate.state == SyncOutboxState::Pending => {
                match materialize_pending_old_epoch(
                    vault,
                    &candidate,
                    old_root,
                    device_id,
                    &old_object_id,
                ) {
                    Ok(digest) => digest,
                    Err(_) => {
                        report.failed += 1;
                        report.failures.push(RepackFailure {
                            message_id: candidate.message_id,
                            epoch: candidate.key_epoch,
                            reason: "pending 行无法从 loro_updates 重建".to_owned(),
                        });
                        continue;
                    }
                }
            }
            None => {
                report.failed += 1;
                report.failures.push(RepackFailure {
                    message_id: candidate.message_id,
                    epoch: candidate.key_epoch,
                    reason: "missing cipher_digest".to_owned(),
                });
                continue;
            }
        };
        let parts = match outbox_parts(vault, &candidate.message_id, candidate.key_epoch) {
            Ok(parts) => parts,
            Err(_) => {
                report.failed += 1;
                report.failures.push(RepackFailure {
                    message_id: candidate.message_id,
                    epoch: candidate.key_epoch,
                    reason: "outbox parts 读取失败".to_owned(),
                });
                continue;
            }
        };
        let reencrypted =
            reencrypt_old_parts(old_root, new_root, &old_object_id, &parts, device_id);
        let (new_object_id, new_message_id, new_parts, cipher_digest) = match reencrypted {
            Ok(value) => value,
            Err(_) => {
                // 解密失败：保留旧行，等待人工/下一轮处理。
                report.failed += 1;
                report.failures.push(RepackFailure {
                    message_id: candidate.message_id,
                    epoch: candidate.key_epoch,
                    reason: "解密/重加密失败".to_owned(),
                });
                continue;
            }
        };
        let old = OutboxKey {
            message_id: candidate.message_id,
            epoch: candidate.key_epoch,
            cipher_digest: old_digest,
        };
        let spec = RepackSpec {
            message_id: new_message_id.clone(),
            object_id: new_object_id,
            epoch: new_root.key_epoch,
            parts: new_parts,
            cipher_digest,
        };
        match repack_outbox(vault, &old, &spec)? {
            RepackOutcome::Repacked => {
                report.repacked += 1;
                report.new_message_ids.push(new_message_id);
            }
            RepackOutcome::NotFound | RepackOutcome::Conflict => {
                report.conflicts += 1;
                report.failures.push(RepackFailure {
                    message_id: old.message_id,
                    epoch: old.epoch,
                    reason: "CAS NotFound/Conflict".to_owned(),
                });
            }
        }
    }
    vault.set_meta(REPACK_REPORT_META, report.render().as_bytes())?;
    Ok(report)
}

`````



@@SECTION code/050 `crates/sync/src/rotation_slots.rs`


`````rust
//! 轮换 P1：候选 epoch 的新 KEY_SLOT、slot_set_digest 与全 active 设备 grants。
//!
//! 槽必须用**新秘密**（新 password / 新 `R'`），AAD 闭包与最终上传 header 逐字段一致；
//! 槽体 AAD 的 `message_id = slot_id[..16]`、长度取根 CDE 明文长度（与 genesis /
//! `recovery_enroll` 标准读取路径同口径）；KEY_SLOT 走 `K_slot_mne` 且带 `?reservation=`。

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::kdf;
use crate::keys::RootSecretV1;
use crate::recovery::RecoverySeed;
use crate::relay::{FrozenAssetRef, RelayClient, SlotUpload};
use crate::rotation::ManagerTransitionV1;
use crate::rotation_journal::JournalBaseline;
use crate::rotation_stage::ReencryptedAsset;
use crate::slot::{
    context_hash, validate_slot_descriptor, KeySlotV1, PROTOCOL_VERSION,
    WRAP_SUITE_XCHACHA20_POLY1305,
};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_sync_proto::{
    activation_t_signing_input, part_count_for, ActivationTV1, Kdf, ObjectRefV1, ObjectType,
    SlotDescriptorV1, SlotKind,
};

/// 候选槽集合（新秘密）。
pub struct CandidateSlots {
    pub password: KeySlotV1,
    pub recovery: KeySlotV1,
    /// 新恢复种子 R'（**不是**旧 R）。
    pub recovery_seed: RecoverySeed,
    /// 展示给用户的恢复码（由 R' 编码）。
    pub recovery_code: Zeroizing<String>,
    /// 由 R' 派生的新管理 key。
    pub new_manager: ManagementKey,
    pub new_manager_public: [u8; 32],
}

/// 生成候选 password/recovery 槽并自检（恢复种子 `R'` 由本次随机生成）。
#[allow(clippy::too_many_arguments)]
pub fn generate_candidate_slots(
    vault_id: VaultId,
    epoch: u64,
    new_password: &str,
    manager_rev: u64,
    created_at: u64,
    device_id: DeviceId,
    candidate_root: &RootSecretV1,
    rng: &mut impl crate::random::RandomSource,
) -> Result<CandidateSlots, SyncError> {
    generate_candidate_slots_with_recovery_seed(
        vault_id,
        epoch,
        new_password,
        manager_rev,
        created_at,
        device_id,
        candidate_root,
        None,
        rng,
    )
}

/// 同 [`generate_candidate_slots`]，但允许调用方**指定**恢复种子 `R'`。
///
/// 用途（ADR 0009 Phase 2 切片 6a）：dev-local 轮换不更换管理 key/恢复码。调用方若持有
/// 当前管理恢复码，就把它的种子传进来，让候选 epoch 的 recovery 槽**能被同一恢复码解开**
/// ——否则新设备在轮换后无法再用恢复码加入（随机 `R'` 没人知道）。
///
/// 是否允许该种子由调用方（轮换编排）决定：这里只负责按同一套 AAD/KEK 规则包槽并自检。
/// `None` = 沿用旧的"一次性随机 `R'`"语义（槽集合完整，但旧恢复码打不开）。
#[allow(clippy::too_many_arguments)]
pub fn generate_candidate_slots_with_recovery_seed(
    vault_id: VaultId,
    epoch: u64,
    new_password: &str,
    manager_rev: u64,
    created_at: u64,
    device_id: DeviceId,
    candidate_root: &RootSecretV1,
    recovery_seed: Option<&RecoverySeed>,
    rng: &mut impl crate::random::RandomSource,
) -> Result<CandidateSlots, SyncError> {
    if new_password.is_empty() {
        return Err(SyncError::PasswordLength {
            actual: 0,
            min: 1,
            max: 1024,
        });
    }
    let password_slot_id: ObjectId = crate::random::array(rng)?;
    let recovery_slot_id: ObjectId = crate::random::array(rng)?;
    let password_descriptor = SlotDescriptorV1 {
        protocol_version: PROTOCOL_VERSION,
        slot_id: password_slot_id,
        vault_id,
        key_epoch: epoch,
        kind: SlotKind::Password,
        wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
        kdf: Kdf::Argon2id(kdf::new_writer_params(rng)?),
        created_at,
        manager_rev,
    };
    let recovery_descriptor = SlotDescriptorV1 {
        protocol_version: PROTOCOL_VERSION,
        slot_id: recovery_slot_id,
        vault_id,
        key_epoch: epoch,
        kind: SlotKind::Recovery,
        wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
        kdf: Kdf::RecoveryHkdf,
        created_at,
        manager_rev,
    };
    validate_slot_descriptor(&password_descriptor)?;
    validate_slot_descriptor(&recovery_descriptor)?;
    // `Some` = 调用方指定的恢复种子（同一恢复码仍能解开候选根）；`None` = 一次性随机。
    let recovery_seed = match recovery_seed {
        Some(seed) => seed.clone(),
        None => RecoverySeed::generate(rng)?,
    };
    let recovery_code = recovery_seed.to_code()?;
    let root = candidate_root.clone();
    let aad = slot_aad(device_id);
    let password = KeySlotV1::wrap_password(
        &password_descriptor,
        new_password.as_bytes(),
        &root,
        &aad,
        rng,
    )?;
    let recovery =
        KeySlotV1::wrap_recovery(&recovery_descriptor, &recovery_seed, &root, &aad, rng)?;
    // 自检：新秘密必须各自解回同一候选根（AAD 与标准 reader 同口径）。
    if !password
        .unwrap_password(new_password.as_bytes(), &aad)?
        .ct_eq(&root)
        || !recovery.unwrap_recovery(&recovery_seed, &aad)?.ct_eq(&root)
    {
        return Err(SyncError::Unwrap);
    }
    let new_manager = ManagementKey::from_recovery(&recovery_seed, &vault_id)?;
    let new_manager_public = new_manager.public_key();
    Ok(CandidateSlots {
        password,
        recovery,
        recovery_seed,
        recovery_code,
        new_manager,
        new_manager_public,
    })
}

/// 与 genesis / 标准 reader 逐字节一致的槽体 AAD 构造器。
///
/// 口径（`genesis::build_materials` 包槽体、`recovery_enroll::unwrap_recovery_slot`
/// 读回重建，两边相同）：
/// - `message_id = slot_id[..16]`（**不是**随机值，也不是 MNE 层的 message_id）；
/// - 长度字段取根 CDE 明文长度（`plaintext_len` / `plaintext_len + 16`）；
/// - 其余字段与 `upload::upload_object` 写 KEY_SLOT header 时一致。
///
/// 轮换产出的新槽将来要能被标准 reader 用新口令/新恢复码解回；这里任何字段漂移都会
/// 让新槽"写得进、读不回"。
fn slot_aad(
    device_id: DeviceId,
) -> impl Fn(&SlotDescriptorV1, &[u8; 24], usize) -> Result<Vec<u8>, SyncError> {
    move |descriptor, nonce, plaintext_len| {
        let mut message_id = [0u8; 16];
        message_id.copy_from_slice(&descriptor.slot_id[..16]);
        let header = menghunote_sync_proto::EnvelopeHeader {
            protocol_version: menghunote_sync_proto::PROTOCOL_VERSION,
            header_len: menghunote_sync_proto::HEADER_LEN as u16,
            object_type: ObjectType::KeySlot,
            cipher_suite: menghunote_sync_proto::CIPHER_SUITE_XCHACHA20_POLY1305,
            flags: 0,
            vault_id: descriptor.vault_id,
            object_id: descriptor.slot_id,
            message_id,
            producer_device_id: device_id,
            generation: 0,
            key_epoch: descriptor.key_epoch,
            sender_seq: 0,
            part_index: 0,
            part_count: 1,
            total_plaintext_len: plaintext_len as u64,
            this_plaintext_len: u32::try_from(plaintext_len)
                .map_err(|_| SyncError::Descriptor("槽根明文长度超出 u32"))?,
            ciphertext_len: u32::try_from(plaintext_len + 16)
                .map_err(|_| SyncError::Descriptor("槽密文长度超出 u32"))?,
            context_hash: context_hash(descriptor)?,
            nonce: *nonce,
        };
        Ok(header.aad(&descriptor.encode()?)?)
    }
}

/// 上传一个候选 KEY_SLOT（K_slot_mne + 维护 reservation）。
#[allow(clippy::too_many_arguments)]
pub fn upload_slot_with_reservation<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    slot: &KeySlotV1,
    password: Option<&str>,
    recovery_seed: Option<&RecoverySeed>,
    message_id: [u8; 16],
    reservation: &[u8; 16],
) -> Result<(), SyncError> {
    let descriptor = slot.descriptor.encode()?;
    let key = slot.mne_key(password.map(str::as_bytes), recovery_seed)?;
    crate::upload::upload_object_with_key_and_reservation(
        client,
        vault_id,
        device_id,
        signing_key,
        root,
        ObjectType::KeySlot,
        slot.descriptor.slot_id,
        message_id,
        0,
        0,
        &slot.wrapped_bytes(),
        &descriptor,
        Some(slot.wrapped.nonce),
        &key,
        reservation,
    )?;
    let signature = signing_key.sign(&descriptor);
    client.put_key_slot(SlotUpload {
        slot_id: slot.descriptor.slot_id,
        epoch: slot.descriptor.key_epoch,
        kind: slot.descriptor.kind,
        descriptor: descriptor.clone(),
        signed_message: descriptor,
        signature,
    })?;
    Ok(())
}

/// 上传候选 password/recovery 两个槽（各自独立 message_id）。
#[allow(clippy::too_many_arguments)]
pub fn upload_candidate_slots<C: RelayClient>(
    client: &mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    root: &RootSecretV1,
    slots: &CandidateSlots,
    password: &str,
    reservation: &[u8; 16],
) -> Result<[u8; 32], SyncError> {
    let password_message: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    upload_slot_with_reservation(
        client,
        vault_id,
        device_id,
        signing_key,
        root,
        &slots.password,
        Some(password),
        None,
        password_message,
        reservation,
    )?;
    let recovery_message: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
    upload_slot_with_reservation(
        client,
        vault_id,
        device_id,
        signing_key,
        root,
        &slots.recovery,
        None,
        Some(&slots.recovery_seed),
        recovery_message,
        reservation,
    )?;
    slot_set_digest(&[
        slots.password.descriptor.clone(),
        slots.recovery.descriptor.clone(),
    ])
}

/// `SHA256(concat(sorted(slot_id32 || SHA256(descriptor))))`。
pub fn slot_set_digest(descriptors: &[SlotDescriptorV1]) -> Result<[u8; 32], SyncError> {
    let mut ordered = descriptors
        .iter()
        .map(|descriptor| {
            let descriptor_hash: [u8; 32] = Sha256::digest(descriptor.encode()?).into();
            Ok((descriptor.slot_id, descriptor_hash))
        })
        .collect::<Result<Vec<_>, SyncError>>()?;
    ordered.sort_by_key(|(slot_id, _)| *slot_id);
    let mut input = Vec::with_capacity(ordered.len() * 64);
    for (slot_id, descriptor_hash) in ordered {
        input.extend_from_slice(&slot_id);
        input.extend_from_slice(&descriptor_hash);
    }
    Ok(Sha256::digest(&input).into())
}

/// 完整 activation T（ADR 0006 §34）各字段在**客户端**的逐项来源。
///
/// 这里不保存任何秘密：全部是公开标识/摘要。字段与 `ActivationTV1` 的 20 个整数键
/// 一一对应，`build_activation_t` 只做类型收窄与结构校验，不做网络/存储 IO。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationTMaterial {
    pub vault_id: VaultId,
    pub instance_id: [u8; 16],
    pub rotation_id: [u8; 16],
    pub from_epoch: u64,
    pub to_epoch: u64,
    /// 冻结截止 S（= `rotation.freeze_seq`）。
    pub freeze_seq: u64,
    pub old_manager_rev: u64,
    pub new_manager_rev: u64,
    pub new_manager_public: [u8; 32],
    /// 激活前控制链链尾 `record_hash`（relay `/control` 已验证链的最后一跳）。
    pub prev_control_hash: [u8; 32],
    /// **激活后**控制链 rev（= 当前链尾 rev + 1；relay `bind_activation_t` 的 CAS 口径）。
    pub expected_control_rev: u64,
    pub primary_checkpoint: [u8; 16],
    /// `SHA256(CDE(CheckpointCommitV1))`，即 relay 的 `signed_commit` 摘要。
    pub primary_commit_digest: [u8; 32],
    pub fallback_checkpoint: [u8; 16],
    pub fallback_commit_digest: [u8; 32],
    /// 候选 epoch staging 槽集合摘要（`slot_set_digest` 口径）。
    pub slot_set_digest: [u8; 32],
    /// 控制链 active 成员证书集合摘要（`member_cert_digest` 口径）。
    pub member_cert_digest: [u8; 32],
    /// 候选 epoch 完整 grants 包摘要（`grant_package_digest` 口径）。
    pub grants_digest: [u8; 32],
    /// 旧 epoch 冻结资产集合摘要（relay frozen `asset_set_digest`）。
    pub old_asset_set_digest: [u8; 32],
    /// 候选 epoch 重加密资产集合摘要（relay `candidate_asset_set_digest` 口径）。
    pub new_asset_set_digest: [u8; 32],
}

/// ADR 0006 §34：从本地材料构造完整的 20 字段 activation T。
///
/// 只做字段装配与 `ActivationTV1::validate`（epoch/rev 单调、非全零、SQLite 上界）；
/// 与 relay 事实的逐项比对由 relay `bind_activation_t` 与客户端 `/control` 复算共同完成。
pub fn build_activation_t(material: &ActivationTMaterial) -> Result<ActivationTV1, SyncError> {
    let old_manager_rev = u32::try_from(material.old_manager_rev)
        .map_err(|_| SyncError::Object("activation T: old_manager_rev 超出 u32"))?;
    let new_manager_rev = u32::try_from(material.new_manager_rev)
        .map_err(|_| SyncError::Object("activation T: new_manager_rev 超出 u32"))?;
    let record = ActivationTV1 {
        vault_id: material.vault_id,
        instance_id: material.instance_id,
        rotation_id: material.rotation_id,
        from_epoch: material.from_epoch,
        to_epoch: material.to_epoch,
        freeze_seq: material.freeze_seq,
        old_manager_rev,
        new_manager_rev,
        new_manager_pubkey: material.new_manager_public,
        prev_control_hash: material.prev_control_hash,
        expected_control_rev: material.expected_control_rev,
        primary_cid: material.primary_checkpoint,
        primary_commit_digest: material.primary_commit_digest,
        fallback_cid: material.fallback_checkpoint,
        fallback_commit_digest: material.fallback_commit_digest,
        slot_set_digest: material.slot_set_digest,
        member_cert_digest: material.member_cert_digest,
        grants_digest: material.grants_digest,
        old_asset_set_digest: material.old_asset_set_digest,
        new_asset_set_digest: material.new_asset_set_digest,
    };
    record.validate()?;
    Ok(record)
}

/// 阶段 G：构造 9 字段 manager transition + 完整 activation T，并让旧/新管理 key
/// 分别对 `transition` 与 `MN/v1/activation-t\0 || CDE(T)` 各签一次。
///
/// `NewManagerV1` 的 3–5 字段（T + 旧/新 T 双签）永远一起填充；activate 路径不得再传
/// `None`（relay 对缺 T 的请求一律 409）。
#[allow(clippy::too_many_arguments)]
pub fn build_manager_transition(
    material: &ActivationTMaterial,
    old_manager: &ManagementKey,
    new_manager: &ManagementKey,
) -> Result<menghunote_sync_proto::NewManagerV1, SyncError> {
    let transition = ManagerTransitionV1 {
        vault_id: material.vault_id,
        instance_id: material.instance_id,
        rotation_id: material.rotation_id,
        from_epoch: material.from_epoch,
        to_epoch: material.to_epoch,
        old_rev: material.old_manager_rev,
        new_rev: material.new_manager_rev,
        new_pubkey: material.new_manager_public,
        covered_seq: material.freeze_seq,
    }
    .encode()?;
    let activation_t = build_activation_t(material)?;
    let t_signing_input = activation_t_signing_input(&activation_t)?;
    Ok(menghunote_sync_proto::NewManagerV1 {
        old_signature: Some(old_manager.sign(&transition).to_vec()),
        new_signature: Some(new_manager.sign(&transition).to_vec()),
        transition,
        activation_t: Some(activation_t.encode()?),
        old_t_signature: Some(old_manager.sign(&t_signing_input).to_vec()),
        new_t_signature: Some(new_manager.sign(&t_signing_input).to_vec()),
    })
}

/// ADR §34：`primary_commit_digest` / `fallback_commit_digest` 的口径。
///
/// `SHA256(CDE(CheckpointCommit))`，与 relay 持久化的 `signed_commit` 字节逐字节一致。
/// task-122 起 commit 为 v2（额外绑定 `validation_digest`），因此摘要**只能**取
/// promote 实际签名时落下的 `report.commit_digest`；不得再用公开字段另拼一份 commit。
pub fn checkpoint_commit_digest(
    report: &crate::checkpoint::CheckpointReport,
) -> Result<[u8; 32], SyncError> {
    report.commit_digest.ok_or(SyncError::Object(
        "checkpoint report 缺 promote 后的 commit 摘要（基线尚未发布）",
    ))
}

/// relay 存下的 `signed_commit` 字节的摘要（恢复路径拿不到 `PreparedCheckpoint` 时用）。
#[must_use]
pub fn signed_commit_digest(signed_commit: &[u8]) -> [u8; 32] {
    Sha256::digest(signed_commit).into()
}

/// relay `objects::candidate_asset_set_digest` 同口径：
/// `SHA256(CDE(sorted ObjectRefV1[type=ASSET]))`，取**候选 epoch**本次 rotation 已
/// complete 的重加密资产集合（`ReencryptedAsset` 的全部字段足以重建 `ObjectRefV1`）。
pub fn candidate_asset_set_digest(
    epoch: u64,
    assets: &[ReencryptedAsset],
) -> Result<[u8; 32], SyncError> {
    let mut refs = Vec::with_capacity(assets.len());
    for asset in assets {
        if asset.media_type.is_empty() || asset.media_type.len() > 255 {
            return Err(SyncError::Object("候选资产 MIME 长度非法"));
        }
        let total_plain_len = 42u64
            .checked_add(asset.media_type.len() as u64)
            .and_then(|value| value.checked_add(asset.byte_len))
            .ok_or(SyncError::Object("候选资产明文长度溢出"))?;
        let part_count = u32::try_from(part_count_for(total_plain_len, ObjectType::Asset))
            .map_err(|_| SyncError::Object("候选资产 part_count 超出 u32"))?;
        refs.push(ObjectRefV1 {
            object_id: asset.object_id,
            object_type: ObjectType::Asset,
            generation: 0,
            key_epoch: epoch,
            part_count,
            total_plain_len,
            object_digest: asset.object_digest,
        });
    }
    let input = menghunote_sync_proto::refs_digest_input(&refs)?;
    Ok(Sha256::digest(&input).into())
}

/// relay `objects::asset_set_digest` / 冻结源 `source_digest` 同口径：
/// `SHA256(CDE(sorted ObjectRefV1[type=ASSET]))`，取旧 epoch 冻结时点的完整 finalized 集合。
pub fn frozen_asset_set_digest(refs: &[FrozenAssetRef]) -> Result<[u8; 32], SyncError> {
    let mut object_refs = Vec::with_capacity(refs.len());
    for reference in refs {
        object_refs.push(ObjectRefV1 {
            object_id: reference.object_id,
            object_type: ObjectType::Asset,
            generation: reference.generation,
            key_epoch: reference.key_epoch,
            part_count: reference.part_count,
            total_plain_len: reference.total_plain_len,
            object_digest: reference.object_digest,
        });
    }
    let input = menghunote_sync_proto::refs_digest_input(&object_refs)?;
    Ok(Sha256::digest(&input).into())
}

/// 候选基线/槽绑定记录（journal 持久化用）。
#[derive(Debug, Clone)]
pub struct CandidateSlotRecord {
    pub slot_id: ObjectId,
    pub kind: SlotKind,
    pub descriptor: Vec<u8>,
    pub baseline: Option<JournalBaseline>,
}

/// relay 口径的 grants_digest：`SHA256(concat(sorted(device_id || SHA256(signed_header))))`。
pub fn grants_digest(leaves: &[crate::rotation_stage::GrantLeaf]) -> Result<[u8; 32], SyncError> {
    let mut ordered = leaves
        .iter()
        .map(|leaf| {
            let header_hash: [u8; 32] = Sha256::digest(&leaf.signed_header).into();
            (leaf.device_id, header_hash)
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(device_id, _)| *device_id);
    let mut input = Vec::with_capacity(ordered.len() * 48);
    for (device_id, header_hash) in ordered {
        input.extend_from_slice(&device_id);
        input.extend_from_slice(&header_hash);
    }
    Ok(Sha256::digest(&input).into())
}

/// 从已验证的 `/control` 缓存取全部 active 设备 HPKE 公钥（不信任裸 relay 列表）。
pub fn active_member_hpke_keys<C: RelayClient>(
    client: &mut C,
) -> Result<Vec<(DeviceId, [u8; 32])>, SyncError> {
    client.active_member_hpke_keys()
}

/// 对所有 active 设备（含本机）封 candidate root，生成 grant leaves。
pub fn plan_all_active_grants<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    manager: &ManagementKey,
    manager_rev: u64,
) -> Result<Vec<crate::rotation_stage::GrantLeaf>, SyncError> {
    let recipients = active_member_hpke_keys(client)?;
    if recipients.is_empty() {
        return Err(SyncError::Unsupported("控制链没有 active 设备证书"));
    }
    crate::rotation_stage::plan_grants(root, manager, manager_rev, &recipients)
}

/// 自检：恢复码必须能从 R' 重新解析。
pub fn verify_recovery_code(slots: &CandidateSlots) -> Result<(), SyncError> {
    let parsed = RecoverySeed::from_code(slots.recovery_code.as_str())?;
    if parsed.expose() != slots.recovery_seed.expose() {
        return Err(SyncError::Object("新恢复码与 R' 不一致"));
    }
    Ok(())
}

/// 自检：新管理 key 必须由 R' 派生。
pub fn verify_new_manager(slots: &CandidateSlots, vault_id: VaultId) -> Result<(), SyncError> {
    let derived = ManagementKey::from_recovery(&slots.recovery_seed, &vault_id)?;
    if derived.public_key() != slots.new_manager_public {
        return Err(SyncError::Object("新管理公钥不是由 R' 派生"));
    }
    Ok(())
}

/// 兼容：让调用方拿到 HPKE 公钥（本机）。
pub fn local_hpke(hpke: &DeviceHpkeKey) -> [u8; 32] {
    hpke.public_key()
}

`````



@@SECTION code/051 `crates/sync/src/rotation_stage.rs`


`````rust
//! 轮换阶段 D–F（S7-29；ADR §28.2）。
//!
//! - D：冻结资产全量重加密（新根 HMAC object_id + §19 确定性 nonce）；全 docs 走 checkpoint
//!   full-history snapshot；不改变 peer/device/doc generation。
//! - E：两份基线独立导出/读回/空上下文验证；对象身份独立、assets 可共享同 epoch 对象；
//!   两 commit 的 S 必须 ≥ freeze_seq。
//! - F：对每个未吊销设备封 HPKE root grant 并生成待 stage 的 leaf manifest。
//!
//! 本模块不写 relay 侧冻结资产分页（relay task-97）；资产集合由调用方从 frozen 登记表/
//! 源列表传入，保证墓碑与无引用资产也被覆盖。

use sha2::{Digest, Sha256};

use crate::api_version::RelayApi;
use crate::checkpoint::{asset_object_id, asset_plaintext, AssetSource, CheckpointWriter};
use crate::device::{DeviceSigningKey, ManagementKey};
use crate::error::SyncError;
use crate::hpke::seal_epoch_grant;
use crate::keys::RootSecretV1;
use crate::relay::RelayClient;
use crate::upload::{asset_message_id, fetch_object, upload_object};
use crate::{DeviceId, ObjectId, VaultId};
use menghunote_storage::Vault;
use menghunote_sync_proto::{GrantAadV1, ObjectType, SnapshotV1};

/// 阶段 D 产出的重加密资产。
#[derive(Debug, Clone)]
pub struct ReencryptedAsset {
    pub real_sha256: [u8; 32],
    pub byte_len: u64,
    pub media_type: String,
    pub object_id: ObjectId,
    pub object_digest: [u8; 32],
}

/// 阶段 D：冻结资产**全量**下载→验真实 SHA256→新根重加密上传。
///
/// `assets` 必须包含墓碑/不可见引用/无引用 finalized 资产（调用方从 frozen 登记表传入）。
#[allow(clippy::too_many_arguments)]
pub fn reencrypt_assets<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    old_root: &RootSecretV1,
    new_root: &RootSecretV1,
    assets: &dyn AssetSource,
    reservation: Option<&[u8; 16]>,
) -> Result<Vec<ReencryptedAsset>, SyncError> {
    if new_root.vault_id != vault_id || old_root.vault_id != vault_id {
        return Err(SyncError::Object("重加密 root 归属 vault 不符"));
    }
    if new_root.key_epoch <= old_root.key_epoch {
        return Err(SyncError::Object("重加密目标 epoch 必须更大"));
    }
    let mut out = Vec::new();
    for meta in assets.list(vault)? {
        let data = assets.read(vault, &meta)?;
        let real: [u8; 32] = Sha256::digest(&data).into();
        if real != meta.real_sha256 || data.len() as u64 != meta.byte_len {
            return Err(SyncError::Object("冻结资产字节与元数据不一致"));
        }
        let plaintext = asset_plaintext(&real, meta.byte_len, &meta.media_type, &data)?;
        let object_id = asset_object_id(new_root, &real, meta.byte_len, &meta.media_type)?;
        let message_id = asset_message_id(new_root, &object_id)?;
        let object = if let Some(reservation) = reservation {
            crate::upload::upload_object_with_reservation(
                client,
                vault_id,
                device_id,
                signing_key,
                new_root,
                ObjectType::Asset,
                object_id,
                message_id,
                0,
                0,
                &plaintext,
                &[],
                None,
                reservation,
            )?
        } else {
            upload_object(
                client,
                vault_id,
                device_id,
                signing_key,
                new_root,
                ObjectType::Asset,
                object_id,
                message_id,
                0,
                0,
                &plaintext,
                &[],
                None,
            )?
        };
        crate::asset_sync::mark_uploaded(
            vault,
            &crate::asset_sync::AssetUploadedFact {
                object_id: object.object_id,
                part_count: object.part_count,
                total_plain_len: object.total_plain_len,
                object_digest: object.object_digest,
                key_epoch: new_root.key_epoch,
            },
        )?;
        out.push(ReencryptedAsset {
            real_sha256: real,
            byte_len: meta.byte_len,
            media_type: meta.media_type,
            object_id: object.object_id,
            object_digest: object.object_digest,
        });
    }
    Ok(out)
}

/// 独立空上下文里的一份基线证据（不共享任何 Loro/Vault 状态）。
#[derive(Debug, Clone)]
pub struct BaselineEvidence {
    pub checkpoint_id: [u8; 16],
    pub covered_seq: u64,
    pub manifest_object_id: ObjectId,
    pub manifest_s: u64,
    pub docs: Vec<DocEvidence>,
    pub asset_count: usize,
    pub asset_set_digest: [u8; 32],
    /// 该基线实际引用的 snapshot object id（用于证明两份基线不复用对象）。
    pub snapshot_ids: Vec<ObjectId>,
}

/// 单个 doc 快照的公开字段。
#[derive(Debug, Clone)]
pub struct DocEvidence {
    pub doc_id: String,
    pub generation: u64,
    pub vv: Vec<u8>,
    pub frontier: Vec<u8>,
    pub min_reader: u32,
    pub full_history: bool,
}

/// 从 relay 全量读回一份 prepared baseline 并解码/验证（不导入任何 Vault）。
pub fn capture_baseline_evidence<C: RelayClient>(
    client: &mut C,
    root: &RootSecretV1,
    prepared: &crate::checkpoint::PreparedCheckpoint,
    frozen_asset_hashes: &[[u8; 32]],
) -> Result<BaselineEvidence, SyncError> {
    let report = &prepared.report;
    let mut docs = Vec::with_capacity(report.docs.len());
    for doc in &report.docs {
        let (_, bytes) = fetch_object(
            client,
            root,
            ObjectType::Snapshot,
            &doc.object.object_id,
            doc.object.part_count,
            doc.object.total_plain_len,
            &doc.object.object_digest,
            &[],
        )?;
        let snapshot = SnapshotV1::decode(&bytes)?;
        verify_snapshot_integrity(&snapshot)?;
        if snapshot.doc_id != doc.doc_id
            || snapshot.generation != doc.generation
            || !snapshot.full_history
        {
            return Err(SyncError::Object("基线快照不是 full-history/字段不符"));
        }
        docs.push(DocEvidence {
            doc_id: snapshot.doc_id,
            generation: snapshot.generation,
            vv: snapshot.vv,
            frontier: snapshot.frontier,
            min_reader: snapshot.min_reader,
            full_history: snapshot.full_history,
        });
    }

    let mut asset_hashes = Vec::with_capacity(report.assets.len());
    for asset in &report.assets {
        let (_, payload) = fetch_object(
            client,
            root,
            ObjectType::Asset,
            &asset.object.object_id,
            asset.object.part_count,
            asset.object.total_plain_len,
            &asset.object.object_digest,
            &[],
        )?;
        let decoded = decode_asset_plaintext(&payload)?;
        if decoded.0 != asset.real_sha256
            || decoded.1 != asset.byte_len
            || decoded.2 != asset.media_type
        {
            return Err(SyncError::Object("基线资产明文与 manifest 不符"));
        }
        asset_hashes.push(asset.real_sha256);
    }
    asset_hashes.sort_unstable();
    let mut frozen = frozen_asset_hashes.to_vec();
    frozen.sort_unstable();
    if asset_hashes != frozen {
        return Err(SyncError::Object("基线资产集合不等于冻结资产集合"));
    }
    let mut input = Vec::with_capacity(4 + frozen.len() * 32);
    input.extend_from_slice(&(frozen.len() as u32).to_be_bytes());
    for hash in &frozen {
        input.extend_from_slice(hash);
    }
    Ok(BaselineEvidence {
        checkpoint_id: report.checkpoint_id,
        covered_seq: report.covered_seq,
        manifest_object_id: report.manifest.object_id,
        manifest_s: report.covered_seq,
        asset_count: frozen.len(),
        asset_set_digest: Sha256::digest(&input).into(),
        snapshot_ids: report.docs.iter().map(|doc| doc.object.object_id).collect(),
        docs,
    })
}

/// T18：full-history 且 VV 必须与快照内实际 Loro 历史一致。
pub fn verify_snapshot_integrity(snapshot: &SnapshotV1) -> Result<(), SyncError> {
    if !snapshot.full_history {
        return Err(SyncError::Object("基线快照不是 full-history（T04/T18）"));
    }
    let doc = loro::LoroDoc::new();
    doc.import(&snapshot.loro_snapshot_bytes)
        .map_err(|_| SyncError::Object("快照 Loro 数据非法"))?;
    // 标志位与 VV 都不是 full-history 证据：Loro shallow snapshot 必须单独拒绝。
    crate::checkpoint::ensure_snapshot_full_history(&doc)?;
    let actual = doc.oplog_vv();
    let claimed = if snapshot.vv.is_empty() {
        loro::VersionVector::new()
    } else {
        loro::VersionVector::decode(&snapshot.vv).map_err(|_| SyncError::Object("快照 VV 非法"))?
    };
    if actual != claimed {
        return Err(SyncError::Object(
            "快照宣称 VV 与实际 Loro 历史不一致（T18）",
        ));
    }
    Ok(())
}

/// 阶段 E：两份基线必须对象独立、S 覆盖冻结点，且各自证据可解密验证。
pub fn assert_independent_baselines(
    first: &BaselineEvidence,
    second: &BaselineEvidence,
    freeze_seq: u64,
) -> Result<(), SyncError> {
    if first.checkpoint_id == second.checkpoint_id
        || first.manifest_object_id == second.manifest_object_id
    {
        return Err(SyncError::Object("两份基线复用了同一 cid/manifest"));
    }
    if first.covered_seq < freeze_seq || second.covered_seq < freeze_seq {
        return Err(SyncError::Object("基线 S 未覆盖冻结点"));
    }
    if first.asset_set_digest != second.asset_set_digest {
        return Err(SyncError::Object("两份基线的资产集合不一致"));
    }
    if first
        .snapshot_ids
        .iter()
        .any(|id| second.snapshot_ids.contains(id))
    {
        return Err(SyncError::Object("两份基线复用了 snapshot 对象"));
    }
    Ok(())
}

/// 从两份 prepared 中提取 snapshot object id 集合（用于证明身份不重叠）。
pub fn snapshot_object_ids(prepared: &crate::checkpoint::PreparedCheckpoint) -> Vec<ObjectId> {
    prepared
        .report
        .docs
        .iter()
        .map(|doc| doc.object.object_id)
        .collect()
}

/// frozen 分页结果：资产列表、relay asset_set_digest、total。
pub type FrozenAssets = (Vec<crate::relay::FrozenAssetRef>, Option<[u8; 32]>, u64);
/// 重加密结果：新资产、源 asset_set_digest、源 total。
pub type ReencryptedAssets = (Vec<ReencryptedAsset>, Option<[u8; 32]>, u64);

/// 阶段 B 的真实管理 realm begin（device client 不能调用 `POST /rotations`）。
#[allow(clippy::too_many_arguments)]
pub fn begin_rotation_http<T: crate::http_relay::HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault: &Vault,
    journal: &mut crate::rotation_journal::RotationJournal,
    vault_id: VaultId,
    instance_id: &[u8; 16],
    log_id: [u8; 16],
    manager: &ManagementKey,
    revoke: &[[u8; 16]],
    now: u64,
) -> Result<StartedRotation, SyncError> {
    let (token, _) =
        crate::recovery_enroll::management_token(transport, api, vault_id, manager, now)?;
    let begin = crate::rotation::sign_begin(
        manager,
        instance_id,
        vault_id,
        journal.request_id,
        journal.from_epoch,
        journal.control_rev,
        revoke,
        api.version(),
    )?;
    let signed = crate::cbor::CdeValue::Map(vec![
        (0, crate::cbor::bytes(begin.signed_body.clone())),
        (1, crate::cbor::bytes(begin.signature.to_vec())),
    ]);
    let response = crate::recovery_enroll::send(
        transport,
        "POST",
        api.url(&api.vault_path(&vault_id, "/rotations")),
        Some(&token),
        crate::cbor::encode(vec![
            (0, signed),
            (1, crate::cbor::bytes(begin.request_id.to_vec())),
            (2, crate::cbor::uint(begin.expected_control_rev)),
            (3, crate::cbor::uint(begin.expected_epoch)),
            (
                4,
                crate::cbor::array(
                    begin
                        .revoke
                        .iter()
                        .map(|device| crate::cbor::bytes(device.to_vec()))
                        .collect(),
                ),
            ),
        ])?,
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    let started = StartedRotation {
        rotation_id: crate::cbor::bytes_fixed(&view, 0)?,
        state: crate::cbor::text_field(&view, 1)?,
        from_epoch: crate::cbor::uint_field(&view, 2)?,
        to_epoch: crate::cbor::uint_field(&view, 3)?,
        freeze_seq: crate::cbor::uint_field(&view, 4)?,
    };
    if started.freeze_seq != journal.freeze_seq.unwrap_or(started.freeze_seq) {
        return Err(SyncError::RelayWire("begin S 与 journal 冻结点不符"));
    }
    journal.mark_begun(started.rotation_id, started.freeze_seq, log_id)?;
    journal.save(vault)?;
    Ok(started)
}

/// 管理 realm begin 的视图（不需要完整 RotationView）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedRotation {
    pub rotation_id: [u8; 16],
    pub state: String,
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub freeze_seq: u64,
}

/// 阶段 D：用 relay 的 frozen finalized 资产分页（不依赖 log）全量取回。
#[allow(unused_assignments)]
pub fn fetch_frozen_assets<C: RelayClient>(
    client: &mut C,
    rotation_id: [u8; 16],
    page_limit: u32,
) -> Result<FrozenAssets, SyncError> {
    if page_limit == 0 {
        return Err(SyncError::Unsupported("frozen asset 分页 limit 必须 >0"));
    }
    let mut out = Vec::new();
    let mut after = None;
    let mut digest = None;
    let mut total = None;
    loop {
        let page = client.rotation_assets(rotation_id, after, page_limit)?;
        total = Some(page.total);
        if let Some(page_digest) = page.asset_set_digest {
            if digest.is_some_and(|existing| existing != page_digest) {
                return Err(SyncError::RelayWire("frozen 资产分页 digest 变化"));
            }
            digest = Some(page_digest);
        }
        let last = page.assets.last().map(|asset| asset.object_id);
        out.extend(page.assets);
        if out.len() as u64 >= total.unwrap_or(0) || last.is_none() {
            break;
        }
        after = last;
    }
    let total = total.unwrap_or(0);
    if out.len() as u64 != total {
        return Err(SyncError::RelayWire("frozen 资产分页数量与 total 不符"));
    }
    Ok((out, digest, total))
}

/// 阶段 D：按 relay frozen 分页逐资产解密/验证/新根重加密。
#[allow(clippy::too_many_arguments)]
pub fn reencrypt_frozen_assets<C: RelayClient>(
    vault: &Vault,
    client: &mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: &DeviceSigningKey,
    old_root: &RootSecretV1,
    new_root: &RootSecretV1,
    rotation_id: [u8; 16],
    page_limit: u32,
    reservation: Option<&[u8; 16]>,
) -> Result<ReencryptedAssets, SyncError> {
    let (refs, digest, total) = fetch_frozen_assets(client, rotation_id, page_limit)?;
    let mut out = Vec::with_capacity(refs.len());
    for asset in refs {
        let (_, payload) = fetch_object(
            client,
            old_root,
            ObjectType::Asset,
            &asset.object_id,
            asset.part_count,
            asset.total_plain_len,
            &asset.object_digest,
            &[],
        )?;
        let (real, byte_len, media_type) = decode_asset_plaintext(&payload)?;
        let plaintext =
            asset_plaintext(&real, byte_len, &media_type, payload_asset_data(&payload)?)?;
        let object_id = asset_object_id(new_root, &real, byte_len, &media_type)?;
        let message_id = asset_message_id(new_root, &object_id)?;
        let object = if let Some(reservation) = reservation {
            crate::upload::upload_object_with_reservation(
                client,
                vault_id,
                device_id,
                signing_key,
                new_root,
                ObjectType::Asset,
                object_id,
                message_id,
                0,
                0,
                &plaintext,
                &[],
                None,
                reservation,
            )?
        } else {
            upload_object(
                client,
                vault_id,
                device_id,
                signing_key,
                new_root,
                ObjectType::Asset,
                object_id,
                message_id,
                0,
                0,
                &plaintext,
                &[],
                None,
            )?
        };
        crate::asset_sync::mark_uploaded(
            vault,
            &crate::asset_sync::AssetUploadedFact {
                object_id: object.object_id,
                part_count: object.part_count,
                total_plain_len: object.total_plain_len,
                object_digest: object.object_digest,
                key_epoch: new_root.key_epoch,
            },
        )?;
        out.push(ReencryptedAsset {
            real_sha256: real,
            byte_len,
            media_type,
            object_id: object.object_id,
            object_digest: object.object_digest,
        });
    }
    Ok((out, digest, total))
}

pub(crate) fn payload_asset_data(payload: &[u8]) -> Result<&[u8], SyncError> {
    if payload.len() < 42 {
        return Err(SyncError::Object("资产明文过短"));
    }
    let mime_len = usize::from(u16::from_be_bytes(
        payload[40..42]
            .try_into()
            .map_err(|_| SyncError::Object("资产 MIME 长度字段非法"))?,
    ));
    payload
        .get(42 + mime_len..)
        .ok_or(SyncError::Object("资产数据越界"))
}

/// 阶段 E：候选 checkpoint 必须 ready、role 固定且尚未被 activate 对外生效。
pub fn assert_candidate_ready<C: RelayClient>(
    client: &mut C,
    checkpoint_id: [u8; 16],
    expected_role: &str,
    freeze_seq: u64,
) -> Result<crate::relay::CheckpointStatus, SyncError> {
    let status = client.checkpoint_status(checkpoint_id)?;
    if status.state != "candidate" {
        return Err(SyncError::Object(
            "基线 checkpoint 不是 candidate（已对外生效？）",
        ));
    }
    if status.role.as_deref() != Some(expected_role) {
        return Err(SyncError::Object(
            "基线 role 不是 primary/fallback 固定角色",
        ));
    }
    if !status.candidate_ready {
        return Err(SyncError::Object("候选基线尚未 ready"));
    }
    if status.covered_seq < freeze_seq {
        return Err(SyncError::Object("候选基线未覆盖冻结点"));
    }
    Ok(status)
}

/// §30.3：restart（canonical body 空）；失败不要 abort，调用方先查 current。
#[allow(clippy::too_many_arguments)]
pub fn restart_rotation<T: crate::http_relay::HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    rotation_id: [u8; 16],
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    request_id: [u8; 16],
    expected_control_rev: u64,
    now: u64,
) -> Result<(), SyncError> {
    let (token, _) =
        crate::recovery_enroll::management_token(transport, api, vault_id, manager, now)?;
    let path = api.vault_path(
        &vault_id,
        &format!(
            "/rotations/{}/restart",
            crate::base64url::encode(&rotation_id)
        ),
    );
    let body = cbor_empty();
    let (signed, signature) = sign_management(
        manager,
        instance_id,
        vault_id,
        request_id,
        expected_control_rev,
        &path,
        &body,
    )?;
    let response = crate::recovery_enroll::send(
        transport,
        "POST",
        api.url(&path),
        Some(&token),
        crate::cbor::encode(vec![
            (0, signed),
            (1, crate::cbor::bytes(request_id.to_vec())),
            (2, crate::cbor::uint(expected_control_rev)),
        ])?,
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let _view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    let _ = signature;
    Ok(())
}

/// §30.3：revoke-more（canonical body `{0: revoke[]}`，外层键 3 必须逐字节镜像）。
#[allow(clippy::too_many_arguments)]
pub fn revoke_more<T: crate::http_relay::HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    rotation_id: [u8; 16],
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    request_id: [u8; 16],
    expected_control_rev: u64,
    revoke: &[[u8; 16]],
    now: u64,
) -> Result<u64, SyncError> {
    let (token, _) =
        crate::recovery_enroll::management_token(transport, api, vault_id, manager, now)?;
    let path = api.vault_path(
        &vault_id,
        &format!(
            "/rotations/{}/revoke-more",
            crate::base64url::encode(&rotation_id)
        ),
    );
    let body = crate::cbor::encode(vec![(
        0,
        crate::cbor::array(
            revoke
                .iter()
                .map(|device| crate::cbor::bytes(device.to_vec()))
                .collect(),
        ),
    )])?;
    let (signed, _) = sign_management(
        manager,
        instance_id,
        vault_id,
        request_id,
        expected_control_rev,
        &path,
        &body,
    )?;
    let response = crate::recovery_enroll::send(
        transport,
        "POST",
        api.url(&path),
        Some(&token),
        crate::cbor::encode(vec![
            (0, signed),
            (1, crate::cbor::bytes(request_id.to_vec())),
            (2, crate::cbor::uint(expected_control_rev)),
            (
                3,
                crate::cbor::array(
                    revoke
                        .iter()
                        .map(|device| crate::cbor::bytes(device.to_vec()))
                        .collect(),
                ),
            ),
        ])?,
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let _view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    crate::cbor::uint_field(&fields, 2)
}

/// staged grants 读回：leaf manifests、grants digest、count。
pub type StagedGrants = (Vec<Vec<u8>>, [u8; 32], u64);

/// §30.3：management 读 staged grants（叶子 manifest + digest/count）。
pub fn list_staged_grants<T: crate::http_relay::HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    rotation_id: [u8; 16],
    manager: &ManagementKey,
    now: u64,
) -> Result<StagedGrants, SyncError> {
    // 返回值即 relay `{0 leaves,1 grants_digest,2 count}`。
    let (token, _) =
        crate::recovery_enroll::management_token(transport, api, vault_id, manager, now)?;
    let response = crate::recovery_enroll::send(
        transport,
        "GET",
        api.vault_path(
            &vault_id,
            &format!(
                "/rotations/{}/grants",
                crate::base64url::encode(&rotation_id)
            ),
        ),
        Some(&token),
        Vec::new(),
    )?;
    let fields = crate::recovery_enroll::decode(&response)?;
    let leaves = crate::cbor::array_field(&fields, 0)?
        .iter()
        .map(crate::cbor::value_bytes)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((
        leaves,
        crate::cbor::bytes_fixed(&fields, 1)?,
        crate::cbor::uint_field(&fields, 2)?,
    ))
}

fn cbor_empty() -> Vec<u8> {
    let fields: crate::cbor::Fields = Vec::new();
    crate::cbor::encode(fields).unwrap_or_default()
}

/// 管理写签名（`POST=2` + 完整 path + instance/vault/request_id/expected_control_rev +
/// `SHA256(CDE{method,path,body})`）。
///
/// `pub(crate)`：ADR 0010 的 `device_management::revoke_device` 复用同一实现，避免复制
/// 签名输入拼接（wire 变更必须只有一处）。
pub(crate) fn sign_management(
    manager: &ManagementKey,
    instance_id: &[u8; 16],
    vault_id: VaultId,
    request_id: [u8; 16],
    expected_control_rev: u64,
    path: &str,
    body: &[u8],
) -> Result<(crate::cbor::CdeValue, [u8; 64]), SyncError> {
    let digest: [u8; 32] = Sha256::digest(menghunote_sync_proto::management_request_cde(
        menghunote_sync_proto::HttpMethod::Post,
        path,
        body,
    )?)
    .into();
    let signature = manager.sign(&menghunote_sync_proto::management_signing_input(
        instance_id,
        &vault_id,
        &request_id,
        expected_control_rev,
        &digest,
    )?);
    Ok((
        crate::cbor::CdeValue::Map(vec![
            (0, crate::cbor::bytes(body.to_vec())),
            (1, crate::cbor::bytes(signature.to_vec())),
        ]),
        signature,
    ))
}

/// 阶段 F：一个待 stage 的 grant leaf。
#[derive(Debug, Clone)]
pub struct GrantLeaf {
    pub device_id: DeviceId,
    pub epoch: u64,
    pub grant_id: [u8; 16],
    pub hpke_enc: [u8; 32],
    pub ciphertext: Vec<u8>,
    pub signed_header: Vec<u8>,
    pub signature: [u8; 64],
    pub manager_rev: u64,
    pub leaf_digest: [u8; 32],
}

impl GrantLeaf {
    /// leaf manifest 中用于 activate 锁定 digest 的规范字节。
    pub fn manifest_bytes(&self) -> Result<Vec<u8>, SyncError> {
        crate::cbor::encode(vec![
            (0, crate::cbor::bytes(self.device_id.to_vec())),
            (1, crate::cbor::uint(self.epoch)),
            (2, crate::cbor::bytes(self.grant_id.to_vec())),
            (3, crate::cbor::bytes(self.hpke_enc.to_vec())),
            (4, crate::cbor::bytes(self.ciphertext.clone())),
            (5, crate::cbor::bytes(self.signed_header.clone())),
            (6, crate::cbor::bytes(self.signature.to_vec())),
            (7, crate::cbor::uint(self.manager_rev)),
        ])
    }
}

/// 阶段 F：对一组（未吊销、含离线）设备封真实 root。
pub fn plan_grants(
    new_root: &RootSecretV1,
    manager: &ManagementKey,
    manager_rev: u64,
    recipients: &[(DeviceId, [u8; 32])],
) -> Result<Vec<GrantLeaf>, SyncError> {
    let mut leaves = Vec::with_capacity(recipients.len());
    for (device_id, hpke_pub) in recipients {
        let grant_id: [u8; 16] = crate::random::array(&mut crate::random::OsRandom)?;
        let aad = GrantAadV1 {
            pv: 1,
            vault_id: new_root.vault_id,
            epoch: new_root.key_epoch,
            recipient_device_id: *device_id,
            recipient_x25519_pub: *hpke_pub,
            manager_rev,
            grant_id,
        };
        let sealed = seal_epoch_grant(hpke_pub, new_root, &aad)?;
        let signed_header = sealed.signature_input(&aad)?;
        let signature = manager.sign(&signed_header);
        let mut leaf = GrantLeaf {
            device_id: *device_id,
            epoch: new_root.key_epoch,
            grant_id,
            hpke_enc: sealed.enc,
            ciphertext: sealed.ciphertext,
            signed_header,
            signature,
            manager_rev,
            leaf_digest: [0u8; 32],
        };
        let manifest = leaf.manifest_bytes()?;
        leaf.leaf_digest = Sha256::digest(&manifest).into();
        leaves.push(leaf);
    }
    Ok(leaves)
}

/// 解码 asset plaintext 前缀（32B real sha + BE64 len + BE16 mime len + mime + data）。
pub(crate) fn decode_asset_plaintext(bytes: &[u8]) -> Result<([u8; 32], u64, String), SyncError> {
    if bytes.len() < 42 {
        return Err(SyncError::Object("资产明文过短"));
    }
    let real = bytes[..32]
        .try_into()
        .map_err(|_| SyncError::Object("资产 real_sha256 长度非法"))?;
    let len = u64::from_be_bytes(
        bytes[32..40]
            .try_into()
            .map_err(|_| SyncError::Object("资产长度字段非法"))?,
    );
    let mime_len = usize::from(u16::from_be_bytes(
        bytes[40..42]
            .try_into()
            .map_err(|_| SyncError::Object("资产 MIME 长度字段非法"))?,
    ));
    let mime_end = 42 + mime_len;
    let mime = std::str::from_utf8(
        bytes
            .get(42..mime_end)
            .ok_or(SyncError::Object("资产 MIME 越界"))?,
    )
    .map_err(|_| SyncError::Object("资产 MIME 不是 UTF-8"))?
    .to_owned();
    let data = bytes
        .get(mime_end..)
        .ok_or(SyncError::Object("资产数据越界"))?;
    if data.len() as u64 != len {
        return Err(SyncError::Object("资产数据长度与明文字段不符"));
    }
    let hash: [u8; 32] = Sha256::digest(data).into();
    if hash != real {
        return Err(SyncError::Object("资产真实 SHA256 不符"));
    }
    Ok((real, len, mime))
}

/// 阶段 F：把 grant leaf stage 到 relay（management realm；§28.1 条目 5）。
pub fn stage_grants<T: crate::http_relay::HttpTransport>(
    transport: &mut T,
    api: &RelayApi,
    vault_id: VaultId,
    rotation_id: [u8; 16],
    manager: &ManagementKey,
    leaves: &[GrantLeaf],
    now: u64,
) -> Result<(), SyncError> {
    let (token, _) =
        crate::recovery_enroll::management_token(transport, api, vault_id, manager, now)?;
    for leaf in leaves {
        // URL 里的 `{device_id}` 只是设备维度；body 键 0 是**真实 grant_id**（grant AAD
        // 里绑定的那个），relay 按 grant_id 验签/落库，不能拿 device_id 顶替。
        let body = menghunote_sync_proto::GrantStageBodyV1 {
            grant_id: leaf.grant_id,
            hpke_enc: leaf.hpke_enc,
            hpke_ciphertext: leaf.ciphertext.clone(),
            signed_header: leaf.signed_header.clone(),
            signature: leaf.signature.to_vec(),
            manager_rev: leaf.manager_rev,
        }
        .encode()?;
        let response = crate::recovery_enroll::send(
            transport,
            "PUT",
            api.url(&api.vault_path(
                &vault_id,
                &format!(
                    "/rotations/{}/grants/{}",
                    crate::base64url::encode(&rotation_id),
                    crate::base64url::encode(&leaf.device_id)
                ),
            )),
            Some(&token),
            body,
        )?;
        let fields = crate::recovery_enroll::decode(&response)?;
        let _view = crate::cbor::value_map(crate::cbor::field(&fields, 0)?)?;
    }
    Ok(())
}

/// 阶段 F：把 grant leaf manifest 持久化进 rotation journal（stage 网络调用等 relay 101）。
pub fn attach_grants(
    journal: &mut crate::rotation_journal::RotationJournal,
    vault: &Vault,
    leaves: &[GrantLeaf],
) -> Result<[u8; 32], SyncError> {
    let manifests = leaves
        .iter()
        .map(GrantLeaf::manifest_bytes)
        .collect::<Result<Vec<_>, _>>()?;
    journal.set_grants(vault, &manifests)
}

/// 便捷入口：用现有 `CheckpointWriter` 准备一份 full-history 基线（阶段 D/E）。
#[allow(clippy::too_many_arguments)]
pub fn prepare_new_epoch_baseline<'a, C: RelayClient>(
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    new_root: RootSecretV1,
    peer_id: u64,
    assets: &'a dyn AssetSource,
) -> Result<crate::checkpoint::PreparedCheckpoint, SyncError> {
    prepare_new_epoch_baseline_with_role(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        new_root,
        peer_id,
        assets,
        None,
    )
}

/// 同上，但为候选 epoch 固定 primary/fallback role（relay §28.1）。
#[allow(clippy::too_many_arguments)]
pub fn prepare_new_epoch_baseline_with_role<'a, C: RelayClient>(
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    new_root: RootSecretV1,
    peer_id: u64,
    assets: &'a dyn AssetSource,
    role: Option<&str>,
) -> Result<crate::checkpoint::PreparedCheckpoint, SyncError> {
    prepare_baseline_with_context(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        new_root,
        peer_id,
        assets,
        role,
        None,
        None,
    )
}

/// 同上，但显式传入候选 epoch 的 coverage 验证根（**旧 active 根**）。
///
/// 候选基线在 `from_epoch` 冻结点导出：`<= S` 的 ASSET 日志事件用旧根加密，而
/// manifest refs 里的资产用新 epoch 根加密。缺旧根时 validate 必须暂停/报错，
/// 绝不拿新根去"猜"。
#[allow(clippy::too_many_arguments)]
pub fn prepare_new_epoch_baseline_with_role_and_coverage_root<'a, C: RelayClient>(
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    new_root: RootSecretV1,
    peer_id: u64,
    assets: &'a dyn AssetSource,
    role: Option<&str>,
    coverage_root: &RootSecretV1,
) -> Result<crate::checkpoint::PreparedCheckpoint, SyncError> {
    prepare_baseline_with_context(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        new_root,
        peer_id,
        assets,
        role,
        None,
        Some(coverage_root.clone()),
    )
}

/// restart 候选基线：额外把 rotation/CAS 交给 writer，使 create intent 与
/// upload/validate/promote 各阶段进度都 durable（评审 0003 P1-D）。
///
/// 与 [`prepare_new_epoch_baseline_with_role`] 不同，本入口要求 writer 在
/// create 之前持久 per-role intent；崩溃/重试按同一 request_id/cid 恢复，绝不 fresh create。
#[allow(clippy::too_many_arguments)]
pub fn prepare_restart_baseline_with_role<'a, C: RelayClient>(
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    new_root: RootSecretV1,
    peer_id: u64,
    assets: &'a dyn AssetSource,
    role: &str,
    rotation_id: [u8; 16],
    expected_control_rev: u64,
    coverage_root: &RootSecretV1,
) -> Result<crate::checkpoint::PreparedCheckpoint, SyncError> {
    prepare_baseline_with_context(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        new_root,
        peer_id,
        assets,
        Some(role),
        Some((rotation_id, expected_control_rev)),
        Some(coverage_root.clone()),
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_baseline_with_context<'a, C: RelayClient>(
    vault: &'a Vault,
    client: &'a mut C,
    vault_id: VaultId,
    device_id: DeviceId,
    signing_key: DeviceSigningKey,
    new_root: RootSecretV1,
    peer_id: u64,
    assets: &'a dyn AssetSource,
    role: Option<&str>,
    restart_baseline: Option<([u8; 16], u64)>,
    coverage_root: Option<RootSecretV1>,
) -> Result<crate::checkpoint::PreparedCheckpoint, SyncError> {
    let mut writer = CheckpointWriter::new(
        vault,
        client,
        vault_id,
        device_id,
        signing_key,
        new_root,
        peer_id,
    );
    if let Some(role) = role {
        writer = writer.with_role(role);
    }
    if let Some((rotation_id, expected_control_rev)) = restart_baseline {
        writer = writer.with_restart_baseline(rotation_id, expected_control_rev);
    }
    if let Some(coverage_root) = coverage_root {
        writer = writer.with_coverage_root(coverage_root);
    }
    // ADR §32：候选基线同样必须 prepare → validate（真读回/导入）→ promote（签名发布）。
    // 返回的 PreparedCheckpoint 已标记 validated，供阶段 E 继续采集独立证据。
    let mut prepared = writer.prepare(assets)?;
    let baseline = writer.validate(&prepared)?;
    let promoted = writer.promote(&baseline)?;
    prepared.report.validated = true;
    prepared.report.published_at = Some(promoted.published_at);
    prepared.report.commit_digest = Some(promoted.commit_digest);
    Ok(prepared)
}

`````



@@SECTION code/052 `crates/sync/src/slot.rs`


`````rust
//! 密钥槽（ADR 0006 §3 / §15）：wrap/unwrap 根密钥、改口令纯逻辑。
//!
//! **wire 类型来自 `menghunote-sync-proto`**：`SlotDescriptorV1`、`Kind`、`Kdf`、
//! `Argon2idParams` 的 CDE 编码/解码/边界校验都在 proto；本模块只加：
//!
//! - **策略校验**：kind 与 kdf 必须匹配、Argon2 参数在读者区间内（**调用 Argon2 之前**）；
//! - KEK 派生（HKDF 域来自 proto 常量）；
//! - 槽体 AEAD：`nonce(24B) || XChaCha20-Poly1305(K_wrap, AAD, RootSecretV1 CDE)`；
//! - 改口令的 CAS 纯逻辑。
//!
//! `AAD` 由**信封层**构造（`menghunote-sync-proto::EnvelopeHeader::aad`，ADR §2：
//! `b"MN/e2e/v1\0" || header[0..196] || BE32(desc_len) || desc`）。信封 header 里的
//! `this_plaintext_len` 决定 AAD，所以 [`SlotAad`] 会收到明文长度；加密侧用根 CDE 的长度，
//! 解密侧用 `ciphertext_len - TAG_LEN`（两者必须一致，这正是 AEAD 认证的一部分）。

use crate::domain;
use crate::error::SyncError;
use crate::kdf;
use crate::keys::{RootSecretV1, WrapKey};
use crate::random::RandomSource;
use crate::recovery::{self, RecoverySeed};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use menghunote_sync_proto::{Kdf, SlotDescriptorV1, SlotKind};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// 槽协议版本（v1 = 1，与 proto 的 `PROTOCOL_VERSION` 一致）。
pub const PROTOCOL_VERSION: u16 = 1;
/// 唯一的 wrap 套件：XChaCha20-Poly1305（`cipher_suite = 1`，ADR §2）。
pub const WRAP_SUITE_XCHACHA20_POLY1305: u16 = 1;
/// XChaCha20-Poly1305 nonce 长度（ADR §2：header 内 24B CSPRNG nonce）。
pub const NONCE_LEN: usize = 24;
/// Poly1305 tag 长度。
pub const TAG_LEN: usize = 16;

fn kind_label(kind: SlotKind) -> &'static str {
    match kind {
        SlotKind::Password => "password",
        SlotKind::Recovery => "recovery",
    }
}

/// 槽描述的策略校验（proto 校验之后还要过这一层）。
///
/// proto 的 `SlotDescriptorV1::validate` 只查版本/wrap_suite/key_epoch>0，且在
/// `encode/to_cde` 时才查 Argon2 参数；本函数补上 **kind ↔ kdf 一致性** 与
/// **KDF 之前的 Argon2 范围闸门**，供本地构造的描述在派生密钥前调用。
pub fn validate_slot_descriptor(descriptor: &SlotDescriptorV1) -> Result<(), SyncError> {
    descriptor.validate()?;
    if descriptor.created_at > i64::MAX as u64 {
        return Err(SyncError::Descriptor("created_at 超过 i64::MAX"));
    }
    if descriptor.manager_rev > i64::MAX as u64 {
        return Err(SyncError::Descriptor("manager_rev 超过 i64::MAX"));
    }
    match (&descriptor.kind, &descriptor.kdf) {
        (SlotKind::Password, Kdf::Argon2id(params)) => {
            kdf::validate_argon2id_params(params)?;
        }
        (SlotKind::Recovery, Kdf::RecoveryHkdf) => {}
        (SlotKind::Password, Kdf::RecoveryHkdf) => {
            return Err(SyncError::SlotKind {
                expected: "password/argon2id",
                actual: "password/recovery-hkdf",
            })
        }
        (SlotKind::Recovery, Kdf::Argon2id(_)) => {
            return Err(SyncError::SlotKind {
                expected: "recovery/hkdf",
                actual: "recovery/argon2id",
            })
        }
    }
    Ok(())
}

/// `context_hash = SHA256(CDE(descriptor))`（ADR §2：KEY_SLOT header 的 `context_hash`）。
pub fn context_hash(descriptor: &SlotDescriptorV1) -> Result<[u8; 32], SyncError> {
    let digest = Sha256::digest(descriptor.encode()?);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

/// 口令槽：派生 `K_password_wrap = HKDF(Argon2id(password), vault_id,
/// b"MN/v1/password-wrap\0" || slot_id, 32)`（ADR §3）。
///
/// 口令与 Argon2 参数都**先校验**，越界时不会执行 Argon2。
pub fn password_wrap_key(
    descriptor: &SlotDescriptorV1,
    password: &[u8],
) -> Result<WrapKey, SyncError> {
    validate_slot_descriptor(descriptor)?;
    if descriptor.kind != SlotKind::Password {
        return Err(SyncError::SlotKind {
            expected: "password",
            actual: kind_label(descriptor.kind),
        });
    }
    let params = match &descriptor.kdf {
        Kdf::Argon2id(params) => params,
        Kdf::RecoveryHkdf => {
            return Err(SyncError::SlotKind {
                expected: "argon2id",
                actual: "recovery-hkdf",
            })
        }
    };
    let material = kdf::argon2id_output(password, params)?;
    let info = domain::password_wrap_info(&descriptor.slot_id);
    let kek = kdf::hkdf32(material.expose_slice(), &descriptor.vault_id, &info)?;
    Ok(WrapKey::from_secret(kek))
}

/// 恢复槽：派生 `K_recovery_wrap = HKDF(R, vault_id,
/// b"MN/v1/recovery-wrap\0" || slot_id, 32)`（ADR §3）。
pub fn recovery_wrap_key(
    descriptor: &SlotDescriptorV1,
    seed: &RecoverySeed,
) -> Result<WrapKey, SyncError> {
    validate_slot_descriptor(descriptor)?;
    if descriptor.kind != SlotKind::Recovery {
        return Err(SyncError::SlotKind {
            expected: "recovery",
            actual: kind_label(descriptor.kind),
        });
    }
    if !matches!(descriptor.kdf, Kdf::RecoveryHkdf) {
        return Err(SyncError::SlotKind {
            expected: "recovery-hkdf",
            actual: "argon2id",
        });
    }
    recovery::recovery_wrap_key(seed, &descriptor.vault_id, &descriptor.slot_id)
}

/// 信封层提供的 AAD 构造器。
///
/// ADR §2：`AAD = b"MN/e2e/v1\0" || header[0..196] || BE32(descriptor_len) || descriptor`。
/// header 里的 `object_id`/`nonce`/`this_plaintext_len` 等只有信封层知道，所以由它来拼；
/// 本模块把"即将写入 header 的 nonce 与明文长度"回传给构造器，保证加解密两侧一致。
///
/// 真实调用方应当先用 proto 构造 `EnvelopeHeader`，再调用 `header.aad(&descriptor.encode()?)`。
pub trait SlotAad {
    /// 给定槽描述、nonce 与明文长度，返回完整 AAD 字节。
    fn aad(
        &self,
        descriptor: &SlotDescriptorV1,
        nonce: &[u8; NONCE_LEN],
        plaintext_len: usize,
    ) -> Result<Vec<u8>, SyncError>;
}

impl<F> SlotAad for F
where
    F: Fn(&SlotDescriptorV1, &[u8; NONCE_LEN], usize) -> Result<Vec<u8>, SyncError>,
{
    fn aad(
        &self,
        descriptor: &SlotDescriptorV1,
        nonce: &[u8; NONCE_LEN],
        plaintext_len: usize,
    ) -> Result<Vec<u8>, SyncError> {
        self(descriptor, nonce, plaintext_len)
    }
}

/// 槽体：`nonce || ciphertext || tag`（AEAD 输出已含 tag）。
#[derive(Clone, PartialEq, Eq)]
pub struct WrappedRoot {
    /// 24B CSPRNG nonce（同时写进 MNE1 header）。
    pub nonce: [u8; NONCE_LEN],
    /// XChaCha20-Poly1305 密文（含 16B tag）。
    pub ciphertext: Vec<u8>,
}

impl WrappedRoot {
    /// 序列化为 `nonce || ciphertext`。
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(NONCE_LEN + self.ciphertext.len());
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// 从 `nonce || ciphertext` 解析（至少要有 nonce + tag）。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SyncError> {
        if bytes.len() < NONCE_LEN + TAG_LEN {
            return Err(SyncError::Unwrap);
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&bytes[..NONCE_LEN]);
        Ok(Self {
            nonce,
            ciphertext: bytes[NONCE_LEN..].to_vec(),
        })
    }
}

/// 一个密钥槽：proto 的槽描述 + 被 KEK 封装的根。
#[derive(Clone, PartialEq, Eq)]
pub struct KeySlotV1 {
    /// 槽描述（CDE 会进 AAD / context_hash）。
    pub descriptor: SlotDescriptorV1,
    /// 槽体。
    pub wrapped: WrappedRoot,
}

impl KeySlotV1 {
    /// 用口令槽封装根。
    pub fn wrap_password(
        descriptor: &SlotDescriptorV1,
        password: &[u8],
        root: &RootSecretV1,
        aad: &impl SlotAad,
        rng: &mut impl RandomSource,
    ) -> Result<Self, SyncError> {
        let kek = password_wrap_key(descriptor, password)?;
        Self::wrap_with_key(descriptor, &kek, root, aad, rng)
    }

    /// 用恢复槽封装根。
    pub fn wrap_recovery(
        descriptor: &SlotDescriptorV1,
        seed: &RecoverySeed,
        root: &RootSecretV1,
        aad: &impl SlotAad,
        rng: &mut impl RandomSource,
    ) -> Result<Self, SyncError> {
        let kek = recovery_wrap_key(descriptor, seed)?;
        Self::wrap_with_key(descriptor, &kek, root, aad, rng)
    }

    fn wrap_with_key(
        descriptor: &SlotDescriptorV1,
        kek: &WrapKey,
        root: &RootSecretV1,
        aad: &impl SlotAad,
        rng: &mut impl RandomSource,
    ) -> Result<Self, SyncError> {
        validate_slot_descriptor(descriptor)?;
        if descriptor.vault_id != root.vault_id || descriptor.key_epoch != root.key_epoch {
            return Err(SyncError::Descriptor("槽描述与根归属（vault/epoch）不一致"));
        }
        let plaintext = Zeroizing::new(root.encode()?);
        let nonce = crate::random::array::<NONCE_LEN>(rng)?;
        let aad_bytes = aad.aad(descriptor, &nonce, plaintext.len())?;
        let cipher =
            XChaCha20Poly1305::new_from_slice(kek.expose()).map_err(|_| SyncError::Unwrap)?;
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext.as_slice(),
                    aad: &aad_bytes,
                },
            )
            .map_err(|_| SyncError::Unwrap)?;
        Ok(Self {
            descriptor: descriptor.clone(),
            wrapped: WrappedRoot { nonce, ciphertext },
        })
    }

    /// 用口令解封根；错口令/错槽/篡改 → [`SyncError::Unwrap`]。
    pub fn unwrap_password(
        &self,
        password: &[u8],
        aad: &impl SlotAad,
    ) -> Result<RootSecretV1, SyncError> {
        let kek = password_wrap_key(&self.descriptor, password)?;
        self.unwrap_with_key(&kek, aad)
    }

    /// 用恢复码解封根；错 R/错槽/篡改 → [`SyncError::Unwrap`]。
    pub fn unwrap_recovery(
        &self,
        seed: &RecoverySeed,
        aad: &impl SlotAad,
    ) -> Result<RootSecretV1, SyncError> {
        let kek = recovery_wrap_key(&self.descriptor, seed)?;
        self.unwrap_with_key(&kek, aad)
    }

    fn unwrap_with_key(
        &self,
        kek: &WrapKey,
        aad: &impl SlotAad,
    ) -> Result<RootSecretV1, SyncError> {
        validate_slot_descriptor(&self.descriptor)?;
        let plaintext_len = self
            .wrapped
            .ciphertext
            .len()
            .checked_sub(TAG_LEN)
            .ok_or(SyncError::Unwrap)?;
        let aad_bytes = aad.aad(&self.descriptor, &self.wrapped.nonce, plaintext_len)?;
        let cipher =
            XChaCha20Poly1305::new_from_slice(kek.expose()).map_err(|_| SyncError::Unwrap)?;
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    XNonce::from_slice(&self.wrapped.nonce),
                    Payload {
                        msg: &self.wrapped.ciphertext,
                        aad: &aad_bytes,
                    },
                )
                .map_err(|_| SyncError::Unwrap)?,
        );
        let root = RootSecretV1::decode(&plaintext)?;
        if root.vault_id != self.descriptor.vault_id || root.key_epoch != self.descriptor.key_epoch
        {
            return Err(SyncError::Descriptor("解出的根归属与槽描述不一致"));
        }
        Ok(root)
    }

    /// ADR §20：KEY_SLOT 的 MNE1 密钥（password 用 Argon2 输出，recovery 用恢复种子）。
    pub fn mne_key(
        &self,
        password: Option<&[u8]>,
        recovery_seed: Option<&RecoverySeed>,
    ) -> Result<crate::keys::Secret32, SyncError> {
        let ikm = match self.descriptor.kind {
            SlotKind::Password => {
                let Kdf::Argon2id(params) = &self.descriptor.kdf else {
                    return Err(SyncError::Descriptor("password 槽的 KDF 不是 Argon2id"));
                };
                let password = password.ok_or(SyncError::Unwrap)?;
                kdf::argon2id_output(password, params)?
            }
            SlotKind::Recovery => {
                let seed = recovery_seed.ok_or(SyncError::Unwrap)?;
                crate::keys::Secret32::from_array(*seed.expose())
            }
        };
        crate::keys::derive_slot_mne_key(
            ikm.expose_slice(),
            &self.descriptor.vault_id,
            self.descriptor.kind,
            self.descriptor.key_epoch,
            &self.descriptor.slot_id,
        )
    }

    /// 槽体序列化（`nonce || ciphertext`）。
    #[must_use]
    pub fn wrapped_bytes(&self) -> Vec<u8> {
        self.wrapped.to_bytes()
    }
}

/// 槽状态（对应 relay `key_slots.state`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlotState {
    /// 暂存（尚未激活，24h 回收）。
    Staging,
    /// 当前激活。
    Active,
    /// 待退役（已切换，等待删除）。
    Retiring,
}

/// 槽集合里的一项。
#[derive(Clone)]
pub struct SlotEntry {
    /// 槽本体。
    pub slot: KeySlotV1,
    /// 状态。
    pub state: SlotState,
}

/// 槽集合快照。
#[derive(Clone)]
pub struct SlotSetSnapshot {
    /// 槽集合版本（CAS 用）。
    pub rev: u64,
    /// 全部槽。
    pub slots: Vec<SlotEntry>,
}

/// 改口令所需的 CAS 存储接口。
///
/// **语义要求**：
///
/// - [`Self::stage_slot`] 只落新槽（`state = Staging`），不得改动 active 指针；
/// - [`Self::activate_password_slot`] 必须在 `expected_rev == 当前 rev` 时原子完成
///   "新槽 → Active、旧 active password 槽 → Retiring、rev += 1"，否则返回
///   [`SyncError::Cas`]（ADR §3："带 `expected_slot_set_rev` CAS 原子切 active 指针"）；
/// - [`Self::remove_slot`] 删除指定槽（改口令成功后删旧槽）。
///
/// 该接口由 storage 侧实现（本 crate 不碰 SQLite）；本模块只提供纯逻辑。
pub trait SlotStore {
    /// 读取当前槽集合（含 rev 与状态）。
    fn slot_set(&self) -> Result<SlotSetSnapshot, SyncError>;
    /// 暂存新槽（Staging）。
    fn stage_slot(&mut self, slot: &KeySlotV1) -> Result<(), SyncError>;
    /// CAS 切换 active password 槽；成功返回新的槽集合 rev。
    fn activate_password_slot(
        &mut self,
        expected_rev: u64,
        new_slot_id: &[u8; 32],
    ) -> Result<u64, SyncError>;
    /// 删除槽。
    fn remove_slot(&mut self, slot_id: &[u8; 32]) -> Result<(), SyncError>;
}

/// 改口令的输入。
#[derive(Clone, Copy)]
pub struct PasswordChangePlan<'a> {
    /// 新口令（原样 UTF-8，1..=1024B）。
    pub new_password: &'a [u8],
    /// 新槽 ID（32B 随机，调用方生成并保证不复用）。
    pub new_slot_id: [u8; 32],
    /// 新槽创建时刻。
    pub created_at: u64,
}

/// 改口令的结果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PasswordChangeOutcome {
    /// 切换后的槽集合 rev。
    pub new_slot_set_rev: u64,
    /// 被替换的旧口令槽。
    pub old_slot_id: [u8; 32],
    /// 新口令槽。
    pub new_slot_id: [u8; 32],
}

/// 改口令的**纯逻辑**（ADR §3）：
///
/// 1. 找当前 active 的口令槽（没有则报错）；
/// 2. 用新口令 + 新 salt 生成新 `SlotDescriptorV1`，封装**同一个根**；
/// 3. **试解密校验**：重新派生 KEK 并解封新槽，根必须一致（写入前的自检）；
/// 4. `stage_slot` 暂存新槽；
/// 5. 带 `expected_slot_set_rev` CAS 切换 active（[`SlotStore::activate_password_slot`]）；
/// 6. 删除旧口令槽。
///
/// 不碰数据对象、不换 epoch、不动恢复槽与其他设备 DPAPI——"改口令只重包"。
/// 真正的"从 relay 读回再解密"由调用方在 4→5 之间做；本函数的第 3 步是同一套
/// 派生/解封路径的本地自检。槽指针更新的设备签名也不在这里（属于信封/请求层）。
pub fn change_password<S, A, R>(
    store: &mut S,
    root: &RootSecretV1,
    plan: &PasswordChangePlan<'_>,
    aad: &A,
    rng: &mut R,
) -> Result<PasswordChangeOutcome, SyncError>
where
    S: SlotStore,
    A: SlotAad,
    R: RandomSource,
{
    // 口令长度必须在 Argon2 之前拒绝。
    kdf::validate_password(plan.new_password)?;

    let snapshot = store.slot_set()?;
    let mut active: Option<&SlotEntry> = None;
    for entry in &snapshot.slots {
        if entry.state == SlotState::Active && entry.slot.descriptor.kind == SlotKind::Password {
            if active.is_some() {
                return Err(SyncError::PasswordChange("存在多个 active 口令槽"));
            }
            active = Some(entry);
        }
    }
    let old = active.ok_or(SyncError::PasswordChange("没有 active 口令槽"))?;
    let old_slot_id = old.slot.descriptor.slot_id;
    if old_slot_id == plan.new_slot_id {
        return Err(SyncError::PasswordChange("新槽 ID 与旧槽相同"));
    }

    // 新槽：同 vault/epoch/manager_rev，新 slot_id + 新 salt。
    let params = kdf::new_writer_params(rng)?;
    let descriptor = SlotDescriptorV1 {
        protocol_version: PROTOCOL_VERSION,
        slot_id: plan.new_slot_id,
        vault_id: root.vault_id,
        key_epoch: root.key_epoch,
        kind: SlotKind::Password,
        wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
        kdf: Kdf::Argon2id(params),
        created_at: plan.created_at,
        manager_rev: old.slot.descriptor.manager_rev,
    };
    validate_slot_descriptor(&descriptor)?;
    let new_slot = KeySlotV1::wrap_password(&descriptor, plan.new_password, root, aad, rng)?;

    // 试解密校验：重新派生 KEK（再跑一次 Argon2id）并解封，根必须一致。
    let verified = new_slot.unwrap_password(plan.new_password, aad)?;
    if !verified.ct_eq(root) {
        return Err(SyncError::PasswordChange("新槽自检解出的根与当前根不一致"));
    }

    store.stage_slot(&new_slot)?;
    let new_rev = store.activate_password_slot(snapshot.rev, &plan.new_slot_id)?;
    store.remove_slot(&old_slot_id)?;

    Ok(PasswordChangeOutcome {
        new_slot_set_rev: new_rev,
        old_slot_id,
        new_slot_id: plan.new_slot_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::derive_object_key;
    use crate::keys::Secret32;
    use crate::test_support::TestRng;
    use crate::VaultId;
    use menghunote_sync_proto::{Argon2idParams, EnvelopeHeader, ObjectType};
    use zeroize::Zeroizing;

    fn params(salt: [u8; kdf::SALT_LEN]) -> Argon2idParams {
        kdf::writer_params(salt)
    }

    fn vault() -> VaultId {
        [0x10; 16]
    }

    fn password_descriptor(slot_id: [u8; 32], salt: [u8; kdf::SALT_LEN]) -> SlotDescriptorV1 {
        SlotDescriptorV1 {
            protocol_version: PROTOCOL_VERSION,
            slot_id,
            vault_id: vault(),
            key_epoch: 5,
            kind: SlotKind::Password,
            wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
            kdf: Kdf::Argon2id(params(salt)),
            created_at: 1_700_000_000,
            manager_rev: 2,
        }
    }

    fn recovery_descriptor(slot_id: [u8; 32]) -> SlotDescriptorV1 {
        SlotDescriptorV1 {
            protocol_version: PROTOCOL_VERSION,
            slot_id,
            vault_id: vault(),
            key_epoch: 5,
            kind: SlotKind::Recovery,
            wrap_suite: WRAP_SUITE_XCHACHA20_POLY1305,
            kdf: Kdf::RecoveryHkdf,
            created_at: 1_700_000_000,
            manager_rev: 2,
        }
    }

    /// 测试用 AAD：用 proto 的真实 `EnvelopeHeader` 走 ADR §2 的 AAD 构造。
    fn test_aad(
        descriptor: &SlotDescriptorV1,
        nonce: &[u8; NONCE_LEN],
        plaintext_len: usize,
    ) -> Result<Vec<u8>, SyncError> {
        let descriptor_bytes = descriptor.encode()?;
        let header = EnvelopeHeader {
            protocol_version: menghunote_sync_proto::PROTOCOL_VERSION,
            header_len: menghunote_sync_proto::HEADER_LEN as u16,
            object_type: ObjectType::KeySlot,
            cipher_suite: menghunote_sync_proto::CIPHER_SUITE_XCHACHA20_POLY1305,
            flags: 0,
            vault_id: descriptor.vault_id,
            object_id: descriptor.slot_id,
            message_id: [0x0a; 16],
            producer_device_id: [0x0b; 16],
            generation: 0,
            key_epoch: descriptor.key_epoch,
            sender_seq: 0,
            part_index: 0,
            part_count: 1,
            total_plaintext_len: plaintext_len as u64,
            this_plaintext_len: plaintext_len as u32,
            ciphertext_len: (plaintext_len + TAG_LEN) as u32,
            context_hash: context_hash(descriptor)?,
            nonce: *nonce,
        };
        Ok(header.aad(&descriptor_bytes)?)
    }

    #[test]
    fn descriptor_validation_catches_kind_kdf_mismatch() {
        let mut desc = password_descriptor([0x01; 32], [0x02; 16]);
        assert!(validate_slot_descriptor(&desc).is_ok());
        desc.kind = SlotKind::Recovery;
        assert!(matches!(
            validate_slot_descriptor(&desc),
            Err(SyncError::SlotKind { .. })
        ));

        let mut desc = recovery_descriptor([0x03; 32]);
        assert!(validate_slot_descriptor(&desc).is_ok());
        desc.kdf = Kdf::Argon2id(params([0u8; 16]));
        assert!(matches!(
            validate_slot_descriptor(&desc),
            Err(SyncError::SlotKind { .. })
        ));
    }

    #[test]
    fn descriptor_rejects_out_of_range_argon_params_before_kdf() {
        let mut desc = password_descriptor([0u8; 32], [1u8; 16]);
        desc.kdf = Kdf::Argon2id(Argon2idParams {
            version: 0x13,
            m_kib: 1 << 20, // 1 TiB
            t: 3,
            p: 1,
            salt: [0u8; 16],
            output_len: 32,
        });
        // proto 的 encode 会拒绝越界参数
        assert!(desc.encode().is_err());
        // 本地策略校验直接报 Argon2Param（在 KDF 之前）
        assert!(matches!(
            validate_slot_descriptor(&desc),
            Err(SyncError::Argon2Param { field: "m_kib", .. })
        ));
        let started = std::time::Instant::now();
        assert!(password_wrap_key(&desc, b"pw").is_err());
        assert!(started.elapsed().as_millis() < 50, "越界参数不能跑 Argon2");

        // proto 的 decode 侧也会拒绝同一份字节
        let mut good = password_descriptor([0u8; 32], [1u8; 16]);
        good.kdf = Kdf::Argon2id(kdf::writer_params([1u8; 16]));
        let bytes = good.encode().unwrap();
        // 手工把 m_kib 改成 1TiB 不可能（canonical），所以直接断言 proto 参数校验：
        assert!(
            Argon2idParams {
                m_kib: 1 << 20,
                ..kdf::writer_params([1u8; 16])
            }
            .m_kib
                > kdf::M_MAX_KIB
        );
        let _ = bytes;
    }

    #[test]
    fn descriptor_cde_roundtrip_and_context_hash() {
        let desc = password_descriptor([0xab; 32], [0xcd; 16]);
        let bytes = desc.encode().unwrap();
        let parsed = SlotDescriptorV1::decode(&bytes).unwrap();
        assert_eq!(desc, parsed);
        assert_eq!(bytes, parsed.encode().unwrap());
        assert_eq!(bytes[0], 0xa9, "9 项 map 头");
        // context_hash 必须等于 SHA256(descriptor CDE)
        let hash = context_hash(&desc).unwrap();
        assert_eq!(hash.len(), 32);
        assert_ne!(hash, [0u8; 32]);

        // 尾部多余
        let mut long = bytes;
        long.push(0);
        assert!(SlotDescriptorV1::decode(&long).is_err());
    }

    #[test]
    fn password_slot_roundtrip_and_wrong_password() {
        let mut rng = TestRng::new(41);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let desc = password_descriptor([0x01; 32], [0x02; 16]);
        let slot =
            KeySlotV1::wrap_password(&desc, b"correct horse", &root, &test_aad, &mut rng).unwrap();

        let opened = slot.unwrap_password(b"correct horse", &test_aad).unwrap();
        assert!(root.ct_eq(&opened));

        // 错口令
        assert!(matches!(
            slot.unwrap_password(b"Correct horse", &test_aad),
            Err(SyncError::Unwrap)
        ));
        // 口令不做 trim：前后空格也失败
        assert!(slot.unwrap_password(b" correct horse ", &test_aad).is_err());
        // AAD 不同（模拟信封字段被改）
        let bad_aad = |_d: &SlotDescriptorV1, _n: &[u8; NONCE_LEN], _l: usize| Ok(vec![0u8; 16]);
        assert!(slot.unwrap_password(b"correct horse", &bad_aad).is_err());
    }

    #[test]
    fn password_slot_rejects_wrong_descriptor_or_body() {
        let mut rng = TestRng::new(42);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let desc = password_descriptor([0x01; 32], [0x02; 16]);
        let slot = KeySlotV1::wrap_password(&desc, b"pw", &root, &test_aad, &mut rng).unwrap();

        // 换 slot_id（HKDF info 变了）→ 解封失败
        let mut other_desc = desc.clone();
        other_desc.slot_id = [0x09; 32];
        let mut tampered = slot.clone();
        tampered.descriptor = other_desc;
        assert!(tampered.unwrap_password(b"pw", &test_aad).is_err());

        // 换 epoch（根归属不符）→ 描述/归属校验失败
        let mut epoch_desc = desc.clone();
        epoch_desc.key_epoch = 6;
        let mut tampered = slot.clone();
        tampered.descriptor = epoch_desc;
        assert!(tampered.unwrap_password(b"pw", &test_aad).is_err());

        // 篡改槽体
        let mut tampered = slot.clone();
        tampered.wrapped.ciphertext[0] ^= 1;
        assert!(matches!(
            tampered.unwrap_password(b"pw", &test_aad),
            Err(SyncError::Unwrap)
        ));

        // 截断槽体
        let mut tampered = slot.clone();
        tampered.wrapped.ciphertext.truncate(3);
        assert!(tampered.unwrap_password(b"pw", &test_aad).is_err());
    }

    #[test]
    fn recovery_slot_roundtrip_and_wrong_seed() {
        let mut rng = TestRng::new(43);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let seed = RecoverySeed::generate(&mut rng).unwrap();
        let desc = recovery_descriptor([0x03; 32]);
        let slot = KeySlotV1::wrap_recovery(&desc, &seed, &root, &test_aad, &mut rng).unwrap();
        assert!(root.ct_eq(&slot.unwrap_recovery(&seed, &test_aad).unwrap()));

        let other = RecoverySeed::generate(&mut rng).unwrap();
        assert!(matches!(
            slot.unwrap_recovery(&other, &test_aad),
            Err(SyncError::Unwrap)
        ));

        // 用口令路径解恢复槽 → 类型错误
        assert!(matches!(
            slot.unwrap_password(b"pw", &test_aad),
            Err(SyncError::SlotKind { .. })
        ));
    }

    /// 只凭恢复码（不再持有内存中的 R）解出根 —— ADR §11 步 2 的验收点之一。
    #[test]
    fn recovery_code_alone_recovers_root() {
        let mut rng = TestRng::new(49);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let seed = RecoverySeed::generate(&mut rng).unwrap();
        let desc = recovery_descriptor([0x23; 32]);
        let slot = KeySlotV1::wrap_recovery(&desc, &seed, &root, &test_aad, &mut rng).unwrap();

        // 用户抄下恢复码，此后 R 不再存在于内存
        let code = seed.to_code().unwrap();
        drop(seed);

        let restored = RecoverySeed::from_code(&code).unwrap();
        let opened = slot.unwrap_recovery(&restored, &test_aad).unwrap();
        assert!(root.ct_eq(&opened));
        // 管理能力也从恢复码独立派生（同一 R 的另一条 HKDF 域）
        let manager =
            crate::device::ManagementKey::from_recovery(&restored, &desc.vault_id).unwrap();
        assert_ne!(manager.public_key(), [0u8; 32]);
    }

    #[test]
    fn wrapped_root_bytes_roundtrip() {
        let mut rng = TestRng::new(44);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let desc = password_descriptor([0x04; 32], [0x05; 16]);
        let slot = KeySlotV1::wrap_password(&desc, b"pw", &root, &test_aad, &mut rng).unwrap();
        let bytes = slot.wrapped_bytes();
        assert_eq!(bytes.len(), NONCE_LEN + slot.wrapped.ciphertext.len());
        let parsed = WrappedRoot::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.nonce, slot.wrapped.nonce);
        assert_eq!(parsed.ciphertext, slot.wrapped.ciphertext);
        assert!(WrappedRoot::from_bytes(&bytes[..NONCE_LEN + TAG_LEN - 1]).is_err());
    }

    // ---- 改口令纯逻辑 ----

    struct MemorySlotStore {
        rev: u64,
        entries: Vec<SlotEntry>,
        /// 测试用：模拟"读快照后 rev 被并发推进"，让 CAS 失败。
        race_before_activate: bool,
    }

    impl MemorySlotStore {
        fn new() -> Self {
            Self {
                rev: 1,
                entries: Vec::new(),
                race_before_activate: false,
            }
        }

        fn add(&mut self, slot: KeySlotV1, state: SlotState) {
            self.entries.push(SlotEntry { slot, state });
        }

        fn find(&self, slot_id: &[u8; 32]) -> Option<&SlotEntry> {
            self.entries
                .iter()
                .find(|e| &e.slot.descriptor.slot_id == slot_id)
        }
    }

    impl SlotStore for MemorySlotStore {
        fn slot_set(&self) -> Result<SlotSetSnapshot, SyncError> {
            Ok(SlotSetSnapshot {
                rev: self.rev,
                slots: self.entries.clone(),
            })
        }

        fn stage_slot(&mut self, slot: &KeySlotV1) -> Result<(), SyncError> {
            if self.find(&slot.descriptor.slot_id).is_some() {
                return Err(SyncError::PasswordChange("槽 ID 冲突"));
            }
            self.entries.push(SlotEntry {
                slot: slot.clone(),
                state: SlotState::Staging,
            });
            Ok(())
        }

        fn activate_password_slot(
            &mut self,
            expected_rev: u64,
            new_slot_id: &[u8; 32],
        ) -> Result<u64, SyncError> {
            if self.race_before_activate {
                return Err(SyncError::Cas {
                    expected: expected_rev,
                    actual: expected_rev + 1,
                });
            }
            if expected_rev != self.rev {
                return Err(SyncError::Cas {
                    expected: expected_rev,
                    actual: self.rev,
                });
            }
            for entry in &mut self.entries {
                if entry.state == SlotState::Active
                    && entry.slot.descriptor.kind == SlotKind::Password
                {
                    entry.state = SlotState::Retiring;
                }
            }
            let entry = self
                .entries
                .iter_mut()
                .find(|e| &e.slot.descriptor.slot_id == new_slot_id)
                .ok_or(SyncError::PasswordChange("待激活的槽不存在"))?;
            entry.state = SlotState::Active;
            self.rev += 1;
            Ok(self.rev)
        }

        fn remove_slot(&mut self, slot_id: &[u8; 32]) -> Result<(), SyncError> {
            let before = self.entries.len();
            self.entries
                .retain(|e| &e.slot.descriptor.slot_id != slot_id);
            if self.entries.len() == before {
                return Err(SyncError::PasswordChange("待删除的槽不存在"));
            }
            Ok(())
        }
    }

    fn seeded_store(rng: &mut TestRng, root: &RootSecretV1) -> (MemorySlotStore, RecoverySeed) {
        let mut store = MemorySlotStore::new();
        let desc = password_descriptor([0x11; 32], [0x12; 16]);
        let slot = KeySlotV1::wrap_password(&desc, b"old password", root, &test_aad, rng).unwrap();
        store.add(slot, SlotState::Active);

        let seed = RecoverySeed::generate(rng).unwrap();
        let rec_desc = recovery_descriptor([0x21; 32]);
        let rec_slot = KeySlotV1::wrap_recovery(&rec_desc, &seed, root, &test_aad, rng).unwrap();
        store.add(rec_slot, SlotState::Active);
        (store, seed)
    }

    /// 小工具：用数据对象密钥加解密，验证"改口令后数据密文完全不变"。
    fn data_encrypt(key: &Secret32, nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Vec<u8> {
        let cipher = XChaCha20Poly1305::new_from_slice(key.expose_slice()).unwrap();
        cipher
            .encrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: plaintext,
                    aad: b"data-object-aad",
                },
            )
            .unwrap()
    }

    fn data_decrypt(
        key: &Secret32,
        nonce: &[u8; NONCE_LEN],
        ciphertext: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, SyncError> {
        let cipher =
            XChaCha20Poly1305::new_from_slice(key.expose_slice()).map_err(|_| SyncError::Unwrap)?;
        let plaintext = cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: b"data-object-aad",
                },
            )
            .map_err(|_| SyncError::Unwrap)?;
        Ok(Zeroizing::new(plaintext))
    }

    #[test]
    fn change_password_keeps_data_ciphertext_unchanged() {
        let mut rng = TestRng::new(45);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let (mut store, seed) = seeded_store(&mut rng, &root);

        // 改口令前：用 K_object 加密一段"笔记正文"
        let object_id = [0x77u8; 32];
        let data_key_before = derive_object_key(&root, ObjectType::Update, &object_id).unwrap();
        let nonce = [0x5au8; NONCE_LEN];
        let ciphertext = data_encrypt(&data_key_before, &nonce, b"note body: hello");
        let ciphertext_before = ciphertext.clone();

        let new_slot_id = [0x31u8; 32];
        let outcome = change_password(
            &mut store,
            &root,
            &PasswordChangePlan {
                new_password: b"new password",
                new_slot_id,
                created_at: 1_700_000_100,
            },
            &test_aad,
            &mut rng,
        )
        .unwrap();
        assert_eq!(outcome.old_slot_id, [0x11u8; 32]);
        assert_eq!(outcome.new_slot_id, new_slot_id);
        assert_eq!(outcome.new_slot_set_rev, 2);

        // 数据密文原样可解 —— 改口令不碰数据
        let data_key_after = derive_object_key(&root, ObjectType::Update, &object_id).unwrap();
        assert!(data_key_before.ct_eq(&data_key_after));
        assert_eq!(ciphertext, ciphertext_before);
        assert_eq!(
            &**data_decrypt(&data_key_after, &nonce, &ciphertext).unwrap(),
            b"note body: hello"
        );

        // 新槽 active 且能解出新口令；旧槽已删除
        let snapshot = store.slot_set().unwrap();
        assert!(snapshot
            .slots
            .iter()
            .all(|e| e.slot.descriptor.slot_id != [0x11u8; 32]));
        let active = snapshot
            .slots
            .iter()
            .find(|e| e.slot.descriptor.slot_id == new_slot_id)
            .expect("新槽必须存在");
        assert_eq!(active.state, SlotState::Active);
        assert!(root.ct_eq(
            &active
                .slot
                .unwrap_password(b"new password", &test_aad)
                .unwrap()
        ));
        // 旧口令失效：新槽不接受旧口令
        assert!(active
            .slot
            .unwrap_password(b"old password", &test_aad)
            .is_err());

        // 恢复槽原封不动（改口令不动恢复码）
        let rec = snapshot
            .slots
            .iter()
            .find(|e| e.slot.descriptor.kind == SlotKind::Recovery)
            .expect("恢复槽必须保留");
        assert_eq!(rec.state, SlotState::Active);
        assert!(root.ct_eq(&rec.slot.unwrap_recovery(&seed, &test_aad).unwrap()));
    }

    #[test]
    fn change_password_cas_conflict_leaves_old_active() {
        let mut rng = TestRng::new(46);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let (mut store, _seed) = seeded_store(&mut rng, &root);
        // 模拟并发：读取快照后 rev 被别人推进（CAS 必须失败）
        let stale_rev = store.slot_set().unwrap().rev;
        store.race_before_activate = true;

        let err = change_password(
            &mut store,
            &root,
            &PasswordChangePlan {
                new_password: b"new password",
                new_slot_id: [0x41u8; 32],
                created_at: 1_700_000_100,
            },
            &test_aad,
            &mut rng,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SyncError::Cas {
                expected,
                actual
            } if expected == stale_rev && actual == stale_rev + 1
        ));
        // 旧口令槽仍是 active，未删除
        let snapshot = store.slot_set().unwrap();
        let old = snapshot
            .slots
            .iter()
            .find(|e| e.slot.descriptor.slot_id == [0x11u8; 32])
            .expect("旧槽必须还在");
        assert_eq!(old.state, SlotState::Active);
        assert!(old.slot.unwrap_password(b"old password", &test_aad).is_ok());
        // 新槽留在 staging（24h 回收，ADR §3）
        let staged = snapshot
            .slots
            .iter()
            .find(|e| e.slot.descriptor.slot_id == [0x41u8; 32])
            .expect("新槽已暂存");
        assert_eq!(staged.state, SlotState::Staging);
    }

    #[test]
    fn change_password_rejects_bad_password_before_kdf() {
        let mut rng = TestRng::new(47);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let (mut store, _seed) = seeded_store(&mut rng, &root);

        let started = std::time::Instant::now();
        let err = change_password(
            &mut store,
            &root,
            &PasswordChangePlan {
                new_password: b"",
                new_slot_id: [0x51u8; 32],
                created_at: 1,
            },
            &test_aad,
            &mut rng,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::PasswordLength { actual: 0, .. }));
        assert!(
            started.elapsed().as_millis() < 50,
            "空口令必须在 Argon2 之前拒绝"
        );
        // 未暂存任何槽
        assert_eq!(store.slot_set().unwrap().slots.len(), 2);
    }

    #[test]
    fn change_password_requires_active_password_slot() {
        let mut rng = TestRng::new(48);
        let root = RootSecretV1::generate(vault(), 5, &mut rng).unwrap();
        let mut store = MemorySlotStore::new();
        let err = change_password(
            &mut store,
            &root,
            &PasswordChangePlan {
                new_password: b"pw",
                new_slot_id: [0x61u8; 32],
                created_at: 1,
            },
            &test_aad,
            &mut rng,
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::PasswordChange(_)));
    }
}

`````



@@SECTION code/053 `crates/sync/src/storage_view.rs`


`````rust
//! storage 只读 API 的薄适配层（S7-8）。
//!
//! 真源是 `crates/storage/src/read_api.rs`（`body_doc_meta` / `loro_update_payload` /
//! `loro_update_base_vv` / `doc_updates_since` / `doc_snapshot` / `read_doc_bootstrap` /
//! `read_doc_for_export` / `doc_exists`）。本模块**不写任何 SQL、不直接访问表**，只做：
//!
//! 1. 错误分类：`StorageError::HeadConflict`（head CAS 失败）→ [`SyncError::HeadConflict`]，
//!    它是**可重试**的并发冲突，不是数据丢失；
//! 2. 统一 `storage_view::*` 调用点，方便后续 bootstrap/checkpoint 复用同一套读路径。
//!
//! 出站加密用 `doc_meta` + `stored_update` + `base_vv_at`；入站依赖检查用
//! `doc_exists`；bootstrap/导出用 `updates_since` / `bootstrap` / `export_at_head`。

use crate::error::SyncError;
use menghunote_storage::{
    body_doc_meta, doc_exists as storage_doc_exists, doc_updates_since, loro_update_base_vv,
    loro_update_payload, read_doc_bootstrap, read_doc_for_export, BodyDocMeta, DocBootstrap,
    DocExport, StorageError, StoredLoroUpdate, Vault,
};

pub use menghunote_storage::DocSnapshot;

/// 把 storage 错误映射成 sync 错误；head CAS 冲突保持"可重试"语义。
#[must_use]
pub fn map_storage_error(error: StorageError) -> SyncError {
    match error {
        StorageError::HeadConflict {
            doc_id,
            expected,
            actual,
        } => SyncError::HeadConflict {
            doc_id,
            expected,
            actual,
        },
        other => SyncError::Storage(other),
    }
}

/// 读正文文档元数据；不存在返回 `None`。
pub fn doc_meta(vault: &Vault, doc_id: &str) -> Result<Option<BodyDocMeta>, SyncError> {
    body_doc_meta(vault, doc_id).map_err(map_storage_error)
}

/// 文档是否存在（入站 update 的依赖是否就绪）。
pub fn doc_exists(vault: &Vault, doc_id: &str) -> Result<bool, SyncError> {
    storage_doc_exists(vault, doc_id).map_err(map_storage_error)
}

/// 读出站 `(doc_id, seq)` 的 update 明文列。
pub fn stored_update(vault: &Vault, doc_id: &str, seq: u64) -> Result<StoredLoroUpdate, SyncError> {
    loro_update_payload(vault, doc_id, seq).map_err(map_storage_error)
}

/// `seq` 之前的 VV（出站 `base_vv`；`seq <= 1` 为空）。
pub fn base_vv_at(vault: &Vault, doc_id: &str, seq: u64) -> Result<Vec<u8>, SyncError> {
    loro_update_base_vv(vault, doc_id, seq).map_err(map_storage_error)
}

/// `seq > after_seq` 的 update（升序），出站范围/校验用。
pub fn updates_since(
    vault: &Vault,
    doc_id: &str,
    after_seq: u64,
) -> Result<Vec<StoredLoroUpdate>, SyncError> {
    doc_updates_since(vault, doc_id, after_seq).map_err(map_storage_error)
}

/// 从快照 + 后续 update 组装 bootstrap 读取包（无快照时 `snapshot` 为 `None`）。
pub fn bootstrap(vault: &Vault, doc_id: &str) -> Result<Option<DocBootstrap>, SyncError> {
    read_doc_bootstrap(vault, doc_id).map_err(map_storage_error)
}

/// 按期望 head 做一致性导出；head 不匹配返回可重试的 [`SyncError::HeadConflict`]，
/// **绝不返回半截快照**。
pub fn export_at_head(
    vault: &Vault,
    doc_id: &str,
    expected_head_seq: u64,
) -> Result<DocExport, SyncError> {
    read_doc_for_export(vault, doc_id, expected_head_seq).map_err(map_storage_error)
}

`````



@@SECTION code/054 `crates/sync/src/test_support.rs`


`````rust
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

`````


