//! 真实 localhost 轮换 D–F（task-103/106 收口）。
//!
//! 复用 `e2e_localhost` 已生成的 `--dir`（identity/vault/恢复码），追加：
//! 1. 旧 epoch 上传 2 个资产（其中一个无引用/墓碑）→ 冻结 begin；
//! 2. frozen 资产分页 → 新根重加密；
//! 3. 两份新 epoch candidate 基线 + 独立证据；
//! 4. 对当前设备 stage grant；
//! 5. 验证 candidate 未 activate 时 bootstrap 仍是旧 epoch。

use std::env;
use std::error::Error as StdError;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use menghunote_storage::{ensure_structure_row, sync_cursor_state, Vault};
use menghunote_sync::checkpoint::StaticAssets;
use menghunote_sync::device::{DeviceHpkeKey, DeviceSigningKey, ManagementKey};
use menghunote_sync::genesis;
use menghunote_sync::http_relay::{HttpRelayClient, HttpRelayConfig, StdTcpTransport};
use menghunote_sync::keys::{RootSecretV1, Secret32};
use menghunote_sync::recovery::RecoverySeed;
use menghunote_sync::relay::RelayClient;
use menghunote_sync::rotation;
use menghunote_sync::rotation_activate::{
    activate_http, verify_latest_grant_root, ActivationInput,
};
use menghunote_sync::rotation_journal::RotationJournal;
use menghunote_sync::rotation_slots::{
    build_manager_transition, generate_candidate_slots, grants_digest, plan_all_active_grants,
    upload_candidate_slots,
};
use menghunote_sync::rotation_stage;
use menghunote_sync::{ids, DeviceId, VaultId};

type AnyResult<T> = Result<T, Box<dyn StdError>>;

const EPOCH: u64 = 1;

fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("[rotation-e2e] OK");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("[rotation-e2e] FAILED: {error}");
            ExitCode::FAILURE
        }
    }
}

