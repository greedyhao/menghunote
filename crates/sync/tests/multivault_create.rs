//! ADR 0014 **S3a**：客户端 v2 API 面 + 一次性票据创建 + `mn-pair://create` 链接。
//!
//! 全部用 `menghunote-dev-relay`（进程内真实 axum relay + 临时 SQLite + `127.0.0.1:0`、
//! **放行验签 hook**）：
//!
//! 1. 同一 dev relay 上两张票据各建一个 vault：身份/pin/库完全独立，`/state` 与对象
//!    不串库（相同 object_id 在两边各自成立）；
//! 2. 票据错误分类：错 vault/过期/已用 → `403 FORBIDDEN`（`retryable=false`）；
//!    同一 vault 重复创建（quota>1）→ `409 ALREADY_INITIALIZED`；限流 → `429 RATE_LIMITED`；
//!    失败不建库、不消费票据（随后用同一张票仍可成功）；
//! 3. v2 路径/签名域：stub transport 记录实际请求 path，断言管理签名覆盖的是**同一个**
//!    `/v2/...` path（用 v1 path 复算必须验签失败）；
//! 4. 链接往返 + ticket 只放 fragment（严格解析的负例在 `create_link` 单测里）；
//! 5. ticket 不落配置/日志/库：创建成功后扫描实例目录与库字节。
//!
//! ⚠️ 非生产：dev relay 放行验签；本文件验证客户端 path/错误分类/隔离，不是鉴权验收。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use menghunote_dev_relay::DevRelay;
use menghunote_sync::api_version::{RelayApi, RelayApiVersion};
use menghunote_sync::cbor;
use menghunote_sync::control_pin::load_control_pin;
use menghunote_sync::create_link::{CreateLink, CreateTicket};
use menghunote_sync::device::ManagementKey;
use menghunote_sync::device_management::{revoke_device, RevokeContext};
use menghunote_sync::http_relay::{
    Clock as _, HttpRelayClient, HttpRelayConfig, HttpRequest, HttpResponse, HttpTransport,
    SystemClock,
};
use menghunote_sync::http_transport::DEFAULT_TRANSPORT_TIMEOUTS;
use menghunote_sync::identity::LocalIdentityV1;
use menghunote_sync::keys::{RootSecretV1, Secret32};
use menghunote_sync::relay::RelayClient as _;
use menghunote_sync::relay_context::RelayContext;
use menghunote_sync::relay_policy::RelayMode;
use menghunote_sync::remote_init::{create_vault_with_ticket, CreatedVault};
use menghunote_sync::upload::{fetch_object, upload_object};
use menghunote_sync::SyncError;
use menghunote_sync_proto::{
    management_request_cde, management_signing_input, HttpMethod, ObjectType,
};
use tempfile::{Builder as TempDirBuilder, TempDir};

const VAULT_A: [u8; 16] = [0xA1; 16];
const VAULT_B: [u8; 16] = [0xB2; 16];
const VAULT_C: [u8; 16] = [0xC3; 16];
const OBJECT: [u8; 32] = [0x42; 32];

fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("multivault-create-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

fn local_context(relay: &DevRelay) -> RelayContext {
    RelayContext::local_dev(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("local_dev context")
        .with_api_version(RelayApiVersion::V2)
}

fn raw_ticket(relay: &DevRelay, vault_id: [u8; 16], ttl_secs: u64, quota: u32) -> [u8; 32] {
    relay
        .issue_create_ticket(vault_id, Duration::from_secs(ttl_secs), quota)
        .expect("发行创建票据")
}

fn ticket(relay: &DevRelay, vault_id: [u8; 16], ttl_secs: u64, quota: u32) -> CreateTicket {
    CreateTicket::from_slice(&raw_ticket(relay, vault_id, ttl_secs, quota)).expect("票据长度为 32B")
}

fn no_identity(_identity: &menghunote_sync::identity::LocalIdentityV1) -> Result<(), SyncError> {
    Ok(())
}

fn assert_no_vault_files(vault_path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut path = vault_path.as_os_str().to_os_string();
        path.push(suffix);
        let path = PathBuf::from(path);
        assert!(!path.exists(), "失败路径不得留下库文件：{}", path.display());
    }
}

fn expect_create_error(result: Result<CreatedVault, SyncError>) -> SyncError {
    match result {
        Ok(_) => panic!("期望创建失败，但成功了"),
        Err(error) => error,
    }
}

fn relay_code(error: &SyncError) -> (&str, bool) {
    match error {
        SyncError::RelayApi {
            code, retryable, ..
        } => (code.as_str(), *retryable),
        other => panic!("期望 relay 错误码，实际 {other:?}"),
    }
}

/// 扫描实例目录（库 + 边车）确认 ticket 明文没有被持久化。
fn assert_ticket_not_persisted(vault_dir: &Path, ticket: &[u8; 32]) {
    let mut files = Vec::new();
    collect_files(vault_dir, &mut files);
    for file in files {
        let name = file.to_string_lossy().to_ascii_lowercase();
        assert!(
            !name.contains("ticket") && !name.contains("create-link"),
            "实例目录不得出现票据相关文件：{}",
            file.display()
        );
        let bytes = std::fs::read(&file).unwrap_or_default();
        // 票据是 32B 随机数：本地库/配置只允许出现它的哈希之外的任何形态都算泄漏。
        let b64 = menghunote_sync::base64url::encode(ticket);
        let hex = menghunote_sync::ids::hex_of(ticket);
        assert!(
            !contains_bytes(&bytes, b64.as_bytes()) && !contains_bytes(&bytes, hex.as_bytes()),
            "库/配置中不得出现 ticket 的编码形态：{}",
            file.display()
        );
        assert!(
            !contains_bytes(&bytes, b"mn-pair://create"),
            "创建链接不得被持久化：{}",
            file.display()
        );
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// 只用于"库里有没有票据编码"的断言：`CreateTicket` 有意不暴露明文，测试通过
/// 对同一 `[u8;32]` 重新构造一个等值 ticket 再读取其字节的路径不存在；因此这里把
/// `ticket` 参数改为不可读的哨兵，真正的明文泄漏由 `Debug`/错误文案断言覆盖。
/// 同一 dev relay 两张票据各建一个 vault：身份/pin/实例独立、相同 object_id 不串库。
#[test]
fn two_ticket_vaults_on_the_same_relay_are_isolated() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("two-vaults");
    let context = local_context(&relay);
    assert_eq!(context.api().create_vault_path(), "/v2/vaults");

    let a_path = dir.path().join("a/vault.sqlite3");
    let b_path = dir.path().join("b/vault.sqlite3");

    // 两个 vault 都通过 **同一形状** 的创建链接进入（link → parse → context）。
    let raw_a = raw_ticket(&relay, VAULT_A, 900, 1);
    let raw_b = raw_ticket(&relay, VAULT_B, 900, 1);
    let link_a = CreateLink::encode(
        RelayMode::LocalDev,
        relay.base_url(),
        VAULT_A,
        CreateTicket::from_slice(&raw_a).expect("ticket A"),
        None,
    )
    .expect("A link");
    let link_b = CreateLink::encode(
        RelayMode::LocalDev,
        relay.base_url(),
        VAULT_B,
        CreateTicket::from_slice(&raw_b).expect("ticket B"),
        None,
    )
    .expect("B link");
    let parsed_a = CreateLink::parse(&link_a.url).expect("parse A");
    let parsed_b = CreateLink::parse(&link_b.url).expect("parse B");
    assert_eq!(parsed_a.vault_id, VAULT_A);
    assert_eq!(parsed_b.vault_id, VAULT_B);

    let a: CreatedVault = create_vault_with_ticket(
        parsed_a
            .relay_context(DEFAULT_TRANSPORT_TIMEOUTS)
            .expect("ctx A"),
        parsed_a.vault_id,
        &parsed_a.ticket,
        parsed_a.pin,
        &a_path,
        "票据 A",
        no_identity,
    )
    .expect("A create via ticket");
    let b = create_vault_with_ticket(
        parsed_b
            .relay_context(DEFAULT_TRANSPORT_TIMEOUTS)
            .expect("ctx B"),
        parsed_b.vault_id,
        &parsed_b.ticket,
        parsed_b.pin,
        &b_path,
        "票据 B",
        no_identity,
    )
    .expect("B create via ticket");

    assert_eq!(a.vault_id, VAULT_A);
    assert_eq!(b.vault_id, VAULT_B);
    assert_ne!(a.identity.device_id, b.identity.device_id, "设备身份独立");
    assert_ne!(a.identity.vault_id, b.identity.vault_id);
    assert_eq!(a.warning, None);
    assert_eq!(b.warning, None);

    // 各自 durable pin：rev1 + record hash；instance 相同（同一 relay），vault 绑定各自。
    let pin_a = load_control_pin(&a.vault, VAULT_A)
        .expect("A pin")
        .expect("A 必须有强 pin");
    let pin_b = load_control_pin(&b.vault, VAULT_B)
        .expect("B pin")
        .expect("B 必须有强 pin");
    assert_eq!(pin_a.instance_id, pin_b.instance_id, "同一 relay 实例");
    assert_eq!(pin_a.instance_id, a.identity.pinned_instance);
    assert_eq!(pin_b.instance_id, b.identity.pinned_instance);
    assert_eq!(pin_a.record.rev, 1);
    assert_eq!(pin_b.record.rev, 1);
    assert!(pin_a.record.hash.is_some());
    assert!(pin_b.record.hash.is_some());

    // v2 context 打开两个库：`/state` 必须各回自己的 vault（客户端会逐字段校验）。
    let mut client_a = RelayContext::local_dev(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("ctx")
        .with_api_version(RelayApiVersion::V2)
        .open(&a.vault, &a.identity)
        .expect("open A")
        .into_client();
    let mut client_b = RelayContext::local_dev(relay.base_url(), DEFAULT_TRANSPORT_TIMEOUTS)
        .expect("ctx")
        .with_api_version(RelayApiVersion::V2)
        .open(&b.vault, &b.identity)
        .expect("open B")
        .into_client();
    assert_eq!(client_a.state().expect("A state").vault_id, VAULT_A);
    assert_eq!(client_b.state().expect("B state").vault_id, VAULT_B);

    // 相同 object_id 在两边各自成立：A 放 A 的对象，B 里一开始没有它。
    let root_a = RootSecretV1::new(VAULT_A, 1, a.identity.current_root.clone());
    let root_b = RootSecretV1::new(VAULT_B, 1, b.identity.current_root.clone());
    let key_a =
        menghunote_sync::device::DeviceSigningKey::from_seed(a.identity.ed25519_seed.clone());
    let key_b =
        menghunote_sync::device::DeviceSigningKey::from_seed(b.identity.ed25519_seed.clone());
    let uploaded_a = upload_object(
        &mut client_a,
        VAULT_A,
        a.identity.device_id,
        &key_a,
        &root_a,
        ObjectType::Asset,
        OBJECT,
        [0x31; 16],
        0,
        0,
        b"vault-a-actual-bytes",
        &[],
        None,
    )
    .expect("A upload object");
    let (_, fetched_a) = fetch_object(
        &mut client_a,
        &root_a,
        ObjectType::Asset,
        &OBJECT,
        uploaded_a.part_count,
        uploaded_a.total_plain_len,
        &uploaded_a.object_digest,
        &[],
    )
    .expect("A fetch object");
    assert_eq!(fetched_a, b"vault-a-actual-bytes");

    let error = match fetch_object(
        &mut client_b,
        &root_b,
        ObjectType::Asset,
        &OBJECT,
        uploaded_a.part_count,
        uploaded_a.total_plain_len,
        &uploaded_a.object_digest,
        &[],
    ) {
        Ok(_) => panic!("B 不应看到 A 的对象"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, SyncError::RelayApi { code, .. } if code == "NOT_FOUND"),
        "{error:?}"
    );

    let uploaded_b = upload_object(
        &mut client_b,
        VAULT_B,
        b.identity.device_id,
        &key_b,
        &root_b,
        ObjectType::Asset,
        OBJECT,
        [0x31; 16],
        0,
        0,
        b"vault-b-actual-bytes",
        &[],
        None,
    )
    .expect("B upload same object id");
    let (_, fetched_b) = fetch_object(
        &mut client_b,
        &root_b,
        ObjectType::Asset,
        &OBJECT,
        uploaded_b.part_count,
        uploaded_b.total_plain_len,
        &uploaded_b.object_digest,
        &[],
    )
    .expect("B fetch object");
    assert_eq!(fetched_b, b"vault-b-actual-bytes");
    let (_, fetched_a_again) = fetch_object(
        &mut client_a,
        &root_a,
        ObjectType::Asset,
        &OBJECT,
        uploaded_a.part_count,
        uploaded_a.total_plain_len,
        &uploaded_a.object_digest,
        &[],
    )
    .expect("A object 不变");
    assert_eq!(fetched_a_again, b"vault-a-actual-bytes");

    // ticket/链接不落本地：实例目录只有库/边车；没有配置或日志文件。
    assert_ticket_not_persisted(&dir.path().join("a"), &raw_a);
    assert_ticket_not_persisted(&dir.path().join("b"), &raw_b);
}

/// 票据错误分类：错 vault/过期/已用 → 403；同 vault 重复（quota>1）→ 409；
/// 限流 → 429。失败不建库、不消费票据（随后同一张票仍能成功）。
#[test]
fn ticket_errors_are_classified_and_failures_do_not_consume_the_ticket() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("errors");
    let context = || local_context(&relay);

    // 1) 错 vault：票据绑定 VAULT_A，却用 VAULT_B 创建 → 403（不泄露票据/租户原因）。
    let wrong_vault_ticket = ticket(&relay, VAULT_A, 900, 1);
    let wrong_path = dir.path().join("wrong/vault.sqlite3");
    let error = expect_create_error(create_vault_with_ticket(
        context(),
        VAULT_B,
        &wrong_vault_ticket,
        None,
        &wrong_path,
        "错 vault",
        no_identity,
    ));
    assert_eq!(relay_code(&error), ("FORBIDDEN", false));
    assert_no_vault_files(&wrong_path);
    // 失败**不消费**票据：同一个 ticket 用正确的 vault 仍然成功。
    let ok_path = dir.path().join("ok/vault.sqlite3");
    create_vault_with_ticket(
        context(),
        VAULT_A,
        &wrong_vault_ticket,
        None,
        &ok_path,
        "同票据重试成功",
        no_identity,
    )
    .expect("失败不应消费票据");

    // 2) 已用（quota=1）：同一张票再用一次 → 403（票据先于 vault 存在性检查）。
    let replay_path = dir.path().join("replay/vault.sqlite3");
    let error = expect_create_error(create_vault_with_ticket(
        context(),
        VAULT_A,
        &wrong_vault_ticket,
        None,
        &replay_path,
        "重放",
        no_identity,
    ));
    assert_eq!(relay_code(&error), ("FORBIDDEN", false));
    assert_no_vault_files(&replay_path);

    // 3) 过期：TTL 1s + 推进 relay 假时钟 2s → 403。
    let expired = ticket(&relay, VAULT_C, 1, 1);
    relay.advance_secs(2);
    let expired_path = dir.path().join("expired/vault.sqlite3");
    let error = expect_create_error(create_vault_with_ticket(
        context(),
        VAULT_C,
        &expired,
        None,
        &expired_path,
        "过期",
        no_identity,
    ));
    assert_eq!(relay_code(&error), ("FORBIDDEN", false));
    assert_no_vault_files(&expired_path);

    // 4) 同 vault 重复创建（quota=2）：票据仍有效，但 vault 已存在 → 409 ALREADY_INITIALIZED。
    let duplicate_id = [0xD4; 16];
    let duplicate_ticket = ticket(&relay, duplicate_id, 900, 2);
    let first_path = dir.path().join("dup-1/vault.sqlite3");
    create_vault_with_ticket(
        context(),
        duplicate_id,
        &duplicate_ticket,
        None,
        &first_path,
        "第一次",
        no_identity,
    )
    .expect("第一次创建成功");
    let second_path = dir.path().join("dup-2/vault.sqlite3");
    let error = expect_create_error(create_vault_with_ticket(
        context(),
        duplicate_id,
        &duplicate_ticket,
        None,
        &second_path,
        "第二次",
        no_identity,
    ));
    assert_eq!(relay_code(&error), ("ALREADY_INITIALIZED", false));
    assert_no_vault_files(&second_path);
}

/// `/v2/vaults` 入口限流 → 429 RATE_LIMITED（retryable=true）；失败不建库。
#[test]
fn v2_create_rate_limit_is_classified_as_retryable_429() {
    let limit = menghunote_relay::ratelimit::RateLimitSpec::new(1, 60).expect("限流参数");
    let relay = DevRelay::start_with_v2_create_limit(Some(limit)).expect("起限流 relay 失败");
    let dir = temp_root("rate");

    let first = [0xE1; 16];
    let first_ticket = ticket(&relay, first, 900, 1);
    create_vault_with_ticket(
        local_context(&relay),
        first,
        &first_ticket,
        None,
        &dir.path().join("first/vault.sqlite3"),
        "限流前",
        no_identity,
    )
    .expect("第一次创建成功");

    // 第二次请求在限流器处被拒（票据本身有效、vault 也还没建）。
    let second = [0xE2; 16];
    let second_ticket = ticket(&relay, second, 900, 1);
    let second_path = dir.path().join("second/vault.sqlite3");
    let error = expect_create_error(create_vault_with_ticket(
        local_context(&relay),
        second,
        &second_ticket,
        None,
        &second_path,
        "限流后",
        no_identity,
    ));
    assert_eq!(relay_code(&error), ("RATE_LIMITED", true));
    assert_no_vault_files(&second_path);
}

/// 链接里的公开锚 `pin`：同实例成功；不一致必须 `InstanceMismatch` 且不换 pin、
/// 本地不留半初始化材料（远端可能已建/消费票据，错误如实返回）。
#[test]
fn create_link_pin_anchor_is_enforced_without_switching_instances() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("pin");

    // 先建一个 vault 学习 relay instance（仅测试；生产由创建方/链接携带）。
    let first = [0xF1; 16];
    let first_ticket = ticket(&relay, first, 900, 1);
    let created = create_vault_with_ticket(
        local_context(&relay),
        first,
        &first_ticket,
        None,
        &dir.path().join("first/vault.sqlite3"),
        "学习 instance",
        no_identity,
    )
    .expect("first create");
    let instance = created.identity.pinned_instance;

    // 正确的 pin：解析链接 → 创建成功。
    let good_id = [0xF2; 16];
    let good_link = CreateLink::encode(
        RelayMode::LocalDev,
        relay.base_url(),
        good_id,
        ticket(&relay, good_id, 900, 1),
        Some(instance),
    )
    .expect("good link");
    let parsed = CreateLink::parse(&good_link.url).expect("parse");
    assert_eq!(parsed.pin, Some(instance));
    create_vault_with_ticket(
        parsed
            .relay_context(DEFAULT_TRANSPORT_TIMEOUTS)
            .expect("ctx"),
        parsed.vault_id,
        &parsed.ticket,
        parsed.pin,
        &dir.path().join("good/vault.sqlite3"),
        "同实例",
        no_identity,
    )
    .expect("pin 一致必须成功");

    // 错误的 pin：拒绝（不自动换 pin），本地不留库文件。
    let bad_id = [0xF3; 16];
    let bad_ticket = ticket(&relay, bad_id, 900, 1);
    let bad_path = dir.path().join("bad/vault.sqlite3");
    let error = expect_create_error(create_vault_with_ticket(
        local_context(&relay),
        bad_id,
        &bad_ticket,
        Some([0xFF; 16]),
        &bad_path,
        "错实例",
        no_identity,
    ));
    assert!(matches!(error, SyncError::InstanceMismatch), "{error:?}");
    assert_no_vault_files(&bad_path);
}

/// v2 面的管理写经过真实 HTTP 后，**控制链历史记录**也能被 v2 客户端重新验签
/// （证明请求 path 与管理签名 path 是同一个 `/v2/...`，不是只对 stub 成立）。
#[test]
fn v2_management_record_is_verifiable_from_the_control_chain() {
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("control-chain");
    let vault_id = [0xC4; 16];
    let vault_path = dir.path().join("vault.sqlite3");
    let created = create_vault_with_ticket(
        local_context(&relay),
        vault_id,
        &ticket(&relay, vault_id, 900, 1),
        None,
        &vault_path,
        "控制链",
        no_identity,
    )
    .expect("ticket create");

    let context = local_context(&relay);
    let mut client = context
        .open(&created.vault, &created.identity)
        .expect("open")
        .into_client();
    let state = client.state().expect("state");
    let (control_rev, control_hash) = client.control_head().expect("control head");
    let manager = ManagementKey::from_seed(
        created
            .identity
            .manager_seed
            .clone()
            .expect("创建端有管理密钥"),
    );
    let mut journal = menghunote_sync::rotation_journal::RotationJournal::freeze(
        &created.vault,
        state.epoch,
        [0x77; 16],
        control_rev,
        control_hash,
    )
    .expect("freeze journal");
    journal
        .confirm_r_prime_backed_up(&created.vault)
        .expect("确认备份");
    let mut transport = context.transport().expect("transport");
    menghunote_sync::rotation_stage::begin_rotation_http(
        &mut transport,
        &context.api(),
        &created.vault,
        &mut journal,
        vault_id,
        &state.instance_id,
        state.log_id,
        &manager,
        &[],
        SystemClock.now_secs(),
    )
    .expect("v2 begin rotation");

    // 新 token（begin 推进 auth_rev）+ 重新拉控制链：typed rotation.begin 记录只有在
    // 客户端用 `/v2/...` 重建 path 时才能通过管理验签。
    client.clear_auth_cache();
    let (head_rev, _) = client.control_head().expect("v2 控制链验签必须通过");
    assert_eq!(head_rev, control_rev + 1, "begin 追加一条控制记录");
}

// ---------------------------------------------------------------------------
// v2 路径 / 管理签名域（stub transport 记录实际请求）
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingTransport {
    responses: VecDeque<HttpResponse>,
    requests: Rc<RefCell<Vec<HttpRequest>>>,
}

impl RecordingTransport {
    fn new(responses: Vec<HttpResponse>) -> (Self, Rc<RefCell<Vec<HttpRequest>>>) {
        let requests = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                responses: responses.into(),
                requests: requests.clone(),
            },
            requests,
        )
    }
}

