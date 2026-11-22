//! durable 控制链 pin：`sync.control_pin`（ADR 0013 §2 / ADR 0006 §35）。
//!
//! ## 为什么要单独的 durable pin
//!
//! `identity.control_rev` 只是**下界**（不知道同 rev 的 `record_hash`），
//! `sync.pinned_control_hash` 是 **bootstrap 摘要**（公式不同、不能当控制链
//! `record_hash`）。两者都不能识别"同 rev 替换 genesis/重写链"。因此本模块把
//! **已验证的控制链链尾**按 canonical CDE v1 落到 SQLite meta：
//!
//! ```text
//! {v:1, vault_id:b16, instance_id:b16,
//!  record:{rev, hash?},
//!  bootstrap?:{rev, hash, epoch, log_id}}
//! ```
//!
//! - `record` 与 `bootstrap` **各自带 rev**，不允许把旧 hash 挂到 `max(rev)` 上；
//! - `record.hash` 可为空：历史实例可能只有 `(instance, rev)` 下界。空 hash 只能当
//!   下界用，前进/同 rev 校验一律 [`SyncError::NeedsTrust`]（fail-closed），
//!   绝不"补成"一个没验证过的 hash；
//! - 读取失败、版本未知、字段残缺、vault 绑定不符都是**明确错误**，**不得**当成
//!   "没有 pin"（ADR 0013 §2「缺 record/损坏 → 停止，不得退回 unpinned」）。
//!
//! ## 提交规则（ADR 0006 §35 / ADR 0013 §2）
//!
//! [`commit_verified_head`] 只接受由已验证路径构造的 [`VerifiedHead`]：
//!
//! 1. `expected` 是调用方读到的当前 pin（`None` = 首次建立）；
//! 2. instance 必须相等（换实例 = `InstanceMismatch`）；
//! 3. 同 rev 的 `record_hash` 必须相等（否则 `Equivocation`）；旧 pin 没有 hash 时
//!    `NeedsTrust`（无法证明同 rev 历史完整性）；
//! 4. 高 rev 必须是**已知旧前缀**才允许前进：旧 pin 必须带 `record_hash`，且调用方
//!    已在完整验证过的链上确认该 hash 仍是新链前缀（`http_relay` 的
//!    `verify_control_pin`）；否则 `NeedsTrust`；
//! 5. 低 rev = `ControlRollback`；
//! 6. 写入在**同一事务**里做 CAS（`UPDATE ... WHERE value = <expected 的 canonical
//!    字节>` / 首次 `INSERT ... WHERE NOT EXISTS`），守卫要求恰好 1 行；冲突时整笔
//!    回滚并返回 [`SyncError::PinConflict`]，**不覆盖赢家**。
//!
//! ## `VerifiedHead` 的构造约束
//!
//! 字段全部私有，唯一构造点是 crate 内的
//! [`crate::http_relay::HttpRelayClient::verified_head`]（`/control` 全链验签、分页
//! 连续性与现有 pin 前缀检查都通过之后）。外部 crate 无法用裸响应 hash 构造它；
//! 测试夹具也只能在 crate 内部构造。

use std::cmp::Ordering;

use menghunote_storage::{AffectedGuard, Statement, StorageError, Value, Vault};
use menghunote_sync_proto::{decode_cde, encode_cde, CdeValue};

use crate::bootstrap::control_head_hash_at;
use crate::error::SyncError;
use crate::http_relay::ControlPin;
use crate::relay::ManagerChainEntry;

/// meta 键：durable 控制链 pin（canonical CDE v1）。
pub const META_CONTROL_PIN: &str = "sync.control_pin";

/// pin 结构版本（canonical CDE 键 0）。
pub const CONTROL_PIN_VERSION: u64 = 1;

const KEY_VERSION: u64 = 0;
const KEY_VAULT_ID: u64 = 1;
const KEY_INSTANCE_ID: u64 = 2;
const KEY_RECORD: u64 = 3;
const KEY_BOOTSTRAP: u64 = 4;

const RECORD_KEY_REV: u64 = 0;
const RECORD_KEY_HASH: u64 = 1;

const BOOTSTRAP_KEY_REV: u64 = 0;
const BOOTSTRAP_KEY_HASH: u64 = 1;
const BOOTSTRAP_KEY_EPOCH: u64 = 2;
const BOOTSTRAP_KEY_LOG_ID: u64 = 3;

/// 控制记录锚：`{rev, hash?}`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinAnchorV1 {
    /// 已认证的 control_rev。
    pub rev: u64,
    /// 该 rev 的链尾 `record_hash`；`None` = 只有 rev 下界（历史实例），不得用于前进/同 rev 校验。
    pub hash: Option<[u8; 32]>,
}

/// bootstrap 锚：`{rev, hash, epoch, log_id}`（摘要公式见
/// [`crate::bootstrap::control_head_hash`]，**不是** record hash）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapAnchorV1 {
    /// bootstrap 视图对应的 control_rev（独立于 record rev）。
    pub rev: u64,
    /// `control_head_hash(state, manager_chain)`。
    pub hash: [u8; 32],
    /// 该 bootstrap 视图的 epoch。
    pub epoch: u64,
    /// relay log id（restore 会换新，不能静默归零）。
    pub log_id: [u8; 16],
}

/// durable 控制链 pin（`sync.control_pin` 的语义结构）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinV1 {
    /// pin 绑定的 vault id（换库/换身份即绑定矛盾）。
    pub vault_id: [u8; 16],
    /// relay instance id（换实例即 fail-stop）。
    pub instance_id: [u8; 16],
    /// 控制链链尾锚。
    pub record: PinAnchorV1,
    /// bootstrap 锚（可选；与 record 各自独立验证）。
    pub bootstrap: Option<BootstrapAnchorV1>,
}

impl PinV1 {
    /// 转成 `http_relay` 使用的本地 [`ControlPin`]。
    ///
    /// bootstrap 摘要只在**同一 rev** 下有意义（否则不能把旧 hash 挂到新 rev 上）；
    /// rev 不同时只保留 record 下界（前进安全由 `verify_control_pin` 的前缀检查覆盖）。
    #[must_use]
    pub fn to_control_pin(&self) -> ControlPin {
        let mut pin = ControlPin::new(self.instance_id, self.record.rev);
        if let Some(hash) = self.record.hash {
            pin = pin.with_record_hash(hash);
        }
        // bootstrap 锚**独立携带** rev/epoch/log_id：rev 不同不代表可以丢（ADR 0013 §2/§3）。
        // 旧实现"只在 rev 相等时带上 hash"会让 identity/record rev 前进后旧锚被静默忽略；
        // 现在整枚锚交给 `verify_control_pin` 按历史视图验证（同 rev 用当前视图、
        // 异 rev 用锚自己的 rev/epoch/log/管理链前缀），验证不通过就 fail-stop。
        if let Some(bootstrap) = &self.bootstrap {
            pin = pin.with_bootstrap_anchor(*bootstrap);
        }
        pin
    }
}

