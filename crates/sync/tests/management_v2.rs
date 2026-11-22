//! ADR 0016 **S4d1a + S4d1b** 集成测试：真实 dev relay（进程内 axum + 临时 SQLite + 真实 HTTP）。
//!
//! S4d1a 覆盖（全部真实 Ed25519、真实 CBOR、无 sleep、临时目录在 `target/tmp`）：
//!
//! 1. 用 relay 真实路由构造 legacy→Upgrade→CapGrant/Revoke→InviteCreate/Cancel→
//!    DeviceApprove/Revoke→RecoveryKeyChange/RecoveryReset 的完整控制链；
//! 2. 客户端 [`V2ManagementClient`] 从 0 回放到最新，状态/revs/caps/设备与 relay 投影一致
//!    （authz/authority rev 用 relay 的 `ChallengeV2` 交叉核对）；
//! 3. cap 选择：单张覆盖、撤销/旧代/过期/错 device 拒绝；
//! 4. realm3 challenge/token 正例 + 错 cap/旧代/未知 cap/跨 vault/challenge 重放/token 版本
//!    失效负例；token 用于真实鉴权请求；
//! 5. 增量回放 + durable pin 前进 + 重启恢复 + 损坏缓存不误信。
//!
//! S4d1b（`approval_s4d1b` 子模块）覆盖：`ApprovalEnvelopeV1` 文件/base64 往返与严格
//! 编解码、typed Operation 构造（Upgrade/CapGrant/Revoke/DeviceApprove/Revoke/Rights/
//! InviteCreate/Cancel）、双签状态机（A 签 → B 导入验证加签 → 提交）、`/control/submit`
//! 回执对账与幂等（丢响应重发原字节）、时间窗/CAS/跨 vault/路径篡改/导入限长/重启草稿。
//!
//! ⚠️ 非生产：dev relay 的"放行验签 hook"只影响 v1 旁路；**V2 控制提交**走 relay 内部的
//! 真实验签（`relay::sig::verify_ed25519`），因此 S4d1b 既验客户端拒绝，也在关键路径
//! （单签/零占位/错 PoP/陈旧 CAS/过期）直接打 relay 确认服务端同样拒绝。
//! 无线程 sleep，时间用 relay/客户端同步的假时钟推进。

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tempfile::{Builder as TempDirBuilder, TempDir};

use menghunote_dev_relay::DevRelay;
use menghunote_sync::api_version::{RelayApi, RelayApiVersion};
use menghunote_sync::cbor;
use menghunote_sync::dev_local::{CreatedVault, DevLocal};
use menghunote_sync::device::DeviceSigningKey;
use menghunote_sync::http_relay::Clock;
use menghunote_sync::http_transport::{
    HttpRequest, HttpTransport, StdTcpTransport, DEFAULT_TRANSPORT_TIMEOUTS,
};
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::invite_link::{
    approve_pending_device, create_invite_link_with, InviteLinkTarget,
};
use menghunote_sync::keys::Secret32;
use menghunote_sync::management_v2::{
    AuthzStateV2, ControlStateV2, ManagementTokenV2, V2ManagementClient, V2ManagementConfig,
    META_CONTROL_PIN_V2, META_MANAGEMENT_CACHE_V2,
};
use menghunote_sync::relay::RelayClient;
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::{DeviceId, VaultId};
use menghunote_sync_proto as proto;
use menghunote_sync_proto::CapScope;

// ---------------------------------------------------------------- 假时钟

/// 与 relay 的 `FakeClock` 同步推进的客户端时钟（无 sleep）。
#[derive(Clone, Default)]
struct TestClock(Arc<AtomicU64>);

impl TestClock {
    fn new(now: u64) -> Self {
        Self(Arc::new(AtomicU64::new(now)))
    }