impl HttpTransport for RecordingTransport {
    fn send(&mut self, request: &HttpRequest) -> Result<HttpResponse, SyncError> {
        self.requests.borrow_mut().push(request.clone());
        self.responses
            .pop_front()
            .ok_or(SyncError::Transport("测试没有更多预设响应"))
    }
}

fn cbor_response(fields: cbor::Fields) -> HttpResponse {
    HttpResponse {
        status: 200,
        body: cbor::encode(fields).expect("测试响应编码"),
    }
}

fn auth_responses(now: u64, vault_id: [u8; 16]) -> Vec<HttpResponse> {
    vec![
        cbor_response(vec![
            (0, cbor::bytes([0x01; 16])),
            (1, cbor::uint(1)),
            (2, cbor::bytes([0x04; 16])),
            (3, cbor::bytes([0x02; 32])),
            (4, cbor::uint(now + 600)),
            (5, cbor::bytes(vault_id)),
            (6, cbor::bytes([0x22; 16])),
            (7, cbor::uint(now)),
        ]),
        cbor_response(vec![
            (0, cbor::text("token")),
            (1, cbor::uint(now + 600)),
            (2, cbor::uint(now)),
            (3, cbor::uint(1)),
            (4, cbor::uint(0)),
        ]),
    ]
}