/// 已验证的控制链链尾。
///
/// **只能**由 [`crate::http_relay::HttpRelayClient::verified_head`] 构造（正文见模块
/// 文档）。字段私有，因此其它 crate 无法用裸响应 hash 拼一个出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedHead {
    vault_id: [u8; 16],
    instance_id: [u8; 16],
    record_rev: u64,
    record_hash: [u8; 32],
    bootstrap: Option<BootstrapAnchorV1>,
}

impl VerifiedHead {
    /// `http_relay` 在 `/control` 全链校验通过后调用（crate 内部构造点）。
    pub(crate) fn from_verified_control(
        vault_id: [u8; 16],
        instance_id: [u8; 16],
        record_rev: u64,
        record_hash: [u8; 32],
    ) -> Self {
        Self {
            vault_id,
            instance_id,
            record_rev,
            record_hash,
            bootstrap: None,
        }
    }

    /// 由 bootstrap 验证路径附加 bootstrap 锚（ADR 0013 §3；S2 使用）。
    ///
    /// **本方法自己做历史视图验证**，不再"只做结构承载"：
    ///
    /// - 锚的 rev 不得超前于已验证的链尾（否则就是把未来/错位的 hash 挂到 head 上）；
    /// - 按锚**自己的** `rev/epoch/log_id` + 截断到该 rev 的管理链复算摘要，必须逐字节
    ///   相等（[`crate::bootstrap::control_head_hash_at`]）——rev 前进时这证明当前链仍
    ///   包含旧锚认证过的前缀，rev 相同则证明锚与当前管理链视图一致；
    /// - 复算不出历史视图 → [`SyncError::NeedsTrust`]（**不**降级成"没有锚"），
    ///   摘要不符 → [`SyncError::Equivocation`]。
    ///
    /// 同 rev 的 `epoch/log_id` 与 relay 当前 `/state` 的逐字段比对由
    /// [`crate::http_relay`] 的 `verify_control_pin` 负责（它同时看到 state 与链）。
    pub fn with_bootstrap(
        mut self,
        anchor: BootstrapAnchorV1,
        chain: &[ManagerChainEntry],
    ) -> Result<Self, SyncError> {
        if anchor.rev > self.record_rev {
            return Err(SyncError::Equivocation(
                "bootstrap 锚的 rev 超前于已验证控制链链尾",
            ));
        }
        let computed = control_head_hash_at(self.vault_id, self.instance_id, &anchor, chain)
            .ok_or(SyncError::NeedsTrust(
                "bootstrap 锚的 rev 不在当前管理链视图内，无法证明历史前缀",
            ))?;
        if computed != anchor.hash {
            return Err(SyncError::Equivocation(
                "bootstrap 锚与当前管理链的历史视图不一致",
            ));
        }
        self.bootstrap = Some(anchor);
        Ok(self)
    }

    /// pin 绑定到的 vault id。
    #[must_use]
    pub const fn vault_id(&self) -> [u8; 16] {
        self.vault_id
    }

    /// 已验证的 relay instance id。
    #[must_use]
    pub const fn instance_id(&self) -> [u8; 16] {
        self.instance_id
    }

    /// 已验证的 control_rev。
    #[must_use]
    pub const fn record_rev(&self) -> u64 {
        self.record_rev
    }

    /// 该 rev 的链尾 `record_hash`。
    #[must_use]
    pub const fn record_hash(&self) -> [u8; 32] {
        self.record_hash
    }

    /// bootstrap 锚（没有则为 `None`）。
    #[must_use]
    pub const fn bootstrap(&self) -> Option<&BootstrapAnchorV1> {
        self.bootstrap.as_ref()
    }
}

/// canonical CDE 编码（唯一编码器；写库/测试夹具共用）。
pub fn encode_pin_v1(pin: &PinV1) -> Result<Vec<u8>, SyncError> {
    let mut fields = vec![
        (KEY_VERSION, CdeValue::UInt(CONTROL_PIN_VERSION)),
        (KEY_VAULT_ID, CdeValue::Bytes(pin.vault_id.to_vec())),
        (KEY_INSTANCE_ID, CdeValue::Bytes(pin.instance_id.to_vec())),
        (KEY_RECORD, encode_anchor(&pin.record)),
    ];
    if let Some(bootstrap) = &pin.bootstrap {
        fields.push((KEY_BOOTSTRAP, encode_bootstrap(bootstrap)));
    }
    encode_cde(&CdeValue::Map(fields))
        .map_err(|_| SyncError::PinSaveFailed("sync.control_pin 无法 canonical 编码".to_owned()))
}

/// canonical CDE 解码 + 严格字段/版本/长度校验（损坏即
/// [`SyncError::PinCorrupt`]，**不**返回 `None`）。
pub fn decode_pin_v1(bytes: &[u8]) -> Result<PinV1, SyncError> {
    let CdeValue::Map(fields) = decode_cde(bytes)
        .map_err(|_| SyncError::PinCorrupt("sync.control_pin 不是合法 canonical CDE"))?
    else {
        return Err(SyncError::PinCorrupt("sync.control_pin 不是 CDE map"));
    };
    ensure_keys(
        &fields,
        &[
            KEY_VERSION,
            KEY_VAULT_ID,
            KEY_INSTANCE_ID,
            KEY_RECORD,
            KEY_BOOTSTRAP,
        ],
        "sync.control_pin 含未知字段",
    )?;
    let version = uint_field(&fields, KEY_VERSION, "sync.control_pin 缺少版本")?;
    if version != CONTROL_PIN_VERSION {
        return Err(SyncError::PinCorrupt("sync.control_pin 版本不支持"));
    }
    let vault_id = fixed_field::<16>(&fields, KEY_VAULT_ID, "sync.control_pin 的 vault_id 非法")?;
    let instance_id = fixed_field::<16>(
        &fields,
        KEY_INSTANCE_ID,
        "sync.control_pin 的 instance_id 非法",
    )?;
    let record = decode_anchor(field(&fields, KEY_RECORD, "sync.control_pin 缺少 record")?)?;
    let bootstrap = match opt_field(&fields, KEY_BOOTSTRAP) {
        None => None,
        Some(value) => Some(decode_bootstrap(value)?),
    };
    Ok(PinV1 {
        vault_id,
        instance_id,
        record,
        bootstrap,
    })
}

