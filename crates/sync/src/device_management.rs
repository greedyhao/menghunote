//! ADR 0010 切片 1b：管理端**独立**吊销设备访问的客户端 helper。
//!
//! 定位：同步是端到端加密，relay 只存密文，删设备凭证只能阻断**访问**，不能让已持有
//! 旧 root 的设备失去解密能力。本模块只发这一个管理端请求并把 relay 的事实返回给调用方，
//! **不删除**本地身份 / `manager_seed` / 恢复材料 / 笔记，也不自动轮换密钥
//! （轮换是显式 opt-in，ADR 0010 §安全边界）。
//!
//! wire（与 `crates/relay/src/api.rs` 的 `revoke_device` 严格一致）：
//!
//! ```text
//! POST /v1/vaults/{vault}/devices/{device}/revoke
//! B    = CDE{0: target_device(bytes16), 1: request_id(bytes16), 2: expected_control_rev(uint)}
//! wire = {0: {0: bytes(B), 1: signature64}, 1: request_id, 2: expected_control_rev}
//! ```
//!
//! - 管理 bearer：复用 `crate::recovery_enroll::management_token`（管理 realm challenge）；
//! - 管理签名：复用 `crate::rotation_stage::sign_management`（`POST=2` + 完整 path +
//!   instance/vault/request_id/expected_control_rev + `SHA256(CDE{method,path,B})`），
//!   **不复制**签名逻辑；
//! - 响应：`200 CDE{0: target, 1: "revoked", 2: changed, 3: auth_rev, 4: control_rev}`。
//!
//! 三条客户端硬约束：
//!
//! 1. **禁止移除本设备**：helper 层面直接拒绝（本地错误，**不发任何请求**）；标准 UI 也
//!    不得提供该入口（ADR 0010）。误删本设备只能靠另一台管理设备或恢复码重新入网；
//! 2. **管理密钥必须与本地身份 pin 一致**：helper 要求调用方传入 `ManagementKey`
//!    （类型上保证"有 key"），但仍校验其公钥等于身份里的 `pinned_manager`；不匹配说明
//!    恢复码/身份对应的是另一个管理代，必须在本地拒绝，不能拿错 key 去撞 relay；
//! 3. **只返回事实**：`control_rev` 不变（独立吊销不是签名控制事实），`auth_rev` 前进
//!    使全 vault 的旧 token 立即失效；缓存失效由 [`crate::http_relay`] 的 `state()` /
//!    REVOKED 处理负责，本模块不碰任何缓存。

use crate::cbor;
use crate::device::ManagementKey;
use crate::error::SyncError;
use crate::http_relay::HttpTransport;
use crate::identity::LocalIdentityV1;
use crate::{DeviceId, VaultId};

/// 吊销请求的本地上下文（`base_url` / vault / instance pin / 本机设备 / CAS rev）。
///
/// 字段都不含秘密：`manager_pub` 是公开管理公钥（本地信任锚），`self_device_id` 是设备
/// 公开 ID。管理私钥只经 [`revoke_device`] 的 `manager` 参数传入。
pub struct RevokeContext {
    /// relay 基址（`http://127.0.0.1:port` 或 `https://...`，末尾 `/` 会被忽略）。
    pub base_url: String,
    /// vault 身份。
    pub vault_id: VaultId,
    /// 已 pin 的 relay instance（签名绑定它；换实例即拒绝）。
    pub instance_id: [u8; 16],
    /// 本地身份 pin 住的管理公钥（不匹配 → 拒绝，不发请求）。
    pub manager_pub: [u8; 32],
    /// 本机设备 ID（**禁止**作为 target；`target == self` 直接拒绝）。
    pub self_device_id: DeviceId,
    /// 期望的 `control_rev`（CAS；吊销**不会**推进它）。
    pub expected_control_rev: u64,
    /// 当前 Unix 秒（管理 challenge/token 的有效期判断）。
    pub now: u64,
}

impl RevokeContext {
    /// 用本地身份构造上下文：instance/管理公钥/本机设备都取自受保护身份，
    /// 避免调用方各自拼错 pin。
    ///
    /// `expected_control_rev` 应取最近一次已验证的控制链 head（`control_head().0` 或
    /// `/state` 的 `control_rev`）；CAS 冲突时刷新列表后换新 `request_id` 重试，
    /// **不要**盲目改 rev 重放同一个请求。
    #[must_use]
    pub fn from_identity(
        base_url: impl Into<String>,
        identity: &LocalIdentityV1,
        expected_control_rev: u64,
        now: u64,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            vault_id: identity.vault_id,
            instance_id: identity.pinned_instance,
            manager_pub: identity.pinned_manager,
            self_device_id: identity.device_id,
            expected_control_rev,
            now,
        }
    }
}

/// 吊销成功后的 relay 事实（`control_rev` 不变，`auth_rev` 已前进）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RevokeOutcome {
    /// 被吊销的目标设备。
    pub target: DeviceId,
    /// 本次请求是否真的改变了状态；已 revoked 的重复请求 → `false`（`auth_rev` 不二次推进）。
    pub changed: bool,
    /// 吊销后的 `vault.auth_rev`（全 vault 旧 token 已失效）。
    pub auth_rev: u64,
    /// `vault.control_rev`（独立吊销**不改变**它，也不追加占位控制记录）。
    pub control_rev: u64,
}