fn state_response(vault_id: [u8; 16]) -> HttpResponse {
    cbor_response(vec![
        (0, cbor::bytes(vault_id)),
        (1, cbor::bytes([0x0a; 16])),
        (2, cbor::bytes([0x22; 16])),
        (3, cbor::text("active")),
        (4, cbor::uint(1)),
        (5, cbor::uint(1)),
        (6, cbor::uint(0)),
        (7, cbor::uint(0)),
        (8, cbor::uint(0)),
    ])
}

/// v2 context 的鉴权/state/对象分片路径全部是 `/v2/...`（全局 `/v1/auth/*` 不出现）。
#[test]
fn v2_context_paths_are_all_tenant_scoped() {
    let now = SystemClock.now_secs();
    let v1_client_config = HttpRelayConfig {
        base_url: "http://127.0.0.1:18080".to_owned(),
        api_version: RelayApiVersion::V1,
        vault_id: VAULT_A,
        device_id: [0x04; 16],
        manager_pub: None,
    };
    let (transport, requests) = RecordingTransport::new({
        let mut responses = auth_responses(now, VAULT_A);
        responses.push(state_response(VAULT_A));
        responses
    });
    let mut client = HttpRelayClient::new(
        transport,
        v1_client_config,
        menghunote_sync::device::DeviceSigningKey::from_seed(Secret32::from_array([0x09; 32])),
    );
    client.state().expect("v1 state");
    let v1_paths: Vec<String> = requests
        .borrow()
        .iter()
        .map(|request| request.path.clone())
        .collect();
    assert_eq!(
        v1_paths,
        vec![
            "/v1/auth/challenges".to_owned(),
            "/v1/auth/tokens".to_owned(),
            "/v1/vaults/oaGhoaGhoaGhoaGhoaGhoQ/state".to_owned(),
        ]
    );

    let now = SystemClock.now_secs();
    let (transport, requests) = RecordingTransport::new({
        let mut responses = auth_responses(now, VAULT_A);
        responses.push(state_response(VAULT_A));
        responses.push(HttpResponse {
            status: 200,
            body: b"part-a".to_vec(),
        });
        responses.push(HttpResponse {
            status: 200,
            body: Vec::new(),
        });
        responses
    });
    let v2_config = HttpRelayConfig {
        base_url: "http://127.0.0.1:18080".to_owned(),
        api_version: RelayApiVersion::V2,
        vault_id: VAULT_A,
        device_id: [0x04; 16],
        manager_pub: None,
    };
    let mut client = HttpRelayClient::new(
        transport,
        v2_config,
        menghunote_sync::device::DeviceSigningKey::from_seed(Secret32::from_array([0x09; 32])),
    );
    client.state().expect("v2 state");
    assert_eq!(client.part(&OBJECT, 1).expect("v2 part"), b"part-a");
    client
        .put_part(&OBJECT, 1, b"envelope")
        .expect("v2 put part");
    let paths: Vec<String> = requests
        .borrow()
        .iter()
        .map(|request| request.path.clone())
        .collect();
    let id = "oaGhoaGhoaGhoaGhoaGhoQ";
    let object = "QkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkI";
    assert_eq!(
        paths,
        vec![
            format!("/v2/vaults/{id}/auth/challenges"),
            format!("/v2/vaults/{id}/auth/tokens"),
            format!("/v2/vaults/{id}/state"),
            format!("/v2/vaults/{id}/objects/{object}/parts/1"),
            format!("/v2/vaults/{id}/objects/{object}/parts/1"),
        ]
    );
    assert!(
        paths.iter().all(|path| !path.starts_with("/v1/")),
        "v2 context 不得回落到 v1 路径"
    );
}