    fn advance(&self, secs: u64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_secs(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------- 临时目录/夹具

fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("management-v2-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 测试根目录");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录")
}

/// 本测试自己掌控私钥的三台设备（A 是 Create 设备；B/C 用邀请兑换成 PENDING 后批准）。
struct Keys {
    a: DeviceSigningKey,
    b: DeviceSigningKey,
    c: DeviceSigningKey,
    recovery1: DeviceSigningKey,
    recovery2: DeviceSigningKey,
    recovery3: DeviceSigningKey,
}

impl Keys {
    /// A 的私钥必须来自 genesis 身份（`ed25519_seed`），B/C 是本次自行生成并兑换的。
    fn new(a: &CreatedVault) -> Self {
        Self {
            a: DeviceSigningKey::from_seed(a.identity.ed25519_seed.clone()),
            b: DeviceSigningKey::from_seed(Secret32::from_array([0xB1; 32])),
            c: DeviceSigningKey::from_seed(Secret32::from_array([0xC1; 32])),
            recovery1: DeviceSigningKey::from_seed(Secret32::from_array([0xD1; 32])),
            recovery2: DeviceSigningKey::from_seed(Secret32::from_array([0xD2; 32])),
            recovery3: DeviceSigningKey::from_seed(Secret32::from_array([0xD3; 32])),
        }
    }
}

struct Fixture {
    relay: DevRelay,
    _dir: TempDir,
    a: CreatedVault,
    /// 邀请兑换时提交的 B/C 公钥材料（批准后 relay 的登记值）。
    b_id: DeviceId,
    b_hpke: [u8; 32],
    c_id: DeviceId,
    c_hpke: [u8; 32],
    keys: Keys,
    clock: TestClock,
    ctx: RelayContext,
    api: RelayApi,
}

fn establish(prefix: &str) -> Fixture {
    let relay = DevRelay::start().expect("起进程内 dev relay");
    let dir = temp_root(prefix);
    let path = dir.path().join("a").join("vault.sqlite3");
    let a = DevLocal::create(relay.base_url(), relay.setup_secret(), &path, "A").expect("A create");
    DevLocal::publish_initial_pair(relay.base_url(), &a.vault, &a.identity, &a.recovery_code)
        .expect("A 发布初始基线");
    let ctx = RelayContext::local_dev(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("local_dev context");
    let api = RelayApi::new(relay.base_url(), RelayApiVersion::V2);
    let keys = Keys::new(&a);
    let now = relay.now() as u64;
    let mut transport = ctx.transport().expect("transport");

    // B：兑换邀请 → 批准（v1 管理写；此时 vault 仍是 legacy）。
    let b_id: DeviceId = [0x0B; 16];
    let b_hpke: [u8; 32] = [0x1B; 32];
    redeem(
        &mut transport,
        &api,
        a.vault_id,
        &ctx,
        &a,
        b_id,
        &keys.b,
        b_hpke,
        now,
    );
    let approved =
        approve_pending_device(&ctx, &a.vault, &a.identity, b_id, now + 1).expect("批准 B");
    assert!(approved.changed, "首次批准必须真的改变状态");

    // C：只兑换成 PENDING（S4d1b 之前由 kind 4 批准）。
    let c_id: DeviceId = [0x0C; 16];
    let c_hpke: [u8; 32] = [0x1C; 32];
    redeem(
        &mut transport,
        &api,
        a.vault_id,
        &ctx,
        &a,
        c_id,
        &keys.c,
        c_hpke,
        now + 2,
    );

    Fixture {
        relay,
        _dir: dir,
        a,
        b_id,
        b_hpke,
        c_id,
        c_hpke,
        keys,
        clock: TestClock::new(now + 3),
        ctx,
        api,
    }
}

/// 兑换邀请（公开路由，不需要 bearer）：body `{0 token,1 device_id,2 sign_pk,3 hpke_pk,
/// 4 proof,5 proof_signature}`。dev relay 放行验签，但仍按真实路由/结构落 PENDING。
#[allow(clippy::too_many_arguments)]
fn redeem(
    transport: &mut StdTcpTransport,
    api: &RelayApi,
    vault_id: VaultId,
    ctx: &RelayContext,
    a: &CreatedVault,
    device_id: DeviceId,
    signing: &DeviceSigningKey,
    hpke_pk: [u8; 32],
    now: u64,
) {
    let link =
        create_invite_link_with(ctx, &a.vault, &a.identity, 3_600, now).expect("创建邀请链接");
    let target = InviteLinkTarget::parse(&link.url).expect("解析邀请链接");
    let proof = format!("proof-{}", crate::hex_of(&device_id));
    let body = vec![
        (0, cbor::bytes(target.token().to_vec())),
        (1, cbor::bytes(device_id.to_vec())),
        (2, cbor::bytes(signing.public_key().to_vec())),
        (3, cbor::bytes(hpke_pk.to_vec())),
        (4, cbor::bytes(proof.as_bytes().to_vec())),
        (5, cbor::bytes(vec![0u8; 64])),
    ];
    let path = api.vault_path(
        &vault_id,
        &format!("/invites/{}/redeem", crate::hex_of(&target.invite_id())),
    );
    let (status, response) = post_cbor(transport, &path, body);
    assert_eq!(status, 201, "兑换邀请失败：{response:?}");
}

// ---------------------------------------------------------------- wire 辅助

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn post_cbor(
    transport: &mut StdTcpTransport,
    path: &str,
    fields: cbor::Fields,
) -> (u16, cbor::Fields) {
    let body = cbor::encode(fields).expect("CBOR 编码");
    let response = transport
        .send(&HttpRequest {
            method: "POST",
            path: path.to_owned(),
            headers: vec![("content-type".to_owned(), "application/cbor".to_owned())],
            body,
        })
        .expect("HTTP POST");
    let fields = cbor::decode(&response.body).unwrap_or_default();
    (response.status, fields)
}

fn get_bearer(transport: &mut StdTcpTransport, path: &str, token: &str) -> (u16, cbor::Fields) {
    let response = transport
        .send(&HttpRequest {
            method: "GET",
            path: path.to_owned(),
            headers: vec![("authorization".to_owned(), format!("Bearer {token}"))],
            body: Vec::new(),
        })
        .expect("HTTP GET");
    let fields = cbor::decode(&response.body).unwrap_or_default();
    (response.status, fields)
}

struct Receipt {
    rev: u64,
    head: [u8; 32],
    authz_rev: u64,
    authority_rev: u64,
}

fn submit(
    transport: &mut StdTcpTransport,
    api: &RelayApi,
    vault_id: VaultId,
    label: &str,
    signed: &proto::SignedControlV2,
) -> Receipt {
    let bytes = signed.encode().expect("SignedControlV2 编码");
    let path = proto::canonical_submit_path(&vault_id);
    let (status, fields) = post_cbor(transport, &path, vec![(0, cbor::bytes(bytes))]);
    assert_eq!(
        status,
        200,
        "控制提交失败（{label} {path}）：{:?}",
        cbor::text_field(&fields, 0)
    );
    let _ = api;
    Receipt {
        rev: cbor::uint_field(&fields, 0).expect("回执 control_rev"),
        head: cbor::bytes_fixed(&fields, 1).expect("回执 head_hash"),
        authz_rev: cbor::uint_field(&fields, 4).expect("回执 authz_rev"),
        authority_rev: cbor::uint_field(&fields, 5).expect("回执 authority_rev"),
    }
}

/// 读取 relay 侧 v2 `/control` 的链尾（realm1 设备 token，用于交叉核对）。
fn relay_realm1_token(
    transport: &mut StdTcpTransport,
    api: &RelayApi,
    vault_id: VaultId,
    device_id: DeviceId,
    signing: &DeviceSigningKey,
) -> (proto::ChallengeV2, String) {
    let (status, fields) = post_cbor(
        transport,
        &api.vault_path(&vault_id, "/authz/challenges"),
        vec![(0, cbor::uint(1)), (1, cbor::bytes(device_id.to_vec()))],
    );
    assert_eq!(status, 200, "realm1 challenge 失败");
    let challenge_bytes = cbor::bytes_field(&fields, 0).expect("challenge bytes");
    let challenge = proto::ChallengeV2::decode(&challenge_bytes).expect("challenge 解码");
    let signature = signing
        .sign(&proto::auth_signing_input_v2(&challenge_bytes))
        .to_vec();
    let (status, fields) = post_cbor(
        transport,
        &api.vault_path(&vault_id, "/authz/tokens"),
        vec![
            (0, cbor::bytes(challenge_bytes)),
            (1, cbor::bytes(signature)),
        ],
    );
    assert_eq!(status, 200, "realm1 token 失败");
    let token = cbor::text_field(&fields, 0).expect("token");
    (challenge, token)
}

fn device_signer(
    device_id: DeviceId,
    device_pk: [u8; 32],
    cap_id: Option<[u8; 16]>,
) -> proto::SignerV2 {
    proto::SignerV2 {
        role: proto::SignerRole::Device,
        device_id: Some(device_id),
        cap_id,
        pk: device_pk,
    }
}

fn root_signer(role: proto::SignerRole, pk: [u8; 32]) -> proto::SignerV2 {
    proto::SignerV2 {
        role,
        device_id: None,
        cap_id: None,
        pk,
    }
}

fn pop_signer(device_id: DeviceId, pk: [u8; 32]) -> proto::SignerV2 {
    proto::SignerV2 {
        role: proto::SignerRole::TargetPop,
        device_id: Some(device_id),
        cap_id: None,
        pk,
    }
}

fn sort_signers(mut signers: Vec<proto::SignerV2>) -> Vec<proto::SignerV2> {
    signers.sort_by(|left, right| {
        left.role
            .as_u8()
            .cmp(&right.role.as_u8())
            .then_with(|| left.device_id.cmp(&right.device_id))
            .then_with(|| left.cap_id.cmp(&right.cap_id))
            .then_with(|| left.pk.cmp(&right.pk))
    });
    signers
}

fn sign(operation: &proto::OperationV2, keys: &[&DeviceSigningKey]) -> proto::SignedControlV2 {
    let control = proto::control_signing_input(operation, hash_fn).expect("control input");
    let upgrade = proto::upgrade_signing_input(operation, hash_fn).expect("upgrade input");
    let recover = proto::recover_signing_input(operation, hash_fn).expect("recover input");
    let signatures = operation
        .signers
        .iter()
        .map(|signer| {
            let signing = keys
                .iter()
                .find(|key| key.public_key() == signer.pk)
                .unwrap_or_else(|| panic!("缺 {} 的私钥", hex_of(&signer.pk)));
            let input = match signer.role {
                proto::SignerRole::Device => control.clone(),
                proto::SignerRole::CreatorUpgrade => upgrade.clone(),
                proto::SignerRole::OfflineRecovery => recover.clone(),
                proto::SignerRole::TargetPop => {
                    proto::pop_signing_input(operation, &signer.pk, hash_fn).expect("pop input")
                }
            };
            signing.sign(&input)
        })
        .collect();
    proto::SignedControlV2 {
        operation: operation.clone(),
        signatures,
    }
}

fn hash_fn(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

fn policy() -> proto::PolicyV2 {
    proto::PolicyV2 {
        version: proto::V2_WIRE_VERSION,
        highrisk_threshold: proto::PolicyV2::HIGHRISK_THRESHOLD,
        min_quorum: proto::PolicyV2::MIN_QUORUM,
        max_cap_ttl: proto::MAX_CAP_TTL_SECS_V2,
        max_invite_ttl: proto::MAX_INVITE_TTL_SECS_V2,
    }
}

fn cert(
    fx: &Fixture,
    device_id: DeviceId,
    sign_pk: [u8; 32],
    hpke_pk: [u8; 32],
    serial: u64,
    issued_authz_rev: u64,
) -> proto::DeviceCertV2 {
    proto::DeviceCertV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: fx.a.identity.pinned_instance,
        vault_id: fx.a.vault_id,
        device_id,
        sign_ed25519_pub: sign_pk,
        hpke_x25519_pub: hpke_pk,
        cert_serial: serial,
        data_rights: proto::Rights::new(proto::Rights::ALL).expect("rights"),
        issued_authz_rev,
    }
}

fn grant_package(
    fx: &Fixture,
    epoch: u64,
    recipient: DeviceId,
    hpke_pk: [u8; 32],
    authority_rev: u64,
    grant_id: [u8; 16],
    issued_authz_rev: u64,
) -> proto::GrantPackageV2 {
    proto::GrantPackageV2 {
        aad: proto::GrantAadV2 {
            version: proto::V2_WIRE_VERSION,
            instance_id: fx.a.identity.pinned_instance,
            vault_id: fx.a.vault_id,
            epoch,
            recipient_device_id: recipient,
            recipient_hpke_pub: hpke_pk,
            authority_rev,
            grant_id,
            issued_authz_rev,
        },
        enc: [0x77u8; 32],
        ciphertext: vec![1, 2, 3, 4],
    }
}

#[allow(clippy::too_many_arguments)]
fn vault_cap(
    fx: &Fixture,
    cap_id: [u8; 16],
    device_id: DeviceId,
    sign_pk: [u8; 32],
    scopes: u32,
    issued_control_rev: u64,
    issued_authz_rev: u64,
    authority_rev: u64,
    not_before: u64,
    expires_at: u64,
) -> proto::ManagementCapV2 {
    proto::ManagementCapV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: fx.a.identity.pinned_instance,
        vault_id: fx.a.vault_id,
        cap_id,
        subject_device_id: device_id,
        subject_ed_pk: sign_pk,
        scopes: proto::CapScope::new(scopes).expect("scopes"),
        not_before,
        expires_at,
        issued_control_rev,
        issued_authz_rev,
        issuer_authority_rev: authority_rev,
        delegable: false,
    }
}

/// 当前链尾：`(rev, head_hash, authz_rev, authority_rev)`。
#[derive(Clone, Copy)]
struct Head {
    rev: u64,
    hash: [u8; 32],
    authz_rev: u64,
    authority_rev: u64,
}

fn head_after(receipt: &Receipt) -> Head {
    Head {
        rev: receipt.rev,
        hash: receipt.head,
        authz_rev: receipt.authz_rev,
        authority_rev: receipt.authority_rev,
    }
}

fn base_operation(
    fx: &Fixture,
    head: &Head,
    kind: proto::ControlKindV2,
    request_id: [u8; 16],
    payload: Vec<u8>,
    signers: Vec<proto::SignerV2>,
    signed_at: u64,
) -> proto::OperationV2 {
    proto::OperationV2 {
        version: proto::V2_WIRE_VERSION,
        instance_id: fx.a.identity.pinned_instance,
        vault_id: fx.a.vault_id,
        kind,
        request_id,
        expected_control_rev: head.rev,
        prev_control_hash: head.hash,
        expected_authz_rev: head.authz_rev,
        authority_rev: head.authority_rev,
        signed_at,
        not_after: signed_at + 300,
        canonical_path: proto::canonical_submit_path(&fx.a.vault_id),
        payload,
        signers: sort_signers(signers),
    }
}

fn two_sign(
    fx: &Fixture,
    head: &Head,
    kind: proto::ControlKindV2,
    request_id: [u8; 16],
    payload: Vec<u8>,
    signers: &[(DeviceId, [u8; 16])],
    signed_at: u64,
) -> proto::OperationV2 {
    let signer_v2 = signers
        .iter()
        .map(|(device_id, cap_id)| {
            let device_pk = if *device_id == fx.a.identity.device_id {
                fx.keys.a.public_key()
            } else if *device_id == fx.b_id {
                fx.keys.b.public_key()
            } else {
                fx.keys.c.public_key()
            };
            device_signer(*device_id, device_pk, Some(*cap_id))
        })
        .collect();
    base_operation(fx, head, kind, request_id, payload, signer_v2, signed_at)
}

// ---------------------------------------------------------------- 主测试

#[test]
fn client_replays_the_real_relay_chain_and_authenticates() {
    let fx = establish("replay");
    // 升级前的链尾（v1 genesis/approve 记录）。
    let (start_rev, start_hash) = {
        let mut pinned = fx
            .ctx
            .open(&fx.a.vault, &fx.a.identity)
            .expect("open pinned");
        pinned.client_mut().control_head().expect("v1 control head")
    };
    let epoch = {
        let mut pinned = fx
            .ctx
            .open(&fx.a.vault, &fx.a.identity)
            .expect("open pinned");
        pinned.client_mut().state().expect("state").epoch
    };
    let mut transport = fx.ctx.transport().expect("transport");
    let mut head = Head {
        rev: start_rev,
        hash: start_hash,
        authz_rev: 0,
        authority_rev: 0,
    };
    let now = |fx: &Fixture| fx.clock.now_secs();
    let a_id = fx.a.identity.device_id;
    let cap_a: [u8; 16] = [0xEA; 16];
    let cap_b: [u8; 16] = [0xEB; 16];
    let cap_c: [u8; 16] = [0xEC; 16];
    let cap_x: [u8; 16] = [0xED; 16];

    // ---- kind 0 UpgradePolicy：旧 creator + A/B 确认 + 新恢复钥 PoP ----
    let manifest = vec![
        cert(
            &fx,
            a_id,
            fx.keys.a.public_key(),
            hpke_of(&fx.a.identity),
            1,
            1,
        ),
        cert(&fx, fx.b_id, fx.keys.b.public_key(), fx.b_hpke, 1, 1),
    ]
    .into_iter()
    .collect::<Vec<_>>();
    let mut manifest = manifest;
    manifest.sort_by_key(|cert| cert.device_id);
    let ts = now(&fx);
    let caps = vec![
        vault_cap(
            &fx,
            cap_a,
            a_id,
            fx.keys.a.public_key(),
            proto::CapScope::ALL,
            start_rev + 1,
            1,
            1,
            ts - 60,
            ts + 86_340,
        ),
        vault_cap(
            &fx,
            cap_b,
            fx.b_id,
            fx.keys.b.public_key(),
            proto::CapScope::ALL,
            start_rev + 1,
            1,
            1,
            ts - 60,
            ts + 86_340,
        ),
    ];
    let payload = proto::UpgradePolicyPayloadV2 {
        old_manager_rev: 1,
        policy: policy(),
        fresh_recovery_pk: fx.keys.recovery1.public_key(),
        bootstrap_caps: caps,
        active_manifest: manifest,
    }
    .encode()
    .expect("upgrade payload");
    let mut signers = vec![
        device_signer(a_id, fx.keys.a.public_key(), None),
        device_signer(fx.b_id, fx.keys.b.public_key(), None),
        root_signer(
            proto::SignerRole::CreatorUpgrade,
            fx.a.identity.pinned_manager,
        ),
        root_signer(
            proto::SignerRole::OfflineRecovery,
            fx.keys.recovery1.public_key(),
        ),
    ];
    signers = sort_signers(signers);
    let mut operation = base_operation(
        &fx,
        &head,
        proto::ControlKindV2::UpgradePolicy,
        [0x01; 16],
        payload,
        signers,
        ts,
    );
    operation.expected_authz_rev = 0;
    operation.authority_rev = 1;
    // 旧 creator 的私钥由 A 的管理 seed 派生（pin 住的 manager pk 必须匹配）。
    let manager_key =
        DeviceSigningKey::from_seed(fx.a.identity.manager_seed.clone().expect("manager seed"));
    let signed = sign(
        &operation,
        &[&fx.keys.a, &fx.keys.b, &manager_key, &fx.keys.recovery1],
    );
    let receipt = submit(&mut transport, &fx.api, fx.a.vault_id, "upgrade", &signed);
    assert_eq!(receipt.rev, start_rev + 1);
    head = head_after(&receipt);
    assert_eq!((head.authz_rev, head.authority_rev), (1, 1));

    // ---- kind 2 CapGrant：A+B 两签，给 A 一张 INVITE-only cap ----
    let ts = now(&fx);
    let cap_x_entry = vault_cap(
        &fx,
        cap_x,
        a_id,
        fx.keys.a.public_key(),
        proto::CapScope::INVITE,
        head.rev + 1,
        head.authz_rev + 1,
        head.authority_rev,
        ts - 60,
        ts + 600,
    );
    let operation = two_sign(
        &fx,
        &head,
        proto::ControlKindV2::CapGrant,
        [0x02; 16],
        proto::CapGrantPayloadV2 { cap: cap_x_entry }
            .encode()
            .expect("cap grant"),
        &[(a_id, cap_a), (fx.b_id, cap_b)],
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.a, &fx.keys.b]);
    head = head_after(&submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "cap-grant-x",
        &signed,
    ));
    assert_eq!(head.authz_rev, 2);

