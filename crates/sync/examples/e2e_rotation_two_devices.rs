//! 双设备轮换 e2e：A genesis + B 恢复码 bootstrap + 冻结/候选/
//! activate + B 离线编辑/未 ACK outbox + B apply/repack + 旧秘密负例。
//!
//! 默认**自包含**：进程内起真实 axum relay（`menghunote-dev-relay`：临时 SQLite + FakeClock +
//! loopback 放行验签 hook + `127.0.0.1:0`），不需要任何外部进程或脚本。命令即验收：
//!
//! ```text
//! cargo run -p menghunote-sync --example e2e_rotation_two_devices
//! # 退出码 0，最后一行 TWO_DEVICE_EXIT=0
//! ```
//!
//! 兼容旧调用：显式给 `--relay <url> --setup-secret <secret>`（或同名环境变量）时仍走
//! **外部** relay，`--dir` 参数形态不变。单进程内用两个独立目录/设备身份完成 A/B；
//! B 全程不跑 sync 循环。A 侧一轮的真实管理路径（begin/两份基线/grants/T/activate/
//! 本地推广/重包）在 [`e2e_rotation_rounds`] 里共享，多轮用例
//! `e2e_rotation_two_rounds` 复用同一实现。

#[path = "e2e_rotation_rounds.rs"]
mod e2e_rotation_rounds;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use e2e_rotation_rounds::e2e_common::{self as common, AnyResult};
use e2e_rotation_rounds::{apply_round_for_device, run_a_rotation_round, ARoundParams};
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{
    outbox_parts, outbox_row, sync_cursor_state, Vault, META_SYNC_RELAY_CURSOR,
};
use menghunote_sync::genesis;
use menghunote_sync::relay::RelayClient;
use menghunote_sync::rotation_repack::{repack_outbox_after_rotation, REPACK_REPORT_META};
use menghunote_sync::slot::NONCE_LEN;
use menghunote_sync::upload::{decrypt_envelope, fetch_object};
use menghunote_sync::{ids, SyncError};
use menghunote_sync_proto::{Envelope, ObjectType, SlotDescriptorV1};

const OLD_PASSWORD: &str = "two-device-old-password";
const NEW_PASSWORD: &str = "two-device-new-password";
const NEW_MANAGER_REV: u64 = genesis::GENESIS_MANAGER_REV + 1;
const LOCAL_ONLY_DOC: &str = "note-b-local-only";

struct Args {
    relay: Option<String>,
    setup_secret: Option<String>,
    dir: PathBuf,
    dir_explicit: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("[two-device] OK");
            println!("TWO_DEVICE_EXIT=0");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("[two-device] FAILED: {error}");
            println!("TWO_DEVICE_EXIT=1");
            ExitCode::FAILURE
        }
    }
}

fn default_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("tmp")
        .join("e2e-rotation-two-devices")
}

fn parse_args() -> AnyResult<Args> {
    let mut relay = env::var("RELAY_DEV_URL").ok();
    let mut setup_secret = env::var("RELAY_SETUP_SECRET").ok();
    let mut dir = default_dir();
    let mut dir_explicit = false;
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--relay" => relay = Some(args.next().ok_or("--relay 需要参数")?),
            "--setup-secret" | "--setup" => {
                setup_secret = Some(args.next().ok_or("--setup/--setup-secret 需要参数")?)
            }
            "--dir" | "--vault" => {
                dir = PathBuf::from(args.next().ok_or("--dir/--vault 需要参数")?);
                dir_explicit = true;
            }
            "-h" | "--help" => {
                println!(
                    "e2e_rotation_two_devices [--relay <http://host:port> --setup-secret <secret>] [--dir <dir>]\n\
                     \n\
                     缺省：进程内起真实 relay（临时 SQLite + FakeClock + 127.0.0.1:0）。\n\
                     --relay/--setup-secret：外部 relay 兼容模式。"
                );
                std::process::exit(0);
            }
            other => return Err(format!("未知参数：{other}").into()),
        }
    }
    Ok(Args {
        relay,
        setup_secret,
        dir,
        dir_explicit,
    })
}

