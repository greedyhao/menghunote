//! §26：管理写只解析**唯一 canonical body**，并实现 `request_id + body_digest` 幂等。
//!
//! 背景（ADR 0006 §26 / 评审 0001 §一 7）：
//! 此前管理写端点把业务字段解析成**两份**——外层 `expected_epoch` / `revoke[]` /
//! `new_manager` 与签名里不透明的 `unsigned_body`——签名只覆盖后者，执行却用前者，
//! 于是"签一套、执行另一套"可能成立。本模块把规则收口成三条：
//!
//! 1. 业务字段**只从 canonical body 读**（`CanonicalBody::*`），签名输入由这份 body 的
//!    原始字节重新 CDE 得到（`verify.rs` 的 `Expectation::Management`，不另行拼装）；
//! 2. 为兼容旧 wire 保留的外层镜像字段必须与 body 对应字段**逐字节一致**，任一不符 → 403；
//! 3. 成功执行后把 `(request_id, SHA256(body))` 与**响应体字节**写进 `dedup`（同一事务），
//!    重放同 request_id 同 body → 原样回放；同 request_id 不同 body → 409。
//!
//! canonical body 的字段编号（v1，键从 0 连续编号；relay 侧唯一定义处）：
//!
//! | 端点 | body 字段 |
//! | --- | --- |
//! | `POST /v1/vaults/{v}/rotations` | 0 expected_epoch, 1 expected_control_rev, 2 request_id, 3 revoke[] |
//! | `POST .../rotations/{r}/activate` | 0 new_manager{0 transition,1 old_sig?,2 new_sig?}, 1 primary_checkpoint, 2 fallback_checkpoint |
//! | `PUT .../rotations/{r}/grants/{d}` | 见 [`crate::rotations::GrantInput`]：0 grant_id, 1 hpke_enc, 2 hpke_ciphertext, 3 signed_header, 4 signature, 5 manager_rev |
//! | `POST .../rotations/{r}/abort` | （空 body） |
//! | `POST .../key-slots/password/activate` | 0 expected_slot_set_rev, 1 new_slot_id（设备签名覆盖本 body） |
//! | `POST .../invites` | 0 token_hash, 1 expires_at? |
//! | `POST .../devices/{d}/approve` | 0 certificate, 1 certificate_signature, 2 grant_id, 3 hpke_enc, 4 hpke_ciphertext, 5 grant_signature, 6 request_id, 7 expected_control_rev |
//!
//! 说明：grant staging 没有独立的"不透明 body"——请求 map 本身即 canonical 字段集，
//! 其签名输入由 relay 从这些字段重新构造 `GrantAadV1 || enc || SHA256(ciphertext)`；
//! 任一所消费字段被替换都会让验签失败（§17.2/§24.2），因此不存在双份载荷。
//!
//! 幂等 scope 按端点区分（`management/begin` 等）：管理签名本身绑定 path，跨端点复用同一
//! request_id 不构成重放，也不应被误判为 409。

use rusqlite::OptionalExtension;

use crate::auth::{self, ManagementProof};
use crate::error::{Error, ErrorCode, Result};
use crate::ids::{sha256, Hash32, Id16};
use crate::scope::ScopeConn;
use crate::sig::SignatureHook;
use crate::time::Clock;
use crate::wire;
use menghunote_sync_proto as proto;

/// `dedup.scope` 前缀；具体端点再拼 `/<name>`。
pub const DEDUP_PREFIX: &str = "management";

fn invalid() -> Error {
    Error::api(ErrorCode::InvalidRequest)
}

/// 外层镜像与 canonical body 不一致 → 403（ADR §26：任一不符即 403）。
fn mirror_conflict(field: &'static str) -> Error {
    let _ = field;
    Error::api(ErrorCode::Forbidden)
}

