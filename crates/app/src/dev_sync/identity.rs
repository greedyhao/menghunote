//! dev-sync 的**持久设备身份**（ADR 0009 切片 2a）。
//!
//! ## 为什么需要它
//!
//! 两个 dev-sync 实例同时开编辑器时，CRDT 的 peer 必须**每个实例不同且重启不变**：
//! 常量 `1` 会让两个实例的 op id 撞车，随机 peer 又会在重启后把同一个库当成新设备。
//! 所以 dev-sync 的身份落盘在 vault 同目录，peer 由
//! [`LocalIdentityV1::loro_peer_id`] 从身份**确定性**映射出来（唯一来源，见
//! `crates/sync/src/identity.rs`）。
//!
//! ## 文件格式（本地、dev-only、**不上 wire**）
//!
//! ```text
//! path = <vault 同目录>/dev-identity.bin
//! magic   = b"MNID" (4B)
//! version = 1      (u8)
//! vault_id   (16B) | device_id (16B) | protected_len (u32 BE)
//! protected = identity_wrap::protect(LocalIdentityV1) 的输出（DPAPI / Linux MNKW）
//! ```
//!
//! **header 只放非秘密 id**（`vault_id` / `device_id` 都是公开标识）：它们是
//! `identity_wrap` 的 entropy 输入（`b"MN/dpapi/v1\0" || vault_id || device_id`），
//! 因此必须能在解封**之前**读出来。密文本身是 AEAD 认证过的，header 的任何改动都会
//! 让解封失败——header 不需要额外签名，但 `protected_len` 是唯一未认证字段，解析时
//! 要求它**精确等于**剩余字节数（尾随/截断都直接拒绝）。
//!
//! ## 落盘纪律
//!
//! - 私有临时文件（0600）→ write → fsync → **不覆盖已有文件**的原子发布（`hard_link`，
//!   目标已存在时原子失败）→ 父目录 fsync；失败路径清掉临时文件，不留半文件；
//! - 已存在的文件**只读**：损坏/不匹配一律明确报错，**不覆盖、不迁移、不删除**；
//! - 日志只打路径与"已存在/已创建"，不打印任何私钥、root、恢复码（本文件里也没有
//!   恢复码：dev identity 没有恢复码备份）。
//!
//! ## 平台
//!
//! Windows 走 DPAPI（CurrentUser），Linux 走用户主键 + `MNKW` 外层格式（ADR 0008）；
//! 两者都先经 `identity_wrap`，本模块**不复制任何 crypto**。Linux 首次调用前必须
//! [`identity_wrap::initialize_user_key`]（失败带上下文、不静默回退）；其它平台明确报错。
//!
//! ## 已知边界（切片 2b / 4 之前）
//!
//! 当前 dev vault 里**不写** vault 身份（ADR 0009：不写 vault meta 身份），所以只能在
//! "header ↔ 解封出的 identity"层面校验一致性；把整个 `dev-identity.bin` 从另一个库
//! 目录复制过来**无法**自动识别（ADR 0009 明确要求两个实例各自独立、不得复制身份）。

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context as _, Result};
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::identity_wrap;
use menghunote_sync::random::{array, OsRandom};
use menghunote_sync::{DeviceId, VaultId};

/// 身份文件名（与 vault 文件同目录）。
pub const IDENTITY_FILE_NAME: &str = "dev-identity.bin";
/// 文件头 magic（"MenghuNote Dev Identity"）。
const MAGIC: [u8; 4] = *b"MNID";
/// 文件格式版本。
const FORMAT_VERSION: u8 = 1;
/// 明文 header 长度 = magic4 + version1 + vault16 + device16 + len4。
const HEADER_LEN: usize = 4 + 1 + 16 + 16 + 4;
/// 受保护 BLOB 的合理上限（防一个坏长度字段让调用方读进几百 MB）。
const MAX_PROTECTED_LEN: usize = 1 << 20;

/// dev-sync 的设备身份存储。
///
/// 无状态：所有信息都在 `<vault 同目录>/dev-identity.bin` 里。启动路径用
/// [`Self::load`] 只读复用；新身份由 Create/Join 经 [`Self::persist_protected`] 落盘。
pub struct DevIdentityStore;

impl DevIdentityStore {
    /// 身份文件路径（`<vault 同目录>/dev-identity.bin`）。
    #[must_use]
    pub fn path_for(vault_path: &Path) -> PathBuf {
        vault_dir(vault_path).join(IDENTITY_FILE_NAME)
    }