    // ---- kind 3 CapRevoke ----
    let ts = now(&fx);
    let operation = two_sign(
        &fx,
        &head,
        proto::ControlKindV2::CapRevoke,
        [0x03; 16],
        proto::CapRevokePayloadV2 { cap_id: cap_x }
            .encode()
            .expect("revoke"),
        &[(a_id, cap_a), (fx.b_id, cap_b)],
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.a, &fx.keys.b]);
    head = head_after(&submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "cap-revoke-x",
        &signed,
    ));
    assert_eq!(head.authz_rev, 3);

    // ---- kind 11/12：邀请创建/取消（一台 INVITE cap，只推进 control）----
    let ts = now(&fx);
    let invite_id: [u8; 16] = [0x51; 16];
    let operation = base_operation(
        &fx,
        &head,
        proto::ControlKindV2::InviteCreate,
        [0x04; 16],
        proto::InviteCreatePayloadV2 {
            invite_id,
            token_hash: [0x61; 32],
            expires_at: ts + 600,
        }
        .encode()
        .expect("invite"),
        vec![device_signer(a_id, fx.keys.a.public_key(), Some(cap_a))],
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.a]);
    let receipt = submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "invite-create",
        &signed,
    );
    assert_eq!(receipt.authz_rev, 3, "邀请只推进 control");
    head = head_after(&receipt);
    let ts = now(&fx);
    let operation = base_operation(
        &fx,
        &head,
        proto::ControlKindV2::InviteCancel,
        [0x05; 16],
        proto::InviteCancelPayloadV2 { invite_id }
            .encode()
            .expect("cancel"),
        vec![device_signer(a_id, fx.keys.a.public_key(), Some(cap_a))],
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.a]);
    head = head_after(&submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "invite-cancel",
        &signed,
    ));
    assert_eq!(head.authz_rev, 3);

    // ---- kind 4 DeviceApprove：A+B APPROVE + C PoP ----
    let ts = now(&fx);
    let cert_c = cert(
        &fx,
        fx.c_id,
        fx.keys.c.public_key(),
        fx.c_hpke,
        1,
        head.authz_rev + 1,
    );
    let payload = proto::DeviceCredentialPayloadV2 {
        cert: cert_c,
        grants: vec![grant_package(
            &fx,
            epoch,
            fx.c_id,
            fx.c_hpke,
            head.authority_rev,
            [0x71; 16],
            head.authz_rev + 1,
        )],
    }
    .encode()
    .expect("approve");
    let signers = vec![
        device_signer(a_id, fx.keys.a.public_key(), Some(cap_a)),
        device_signer(fx.b_id, fx.keys.b.public_key(), Some(cap_b)),
        pop_signer(fx.c_id, fx.keys.c.public_key()),
    ];
    let operation = base_operation(
        &fx,
        &head,
        proto::ControlKindV2::DeviceApprove,
        [0x06; 16],
        payload,
        signers,
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.a, &fx.keys.b, &fx.keys.c]);
    let receipt = submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "device-approve-c",
        &signed,
    );
    assert_eq!(receipt.authz_rev, 4);
    head = head_after(&receipt);

    // ---- kind 2：给 C 一张全 scope cap ----
    let ts = now(&fx);
    let cap_c_entry = vault_cap(
        &fx,
        cap_c,
        fx.c_id,
        fx.keys.c.public_key(),
        proto::CapScope::ALL,
        head.rev + 1,
        head.authz_rev + 1,
        head.authority_rev,
        ts - 60,
        ts + 86_340,
    );
    let operation = two_sign(
        &fx,
        &head,
        proto::ControlKindV2::CapGrant,
        [0x07; 16],
        proto::CapGrantPayloadV2 { cap: cap_c_entry }
            .encode()
            .expect("cap grant"),
        &[(a_id, cap_a), (fx.b_id, cap_b)],
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.a, &fx.keys.b]);
    head = head_after(&submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "cap-grant-c",
        &signed,
    ));
    assert_eq!(head.authz_rev, 5);

    // ---- kind 5 DeviceRevoke：A+C 两签撤销 B ----
    let ts = now(&fx);
    let operation = two_sign(
        &fx,
        &head,
        proto::ControlKindV2::DeviceRevoke,
        [0x08; 16],
        proto::DeviceRevokePayloadV2 {
            device_ids: vec![fx.b_id],
        }
        .encode()
        .expect("revoke"),
        &[(a_id, cap_a), (fx.c_id, cap_c)],
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.a, &fx.keys.c]);
    let receipt = submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "device-revoke-b",
        &signed,
    );
    assert_eq!(receipt.authz_rev, 6);
    head = head_after(&receipt);

    // ---- kind 14 RecoveryKeyChange：A+C POLICY + 新恢复钥 PoP ----
    let ts = now(&fx);
    let cap_d1: [u8; 16] = [0xEE; 16];
    let cap_d2: [u8; 16] = [0xEF; 16];
    let payload = proto::RecoveryKeyChangePayloadV2 {
        fresh_recovery_pk: fx.keys.recovery2.public_key(),
        bootstrap_caps: vec![
            vault_cap(
                &fx,
                cap_d1,
                a_id,
                fx.keys.a.public_key(),
                proto::CapScope::ALL,
                head.rev + 1,
                head.authz_rev + 1,
                head.authority_rev + 1,
                ts - 60,
                ts + 86_340,
            ),
            vault_cap(
                &fx,
                cap_d2,
                fx.c_id,
                fx.keys.c.public_key(),
                proto::CapScope::ALL,
                head.rev + 1,
                head.authz_rev + 1,
                head.authority_rev + 1,
                ts - 60,
                ts + 86_340,
            ),
        ],
    }
    .encode()
    .expect("key change");
    let signers = vec![
        device_signer(a_id, fx.keys.a.public_key(), Some(cap_a)),
        device_signer(fx.c_id, fx.keys.c.public_key(), Some(cap_c)),
        root_signer(
            proto::SignerRole::OfflineRecovery,
            fx.keys.recovery2.public_key(),
        ),
    ];
    let operation = base_operation(
        &fx,
        &head,
        proto::ControlKindV2::RecoveryKeyChange,
        [0x09; 16],
        payload,
        signers,
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.a, &fx.keys.c, &fx.keys.recovery2]);
    let receipt = submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "recovery-key-change",
        &signed,
    );
    assert_eq!((receipt.authz_rev, receipt.authority_rev), (7, 2));
    head = head_after(&receipt);

    // ---- kind 13 RecoveryReset：当前恢复根 + 新钥 PoP ----
    let ts = now(&fx);
    let cap_d3: [u8; 16] = [0xF1; 16];
    let cap_d4: [u8; 16] = [0xF2; 16];
    let payload = proto::RecoveryResetPayloadV2 {
        policy: policy(),
        fresh_recovery_pk: fx.keys.recovery3.public_key(),
        bootstrap_caps: vec![
            vault_cap(
                &fx,
                cap_d3,
                a_id,
                fx.keys.a.public_key(),
                cap_admin_scopes(),
                head.rev + 1,
                head.authz_rev + 1,
                head.authority_rev + 1,
                ts - 60,
                ts + 86_340,
            ),
            vault_cap(
                &fx,
                cap_d4,
                fx.c_id,
                fx.keys.c.public_key(),
                cap_admin_scopes(),
                head.rev + 1,
                head.authz_rev + 1,
                head.authority_rev + 1,
                ts - 60,
                ts + 86_340,
            ),
        ],
        device_adds: Vec::new(),
        revoke_ids: Vec::new(),
    }
    .encode()
    .expect("reset");
    let signers = vec![
        root_signer(
            proto::SignerRole::OfflineRecovery,
            fx.keys.recovery2.public_key(),
        ),
        root_signer(
            proto::SignerRole::OfflineRecovery,
            fx.keys.recovery3.public_key(),
        ),
    ];
    let operation = base_operation(
        &fx,
        &head,
        proto::ControlKindV2::RecoveryReset,
        [0x0A; 16],
        payload,
        signers,
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.recovery2, &fx.keys.recovery3]);
    let receipt = submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "recovery-reset",
        &signed,
    );
    assert_eq!((receipt.authz_rev, receipt.authority_rev), (8, 3));
    head = head_after(&receipt);
    let final_head = head;

    // ---- 客户端从 0 回放 ----
    let signing_a = fx.keys.a.clone();
    let config = V2ManagementConfig {
        vault_id: fx.a.vault_id,
        instance_id: fx.a.identity.pinned_instance,
        device_id: a_id,
        manager_pub: fx.a.identity.pinned_manager,
    };
    let mut client = V2ManagementClient::with_clock(
        fx.ctx.transport().expect("client transport"),
        fx.relay.base_url(),
        config,
        fx.keys.a.clone(),
        fx.clock.clone(),
    )
    .with_store(fx.a.vault.clone())
    .expect("client with store");
    assert!(!client.is_v2(), "回放前没有 V2 状态");
    client.refresh_control().expect("refresh control");
    let state = client.state().clone();
    assert!(state.is_v2());
    assert_eq!(state.rev(), final_head.rev);
    assert_eq!(state.head_hash(), final_head.hash);
    assert_eq!(state.authz_rev(), final_head.authz_rev);
    assert_eq!(state.authority_rev(), final_head.authority_rev);
    assert_eq!(state.authz_state(), Some(AuthzStateV2::Active));
    assert_eq!(state.active_device_count(), 2, "B 已撤销，A/C 仍 ACTIVE");
    assert!(state.is_active_device(&a_id));
    assert!(!state.is_active_device(&fx.b_id));
    assert!(state.is_active_device(&fx.c_id));
    assert_eq!(state.recovery_pub(), Some(fx.keys.recovery3.public_key()));
    assert_eq!(
        state.caps().count(),
        8,
        "2 bootstrap + X + C + 2 keychange + 2 reset"
    );
    assert!(state.cap(&cap_x).expect("cap X").is_revoked());
    assert!(state.cap(&cap_b).expect("cap B").is_revoked());
    assert_eq!(state.invites().count(), 1);
    assert!(state
        .invites()
        .next()
        .expect("invite")
        .cancelled_rev
        .is_some());

    // 用 relay 的 realm1 ChallengeV2 交叉核对 authz/authority rev（relay 是权威投影）。
    let (challenge, _device_token) =
        relay_realm1_token(&mut transport, &fx.api, fx.a.vault_id, a_id, &signing_a);
    assert_eq!(
        challenge.authz_rev,
        state.authz_rev(),
        "authz_rev 与 relay 一致"
    );
    assert_eq!(
        challenge.authority_rev,
        state.authority_rev(),
        "authority 与 relay 一致"
    );

    // ---- cap 选择 ----
    let selected = client
        .select_cap(CapScope::new(CapScope::INVITE).expect("scope"))
        .expect("选 cap");
    assert_eq!(selected.cap.cap_id, cap_d3, "只选当前代的单张 cap");
    let state_authz = state.authz_rev();
    let error = client
        .select_cap(CapScope::new(CapScope::ROTATE).expect("scope"))
        .expect_err("没有 ROTATE cap");
    assert!(
        error.to_string().contains("单张 cap") || error.to_string().contains("scope"),
        "{error:?}"
    );
    // 错 device：B 已撤销。
    let error = client
        .state()
        .select_cap(
            &fx.b_id,
            &fx.keys.b.public_key(),
            CapScope::new(CapScope::INVITE).expect("s"),
            fx.clock.now_secs(),
        )
        .expect_err("撤销设备不能选 cap");
    assert!(error.to_string().contains("撤销"), "{error:?}");

    // ---- realm3 正例：选定 cap 的 bearer 可用于真实鉴权请求 ----
    let token: ManagementTokenV2 = client
        .management_bearer(CapScope::new(CapScope::INVITE).expect("scope"))
        .expect("管理 bearer");
    assert_eq!(token.cap_id(), cap_d3);
    assert_eq!(token.authz_rev(), state_authz);
    let (status, _fields) = get_bearer(
        &mut transport,
        &fx.api.vault_path(&fx.a.vault_id, "/control"),
        token.token(),
    );
    assert_eq!(status, 200, "realm3 bearer 可用于真实鉴权请求");

    // ---- realm3 负例：撤销 cap / 旧代 cap / 未知 cap / 跨 vault ----
    for (label, cap_id) in [
        ("已撤销 cap", cap_x),
        ("旧 authority 代 cap", cap_a),
        ("未知 cap", [0xAA; 16]),
    ] {
        let (status, _) = post_cbor(
            &mut transport,
            &fx.api.vault_path(&fx.a.vault_id, "/authz/challenges"),
            vec![
                (0, cbor::uint(3)),
                (1, cbor::bytes(a_id.to_vec())),
                (2, cbor::bytes(cap_id.to_vec())),
            ],
        );
        assert_eq!(status, 403, "{label} 必须被 relay 拒绝");
    }
    let (status, _) = post_cbor(
        &mut transport,
        &fx.api.vault_path(&[0xAB; 16], "/authz/challenges"),
        vec![
            (0, cbor::uint(3)),
            (1, cbor::bytes(a_id.to_vec())),
            (2, cbor::bytes(cap_d3.to_vec())),
        ],
    );
    assert_eq!(status, 404, "跨 vault 不泄露存在性");

    // ---- challenge 重放：同一 challenge+sig 第二次被拒绝 ----
    let (status, fields) = post_cbor(
        &mut transport,
        &fx.api.vault_path(&fx.a.vault_id, "/authz/challenges"),
        vec![
            (0, cbor::uint(3)),
            (1, cbor::bytes(a_id.to_vec())),
            (2, cbor::bytes(cap_d3.to_vec())),
        ],
    );
    assert_eq!(status, 200);
    let challenge_bytes = cbor::bytes_field(&fields, 0).expect("challenge");
    let signature = signing_a
        .sign(&proto::auth_signing_input_v2(&challenge_bytes))
        .to_vec();
    let body = vec![
        (0, cbor::bytes(challenge_bytes.clone())),
        (1, cbor::bytes(signature.clone())),
    ];
    let (status, _) = post_cbor(
        &mut transport,
        &fx.api.vault_path(&fx.a.vault_id, "/authz/tokens"),
        body.clone(),
    );
    assert_eq!(status, 200);
    let (status, fields) = post_cbor(
        &mut transport,
        &fx.api.vault_path(&fx.a.vault_id, "/authz/tokens"),
        body,
    );
    assert_eq!(status, 401, "challenge 60s 单用（重放必须拒绝）");
    assert_eq!(
        cbor::text_field(&fields, 0).unwrap_or_default(),
        "CHALLENGE_USED"
    );

    // ---- token 版本失效：提交新 cap 后 authz_rev 前进，旧 bearer 立即 403 ----
    let ts = now(&fx);
    let cap_short: [u8; 16] = [0xF3; 16];
    let short = vault_cap(
        &fx,
        cap_short,
        a_id,
        fx.keys.a.public_key(),
        proto::CapScope::INVITE,
        head.rev + 1,
        head.authz_rev + 1,
        head.authority_rev,
        ts - 60,
        ts + 60,
    );
    let operation = two_sign(
        &fx,
        &head,
        proto::ControlKindV2::CapGrant,
        [0x0B; 16],
        proto::CapGrantPayloadV2 { cap: short }
            .encode()
            .expect("grant"),
        &[(a_id, cap_d3), (fx.c_id, cap_d4)],
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.a, &fx.keys.c]);
    let receipt = submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "cap-grant-short",
        &signed,
    );
    assert_eq!(receipt.authz_rev, 9);
    head = head_after(&receipt);
    let (status, _) = get_bearer(
        &mut transport,
        &fx.api.vault_path(&fx.a.vault_id, "/control"),
        token.token(),
    );
    assert_eq!(status, 403, "旧 bearer 在 rev 前进后必须立即失效");

    // 客户端 refresh：device token 随 auth_rev 失效也要能透明重认证；pin 前进到新 head。
    client.refresh_control().expect("refresh after rev advance");
    assert_eq!(client.state().rev(), head.rev);
    assert_eq!(client.state().authz_rev(), 9);
    assert_eq!(
        client.pin().expect("pin").rev,
        head.rev,
        "pin 必须随已验证 head 前进"
    );
    let new_token = client
        .management_bearer(CapScope::new(CapScope::INVITE).expect("scope"))
        .expect("重新鉴权拿到 short cap");
    assert_eq!(
        new_token.cap_id(),
        cap_short,
        "精确匹配优先于 scope 更大的 cap"
    );

    // ---- 自然到期：relay 与客户端时钟同步推进后 cap 即时失效 ----
    fx.relay.advance_secs(90);
    fx.clock.advance(90);
    let (status, _) = post_cbor(
        &mut transport,
        &fx.api.vault_path(&fx.a.vault_id, "/authz/challenges"),
        vec![
            (0, cbor::uint(3)),
            (1, cbor::bytes(a_id.to_vec())),
            (2, cbor::bytes(cap_short.to_vec())),
        ],
    );
    assert_eq!(status, 403, "自然到期即时失效（不需要新记录）");
    client.refresh_control().expect("refresh after expiry");
    let expired = client.state().cap(&cap_short).expect("short cap");
    assert!(
        !client
            .state()
            .cap_is_effective_at(expired, fx.clock.now_secs()),
        "客户端同样按 expires_at 即时判失效"
    );
    assert!(
        client
            .select_cap(CapScope::new(CapScope::INVITE).expect("scope"))
            .expect("仍可选到未过期 cap")
            .cap
            .cap_id
            == cap_d3,
        "过期 cap 不得被选中"
    );

    // ---- 重启：从 durable pin + 缓存恢复，不重放就得到同一状态 ----
    let cached_rev = client.state().rev();
    let cached_authz = client.state().authz_rev();
    drop(client);
    let reloaded = V2ManagementClient::with_clock(
        fx.ctx.transport().expect("reload transport"),
        fx.relay.base_url(),
        V2ManagementConfig {
            vault_id: fx.a.vault_id,
            instance_id: fx.a.identity.pinned_instance,
            device_id: a_id,
            manager_pub: fx.a.identity.pinned_manager,
        },
        fx.keys.a.clone(),
        fx.clock.clone(),
    )
    .with_store(fx.a.vault.clone())
    .expect("重启恢复");
    assert_eq!(reloaded.state().rev(), cached_rev);
    assert_eq!(reloaded.state().authz_rev(), cached_authz);
    assert_eq!(reloaded.pin().expect("pin").rev, cached_rev);

    // 损坏缓存不误信：pin 仍在、回放从 0 重建（rev 归 0），再次 refresh 后与 relay 一致。
    fx.a.vault
        .set_meta(META_MANAGEMENT_CACHE_V2, b"corrupted-cache")
        .expect("写坏缓存");
    let mut rebuilt = V2ManagementClient::with_clock(
        fx.ctx.transport().expect("rebuild transport"),
        fx.relay.base_url(),
        V2ManagementConfig {
            vault_id: fx.a.vault_id,
            instance_id: fx.a.identity.pinned_instance,
            device_id: a_id,
            manager_pub: fx.a.identity.pinned_manager,
        },
        fx.keys.a.clone(),
        fx.clock.clone(),
    )
    .with_store(fx.a.vault.clone())
    .expect("坏缓存可丢弃");
    assert_eq!(rebuilt.state().rev(), 0, "损坏缓存不得被当作已回放状态");
    assert_eq!(rebuilt.pin().expect("pin 保留").rev, cached_rev);
    rebuilt.refresh_control().expect("从 0 重建");
    assert_eq!(rebuilt.state().rev(), cached_rev);
    assert_eq!(rebuilt.state().authz_rev(), cached_authz);
    assert_eq!(rebuilt.pin().expect("pin").rev, cached_rev);

    // ---- 增量 replay：再提交一条 cap 撤销，客户端从水位续到新 head ----
    let ts = now(&fx);
    let operation = two_sign(
        &fx,
        &head,
        proto::ControlKindV2::CapRevoke,
        [0x0C; 16],
        proto::CapRevokePayloadV2 { cap_id: cap_short }
            .encode()
            .expect("revoke"),
        &[(a_id, cap_d3), (fx.c_id, cap_d4)],
        ts,
    );
    let signed = sign(&operation, &[&fx.keys.a, &fx.keys.c]);
    let receipt = submit(
        &mut transport,
        &fx.api,
        fx.a.vault_id,
        "final-cap-revoke",
        &signed,
    );
    assert_eq!(receipt.authz_rev, 10);
    head = head_after(&receipt);
    rebuilt.refresh_control().expect("incremental refresh");
    assert_eq!(rebuilt.state().rev(), head.rev);
    assert_eq!(rebuilt.state().authz_rev(), 10);
    assert!(rebuilt
        .state()
        .cap(&cap_short)
        .expect("short cap")
        .is_revoked());
    let pin = rebuilt.pin().expect("pin");
    assert_eq!(pin.rev, head.rev);
    assert_eq!(pin.head_hash, head.hash);
    assert_eq!(pin.authz_rev, 10);
    assert_eq!(pin.authority_rev, 3);

    // pin 与 meta 一致性：pin 存的是 I/V/head/authz/authority（另带缓存摘要）。
    let raw_pin =
        fx.a.vault
            .get_meta(META_CONTROL_PIN_V2)
            .expect("meta")
            .expect("pin 必须落盘");
    let decoded = menghunote_sync::management_v2::decode_v2_pin(&raw_pin).expect("pin 解码");
    assert_eq!(decoded.vault_id, fx.a.vault_id);
    assert_eq!(decoded.instance_id, fx.a.identity.pinned_instance);
    assert_eq!(decoded.rev, head.rev);
    assert_eq!(decoded.head_hash, head.hash);
    let cache =
        fx.a.vault
            .get_meta(META_MANAGEMENT_CACHE_V2)
            .expect("meta")
            .expect("缓存必须落盘");
    let cache_state =
        ControlStateV2::decode_cache(&cache, fx.a.vault_id, fx.a.identity.pinned_instance)
            .expect("缓存可解码");
    assert_eq!(cache_state.rev(), head.rev);
    assert_eq!(
        hash_fn(&cache),
        decoded.state_digest,
        "pin 里的缓存摘要必须与缓存字节一致"
    );
    // 未启用 V2 的 legacy 语句：pin/pin 解析对换 vault 明确拒绝。
    assert!(menghunote_sync::management_v2::decode_v2_pin(b"not-a-pin").is_err());
    assert_ne!(cache_state.instance_id(), [0u8; 16]);
}

