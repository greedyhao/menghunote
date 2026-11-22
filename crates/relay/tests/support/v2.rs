//! ADR 0015 **S4b1** 测试夹具：真实 manager/设备密钥 + V2 控制提交与 `/authz/*` 的 HTTP 驱动。
//!
//! 设计口径（任务书）：
//! - 签名一律由**真实 Ed25519**私钥产生，签名输入复用 `sync-proto` 的域函数
//!   （`control_signing_input`/`upgrade_signing_input`/`recover_signing_input`/`auth_signing_input_v2`），
//!   测试不复刻协议逻辑；
//! - 生产路径走**真实 relay 路由**（`router` + `oneshot`），不做模块内联调用；
//! - 无 sleep：所有断言都在 HTTP 响应收敛点之后；时间用 `FakeClock` 推进。

#![allow(dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use ed25519_dalek::{Signer as _, SigningKey};
use http_body_util::BodyExt;
use relay::api::{router, AppState};
use relay::ids::{hex_lower, Id16};
use relay::verify::Ed25519Hook;
use relay::wire;
use tower::ServiceExt;

use menghunote_sync_proto as proto;

use super::*;

/// 两台追加 ACTIVE 设备：`(device_id, 私钥 seed)`（与 genesis 设备一起构成三台 ACTIVE）。
pub const EXTRA_DEVICES: [(Id16, [u8; 32]); 2] =
    [([0x2Au8; 16], [0x21u8; 32]), ([0x2Bu8; 16], [0x22u8; 32])];
/// 新离线恢复根的 seed（与 manager/genesis 都不同）。
pub const RECOVERY_SEED: [u8; 32] = [0x5Cu8; 32];

/// V2 测试夹具：真实 manager + genesis 设备 + 两台可自行签名的 ACTIVE 设备。
pub struct V2Fixture {
    pub fx: Fixture,
    pub extra: Vec<(Id16, SigningKey)>,
    pub recovery: SigningKey,
}

