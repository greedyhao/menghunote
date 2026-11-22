//! 真实 localhost 端到端示例（S7-11）：对跑 `menghunote-relay` 的 dev_server。
//!
//! 用法：
//!
//! ```text
//! cargo run -p menghunote-relay --example dev_server -- --reset --setup-only
//! # 复制输出的 setup secret
//! cargo run -p menghunote-sync --example e2e_localhost -- \
//!     --relay http://127.0.0.1:18080 --setup-secret <secret>
//! ```
//!
//! 流程（每步打印可判定日志：seq/cursor/epoch/object_id）：
//!
//! 1. genesis（首次）：本地造根/设备/恢复码/初始槽 → `POST /v1/vaults` → DPAPI 落盘身份；
//! 2. 写 2 条笔记（走 storage）→ 加密推送 → 拉取自己的回环（按 end_vv 直接 integrated，不 echo）；
//! 3. 用同一游标重拉一页 log，逐条按 §2 验 `ObjectCommit` 签名（第二客户端 bootstrap 的前置）；
//! 4. 模拟重启：重新打开 Vault / 重新认证 → 按 cursor 续传（应 0 事件）→ 再写 1 条并推送/拉取。
//!
//! 首次运行会把 DPAPI 保护的 `identity.bin`、`identity.meta`（公开 id）与两个槽体写进
//! `--dir`；再次运行同一 `--dir` 即走"重启续传"分支，不会重复 genesis。
//!
//! 不做：新设备 bootstrap 全量恢复、checkpoint、轮换、TLS。
//!
//! 本文件同时被 `e2e_rotation_verify`（`cargo run -p menghunote-sync --example
//! e2e_rotation_verify`）以 `#[path] mod` 复用：可调用入口是
//! `run`/`run_with`（`main` 只负责 parse + 退出码），外置 relay 的 CLI 行为不变。

#![allow(dead_code)]

use std::env;
use std::error::Error as StdError;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use menghunote_storage::{
    append_updates_with_sync_intents, create_body_doc, doc_updates_since, ensure_structure_row,
    insert_asset, load_body_doc, load_head, sync_cursor_state, BodyDoc, SyncIntent, UpdateRecord,
    Vault,
};
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::checkpoint::{CheckpointWriter, EmptyAssets, StorageAssets};
use menghunote_sync::device::{verify_signature, DeviceSigningKey};
use menghunote_sync::engine::{
    AckReport, EngineIdentity, IntegrateReport, PullReport, PushReport, SyncEngine,
    RELAY_CURSOR_META,
};
use menghunote_sync::genesis;
use menghunote_sync::http_relay::{HttpRelayClient, HttpRelayConfig, StdTcpTransport};
use menghunote_sync::keys::RootSecretV1;
use menghunote_sync::random::{array, OsRandom};
use menghunote_sync::relay::RelayClient;
use menghunote_sync::{ids, DeviceId, VaultId};
use menghunote_sync_proto::{Envelope, ObjectCommit};
use sha2::{Digest, Sha256};

type AnyResult<T> = Result<T, Box<dyn StdError>>;

const DOCS: [&str; 2] = ["note-1", "note-2"];
const DOC_AFTER_RESTART: &str = "note-3";

/// CLI 缺省口令（`--password` 未给时）；仅用于包 e2e 槽，不是安全边界。
pub const DEFAULT_PASSWORD: &str = "e2e-password";

/// 一次 localhost e2e 的结论摘要（`e2e_rotation_verify` 串联两阶段时打印用）。
#[derive(Debug, Clone, Copy)]
pub struct Summary {
    pub vault_id: VaultId,
    pub device_id: DeviceId,
    pub integrated_seq: u64,
}

fn main() -> ExitCode {
    match run_cli() {
        Ok(()) => {
            println!("[e2e] OK");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("[e2e] FAILED: {error}");
            ExitCode::FAILURE
        }
    }
}