/// 读取 durable pin：不存在 → `Ok(None)`；损坏/版本/绑定不符 → 明确错误。
pub fn load_control_pin(
    vault: &Vault,
    expected_vault: [u8; 16],
) -> Result<Option<PinV1>, SyncError> {
    let Some(bytes) = vault.get_meta(META_CONTROL_PIN)? else {
        return Ok(None);
    };
    let pin = decode_pin_v1(&bytes)?;
    if pin.vault_id != expected_vault {
        return Err(SyncError::PinCorrupt(
            "sync.control_pin 的 vault 绑定与本地 vault 不一致",
        ));
    }
    Ok(Some(pin))
}

/// 提交**已验证**的控制链 head（ADR 0006 §35：事务重读 + CAS，只进不退）。
///
/// `expected` 必须来自 [`load_control_pin`]（或在同一批验证里得到的等价快照）。
/// 失败时整笔事务回滚，库里仍是冲突赢家写的值。
pub fn commit_verified_head(
    vault: &Vault,
    expected: Option<&PinV1>,
    verified: VerifiedHead,
) -> Result<(), SyncError> {
    let new_pin = validate_transition(expected, &verified)?;
    let new_bytes = encode_pin_v1(&new_pin)?;
    let key = Value::Text(META_CONTROL_PIN.to_owned());
    let statement = match expected {
        None => Statement::new(
            // 首次：只有 key 仍不存在时才插入（同一事务内判定，不是先查后写）。
            "INSERT INTO meta(key, value) SELECT ?1, ?2 \
             WHERE NOT EXISTS (SELECT 1 FROM meta WHERE key = ?1)",
            vec![key, Value::Blob(new_bytes)],
        ),
        Some(pin) => {
            let old_bytes = encode_pin_v1(pin)?;
            Statement::new(
                // CAS：比较**存储里的 canonical 字节**；类型/内容任一不符都不更新。
                "UPDATE meta SET value = ?2 \
                 WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?3",
                vec![key, Value::Blob(new_bytes), Value::Blob(old_bytes)],
            )
        }
    };
    match vault.exec_transaction(&[statement], &[AffectedGuard::exactly(0, 1)]) {
        Ok(_) => Ok(()),
        Err(StorageError::GuardFailed { .. }) => Err(concurrent_conflict(vault, expected)?),
        Err(error) => Err(SyncError::PinSaveFailed(format!(
            "sync.control_pin 写入失败：{error}"
        ))),
    }
}

/// 把"只有 `(instance, rev)` 下界"的旧 pin 升级成带 `record.hash` 的 canonical pin。
///
/// 这是**唯一**允许在同一 rev 上补 `record.hash` 的路径（ADR 0013 §3 的
/// `VerifiedMigration` / FreshRecovery 崩溃续跑）：[`commit_verified_head`] 对
/// "同 rev 但本地缺 hash" 一律 [`SyncError::NeedsTrust`]（不能凭空补一个没验证过的
/// hash），而这里补 hash 的依据是**已提交的 bootstrap 锚**：
///
/// 1. `expected` 必须仍是库里的下界 pin（调用方从 [`load_control_pin`] 读到的快照）；
/// 2. `verified` 必须来自已验证路径（[`crate::http_relay::HttpRelayClient::verified_head_with_anchor`]）；
/// 3. `verified.bootstrap` 必须存在，且**锚 rev == 已验证链尾 rev**——这正是"锚当初
///    认证的就是这个视图"的证明；否则返回 [`SyncError::NeedsTrust`]，绝不猜；
/// 4. vault/instance 必须一致；写入仍是 canonical 字节 CAS，冲突不覆盖赢家。
///
/// rev 已经前进（`expected.record.rev < verified.record_rev`）时**不**在这里补：
/// 旧 hash 缺失就无法证明旧前缀，调用方必须 `NeedsTrust`。
pub fn upgrade_lower_bound_pin(
    vault: &Vault,
    expected: &PinV1,
    verified: VerifiedHead,
) -> Result<(), SyncError> {
    if expected.vault_id != verified.vault_id {
        return Err(SyncError::PinCorrupt(
            "已验证 head 的 vault 与本地 pin 的 vault 不一致",
        ));
    }
    if expected.instance_id != verified.instance_id {
        return Err(SyncError::InstanceMismatch);
    }
    if expected.record.hash.is_some() {
        return Err(SyncError::Unsupported(
            "本地 pin 已带 record hash：走 commit_verified_head，不做下界升级",
        ));
    }
    if expected.record.rev != verified.record_rev {
        return Err(SyncError::NeedsTrust(
            "旧 pin 只有 rev 下界且链尾已前进：无法证明同 rev 历史完整性",
        ));
    }
    let Some(anchor) = verified.bootstrap else {
        return Err(SyncError::NeedsTrust(
            "缺少与链尾同视图的 bootstrap 锚：无法证明该 rev 的历史完整性",
        ));
    };
    if anchor.rev != verified.record_rev {
        return Err(SyncError::NeedsTrust(
            "bootstrap 锚的 rev 与已验证链尾不同：不是同一验证视图",
        ));
    }
    let new_pin = PinV1 {
        vault_id: expected.vault_id,
        instance_id: verified.instance_id,
        record: PinAnchorV1 {
            rev: verified.record_rev,
            hash: Some(verified.record_hash),
        },
        bootstrap: Some(anchor),
    };
    let new_bytes = encode_pin_v1(&new_pin)?;
    let old_bytes = encode_pin_v1(expected)?;
    match vault.exec_transaction(
        &[Statement::new(
            "UPDATE meta SET value = ?2 \
             WHERE key = ?1 AND typeof(value) = 'blob' AND value = ?3",
            vec![
                Value::Text(META_CONTROL_PIN.to_owned()),
                Value::Blob(new_bytes),
                Value::Blob(old_bytes),
            ],
        )],
        &[AffectedGuard::exactly(0, 1)],
    ) {
        Ok(_) => Ok(()),
        Err(StorageError::GuardFailed { .. }) => Err(concurrent_conflict(vault, Some(expected))?),
        Err(error) => Err(SyncError::PinSaveFailed(format!(
            "sync.control_pin 升级写入失败：{error}"
        ))),
    }
}