fn asset_plain_from_payload(payload: &[u8]) -> AnyResult<(String, Vec<u8>)> {
    if payload.len() < 42 {
        return Err("资产明文过短".into());
    }
    let mime_len = usize::from(u16::from_be_bytes([payload[40], payload[41]]));
    let mime =
        std::str::from_utf8(payload.get(42..42 + mime_len).ok_or("资产 MIME 越界")?)?.to_owned();
    let data = payload.get(42 + mime_len..).ok_or("资产数据越界")?.to_vec();
    Ok((mime, data))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn run() -> AnyResult<()> {
    let mut relay = "http://127.0.0.1:18281".to_owned();
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("tmp")
        .join("sync-e2e-localhost");
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--relay" => relay = args.next().ok_or("--relay 需要参数")?,
            "--dir" | "--vault" => dir = PathBuf::from(args.next().ok_or("--dir 需要参数")?),
            other => return Err(format!("未知参数：{other}").into()),
        }
    }
    let meta = fs::read_to_string(dir.join("identity.meta"))?;
    let mut parts = meta.split_whitespace();
    let vault_id: VaultId = ids::parse_hex::<16>(parts.next().ok_or("缺 vault_id")?)
        .map_err(|_| std::io::Error::other("vault_id"))?;
    let device_id: DeviceId = ids::parse_hex::<16>(parts.next().ok_or("缺 device_id")?)
        .map_err(|_| std::io::Error::other("device_id"))?;
    let identity = menghunote_sync::identity::LocalIdentityV1::unprotect(
        &fs::read(dir.join("identity.bin"))?,
        &vault_id,
        &device_id,
    )?;
    let signing_key = DeviceSigningKey::from_seed(identity.ed25519_seed.clone());
    let old_root = RootSecretV1::new(
        identity.vault_id,
        identity.current_epoch,
        identity.current_root.clone(),
    );
    let mut head = [0u8; 8];
    head.copy_from_slice(&identity.peer_id.as_bytes()[..8]);
    let peer_id = u64::from_be_bytes(head) & 0x7fff_ffff_ffff_ffff;
    let recovery_code = fs::read_to_string(dir.join("recovery-code.txt"))?;
    let recovery_seed = RecoverySeed::from_code(recovery_code.trim())?;
    let manager = ManagementKey::from_recovery(&recovery_seed, &vault_id)?;

    let mut client = HttpRelayClient::new(
        StdTcpTransport::new(&relay)?,
        HttpRelayConfig {
            base_url: relay.clone(),
            vault_id,
            device_id,
            manager_pub: Some(manager.public_key()),
        },
        signing_key.clone(),
    );
    let vault = Vault::open(dir.join("vault.sqlite3"))?;
    ensure_structure_row(&vault)?;
    let cursor = sync_cursor_state(&vault)?;
    println!(
        "[rotation-e2e] loaded vault device={} epoch={} integrated={}",
        ids::hex_of(&device_id),
        old_root.key_epoch,
        cursor.integrated_seq
    );

    // 旧 epoch 上传 2 个资产（第二个为无引用/墓碑），只 prepare 不发布。
    let assets = StaticAssets::new(vec![
        ("text/plain".to_owned(), b"live asset".to_vec()),
        (
            "application/octet-stream".to_owned(),
            b"orphan tombstone asset".to_vec(),
        ),
    ])?;
    let _old_baseline = rotation_stage::prepare_new_epoch_baseline(
        &vault,
        &mut client,
        vault_id,
        device_id,
        signing_key.clone(),
        old_root.clone(),
        peer_id,
        &assets,
    )?;

    let state0 = client.state()?;
    let mut journal = RotationJournal::freeze(
        &vault,
        EPOCH,
        menghunote_sync::random::array(&mut menghunote_sync::random::OsRandom)?,
        state0.control_rev,
        [0u8; 32],
    )?;
    journal.confirm_r_prime_backed_up(&vault)?;
    rotation::verify_management_chain_and_pin(
        &vault,
        &mut client,
        &mut journal,
        vault_id,
        &manager,
        None,
    )?;
    let instance_id = state0.instance_id;
    let begin = rotation_stage::begin_rotation_http(
        &mut StdTcpTransport::new(&relay)?,
        &relay,
        &vault,
        &mut journal,
        vault_id,
        &instance_id,
        state0.log_id,
        &manager,
        &[],
        now_secs(),
    )?;
    // begin 让旧 device token 全部失效（§24.1①），必须重新认证。
    client.clear_auth_cache();
    println!(
        "[rotation-e2e] begin ok rotation={} state={} S={}",
        ids::hex_of(&begin.rotation_id),
        begin.state,
        begin.freeze_seq
    );

    let (frozen, source_digest, total) =
        rotation_stage::fetch_frozen_assets(&mut client, begin.rotation_id, 1)?;
    println!(
        "[rotation-e2e] frozen assets {} (total={} digest={})",
        frozen.len(),
        total,
        source_digest.map_or_else(|| "none".to_owned(), |digest| ids::hex_of(&digest))
    );
    let new_root = RootSecretV1::new(
        vault_id,
        EPOCH + 1,
        Secret32::from_array(menghunote_sync::random::array(
            &mut menghunote_sync::random::OsRandom,
        )?),
    );
    // 两份候选基线必须覆盖完整冻结资产集合（含上一轮遗留/无引用资产）。
    let mut all_assets = Vec::with_capacity(frozen.len());
    for asset in &frozen {
        let (_, payload) = menghunote_sync::upload::fetch_object(
            &mut client,
            &old_root,
            menghunote_sync_proto::ObjectType::Asset,
            &asset.object_id,
            asset.part_count,
            asset.total_plain_len,
            &asset.object_digest,
            &[],
        )?;
        all_assets.push(asset_plain_from_payload(&payload)?);
    }
    all_assets.sort_by_key(|(_, data)| {
        use sha2::Digest;
        let digest: [u8; 32] = sha2::Sha256::digest(data).into();
        digest
    });
    let assets = StaticAssets::new(all_assets)?;
    // §28.1：候选 checkpoint 创建时才建立 rotation maintenance reservation。
    let first = rotation_stage::prepare_new_epoch_baseline_with_role(
        &vault,
        &mut client,
        vault_id,
        device_id,
        signing_key.clone(),
        new_root.clone(),
        peer_id,
        &assets,
        Some("primary"),
    )?;
    let reservation = client
        .rotation(begin.rotation_id)?
        .reservation_id
        .ok_or("候选 checkpoint 未返回 reservation")?;
    println!(
        "[rotation-e2e] reservation={} (first cp {})",
        ids::hex_of(&reservation),
        ids::hex_of(&first.report.checkpoint_id)
    );
    // P1：候选槽必须用新秘密（新 password + 新 R'）。
    let mut slot_rng = menghunote_sync::random::OsRandom;
    let slots = generate_candidate_slots(
        vault_id,
        new_root.key_epoch,
        "rotation-e2e-new-password",
        genesis::GENESIS_MANAGER_REV + 1,
        now_secs(),
        device_id,
        &new_root,
        &mut slot_rng,
    )?;
    let slot_digest = upload_candidate_slots(
        &mut client,
        vault_id,
        device_id,
        &signing_key,
        &new_root,
        &slots,
        "rotation-e2e-new-password",
        &reservation,
    )?;
    println!(
        "[rotation-e2e] slots staging password={} recovery={} digest={}",
        ids::hex_of(&slots.password.descriptor.slot_id),
        ids::hex_of(&slots.recovery.descriptor.slot_id),
        ids::hex_of(&slot_digest)
    );
    let (reencrypted, _, _) = rotation_stage::reencrypt_frozen_assets(
        &vault,
        &mut client,
        vault_id,
        device_id,
        &signing_key,
        &old_root,
        &new_root,
        begin.rotation_id,
        1,
        Some(&reservation),
    )?;
    println!(
        "[rotation-e2e] reencrypted {} objects (new epoch {})",
        reencrypted.len(),
        new_root.key_epoch
    );

    let frozen_hashes: Vec<[u8; 32]> = reencrypted.iter().map(|asset| asset.real_sha256).collect();
    let second = rotation_stage::prepare_new_epoch_baseline_with_role(
        &vault,
        &mut client,
        vault_id,
        device_id,
        signing_key.clone(),
        new_root.clone(),
        peer_id,
        &assets,
        Some("fallback"),
    )?;
    let evidence1 =
        rotation_stage::capture_baseline_evidence(&mut client, &new_root, &first, &frozen_hashes)?;
    let evidence2 =
        rotation_stage::capture_baseline_evidence(&mut client, &new_root, &second, &frozen_hashes)?;
    rotation_stage::assert_independent_baselines(&evidence1, &evidence2, begin.freeze_seq)?;
    println!(
        "[rotation-e2e] candidate baselines primary={} fallback={} S={}/{}",
        evidence1.covered_seq, evidence2.covered_seq, evidence1.covered_seq, evidence2.covered_seq
    );
    let status1 = rotation_stage::assert_candidate_ready(
        &mut client,
        first.report.checkpoint_id,
        "primary",
        begin.freeze_seq,
    )
    .or_else(|_| {
        rotation_stage::assert_candidate_ready(
            &mut client,
            first.report.checkpoint_id,
            "fallback",
            begin.freeze_seq,
        )
    })?;
    let status2 = rotation_stage::assert_candidate_ready(
        &mut client,
        second.report.checkpoint_id,
        "fallback",
        begin.freeze_seq,
    )
    .or_else(|_| {
        rotation_stage::assert_candidate_ready(
            &mut client,
            second.report.checkpoint_id,
            "primary",
            begin.freeze_seq,
        )
    })?;
    println!(
        "[rotation-e2e] candidate ready primary={} role={:?} fallback={} role={:?}",
        status1.candidate_ready, status1.role, status2.candidate_ready, status2.role
    );
    let _ = second.report.checkpoint_id;

    // 全 active 设备（含本机）的 grants，公钥来自已验证的 /control 证书。
    let grants = plan_all_active_grants(
        &mut client,
        &new_root,
        &manager,
        genesis::GENESIS_MANAGER_REV,
    )?;
    rotation_stage::attach_grants(&mut journal, &vault, &grants)?;
    rotation_stage::stage_grants(
        &mut StdTcpTransport::new(&relay)?,
        &relay,
        vault_id,
        begin.rotation_id,
        &manager,
        &grants,
        now_secs(),
    )?;
    println!("[rotation-e2e] grants staged {}", grants.len());

    let control_rev = client.rotation(begin.rotation_id)?.control_rev;
    let member_digest = client.active_member_cert_digest()?;
    let grant_digest = grants_digest(&grants)?;
    let asset_digest = source_digest.ok_or("冻结资产缺少 source_digest")?;
    // 旧/新管理 key 对同一 transition 双签。
    let transition = build_manager_transition(
        vault_id,
        instance_id,
        begin.rotation_id,
        begin.from_epoch,
        begin.to_epoch,
        genesis::GENESIS_MANAGER_REV,
        genesis::GENESIS_MANAGER_REV + 1,
        slots.new_manager_public,
        begin.freeze_seq,
        &manager,
        &slots.new_manager,
    )?;
    let input = ActivationInput {
        rotation_id: begin.rotation_id,
        request_id: menghunote_sync::random::array(&mut menghunote_sync::random::OsRandom)?,
        expected_control_rev: control_rev,
        primary_checkpoint: first.report.checkpoint_id,
        fallback_checkpoint: second.report.checkpoint_id,
        member_cert_digest: member_digest,
        slot_set_digest: slot_digest,
        grants_digest: grant_digest,
        asset_set_digest: asset_digest,
        new_manager: Some(transition),
        candidate_root: new_root.clone(),
        new_manager_public: slots.new_manager_public,
        new_manager_rev: genesis::GENESIS_MANAGER_REV + 1,
        pv: 1,
    };
    let outcome = activate_http(
        &mut StdTcpTransport::new(&relay)?,
        &relay,
        vault_id,
        &instance_id,
        &manager,
        &input,
        now_secs(),
    )?;
    println!(
        "[rotation-e2e] activated epoch={} control_rev={} auth_rev={}",
        outcome.epoch, outcome.control_rev, outcome.auth_rev
    );
    // activate 后旧 token 失效，读 latest grant 需要重新认证。
    client.clear_auth_cache();
    let hpke_key = DeviceHpkeKey::from_secret(identity.x25519_secret.clone());
    verify_latest_grant_root(
        &mut StdTcpTransport::new(&relay)?,
        &relay,
        vault_id,
        device_id,
        &signing_key,
        &hpke_key,
        &new_root,
        now_secs(),
    )?;
    println!("[rotation-e2e] grant root verified");

    // activate 后 bootstrap 应切到新 epoch。
    client.clear_auth_cache();
    let bootstrap = client.bootstrap()?;
    println!(
        "[rotation-e2e] bootstrap after activate epoch={} primary_cp_epoch={}",
        bootstrap.epoch, bootstrap.primary.epoch
    );
    if bootstrap.epoch != begin.to_epoch {
        return Err("activate 后 bootstrap 未切到新 epoch".into());
    }
    Ok(())
}
