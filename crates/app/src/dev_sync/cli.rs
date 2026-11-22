//! dev-sync 的**手工演示交付**（ADR 0009 Phase 1 / 切片 4c-3）：一次性恢复码块 +
//! `--dev-sync-info` + `--dev-sync-publish`。
//!
//! ## 为什么单独一层
//!
//! Create/Join 已经能跑通（切片 4c-1），但恢复码在 `prepare` 返回后就被 `main` drop 了：
//! 操作者永远看不到它，也无法把 `vault_id + 恢复码` 交给 B。本模块补上"人能看到"的最后
//! 一段，并把两个不开窗的查询/发布动作做成**可测的库函数**（`main` 只负责 println）。
//!
//! | 函数 | 作用 | 网络 | 开窗 |
//! | --- | --- | --- | --- |
//! | [`create_handoff_block`] | Create 成功后的 DEV ONLY 交接块（含恢复码，**只打印一次**） | 否 | 否 |
//! | [`inspect`] + [`info_block`] | `--dev-sync-info`：读身份/库，打印公开标识 | 否 | 否 |
//! | [`publish_baseline`] + [`publish_block`] | `--dev-sync-publish`：发布初始基线 | 是 | 否 |
//!
//! ## 安全
//!
//! - **恢复码只出现在 [`create_handoff_block`] 的返回值里**：本模块不写日志/配置/meta，
//!   也不把它放进任何 `Debug`（[`InstanceInfo`] / [`PublishSummary`] 只有公开事实）；
//! - [`inspect`] 不打印恢复码/root/secret；`--dev-sync-info` 也**不联网**；
//! - [`publish_baseline`] 的恢复码只从环境变量读（argv 会被 shell history/进程列表
//!   记录），错误文案只报变量名，不回显内容；
//! - 发布先做"恢复码 ↔ 管理身份"的本地一致性校验，**在打开库、发任何请求之前**失败：
//!   缺码/错码不会碰库文件，也不会向 relay 上传半成品。
//!
//! ## 已知限制（与 4b/4c-2 一致）
//!
//! - 发布要求 app 已关闭：CLI 与 GUI 同时打开同一库会出现并发写（SQLite WAL 下不一定
//!   立刻报错，但 head CAS 可能冲突）。本模块**不自动重试**；
//! - 重复发布沿用 4b 语义：允许重跑（幂等），但每跑一次会多一对真实 checkpoint；
//! - Create/Join/Publish 仍是 CLI，面板按钮留到后续切片。

use anyhow::{bail, Context as _};
use menghunote_storage::{list_tree, Vault};
use menghunote_sync::dev_local::DevLocal;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::ids;
use menghunote_sync::recovery::{management_seed, RecoverySeed};
use menghunote_sync::{DeviceId, VaultId};

use super::gate::{DevSyncConfig, PUBLISH_FLAG};
use super::identity::DevIdentityStore;
use super::setup::{
    prepare, required_secret, SetupError, SetupMode, SetupSecrets, RECOVERY_CODE_ENV,
};

/// `--dev-sync-info` 的结论：全部是**公开/可公开**标识，不含任何秘密。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceInfo {
    /// 本实例所属 vault id（Join 时操作者要输入的那串十六进制）。
    pub vault_id: VaultId,
    /// 本设备 id（relay 管理端的设备标识）。
    pub device_id: DeviceId,
    /// 本设备在 Loro/CRDT 里的 peer id（`LocalIdentityV1::loro_peer_id()`）。
    pub peer: u64,
}

/// `--dev-sync-publish` 的结论：checkpoint 的公开事实 + 本次覆盖的文档数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishSummary {
    /// 较新那份 checkpoint 的 id（`/bootstrap` 只认最新两份 published）。
    pub checkpoint_id: [u8; 16],
    /// 该 checkpoint 的 epoch。
    pub epoch: u64,
    /// 该 checkpoint 固定的 relay 水位 S。
    pub covered_seq: u64,
    /// relay 记录的发布时间（Unix 秒，公开事实）。
    pub published_at: u64,
    /// 发布时本地结构投影里的文档（笔记）数。
    pub notes: usize,
}

/// Create 成功后的**一次性**交接块（DEV ONLY）。
///
/// 调用方必须在开窗前把它打到 stdout **恰好一次**；不要写进日志/文件/配置。格式是给人
/// 复制粘贴的：`vault_id` 直接用于 B 侧 `MENGHUNOTE_DEV_VAULT_ID`，恢复码用于
/// `MENGHUNOTE_DEV_RECOVERY_CODE`。
#[must_use]
pub fn create_handoff_block(vault_id: &VaultId, recovery_code: &str) -> String {
    format!(
        "[DEV ONLY] vault_id={}\n\
         [DEV ONLY] recovery_code={recovery_code}\n\
         [DEV ONLY] 上面的恢复码只显示一次；B 侧 Join 用 vault_id + 恢复码。关闭终端后不再可恢复。",
        ids::hex_of(vault_id)
    )
}

