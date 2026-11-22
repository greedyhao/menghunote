//! 同一 durable E1 旧设备跨两轮轮换的 e2e（评审 0003 T13）。
//!
//! 与旧版（轮 1 就把 B 推广到 E2、轮 2 用 E2 身份）的关键区别：**B 从 bootstrap 之后
//! 一直到 apply 之前都不在场**——不构造 client、不调 relay，只保留 E1 的 durable
//! 身份/授权材料与本机离线编辑。A 连续做两轮：
//!
//! ```text
//! 轮 1：E1 -> E2（普通轮）
//!   激活后 A 再发布 2 份**普通** checkpoint，把 E2 的激活 pair 顶出 /bootstrap；
//!
//! 轮 2：E2 -> E3（begin 拿到 E3）--restart--> E3 被原子废弃、relay 分配 E4
//!   A 在 E4 上跑标准 stage 并激活 2 -> 4（E3 永远没有 epoch.activated）；
//!   同样再发布 2 份普通 checkpoint；
//!
//! 时间：relay 用 FakeClock，+90d 后连跑 3 次 `run_gc`（每次之间再 +7d+1h）
//!   → E1/E2 的 checkpoint（含激活 pair）与 E4 的激活 pair 被 prune（refs 清空、
//!     行与 signed_commit/commit_signature 签名骨架保留），E4 普通 pair 保留；
//!
//! B：drop vault → 重新 `Vault::open` + `load_identity`（restart），仍 epoch=1；
//!   用**同一份** E1 身份走真实 HTTP/HPKE/Loro apply：
//!   - `/control` 的 activation 链从 E1 起逐环复算（1->2、2->4，允许跳号）；
//!   - latest grant 必须由 B 不变的 X25519 身份解回 E4 候选根；
//!   - 选中基线是 E4 的**普通** checkpoint（不在 activation T 的 pair 内），
//!     走 checkpoint 级认证（task-119 的弱路径）；
//!   - 离线编辑 / local-only 保留；outbox 用旧根（E1）重包到 E4 且不重复；
//!   - 水位单调；旧根/旧口令/旧恢复码 R 都解不开 E4 新对象。
//! ```
//!
//! 默认**自包含**：进程内起真实 axum relay（临时 SQLite + FakeClock + loopback 放行验签
//! hook + `127.0.0.1:0`），不需要任何外部进程或脚本。命令即验收：
//!
//! ```text
//! cargo run -p menghunote-sync --example e2e_rotation_two_rounds
//! # 退出码 0，最后一行 TWO_ROUND_EXIT=0
//! ```
//!
//! 兼容旧调用：显式给 `--relay <url> --setup <secret>` 时仍走**外部** relay（旧 ps1 的
//! 参数形态不变），但外部 relay 没有可控假时钟/GC 入口，因此该模式会跳过 90d/GC 段并以
//! `TWO_ROUND_EXIT=2` 退出（SKIP，不是通过）——T13 的完整验收必须用缺省的进程内模式。

#[path = "e2e_rotation_rounds.rs"]
mod e2e_rotation_rounds;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use e2e_rotation_rounds::e2e_common::{self as common, AnyResult};
use e2e_rotation_rounds::{
    apply_round_for_device, audit_epoch_chain, run_a_restart_round, run_a_rotation_round,
    ARoundParams,
};
use menghunote_dev_relay::DevRelay;
use menghunote_relay::checkpoints::CheckpointState;
use menghunote_storage::{
    outbox_parts, outbox_row, repack_required, set_repack_required, sync_cursor_state,
    SyncCursorState, Vault, META_SYNC_RELAY_CURSOR,
};
use menghunote_sync::device::{DeviceHpkeKey, DeviceSigningKey};
use menghunote_sync::genesis;
use menghunote_sync::identity::IdentityRotationStateV1;
use menghunote_sync::relay::RelayClient;
use menghunote_sync::rotation_repack::{repack_outbox_after_rotation, REPACK_REPORT_META};
use menghunote_sync::slot::NONCE_LEN;
use menghunote_sync::upload::{decrypt_envelope, fetch_object};
use menghunote_sync::{ids, SyncError};
use menghunote_sync_proto::{Envelope, ObjectType, SlotDescriptorV1};

const PASSWORD_E1: &str = "two-round-password-e1";
const PASSWORD_E2: &str = "two-round-password-e2";
const PASSWORD_E4: &str = "two-round-password-e4";
const MANAGER_REV_E2: u64 = genesis::GENESIS_MANAGER_REV + 1;
const MANAGER_REV_E4: u64 = genesis::GENESIS_MANAGER_REV + 2;
const LOCAL_ONLY_E1: &str = "note-b-local-only-e1";
const LOCAL_ONLY_E1B: &str = "note-b-local-only-e1b";
const OFFLINE_EDIT_R1: &str = "\nB offline edit round 1";
const OFFLINE_EDIT_R2: &str = "\nB offline edit round 2";
const DAY_SECS: i64 = 24 * 60 * 60;

struct Args {
    relay: Option<String>,
    setup_secret: Option<String>,
    dir: PathBuf,
    dir_explicit: bool,
}