/// 语义校验（全部相对调用方读到的 `expected`；CAS 保证校验对象未被并发改写）。
fn validate_transition(
    expected: Option<&PinV1>,
    verified: &VerifiedHead,
) -> Result<PinV1, SyncError> {
    let record = PinAnchorV1 {
        rev: verified.record_rev,
        hash: Some(verified.record_hash),
    };
    let Some(expected) = expected else {
        // 首次建立：head 由已验证路径给出；这里只固化，不做"盲信 continue"之外的推断。
        return Ok(PinV1 {
            vault_id: verified.vault_id,
            instance_id: verified.instance_id,
            record,
            bootstrap: verified.bootstrap,
        });
    };
    if expected.vault_id != verified.vault_id {
        return Err(SyncError::PinCorrupt(
            "已验证 head 的 vault 与本地 pin 的 vault 不一致",
        ));
    }
    if expected.instance_id != verified.instance_id {
        return Err(SyncError::InstanceMismatch);
    }
    match verified.record_rev.cmp(&expected.record.rev) {
        Ordering::Less => {
            return Err(SyncError::ControlRollback {
                expected: expected.record.rev,
                actual: verified.record_rev,
            })
        }
        Ordering::Equal => match expected.record.hash {
            Some(hash) if hash == verified.record_hash => {}
            Some(_) => return Err(SyncError::Equivocation("同 rev 的 record hash 不一致")),
            None => {
                return Err(SyncError::NeedsTrust(
                    "同 rev 的本地 pin 缺少 record hash，无法证明同 rev 历史完整性",
                ))
            }
        },
        Ordering::Greater => {
            if expected.record.hash.is_none() {
                return Err(SyncError::NeedsTrust(
                    "高 rev 前进缺少旧 record hash 前缀 anchor；必须先建立可信同 rev pin",
                ));
            }
        }
    }
    if let Some(anchor) = verified.bootstrap.as_ref() {
        // 防"把旧 hash 挂到 max(rev)"：锚可以有更低的 rev，但绝不能超前于已验证链尾。
        if anchor.rev > verified.record_rev {
            return Err(SyncError::Equivocation(
                "bootstrap 锚的 rev 超前于已验证控制链链尾",
            ));
        }
    }
    let bootstrap = merge_bootstrap(expected.bootstrap.as_ref(), verified.bootstrap.as_ref())?;
    Ok(PinV1 {
        vault_id: expected.vault_id,
        instance_id: verified.instance_id,
        record,
        bootstrap,
    })
}

/// bootstrap 锚合并：不能因 record 前进丢 bootstrap；同 rev 必须一致。
fn merge_bootstrap(
    old: Option<&BootstrapAnchorV1>,
    new: Option<&BootstrapAnchorV1>,
) -> Result<Option<BootstrapAnchorV1>, SyncError> {
    match (old, new) {
        (None, None) => Ok(None),
        (Some(old), None) => Ok(Some(*old)),
        (None, Some(new)) => Ok(Some(*new)),
        (Some(old), Some(new)) => match new.rev.cmp(&old.rev) {
            Ordering::Less => Err(SyncError::ControlRollback {
                expected: old.rev,
                actual: new.rev,
            }),
            Ordering::Equal => {
                if new.hash != old.hash || new.epoch != old.epoch || new.log_id != old.log_id {
                    Err(SyncError::Equivocation("同 rev 的 bootstrap 锚内容不一致"))
                } else {
                    Ok(Some(*new))
                }
            }
            Ordering::Greater => {
                // 前进由调用方按旧 rev/epoch/log 视图验证历史 anchor（ADR 0013 §3）；
                // 这里只拒绝明显的 epoch 回退。
                if new.epoch < old.epoch {
                    Err(SyncError::Equivocation("bootstrap 锚的 epoch 回退"))
                } else {
                    Ok(Some(*new))
                }
            }
        },
    }
}

/// CAS 未命中后的**只读**分类：重读当前 pin，与 `expected` 比较。
fn concurrent_conflict(vault: &Vault, expected: Option<&PinV1>) -> Result<SyncError, SyncError> {
    let Some(bytes) = vault.get_meta(META_CONTROL_PIN).map_err(|error| {
        SyncError::PinSaveFailed(format!("sync.control_pin 冲突后重读失败：{error}"))
    })?
    else {
        return Ok(SyncError::PinConflict);
    };
    let current = decode_pin_v1(&bytes)?;
    if expected.is_some_and(|pin| pin == &current) {
        // 内容没变却 CAS 失败：不是并发冲突，明确报保存失败（不重试覆盖）。
        return Ok(SyncError::PinSaveFailed(
            "sync.control_pin CAS 未命中但内容未变".to_owned(),
        ));
    }
    Ok(SyncError::PinConflict)
}

fn encode_anchor(anchor: &PinAnchorV1) -> CdeValue {
    let mut fields = vec![(RECORD_KEY_REV, CdeValue::UInt(anchor.rev))];
    if let Some(hash) = anchor.hash {
        fields.push((RECORD_KEY_HASH, CdeValue::Bytes(hash.to_vec())));
    }
    CdeValue::Map(fields)
}

fn decode_anchor(value: &CdeValue) -> Result<PinAnchorV1, SyncError> {
    let CdeValue::Map(fields) = value else {
        return Err(SyncError::PinCorrupt("sync.control_pin 的 record 不是 map"));
    };
    ensure_keys(
        fields,
        &[RECORD_KEY_REV, RECORD_KEY_HASH],
        "sync.control_pin 的 record 含未知字段",
    )?;
    let rev = uint_field(fields, RECORD_KEY_REV, "sync.control_pin 的 record 缺 rev")?;
    let hash = match opt_field(fields, RECORD_KEY_HASH) {
        None => None,
        Some(CdeValue::Bytes(bytes)) => Some(
            <[u8; 32]>::try_from(bytes.as_slice())
                .map_err(|_| SyncError::PinCorrupt("sync.control_pin 的 record hash 长度非法"))?,
        ),
        Some(_) => {
            return Err(SyncError::PinCorrupt(
                "sync.control_pin 的 record hash 类型非法",
            ))
        }
    };
    Ok(PinAnchorV1 { rev, hash })
}

