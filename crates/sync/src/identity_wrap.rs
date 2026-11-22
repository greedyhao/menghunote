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