impl V2Fixture {
    pub fn new(label: &str) -> Self {
        let fx = Fixture::with_real_manager(label);
        let vault_id = fx.vault_id;
        let mut extra = Vec::new();
        for (device_id, seed) in EXTRA_DEVICES {
            let signing = SigningKey::from_bytes(&seed);
            let sign_pk = signing.verifying_key().to_bytes();
            let hpke_pk = [seed[0] ^ 0x5A; 32];
            let certificate = certificate_bytes(&vault_id, &device_id, &sign_pk, &hpke_pk, 1);
            let db = fx.db.clone();
            db.write_blocking(move |conn| {
                conn.execute(
                    "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate,
                                        certificate_signature, status, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'active', ?6)",
                    (
                        conn.scope().as_slice(),
                        device_id.as_slice(),
                        sign_pk.as_slice(),
                        hpke_pk.as_slice(),
                        certificate,
                        BASE_TIME,
                    ),
                )?;
                Ok(())
            })
            .expect("insert extra active device");
            extra.push((device_id, signing));
        }
        Self {
            fx,
            extra,
            recovery: SigningKey::from_bytes(&RECOVERY_SEED),
        }
    }

    pub fn vault_id(&self) -> Id16 {
        self.fx.vault_id
    }

    pub fn now(&self) -> i64 {
        self.fx.clock_now()
    }

    pub fn now_u64(&self) -> u64 {
        u64::try_from(self.now()).expect("now")
    }

    pub fn vault(&self) -> relay::vault::Vault {
        self.fx.vault()
    }

    pub fn head(&self) -> (u64, [u8; 32]) {
        self.fx
            .db
            .read(relay::vault::control_head)
            .expect("control head")
    }

    pub fn manager_key(&self) -> &SigningKey {
        self.fx
            .manager_signing_key
            .as_ref()
            .expect("V2Fixture 需要真实 manager 私钥")
    }

    pub fn genesis_device(&self) -> Id16 {
        self.fx.device
    }

    pub fn genesis_key(&self) -> &SigningKey {
        &self.fx.device_signing_key
    }

    pub fn sign_pk(&self, device_id: &Id16) -> [u8; 32] {
        if *device_id == self.fx.device {
            return self.fx.device_signing_key.verifying_key().to_bytes();
        }
        self.extra
            .iter()
            .find(|(id, _)| id == device_id)
            .map(|(_, key)| key.verifying_key().to_bytes())
            .expect("unknown test device")
    }

    pub fn key(&self, device_id: &Id16) -> &SigningKey {
        if *device_id == self.fx.device {
            return &self.fx.device_signing_key;
        }
        &self
            .extra
            .iter()
            .find(|(id, _)| id == device_id)
            .expect("unknown test device")
            .1
    }

    /// 全部 ACTIVE 设备的完整 `DeviceCertV2`（device_id 升序；升级 manifest 的默认值）。
    pub fn active_manifest(&self) -> Vec<Vec<u8>> {
        let vault = self.vault();
        let rows = self
            .fx
            .db
            .read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT device_id, sign_pk, hpke_pk FROM devices
                     WHERE vault_id = ?1 AND status = 'active' ORDER BY device_id ASC",
                )?;
                let rows = stmt.query_map([conn.scope().as_slice()], |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    let (device_id, sign_pk, hpke_pk) = row?;
                    out.push((device_id, sign_pk, hpke_pk));
                }
                Ok(out)
            })
            .expect("active devices");
        rows.into_iter()
            .map(|(device_id, sign_pk, hpke_pk)| {
                proto::DeviceCertV2 {
                    version: proto::V2_WIRE_VERSION,
                    instance_id: vault.instance_id,
                    vault_id: vault.vault_id,
                    device_id: device_id.as_slice().try_into().expect("device id"),
                    sign_ed25519_pub: sign_pk.as_slice().try_into().expect("sign pk"),
                    hpke_x25519_pub: hpke_pk.as_slice().try_into().expect("hpke pk"),
                    cert_serial: 1,
                    data_rights: proto::Rights::new(proto::Rights::ALL).expect("rights"),
                    issued_authz_rev: 1,
                }
                .encode()
                .expect("cert encode")
            })
            .collect()
    }

    /// 默认 `PolicyV2`：门限固定 2，TTL 取 ADR 上限。
    pub fn default_policy(&self) -> proto::PolicyV2 {
        proto::PolicyV2 {
            version: proto::V2_WIRE_VERSION,
            highrisk_threshold: proto::PolicyV2::HIGHRISK_THRESHOLD,
            min_quorum: proto::PolicyV2::MIN_QUORUM,
            max_cap_ttl: proto::MAX_CAP_TTL_SECS_V2,
            max_invite_ttl: proto::MAX_INVITE_TTL_SECS_V2,
        }
    }

    /// 升级用的 bootstrap cap（rev 绑定到提交后 control rev，authz/authority = 1）。
    pub fn bootstrap_cap(
        &self,
        cap_id: [u8; 16],
        device_id: Id16,
        scopes: u32,
        ttl_secs: u64,
    ) -> proto::ManagementCapV2 {
        let vault = self.vault();
        let (head_rev, _) = self.head();
        proto::ManagementCapV2 {
            version: proto::V2_WIRE_VERSION,
            instance_id: vault.instance_id,
            vault_id: vault.vault_id,
            cap_id,
            subject_device_id: device_id,
            subject_ed_pk: self.sign_pk(&device_id),
            scopes: proto::CapScope::new(scopes).expect("scopes"),
            not_before: self.now_u64().saturating_sub(10),
            expires_at: self.now_u64().saturating_add(ttl_secs),
            issued_control_rev: head_rev + 1,
            issued_authz_rev: 1,
            issuer_authority_rev: 1,
            delegable: false,
        }
    }

    /// 默认升级材料：两台设备持全部 scope；manifest = 全部 ACTIVE 设备。
    pub fn default_upgrade_parts(&self) -> UpgradeParts {
        let vault = self.vault();
        let all_scopes = proto::CapScope::ALL;
        UpgradeParts {
            old_manager_rev: vault.manager_rev,
            policy: self.default_policy(),
            recovery_pk: self.recovery.verifying_key().to_bytes(),
            caps: vec![
                self.bootstrap_cap([0xC1; 16], self.extra[0].0, all_scopes, 86_400),
                self.bootstrap_cap([0xC2; 16], self.extra[1].0, all_scopes, 86_400),
            ],
            manifest: self.active_manifest(),
        }
    }

    /// 当前 head/rev 上的 `OperationV2`（唯一路径、not_after = signed_at+300）。
    pub fn base_operation(
        &self,
        kind: proto::ControlKindV2,
        request_id: [u8; 16],
        payload: Vec<u8>,
        signers: Vec<proto::SignerV2>,
        signed_at: u64,
    ) -> proto::OperationV2 {
        let vault = self.vault();
        let (head_rev, head_hash) = self.head();
        proto::OperationV2 {
            version: proto::V2_WIRE_VERSION,
            instance_id: vault.instance_id,
            vault_id: vault.vault_id,
            kind,
            request_id,
            expected_control_rev: head_rev,
            prev_control_hash: head_hash,
            expected_authz_rev: vault.authz_rev,
            authority_rev: vault.authority_rev,
            signed_at,
            not_after: signed_at + 300,
            canonical_path: proto::canonical_submit_path(&vault.vault_id),
            payload,
            signers: sort_signers(signers),
        }
    }

    /// 升级提交的默认 `OperationV2`（role0 ×2 + role1 + role2）。
    pub fn upgrade_operation(
        &self,
        request_id: [u8; 16],
        parts: &UpgradeParts,
        signed_at: u64,
    ) -> proto::OperationV2 {
        let payload = proto::UpgradePolicyPayloadV2 {
            old_manager_rev: parts.old_manager_rev,
            policy: parts.policy.clone(),
            fresh_recovery_pk: parts.recovery_pk,
            bootstrap_caps: parts.caps.clone(),
            active_manifest: parts
                .manifest
                .iter()
                .map(|bytes| proto::DeviceCertV2::decode(bytes).expect("manifest cert"))
                .collect(),
        }
        .encode()
        .expect("upgrade payload");
        let signers = vec![
            device_signer(self, self.extra[0].0),
            device_signer(self, self.extra[1].0),
            proto::SignerV2 {
                role: proto::SignerRole::CreatorUpgrade,
                device_id: None,
                cap_id: None,
                pk: self.manager_key().verifying_key().to_bytes(),
            },
            proto::SignerV2 {
                role: proto::SignerRole::OfflineRecovery,
                device_id: None,
                cap_id: None,
                pk: parts.recovery_pk,
            },
        ];
        let mut operation = self.base_operation(
            proto::ControlKindV2::UpgradePolicy,
            request_id,
            payload,
            signers,
            signed_at,
        );
        // S4b 冻结 §3：Upgrade 是首条 V2 记录，提交前 authz_rev=0、authority_rev=1。
        operation.expected_authz_rev = 0;
        operation.authority_rev = 1;
        operation
    }

    /// 两条 CAP_ADMIN 签名的操作（CapGrant/CapRevoke 用）。
    pub fn two_signer_operation(
        &self,
        kind: proto::ControlKindV2,
        request_id: [u8; 16],
        payload: Vec<u8>,
        signers: &[(Id16, Id16)],
        signed_at: u64,
    ) -> proto::OperationV2 {
        let signer_v2 = signers
            .iter()
            .map(|(device_id, cap_id)| proto::SignerV2 {
                role: proto::SignerRole::Device,
                device_id: Some(*device_id),
                cap_id: Some(*cap_id),
                pk: self.sign_pk(device_id),
            })
            .collect();
        self.base_operation(kind, request_id, payload, signer_v2, signed_at)
    }

    /// 一台 INVITE cap 的操作。
    pub fn one_signer_operation(
        &self,
        kind: proto::ControlKindV2,
        request_id: [u8; 16],
        payload: Vec<u8>,
        device_id: Id16,
        cap_id: Id16,
        signed_at: u64,
    ) -> proto::OperationV2 {
        self.two_signer_operation(kind, request_id, payload, &[(device_id, cap_id)], signed_at)
    }

    fn key_for_signer(&self, signer: &proto::SignerV2) -> &SigningKey {
        match signer.role {
            proto::SignerRole::Device | proto::SignerRole::TargetPop => {
                self.key(&signer.device_id.expect("device signer"))
            }
            proto::SignerRole::CreatorUpgrade => self.manager_key(),
            proto::SignerRole::OfflineRecovery => &self.recovery,
        }
    }

    /// 按 signer 角色用**对应真实私钥**签名（正常路径）。
    pub fn sign_operation(&self, operation: &proto::OperationV2) -> proto::SignedControlV2 {
        let keys = operation
            .signers
            .iter()
            .map(|signer| self.key_for_signer(signer))
            .collect::<Vec<_>>();
        self.sign_operation_with(operation, &keys)
    }

    /// 用指定私钥（顺序 = signers 顺序）签名；用于错误签名者/错误密钥用例。
    pub fn sign_operation_with(
        &self,
        operation: &proto::OperationV2,
        keys: &[&SigningKey],
    ) -> proto::SignedControlV2 {
        assert_eq!(
            keys.len(),
            operation.signers.len(),
            "签名数必须等于 signer 数"
        );
        let control = proto::control_signing_input(operation, hash_fn).expect("control input");
        let upgrade = proto::upgrade_signing_input(operation, hash_fn).expect("upgrade input");
        let recover = proto::recover_signing_input(operation, hash_fn).expect("recover input");
        let signatures = operation
            .signers
            .iter()
            .zip(keys)
            .map(|(signer, key)| {
                let input = match signer.role {
                    proto::SignerRole::Device => &control,
                    proto::SignerRole::CreatorUpgrade => &upgrade,
                    proto::SignerRole::OfflineRecovery => &recover,
                    proto::SignerRole::TargetPop => {
                        &proto::pop_signing_input(operation, &signer.pk, hash_fn)
                            .expect("pop input")
                    }
                };
                key.sign(input).to_bytes()
            })
            .collect();
        proto::SignedControlV2 {
            operation: operation.clone(),
            signatures,
        }
    }
}