fn encode_bootstrap(anchor: &BootstrapAnchorV1) -> CdeValue {
    CdeValue::Map(vec![
        (BOOTSTRAP_KEY_REV, CdeValue::UInt(anchor.rev)),
        (BOOTSTRAP_KEY_HASH, CdeValue::Bytes(anchor.hash.to_vec())),
        (BOOTSTRAP_KEY_EPOCH, CdeValue::UInt(anchor.epoch)),
        (
            BOOTSTRAP_KEY_LOG_ID,
            CdeValue::Bytes(anchor.log_id.to_vec()),
        ),
    ])
}

fn decode_bootstrap(value: &CdeValue) -> Result<BootstrapAnchorV1, SyncError> {
    let CdeValue::Map(fields) = value else {
        return Err(SyncError::PinCorrupt(
            "sync.control_pin 的 bootstrap 不是 map",
        ));
    };
    ensure_keys(
        fields,
        &[
            BOOTSTRAP_KEY_REV,
            BOOTSTRAP_KEY_HASH,
            BOOTSTRAP_KEY_EPOCH,
            BOOTSTRAP_KEY_LOG_ID,
        ],
        "sync.control_pin 的 bootstrap 含未知字段",
    )?;
    Ok(BootstrapAnchorV1 {
        rev: uint_field(
            fields,
            BOOTSTRAP_KEY_REV,
            "sync.control_pin 的 bootstrap 缺 rev",
        )?,
        hash: fixed_field::<32>(
            fields,
            BOOTSTRAP_KEY_HASH,
            "sync.control_pin 的 bootstrap hash 非法",
        )?,
        epoch: uint_field(
            fields,
            BOOTSTRAP_KEY_EPOCH,
            "sync.control_pin 的 bootstrap 缺 epoch",
        )?,
        log_id: fixed_field::<16>(
            fields,
            BOOTSTRAP_KEY_LOG_ID,
            "sync.control_pin 的 bootstrap log_id 非法",
        )?,
    })
}

fn ensure_keys(
    fields: &[(u64, CdeValue)],
    allowed: &[u64],
    message: &'static str,
) -> Result<(), SyncError> {
    for (key, _) in fields {
        if !allowed.contains(key) {
            return Err(SyncError::PinCorrupt(message));
        }
    }
    Ok(())
}

fn field<'a>(
    fields: &'a [(u64, CdeValue)],
    key: u64,
    message: &'static str,
) -> Result<&'a CdeValue, SyncError> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
        .ok_or(SyncError::PinCorrupt(message))
}

fn opt_field(fields: &[(u64, CdeValue)], key: u64) -> Option<&CdeValue> {
    fields
        .iter()
        .find(|(index, _)| *index == key)
        .map(|(_, value)| value)
}

fn uint_field(
    fields: &[(u64, CdeValue)],
    key: u64,
    message: &'static str,
) -> Result<u64, SyncError> {
    match field(fields, key, message)? {
        CdeValue::UInt(value) => Ok(*value),
        _ => Err(SyncError::PinCorrupt(message)),
    }
}