/// 管理端吊销一台设备的数据访问权（ADR 0010）。
///
/// 顺序是语义相关的：本地守卫（本设备 / 管理公钥 pin）→ 管理认证 → 签名请求 →
/// 响应逐字段校验。**任何失败都不产生本地副作用**：不删身份、不删密钥、不改笔记。
///
/// 错误映射沿用既有 [`SyncError`]：relay 的业务错误（`NOT_FOUND` / `INVALID_STATE` /
/// `ROTATING` / `CAS_CONFLICT` / `ID_CONFLICT` / `REVOKED` / `MANAGEMENT_REQUIRED` …）
/// 经 `error_from_response` 变成 [`SyncError::RelayApi`]；本设备/错管理 key 是本地
/// [`SyncError::Unsupported`]，此时**不会发任何 HTTP 请求**。
pub fn revoke_device<T: HttpTransport>(
    transport: &mut T,
    ctx: &RevokeContext,
    manager: &ManagementKey,
    target: DeviceId,
    request_id: [u8; 16],
) -> Result<RevokeOutcome, SyncError> {
    // ① 禁止 self-remove：先于任何网络动作拒绝（ADR 0010 客户端硬约束）。
    if target == ctx.self_device_id {
        return Err(SyncError::Unsupported(
            "禁止吊销本设备：会立即自断同步/管理访问（ADR 0010）；请用另一台管理设备或恢复码",
        ));
    }
    // ② 管理 key 必须与身份 pin 一致。类型上已保证"有 key"，这里拒绝"错 key"：
    //    用错管理私钥去撞 relay 只会得到 MANAGEMENT_REQUIRED，还会把恢复码/身份
    //    对不上的问题掩盖成 relay 拒绝。
    if manager.public_key() != ctx.manager_pub {
        return Err(SyncError::Unsupported(
            "管理私钥与本地身份 pin 的管理公钥不符（恢复码/身份不匹配）；拒绝发送吊销请求",
        ));
    }

    let base = ctx.base_url.trim_end_matches('/');
    // ③ 管理 realm bearer。challenge 响应里的 instance 必须等于本地 pin。
    let (token, instance_id) =
        crate::recovery_enroll::management_token(transport, base, ctx.vault_id, manager, ctx.now)?;
    if instance_id != ctx.instance_id {
        return Err(SyncError::RelayWire("relay instance 与本地 pin 不符"));
    }

    // ④ canonical body `B`（业务字段只在这份被签 body 里；外层 1/2 是同一份事实的镜像）。
    let payload = cbor::encode(vec![
        (0, cbor::bytes(target.to_vec())),
        (1, cbor::bytes(request_id.to_vec())),
        (2, cbor::uint(ctx.expected_control_rev)),
    ])?;
    let path = format!(
        "/v1/vaults/{}/devices/{}/revoke",
        crate::base64url::encode(&ctx.vault_id),
        crate::base64url::encode(&target)
    );
    // ⑤ 签名绑定 POST=2/完整 path/instance/vault/request_id/expected_control_rev。
    //    复用管理写签名函数，绝不在这里复制一份签名输入拼接。
    let (signed, _signature) = crate::rotation_stage::sign_management(
        manager,
        &ctx.instance_id,
        ctx.vault_id,
        request_id,
        ctx.expected_control_rev,
        &path,
        &payload,
    )?;
    let body = cbor::encode(vec![
        (0, signed),
        (1, cbor::bytes(request_id.to_vec())),
        (2, cbor::uint(ctx.expected_control_rev)),
    ])?;
    let response = crate::recovery_enroll::send(
        transport,
        "POST",
        format!("{base}{path}"),
        Some(&token),
        body,
    )?;

    // ⑥ 响应逐字段校验：target 必须与请求一致；`control_rev` 必须没变（CAS 的另一半）；
    //    业务状态字面量按 ADR 固定为 `"revoked"`。
    let fields = crate::recovery_enroll::decode(&response)?;
    let echoed_target: [u8; 16] = cbor::bytes_fixed(&fields, 0)?;
    if echoed_target != target {
        return Err(SyncError::RelayWire("吊销响应的 target 与请求不一致"));
    }
    let status = cbor::text_field(&fields, 1)?;
    if status != "revoked" {
        return Err(SyncError::RelayWire("吊销响应的状态字面量不是 revoked"));
    }
    let changed = cbor::bool_field(&fields, 2)?;
    let auth_rev = cbor::uint_field(&fields, 3)?;
    let control_rev = cbor::uint_field(&fields, 4)?;
    if control_rev != ctx.expected_control_rev {
        return Err(SyncError::RelayWire(
            "吊销响应的 control_rev 与请求不一致（控制链被并发推进？）",
        ));
    }
    Ok(RevokeOutcome {
        target,
        changed,
        auth_rev,
        control_rev,
    })
}