/// `--dev-sync-info` 的实现：**只读**身份与库，不联网、不创建、不打印秘密。
///
/// 身份缺失 → 明确错误（不自动创建）；库文件缺失 → 明确错误（`Vault::open` 会新建空库，
/// info 不能有这种副作用，所以先判存在再打开）。
pub fn inspect(config: &DevSyncConfig) -> Result<InstanceInfo, SetupError> {
    let identity = DevIdentityStore.load(&config.vault).with_context(|| {
        format!(
            "读取实例身份失败：{}（info 只查询已有实例，不创建；新实例请用 --dev-sync-create）",
            DevIdentityStore::path_for(&config.vault).display()
        )
    })?;
    require_existing_vault(config)?;
    // 真正打开一次库，证明"这个实例是可用的"（不是只看文件在不在）。
    let vault = Vault::open(&config.vault)
        .with_context(|| format!("打开实例库失败：{}", config.vault.display()))?;
    list_tree(&vault).with_context(|| format!("读取结构投影失败：{}", config.vault.display()))?;
    Ok(InstanceInfo {
        vault_id: identity.vault_id,
        device_id: identity.device_id,
        peer: identity.loro_peer_id(),
    })
}

/// `--dev-sync-info` 的输出（DEV ONLY；全部是公开标识）。
#[must_use]
pub fn info_block(info: &InstanceInfo) -> String {
    format!(
        "[DEV ONLY] vault_id={}\n\
         [DEV ONLY] device_id={}\n\
         [DEV ONLY] peer={}\n\
         [DEV ONLY] 以上都是公开标识；恢复码不在此显示、也不落盘（Create 时只打印一次）",
        ids::hex_of(&info.vault_id),
        ids::hex_of(&info.device_id),
        info.peer
    )
}

/// `--dev-sync-publish` 的实现：对**已有实例**发布初始基线，返回公开事实。
///
/// 顺序（失败语义就是顺序本身）：
///
/// 1. 恢复码只从 env 读；缺失 → 点名 `MENGHUNOTE_DEV_RECOVERY_CODE`，不碰任何东西；
/// 2. 只读身份 + 本地校验恢复码 ↔ 管理身份：错码/非管理设备在**打开库、发请求之前**
///    失败，库文件零改动；
/// 3. 打开同一库（要求 app 已关闭）→ `DevLocal::publish_initial_pair`
///    （上传初始槽 → 一轮 sync → 两份真实 checkpoint，4b 幂等）；
/// 4. 返回 checkpoint id / epoch / 覆盖 seq / 文档数（都是公开事实）。
///
/// 失败时进程以非 0 退出（由 `main` 的 `Result` 决定）；到达第 3 步之后的失败可能已在
/// relay 留下部分对象、或在本地推进了 durable 游标——重跑会补齐（不自动重试）。
pub fn publish_baseline(
    config: &DevSyncConfig,
    recovery_code: Option<String>,
) -> Result<PublishSummary, SetupError> {
    let recovery_code = required_secret(recovery_code, RECOVERY_CODE_ENV, PUBLISH_FLAG)?;

    // 1) 只读身份 + 本地一致性校验：任何写库/联网之前就该失败。
    let identity = DevIdentityStore.load(&config.vault).with_context(|| {
        format!(
            "发布初始基线需要已有实例身份：{}（先 --dev-sync-create 或 --dev-sync-join；\
             本命令只复用，不创建）",
            DevIdentityStore::path_for(&config.vault).display()
        )
    })?;
    verify_recovery_code(&identity, &recovery_code)?;
    require_existing_vault(config)?;

    // 2) 打开同一库：`Existing` 模式只 load 身份/打开库，不创建任何东西。
    let outcome = prepare(config, SetupMode::Existing, SetupSecrets::default())
        .context("打开已有实例失败（发布初始基线不会创建新实例）")?;
    let notes = list_tree(&outcome.vault)
        .context("读取结构投影（统计文档数）失败")?
        .len();

    // 3) 真实发布。
    let report = DevLocal::publish_initial_pair(
        config.relay(),
        &outcome.vault,
        &outcome.identity,
        &recovery_code,
    )
    .with_context(|| {
        format!(
            "发布初始基线失败（relay = {}）；修复后可直接重跑（4b 幂等，但每跑一次会多一对 checkpoint）",
            config.relay()
        )
    })?;
    let published_at = report
        .published_at
        .context("checkpoint 报告缺发布时间（内部错误）")?;
    Ok(PublishSummary {
        checkpoint_id: report.checkpoint_id,
        epoch: report.epoch,
        covered_seq: report.covered_seq,
        published_at,
        notes,
    })
}