    /// 读取已有身份（**绝不创建**；文件缺失/损坏都明确报错）。
    ///
    /// 与 [`Self::load_or_create`] 的区别：这里没有"不存在就新建"的分支，供切片 4
    /// Create/Join 之后回读、以及测试断言"失败路径没有凭空造身份"。
    pub fn load(&self, vault_path: &Path) -> Result<LocalIdentityV1> {
        ensure_supported()?;
        identity_wrap::initialize_user_key()
            .context("初始化本地身份封装主键失败（Linux 见 ADR 0008；Windows 为 DPAPI）")?;
        let path = Self::path_for(vault_path);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("dev identity 文件不存在或不可读：{}", path.display()))?;
        reject_non_regular(&metadata, &path)?;
        load_file(&path)
    }

    /// 读取已有身份；不存在则创建并原子发布（**仅测试夹具/非启动路径**）。
    ///
    /// - 已存在：读 header → `unprotect` → 校验 header 与 identity 一致（`unprotect` 内
    ///   已按 entropy + 字段双重校验），任何损坏都明确报错且**不改文件**；
    /// - 不存在：生成新的 `LocalIdentityV1`（OS 随机 peer/seed/x25519/root）→ 原子落盘。
    ///
    /// 返回的身份只用于取 peer（[`LocalIdentityV1::loro_peer_id`]）与测试夹具。
    ///
    /// **切片 4c-1 起，app 启动路径不再调用本函数**：普通 `--dev-sync` 只 [`Self::load`]，
    /// 新身份只能由 Create/Join（`dev_sync::setup`）产生（ADR 0009）。保留它是因为现有
    /// 集成测试用它构造"已有实例"夹具；生产代码不应再出现 `load_or_create`。
    ///
    /// **旧/非空库**：测试夹具调用前也应先调 [`ensure_instance_vault_ready`]，避免给一个
    /// 已有内容的旧开发库凭空造出新身份（ADR 0009「只接受修复后新库」）。
    pub fn load_or_create(vault_path: &Path) -> Result<LocalIdentityV1> {
        ensure_supported()?;
        // Linux 必须先有每用户主键（Windows 是 no-op）；失败带上下文，不回退明文。
        identity_wrap::initialize_user_key()
            .context("初始化本地身份封装主键失败（Linux 见 ADR 0008；Windows 为 DPAPI）")?;

        let dir = vault_dir(vault_path);
        fs::create_dir_all(&dir)
            .with_context(|| format!("创建 dev identity 目录失败：{}", dir.display()))?;
        let path = dir.join(IDENTITY_FILE_NAME);

        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                reject_non_regular(&metadata, &path)?;
                let identity = load_file(&path)?;
                eprintln!(
                    "[menghunote] dev identity 已存在：{}（复用已有持久 peer）",
                    path.display()
                );
                Ok(identity)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let identity = create_file(&dir, &path)?;
                eprintln!(
                    "[menghunote] dev identity 已创建：{}（DEV ONLY：dev identity，无恢复码备份）",
                    path.display()
                );
                Ok(identity)
            }
            Err(error) => Err(error)
                .with_context(|| format!("读取 dev identity 文件状态失败：{}", path.display())),
        }
    }

    /// 原子**替换**已有身份（切片 4：Create/Join 后把身份换成真实设备身份）。
    ///
    /// 语义（ADR 0009「身份保护写盘」）：
    ///
    /// - 私有临时文件（0600）→ write → fsync → `rename` **原子替换** → 父目录 fsync；
    /// - 任何一步失败都**不破坏旧文件**（临时名先被清掉，旧文件原样保留）；
    /// - 目标若不是常规文件（符号链接/目录）→ 明确拒绝，不做隐式跟随；
    /// - 只落盘 `protect()` 的密文与公开 header，不打印任何私钥。
    pub fn persist_protected(&self, vault_path: &Path, identity: &LocalIdentityV1) -> Result<()> {
        ensure_supported()?;
        identity_wrap::initialize_user_key()
            .context("初始化本地身份封装主键失败（Linux 见 ADR 0008；Windows 为 DPAPI）")?;

        let dir = vault_dir(vault_path);
        fs::create_dir_all(&dir)
            .with_context(|| format!("创建 dev identity 目录失败：{}", dir.display()))?;
        let path = dir.join(IDENTITY_FILE_NAME);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            reject_non_regular(&metadata, &path)?;
        }

        let protected = identity
            .protect()
            .context("封装 dev 身份失败（本地主键/DPAPI 不可用）")?;
        let header = encode_header(&identity.vault_id, &identity.device_id, protected.len())?;
        let mut bytes = header;
        bytes.extend_from_slice(&protected);
        publish_replace(&dir, &path, &bytes)
    }

    /// 原子**替换**已有身份，且只允许**同 vault / 同 device**（ADR 0009 Phase 2 切片 6a）。
    ///
    /// 与 [`Self::persist_protected`] 的差别（不是重复实现，是更窄的契约）：
    ///
    /// - 调用方给的是**身份文件路径**（不是 vault 路径），因为轮换/apply 的提交点是
    ///   某个具体文件；
    /// - 目标**必须已存在**且是常规文件：解析 MNID header 后要求
    ///   `vault_id`/`device_id` 与传入身份**完全一致**，否则明确拒绝（不换设备/不换库）；
    /// - 加密与发布沿用同一套：`identity_wrap::protect`（header 里的 id 就是 entropy 输入）
    ///   → 0600 临时文件 → write → fsync → 原子 `rename` → 父目录 fsync；
    /// - 任意一步失败都**不触碰旧文件**（所有写入发生在临时名上，`rename` 是唯一提交点）。
    ///
    /// 轮换/apply 成功后由 worker 调用；失败时旧身份逐字节保留，内存身份也不更新。
    pub fn replace_protected(&self, path: &Path, identity: &LocalIdentityV1) -> Result<()> {
        ensure_supported()?;
        identity_wrap::initialize_user_key()
            .context("初始化本地身份封装主键失败（Linux 见 ADR 0008；Windows 为 DPAPI）")?;

        // 1) 目标必须已存在（替换语义，不负责创建）且是常规文件。
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("待替换的 dev identity 不存在或不可读：{}", path.display()))?;
        reject_non_regular(&metadata, path)?;

        // 2) header 的 vault/device 必须与传入身份一致：轮换/apply 只推进 epoch/root/pin，
        //    永远不换设备、不换库（换了就是另一份身份，必须走 Create/Join）。
        let existing = fs::read(path)
            .with_context(|| format!("读取待替换的 dev identity 失败：{}", path.display()))?;
        {
            let (vault_id, device_id, _protected) = parse_header(&existing, path)?;
            ensure!(
                vault_id == identity.vault_id && device_id == identity.device_id,
                "拒绝替换不同 vault/device 的 dev identity：文件里是 {}/{}，传入的是 {}/{}；\
                 轮换/apply 不得更换设备或库（不覆盖、不迁移）",
                menghunote_sync::ids::hex_of(&vault_id),
                menghunote_sync::ids::hex_of(&device_id),
                menghunote_sync::ids::hex_of(&identity.vault_id),
                menghunote_sync::ids::hex_of(&identity.device_id)
            );
        }
        drop(existing);

        // 3) 同一个 MNID 外层：header 只放非秘密 id，密文由 identity_wrap 生成。
        let protected = identity
            .protect()
            .context("封装轮换后的 dev 身份失败（本地主键/DPAPI 不可用）")?;
        let header = encode_header(&identity.vault_id, &identity.device_id, protected.len())?;
        let mut bytes = header;
        bytes.extend_from_slice(&protected);
        let dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        publish_replace(dir, path, &bytes)
    }
}