/// 解码后的 canonical unsigned body（`wire::Signed` 的 `payload` 必须是 CDE map）。
#[derive(Debug, Clone)]
pub struct CanonicalBody {
    /// 被签名的**原始字节**（签名输入直接用它，不重新拼装）。
    pub payload: Vec<u8>,
    /// 解码后的字段（业务字段唯一来源）。
    pub fields: wire::Fields,
    /// 管理/设备对 `payload` 的签名。
    pub signature: Vec<u8>,
}

impl CanonicalBody {
    /// 从外层 map 的签名包装（`{0: payload, 1: signature}`）解析唯一 canonical body。
    pub fn parse(outer: &wire::Fields, signed_key: u64) -> Result<Self> {
        let signed = wire::Signed::from_fields(outer, signed_key)?;
        let fields = signed.payload_fields()?;
        Ok(Self {
            payload: signed.payload,
            fields,
            signature: signed.signature,
        })
    }

    /// body 中的必填 u64。
    pub fn u64(&self, key: u64) -> Result<u64> {
        wire::uint_field(&self.fields, key)
    }

    /// body 中的可选 u64。
    pub fn opt_u64(&self, key: u64) -> Result<Option<u64>> {
        wire::opt_uint_field(&self.fields, key)
    }

    /// body 中的必填 16 字节 ID。
    pub fn id16(&self, key: u64) -> Result<Id16> {
        wire::id16_field(&self.fields, key)
    }

    /// body 中的可选 16 字节 ID。
    pub fn opt_id16(&self, key: u64) -> Result<Option<Id16>> {
        match wire::opt_field(&self.fields, key) {
            None => Ok(None),
            Some(wire::CdeValue::Bytes(value)) => {
                Ok(Some(value.as_slice().try_into().map_err(|_| invalid())?))
            }
            Some(_) => Err(invalid()),
        }
    }

    /// body 中的必填 32 字节 hash。
    pub fn hash32(&self, key: u64) -> Result<Hash32> {
        wire::hash32_field(&self.fields, key)
    }

    /// body 中的可选 32 字节 hash。
    pub fn opt_hash32(&self, key: u64) -> Result<Option<Hash32>> {
        match wire::opt_field(&self.fields, key) {
            None => Ok(None),
            Some(wire::CdeValue::Bytes(value)) => {
                Ok(Some(value.as_slice().try_into().map_err(|_| invalid())?))
            }
            Some(_) => Err(invalid()),
        }
    }

    /// body 中的必填 byte string。
    pub fn bytes(&self, key: u64) -> Result<Vec<u8>> {
        wire::bytes_field(&self.fields, key)
    }

    /// body 中的必填嵌套 map。
    pub fn map(&self, key: u64) -> Result<wire::Fields> {
        wire::map_field(&self.fields, key)
    }

    /// body 中的可选嵌套 map。
    pub fn opt_map(&self, key: u64) -> Result<Option<wire::Fields>> {
        wire::opt_map_field(&self.fields, key)
    }

    /// body 中的必填数组。
    pub fn array(&self, key: u64) -> Result<Vec<wire::CdeValue>> {
        wire::array_field(&self.fields, key)
    }

    /// body 的 SHA256（幂等键）。
    pub fn digest(&self) -> Hash32 {
        sha256(&self.payload)
    }
}

/// 两个 CDE 值是否**逐字节一致**（重新 canonical 编码后比较）。
fn same_bytes(left: &wire::CdeValue, right: &wire::CdeValue) -> Result<bool> {
    let left = proto::encode_cde(left).map_err(|_| invalid())?;
    let right = proto::encode_cde(right).map_err(|_| invalid())?;
    Ok(left == right)
}