fn hpke_of(identity: &LocalIdentityV1) -> [u8; 32] {
    use menghunote_sync::device::DeviceHpkeKey;
    DeviceHpkeKey::from_secret(identity.x25519_secret.clone()).public_key()
}

fn cap_admin_scopes() -> u32 {
    proto::CapScope::CAP_ADMIN
        | proto::CapScope::INVITE
        | proto::CapScope::APPROVE
        | proto::CapScope::REVOKE
        | proto::CapScope::POLICY
}

// ========================================================================
// ADR 0016 **S4d1b**：ApprovalEnvelopeV1 / typed Operation 构造 / 双签状态机 / 提交对账
// ========================================================================
//
// 与上面的 S4d1a 用例共用同一套 dev relay 夹具（真实 HTTP、真实 Ed25519、临时目录、
// 无 sleep）。A/B/C 都是本测试自己掌控私钥的本地身份（B 已由 v1 批准，C 是 PENDING）。

mod approval_s4d1b {
    use super::*;
    use menghunote_sync::approval_v1::{
        ApprovalDraftV1, ApprovalFailureV1, ApprovalRequestV1, ApprovalStateV1, CapGrantRequestV1,
        CapRevokeRequestV1, DeviceApproveRequestV1, DeviceRevokeRequestV1,
        DeviceRightsChangeRequestV1, InviteCancelRequestV1, InviteCreateRequestV1,
        UpgradePolicyRequestV1, MAX_APPROVAL_ENVELOPE_BYTES_V1,
    };
    use menghunote_sync::http_transport::HttpResponse;
    use menghunote_sync::management_v2::V2ManagementClient;
    use menghunote_sync::random::RandomSource;
    use menghunote_sync::SyncError;
    use std::path::Path;