/// 旧/非空开发库拒绝（ADR 0009「只接受修复后新库」，切片 2a 遗留项）。
///
/// 判据（**身份文件是否存在**是分界线）：
///
/// - 身份已存在 → `Ok(())`：沿用 2a 语义，库损坏/不匹配由后续 `open` / `load` 明确报错；
/// - 身份不存在 → 只允许"文件不存在"或"空库"：库里已有任何 `loro_updates` / `tree` /
///   `notes` 行时明确拒绝，**不新建身份、不迁移、不删除**。
///
/// 探测走只读连接（`existing_vault_has_content`）：不迁移、不建库，避免在"决定拒绝"之前
/// 就改写了旧库。探测本身失败（不是 SQLite / WAL 需要恢复）时**fail-closed**：宁可让
/// 用户换新目录，也不给来历不明的库造身份。
pub fn ensure_instance_vault_ready(vault_path: &Path) -> Result<()> {
    let identity_path = vault_dir(vault_path).join(IDENTITY_FILE_NAME);
    match fs::symlink_metadata(&identity_path) {
        // 已有身份：这是切片 2a 就存在的实例，旧库拒绝的判断不再适用。
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let has_content = menghunote_storage::existing_vault_has_content(vault_path)
                .with_context(|| {
                    format!(
                        "无法确认开发库是否为空（不迁移、不删除，请改用新目录）：{}",
                        vault_path.display()
                    )
                })?;
            if has_content {
                bail!(
                    "旧/非空开发库不得启用 dev-sync：{} 已有内容但没有 {}；\
                     请用**新目录**（例如 MENGHUNOTE_DEV_VAULT=target/tmp/menghunote-dev/inst-a/vault.sqlite3）\
                     重新初始化，不迁移、不覆盖、不删除旧库",
                    vault_path.display(),
                    identity_path.display()
                );
            }
            Ok(())
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "读取 dev identity 文件状态失败：{}",
                identity_path.display()
            )
        }),
    }
}