/// 升级 payload 的可替换部分（负例测试改单个字段）。
#[derive(Clone)]
pub struct UpgradeParts {
    pub old_manager_rev: u64,
    pub policy: proto::PolicyV2,
    pub recovery_pk: [u8; 32],
    pub caps: Vec<proto::ManagementCapV2>,
    pub manifest: Vec<Vec<u8>>,
}

/// 从测试夹具构造 role0 signer。
pub fn device_signer(v2: &V2Fixture, device_id: Id16) -> proto::SignerV2 {
    proto::SignerV2 {
        role: proto::SignerRole::Device,
        device_id: Some(device_id),
        cap_id: None,
        pk: v2.sign_pk(&device_id),
    }
}

/// 按 proto 的 `(role, device, cap, pk)` 升序排 signer（None 视作空字节串）。
pub fn sort_signers(mut signers: Vec<proto::SignerV2>) -> Vec<proto::SignerV2> {
    signers.sort_by(|left, right| {
        left.role
            .as_u8()
            .cmp(&right.role.as_u8())
            .then_with(|| {
                left.device_id
                    .unwrap_or_default()
                    .cmp(&right.device_id.unwrap_or_default())
            })
            .then_with(|| {
                left.cap_id
                    .unwrap_or_default()
                    .cmp(&right.cap_id.unwrap_or_default())
            })
            .then_with(|| left.pk.cmp(&right.pk))
    });
    signers
}