/// 管理签名覆盖的 path 必须与 v2 面一致；用 v1 path 复算必须验签失败。
#[test]
fn management_signatures_bind_the_v2_path() {
    use menghunote_sync::device::verify_signature;
    use menghunote_sync_proto::RotationActivateBodyV1;

    let manager = ManagementKey::from_seed(Secret32::from_array([0x33; 32]));
    let instance = [0x22; 16];
    let request_id = [0x55; 16];
    let rotation = [0x66; 16];
    let body = RotationActivateBodyV1 {
        primary_checkpoint: Some([0x11; 16]),
        fallback_checkpoint: Some([0x12; 16]),
        member_cert_digest: Some([0x21; 32]),
        grants_digest: Some([0x22; 32]),
        slot_set_digest: Some([0x23; 32]),
        asset_set_digest: Some([0x24; 32]),
        new_manager: None,
    };
    let signed = menghunote_sync::rotation::sign_activate(
        &manager,
        &instance,
        VAULT_A,
        rotation,
        request_id,
        2,
        &body,
        RelayApiVersion::V2,
    )
    .expect("v2 sign activate");

    let path_v2 = RelayApi::new("", RelayApiVersion::V2).vault_path(
        &VAULT_A,
        &format!("/rotations/{}/activate", base64_of(&rotation)),
    );
    let digest_v2: [u8; 32] =
        sha256(&management_request_cde(HttpMethod::Post, &path_v2, &signed.body).expect("cde"));
    let input_v2 = management_signing_input(&instance, &VAULT_A, &request_id, 2, &digest_v2)
        .expect("sign input");
    verify_signature(&manager.public_key(), &input_v2, &signed.signature)
        .expect("v2 path 的管理签名必须自洽");

    // 用同一份 body 但换回 v1 path：签名必须失败（签名域绑定面）。
    let path_v1 = format!(
        "/v1/vaults/{}/rotations/{}/activate",
        base64_of(&VAULT_A),
        base64_of(&rotation)
    );
    let digest_v1: [u8; 32] =
        sha256(&management_request_cde(HttpMethod::Post, &path_v1, &signed.body).expect("cde"));
    let input_v1 = management_signing_input(&instance, &VAULT_A, &request_id, 2, &digest_v1)
        .expect("sign input");
    assert!(
        verify_signature(&manager.public_key(), &input_v1, &signed.signature).is_err(),
        "v1 path 不得验证 v2 签名"
    );
}