    /// 确定性随机源（只在测试里用；与库内 `test_support::TestRng` 同算法）。
    struct TestRng {
        state: u64,
    }

    impl TestRng {
        fn new(seed: u64) -> Self {
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

    // ------------------------------------------------------------ 夹具

    /// v1 侧已验证事实（升级前 V2 回放里没有这些）。
    #[derive(Clone, Copy)]
    struct V1Facts {
        rev: u64,
        hash: [u8; 32],
        epoch: u64,
    }

    fn v1_facts(fx: &Fixture) -> V1Facts {
        let mut pinned = fx
            .ctx
            .open(&fx.a.vault, &fx.a.identity)
            .expect("open pinned");
        let (rev, hash) = pinned.client_mut().control_head().expect("v1 control head");
        let state = pinned.client_mut().state().expect("v1 state");
        V1Facts {
            rev,
            hash,
            epoch: state.epoch,
        }
    }

    struct V2World {
        fx: Fixture,
        facts: V1Facts,
        /// 升级后的链尾（authz/authority = 1）。
        head: Head,
    }

    /// 用 v1 手工路径把 vault 升到 V2（kind 0 的 *构造器* 路径由单独用例覆盖）。
    fn upgrade_world(prefix: &str) -> V2World {
        let fx = establish(prefix);
        let facts = v1_facts(&fx);
        let a_id = fx.a.identity.device_id;
        let cap_a: [u8; 16] = [0xEA; 16];
        let cap_b: [u8; 16] = [0xEB; 16];
        let mut manifest = vec![
            cert(
                &fx,
                a_id,
                fx.keys.a.public_key(),
                hpke_of(&fx.a.identity),
                1,
                1,
            ),
            cert(&fx, fx.b_id, fx.keys.b.public_key(), fx.b_hpke, 1, 1),
        ];
        manifest.sort_by_key(|cert| cert.device_id);
        let ts = fx.clock.now_secs();
        let caps = vec![
            vault_cap(
                &fx,
                cap_a,
                a_id,
                fx.keys.a.public_key(),
                proto::CapScope::ALL,
                facts.rev + 1,
                1,
                1,
                ts - 60,
                ts + 86_340,
            ),
            vault_cap(
                &fx,
                cap_b,
                fx.b_id,
                fx.keys.b.public_key(),
                proto::CapScope::ALL,
                facts.rev + 1,
                1,
                1,
                ts - 60,
                ts + 86_340,
            ),
        ];
        let payload = proto::UpgradePolicyPayloadV2 {
            old_manager_rev: 1,
            policy: policy(),
            fresh_recovery_pk: fx.keys.recovery1.public_key(),
            bootstrap_caps: caps,
            active_manifest: manifest,
        }
        .encode()
        .expect("upgrade payload");
        let signers = sort_signers(vec![
            device_signer(a_id, fx.keys.a.public_key(), None),
            device_signer(fx.b_id, fx.keys.b.public_key(), None),
            root_signer(
                proto::SignerRole::CreatorUpgrade,
                fx.a.identity.pinned_manager,
            ),
            root_signer(
                proto::SignerRole::OfflineRecovery,
                fx.keys.recovery1.public_key(),
            ),
        ]);
        let mut operation = base_operation(
            &fx,
            &Head {
                rev: facts.rev,
                hash: facts.hash,
                authz_rev: 0,
                authority_rev: 0,
            },
            proto::ControlKindV2::UpgradePolicy,
            [0x01; 16],
            payload,
            signers,
            ts,
        );
        operation.expected_authz_rev = 0;
        operation.authority_rev = 1;
        let manager_key =
            DeviceSigningKey::from_seed(fx.a.identity.manager_seed.clone().expect("manager seed"));
        let signed = sign(
            &operation,
            &[&fx.keys.a, &fx.keys.b, &manager_key, &fx.keys.recovery1],
        );
        let mut transport = fx.ctx.transport().expect("transport");
        let receipt = submit(
            &mut transport,
            &fx.api,
            fx.a.vault_id,
            "s4d1b-upgrade",
            &signed,
        );
        assert_eq!(receipt.rev, facts.rev + 1);
        assert_eq!((receipt.authz_rev, receipt.authority_rev), (1, 1));
        V2World {
            head: Head {
                rev: receipt.rev,
                hash: receipt.head,
                authz_rev: 1,
                authority_rev: 1,
            },
            fx,
            facts,
        }
    }

    fn config_a(world: &V2World) -> V2ManagementConfig {
        V2ManagementConfig {
            vault_id: world.fx.a.vault_id,
            instance_id: world.fx.a.identity.pinned_instance,
            device_id: world.fx.a.identity.device_id,
            manager_pub: world.fx.a.identity.pinned_manager,
        }
    }

    fn config_b(world: &V2World) -> V2ManagementConfig {
        V2ManagementConfig {
            vault_id: world.fx.a.vault_id,
            instance_id: world.fx.a.identity.pinned_instance,
            device_id: world.fx.b_id,
            manager_pub: world.fx.a.identity.pinned_manager,
        }
    }

    fn client_a(world: &V2World) -> V2ManagementClient<StdTcpTransport, TestClock> {
        V2ManagementClient::with_clock(
            world.fx.ctx.transport().expect("A transport"),
            world.fx.relay.base_url(),
            config_a(world),
            world.fx.keys.a.clone(),
            world.fx.clock.clone(),
        )
        .with_store(world.fx.a.vault.clone())
        .expect("A client with store")
    }

    fn b_side_client(world: &V2World) -> V2ManagementClient<StdTcpTransport, TestClock> {
        V2ManagementClient::with_clock(
            world.fx.ctx.transport().expect("B transport"),
            world.fx.relay.base_url(),
            config_b(world),
            world.fx.keys.b.clone(),
            world.fx.clock.clone(),
        )
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!(
                "approval-s4d1b-{}-{}-{name}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("time")
                    .as_nanos()
            ))
            .with_extension("mn-approval")
    }

    /// 把草稿的 CDE 复制一份并原地改一个字段（负例用；不重新计算 `h`）。
    fn tamper(
        draft: &ApprovalDraftV1,
        mutate: impl FnOnce(&mut Vec<(u64, cbor::CdeValue)>),
    ) -> Vec<u8> {
        use menghunote_sync_proto::{decode_cde, encode_cde, CdeValue};
        let bytes = draft.to_bytes().expect("bytes");
        let mut value = decode_cde(&bytes).expect("decode");
        let CdeValue::Map(ref mut fields) = value else {
            panic!("map");
        };
        mutate(fields);
        encode_cde(&value).expect("encode")
    }

    fn signed_control_with_zero_gaps(draft: &ApprovalDraftV1) -> proto::SignedControlV2 {
        let signatures = (0..draft.envelope().operation().signers.len() as u32)
            .map(|index| draft.envelope().signature_for(index).unwrap_or([0u8; 64]))
            .collect();
        proto::SignedControlV2 {
            operation: draft.envelope().operation().clone(),
            signatures,
        }
    }

    // ------------------------------------------------------------ 1. 单签邀请

    #[test]
    fn approval_invite_create_and_cancel_single_sign_is_idempotent() {
        let world = upgrade_world("approval-invite");
        let a_id = world.fx.a.identity.device_id;
        let head = world.head;
        let mut client = client_a(&world);
        client.refresh_control().expect("refresh");
        let now = client.now_secs();
        let mut rng = TestRng::new(11);
        let invite_id: [u8; 16] = [0x51; 16];
        let token_hash: [u8; 32] = [0x61; 32];
        let mut draft = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::InviteCreate(InviteCreateRequestV1 {
                invite_id,
                token_hash,
                expires_at: now + 600,
            }),
            &mut rng,
            now,
        )
        .expect("propose invite");
        assert_eq!(draft.state(), ApprovalStateV1::Draft);
        draft.freeze();
        assert_eq!(draft.state(), ApprovalStateV1::Frozen);
        draft.sign_with(&world.fx.keys.a).expect("A 签");
        assert_eq!(draft.state(), ApprovalStateV1::Ready);
        // 另一台 ACTIVE 设备（B）持有满签包也可以提交（ADR：满签包任何持有者可 submit）。
        let bytes = draft.to_bytes().expect("bytes");
        let mut client_b = b_side_client(&world);
        client_b.refresh_control().expect("B refresh");
        let mut imported = client_b.import_approval_bytes(&bytes).expect("B 导入");
        assert_eq!(imported.state(), ApprovalStateV1::Ready);
        let receipt = imported.submit(&mut client_b).expect("B 提交 invite");
        assert_eq!(receipt.control_rev, head.rev + 1);
        assert_eq!(receipt.kind, proto::ControlKindV2::InviteCreate.as_u8());
        assert_eq!(receipt.authz_rev, head.authz_rev, "邀请只推进 control");