/// CLI 入口：parse_args → [`run_with`]；旧参数/退出码契约不变。
fn run_cli() -> AnyResult<()> {
    let args = parse_args()?;
    let summary = run_with(
        &args.relay,
        Some(args.setup_secret.as_str()),
        &args.dir,
        &args.password,
        args.genesis_only,
    )?;
    println!(
        "[e2e] summary vault={} device={} integrated={}",
        ids::hex_of(&summary.vault_id),
        ids::hex_of(&summary.device_id),
        summary.integrated_seq
    );
    Ok(())
}

/// 可调用入口（`e2e_rotation_verify` 用）：缺省口令、非 `--genesis-only`。
///
/// `setup_secret` 只在**首次** genesis 需要；重启分支（目录已有身份）可为 `None`。
pub fn run(relay: &str, setup_secret: Option<&str>, dir: &Path) -> AnyResult<Summary> {
    run_with(relay, setup_secret, dir, DEFAULT_PASSWORD, false)
}

struct Args {
    relay: String,
    setup_secret: String,
    dir: PathBuf,
    password: String,
    genesis_only: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut relay = env::var("RELAY_DEV_URL").unwrap_or_else(|_| "http://127.0.0.1:18080".into());
    let mut setup_secret = env::var("RELAY_SETUP_SECRET").unwrap_or_default();
    let mut dir = default_dir();
    let mut password = "e2e-password".to_owned();
    let mut genesis_only = false;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--relay" => relay = args.next().ok_or("--relay 需要参数")?,
            "--setup-secret" | "--setup" => {
                setup_secret = args.next().ok_or("--setup/--setup-secret 需要参数")?
            }
            "--dir" | "--vault" => {
                dir = PathBuf::from(args.next().ok_or("--dir/--vault 需要参数")?)
            }
            "--password" => password = args.next().ok_or("--password 需要参数")?,
            "--genesis-only" => genesis_only = true,
            "-h" | "--help" => {
                println!(
                    "e2e_localhost --relay <http://host:port> --setup-secret <secret> [--dir <dir>] [--password <p>]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("未知参数：{other}")),
        }
    }
    if setup_secret.is_empty() {
        return Err("缺少 --setup-secret（或环境变量 RELAY_SETUP_SECRET）".to_owned());
    }
    Ok(Args {
        relay,
        setup_secret,
        dir,
        password,
        genesis_only,
    })
}

fn default_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("tmp")
        .join("sync-e2e-localhost")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// 完整参数版：保留 `--password` / `--genesis-only` 的旧 CLI 行为。
