//! 双目录 bootstrap e2e：源目录已由 `e2e_localhost` 完成 genesis/checkpoints；
//! 本程序只用「源目录的 vault_id + 恢复码」+ relay，恢复到一个全新空目录。
//!
//! 日志固定包含：recovery self-approval / manifest verified / docs imported /
//! assets imported / offline read。

use std::error::Error as StdError;
use std::fs;
use std::path::PathBuf;

use menghunote_storage::{
    asset_bytes, list_all_docs, list_assets, load_body_doc, load_structure, Vault,
};
use menghunote_sync::api_version::RelayApiVersion;
use menghunote_sync::bootstrap;
use menghunote_sync::http_relay::{HttpRelayClient, HttpRelayConfig, StdTcpTransport};
use menghunote_sync::recovery_enroll;
use menghunote_sync::DeviceId;

type AnyResult<T> = Result<T, Box<dyn StdError>>;

fn main() -> AnyResult<()> {
    let mut args = std::env::args().skip(1);
    let mut relay = None;
    let mut source_dir: Option<PathBuf> = None;
    let mut vault_dir: Option<PathBuf> = None;
    let mut recovery_code: Option<String> = None;
    let mut offline_only = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--relay" => relay = args.next(),
            "--source-dir" => source_dir = args.next().map(PathBuf::from),
            "--vault" => vault_dir = args.next().map(PathBuf::from),
            "--recovery-code" => recovery_code = args.next(),
            "--offline-only" => offline_only = true,
            other => return Err(format!("unknown arg {other}").into()),
        }
    }
    if offline_only {
        let vault_dir = vault_dir.ok_or("missing --vault")?;
        let vault = Vault::open(vault_dir.join("vault.sqlite3"))?;
        offline_read(&vault, 1)?;
        return Ok(());
    }
    let relay = relay.ok_or("missing --relay")?;
    let source_dir = source_dir.ok_or("missing --source-dir")?;
    let vault_dir = vault_dir.ok_or("missing --vault")?;
    let recovery_code = recovery_code.ok_or("missing --recovery-code")?;
    let meta = fs::read_to_string(source_dir.join("identity.meta"))?;
    let vault_hex = meta
        .split_whitespace()
        .next()
        .ok_or("identity.meta 缺 vault_id")?;
    let vault_id: [u8; 16] = parse_hex16(vault_hex)?;
    println!(
        "[bootstrap] vault={vault_hex} source={} target={}",
        source_dir.display(),
        vault_dir.display()
    );

    // 1. 恢复码自批（管理 token → invite → redeem → approve）。
    let mut transport = StdTcpTransport::new_local_dev(&relay)?;
    let api = menghunote_sync::api_version::RelayApi::new(&relay, RelayApiVersion::V1);
    let enrolled =
        recovery_enroll::self_enroll(&mut transport, &api, vault_id, &recovery_code, now_secs())?;
    for step in &enrolled.steps {
        println!("[bootstrap] recovery self-approval: {step}");
    }
    let manager_pub = management_pub(&recovery_code, vault_id)?;

    // 2. 设备 token + /bootstrap + manifest/snapshot/asset 导入。
    let mut client = HttpRelayClient::new(
        StdTcpTransport::new_local_dev(&relay)?,
        HttpRelayConfig {
            base_url: relay.clone(),
            api_version: RelayApiVersion::V1,
            vault_id,
            device_id: enrolled.device_id,
            manager_pub: Some(manager_pub),
        },
        enrolled.signing_key.clone(),
    );
    let vault = Vault::open(vault_dir.join("vault.sqlite3"))?;
    let report = bootstrap::run(
        &vault,
        &mut client,
        vault_id,
        enrolled.peer_id,
        &enrolled.root,
    )?;
    println!(
        "[bootstrap] manifest verified cid={} S={} fallback={} signed={}",
        hex16(&report.checkpoint_id),
        report.covered_seq,
        report.used_fallback,
        report.signed_checkpoint
    );
    println!("[bootstrap] docs imported count={}", report.docs.len());
    for (doc_id, text) in &report.docs {
        println!(
            "[bootstrap] doc {doc_id} text={}",
            text.as_deref()
                .map_or("<structure>".to_owned(), |value| format!("{value:?}"))
        );
    }
    println!("[bootstrap] assets imported count={}", report.assets);
    drop(client);

    // 3. 关掉 relay（不再发请求）后本地重读：正文/结构/资产。
    offline_read(&vault, enrolled.peer_id)?;
    Ok(())
}

fn offline_read(vault: &Vault, peer_id: u64) -> AnyResult<()> {
    let docs = list_all_docs(vault)?;
    println!("[bootstrap] offline read docs={}", docs.len());
    for doc in &docs {
        if doc.doc_id == "structure" {
            let (structure, _) = load_structure(vault, peer_id)?;
            println!(
                "[bootstrap] offline structure snapshot_bytes={}",
                structure.export_snapshot()?.len()
            );
        } else {
            let (body, _) = load_body_doc(vault, &doc.doc_id, peer_id, None)?;
            println!(
                "[bootstrap] offline doc {} text={:?}",
                doc.doc_id,
                body.text()
            );
        }
    }
    let assets = list_assets(vault)?;
    for asset in &assets {
        let Some(bytes) = asset_bytes(vault, asset.asset_hash)? else {
            return Err("offline asset 缺 data".into());
        };
        println!(
            "[bootstrap] offline asset hash={} len={}",
            hex16(&asset.asset_hash),
            bytes.len()
        );
    }
    println!("[bootstrap] offline read OK");
    Ok(())
}

fn management_pub(code: &str, vault_id: [u8; 16]) -> AnyResult<[u8; 32]> {
    let seed = menghunote_sync::recovery::RecoverySeed::from_code(code)?;
    let manager = menghunote_sync::device::ManagementKey::from_recovery(&seed, &vault_id)?;
    Ok(manager.public_key())
}

fn parse_hex16(text: &str) -> AnyResult<[u8; 16]> {
    if text.len() != 32 {
        return Err("vault_id hex 长度不是 32".into());
    }
    let mut out = [0u8; 16];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        out[index] = u8::from_str_radix(std::str::from_utf8(chunk)?, 16)?;
    }
    Ok(out)
}

fn hex16(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[allow(dead_code)]
fn _device_id(_: DeviceId) {}