pub fn hash_fn(bytes: &[u8]) -> [u8; 32] {
    relay::ids::sha256(bytes)
}

// ---------------------------------------------------------------- HTTP 驱动

/// 真实 relay 路由 + 生产 `Ed25519Hook`（v1 与 V2 签名都按密码学校验）。
pub fn app(v2: &V2Fixture) -> Router {
    router(AppState::new(
        v2.fx.db.inner().clone(),
        v2.fx.cfg.clone(),
        Arc::new(Ed25519Hook),
    ))
}

pub async fn call(app: &Router, request: Request<Body>) -> (StatusCode, Vec<u8>) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes()
        .to_vec();
    (status, bytes)
}

pub fn get(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("request")
}

pub fn post_cbor(uri: &str, token: Option<&str>, fields: wire::Fields) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, wire::CONTENT_TYPE);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder
        .body(Body::from(cbor_body(fields)))
        .expect("request")
}

pub fn post_raw(uri: &str, token: Option<&str>, body: Vec<u8>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, wire::CONTENT_TYPE);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::from(body)).expect("request")
}

/// `vault_id` 的小写 hex（控制提交唯一路径要求）。
pub fn hex_path(vault_id: &Id16) -> String {
    format!("/v2/vaults/{}", hex_lower(vault_id))
}