pub fn run_with(
    relay: &str,
    setup_secret: Option<&str>,
    dir: &Path,
    password: &str,
    genesis_only: bool,
) -> AnyResult<Summary> {
    fs::create_dir_all(dir)?;
    let vault_path = dir.join("vault.sqlite3");
    let identity_path = dir.join("identity.bin");
    let meta_path = dir.join("identity.meta");

    let first_run = !(identity_path.exists() && meta_path.exists());
    println!(
        "[e2e] relay={} dir={} first_run={first_run}",
        relay,
        dir.display()
    );

    let (
        vault_id,
        device_id,
        peer_id,
        signing_key,
        root,
        manager_pub,
        genesis_cursor,
        genesis_slots,
    ) = if first_run {
        // 首次 genesis 必须拿 setup secret；非首次（重启续传）不需要。
        let setup_secret = setup_secret.ok_or(
            "首次 genesis 需要 setup secret（--setup/--setup-secret 或 RELAY_SETUP_SECRET）",
        )?;
        // 首次初始化：Linux 用户主密钥（Windows no-op）。重启分支只 load，不初始化。
        menghunote_sync::identity_wrap::initialize_user_key()?;
        let mut transport = StdTcpTransport::new_local_dev(relay)?;
        let mut rng = OsRandom;
        let outcome = genesis::run(&mut transport, setup_secret, password, now_secs(), &mut rng)?;
        let identity_blob = outcome.identity.protect()?;
        fs::write(&identity_path, &identity_blob)?;
        fs::write(
            &meta_path,
            format!(
                "{} {}\n",
                ids::hex_of(&outcome.materials.vault_id),
                ids::hex_of(&outcome.materials.device_id)
            ),
        )?;
        fs::write(
            dir.join("slot-password.bin"),
            outcome.materials.password_slot.wrapped_bytes(),
        )?;
        fs::write(
            dir.join("slot-password.desc.bin"),
            outcome.materials.password_slot.descriptor.encode()?,
        )?;
        fs::write(
            dir.join("slot-recovery.bin"),
            outcome.materials.recovery_slot.wrapped_bytes(),
        )?;
        fs::write(
            dir.join("slot-recovery.desc.bin"),
            outcome.materials.recovery_slot.descriptor.encode()?,
        )?;
        println!(
                "[e2e] genesis ok vault={} device={} instance={} log_id={} epoch={} control_rev={} auth_rev={}",
                ids::hex_of(&outcome.materials.vault_id),
                ids::hex_of(&outcome.materials.device_id),
                ids::hex_of(&outcome.instance_id),
                ids::hex_of(&outcome.log_id),
                outcome.epoch,
                outcome.control_rev,
                outcome.auth_rev
            );
        println!(
            "[e2e] recovery code (只显示一次，请抄走)：{}",
            outcome.materials.recovery_code.as_str()
        );
        // dev e2e：把恢复码写到目录里供 e2e_bootstrap 分离进程使用（生产不得这样做）。
        fs::write(
            dir.join("recovery-code.txt"),
            outcome.materials.recovery_code.as_str(),
        )?;
        println!(
            "[e2e] device fingerprint：{}",
            outcome.materials.fingerprint().to_hex()
        );
        let peer = outcome.materials.peer_id_u64();
        let signing = outcome.materials.signing_key.clone();
        let root = outcome.materials.root.clone();
        let manager_pub = outcome.materials.manager_pub();
        (
            outcome.materials.vault_id,
            outcome.materials.device_id,
            peer,
            signing,
            root,
            Some(manager_pub),
            outcome.initial_cursor,
            Some((
                outcome.materials.password_slot.clone(),
                outcome.materials.recovery_slot.clone(),
                outcome.materials.recovery_seed.clone(),
            )),
        )
    } else {
        let meta = fs::read_to_string(&meta_path)?;
        let mut parts = meta.split_whitespace();
        let vault_id: VaultId =
            ids::parse_hex::<16>(parts.next().ok_or("identity.meta 缺 vault_id")?)
                .map_err(|_| std::io::Error::other("identity.meta vault_id 非法"))?;
        let device_id: DeviceId =
            ids::parse_hex::<16>(parts.next().ok_or("identity.meta 缺 device_id")?)
                .map_err(|_| std::io::Error::other("identity.meta device_id 非法"))?;
        let blob = fs::read(&identity_path)?;
        let identity =
            menghunote_sync::identity::LocalIdentityV1::unprotect(&blob, &vault_id, &device_id)?;
        let signing = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
        let root = RootSecretV1::new(
            identity.vault_id,
            identity.current_epoch,
            identity.current_root.clone(),
        );
        let mut head = [0u8; 8];
        head.copy_from_slice(&identity.peer_id.as_bytes()[..8]);
        let peer = u64::from_be_bytes(head) & 0x7fff_ffff_ffff_ffff;
        println!(
            "[e2e] 已加载 DPAPI 身份 vault={} device={} epoch={} control_rev={}",
            ids::hex_of(&identity.vault_id),
            ids::hex_of(&identity.device_id),
            identity.current_epoch,
            identity.control_rev
        );
        (
            identity.vault_id,
            identity.device_id,
            peer,
            signing,
            root,
            Some(identity.pinned_manager),
            None,
            None,
        )
    };

    if genesis_only {
        println!("[e2e] --genesis-only：身份/槽已落盘，停止在联网同步之前");
        return Ok(Summary {
            vault_id,
            device_id,
            integrated_seq: 0,
        });
    }

    let mut client = HttpRelayClient::new(
        StdTcpTransport::new_local_dev(relay)?,
        HttpRelayConfig {
            base_url: relay.to_owned(),
            api_version: RelayApiVersion::V1,
            vault_id,
            device_id,
            manager_pub,
        },
        signing_key.clone(),
    );

    let vault = Vault::open(&vault_path)?;
    // 主 vault 也需要 structure 行：checkpoint 必须含 structure 快照（bootstrap 依赖）。
    ensure_structure_row(&vault)?;
    let stored_cursor = vault
        .get_meta(RELAY_CURSOR_META)?
        .map(String::from_utf8)
        .transpose()
        .map_err(|_| std::io::Error::other("本地游标不是 UTF-8"))?;
    let cursor = match stored_cursor {
        Some(cursor) => cursor,
        None => {
            let cursor = match genesis_cursor {
                Some(cursor) => {
                    println!("[e2e] 初始游标来自 genesis 响应键 5");
                    cursor
                }
                None => {
                    println!("[e2e] genesis 响应没有键 5，尝试 GET /state 键 14");
                    client.initial_cursor()?
                }
            };
            vault.set_meta(RELAY_CURSOR_META, cursor.as_bytes())?;
            cursor
        }
    };
    println!("[e2e] cursor={cursor}");

    let identity = EngineIdentity {
        device_id,
        signing_key: signing_key.clone(),
        peer_id,
    };

    if first_run {
        for (index, doc_id) in DOCS.iter().enumerate() {
            create_body_doc(&vault, doc_id)?;
            let doc = BodyDoc::create(*doc_id, peer_id)?;
            doc.insert_scalars(0, &format!("genesis note {index} 的内容\n"))?;
            let object_id = array::<32>(&mut OsRandom)?;
            commit_with_intent(&vault, &doc, &uuid_like(100 + index as u64), object_id)?;
        }
        let report = {
            let mut engine = SyncEngine::new(&vault, &mut client, identity.clone(), root.clone());
            engine.sync_once()?
        };
        print_round("第一轮（首次 genesis）", &report);
        assert_no_echo(&vault)?;
    }

    // 第二客户端 bootstrap 的前置：用 genesis 游标重拉一页，逐条验 ObjectCommit 签名。
    let page = client.log(Some(&cursor), 256)?;
    println!(
        "[e2e] log 验签：events={} next_cursor={} head={} epoch={}",
        page.events.len(),
        page.next_cursor,
        page.query_head,
        page.epoch
    );
    for event in &page.events {
        let Some(signature) = event.signature else {
            return Err(std::io::Error::other("log 事件没有签名").into());
        };
        let object_id = event
            .object_id
            .ok_or_else(|| std::io::Error::other("log 事件缺 object_id"))?;
        let commit = ObjectCommit::decode(&event.event)?;
        if commit.object_id != object_id {
            return Err(std::io::Error::other("ObjectCommit.object_id 与事件不符").into());
        }
        let envelope = Envelope::decode(&client.part(&object_id, 0)?)?;
        let producer = envelope.header.producer_device_id;
        let certificate = client
            .device_cert(&producer)?
            .ok_or_else(|| std::io::Error::other("缺生产者设备证书"))?;
        verify_signature(
            &certificate.sign_ed25519_pub,
            &menghunote_sync_proto::object_commit_signing_input(&commit)?,
            &signature,
        )?;
        println!(
            "[e2e] log 验签通过 seq={} object={} epoch={}",
            event.relay_seq,
            ids::hex_of(&object_id),
            event.epoch
        );
    }

    // 模拟重启：重新打开 Vault（游标在 meta 里）+ 重新认证。
    drop(client);
    let vault = Vault::open(&vault_path)?;
    if genesis_only {
        println!("[e2e] --genesis-only：身份/槽已落盘，停止在联网同步之前");
        return Ok(Summary {
            vault_id,
            device_id,
            integrated_seq: 0,
        });
    }

    let mut client = HttpRelayClient::new(
        StdTcpTransport::new_local_dev(relay)?,
        HttpRelayConfig {
            base_url: relay.to_owned(),
            api_version: RelayApiVersion::V1,
            vault_id,
            device_id,
            manager_pub,
        },
        signing_key.clone(),
    );
    let before = sync_cursor_state(&vault)?;
    println!(
        "[e2e] 模拟重启：received={} integrated={} cursor={}",
        before.received_seq,
        before.integrated_seq,
        vault
            .get_meta(RELAY_CURSOR_META)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    );
    {
        let mut engine = SyncEngine::new(&vault, &mut client, identity.clone(), root.clone());
        let resume = engine.sync_once()?;
        print_round("重启续传", &resume);
        if resume.pull.events == 0 {
            println!("[e2e] 重启后无新事件：cursor 生效");
        } else {
            println!(
                "[e2e] 重启补拉 {} 条事件（上一轮未完成/中断的续传），不是重复消费",
                resume.pull.events
            );
        }

        // 续传后继续写一条并同步，证明重启后可继续工作。
        let doc = if vault_doc_exists(&vault, DOC_AFTER_RESTART)? {
            load_body_doc(&vault, DOC_AFTER_RESTART, peer_id, None)?.0
        } else {
            create_body_doc(&vault, DOC_AFTER_RESTART)?;
            BodyDoc::create(DOC_AFTER_RESTART, peer_id)?
        };
        doc.insert_scalars(doc.text().len(), "-restart 之后的新内容")?;
        let object_id = array::<32>(&mut OsRandom)?;
        commit_with_intent(&vault, &doc, &uuid_like(200), object_id)?;
        let report = engine.sync_once()?;
        print_round("重启后续写", &report);
        assert_no_echo(&vault)?;
    }

    // S7-14：初始槽 + 两份 checkpoint（空库 / 含 1 个合成资产），各自读回导入验证。
    // 放在所有 engine 同步之后：checkpoint 对象会在 relay 日志里产生非 UPDATE 事件，
    // v1 引擎遇到它们会按设计暂停（checkpoint 由 bootstrap 任务消费）。
    if let Some((password_slot, recovery_slot, recovery_seed)) = genesis_slots {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut client,
            vault_id,
            device_id,
            signing_key.clone(),
            root.clone(),
            peer_id,
        );
        let secrets = menghunote_sync::checkpoint::SlotSecrets {
            password: Some(password),
            recovery_seed: Some(&recovery_seed),
        };
        let slots = writer.upload_initial_slots(&password_slot, &recovery_slot, &secrets)?;
        println!(
            "[e2e] slots uploaded password_state={} active_rev={} recovery_state={}",
            slots.password.state, slots.active_password_rev, slots.recovery.state
        );
    }

    // 第二份 checkpoint：主 vault、不含资产（验证 EmptyAssets 路径与无资产基线）。
    // 注意：不能再用"全新空库"生成覆盖 relay head 的 checkpoint——那会假覆盖 S，
    // validate 现在会正确拒绝；空库必须先有游标并真实拉取到 S。
    {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut client,
            vault_id,
            device_id,
            signing_key.clone(),
            root.clone(),
            peer_id,
        );
        let report = writer.create_checkpoint(&EmptyAssets)?;
        let verified = writer.read_back(&report)?;
        println!(
            "[e2e] checkpoint(no-assets) id={} S={} docs={} assets={} verified_docs={} manifest_checked={}",
            ids::hex_of(&report.checkpoint_id),
            report.covered_seq,
            report.docs.len(),
            report.assets.len(),
            verified.docs.len(),
            verified.manifest_checked
        );
    }

    let asset_data = b"real e2e asset bytes".to_vec();
    let asset_hash: [u8; 32] = Sha256::digest(&asset_data).into();
    insert_asset(
        &vault,
        &asset_hash,
        asset_data.len() as u64,
        Some("text/plain"),
        &asset_data,
    )?;
    // e2e 的 note 只有 loro_docs 行、没有 notes 行，故不建 asset_refs（FK 指向 notes）；
    // checkpoint 资产枚举只看 assets 表，足以验证真实资产路径。
    {
        let mut writer = CheckpointWriter::new(
            &vault,
            &mut client,
            vault_id,
            device_id,
            signing_key.clone(),
            root.clone(),
            peer_id,
        );
        let report = writer.create_checkpoint(&StorageAssets)?;
        let verified = writer.read_back(&report)?;
        println!(
            "[e2e] checkpoint(asset) id={} S={} docs={} assets={} verified_docs={} verified_assets={} manifest_checked={} asset_hash={}",
            ids::hex_of(&report.checkpoint_id),
            report.covered_seq,
            report.docs.len(),
            report.assets.len(),
            verified.docs.len(),
            verified.asset_count,
            verified.manifest_checked,
            ids::hex_of(&asset_hash)
        );
    }

    let state = sync_cursor_state(&vault)?;
    println!(
        "[e2e] 最终 received={} integrated={} cursor={}",
        state.received_seq,
        state.integrated_seq,
        vault
            .get_meta(RELAY_CURSOR_META)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    );
    Ok(Summary {
        vault_id,
        device_id,
        integrated_seq: state.integrated_seq,
    })
}