        // A 的本地视图陈旧：提交同一满签包 → 必须换算出"已在链上"并幂等返回原回执。
        let receipt_a = draft.submit(&mut client).expect("A 幂等提交");
        assert_eq!(receipt_a, receipt);
        assert_eq!(draft.state(), ApprovalStateV1::Confirmed);
        assert_eq!(client.state().rev(), receipt.control_rev);
        let invite = client
            .state()
            .invites()
            .find(|invite| invite.invite_id == invite_id)
            .expect("邀请已在链上");
        assert_eq!(invite.token_hash, token_hash);

        // 已确认后再提交：直接返回本地回执（不重复写链）。
        let again = draft.submit(&mut client).expect("idempotent submit");
        assert_eq!(again, receipt);
        assert_eq!(client.state().rev(), receipt.control_rev);

        // 取消邀请：A 单签、提交、链上出现 cancelled_rev。
        let mut cancel = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::InviteCancel(InviteCancelRequestV1 { invite_id }),
            &mut rng,
            now,
        )
        .expect("propose cancel");
        cancel.sign_with(&world.fx.keys.a).expect("A 签 cancel");
        let cancel_receipt = cancel.submit(&mut client).expect("submit cancel");
        assert_eq!(cancel_receipt.control_rev, receipt.control_rev + 1);
        let invite = client
            .state()
            .invites()
            .find(|invite| invite.invite_id == invite_id)
            .expect("邀请仍在链上");
        assert_eq!(invite.cancelled_rev, Some(cancel_receipt.control_rev));
        assert_eq!(
            cancel.submit(&mut client).expect("idempotent"),
            cancel_receipt
        );
    }

    // ------------------------------------------------------------ 2. 双签 cap

    #[test]
    fn approval_dual_signed_cap_grant_and_revoke() {
        let world = upgrade_world("approval-caps");
        let a_id = world.fx.a.identity.device_id;
        let head = world.head;
        let cap_x: [u8; 16] = [0xD1; 16];
        let mut client = client_a(&world);
        client.refresh_control().expect("refresh");
        let now = client.now_secs();
        let mut rng = TestRng::new(21);
        let mut grant = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::CapGrant(CapGrantRequestV1 {
                cap_id: cap_x,
                subject_device_id: a_id,
                scopes: proto::CapScope::INVITE | proto::CapScope::APPROVE,
                not_before: now - 10,
                expires_at: now + 600,
                counterparty: world.fx.b_id,
            }),
            &mut rng,
            now,
        )
        .expect("propose grant");
        assert_eq!(grant.slots().len(), 2);

        // 缺签禁止提交。
        grant.sign_with(&world.fx.keys.a).expect("A 签");
        assert_eq!(grant.state(), ApprovalStateV1::InitiatorSigned);
        let error = {
            let mut half = grant.clone();
            let client_ref = &mut client;
            half.submit(client_ref).expect_err("单签禁止提交")
        };
        assert_eq!(error.kind, ApprovalFailureV1::NotReady);

        // 导出（file + base64）→ B 导入验证 → 加签 → 满签。
        assert!(grant
            .to_base64()
            .expect("base64")
            .starts_with("mn-approval-v1:"));
        let file = temp_path("grant");
        grant.write_file(&file).expect("write file");
        let mut client_b = b_side_client(&world);
        client_b.refresh_control().expect("B refresh");
        let mut imported = client_b.import_approval_file(&file).expect("B 导入并验证");
        assert_eq!(imported.state(), ApprovalStateV1::AwaitingCosign);
        assert_eq!(
            imported.to_bytes().expect("bytes"),
            grant.to_bytes().expect("bytes")
        );
        imported.sign_with(&world.fx.keys.b).expect("B 签");
        assert_eq!(imported.state(), ApprovalStateV1::Ready);
        // 提交前的已验证回放快照（负例必须对同一链位置验证）。
        let replay_at_head = client.replay().clone();
        let receipt = imported.submit(&mut client_b).expect("B 提交");
        assert_eq!(receipt.kind, proto::ControlKindV2::CapGrant.as_u8());
        assert_eq!(receipt.control_rev, head.rev + 1);
        assert_eq!(receipt.authz_rev, head.authz_rev + 1);
        // 成功提交后 cap 生效（A 刷新后可选到）。
        client.refresh_control().expect("A refresh");
        let selected = client
            .select_cap(proto::CapScope::new(proto::CapScope::INVITE).expect("scope"))
            .expect("新 cap 生效");
        assert_eq!(selected.cap.cap_id, cap_x);

        // —— 负例：换 payload 的旧签 / 错索引 / 单签直接发 relay ——
        // 1) 换 payload（不同 cap_id）但把旧包的 A 签名塞进同槽 → 签名无效。
        let other = ApprovalDraftV1::propose(
            &replay_at_head,
            a_id,
            &ApprovalRequestV1::CapGrant(CapGrantRequestV1 {
                cap_id: [0xD2; 16],
                subject_device_id: a_id,
                scopes: proto::CapScope::INVITE,
                not_before: now - 10,
                expires_at: now + 600,
                counterparty: world.fx.b_id,
            }),
            &mut rng,
            now,
        )
        .expect("propose other grant");
        let old_sig = grant.envelope().signatures()[0];
        let tampered = tamper(&other, |fields| {
            let CdeValue_::Array(ref mut entries) = fields
                .iter_mut()
                .find(|(key, _)| *key == 4)
                .expect("signatures")
                .1
            else {
                panic!("signatures");
            };
            entries.push(CdeValue_::Map(vec![
                (0, CdeValue_::UInt(u64::from(old_sig.signer_index))),
                (1, CdeValue_::Bytes(old_sig.signature.to_vec())),
            ]));
        });
        let imported = ApprovalDraftV1::from_bytes(&tampered).expect("结构合法");
        let error = imported
            .verify_import(&replay_at_head, now)
            .expect_err("换 payload 的旧签必须拒绝");
        assert!(error.to_string().contains("签名"), "{error}");
        // 2) 错索引：把 A 的签名标到 B 的槽 → 同样拒绝。
        let wrong_index = 1 - grant.envelope().initiator_index();
        let tampered = tamper(&grant, |fields| {
            let CdeValue_::Array(ref mut entries) = fields
                .iter_mut()
                .find(|(key, _)| *key == 4)
                .expect("signatures")
                .1
            else {
                panic!("signatures");
            };
            let CdeValue_::Map(ref mut entry) = entries[0] else {
                panic!("entry");
            };
            entry[0] = (0, CdeValue_::UInt(u64::from(wrong_index)));
        });
        let imported = ApprovalDraftV1::from_bytes(&tampered).expect("结构合法");
        let error = imported
            .verify_import(&replay_at_head, now)
            .expect_err("错索引必须拒绝");
        assert!(error.to_string().contains("签名"), "{error}");
        // 3) 单签直接发 relay（绕过本地状态机）：relay 必须拒绝。
        let now_ts = client.now_secs();
        let mut single = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::CapGrant(CapGrantRequestV1 {
                cap_id: [0xD3; 16],
                subject_device_id: a_id,
                scopes: proto::CapScope::INVITE,
                not_before: now_ts - 10,
                expires_at: now_ts + 600,
                counterparty: world.fx.b_id,
            }),
            &mut rng,
            now_ts,
        )
        .expect("propose single");
        single.sign_with(&world.fx.keys.a).expect("A 签");
        let mut forged = signed_control_with_zero_gaps(&single);
        forged.signatures[1] = [0u8; 64];
        let error = client
            .submit_control_bytes(&forged)
            .expect_err("单签/零占位必须被 relay 拒绝");
        match &error {
            SyncError::RelayApi { code, .. } => {
                assert_eq!(code, "SIGNATURE_INVALID", "{error:?}");
            }
            other => panic!("期望 SIGNATURE_INVALID，实际 {other:?}"),
        }
        // 4) 换 B 的签名（B 的钥不在另一份包的槽里）→ 加签必须拒绝。
        let mut other_draft = other.clone();
        let error = other_draft
            .sign_with(&world.fx.keys.c)
            .expect_err("陌生钥不能签");
        assert!(error.to_string().contains("签名槽"), "{error}");

        // —— CapRevoke：A/B 双签撤销刚授予的 cap ——
        let now = client.now_secs();
        let mut revoke = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::CapRevoke(CapRevokeRequestV1 {
                cap_id: cap_x,
                counterparty: world.fx.b_id,
            }),
            &mut rng,
            now,
        )
        .expect("propose revoke");
        revoke.sign_with(&world.fx.keys.a).expect("A 签 revoke");
        let mut client_b = b_side_client(&world);
        client_b.refresh_control().expect("B refresh 2");
        let mut imported = client_b
            .import_approval_base64(&revoke.to_base64().expect("base64"))
            .expect("B 导入 revoke");
        imported.sign_with(&world.fx.keys.b).expect("B 签 revoke");
        let receipt = imported.submit(&mut client_b).expect("提交 revoke");
        assert_eq!(receipt.kind, proto::ControlKindV2::CapRevoke.as_u8());
        assert_eq!(receipt.authz_rev, head.authz_rev + 2);
        client.refresh_control().expect("A refresh 2");
        assert!(client.state().cap(&cap_x).expect("cap X").is_revoked());
        let revoked = client.state().cap(&cap_x).expect("cap X").clone();
        assert!(revoked.is_revoked());
        assert!(!client
            .state()
            .cap_is_effective_at(&revoked, client.now_secs()));
        // 撤销后 select_cap 只会退化到未被撤销的 bootstrap cap（不再返回 cap_x）。
        let selected = client
            .select_cap(proto::CapScope::new(proto::CapScope::INVITE).expect("scope"))
            .expect("bootstrap cap 仍在");
        assert_ne!(selected.cap.cap_id, cap_x);

        // —— 旧签/陈旧包：链前进后旧包必须显式失效 ——
        let stale = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::InviteCreate(InviteCreateRequestV1 {
                invite_id: [0x52; 16],
                token_hash: [0x62; 32],
                expires_at: client.now_secs() + 600,
            }),
            &mut rng,
            client.now_secs(),
        )
        .expect("propose stale invite");
        let mut stale = stale;
        stale.sign_with(&world.fx.keys.a).expect("A 签 stale");
        // 先提交一个别的操作让链前进。
        let mut advance = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::InviteCreate(InviteCreateRequestV1 {
                invite_id: [0x53; 16],
                token_hash: [0x63; 32],
                expires_at: client.now_secs() + 600,
            }),
            &mut rng,
            client.now_secs(),
        )
        .expect("propose advance");
        advance.sign_with(&world.fx.keys.a).expect("A 签 advance");
        advance.submit(&mut client).expect("提交 advance");
        let error = stale.submit(&mut client).expect_err("陈旧包必须拒绝");
        assert_eq!(error.kind, ApprovalFailureV1::Stale);
        // relay 侧同样拒绝（CAS）。
        let stale_signed = signed_control_with_zero_gaps(&stale);
        let error = client
            .submit_control_bytes(&stale_signed)
            .expect_err("relay 必须 CAS 拒绝");
        match &error {
            SyncError::RelayApi { code, .. } => assert_eq!(code, "CAS_CONFLICT", "{error:?}"),
            other => panic!("期望 CAS_CONFLICT，实际 {other:?}"),
        }
    }

    // ------------------------------------------------------------ 3. 设备批准/撤销/改权

    #[test]
    fn approval_device_approve_pop_revoke_and_rights_change() {
        let world = upgrade_world("approval-devices");
        let a_id = world.fx.a.identity.device_id;
        let b_id = world.fx.b_id;
        let c_id = world.fx.c_id;
        let head = world.head;
        let mut client = client_a(&world);
        client.refresh_control().expect("refresh");
        let now = client.now_secs();
        let mut rng = TestRng::new(31);
        let cert_c = cert(
            &world.fx,
            c_id,
            world.fx.keys.c.public_key(),
            world.fx.c_hpke,
            1,
            head.authz_rev + 1,
        );
        let grant_c = grant_package(
            &world.fx,
            world.facts.epoch,
            c_id,
            world.fx.c_hpke,
            head.authority_rev,
            [0x71; 16],
            head.authz_rev + 1,
        );
        let approve_request = ApprovalRequestV1::DeviceApprove(DeviceApproveRequestV1 {
            cert: cert_c.clone(),
            grants: vec![grant_c.clone()],
            counterparty: b_id,
        });
        // 错 grant（recipient 不是目标）→ 构造期干跑拒绝。
        let mut bad_grant = grant_c.clone();
        bad_grant.aad.recipient_device_id = a_id;
        let error = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::DeviceApprove(DeviceApproveRequestV1 {
                cert: cert_c.clone(),
                grants: vec![bad_grant],
                counterparty: b_id,
            }),
            &mut rng,
            now,
        )
        .expect_err("错 grant 必须在构造期拒绝");
        assert!(error.to_string().contains("干跑"), "{error}");

        // 错证书：把目标公钥换成"自造钥"并为它做 PoP（客户端干跑通过，但 relay 会发现
        // 证书没有绑定兑换时登记的 PENDING 公钥 → 403）。必须在正确批准之前发一次。
        let forged_key = DeviceSigningKey::from_seed(Secret32::from_array([0x77; 32]));
        let mut forged_cert = cert_c.clone();
        forged_cert.sign_ed25519_pub = forged_key.public_key();
        let mut forged_grant = grant_c.clone();
        forged_grant.aad.recipient_hpke_pub = world.fx.c_hpke;
        let mut forged = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::DeviceApprove(DeviceApproveRequestV1 {
                cert: forged_cert,
                grants: vec![forged_grant],
                counterparty: b_id,
            }),
            &mut rng,
            now,
        )
        .expect("错证书在构造期结构合法");
        forged.sign_with(&world.fx.keys.a).expect("A 签错证书");
        forged.sign_with(&world.fx.keys.b).expect("B 签错证书");
        forged.sign_with(&forged_key).expect("自造钥 PoP");
        let mut client_b = b_side_client(&world);
        client_b.refresh_control().expect("B refresh forged");
        let error = forged
            .submit(&mut client_b)
            .expect_err("证书未绑定登记公钥，relay 必须拒绝");
        match error.kind {
            ApprovalFailureV1::Forbidden | ApprovalFailureV1::Conflict => {}
            other => panic!("期望 FORBIDDEN，实际 {other:?}"),
        }

        let mut approve =
            ApprovalDraftV1::propose(client.replay(), a_id, &approve_request, &mut rng, now)
                .expect("propose approve");
        assert_eq!(approve.slots().len(), 3, "A/B 两签 + C 目标 PoP");
        approve.sign_with(&world.fx.keys.a).expect("A 签 approve");
        let mut client_b = b_side_client(&world);
        client_b.refresh_control().expect("B refresh");
        let mut imported = client_b
            .import_approval_bytes(&approve.to_bytes().expect("bytes"))
            .expect("B 导入 approve");
        imported.sign_with(&world.fx.keys.b).expect("B 签 approve");
        imported.sign_with(&world.fx.keys.c).expect("C PoP");
        assert_eq!(imported.state(), ApprovalStateV1::Ready);
        // 错 PoP：把 PoP 槽的签名换成垃圾字节后直接发 relay（绕过本地预检；
        // 此时 request_id 尚未写入 dedup，relay 必须按签名拒绝）。
        let pop_index = imported
            .envelope()
            .operation()
            .signers
            .iter()
            .position(|signer| signer.role == proto::SignerRole::TargetPop)
            .expect("PoP 槽") as usize;
        let mut forged = signed_control_with_zero_gaps(&imported);
        forged.signatures[pop_index] = [0x42u8; 64];
        let error = client_b
            .submit_control_bytes(&forged)
            .expect_err("relay 必须拒绝错 PoP");
        match &error {
            SyncError::RelayApi { code, .. } => {
                assert_eq!(code, "SIGNATURE_INVALID", "{error:?}");
            }
            other => panic!("期望 SIGNATURE_INVALID，实际 {other:?}"),
        }
        let receipt = imported.submit(&mut client_b).expect("提交 approve");
        assert_eq!(receipt.authz_rev, head.authz_rev + 1);
        client.refresh_control().expect("A refresh");
        assert!(client.state().is_active_device(&c_id), "C 已 ACTIVE");
        assert_eq!(
            client
                .state()
                .device(&c_id)
                .expect("C")
                .cert
                .sign_ed25519_pub,
            world.fx.keys.c.public_key()
        );

        // 给 C 授予全 scope cap（A/B 双签）。
        let now = client.now_secs();
        let cap_c: [u8; 16] = [0xEC; 16];
        let mut grant = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::CapGrant(CapGrantRequestV1 {
                cap_id: cap_c,
                subject_device_id: c_id,
                scopes: proto::CapScope::ALL,
                not_before: now - 10,
                expires_at: now + 86_340,
                counterparty: b_id,
            }),
            &mut rng,
            now,
        )
        .expect("propose grant C");
        grant.sign_with(&world.fx.keys.a).expect("A 签 grant C");
        let mut client_b = b_side_client(&world);
        client_b.refresh_control().expect("B refresh 3");
        let mut imported = client_b
            .import_approval_bytes(&grant.to_bytes().expect("bytes"))
            .expect("B 导入 grant C");
        imported.sign_with(&world.fx.keys.b).expect("B 签 grant C");
        imported.submit(&mut client_b).expect("提交 grant C");
        client.refresh_control().expect("A refresh 3");

        // 换 B（同一 payload 换第二签名者）必须新建 Operation：旧签名不能移植。
        let now = client.now_secs();
        let with_b = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::CapGrant(CapGrantRequestV1 {
                cap_id: [0xD7; 16],
                subject_device_id: a_id,
                scopes: proto::CapScope::INVITE,
                not_before: now - 10,
                expires_at: now + 600,
                counterparty: b_id,
            }),
            &mut rng,
            now,
        )
        .expect("propose 换 B 前");
        let with_c = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::CapGrant(CapGrantRequestV1 {
                cap_id: [0xD7; 16],
                subject_device_id: a_id,
                scopes: proto::CapScope::INVITE,
                not_before: now - 10,
                expires_at: now + 600,
                counterparty: c_id,
            }),
            &mut rng,
            now,
        )
        .expect("propose 换 B 后（C 也在链上）");
        assert_ne!(
            with_b.envelope().operation_bytes(),
            with_c.envelope().operation_bytes(),
            "换第二签名者 = 新 Operation（旧签名作废）"
        );
        let old_sig = {
            let mut signed = with_b.clone();
            signed.sign_with(&world.fx.keys.a).expect("A 签原包");
            signed.envelope().signatures()[0]
        };
        let tampered = tamper(&with_c, |fields| {
            let CdeValue_::Array(ref mut entries) = fields
                .iter_mut()
                .find(|(key, _)| *key == 4)
                .expect("signatures")
                .1
            else {
                panic!("signatures");
            };
            entries.push(CdeValue_::Map(vec![
                (0, CdeValue_::UInt(u64::from(old_sig.signer_index))),
                (1, CdeValue_::Bytes(old_sig.signature.to_vec())),
            ]));
        });
        let imported = ApprovalDraftV1::from_bytes(&tampered).expect("结构合法");
        let error = imported
            .verify_import(client.replay(), now)
            .expect_err("换 B 的旧签必须拒绝");
        assert!(error.to_string().contains("签名"), "{error}");

        // 改权 C：serial=2、只保留 READ、grant 换新 grant_id。
        let now = client.now_secs();
        let c_cert = client.state().device(&c_id).expect("C").cert.clone();
        let new_rights = proto::Rights::new(proto::Rights::READ).expect("rights");
        let cert_16 = proto::DeviceCertV2 {
            cert_serial: c_cert.cert_serial + 1,
            data_rights: new_rights,
            issued_authz_rev: client.state().authz_rev() + 1,
            ..c_cert.clone()
        };
        let grant_16 = grant_package(
            &world.fx,
            world.facts.epoch,
            c_id,
            world.fx.c_hpke,
            client.state().authority_rev(),
            [0x72; 16],
            client.state().authz_rev() + 1,
        );
        // 错 serial → 构造期拒绝。
        let mut bad_cert = cert_16.clone();
        bad_cert.cert_serial = 1;
        let error = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::DeviceRightsChange(DeviceRightsChangeRequestV1 {
                cert: bad_cert,
                grants: vec![grant_16.clone()],
                counterparty: c_id,
            }),
            &mut rng,
            now,
        )
        .expect_err("错 serial 必须拒绝");
        assert!(error.to_string().contains("serial"), "{error}");
        let mut rights = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::DeviceRightsChange(DeviceRightsChangeRequestV1 {
                cert: cert_16.clone(),
                grants: vec![grant_16],
                counterparty: c_id,
            }),
            &mut rng,
            now,
        )
        .expect("propose rights C");
        rights.sign_with(&world.fx.keys.a).expect("A 签 rights");
        rights.sign_with(&world.fx.keys.c).expect("C 签 rights");
        rights.submit(&mut client).expect("提交 rights C");
        let c_cert = client.state().device(&c_id).expect("C").cert.clone();
        assert_eq!(c_cert.cert_serial, 2);
        assert!(!c_cert.data_rights.can_write(), "已改权为只读");
        // 撤销 B（A + C 两签 REVOKE）。
        let now = client.now_secs();
        let mut revoke = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::DeviceRevoke(DeviceRevokeRequestV1 {
                device_ids: vec![b_id],
                counterparty: c_id,
            }),
            &mut rng,
            now,
        )
        .expect("propose revoke B");
        revoke.sign_with(&world.fx.keys.a).expect("A 签 revoke B");
        revoke.sign_with(&world.fx.keys.c).expect("C 签 revoke B");
        let receipt = revoke.submit(&mut client).expect("提交 revoke B");
        assert!(!client.state().is_active_device(&b_id), "B 已撤销");
        assert!(client.state().is_active_device(&c_id));
        assert_eq!(receipt.authz_rev, head.authz_rev + 4);
    }

    // ------------------------------------------------------------ 4. Upgrade（构造器路径）

    #[test]
    fn approval_upgrade_via_builder_sets_authz_state() {
        let fx = establish("approval-upgrade");
        let facts = v1_facts(&fx);
        let a_id = fx.a.identity.device_id;
        let cap_a: [u8; 16] = [0xF1; 16];
        let cap_b: [u8; 16] = [0xF2; 16];
        let config = V2ManagementConfig {
            vault_id: fx.a.vault_id,
            instance_id: fx.a.identity.pinned_instance,
            device_id: a_id,
            manager_pub: fx.a.identity.pinned_manager,
        };
        let mut client = V2ManagementClient::with_clock(
            fx.ctx.transport().expect("transport"),
            fx.relay.base_url(),
            config,
            fx.keys.a.clone(),
            fx.clock.clone(),
        )
        .with_store(fx.a.vault.clone())
        .expect("client with store");
        client.refresh_control().expect("legacy refresh 是 no-op");
        assert!(!client.is_v2(), "升级前没有 V2 状态");
        let ts = client.now_secs();
        let mut manifest = vec![
            cert(
                &fx,
                a_id,
                fx.keys.a.public_key(),
                hpke_of(&fx.a.identity),
                1,
                1,
            ),
            cert(&fx, fx.b_id, fx.keys.b.public_key(), fx.b_hpke, 1, 1),
        ];
        manifest.sort_by_key(|cert| cert.device_id);
        let bootstrap = vec![
            vault_cap(
                &fx,
                cap_a,
                a_id,
                fx.keys.a.public_key(),
                proto::CapScope::ALL,
                facts.rev + 1,
                1,
                1,
                ts - 60,
                ts + 86_340,
            ),
            vault_cap(
                &fx,
                cap_b,
                fx.b_id,
                fx.keys.b.public_key(),
                proto::CapScope::ALL,
                facts.rev + 1,
                1,
                1,
                ts - 60,
                ts + 86_340,
            ),
        ];
        let request = UpgradePolicyRequestV1 {
            control_rev: facts.rev,
            prev_control_hash: facts.hash,
            old_manager_rev: 1,
            policy: policy(),
            fresh_recovery_pk: fx.keys.recovery1.public_key(),
            bootstrap_caps: bootstrap,
            active_manifest: manifest.clone(),
            confirmations: vec![
                (a_id, fx.keys.a.public_key()),
                (fx.b_id, fx.keys.b.public_key()),
            ],
        };
        let mut rng = TestRng::new(41);
        let legacy_replay = client.replay().clone();
        let mut draft = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::UpgradePolicy(request.clone()),
            &mut rng,
            ts,
        )
        .expect("propose upgrade");
        assert_eq!(draft.slots().len(), 4, "2 确认 + 旧 creator + 新恢复钥");
        draft.sign_with(&fx.keys.a).expect("A 确认");
        draft.sign_with(&fx.keys.b).expect("B 确认");
        let manager_key =
            DeviceSigningKey::from_seed(fx.a.identity.manager_seed.clone().expect("manager"));
        draft.sign_with(&manager_key).expect("旧 creator 签");
        draft.sign_with(&fx.keys.recovery1).expect("新恢复钥 PoP");
        assert_eq!(draft.state(), ApprovalStateV1::Ready);
        let receipt = draft.submit(&mut client).expect("提交 upgrade");
        assert_eq!(receipt.control_rev, facts.rev + 1);
        assert_eq!((receipt.authz_rev, receipt.authority_rev), (1, 1));
        assert_eq!(draft.state(), ApprovalStateV1::Confirmed);
        let state = client.state().clone();
        assert!(state.is_v2());
        assert_eq!(state.authz_rev(), 1);
        assert_eq!(state.authority_rev(), 1);
        assert_eq!(state.recovery_pub(), Some(fx.keys.recovery1.public_key()));
        assert!(state.cap(&cap_a).is_some());
        assert!(state.cap(&cap_b).is_some());
        assert!(state.is_active_device(&a_id));
        assert!(state.is_active_device(&fx.b_id));

        // 负例：错误 manifest（缺一台）在**构造期**就被干跑拒绝。
        let mut bad = request.clone();
        bad.active_manifest = vec![manifest[0].clone()];
        let error = ApprovalDraftV1::propose(
            &legacy_replay,
            a_id,
            &ApprovalRequestV1::UpgradePolicy(bad),
            &mut rng,
            ts,
        )
        .expect_err("缺成员 manifest 必须拒绝");
        assert!(error.to_string().contains("manifest"), "{error}");
        // 负例：签名槽冻结后不能换钥签（新恢复钥 PoP 用陌生钥）。
        let mut bad_payload = request.clone();
        bad_payload.fresh_recovery_pk = fx.keys.recovery2.public_key();
        let mut bad_draft = ApprovalDraftV1::propose(
            &legacy_replay,
            a_id,
            &ApprovalRequestV1::UpgradePolicy(bad_payload),
            &mut rng,
            ts,
        )
        .expect("propose 仍有结构合法性");
        let error = bad_draft
            .sign_with(&fx.keys.recovery3)
            .expect_err("陌生恢复钥不能签 PoP 槽");
        assert!(error.to_string().contains("签名槽"), "{error}");
    }

    // ------------------------------------------------------------ 5. 时间窗/CAS/跨库/草稿/丢响应

    #[test]
    fn approval_time_window_cas_cross_vault_draft_and_lost_response() {
        let world = upgrade_world("approval-robust");
        let a_id = world.fx.a.identity.device_id;
        let head = world.head;
        let mut client = client_a(&world);
        client.refresh_control().expect("refresh");
        let now = client.now_secs();
        let mut rng = TestRng::new(51);

        // —— 文件/base64 往返 + 重启草稿 ——
        let mut draft = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::InviteCreate(InviteCreateRequestV1 {
                invite_id: [0x54; 16],
                token_hash: [0x64; 32],
                expires_at: now + 600,
            }),
            &mut rng,
            now,
        )
        .expect("propose invite");
        draft.sign_with(&world.fx.keys.a).expect("A 签");
        let file = temp_path("draft");
        draft.write_file(&file).expect("写文件");
        let reloaded = ApprovalDraftV1::read_file(&file).expect("读文件");
        assert_eq!(
            reloaded.to_bytes().expect("bytes"),
            draft.to_bytes().expect("bytes")
        );
        assert_eq!(reloaded.state(), ApprovalStateV1::Ready);
        // 草稿持久化（vault 公开 meta）→ 新 client 从 pin/缓存恢复后继续提交。
        draft.save_draft(&world.fx.a.vault).expect("保存草稿");
        let mut restarted = client_a(&world);
        restarted.refresh_control().expect("重启刷新");
        let restored = ApprovalDraftV1::load_draft(&world.fx.a.vault)
            .expect("读草稿")
            .expect("草稿存在");
        assert_eq!(restored.state(), ApprovalStateV1::Ready);
        let receipt = restored.clone().submit(&mut restarted).expect("重启后提交");
        assert_eq!(receipt.control_rev, head.rev + 1);
        ApprovalDraftV1::clear_draft(&world.fx.a.vault).expect("清草稿");

        // —— 跨 vault 导入拒绝 ——
        let other_world = upgrade_world("approval-robust-other");
        let mut other_client = client_a(&other_world);
        other_client.refresh_control().expect("other refresh");
        let error = other_client
            .import_approval_bytes(&draft.to_bytes().expect("bytes"))
            .expect_err("跨 vault 必须拒绝");
        assert!(
            error.to_string().contains("vault") || error.to_string().contains("跨租户"),
            "{error}"
        );

        // —— 路径篡改：wire 层就拒绝（唯一提交路径绑定 vault） ——
        let tampered = tamper(&draft, |fields| {
            let CdeValue_::Bytes(ref mut operation) = fields[1].1 else {
                panic!("operation");
            };
            let mut value = menghunote_sync_proto::decode_cde(operation).expect("op cde");
            let CdeValue_::Map(ref mut op_fields) = value else {
                panic!("op map");
            };
            op_fields[12] = (
                12,
                CdeValue_::Text(
                    "/v2/vaults/00000000000000000000000000000001/control/submit".into(),
                ),
            );
            *operation = menghunote_sync_proto::encode_cde(&value).expect("op encode");
        });
        let error = ApprovalDraftV1::from_bytes(&tampered).expect_err("路径篡改必须拒绝");
        assert!(!error.to_string().is_empty());

        // —— 导入限长 ——
        let big = temp_path("too-big");
        std::fs::write(&big, vec![0u8; MAX_APPROVAL_ENVELOPE_BYTES_V1 + 1]).expect("写大文件");
        let error = ApprovalDraftV1::read_file(&big).expect_err("超限文件必须拒绝");
        assert!(error.to_string().contains("上限"), "{error}");

        // —— 时间窗：短窗过期后本地/relay 都拒绝 ——
        let mut client = client_a(&world);
        client.refresh_control().expect("refresh 2");
        let now = client.now_secs();
        let mut expiring = ApprovalDraftV1::propose(
            client.replay(),
            a_id,
            &ApprovalRequestV1::InviteCreate(InviteCreateRequestV1 {
                invite_id: [0x55; 16],
                token_hash: [0x65; 32],
                expires_at: now + 600,
            }),
            &mut rng,
            now,
        )
        .expect("propose expiring");
        expiring.sign_with(&world.fx.keys.a).expect("A 签");
        world.fx.clock.advance(400);
        world.fx.relay.advance_secs(400);
        let error = expiring.submit(&mut client).expect_err("过期包必须拒绝");
        assert_eq!(error.kind, ApprovalFailureV1::Expired);
        // relay 侧同样 410（用原字节）。
        let error = client
            .submit_control_bytes(&signed_control_with_zero_gaps(&expiring))
            .expect_err("relay 必须 EXPIRED");
        match &error {
            SyncError::RelayApi { code, .. } => assert_eq!(code, "EXPIRED", "{error:?}"),
            other => panic!("期望 EXPIRED，实际 {other:?}"),
        }

        // —— 丢响应后重发原字节（幂等，不重复写链） ——
        let now = world.fx.clock.now_secs();
        let mut lossy_client = V2ManagementClient::with_clock(
            LossyTransport {
                inner: world.fx.ctx.transport().expect("lossy transport"),
                remaining_drops: 1,
            },
            world.fx.relay.base_url(),
            config_a(&world),
            world.fx.keys.a.clone(),
            world.fx.clock.clone(),
        );
        lossy_client.refresh_control().expect("lossy refresh");
        let mut lossy = ApprovalDraftV1::propose(
            lossy_client.replay(),
            a_id,
            &ApprovalRequestV1::InviteCreate(InviteCreateRequestV1 {
                invite_id: [0x56; 16],
                token_hash: [0x66; 32],
                expires_at: now + 600,
            }),
            &mut rng,
            now,
        )
        .expect("propose lossy");
        lossy.sign_with(&world.fx.keys.a).expect("A 签 lossy");
        let error = lossy.submit(&mut lossy_client).expect_err("响应丢失");
        assert_eq!(error.kind, ApprovalFailureV1::Transport);
        assert_eq!(lossy.state(), ApprovalStateV1::Failed);
        // relay 已经落链：换正常 client 刷新确认 rev 前进了一条。
        let mut verify_client = client_a(&world);
        verify_client.refresh_control().expect("verify refresh");
        let rev_after = verify_client.state().rev();
        // 重试同一草稿（同字节）：dedup 返回原回执并确认。
        let receipt = lossy.submit(&mut lossy_client).expect("重发原字节");
        assert_eq!(receipt.control_rev, rev_after);
        assert_eq!(lossy.state(), ApprovalStateV1::Confirmed);
        let mut final_client = client_a(&world);
        final_client.refresh_control().expect("final refresh");
        assert_eq!(
            final_client.state().rev(),
            rev_after,
            "丢响应重发不得重复写链"
        );
        assert_eq!(final_client.state().rev(), receipt.control_rev);
        assert!(final_client
            .state()
            .invites()
            .any(|invite| invite.invite_id == [0x56; 16]));
    }

    /// 模拟"relay 已接受但响应丢失"的传输层。
    struct LossyTransport {
        inner: StdTcpTransport,
        remaining_drops: usize,
    }

    impl HttpTransport for LossyTransport {
        fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
            let response = self.inner.send(request)?;
            if self.remaining_drops > 0
                && request.method == "POST"
                && request.path.contains("/control/submit")
            {
                self.remaining_drops -= 1;
                return Err(SyncError::Transport("模拟响应丢失"));
            }
            Ok(response)
        }
    }

    // 让 `tamper` 里的别名短一点（保持测试可读）。
    type CdeValue_ = menghunote_sync_proto::CdeValue;
}