/// 提交一个 `SignedControlV2`，返回 `(status, body)`。
pub async fn submit(
    app: &Router,
    vault_id: &Id16,
    signed: &proto::SignedControlV2,
) -> (StatusCode, Vec<u8>) {
    let bytes = signed.encode().expect("signed control encode");
    let uri = format!("{}/control/submit", hex_path(vault_id));
    call(app, post_cbor(&uri, None, vec![(0, wire::bytes(bytes))])).await
}

/// 提交原始 body（未知 kind / 非 canonical 的负例）。
pub async fn submit_raw(app: &Router, vault_id: &Id16, body: Vec<u8>) -> (StatusCode, Vec<u8>) {
    let uri = format!("{}/control/submit", hex_path(vault_id));
    call(app, post_raw(&uri, None, body)).await
}

/// 用默认材料完成 Upgrade（返回回执字段）。
pub async fn upgrade_default(
    v2: &V2Fixture,
    app: &Router,
    request_id: [u8; 16],
) -> (StatusCode, Vec<u8>) {
    let parts = v2.default_upgrade_parts();
    let operation = v2.upgrade_operation(request_id, &parts, v2.now_u64());
    let signed = v2.sign_operation(&operation);
    submit(app, &v2.vault_id(), &signed).await
}

/// 从回执/错误体里取 code 字符串。
pub fn error_code(body: &[u8]) -> String {
    wire::text_field(&cbor_fields(body), 0).unwrap_or_default()
}

/// 回执字段快照。
pub struct ReceiptView {
    pub control_rev: u64,
    pub head_hash: [u8; 32],
    pub kind: u64,
    pub request_id: [u8; 16],
    pub authz_rev: u64,
    pub authority_rev: u64,
}

pub fn receipt(body: &[u8]) -> ReceiptView {
    let fields = cbor_fields(body);
    ReceiptView {
        control_rev: wire::uint_field(&fields, 0).expect("control_rev"),
        head_hash: wire::hash32_field(&fields, 1).expect("head hash"),
        kind: wire::uint_field(&fields, 2).expect("kind"),
        request_id: wire::id16_field(&fields, 3).expect("request id"),
        authz_rev: wire::uint_field(&fields, 4).expect("authz rev"),
        authority_rev: wire::uint_field(&fields, 5).expect("authority rev"),
    }
}

/// 通过 `/authz/challenges|tokens` 拿一个 V2 token（realm 1 或 3）。
pub async fn authz_login(
    app: &Router,
    vault_id: &Id16,
    device_id: Id16,
    signing: &SigningKey,
    cap_id: Option<Id16>,
) -> std::result::Result<String, (StatusCode, Vec<u8>)> {
    let realm = if cap_id.is_some() { 3u64 } else { 1u64 };
    let mut request = vec![(0, wire::uint(realm)), (1, wire::bytes(device_id.to_vec()))];
    if let Some(cap_id) = cap_id {
        request.push((2, wire::bytes(cap_id.to_vec())));
    }
    let (status, body) = call(
        app,
        post_cbor(
            &format!("{}/authz/challenges", hex_path(vault_id)),
            None,
            request,
        ),
    )
    .await;
    if status != StatusCode::OK {
        return Err((status, body));
    }
    let challenge_bytes = wire::bytes_field(&cbor_fields(&body), 0).expect("challenge bytes");
    let challenge = proto::ChallengeV2::decode(&challenge_bytes).expect("challenge decode");
    assert_eq!(challenge.subject, device_id);
    let signature = signing
        .sign(&proto::auth_signing_input_v2(&challenge_bytes))
        .to_bytes()
        .to_vec();
    let (status, body) = call(
        app,
        post_cbor(
            &format!("{}/authz/tokens", hex_path(vault_id)),
            None,
            vec![
                (0, wire::bytes(challenge_bytes)),
                (1, wire::bytes(signature)),
            ],
        ),
    )
    .await;
    if status != StatusCode::OK {
        return Err((status, body));
    }
    Ok(wire::text_field(&cbor_fields(&body), 0).expect("token"))
}