/// 本平台是否支持本地身份封装（明确报错，不静默降级）。
fn ensure_supported() -> Result<()> {
    ensure!(
        identity_wrap::is_available(),
        "dev-sync 的本地身份封装只在 Windows（DPAPI）/ Linux（用户主键，ADR 0008）可用；\
         本平台明确不支持，不回退明文"
    );
    Ok(())
}

/// vault 文件所在目录（相对文件名退化为当前目录，与 `open_vault_at` 的语义一致）。
fn vault_dir(vault_path: &Path) -> PathBuf {
    match vault_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// 拒绝符号链接/目录/设备等非普通文件：身份文件必须是常规文件。
fn reject_non_regular(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    ensure!(
        metadata.file_type().is_file(),
        "dev identity 路径不是常规文件（拒绝符号链接/目录/设备）：{}",
        path.display()
    );
    Ok(())
}

/// 读已有身份：header → 解封 → 一致性校验。**只读**，失败不改文件。
fn load_file(path: &Path) -> Result<LocalIdentityV1> {
    let bytes = fs::read(path)
        .with_context(|| format!("读取 dev identity 文件失败：{}", path.display()))?;
    let (vault_id, device_id, protected) = parse_header(&bytes, path)?;
    let identity =
        LocalIdentityV1::unprotect(protected, &vault_id, &device_id).with_context(|| {
            format!(
                "解封 dev identity 失败：{}（文件损坏、用户主键不匹配或来自其它用户/设备；\
             按 ADR 0009 不覆盖、不迁移、不删除，请人工确认后再处理）",
                path.display()
            )
        })?;
    // `unprotect` 已校验 vault/device 与 entropy 一致；这里再显式比一次 header，
    // 让"header 被改坏"与"密文被改坏"在错误信息上可区分。
    ensure!(
        identity.vault_id == vault_id && identity.device_id == device_id,
        "dev identity 的 header 与解封内容不一致：{}",
        path.display()
    );
    Ok(identity)
}

/// 解析明文 header，返回 `(vault_id, device_id, protected_blob)`。
///
/// 错误信息只带偏移/原因与路径，不打印文件内容（里面是密文，仍按敏感数据对待）。
fn parse_header<'a>(bytes: &'a [u8], path: &Path) -> Result<(VaultId, DeviceId, &'a [u8])> {
    ensure!(
        bytes.len() >= HEADER_LEN,
        "dev identity 文件长度不足 {HEADER_LEN}B 的明文 header：{}（实际 {}B）",
        path.display(),
        bytes.len()
    );
    ensure!(
        bytes[..4] == MAGIC,
        "dev identity 文件 magic 不符（不是 MNID 格式）：{}",
        path.display()
    );
    let version = bytes[4];
    ensure!(
        version == FORMAT_VERSION,
        "dev identity 文件版本不支持：v{version}（本程序只支持 v{FORMAT_VERSION}）：{}",
        path.display()
    );
    let mut vault_id: VaultId = [0u8; 16];
    vault_id.copy_from_slice(&bytes[5..21]);
    let mut device_id: DeviceId = [0u8; 16];
    device_id.copy_from_slice(&bytes[21..37]);
    let declared =
        u32::from_be_bytes(bytes[37..41].try_into().expect("4 字节切片转数组不会失败")) as usize;
    let protected = &bytes[HEADER_LEN..];
    ensure!(
        (HEADER_LEN + declared) == bytes.len(),
        "dev identity 长度字段与实际不符：声明 {declared}B，实际 {}B（文件被截断或尾随了多余字节）：{}",
        protected.len(),
        path.display()
    );
    ensure!(
        (1..=MAX_PROTECTED_LEN).contains(&declared),
        "dev identity 受保护 BLOB 长度非法：{declared}B：{}",
        path.display()
    );
    Ok((vault_id, device_id, protected))
}

/// 生成新身份并原子落盘。
fn create_file(dir: &Path, path: &Path) -> Result<LocalIdentityV1> {
    let vault_id: VaultId =
        array(&mut OsRandom).context("生成 dev vault_id 失败（OS 随机源不可用）")?;
    let device_id: DeviceId =
        array(&mut OsRandom).context("生成 dev device_id 失败（OS 随机源不可用）")?;
    let identity = LocalIdentityV1::generate_dev(vault_id, device_id)
        .context("生成 dev 身份失败（OS 随机源不可用）")?;
    let protected = identity
        .protect()
        .context("封装 dev 身份失败（本地主键/DPAPI 不可用）")?;
    let header = encode_header(&vault_id, &device_id, protected.len())?;
    let mut bytes = header;
    bytes.extend_from_slice(&protected);
    publish_new(dir, path, &bytes)?;
    Ok(identity)
}