fn vault_doc_exists(vault: &Vault, doc_id: &str) -> Result<bool, Box<dyn StdError>> {
    Ok(menghunote_storage::body_doc_meta(vault, doc_id)?.is_some())
}

fn commit_with_intent(
    vault: &Vault,
    doc: &BodyDoc,
    message_id: &str,
    object_id: [u8; 32],
) -> AnyResult<u64> {
    let head = load_head(vault, doc.doc_id())?;
    let base = head.version_vector()?;
    let update = doc.pending_update(&base)?;
    let intent = SyncIntent {
        message_id: message_id.to_owned(),
        batch_id: format!("batch-{message_id}"),
        key_epoch: 1,
        object_id,
    };
    let state = append_updates_with_sync_intents(
        vault,
        doc.doc_id(),
        head.head_seq,
        &[UpdateRecord::new(intent.batch_id.clone(), update)],
        &[intent],
    )?;
    Ok(state.head_seq)
}

fn uuid_like(value: u64) -> String {
    format!("00000000-0000-4000-8000-{value:012x}")
}

fn assert_no_echo(vault: &Vault) -> AnyResult<()> {
    for doc_id in DOCS.iter().chain(std::iter::once(&DOC_AFTER_RESTART)) {
        if !vault_doc_exists(vault, doc_id)? {
            continue;
        }
        let rows = doc_updates_since(vault, doc_id, 0)?;
        let head = load_head(vault, doc_id)?.head_seq;
        if rows.len() as u64 != head {
            return Err(std::io::Error::other(format!(
                "{doc_id} 的 loro_updates 行数与 head 不一致（可能 echo 了）"
            ))
            .into());
        }
        println!(
            "[e2e] {doc_id} 本地 seq={:?}（无 echo 追加）",
            rows.iter().map(|row| row.seq).collect::<Vec<_>>()
        );
    }
    Ok(())
}

fn print_round(label: &str, report: &menghunote_sync::engine::SyncReport) {
    let PushReport {
        uploaded,
        acked,
        failed,
    } = report.push;
    let pull = &report.pull;
    let PullReport {
        events,
        stored,
        skipped,
        next_cursor,
        update_bytes,
        asset_bytes,
        elapsed_ms,
        ..
    } = pull;
    let IntegrateReport {
        integrated,
        deferred,
        ..
    } = report.integrate;
    let AckReport {
        received_seq,
        integrated_seq,
    } = report.ack;
    println!(
        "[e2e] {label}: encrypted={} uploaded={} acked={} failed={} pull.events={} stored={} skipped={} next_cursor={} integrated={} deferred={} ack.received={} ack.integrated={} pull.update_bytes={} pull.asset_bytes={} pull.elapsed_ms={} integrate.elapsed_ms={}",
        report.encrypted,
        uploaded,
        acked,
        failed,
        events,
        stored,
        skipped,
        next_cursor,
        integrated,
        deferred,
        received_seq,
        integrated_seq,
        update_bytes,
        asset_bytes,
        elapsed_ms,
        report.integrate.elapsed_ms
    );
}