/// `--dev-sync-publish` 的输出（DEV ONLY；只有公开事实，不含恢复码）。
#[must_use]
pub fn publish_block(summary: &PublishSummary) -> String {
    format!(
        "[DEV ONLY] 初始基线已发布（重跑会多一份 checkpoint；B 侧现在可以 Join）\n\
         [DEV ONLY] checkpoint_id={}\n\
         [DEV ONLY] epoch={} covered_seq={}\n\
         [DEV ONLY] published_at={} notes={}",
        ids::hex_of(&summary.checkpoint_id),
        summary.epoch,
        summary.covered_seq,
        summary.published_at,
        summary.notes
    )
}

/// 恢复码 ↔ 本地管理身份的一致性校验（纯本地、不读库）。
///
/// 与 `DevLocal::publish_initial_pair` 内部的校验同源（`HKDF(R, vault_id, …)`），这里提前
/// 做一次只是为了守住"错码不碰库"：`publish_initial_pair` 之后还会再校验一遍。
fn verify_recovery_code(identity: &LocalIdentityV1, recovery_code: &str) -> Result<(), SetupError> {
    let seed = RecoverySeed::from_code(recovery_code)
        .context("恢复码格式非法（应为 mnrec1… 开头的 Bech32m 恢复码）")?;
    let Some(stored) = identity.manager_seed.as_ref() else {
        bail!("该实例没有管理私钥（Join 设备不能发布初始基线）：请用 A 侧 Create 出来的实例发布");
    };
    let derived = management_seed(&seed, &identity.vault_id)
        .context("从恢复码派生管理密钥失败（vault 归属不匹配？）")?;
    if !derived.ct_eq(stored) {
        bail!(
            "恢复码与本地管理身份不匹配（确认 {RECOVERY_CODE_ENV} 是 A 侧 Create 时终端里\
             显示的那一串；本命令不回显恢复码）"
        );
    }
    Ok(())
}

/// info/publish 的共同前提：库文件必须**已经存在**（`Vault::open` 会新建空库）。
fn require_existing_vault(config: &DevSyncConfig) -> Result<(), SetupError> {
    if !config.vault.exists() {
        bail!(
            "实例库不存在：{}（本命令只查询/发布已有实例，不创建新库；新实例请用 --dev-sync-create）",
            config.vault.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT_ID: VaultId = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    const DEVICE_ID: DeviceId = [
        0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10, 0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69,
        0x78,
    ];

    #[test]
    fn create_handoff_block_is_one_time_and_dev_only() {
        let block = create_handoff_block(&VAULT_ID, "mnrec1testcode");
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines.len(), 3, "Create 交接块固定三行：{block}");
        assert!(lines.iter().all(|line| line.starts_with("[DEV ONLY]")));
        assert_eq!(
            lines[0],
            "[DEV ONLY] vault_id=00112233445566778899aabbccddeeff"
        );
        assert_eq!(lines[1], "[DEV ONLY] recovery_code=mnrec1testcode");
        assert!(
            lines[2].contains("只显示一次") && lines[2].contains("关闭终端后不再可恢复"),
            "必须写清一次性：{block}"
        );
    }

    #[test]
    fn info_block_reports_only_public_identifiers() {
        let block = info_block(&InstanceInfo {
            vault_id: VAULT_ID,
            device_id: DEVICE_ID,
            peer: 42,
        });
        assert!(block.contains("vault_id=00112233445566778899aabbccddeeff"));
        assert!(block.contains("device_id=fedcba98765432100f1e2d3c4b5a6978"));
        assert!(block.contains("peer=42"));
        // info 不得出现任何恢复码/秘密字段。
        assert!(!block.contains("recovery_code"), "{block}");
        assert!(!block.to_ascii_lowercase().contains("mnrec1"), "{block}");
    }

    #[test]
    fn publish_block_reports_checkpoint_facts() {
        let block = publish_block(&PublishSummary {
            checkpoint_id: [0xab; 16],
            epoch: 1,
            covered_seq: 7,
            published_at: 1_700_000_000,
            notes: 3,
        });
        assert!(block.contains("checkpoint_id=abababababababababababababababab"));
        assert!(block.contains("epoch=1 covered_seq=7"));
        assert!(block.contains("published_at=1700000000 notes=3"));
        assert!(!block.contains("recovery_code"));
    }
}