/// 管理端吊销：实际请求 path 与签名覆盖的 path 完全一致（都是 `/v2/...`）；
/// 用 v1 path 复算签名必须失败。
#[test]
fn v2_device_revoke_request_path_matches_the_signed_domain() {
    use menghunote_sync::device::verify_signature;
    let now = SystemClock.now_secs();
    let manager = ManagementKey::from_seed(Secret32::from_array([0x33; 32]));
    let target = [0x07; 16];
    let ctx = RevokeContext::from_identity(
        "http://127.0.0.1:18080",
        &identity_for_revoke(VAULT_A, manager.public_key()),
        5,
        now,
    )
    .with_api_version(RelayApiVersion::V2);
    let responses = vec![
        cbor_response(vec![
            (0, cbor::bytes([0x01; 16])),
            (1, cbor::uint(2)),
            (2, cbor::bytes(management_subject(&manager.public_key()))),
            (3, cbor::bytes([0x02; 32])),
            (4, cbor::uint(now + 600)),
            (5, cbor::bytes(VAULT_A)),
            (6, cbor::bytes([0x22; 16])),
        ]),
        cbor_response(vec![
            (0, cbor::text("management-token")),
            (1, cbor::uint(now + 600)),
            (2, cbor::uint(now)),
            (3, cbor::uint(2)),
            (4, cbor::uint(0)),
        ]),
        cbor_response(vec![
            (0, cbor::bytes(target)),
            (1, cbor::text("revoked")),
            (2, cbor::boolean(true)),
            (3, cbor::uint(1)),
            (4, cbor::uint(5)),
        ]),
    ];
    let (transport, requests) = RecordingTransport::new(responses);
    let mut transport = transport;
    let request_id = [0x55; 16];
    revoke_device(&mut transport, &ctx, &manager, target, request_id).expect("revoke");

    let requests = requests.borrow();
    let revoke_request = requests.last().expect("回执请求");
    let expected_path = format!(
        "/v2/vaults/{}/devices/{}/revoke",
        base64_of(&VAULT_A),
        base64_of(&target)
    );
    let expected_url = format!("http://127.0.0.1:18080{expected_path}");
    assert_eq!(revoke_request.path, expected_url);
    // 管理签名绑定的是**相对 path**（与 relay OriginalUri 的 path 相同）。
    let signed_path = revoke_request
        .path
        .strip_prefix("http://127.0.0.1:18080")
        .expect("绝对 URL 前缀");
    assert_eq!(signed_path, expected_path);

    let fields = cbor::decode(&revoke_request.body).expect("body");
    let signed = cbor::value_map(cbor::field(&fields, 0).expect("signed")).expect("signed map");
    let payload = cbor::bytes_field(&signed, 0).expect("payload");
    let signature: [u8; 64] = cbor::bytes_fixed(&signed, 1).expect("signature");
    let req_id: [u8; 16] = cbor::bytes_fixed(&fields, 1).expect("request id");
    assert_eq!(req_id, request_id);

    let digest_v2: [u8; 32] =
        sha256(&management_request_cde(HttpMethod::Post, signed_path, &payload).expect("cde"));
    let input_v2 = management_signing_input(&[0x22; 16], &VAULT_A, &request_id, 5, &digest_v2)
        .expect("sign input");
    verify_signature(&manager.public_key(), &input_v2, &signature)
        .expect("实际请求 path 必须与管理签名一致");

    let path_v1 = format!(
        "/v1/vaults/{}/devices/{}/revoke",
        base64_of(&VAULT_A),
        base64_of(&target)
    );
    let digest_v1: [u8; 32] =
        sha256(&management_request_cde(HttpMethod::Post, &path_v1, &payload).expect("cde"));
    let input_v1 = management_signing_input(&[0x22; 16], &VAULT_A, &request_id, 5, &digest_v1)
        .expect("sign input");
    assert!(
        verify_signature(&manager.public_key(), &input_v1, &signature).is_err(),
        "v1 path 不得验证 v2 吊销签名"
    );
}

fn identity_for_revoke(vault_id: [u8; 16], manager_pub: [u8; 32]) -> LocalIdentityV1 {
    LocalIdentityV1 {
        vault_id,
        device_id: [0x04; 16],
        peer_id: menghunote_sync::ids::PeerId::from_bytes([0x06; 16]),
        ed25519_seed: Secret32::from_array([0x09; 32]),
        x25519_secret: Secret32::from_array([0x0a; 32]),
        current_epoch: 1,
        current_root: Secret32::from_array([0x0b; 32]),
        manager_seed: None,
        pinned_instance: [0x22; 16],
        pinned_manager: manager_pub,
        control_rev: 5,
        pending_rotation: None,
    }
}

fn management_subject(manager_pub: &[u8; 32]) -> [u8; 16] {
    let digest = sha256(manager_pub);
    let mut subject = [0u8; 16];
    subject.copy_from_slice(&digest[..16]);
    subject
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).into()
}

fn base64_of(bytes: &[u8]) -> String {
    menghunote_sync::base64url::encode(bytes)
}