/// 读取当前 control/链状态（用 v1 GET /control，device token 可用）。
pub async fn read_control(app: &Router, vault_id: &Id16, token: &str) -> wire::Fields {
    let (status, body) = call(
        app,
        get(&format!("{}/control", hex_path(vault_id)), Some(token)),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "control read: {}",
        error_code(&body)
    );
    cbor_fields(&body)
}

/// 同 relay 里的第二个 vault（独立设备/密钥；默认 legacy，用于跨租户隔离负例）。
pub struct SecondVault {
    pub vault_id: Id16,
    pub genesis_device: Id16,
    pub genesis_key: SigningKey,
    pub extras: Vec<(Id16, SigningKey)>,
}

impl V2Fixture {
    /// 在同一 relay DB 里创建第二个 vault（vault 级 manager/设备/证书全部独立）。
    pub fn create_second_vault(&self, vault_id: Id16) -> SecondVault {
        let genesis_key = SigningKey::from_bytes(&[0x13u8; 32]);
        let genesis_device = [0x0Bu8; 16];
        let sign_pk = genesis_key.verifying_key().to_bytes();
        let hpke_pk = [0x14u8; 32];
        let certificate = certificate_bytes(&vault_id, &genesis_device, &sign_pk, &hpke_pk, 1);
        let cert = proto::DeviceCertV1::decode(&certificate).expect("device cert v1");
        let certificate_signature = self
            .manager_key()
            .sign(&proto::device_cert_signed_bytes(&cert).expect("cert signed bytes"))
            .to_bytes()
            .to_vec();
        let request = relay::vault::InitRequest {
            vault_id,
            device_id: genesis_device,
            sign_pk,
            hpke_pk,
            manager_pub: self.fx.manager_pub,
            setup_secret: "setup-secret".to_owned(),
            certificate,
            certificate_signature,
        };
        let db = self.fx.db.inner().clone();
        let clock = self.fx.clock.clone();
        let cfg = self.fx.cfg.clone();
        let scope_id = vault_id;
        db.write_blocking(relay::VaultScope::new(scope_id), move |conn| {
            relay::vault::create_vault_in_scope(
                conn,
                clock.as_ref(),
                &cfg,
                &Ed25519Hook,
                &request,
                false,
            )
            .map(|_| ())
        })
        .expect("create second vault");

        let mut extras = Vec::new();
        for (device_id, seed) in [([0x2Cu8; 16], [0x23u8; 32]), ([0x2Du8; 16], [0x24u8; 32])] {
            let signing = SigningKey::from_bytes(&seed);
            let sign_pk = signing.verifying_key().to_bytes();
            let hpke_pk = [seed[0] ^ 0x33; 32];
            let certificate = certificate_bytes(&vault_id, &device_id, &sign_pk, &hpke_pk, 1);
            let db = self.fx.db.inner().clone();
            db.write_blocking(relay::VaultScope::new(vault_id), move |conn| {
                conn.execute(
                    "INSERT INTO devices(vault_id, device_id, sign_pk, hpke_pk, certificate,
                                        certificate_signature, status, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'active', ?6)",
                    (
                        conn.scope().as_slice(),
                        device_id.as_slice(),
                        sign_pk.as_slice(),
                        hpke_pk.as_slice(),
                        certificate,
                        BASE_TIME,
                    ),
                )?;
                Ok(())
            })
            .expect("insert second-vault device");
            extras.push((device_id, signing));
        }
        SecondVault {
            vault_id,
            genesis_device,
            genesis_key,
            extras,
        }
    }
}