/// 外层镜像字段 vs canonical body 字段：外层可以缺省（新客户端），一旦出现必须逐字节一致。
fn mirror(
    outer: &wire::Fields,
    outer_key: u64,
    body: &CanonicalBody,
    body_key: u64,
) -> Result<Option<wire::CdeValue>> {
    let outer_value = wire::opt_field(outer, outer_key);
    let body_value = wire::opt_field(&body.fields, body_key);
    match (outer_value, body_value) {
        (None, None) => Ok(None),
        (None, Some(value)) => Ok(Some(value.clone())),
        // 外层有、body 没有：无法证明两者一致 → 拒绝（不能"外层另行解析后执行"）。
        (Some(_), None) => Err(mirror_conflict("outer without body")),
        (Some(outer_value), Some(body_value)) => {
            if !same_bytes(outer_value, body_value)? {
                return Err(mirror_conflict("outer mismatch"));
            }
            Ok(Some(body_value.clone()))
        }
    }
}

/// 必填 u64：只从 canonical body 读；外层若存在镜像必须一致。
pub fn u64_field(
    outer: &wire::Fields,
    outer_key: u64,
    body: &CanonicalBody,
    body_key: u64,
) -> Result<u64> {
    match mirror(outer, outer_key, body, body_key)? {
        Some(wire::CdeValue::UInt(value)) => Ok(value),
        Some(_) => Err(invalid()),
        None => Err(invalid()),
    }
}

/// 可选 u64（body 可省略；外层若存在镜像必须一致）。
pub fn opt_u64_field(
    outer: &wire::Fields,
    outer_key: u64,
    body: &CanonicalBody,
    body_key: u64,
) -> Result<Option<u64>> {
    match mirror(outer, outer_key, body, body_key)? {
        Some(wire::CdeValue::UInt(value)) => Ok(Some(value)),
        Some(_) => Err(invalid()),
        None => Ok(None),
    }
}

/// 必填 16 字节 ID。
pub fn id16_field(
    outer: &wire::Fields,
    outer_key: u64,
    body: &CanonicalBody,
    body_key: u64,
) -> Result<Id16> {
    match mirror(outer, outer_key, body, body_key)? {
        Some(wire::CdeValue::Bytes(value)) => value.as_slice().try_into().map_err(|_| invalid()),
        Some(_) => Err(invalid()),
        None => Err(invalid()),
    }
}

/// 必填 32 字节 hash。
pub fn hash32_field(
    outer: &wire::Fields,
    outer_key: u64,
    body: &CanonicalBody,
    body_key: u64,
) -> Result<Hash32> {
    match mirror(outer, outer_key, body, body_key)? {
        Some(wire::CdeValue::Bytes(value)) => value.as_slice().try_into().map_err(|_| invalid()),
        Some(_) => Err(invalid()),
        None => Err(invalid()),
    }
}

/// 必填 byte string。
pub fn bytes_field(
    outer: &wire::Fields,
    outer_key: u64,
    body: &CanonicalBody,
    body_key: u64,
) -> Result<Vec<u8>> {
    match mirror(outer, outer_key, body, body_key)? {
        Some(wire::CdeValue::Bytes(value)) => Ok(value),
        Some(_) => Err(invalid()),
        None => Err(invalid()),
    }
}

/// 可选嵌套 map。
pub fn opt_map_field(
    outer: &wire::Fields,
    outer_key: u64,
    body: &CanonicalBody,
    body_key: u64,
) -> Result<Option<wire::Fields>> {
    match mirror(outer, outer_key, body, body_key)? {
        Some(wire::CdeValue::Map(value)) => Ok(Some(value)),
        Some(_) => Err(invalid()),
        None => Ok(None),
    }
}

/// 必填数组。
pub fn array_field(
    outer: &wire::Fields,
    outer_key: u64,
    body: &CanonicalBody,
    body_key: u64,
) -> Result<Vec<wire::CdeValue>> {
    match mirror(outer, outer_key, body, body_key)? {
        Some(wire::CdeValue::Array(value)) => Ok(value),
        Some(_) => Err(invalid()),
        None => Err(invalid()),
    }
}