/// 编码明文 header（只含非秘密 id 与密文长度）。
fn encode_header(
    vault_id: &VaultId,
    device_id: &DeviceId,
    protected_len: usize,
) -> Result<Vec<u8>> {
    let declared = u32::try_from(protected_len).context("dev identity 密文长度超出 u32")?;
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(&MAGIC);
    header.push(FORMAT_VERSION);
    header.extend_from_slice(vault_id);
    header.extend_from_slice(device_id);
    header.extend_from_slice(&declared.to_be_bytes());
    Ok(header)
}

/// 私有临时文件（0600）→ write → fsync → 原子 no-replace 发布 → 父目录 fsync。
///
/// 发布用 `hard_link`：目标已存在时**原子失败**，绝不覆盖已有身份（也不用
/// `rename` 的覆盖语义）。临时名带 pid + 随机后缀，失败时清掉。
fn publish_new(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let suffix: [u8; 8] = array(&mut OsRandom).context("生成 dev identity 临时名失败")?;
    let tmp = dir.join(format!(
        ".{IDENTITY_FILE_NAME}.tmp.{}.{}",
        std::process::id(),
        menghunote_sync::ids::hex_of(&suffix)
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // 私有临时文件：身份密文只有当前用户可读。
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("创建 dev identity 临时文件失败：{}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("写入 dev identity 失败：{}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync dev identity 失败：{}", tmp.display()))?;
        drop(file);
        match fs::hard_link(&tmp, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => bail!(
                "dev identity 文件已存在，拒绝覆盖：{}（并发创建或旧实例残留，\
                 请人工确认；不覆盖、不迁移、不删除）",
                path.display()
            ),
            Err(error) => Err(error).with_context(|| {
                format!(
                    "原子发布 dev identity 失败：{} → {}（需要支持硬链接的文件系统）",
                    tmp.display(),
                    path.display()
                )
            }),
        }
    })();
    if result.is_err() {
        // 失败不留半文件；已发布的 final 文件不在此列（此时 result 是 Ok）。
        let _ = fs::remove_file(&tmp);
    }
    result?;
    // 发布成功：临时名只是同一 inode 的第二个名字，删掉它让 nlink 回到 1。
    let _ = fs::remove_file(&tmp);
    fsync_dir(dir)
}

/// 私有临时文件（0600）→ write → fsync → `rename` **原子替换** → 父目录 fsync。
///
/// 与 [`publish_new`] 的区别：这里允许覆盖已有身份（切片 4 Create/Join 之后换身份），
/// 但**失败不会破坏旧文件**：所有写入都发生在临时名上，只有 `rename` 成功那一刻旧
/// 文件才被替换；`rename` 之前的任何失败都只清掉临时文件。
fn publish_replace(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    let suffix: [u8; 8] = array(&mut OsRandom).context("生成 dev identity 临时名失败")?;
    let tmp = dir.join(format!(
        ".{IDENTITY_FILE_NAME}.replace.{}.{}",
        std::process::id(),
        menghunote_sync::ids::hex_of(&suffix)
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // 私有临时文件：身份密文只有当前用户可读。
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("创建 dev identity 临时文件失败：{}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("写入 dev identity 失败：{}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync dev identity 失败：{}", tmp.display()))?;
        drop(file);
        fs::rename(&tmp, path).with_context(|| {
            format!(
                "原子替换 dev identity 失败：{} → {}（旧文件保持不变）",
                tmp.display(),
                path.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        // 失败不留半文件；旧身份文件原样保留。
        let _ = fs::remove_file(&tmp);
    }
    result?;
    fsync_dir(dir)
}

/// fsync 父目录，使新发布的目录项持久化。
#[cfg(unix)]
fn fsync_dir(dir: &Path) -> Result<()> {
    let handle =
        File::open(dir).with_context(|| format!("打开目录失败（fsync 用）：{}", dir.display()))?;
    handle
        .sync_all()
        .with_context(|| format!("fsync 目录失败：{}", dir.display()))
}

/// Windows 的 std 不提供目录 fsync（`File::open` 打不开目录）；发布已由
/// `CreateHardLinkW` 的元数据语义保证，这里明确 no-op。
#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}