/// 验收结论：完整跑通（in-process）或外部 relay 的降级运行（SKIP，非通过）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Acceptance {
    InProcess,
    ExternalDegraded,
}

fn main() -> ExitCode {
    match run() {
        Ok(Acceptance::InProcess) => {
            println!("[two-round] OK");
            println!("TWO_ROUND_EXIT=0");
            ExitCode::SUCCESS
        }
        Ok(Acceptance::ExternalDegraded) => {
            println!(
                "[two-round] SKIP: 外部 relay 模式没有可控假时钟/GC 入口，90d+GC 段未执行；T13 验收请用缺省进程内模式"
            );
            println!("TWO_ROUND_EXIT=2");
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("[two-round] FAILED: {error}");
            println!("TWO_ROUND_EXIT=1");
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
        .join("e2e-rotation-two-rounds")
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
                    "e2e_rotation_two_rounds [--relay <http://host:port> --setup <secret>] [--dir <dir>]\n\
                     \n\
                     缺省：进程内起真实 relay（临时 SQLite + FakeClock + 127.0.0.1:0），完整 T13 验收。\n\
                     --relay/--setup：外部 relay 兼容模式（无假时钟/GC，退出码 2 = SKIP）。"
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

fn run() -> AnyResult<Acceptance> {
    let args = parse_args()?;

    // ---- relay：缺省进程内（真实 axum + 临时 SQLite + FakeClock）----
    let mut dev_relay: Option<DevRelay> = None;
    let (relay_url, setup_secret) = match (args.relay.clone(), args.setup_secret.clone()) {
        (Some(relay), Some(secret)) => (relay, secret),
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
        println!("[two-round] reset work dir {}", work.display());
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
        "[two-round] mode={} relay={} work={}",
        if in_process { "in-process" } else { "external" },
        relay_url,
        work.display()
    );

    // ---- P0：A genesis + seed；B 用恢复码 bootstrap（durable E1 身份/授权）----
    let a = common::run_genesis(&relay_url, &setup_secret, &a_dir, PASSWORD_E1)?;
    let a_vault = common::seed_a_vault(&relay_url, &a, PASSWORD_E1)?;
    let b = common::bootstrap_from_recovery(&relay_url, &a_dir, &b_dir, &a.recovery_code)?;
    if !b.root.ct_eq(&a.root) {
        return Err("B bootstrap 解出的根与 A 不一致".into());
    }
    if b.identity.current_epoch != genesis::GENESIS_EPOCH {
        return Err(format!(
            "B bootstrap 后身份 epoch={}，期望 genesis E{}",
            b.identity.current_epoch,
            genesis::GENESIS_EPOCH
        )
        .into());
    }
    if b.identity.pinned_manager != a.manager.public_key() {
        return Err("B bootstrap 的 pinned_manager 不是 genesis 管理公钥".into());
    }
    let mut b_vault = Vault::open(b_dir.join("vault.sqlite3"))?;
    println!(
        "[two-round] bootstrap: A epoch={} B epoch={} B bootstrap_S={} B durable E1 identity: epoch=1",
        a.root.key_epoch, b.root.key_epoch, b.report.covered_seq
    );

    // ---- P0.4：B 离线编辑 1（仍用 E1 根加密，不触网）----
    let b_seq1 = common::append_offline_edit(&b_vault, "note-1", b.peer_id, OFFLINE_EDIT_R1)?;
    let b_local1 = common::create_local_only_doc(
        &b_vault,
        LOCAL_ONLY_E1,
        b.peer_id,
        "B local only content E1\n",
    )?;
    let (b_outbox_1, _digest_1, _) = common::inject_unacked_outbox(
        &b_vault,
        &b.root,
        b.device_id,
        "note-1",
        b_seq1,
        b"offline-update-before-round-1",
    )?;
    if common::outbox_count(&b_vault)? != 1 {
        return Err("轮 1 前：B 离线 outbox 不是 1 行".into());
    }
    println!(
        "[two-round] round 1 offline: B note seq={} local_only seq={} outbox={}",
        b_seq1, b_local1, b_outbox_1
    );

    // ---- P0.5：离线快照（identity 字节 SHA256 / 水位 / inbox / outbox）----
    let b_identity_sha = common::file_sha256(&b.identity_path)?;
    let b_cursor0 = sync_cursor_state(&b_vault)?;
    let b_inbox0 = common::inbox_count(&b_vault)?;
    let b_outbox0 = common::outbox_count(&b_vault)?;
    if IdentityRotationStateV1::load_current(&b_vault, &b.identity)?.is_some() {
        return Err("B 在 apply 前就有轮换旁路状态（说明它参与过轮换）".into());
    }
    // B 没有 durable 的 grant 文件：grant 每次由 relay 的 T 链尾重签、按设备 HPKE 解回，
    // 所以"旧 grant"的可判定等价物 = **不变的 E1 设备身份 + genesis 管理 pin + E1 根**。
    println!(
        "[two-round] B offline snapshot: identity_sha256={} epoch={} grant_epoch={} cursor(received={} integrated={} highwater={:?} log={:?}) inbox={} outbox={}",
        ids::hex_of(&b_identity_sha),
        b.identity.current_epoch,
        b.root.key_epoch,
        b_cursor0.received_seq,
        b_cursor0.integrated_seq,
        b_cursor0.confirmed_highwater,
        b_cursor0.log_id.map(|id| ids::hex_of(&id)),
        b_inbox0,
        b_outbox0
    );

    let a_identity_path = a_dir.join("identity.bin");
    let mut a_client = common::make_client(
        &relay_url,
        a.vault_id,
        a.device_id,
        a.signing_key.clone(),
        Some(a.manager.public_key()),
    )?;

    // ---- 轮 1：E1 -> E2（普通轮；B 全程不构造 client）----
    let round1 = run_a_rotation_round(
        &a_vault,
        &mut a_client,
        &ARoundParams {
            relay: &relay_url,
            vault_id: a.vault_id,
            device_id: a.device_id,
            peer_id: a.peer_id,
            signing_key: &a.signing_key,
            hpke_key: &a.hpke_key,
            identity_path: &a_identity_path,
            identity: &a.identity,
            old_root: &a.root,
            old_manager: &a.manager,
            old_manager_rev: genesis::GENESIS_MANAGER_REV,
            genesis_manager_public: a.manager.public_key(),
            new_password: PASSWORD_E2,
            new_manager_rev: MANAGER_REV_E2,
        },
    )?;
    if round1.from_epoch != 1 || round1.to_epoch != 2 || round1.restarted {
        return Err(format!(
            "轮 1 事实不符：from={} to={} restarted={}",
            round1.from_epoch, round1.to_epoch, round1.restarted
        )
        .into());
    }

    // P1.2：激活 pair 被 2 份普通 checkpoint 顶出 /bootstrap（为 90d 后 prune 做铺垫）。
    let (n1_first, n1_second) = common::publish_ordinary_pair(
        &a_vault,
        &mut a_client,
        a.vault_id,
        a.device_id,
        a.signing_key.clone(),
        round1.new_root.clone(),
        a.peer_id,
    )?;
    let bundle_e2 = a_client.bootstrap()?;
    if bundle_e2.primary.checkpoint_id != n1_second.checkpoint_id
        || bundle_e2.fallback.checkpoint_id != n1_first.checkpoint_id
    {
        return Err(format!(
            "E2 普通 pair 未成为 /bootstrap 最新两份：primary={} fallback={}",
            ids::hex_of(&bundle_e2.primary.checkpoint_id),
            ids::hex_of(&bundle_e2.fallback.checkpoint_id)
        )
        .into());
    }
    if bundle_e2.primary.checkpoint_id == round1.primary_checkpoint
        || bundle_e2.fallback.checkpoint_id == round1.fallback_checkpoint
    {
        return Err("E2 /bootstrap 仍是激活 pair".into());
    }
    println!(
        "[two-round] round 1 (E1->E2) complete: epoch={} S={} identity epoch={}",
        round1.to_epoch, round1.primary_covered_seq, round1.promoted_identity.current_epoch
    );
    println!(
        "[two-round] E2 activation pair replaced by ordinary pair primary={} fallback={}",
        ids::hex_of(&n1_second.checkpoint_id),
        ids::hex_of(&n1_first.checkpoint_id)
    );

    // P1.3：B 完全离线（身份/水位/inbox/outbox 逐项不变）。
    assert_b_offline(
        &b_vault,
        &b.identity_path,
        &b_identity_sha,
        &b_cursor0,
        b_inbox0,
        b_outbox0,
        "round 1",
    )?;
    println!(
        "[two-round] B offline during round 1: identity/cursor/inbox/outbox unchanged (epoch=1)"
    );

    // ---- 轮 2：begin 分配 E3 → restart 废弃 E3 → E4 上完成 2->4 ----
    let a_identity_e2 = common::load_identity(&a_dir)?;
    if a_identity_e2.current_epoch != round1.to_epoch {
        return Err(format!(
            "轮 2 前：A 身份 epoch={}，期望 {}",
            a_identity_e2.current_epoch, round1.to_epoch
        )
        .into());
    }
    let round2 = run_a_restart_round(
        &a_vault,
        &mut a_client,
        &ARoundParams {
            relay: &relay_url,
            vault_id: a.vault_id,
            device_id: a.device_id,
            peer_id: a.peer_id,
            signing_key: &a.signing_key,
            hpke_key: &a.hpke_key,
            identity_path: &a_identity_path,
            identity: &a_identity_e2,
            old_root: &round1.new_root,
            old_manager: &round1.new_manager,
            old_manager_rev: round1.new_manager_rev,
            genesis_manager_public: a.manager.public_key(),
            new_password: PASSWORD_E4,
            new_manager_rev: MANAGER_REV_E4,
        },
    )?;
    if !round2.restarted || round2.previous_candidate_epoch != Some(3) {
        return Err(format!(
            "轮 2 没有走 restart 跳 epoch：restarted={} previous_candidate={:?}",
            round2.restarted, round2.previous_candidate_epoch
        )
        .into());
    }
    if round2.from_epoch != 2 || round2.to_epoch != 4 {
        return Err(format!(
            "轮 2 事实不符：{}->{}（期望 2->4）",
            round2.from_epoch, round2.to_epoch
        )
        .into());
    }
    println!("[two-round] round 2 restart: E3 aborted, relay allocated E4");

    // P2.2：E4 同样用 2 份普通 checkpoint 顶掉激活 pair。
    let (n2_first, n2_second) = common::publish_ordinary_pair(
        &a_vault,
        &mut a_client,
        a.vault_id,
        a.device_id,
        a.signing_key.clone(),
        round2.new_root.clone(),
        a.peer_id,
    )?;
    let bundle_e4 = a_client.bootstrap()?;
    if bundle_e4.primary.checkpoint_id != n2_second.checkpoint_id
        || bundle_e4.fallback.checkpoint_id != n2_first.checkpoint_id
    {
        return Err(format!(
            "E4 普通 pair 未成为 /bootstrap 最新两份：primary={} fallback={}",
            ids::hex_of(&bundle_e4.primary.checkpoint_id),
            ids::hex_of(&bundle_e4.fallback.checkpoint_id)
        )
        .into());
    }
    if bundle_e4.primary.checkpoint_id == round2.primary_checkpoint
        || bundle_e4.fallback.checkpoint_id == round2.fallback_checkpoint
    {
        return Err("E4 /bootstrap 仍是激活 pair".into());
    }
    println!(
        "[two-round] round 2 (E2->E4, restart) complete: S={} identity epoch={}",
        round2.primary_covered_seq, round2.promoted_identity.current_epoch
    );
    println!(
        "[two-round] E4 activation pair replaced by ordinary pair primary={} fallback={}",
        ids::hex_of(&n2_second.checkpoint_id),
        ids::hex_of(&n2_first.checkpoint_id)
    );

    // P2.4：B 仍未被 A 的两轮改动（身份/水位/inbox/outbox 与 P0.5 一致）。
    assert_b_offline(
        &b_vault,
        &b.identity_path,
        &b_identity_sha,
        &b_cursor0,
        b_inbox0,
        b_outbox0,
        "round 2",
    )?;
    let b_identity_before = common::load_identity(&b_dir)?;
    if b_identity_before.current_epoch != genesis::GENESIS_EPOCH {
        return Err(format!(
            "B 离线期间身份 epoch={}，期望仍为 E1",
            b_identity_before.current_epoch
        )
        .into());
    }
    println!("[two-round] B offline during round 2: identity epoch=1 (never saw E2/E3), outbox=1");

    // ---- P2.3：B 的离线编辑 2（仍在 E1；不触网）----
    let b_seq2 = common::append_offline_edit(&b_vault, "note-1", b.peer_id, OFFLINE_EDIT_R2)?;
    let b_local2 = common::create_local_only_doc(
        &b_vault,
        LOCAL_ONLY_E1B,
        b.peer_id,
        "B local only content E1b\n",
    )?;
    let (b_outbox_2, _digest_2, _) = common::inject_unacked_outbox(
        &b_vault,
        &b.root,
        b.device_id,
        "note-1",
        b_seq2,
        b"offline-update-before-apply",
    )?;
    if common::outbox_count(&b_vault)? != 2 {
        return Err("B apply 前 outbox 不是 2 行（两次离线编辑各一条）".into());
    }
    for message_id in [&b_outbox_1, &b_outbox_2] {
        let row = outbox_row(&b_vault, message_id)?.ok_or("B outbox 行缺失")?;
        if row.key_epoch != b.root.key_epoch {
            return Err(format!(
                "B 离线 outbox 行 epoch={}，期望 E1={}",
                row.key_epoch, b.root.key_epoch
            )
            .into());
        }
    }
    println!(
        "[two-round] round 2 offline: B note seq={} local_only seq={} outbox=2 (both key_epoch=1)",
        b_seq2, b_local2
    );

    // ---- 时间：+90d，然后 GC×3（每次前再 +7d+1h）----
    let mut acceptance = Acceptance::InProcess;
    if let Some(harness) = dev_relay.as_ref() {
        let before = harness.now();
        let after = harness.advance_secs(90 * DAY_SECS);
        common::set_now_override(Some(after as u64));
        println!(
            "[two-round] 90d clock advance: relay now {} -> {} (+{}d)",
            before,
            after,
            (after - before) / DAY_SECS
        );
        let mut pruned_total = 0u64;
        for round in 0..3 {
            if round > 0 {
                let next = harness.advance_secs(7 * DAY_SECS + 3600);
                common::set_now_override(Some(next as u64));
            }
            let gc = harness.run_gc()?;
            if gc.blocked {
                return Err(format!(
                    "GC #{} blocked（保留根不健康/unhealthy={:?}）",
                    round + 1,
                    gc.unhealthy
                )
                .into());
            }
            pruned_total += gc.pruned_checkpoints;
            println!(
                "[two-round] gc #{}: pruned_checkpoints={} retired_pruned_objects={} purged_slots={} cut_seq={} floor={} deleted_log_rows={} blocked=false",
                round + 1,
                gc.pruned_checkpoints,
                gc.retired_pruned_objects,
                gc.purged_slots,
                gc.cut_seq,
                gc.floor,
                gc.deleted_log_rows
            );
        }
        if pruned_total == 0 {
            return Err("90d + 3 次 GC 后 pruned_checkpoints 仍为 0".into());
        }
    } else {
        acceptance = Acceptance::ExternalDegraded;
        println!(
            "[two-round] EXTERNAL: 没有可控假时钟/GC 入口，跳过 90d+GC 段（TWO_ROUND_EXIT=2）"
        );
    }

    // ---- P4.1：B 进程级重启（drop + Vault::open + load_identity），身份仍 E1 ----
    drop(b_vault);
    b_vault = Vault::open(b_dir.join("vault.sqlite3"))?;
    let b_identity = common::load_identity(&b_dir)?;
    if b_identity.current_epoch != genesis::GENESIS_EPOCH {
        return Err(format!(
            "B 重启后身份 epoch={}，期望 E1（不是 E2）",
            b_identity.current_epoch
        )
        .into());
    }
    if !b_identity.current_root.ct_eq(&a.root.root) {
        return Err("B 重启后的根不是 E1 root".into());
    }
    let b_reloaded_sha = common::file_sha256(&b.identity_path)?;
    if b_reloaded_sha != b_identity_sha {
        return Err("B 重启前后 identity.bin 字节不一致".into());
    }
    // 设备签名/HPKE 全部从磁盘身份重建（e2e 不再持有 bootstrap 时的内存副本）。
    let b_signing = DeviceSigningKey::from_seed(b_identity.ed25519_seed.clone());
    let b_hpke = DeviceHpkeKey::from_secret(b_identity.x25519_secret.clone());
    let mut b_client = common::make_client(
        &relay_url,
        b.vault_id,
        b.device_id,
        b_signing,
        Some(a.manager.public_key()),
    )?;
    b_client.clear_auth_cache();
    let state_after_gc = b_client.state()?;
    if state_after_gc.epoch != round2.to_epoch {
        return Err(format!(
            "B 重启后看到 relay epoch={}，期望 {}",
            state_after_gc.epoch, round2.to_epoch
        )
        .into());
    }
    println!(
        "[two-round] B reopened: identity epoch=1 identity_sha256={} relay_epoch={} — 1->2->4 的起点是 E1",
        ids::hex_of(&b_reloaded_sha),
        state_after_gc.epoch
    );

    // ---- P3.3：prune 事实（激活 pair 已 pruned、普通 pair 仍 published）----
    let bundle = b_client.bootstrap()?;
    if bundle.epoch != round2.to_epoch
        || bundle.primary.checkpoint_id != n2_second.checkpoint_id
        || bundle.fallback.checkpoint_id != n2_first.checkpoint_id
    {
        return Err(format!(
            "90d 后 /bootstrap 不是 E4 普通 pair：epoch={} primary={} fallback={}",
            bundle.epoch,
            ids::hex_of(&bundle.primary.checkpoint_id),
            ids::hex_of(&bundle.fallback.checkpoint_id)
        )
        .into());
    }
    let activation_status = b_client.checkpoint_status(round2.primary_checkpoint)?;
    let ordinary_status = b_client.checkpoint_status(n2_second.checkpoint_id)?;
    if ordinary_status.state != "published" {
        return Err(format!(
            "E4 普通基线 state={}，期望 published",
            ordinary_status.state
        )
        .into());
    }
    if dev_relay.is_some() {
        if activation_status.state != "pruned" {
            return Err(format!(
                "E4 激活 pair state={}，期望 90d+GC 后 pruned",
                activation_status.state
            )
            .into());
        }
        let harness = dev_relay.as_ref().ok_or("relay 已停止")?;
        let activation_row = harness.checkpoint(round2.primary_checkpoint)?;
        if activation_row.state != CheckpointState::Pruned {
            return Err("E4 激活 pair 的行状态不是 pruned".into());
        }
        if activation_row.manifest_oid.is_none()
            || activation_row.signed_commit.is_none()
            || activation_row.commit_signature.is_none()
            || activation_row.refs_digest.is_none()
        {
            return Err(
                "prune 后签名骨架（manifest/signed_commit/signature/refs_digest）被删".into(),
            );
        }
        if harness.checkpoint_ref_count(round2.primary_checkpoint)? != 0 {
            return Err("prune 后 E4 激活 pair 的 checkpoint_refs 未清空".into());
        }
        let old_epoch_row = harness.checkpoint(round1.primary_checkpoint)?;
        if old_epoch_row.state != CheckpointState::Pruned {
            return Err("E2（retired epoch）的激活 pair 未被 prune".into());
        }
        println!(
            "[two-round] E4 activation pair pruned (refs=0, signed_commit/commit_signature kept), ordinary pair published; E2 pair pruned"
        );
    } else {
        println!("[two-round] EXTERNAL: 跳过 pruned 断言（未跑 GC）");
    }
    if bundle.gc_floor > bundle.primary.covered_seq {
        return Err(format!(
            "relay gc_floor={} 高于可选择基线 S={}，B 无法合法续读",
            bundle.gc_floor, bundle.primary.covered_seq
        )
        .into());
    }
    println!(
        "[two-round] bootstrap after GC: epoch={} S={} gc_floor={} head_seq={}",
        bundle.epoch, bundle.primary.covered_seq, bundle.gc_floor, bundle.head_seq
    );

    // ---- P4.2：控制链事实（restart 记录 1 条、激活 2 条）----
    let kinds = RelayClient::control_kinds(&mut b_client)?;
    let activations = kinds
        .iter()
        .filter(|(_, kind)| kind == "epoch.activated" || kind == "epoch_activated")
        .count();
    let restarts = kinds
        .iter()
        .filter(|(_, kind)| kind == "rotation.restarted" || kind == "rotation_restarted")
        .count();
    if activations != 2 || restarts != 1 {
        return Err(format!(
            "控制链记录不符：epoch.activated={activations}（期望 2）rotation.restarted={restarts}（期望 1）"
        )
        .into());
    }
    println!(
        "[two-round] control chain: epoch.activated={activations} rotation.restarted={restarts}"
    );

    // ---- P4.3：从 E1 起点审计 activation 链（1->2、2->4，E3 不在链上）----
    let chain = audit_epoch_chain(
        &mut b_client,
        b.vault_id,
        state_after_gc.instance_id,
        genesis::GENESIS_EPOCH,
        a.manager.public_key(),
        round2.rotation_id,
    )?;
    if chain.len() != 2 || chain[0].0 != 1 || chain[0].1 != 2 || chain[1].0 != 2 || chain[1].1 != 4
    {
        return Err(format!("activation 链不是 1->2、2->4：{chain:?}").into());
    }
    println!(
        "[two-round] B chain audit from E1: 1->2 (rev {}), 2->4 (rev {}) — E3 被 restart 跳过",
        chain[0].2, chain[1].2
    );

    // ---- P4.4：B 用同一份 E1 身份网络 apply 到 E4 ----
    let catch = apply_round_for_device(
        &mut b_client,
        &b_vault,
        &b_dir,
        &b.identity_path,
        &b_identity,
        &b_hpke,
        &round2,
    )?;
    let b_final = common::load_identity(&b_dir)?;
    if b_final.current_epoch != round2.to_epoch {
        return Err(format!(
            "B 最终身份 epoch={}，期望 {}",
            b_final.current_epoch, round2.to_epoch
        )
        .into());
    }
    if !b_final.current_root.ct_eq(&round2.new_root.root) {
        return Err("B 最终身份 root 不是 E4 candidate root".into());
    }
    if b_final.pinned_manager != round2.new_manager_public {
        return Err("B 最终身份 pin 的管理公钥不是 E4 新管理 key".into());
    }
    if b_final.control_rev != round2.control_rev {
        return Err(format!(
            "B 最终身份 control_rev={}，期望 {}",
            b_final.control_rev, round2.control_rev
        )
        .into());
    }
    if b_final.pending_rotation.is_some() {
        return Err("B 最终身份仍残留 pending_rotation".into());
    }
    println!(
        "[two-round] B caught up: E1 -> E{} (baseline_s={} control_rev={})",
        b_final.current_epoch, catch.baseline_s, b_final.control_rev
    );

    // ---- 离线编辑 / local-only / 水位 ----
    let b_note_final = common::body_text(&b_vault, "note-1", b.peer_id)?;
    if !b_note_final.contains("genesis note 0 from A")
        || !b_note_final.contains("B offline edit round 1")
        || !b_note_final.contains("B offline edit round 2")
    {
        return Err(format!("B 最终 note-1 丢了离线编辑：{b_note_final:?}").into());
    }
    let local_e1 = common::body_text(&b_vault, LOCAL_ONLY_E1, b.peer_id)?;
    let local_e1b = common::body_text(&b_vault, LOCAL_ONLY_E1B, b.peer_id)?;
    if local_e1 != "B local only content E1\n" || local_e1b != "B local only content E1b\n" {
        return Err(
            format!("B 最终 local-only 内容变了：e1={local_e1:?} e1b={local_e1b:?}").into(),
        );
    }
    println!(
        "[two-round] B offline edits preserved: note={:?} local_only={:?}/{:?}",
        b_note_final, local_e1, local_e1b
    );

    let cursor_final = sync_cursor_state(&b_vault)?;
    if cursor_final.integrated_seq != catch.baseline_s {
        return Err(format!(
            "B 最终 integrated_seq={} 与 E4 基线 S={} 不一致",
            cursor_final.integrated_seq, catch.baseline_s
        )
        .into());
    }
    if cursor_final.received_seq < b_cursor0.received_seq
        || cursor_final.integrated_seq < b_cursor0.integrated_seq
    {
        return Err(format!(
            "B 水位倒退：before(received={},integrated={}) after(received={},integrated={})",
            b_cursor0.received_seq,
            b_cursor0.integrated_seq,
            cursor_final.received_seq,
            cursor_final.integrated_seq
        )
        .into());
    }
    if let (Some(after), Some(before)) = (
        cursor_final.confirmed_highwater,
        b_cursor0.confirmed_highwater,
    ) {
        if after < before {
            return Err(format!("B confirmed_highwater 倒退：{before} -> {after}").into());
        }
    }
    let relay_cursor = b_vault.get_meta(META_SYNC_RELAY_CURSOR)?;
    if relay_cursor.as_deref() != Some(catch.baseline_cursor.as_bytes()) {
        return Err(format!(
            "B 最终 relay cursor={:?} 与 E4 基线 cursor={} 不一致",
            relay_cursor.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
            catch.baseline_cursor
        )
        .into());
    }
    if catch.baseline_s < b.report.covered_seq {
        return Err(format!(
            "B 水位倒退：E4 S={} < bootstrap S={}",
            catch.baseline_s, b.report.covered_seq
        )
        .into());
    }
    println!(
        "[two-round] B watermarks monotonic: bootstrap_S={} E4_S={} integrated={} cursor={}",
        b.report.covered_seq, catch.baseline_s, cursor_final.integrated_seq, catch.baseline_cursor
    );

    // ---- P5：outbox 用旧根（E1）重包到 E4，旧行删、新行不重复 ----
    let repack = repack_outbox_after_rotation(&b_vault, &b.root, &round2.new_root, b.device_id)?;
    if repack.repacked != 2 || repack.failed != 0 || repack.conflicts != 0 {
        return Err(format!(
            "B repack 期望 2/0/0，实际 {}/{}/{}",
            repack.repacked, repack.failed, repack.conflicts
        )
        .into());
    }
    if outbox_row(&b_vault, &b_outbox_1)?.is_some() || outbox_row(&b_vault, &b_outbox_2)?.is_some()
    {
        return Err("repack 后旧 outbox 行仍存在".into());
    }
    let rows_final = common::outbox_count(&b_vault)?;
    if rows_final != 2 || repack.new_message_ids.len() != 2 {
        return Err(format!(
            "repack 后 outbox 行数={rows_final} new_ids={}，期望 2/2（不重复）",
            repack.new_message_ids.len()
        )
        .into());
    }
    for new_message in &repack.new_message_ids {
        let row = outbox_row(&b_vault, new_message)?.ok_or("repack 新行不存在")?;
        if row.key_epoch != round2.new_root.key_epoch {
            return Err(format!(
                "repack 新行 epoch={} 不是 {}",
                row.key_epoch, round2.new_root.key_epoch
            )
            .into());
        }
        let parts = outbox_parts(&b_vault, new_message, round2.new_root.key_epoch)?;
        if parts.is_empty() {
            return Err("repack 新行没有 parts".into());
        }
        // 新 part 必须能被 E4 根解开（重包真的换了根）。
        let object_id = row.object_id.ok_or("repack 新行缺 object_id")?;
        if decrypt_envelope(
            &round2.new_root,
            ObjectType::Update,
            &object_id,
            &parts[0].envelope,
            &[],
        )
        .is_err()
        {
            return Err("E4 根解不开 repack 后的新 part".into());
        }
    }
    let report_meta = b_vault
        .get_meta(REPACK_REPORT_META)?
        .ok_or("缺少 sync.rotation_repack_report")?;
    let report_text = String::from_utf8_lossy(&report_meta).into_owned();
    if !report_text.contains("repacked=2")
        || !report_text.contains("failed=0")
        || !report_text.contains("conflicts=0")
    {
        return Err(format!("repack report 不符合预期：{report_text}").into());
    }
    if !repack_required(&b_vault)? {
        return Err("apply 后 repack 屏障应为 true".into());
    }
    set_repack_required(&b_vault, false)?;
    if repack_required(&b_vault)? {
        return Err("清除后 repack 屏障仍为 true".into());
    }
    println!(
        "[two-round] B outbox repack success: repacked={} failed={} conflicts={} rows={} (old_root=E1 -> E4)",
        repack.repacked, repack.failed, repack.conflicts, rows_final
    );

    // ---- 负例：旧根（E1/E2）解不开 E4 新对象 ----
    let new_message = repack
        .new_message_ids
        .first()
        .ok_or("repack 没有新 message_id")?;
    let new_row = outbox_row(&b_vault, new_message)?.ok_or("repack 新行不存在")?;
    let new_object = new_row.object_id.ok_or("repack 新行缺 object_id")?;
    let new_parts = outbox_parts(&b_vault, new_message, round2.new_root.key_epoch)?;
    for (label, root) in [("E1", &b.root), ("E2", &round1.new_root)] {
        if decrypt_envelope(
            root,
            ObjectType::Update,
            &new_object,
            &new_parts[0].envelope,
            &[],
        )
        .is_ok()
        {
            return Err(format!("旧 epoch root（{label}）解开了 E4 重包 outbox part").into());
        }
        println!("[two-round] negative: old epoch root ({label}) cannot decrypt E4 outbox part");
    }

    let snapshot = round2
        .primary_snapshot
        .as_ref()
        .ok_or("E4 候选基线没有 docs snapshot")?;
    // 先证明对象密文仍在（active epoch 的孤儿对象不回收），再证明旧根解不开。
    b_client.part(&snapshot.object_id, 0)?;
    for (label, root) in [("E1", &b.root), ("E2", &round1.new_root)] {
        if fetch_object(
            &mut b_client,
            root,
            ObjectType::Snapshot,
            &snapshot.object_id,
            snapshot.part_count,
            snapshot.total_plain_len,
            &snapshot.object_digest,
            &[],
        )
        .is_ok()
        {
            return Err(format!("旧 epoch root（{label}）解开了 E4 基线快照").into());
        }
        println!(
            "[two-round] negative: old epoch root ({label}) cannot decrypt E4 baseline snapshot"
        );
    }

    // ---- 负例：旧口令解不开 E4 password 槽 ----
    let password_header =
        Envelope::decode(&b_client.part(&round2.password_slot.descriptor.slot_id, 0)?)?.header;
    let password_descriptor = round2.password_slot.descriptor.encode()?;
    let password_aad = |_: &SlotDescriptorV1, _: &[u8; NONCE_LEN], _: usize| {
        password_header
            .aad(&password_descriptor)
            .map_err(SyncError::Proto)
    };
    for old_password in [PASSWORD_E1, PASSWORD_E2] {
        if round2
            .password_slot
            .unwrap_password(old_password.as_bytes(), &password_aad)
            .is_ok()
        {
            return Err(format!("旧口令 {old_password} 解开了 E4 password 槽").into());
        }
        println!(
            "[two-round] negative: old password ({old_password}) cannot unwrap E4 password slot"
        );
    }

    // ---- 负例：旧恢复码 R（E1/E2）解不开 E4 recovery 槽 ----
    let recovery_header =
        Envelope::decode(&b_client.part(&round2.recovery_slot.descriptor.slot_id, 0)?)?.header;
    let recovery_descriptor = round2.recovery_slot.descriptor.encode()?;
    let recovery_aad = |_: &SlotDescriptorV1, _: &[u8; NONCE_LEN], _: usize| {
        recovery_header
            .aad(&recovery_descriptor)
            .map_err(SyncError::Proto)
    };
    for (label, seed) in [("E1", &a.recovery_seed), ("E2", &round1.new_recovery_seed)] {
        if round2
            .recovery_slot
            .unwrap_recovery(seed, &recovery_aad)
            .is_ok()
        {
            return Err(format!("旧恢复码 R（{label}）解开了 E4 recovery 槽").into());
        }
        println!(
            "[two-round] negative: old recovery seed ({label}) cannot unwrap E4 recovery slot"
        );
    }

    // ---- 最终 relay 事实 ----
    b_client.clear_auth_cache();
    let final_state = b_client.state()?;
    if final_state.epoch != round2.to_epoch {
        return Err(format!(
            "最终 relay epoch={}，期望 {}",
            final_state.epoch, round2.to_epoch
        )
        .into());
    }
    let final_bundle = b_client.bootstrap()?;
    if final_bundle.epoch != round2.to_epoch
        || final_bundle.primary.epoch != round2.to_epoch
        || final_bundle.primary.checkpoint_id != n2_second.checkpoint_id
    {
        return Err(format!(
            "最终 bootstrap epoch={} primary_epoch={} primary={}，期望 E4 普通 pair",
            final_bundle.epoch,
            final_bundle.primary.epoch,
            ids::hex_of(&final_bundle.primary.checkpoint_id)
        )
        .into());
    }
    println!(
        "[two-round] final: relay epoch={} epoch_activated={} rotation_restarted={} bootstrap_epoch={} primary_cp_epoch={}",
        final_state.epoch,
        activations,
        restarts,
        final_bundle.epoch,
        final_bundle.primary.epoch
    );
    println!(
        "[two-round] summary: round1 E1->E2 S={} ; round2 E2->E4 (E3 restarted) S={} ; B offline E1 -> E4 baseline_s={} ; outbox repacked={}",
        round1.primary_covered_seq,
        round2.primary_covered_seq,
        catch.baseline_s,
        repack.repacked
    );
    Ok(acceptance)
}

/// B 在 A 做某一轮期间必须逐字节不变（身份/水位/inbox/outbox）。
fn assert_b_offline(
    vault: &Vault,
    identity_path: &Path,
    identity_sha: &[u8; 32],
    cursor_before: &SyncCursorState,
    inbox_before: u64,
    outbox_before: u64,
    stage: &str,
) -> AnyResult<()> {
    let sha = common::file_sha256(identity_path)?;
    if &sha != identity_sha {
        return Err(format!("A 做{stage}期间 B 的 identity.bin 被改动（B 不是离线）").into());
    }
    let cursor = sync_cursor_state(vault)?;
    if &cursor != cursor_before {
        return Err(format!("A 做{stage}期间 B 的水位被改动（B 不是离线）").into());
    }
    if common::inbox_count(vault)? != inbox_before {
        return Err(format!("A 做{stage}期间 B 的 sync_inbox 被改动（B 不是离线）").into());
    }
    if common::outbox_count(vault)? != outbox_before {
        return Err(format!("A 做{stage}期间 B 的 outbox 被改动（B 不是离线）").into());
    }
    Ok(())
}