/// 幂等 scope（端点级）。
pub fn scope(endpoint: &str) -> String {
    format!("{DEDUP_PREFIX}/{endpoint}")
}

/// 一次管理写的固定输入。
pub struct ManagementCall<'a> {
    /// 端点级幂等 scope（[`scope`]）。
    pub scope: &'a str,
    /// `GET=1/POST=2/PUT=3/PATCH=4/DELETE=5`（§15）。
    pub method_tag: u8,
    /// 规范化请求路径。
    pub path: &'a str,
    /// 外层 request_id（同时必须等于 body 内的 request_id，若有）。
    pub request_id: Id16,
    /// 外层 `expected_control_rev`（签名输入的一部分）。
    pub expected_control_rev: u64,
    /// 唯一 canonical body。
    pub body: &'a CanonicalBody,
}

/// 管理写结果。
pub enum ManagedOutcome {
    /// 首次执行：handler 用这组字段编码响应（状态码由端点固定）。
    Fresh(wire::Fields),
    /// 同一 request_id + 同一 body 的重放：原样回放上次响应体（字节一致）。
    Replayed(Vec<u8>),
}

/// 读幂等记录：`(request_hash, response_bytes)`。
pub fn lookup(
    conn: &ScopeConn,
    scope: &str,
    request_id: &Id16,
) -> Result<Option<(Hash32, Vec<u8>)>> {
    let mut stmt = conn.prepare(
        "SELECT request_hash, response FROM dedup
         WHERE scope = ?1 AND id = ?2 AND vault_id = ?3",
    )?;
    let row = stmt
        .query_row(
            (scope, request_id.as_slice(), conn.scope().as_slice()),
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((hash, response)) => Ok(Some((
            hash.as_slice()
                .try_into()
                .map_err(|_| Error::Invariant("dedup.request_hash"))?,
            response,
        ))),
    }
}

