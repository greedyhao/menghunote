//! 两阶段轮换验收（原 `scripts/e2e_rotation_verify.ps1`）：自包含进程内 relay。
//!
//! 旧脚本先起外部 `dev_server --setup-only`，再依次跑**两个独立二进制**：
//! 1. `e2e_localhost`：genesis + localhost 同步 + 初始槽/checkpoint；
//! 2. `e2e_rotation`：在**同一个 vault** 上做轮换 D–F。
//!
//! 两个进程共享同一 relay DB 与 vault，进程内 relay 无法跨二进制共享，所以这里把两个
//! example 的入口拆成可调用函数（`e2e_localhost::run` / `e2e_rotation::run`），由本文件
//! 在**同一进程、同一个** [`DevRelay`] 上依次调用；两阶段之间**不重启 relay**。
//!
//! ```text
//! cargo run -p menghunote-sync --example e2e_rotation_verify
//! # 退出码 0，最后一行 VERIFY_EXIT=0
//! ```
//!
//! 工作目录每次运行重建：`<CARGO_TARGET_TMPDIR>/e2e-rotation-verify/vault`（与旧 ps1 的
//! `target/tmp/e2e-rot-verify` 同义），vault 必须从全新身份开始，否则会走"重启续传"分支。

#[path = "dev_relay.rs"]
mod dev_relay;
#[path = "e2e_localhost.rs"]
mod e2e_localhost;
#[path = "e2e_rotation.rs"]
mod e2e_rotation;

use std::error::Error as StdError;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dev_relay::DevRelay;
use menghunote_sync::ids;

type AnyResult<T> = Result<T, Box<dyn StdError>>;

fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("[verify] OK");
            println!("VERIFY_EXIT=0");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("[verify] FAILED: {error}");
            println!("VERIFY_EXIT=1");
            ExitCode::FAILURE
        }
    }
}

/// 工作区根（`crates/sync/examples` 往上三层）。
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// 每次运行重置的工作目录；测试临时目录用 `CARGO_TARGET_TMPDIR`（example 未设置时退到
/// `target/tmp`，与 `dev_relay.rs` 同一回退口径）。
fn work_dir() -> PathBuf {
    let root = match option_env!("CARGO_TARGET_TMPDIR") {
        Some(dir) => PathBuf::from(dir),
        None => workspace_root().join("target").join("tmp"),
    };
    root.join("e2e-rotation-verify")
}

fn run() -> AnyResult<()> {
    let work = work_dir();
    if work.exists() {
        std::fs::remove_dir_all(&work)?;
    }
    let vault_dir = work.join("vault");
    std::fs::create_dir_all(&vault_dir)?;

    // 两阶段共用同一个进程内 relay（临时 SQLite + FakeClock，仅 loopback）。
    let relay = DevRelay::start()?;
    println!(
        "[verify] in-process relay={} vault={}",
        relay.base_url(),
        vault_dir.display()
    );

    // 阶段 1：genesis + localhost 同步 + 初始槽/checkpoint（旧 e2e_localhost）。
    println!("[verify] stage 1/2: e2e_localhost");
    let localhost = e2e_localhost::run(relay.base_url(), Some(relay.setup_secret()), &vault_dir)?;
    println!(
        "[verify] stage 1 ok vault={} device={} integrated={}",
        ids::hex_of(&localhost.vault_id),
        ids::hex_of(&localhost.device_id),
        localhost.integrated_seq
    );

    // 阶段 2：同一 relay DB + 同一 vault 上做轮换（旧 e2e_rotation）。
    println!("[verify] stage 2/2: e2e_rotation");
    let rotation = e2e_rotation::run(relay.base_url(), &vault_dir)?;
    if rotation.vault_id != localhost.vault_id || rotation.device_id != localhost.device_id {
        return Err("旋转阶段读到的 vault/device 与 localhost 阶段不一致".into());
    }
    if rotation.to_epoch != 2 {
        return Err(format!("旋转后 epoch={}，期望 2", rotation.to_epoch).into());
    }
    println!(
        "[verify] stage 2 ok vault={} device={} epoch={} integrated={}",
        ids::hex_of(&rotation.vault_id),
        ids::hex_of(&rotation.device_id),
        rotation.to_epoch,
        rotation.integrated_seq
    );
    Ok(())
}