fn fixed_field<const N: usize>(
    fields: &[(u64, CdeValue)],
    key: u64,
    message: &'static str,
) -> Result<[u8; N], SyncError> {
    let CdeValue::Bytes(bytes) = field(fields, key, message)? else {
        return Err(SyncError::PinCorrupt(message));
    };
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| SyncError::PinCorrupt(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rotation::ManagerTransitionV1;

    const VAULT: [u8; 16] = [0x11; 16];
    const INSTANCE: [u8; 16] = [0x22; 16];

    fn fresh() -> (tempfile::TempDir, Vault) {
        let base = std::env::var("CARGO_TARGET_TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("target/tmp"));
        std::fs::create_dir_all(&base).expect("target/tmp");
        let dir = tempfile::Builder::new()
            .prefix("control-pin-")
            .tempdir_in(&base)
            .expect("临时目录");
        let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("vault");
        (dir, vault)
    }

    fn head(rev: u64, hash: [u8; 32]) -> VerifiedHead {
        VerifiedHead::from_verified_control(VAULT, INSTANCE, rev, hash)
    }

    fn head_with_bootstrap(rev: u64, hash: [u8; 32], bootstrap: BootstrapAnchorV1) -> VerifiedHead {
        head(rev, hash)
            .with_bootstrap(bootstrap, &test_chain())
            .expect("测试锚必须与测试管理链一致")
    }

    /// 测试管理链：rev1 genesis + rev2 转换（epoch 1 → 2）。
    fn test_chain() -> Vec<ManagerChainEntry> {
        let transition = ManagerTransitionV1 {
            vault_id: VAULT,
            instance_id: INSTANCE,
            rotation_id: [0x0c; 16],
            from_epoch: 1,
            to_epoch: 2,
            old_rev: 1,
            new_rev: 2,
            new_pubkey: [0x02; 32],
            covered_seq: 0,
        }
        .encode()
        .expect("测试转换编码");
        vec![
            ManagerChainEntry {
                rev: 1,
                pubkey: [0x01; 32],
                transition: vec![0xA1, 0xA2],
                old_rev: 1,
                old_signature: None,
                new_signature: None,
            },
            ManagerChainEntry {
                rev: 2,
                pubkey: [0x02; 32],
                transition,
                old_rev: 1,
                old_signature: Some([0x03; 64]),
                new_signature: Some([0x04; 64]),
            },
        ]
    }

    /// 与 [`test_chain`] 自洽的锚摘要（rev/epoch/log 由调用方决定）。
    ///
    /// 历史视图按**生效 epoch** 截断（rev1 + `to_epoch <= epoch` 的转换）。
    fn anchor_hash(rev: u64, epoch: u64, log_id: [u8; 16]) -> [u8; 32] {
        let chain = test_chain();
        let end = if epoch >= 2 { chain.len() } else { 1 };
        crate::bootstrap::control_head_hash_parts(
            VAULT,
            INSTANCE,
            log_id,
            epoch,
            rev,
            &chain[..end],
        )
    }

    fn load(vault: &Vault) -> Option<PinV1> {
        load_control_pin(vault, VAULT).expect("读取 pin")
    }

    fn write_raw(vault: &Vault, pin: &PinV1) {
        vault
            .set_meta(META_CONTROL_PIN, &encode_pin_v1(pin).expect("编码"))
            .expect("写 raw pin");
    }

    #[test]
    fn first_commit_is_durable_and_reloads_identically() {
        let (_dir, vault) = fresh();
        assert!(load(&vault).is_none(), "首次没有 pin");

        let verified = head(1, [0xA1; 32]);
        commit_verified_head(&vault, None, verified).expect("首次提交");
        let pin = load(&vault).expect("必须能读回");
        assert_eq!(pin.vault_id, VAULT);
        assert_eq!(pin.instance_id, INSTANCE);
        assert_eq!(pin.record.rev, 1);
        assert_eq!(pin.record.hash, Some([0xA1; 32]));
        assert_eq!(pin.bootstrap, None);
        // canonical 编码可往返。
        let bytes = encode_pin_v1(&pin).expect("编码");
        assert_eq!(decode_pin_v1(&bytes).expect("解码"), pin);
        // 存储的是 canonical 字节，不是裸响应 hash。
        assert_eq!(
            vault
                .get_meta(META_CONTROL_PIN)
                .expect("meta")
                .expect("存在"),
            bytes
        );
    }

    #[test]
    fn same_rev_with_different_hash_is_equivocation_and_does_not_overwrite() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(2, [0xB1; 32])).expect("首次");
        let expected = load(&vault).expect("pin");
        let error = commit_verified_head(&vault, Some(&expected), head(2, [0xB2; 32]))
            .expect_err("同 rev 换 hash 必须拒绝");
        assert!(matches!(error, SyncError::Equivocation(_)), "{error:?}");
        assert_eq!(load(&vault).expect("pin").record.hash, Some([0xB1; 32]));
    }

    #[test]
    fn same_rev_without_a_known_hash_needs_trust() {
        let (_dir, vault) = fresh();
        write_raw(
            &vault,
            &PinV1 {
                vault_id: VAULT,
                instance_id: INSTANCE,
                record: PinAnchorV1 { rev: 4, hash: None },
                bootstrap: None,
            },
        );
        let expected = load(&vault).expect("pin");
        let error = commit_verified_head(&vault, Some(&expected), head(4, [0xC1; 32]))
            .expect_err("同 rev 缺 hash 不能补");
        assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
    }

    #[test]
    fn lower_rev_is_rejected_as_rollback() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(3, [0xD1; 32])).expect("首次");
        let expected = load(&vault).expect("pin");
        let error = commit_verified_head(&vault, Some(&expected), head(2, [0xD2; 32]))
            .expect_err("低 rev 必须拒绝");
        assert!(
            matches!(
                error,
                SyncError::ControlRollback {
                    expected: 3,
                    actual: 2
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn instance_change_is_rejected() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(1, [0xE1; 32])).expect("首次");
        let expected = load(&vault).expect("pin");
        let other = VerifiedHead::from_verified_control(VAULT, [0x33; 16], 2, [0xE2; 32]);
        let error =
            commit_verified_head(&vault, Some(&expected), other).expect_err("换实例必须拒绝");
        assert!(matches!(error, SyncError::InstanceMismatch), "{error:?}");
    }

    #[test]
    fn advancing_without_a_known_prefix_needs_trust() {
        let (_dir, vault) = fresh();
        write_raw(
            &vault,
            &PinV1 {
                vault_id: VAULT,
                instance_id: INSTANCE,
                record: PinAnchorV1 { rev: 1, hash: None },
                bootstrap: None,
            },
        );
        let expected = load(&vault).expect("pin");
        let error = commit_verified_head(&vault, Some(&expected), head(2, [0xF1; 32]))
            .expect_err("无旧前缀不得前进");
        assert!(matches!(error, SyncError::NeedsTrust(_)), "{error:?}");
    }

    #[test]
    fn advancing_keeps_bootstrap_when_verified_head_has_none() {
        let (_dir, vault) = fresh();
        let bootstrap = BootstrapAnchorV1 {
            rev: 1,
            hash: anchor_hash(1, 1, [0x52; 16]),
            epoch: 1,
            log_id: [0x52; 16],
        };
        commit_verified_head(&vault, None, head_with_bootstrap(1, [0x53; 32], bootstrap))
            .expect("首次");
        let expected = load(&vault).expect("pin");
        commit_verified_head(&vault, Some(&expected), head(2, [0x54; 32])).expect("前进");
        let pin = load(&vault).expect("pin");
        assert_eq!(pin.record.rev, 2);
        assert_eq!(
            pin.bootstrap,
            Some(bootstrap),
            "不得因 record 前进丢 bootstrap"
        );
        // 同 rev 换 hash（与旧锚自洽但内容不同）：`with_bootstrap` 的历史视图就对不上。
        let inconsistent = head(2, [0x54; 32]).with_bootstrap(
            BootstrapAnchorV1 {
                hash: [0x55; 32],
                ..bootstrap
            },
            &test_chain(),
        );
        assert!(matches!(inconsistent, Err(SyncError::Equivocation(_))));

        // 自洽但与旧锚不同的同 rev 锚：`with_bootstrap` 能过（它只看锚自己的视图），
        // commit 的 merge 必须拒绝——不能因为"自洽"就替掉已提交的同 rev 锚。
        let other = BootstrapAnchorV1 {
            hash: anchor_hash(1, 1, [0x53; 16]),
            log_id: [0x53; 16],
            ..bootstrap
        };
        let error = commit_verified_head(
            &vault,
            Some(&pin),
            head_with_bootstrap(2, [0x54; 32], other),
        )
        .expect_err("同 rev bootstrap 不一致必须拒绝");
        assert!(matches!(error, SyncError::Equivocation(_)), "{error:?}");
    }

    /// 下界 pin 的同 rev 升级：必须同时满足"同视图锚 + 同 rev + 同实例"。
    #[test]
    fn lower_bound_upgrade_requires_a_same_view_anchor() {
        let (_dir, vault) = fresh();
        let bootstrap = BootstrapAnchorV1 {
            rev: 2,
            hash: anchor_hash(2, 1, [0x62; 16]),
            epoch: 1,
            log_id: [0x62; 16],
        };
        let lower = PinV1 {
            vault_id: VAULT,
            instance_id: INSTANCE,
            record: PinAnchorV1 { rev: 2, hash: None },
            bootstrap: Some(bootstrap),
        };
        write_raw(&vault, &lower);
        let expected = load(&vault).expect("pin");
        let verified = head_with_bootstrap(2, [0x63; 32], bootstrap);
        upgrade_lower_bound_pin(&vault, &expected, verified).expect("同 rev + 同视图锚可升级");
        let pin = load(&vault).expect("pin");
        assert_eq!(pin.record.rev, 2);
        assert_eq!(pin.record.hash, Some([0x63; 32]));
        assert_eq!(pin.bootstrap, Some(bootstrap));

        // 已经是 canonical pin 时不再走升级路径（明确拒绝，避免语义混淆）。
        let error =
            upgrade_lower_bound_pin(&vault, &pin, head_with_bootstrap(2, [0x63; 32], bootstrap))
                .expect_err("已带 hash 不得再升级");
        assert!(matches!(error, SyncError::Unsupported(_)), "{error:?}");

        // 每个失败模式都在写库前拒绝，且不改库里的 pin。
        for (label, expected, verified) in [
            (
                "链尾前进但旧 pin 只有下界",
                lower,
                head(3, [0x64; 32]).with_bootstrap(
                    BootstrapAnchorV1 {
                        rev: 3,
                        hash: anchor_hash(3, 1, [0x62; 16]),
                        ..bootstrap
                    },
                    &test_chain(),
                ),
            ),
            ("同 rev 但锚不同视图", lower, Ok(head(2, [0x63; 32]))),
            (
                "锚 rev 与链尾不同",
                lower,
                head(1, [0x65; 32]).with_bootstrap(
                    BootstrapAnchorV1 {
                        rev: 1,
                        hash: anchor_hash(1, 1, [0x62; 16]),
                        ..bootstrap
                    },
                    &test_chain(),
                ),
            ),
        ] {
            let verified = match verified {
                Ok(verified) => verified,
                Err(error) => panic!("{label}: 测试夹具必须是已验证 head：{error:?}"),
            };
            let error = upgrade_lower_bound_pin(&vault, &expected, verified)
                .expect_err(&format!("{label} 必须拒绝"));
            assert!(
                matches!(error, SyncError::NeedsTrust(_)),
                "{label}: {error:?}"
            );
            assert_eq!(load(&vault).expect("pin"), pin, "{label} 不得改写 pin");
        }

        // instance 不符（在写库前拒绝，不产生 CAS 冲突噪音）。
        let other_instance = PinV1 {
            instance_id: [0x44; 16],
            ..lower
        };
        let error = upgrade_lower_bound_pin(
            &vault,
            &other_instance,
            head_with_bootstrap(2, [0x63; 32], bootstrap),
        )
        .expect_err("换实例必须拒绝");
        assert!(matches!(error, SyncError::InstanceMismatch), "{error:?}");
    }

    #[test]
    fn corrupt_or_wrong_version_pins_are_errors_not_missing() {
        let (_dir, vault) = fresh();
        let blobs: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0xFF, 0x00],
            // 版本 2。
            encode_cde(&CdeValue::Map(vec![
                (KEY_VERSION, CdeValue::UInt(2)),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_INSTANCE_ID, CdeValue::Bytes(INSTANCE.to_vec())),
                (
                    KEY_RECORD,
                    CdeValue::Map(vec![(RECORD_KEY_REV, CdeValue::UInt(1))]),
                ),
            ]))
            .expect("cde"),
            // 缺 vault_id。
            encode_cde(&CdeValue::Map(vec![
                (KEY_VERSION, CdeValue::UInt(CONTROL_PIN_VERSION)),
                (KEY_INSTANCE_ID, CdeValue::Bytes(INSTANCE.to_vec())),
                (
                    KEY_RECORD,
                    CdeValue::Map(vec![(RECORD_KEY_REV, CdeValue::UInt(1))]),
                ),
            ]))
            .expect("cde"),
            // 未知字段。
            encode_cde(&CdeValue::Map(vec![
                (KEY_VERSION, CdeValue::UInt(CONTROL_PIN_VERSION)),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_INSTANCE_ID, CdeValue::Bytes(INSTANCE.to_vec())),
                (
                    KEY_RECORD,
                    CdeValue::Map(vec![(RECORD_KEY_REV, CdeValue::UInt(1))]),
                ),
                (9, CdeValue::UInt(1)),
            ]))
            .expect("cde"),
            // record hash 长度错。
            encode_cde(&CdeValue::Map(vec![
                (KEY_VERSION, CdeValue::UInt(CONTROL_PIN_VERSION)),
                (KEY_VAULT_ID, CdeValue::Bytes(VAULT.to_vec())),
                (KEY_INSTANCE_ID, CdeValue::Bytes(INSTANCE.to_vec())),
                (
                    KEY_RECORD,
                    CdeValue::Map(vec![
                        (RECORD_KEY_REV, CdeValue::UInt(1)),
                        (RECORD_KEY_HASH, CdeValue::Bytes(vec![0; 31])),
                    ]),
                ),
            ]))
            .expect("cde"),
        ];
        for blob in blobs {
            vault.set_meta(META_CONTROL_PIN, &blob).expect("写坏 pin");
            let error = load_control_pin(&vault, VAULT).expect_err("损坏必须报错，不能当缺失");
            assert!(matches!(error, SyncError::PinCorrupt(_)), "{error:?}");
        }
    }

    #[test]
    fn vault_binding_mismatch_is_rejected() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(1, [0x61; 32])).expect("首次");
        let error = load_control_pin(&vault, [0x99; 16]).expect_err("换 vault 读取必须报错");
        assert!(matches!(error, SyncError::PinCorrupt(_)), "{error:?}");
    }

    #[test]
    fn concurrent_insert_does_not_overwrite_the_winner() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(1, [0x71; 32])).expect("赢家");
        // 第二个写者仍以 expected=None 提交：语义上允许首次，但 CAS 发现已存在。
        let error =
            commit_verified_head(&vault, None, head(1, [0x72; 32])).expect_err("并发首次必须冲突");
        assert!(matches!(error, SyncError::PinConflict), "{error:?}");
        assert_eq!(load(&vault).expect("pin").record.hash, Some([0x71; 32]));
    }

    #[test]
    fn concurrent_update_does_not_overwrite_the_winner() {
        let (_dir, vault) = fresh();
        commit_verified_head(&vault, None, head(1, [0x81; 32])).expect("首次");
        let stale = load(&vault).expect("pin");
        // 另一个写者先推进到 rev2。
        commit_verified_head(&vault, Some(&stale), head(2, [0x82; 32])).expect("赢家推进");
        // 过期写者用同一个 stale expected 提交 rev2 的另一条 hash。
        let error = commit_verified_head(&vault, Some(&stale), head(2, [0x83; 32]))
            .expect_err("过期 CAS 必须冲突");
        assert!(matches!(error, SyncError::PinConflict), "{error:?}");
        let pin = load(&vault).expect("pin");
        assert_eq!(pin.record.rev, 2);
        assert_eq!(pin.record.hash, Some([0x82; 32]), "不得覆盖赢家");
    }

    #[test]
    fn bootstrap_anchor_regressions_are_rejected() {
        let (_dir, vault) = fresh();
        let bootstrap = BootstrapAnchorV1 {
            rev: 3,
            hash: anchor_hash(3, 5, [0xA8; 16]),
            epoch: 5,
            log_id: [0xA8; 16],
        };
        commit_verified_head(&vault, None, head_with_bootstrap(3, [0xA9; 32], bootstrap))
            .expect("首次");
        let pin = load(&vault).expect("pin");

        // 低 rev 但自洽的锚：`with_bootstrap` 放行（它只验历史视图），commit 必须回退拒绝。
        let lower = head_with_bootstrap(
            4,
            [0xAA; 32],
            BootstrapAnchorV1 {
                rev: 2,
                hash: anchor_hash(2, 5, [0xA8; 16]),
                ..bootstrap
            },
        );
        assert!(matches!(
            commit_verified_head(&vault, Some(&pin), lower),
            Err(SyncError::ControlRollback { .. })
        ));

        // 同 rev 换 hash（自洽的另一枚锚）：merge 必须报 equivocation。
        let swapped = head_with_bootstrap(
            4,
            [0xAA; 32],
            BootstrapAnchorV1 {
                hash: anchor_hash(3, 5, [0xAB; 16]),
                log_id: [0xAB; 16],
                ..bootstrap
            },
        );
        assert!(matches!(
            commit_verified_head(&vault, Some(&pin), swapped),
            Err(SyncError::Equivocation(_))
        ));

        // 同 rev epoch 回退（同样是自洽锚）：merge 必须拒绝，不能让 epoch 静默回退。
        let epoch_back = head_with_bootstrap(
            4,
            [0xAA; 32],
            BootstrapAnchorV1 {
                hash: anchor_hash(3, 4, [0xA8; 16]),
                epoch: 4,
                ..bootstrap
            },
        );
        assert!(matches!(
            commit_verified_head(&vault, Some(&pin), epoch_back),
            Err(SyncError::Equivocation(_))
        ));

        // 锚 rev 超前于已验证链尾：不能把"未来"的锚挂到当前 head 上。
        let ahead = head(2, [0xAC; 32]).with_bootstrap(
            BootstrapAnchorV1 {
                rev: 3,
                hash: anchor_hash(3, 5, [0xA8; 16]),
                ..bootstrap
            },
            &test_chain(),
        );
        assert!(matches!(ahead, Err(SyncError::Equivocation(_))));
    }

    /// 异 rev 的旧锚只能按**历史视图**验证：链前缀被重写即拒，链未覆盖锚 rev 则 NeedsTrust。
    #[test]
    fn historical_bootstrap_anchor_requires_the_chain_prefix() {
        let anchor = BootstrapAnchorV1 {
            rev: 1,
            hash: anchor_hash(1, 1, [0x52; 16]),
            epoch: 1,
            log_id: [0x52; 16],
        };
        // 同一枚锚 + 原链：通过（rev 前进也能证明旧前缀）。
        head(3, [0x61; 32])
            .with_bootstrap(anchor, &test_chain())
            .expect("原链必须包含旧锚认证过的前缀");

        // rev1 的 genesis 被重写（管理链前缀变了）→ 旧摘要不再成立。
        let mut rewritten = test_chain();
        rewritten[0].pubkey = [0x09; 32];
        assert!(matches!(
            head(3, [0x61; 32]).with_bootstrap(anchor, &rewritten),
            Err(SyncError::Equivocation(_))
        ));

        // 链根本没覆盖锚的 rev（空链/只有更晚的 rev）→ NeedsTrust，不能当"没有锚"。
        assert!(matches!(
            head(3, [0x61; 32]).with_bootstrap(anchor, &[]),
            Err(SyncError::NeedsTrust(_))
        ));
        let later_only = &test_chain()[1..];
        assert!(matches!(
            head(3, [0x61; 32]).with_bootstrap(anchor, later_only),
            Err(SyncError::NeedsTrust(_)) | Err(SyncError::Equivocation(_))
        ));

        // 锚 epoch 与摘要不匹配（锚自己的坐标被改）：也要拒。
        assert!(matches!(
            head(3, [0x61; 32])
                .with_bootstrap(BootstrapAnchorV1 { epoch: 2, ..anchor }, &test_chain()),
            Err(SyncError::Equivocation(_))
        ));
    }

    #[test]
    fn pin_to_control_pin_keeps_the_bootstrap_anchor_across_revs() {
        let bootstrap = BootstrapAnchorV1 {
            rev: 2,
            hash: [0xB9; 32],
            epoch: 1,
            log_id: [0xBA; 16],
        };
        let pin = PinV1 {
            vault_id: VAULT,
            instance_id: INSTANCE,
            record: PinAnchorV1 {
                rev: 2,
                hash: Some([0xBB; 32]),
            },
            bootstrap: Some(bootstrap),
        };
        let control = pin.to_control_pin();
        assert_eq!(control.instance_id, INSTANCE);
        assert_eq!(control.control_rev, 2);
        assert_eq!(control.record_hash, Some([0xBB; 32]));
        assert_eq!(control.bootstrap, Some(bootstrap));

        // record rev 前进：旧 bootstrap 锚**不能丢**，也不能被改写成新 rev 的 hash；
        // 它整枚（rev/epoch/log_id）随 pin 携带，由 `verify_control_pin` 按历史视图验证。
        let advanced = PinV1 {
            record: PinAnchorV1 {
                rev: 3,
                hash: Some([0xBC; 32]),
            },
            ..pin
        };
        let control = advanced.to_control_pin();
        assert_eq!(control.control_rev, 3);
        assert_eq!(
            control.bootstrap,
            Some(bootstrap),
            "bootstrap 锚必须独立携带自己的 rev（不能挂到 max(rev) 上）"
        );
    }
}