fn run() -> AnyResult<()> {
    let args = parse_args()?;

    // ---- relay：缺省进程内（真实 axum + 临时 SQLite + FakeClock）----
    let mut dev_relay: Option<DevRelay> = None;
    let (relay_url, setup_secret) = match (args.relay.clone(), args.setup_secret.clone()) {
        (Some(relay), Some(secret)) if !secret.is_empty() => (relay, secret),
        (None, None) => {
            let harness = DevRelay::start()?;
            common::set_now_override(Some(harness.now() as u64));
            let urls = (
                harness.base_url().to_owned(),
                harness.setup_secret().to_owned(),
            );
            dev_relay = Some(harness);
            urls
        }
        _ => return Err("--relay 与 --setup-secret 必须成对给出（否则用缺省进程内模式）".into()),
    };
    let in_process = dev_relay.is_some();

    // 默认目录是"每次运行重置"的临时工作区；显式 --dir 时不动它。
    //
    // 显式目录必须**完全为空**（或不存在）：真实身份/恢复码/槽在
    // `<dir>/device-a|device-b/identity.bin`，只查 `<dir>/identity.bin` 会漏过并让
    // 下一次 genesis 无条件覆写旧身份，随后再打开旧库 → 旧密钥已被抹掉才失败。
    // 因此这里在创建目录、写任何文件之前先拒绝非空目录。
    let work = args.dir.clone();
    if in_process && !args.dir_explicit && work == default_dir() && work.exists() {
        fs::remove_dir_all(&work)?;
        println!("[two-device] reset work dir {}", work.display());
    }
    if args.dir_explicit && work.exists() {
        let mut entries = fs::read_dir(&work)?;
        if entries.next().is_some() {
            return Err(format!(
                "显式 --dir {} 不是空目录；为避免覆盖 device-a/device-b 里的旧身份与库，请清空或换目录",
                work.display()
            )
            .into());
        }
    }
    fs::create_dir_all(&work)?;
    let a_dir = work.join("device-a");
    let b_dir = work.join("device-b");
    println!(
        "[two-device] mode={} relay={} work={}",
        if in_process { "in-process" } else { "external" },
        relay_url,
        work.display()
    );

    // 1. A：genesis + 内容 + 初始槽 + 两份已发布 checkpoint（B bootstrap 的前置）。
    let a = common::run_genesis(&relay_url, &setup_secret, &a_dir, OLD_PASSWORD)?;
    let a_vault = common::seed_a_vault(&relay_url, &a, OLD_PASSWORD)?;

    // 2. B：只用恢复码 bootstrap 到独立目录，得到独立 device 身份与本地库（E1）。
    let b = common::bootstrap_from_recovery(&relay_url, &a_dir, &b_dir, &a.recovery_code)?;
    if !b.root.ct_eq(&a.root) {
        return Err("B bootstrap 解出的根与 A 不一致".into());
    }
    println!(
        "[two-device] B recovery root matches A epoch={}",
        b.root.key_epoch
    );
    let b_vault = Vault::open(b_dir.join("vault.sqlite3"))?;

    // 3. B 离线：不跑 sync 循环，直接改本地 Loro + 建 local-only doc + 造未 ACK outbox。
    let b_before = common::body_text(&b_vault, "note-1", b.peer_id)?;
    let b_note_seq =
        common::append_offline_edit(&b_vault, "note-1", b.peer_id, "\nB offline edit")?;
    let b_local_seq = common::create_local_only_doc(
        &b_vault,
        LOCAL_ONLY_DOC,
        b.peer_id,
        "B local only content\n",
    )?;
    let (old_message, _old_digest, _) = common::inject_unacked_outbox(
        &b_vault,
        &b.root,
        b.device_id,
        "note-1",
        b_note_seq,
        b"offline-update-before-rotation",
    )?;
    let b_after = common::body_text(&b_vault, "note-1", b.peer_id)?;
    if common::outbox_count(&b_vault)? != 1 {
        return Err("B 离线 outbox 不是 1 行".into());
    }
    println!(
        "[two-device] B offline note seq={} local_only seq={} outbox={} text_before={:?} text_after={:?}",
        b_note_seq, b_local_seq, old_message, b_before, b_after
    );

    // 4. A 侧一整轮：冻结 begin → 两份独立基线 → 候选槽 → grants → T/双签 activate
    //    → 读回 T/grant → 本地原子推广 → journal 收尾 → 本机 outbox 重包。
    let identity_path = a_dir.join("identity.bin");
    let mut a_client = common::make_client(
        &relay_url,
        a.vault_id,
        a.device_id,
        a.signing_key.clone(),
        Some(a.manager.public_key()),
    )?;
    let round = run_a_rotation_round(
        &a_vault,
        &mut a_client,
        &ARoundParams {
            relay: &relay_url,
            vault_id: a.vault_id,
            device_id: a.device_id,
            peer_id: a.peer_id,
            signing_key: &a.signing_key,
            hpke_key: &a.hpke_key,
            identity_path: &identity_path,
            identity: &a.identity,
            old_root: &a.root,
            old_manager: &a.manager,
            old_manager_rev: genesis::GENESIS_MANAGER_REV,
            genesis_manager_public: a.manager.public_key(),
            new_password: NEW_PASSWORD,
            new_manager_rev: NEW_MANAGER_REV,
        },
    )?;

    // 5. B 侧：验证自己的 grant → 网络 apply（merge 基线 + 原子推广身份）。
    let mut b_client = common::make_client(
        &relay_url,
        b.vault_id,
        b.device_id,
        b.signing_key.clone(),
        Some(a.manager.public_key()),
    )?;
    let catch = apply_round_for_device(
        &mut b_client,
        &b_vault,
        &b_dir,
        &b.identity_path,
        &b.identity,
        &b.hpke_key,
        &round,
    )?;
    println!(
        "[two-device] B verified relay activation commit hash={}",
        ids::hex_of(&round.activation_commit_hash)
    );
    // 对照 A 侧已发出的 T/commit：apply 入口内部也做了同样绑定，这里只打印可判定事实。
    if round.activation_commit.control_rev != round.control_rev {
        return Err("activation commit 的 control_rev 与 activate 回执不一致".into());
    }
    println!(
        "[two-device] B apply done baseline_s={} promoted=true",
        catch.baseline_s
    );

    // 6. B 的离线编辑/local-only doc 必须保留；cursor 必须等于所选基线 S。
    let b_note = common::body_text(&b_vault, "note-1", b.peer_id)?;
    if !b_note.contains("genesis note 0 from A") || !b_note.contains("B offline edit") {
        return Err(format!("B note-1 合并后丢了内容：{b_note:?}").into());
    }
    let local_text = common::body_text(&b_vault, LOCAL_ONLY_DOC, b.peer_id)?;
    if local_text != "B local only content\n" {
        return Err(format!("B local-only doc 内容变了：{local_text:?}").into());
    }
    println!("[two-device] B offline edit preserved text={b_note:?}");
    println!("[two-device] B local-only doc preserved text={local_text:?}");

    let cursor = sync_cursor_state(&b_vault)?;
    if cursor.integrated_seq != catch.baseline_s {
        return Err(format!(
            "B integrated_seq={} 与所选基线 S={} 不一致",
            cursor.integrated_seq, catch.baseline_s
        )
        .into());
    }
    let relay_cursor = b_vault.get_meta(META_SYNC_RELAY_CURSOR)?;
    if relay_cursor.as_deref() != Some(catch.baseline_cursor.as_bytes()) {
        return Err(format!(
            "B relay cursor={:?} 与基线 cursor={} 不一致",
            relay_cursor.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
            catch.baseline_cursor
        )
        .into());
    }
    println!(
        "[two-device] B cursor integrated={} == baseline S={} relay_cursor={}",
        cursor.integrated_seq, catch.baseline_s, catch.baseline_cursor
    );

    // 7. outbox 重包：旧行消失、新行只一次、report 记录 repacked=1。
    let repack = repack_outbox_after_rotation(&b_vault, &b.root, &round.new_root, b.device_id)?;
    if repack.repacked != 1 || repack.failed != 0 || repack.conflicts != 0 {
        return Err(format!(
            "B repack 期望 1/0/0，实际 {}/{}/{}",
            repack.repacked, repack.failed, repack.conflicts
        )
        .into());
    }
    let new_message = repack
        .new_message_ids
        .first()
        .ok_or("repack 没有返回新 message_id")?;
    if outbox_row(&b_vault, &old_message)?.is_some() {
        return Err("repack 后旧 outbox 行仍存在".into());
    }
    let new_row = outbox_row(&b_vault, new_message)?.ok_or("repack 后新 outbox 行不存在")?;
    if new_row.key_epoch != round.new_root.key_epoch {
        return Err(format!(
            "repack 后 outbox epoch={} 不是 {}",
            new_row.key_epoch, round.new_root.key_epoch
        )
        .into());
    }
    let new_parts = outbox_parts(&b_vault, new_message, round.new_root.key_epoch)?;
    if new_parts.is_empty() {
        return Err("repack 后 outbox 没有 parts".into());
    }
    let rows = common::outbox_count(&b_vault)?;
    if rows != 1 {
        return Err(format!("repack 后 outbox 行数={rows}，期望 1（不重复）").into());
    }
    let report_meta = b_vault
        .get_meta(REPACK_REPORT_META)?
        .ok_or("缺少 sync.rotation_repack_report")?;
    let report_text = String::from_utf8_lossy(&report_meta).into_owned();
    if !report_text.contains("repacked=1")
        || !report_text.contains("failed=0")
        || !report_text.contains("conflicts=0")
    {
        return Err(format!("repack report 不符合预期：{report_text}").into());
    }
    println!(
        "[two-device] B outbox repacked rows={} report={}",
        rows,
        report_text.trim()
    );
    println!(
        "[two-device] B outbox no duplicate old_message_missing=true new_message={}",
        new_message
    );

    // 8. 旧秘密负例：旧根解不开新 outbox part / 新基线快照；旧 password/旧 R 解不开新槽。
    let new_object = new_row.object_id.ok_or("新 outbox 行缺 object_id")?;
    if decrypt_envelope(
        &b.root,
        ObjectType::Update,
        &new_object,
        &new_parts[0].envelope,
        &[],
    )
    .is_ok()
    {
        return Err("旧根解开了新重包 outbox part".into());
    }
    println!("[two-device] negative: old epoch root cannot decrypt new outbox part");

    let snapshot_object = round.primary_snapshot.as_ref().ok_or("候选基线没有 docs")?;
    if fetch_object(
        &mut a_client,
        &a.root,
        ObjectType::Snapshot,
        &snapshot_object.object_id,
        snapshot_object.part_count,
        snapshot_object.total_plain_len,
        &snapshot_object.object_digest,
        &[],
    )
    .is_ok()
    {
        return Err("旧根解开了新基线快照".into());
    }
    println!("[two-device] negative: old epoch root cannot decrypt new baseline snapshot");

    let password_header =
        Envelope::decode(&b_client.part(&round.password_slot.descriptor.slot_id, 0)?)?.header;
    let password_descriptor = round.password_slot.descriptor.encode()?;
    let password_aad = |_: &SlotDescriptorV1, _: &[u8; NONCE_LEN], _: usize| {
        password_header
            .aad(&password_descriptor)
            .map_err(SyncError::Proto)
    };
    if round
        .password_slot
        .unwrap_password(OLD_PASSWORD.as_bytes(), &password_aad)
        .is_ok()
    {
        return Err("旧 password 解开了新 password 槽".into());
    }
    println!("[two-device] negative: old password cannot unwrap new password slot");

    let recovery_header =
        Envelope::decode(&b_client.part(&round.recovery_slot.descriptor.slot_id, 0)?)?.header;
    let recovery_descriptor = round.recovery_slot.descriptor.encode()?;
    let recovery_aad = |_: &SlotDescriptorV1, _: &[u8; NONCE_LEN], _: usize| {
        recovery_header
            .aad(&recovery_descriptor)
            .map_err(SyncError::Proto)
    };
    if round
        .recovery_slot
        .unwrap_recovery(&a.recovery_seed, &recovery_aad)
        .is_ok()
    {
        return Err("旧恢复种子 R 解开了新 recovery 槽".into());
    }
    println!("[two-device] negative: old recovery seed R cannot unwrap new recovery slot");

    // 9. relay epoch 只切一次；最终 bootstrap epoch=2。
    if catch.bootstrap_epoch != round.to_epoch {
        return Err("activate 后 relay epoch 不符".into());
    }
    b_client.clear_auth_cache();
    let epoch_after_apply = b_client.state()?.epoch;
    if epoch_after_apply != round.to_epoch {
        return Err(format!("B apply 后 relay epoch={epoch_after_apply}，发生第二次切换").into());
    }
    let kinds = RelayClient::control_kinds(&mut b_client)?;
    let activations = kinds
        .iter()
        .filter(|(_, kind)| kind == "epoch_activated" || kind == "epoch.activated")
        .count();
    if activations != 1 {
        return Err(format!("控制链 epoch_activated 记录数={activations}，期望 1").into());
    }
    println!(
        "[two-device] relay epoch switched once: epoch={} epoch_activated={}",
        epoch_after_apply, activations
    );
    let final_bundle = b_client.bootstrap()?;
    if final_bundle.epoch != round.to_epoch {
        return Err(format!(
            "最终 bootstrap epoch={}，期望 {}",
            final_bundle.epoch, round.to_epoch
        )
        .into());
    }
    println!(
        "[two-device] final bootstrap epoch={} primary_cp_epoch={}",
        final_bundle.epoch, final_bundle.primary.epoch
    );
    Ok(())
}