/// 按 request_id 跨全部管理 scope 找幂等结果（`GET rotations/current?request_id=`）。
///
/// 返回 `(scope, response_bytes)`；同一次请求只会落在端点的 scope 下，多命中时取最新。
pub fn lookup_any(conn: &ScopeConn, request_id: &Id16) -> Result<Option<(String, Vec<u8>)>> {
    let mut stmt = conn.prepare(
        "SELECT scope, response FROM dedup
         WHERE id = ?1 AND scope LIKE ?2 AND vault_id = ?3
         ORDER BY created_at DESC LIMIT 1",
    )?;
    let row = stmt
        .query_row(
            (
                request_id.as_slice(),
                format!("{DEDUP_PREFIX}/%"),
                conn.scope().as_slice(),
            ),
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    Ok(row)
}

/// 管理写统一入口：验签 → 幂等查询 → CAS → 执行 → 同事务缓存响应。
///
/// 顺序是安全相关的：
/// - 验签**先于**幂等查询，未授权者不能靠 request_id 读到别人的响应；
/// - 幂等查询先于 CAS，因为成功执行本身会推进 `control_rev`，重放必须仍能找回结果；
/// - 缓存写入与业务写入在同一个 `BEGIN IMMEDIATE` 事务里（handler 的 `Db::write`）。
pub fn run(
    conn: &ScopeConn,
    clock: &dyn Clock,
    hook: &dyn SignatureHook,
    call: &ManagementCall<'_>,
    execute: impl FnOnce() -> Result<wire::Fields>,
) -> Result<ManagedOutcome> {
    // ① 管理签名（签名输入 = MN/v1/manage\0 || instance || vault || request_id ||
    //    expected_control_rev || SHA256(CDE{method,path,canonical_body})）。
    auth::verify_management(
        conn,
        hook,
        &ManagementProof {
            method_tag: call.method_tag,
            path: call.path,
            request_id: call.request_id,
            expected_control_rev: call.expected_control_rev,
            signature: &call.body.signature,
        },
        &call.body.payload,
    )?;
    let request_hash = call.body.digest();
    // ② request_id + body_digest 幂等。
    if let Some((stored_hash, response)) = lookup(conn, call.scope, &call.request_id)? {
        if stored_hash != request_hash {
            // 同 request_id 不同 body：既不能回放旧结果，也不能执行新结果。
            return Err(Error::api(ErrorCode::IdConflict));
        }
        return Ok(ManagedOutcome::Replayed(response));
    }
    // ③ control_rev CAS（签名已绑定该值；这里比较服务端当前值）。
    auth::require_control_rev(conn, call.expected_control_rev)?;
    // ④ 执行（业务写），同一事务内缓存响应。
    let fields = execute()?;
    let encoded = wire::encode(fields.clone())?;
    conn.execute(
        "INSERT INTO dedup(vault_id, scope, id, request_hash, accepted_seq, response, created_at)
         VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
        (
            conn.scope().as_slice(),
            call.scope,
            call.request_id.as_slice(),
            request_hash.as_slice(),
            encoded.as_slice(),
            clock.now_secs(),
        ),
    )?;
    Ok(ManagedOutcome::Fresh(fields))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cbody(fields: wire::Fields) -> CanonicalBody {
        let payload = wire::encode(fields.clone()).expect("body encode");
        CanonicalBody {
            payload,
            fields,
            signature: vec![0xAA; 64],
        }
    }

    #[test]
    fn mirror_accepts_absent_or_identical_outer_fields() {
        let body = cbody(vec![(0, wire::uint(7)), (1, wire::bytes([1u8; 16]))]);
        // 外层缺省 → 用 body。
        let empty: wire::Fields = Vec::new();
        assert_eq!(u64_field(&empty, 0, &body, 0).expect("body only"), 7);
        // 外层一致 → 接受。
        let outer = vec![(0, wire::uint(7))];
        assert_eq!(u64_field(&outer, 0, &body, 0).expect("mirror ok"), 7);
        assert_eq!(
            id16_field(&outer, 5, &body, 1).expect("no outer"),
            [1u8; 16]
        );
    }

    #[test]
    fn mirror_rejects_any_byte_difference_with_403() {
        // 外层 u64 与 body 不同 → 403。
        let body = cbody(vec![(0, wire::uint(7))]);
        let outer = vec![(0, wire::uint(8))];
        let err = u64_field(&outer, 0, &body, 0).expect_err("mismatch");
        assert!(matches!(err, Error::Api(api) if api.code == ErrorCode::Forbidden));

        // 外层 revoke 列表与 body 不同（顺序/成员任一不同）→ 403。
        let revoke = vec![wire::CdeValue::Bytes(vec![1u8; 16])];
        let body = cbody(vec![(0, wire::CdeValue::Array(revoke))]);
        let other = vec![(
            0,
            wire::CdeValue::Array(vec![wire::CdeValue::Bytes(vec![2u8; 16])]),
        )];
        let err = array_field(&other, 0, &body, 0).expect_err("revoke mismatch");
        assert!(matches!(err, Error::Api(api) if api.code == ErrorCode::Forbidden));

        // 外层有、body 没有 → 403（不能只执行外层）。
        let body = cbody(vec![(0, wire::uint(7))]);
        let outer = vec![(1, wire::uint(7))];
        let err = u64_field(&outer, 1, &body, 1).expect_err("outer-only");
        assert!(matches!(err, Error::Api(api) if api.code == ErrorCode::Forbidden));
    }

    #[test]
    fn canonical_body_digest_is_the_signed_payload_digest() {
        let body = cbody(vec![(0, wire::uint(1)), (1, wire::uint(2))]);
        assert_eq!(body.digest(), sha256(&body.payload));
        // 语义相同但字段顺序不同：CDE 编码唯一 → digest 不变。
        let reordered = cbody(vec![(1, wire::uint(2)), (0, wire::uint(1))]);
        assert_eq!(body.digest(), reordered.digest());
    }
}
