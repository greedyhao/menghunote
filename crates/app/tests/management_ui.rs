//! ADR 0016 **S4d1c**：客户端 V2 管理 UI（设置页里的「管理」区，通道 A 双签）。
//!
//! 覆盖：
//!
//! 1. **无 V2 提示**（真实 dev relay + 真实 legacy vault + 真实 `RealManagementRunner`）：
//!    管理区显示「当前 vault 未启用 V2」，不展示伪造的 cap/设备成功状态，不报假错误；
//! 2. **状态概览**：control head/rev、authz/authority rev、cap 摘要、设备 ACTIVE/
//!    PENDING/REVOKED、待办审批数；
//! 3. **审批详情**：kind/目标/cap scope/租户/实例/路径/rev/A/B 槽位指纹、`h` 的 128 位
//!    比较指纹（展开 256 位）、回执；
//! 4. **导入/导出 + A 签/B 加签/满签提交/取消**：用**真实** `ApprovalEnvelopeV1`/真实
//!    Ed25519 签名（fixture 由 `approval_v1` 的确定性构造生成，见文件末尾常量），
//!    文件与 base64 两种通道都走真实编解码；
//! 5. **过期/失效**：过期后签署/提交被本地拦截（不自动延 deadline）；证据失效显示原因；
//! 6. **重启恢复**：新面板/新执行体共享同一 vault，能从公开 meta 恢复草稿；
//! 7. **错误脱敏**：路径/URL/恢复码/长 base64 token 不进 UI；
//! 8. **等待第二签不冻结编辑/同步**：A 签后（等待 B）编辑器仍可输入，且没有持闸。
//!
//! 纪律：临时目录一律在 `CARGO_TARGET_TMPDIR`；**不 sleep**（收敛判据 = 面板快照被
//! 事件更新，循环里有 deadline）；headless 下 GPUI 定时器不会自动走，显式
//! `advance_clock` 推进管理面板的 40 ms 泵。
//!
//! feature 关闭时整个文件编译为空（0 个测试）。

#![cfg(feature = "dev-sync")]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    div, AppContext as _, Context, Entity, InteractiveElement as _, Modifiers, ParentElement as _,
    Render, Styled as _, TestAppContext, VisualTestContext,
};

use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::management::{
    approval_view, CapView, DeviceStatusView, DeviceView, ManagementArtifact, ManagementCommand,
    ManagementReport, ManagementRunner, ManagementSnapshot, PolicyView, ReceiptView,
    MANAGEMENT_PUMP_INTERVAL,
};
use menghunote_app::dev_sync::service::{ServiceConfig, SyncService};
use menghunote_app::dev_sync::setup::{self, SetupMode, SetupSecrets};
use menghunote_app::dev_vault;
use menghunote_app::onboarding::{
    AppRoot, AppRootOptions, DevSyncProfile, InstanceEntry, ProfileLayout, RelayApiVersion,
    RelayMode, DEFAULT_OPERATION_BUDGET, VAULT_FILE_NAME,
};
use menghunote_app::shell::AppShell;
use menghunote_dev_relay::DevRelay;
use menghunote_storage::{SyncContext, Vault};
use menghunote_sync::approval_v1::{ApprovalDraftV1, ApprovalStateV1};
use menghunote_sync::device::DeviceSigningKey;
use menghunote_sync::keys::Secret32;
use menghunote_sync::management_v2::AuthzStateV2;

/// 事件/状态收敛的安全上限（真实 relay 走 loopback，正常毫秒级）。
const DEADLINE: Duration = Duration::from_secs(30);

/// UI 测试用的确定性审批包（**零签名**；两个 role0 设备槽 A/B，各自带 cap 证据）。
///
/// 由 `crates/sync/src/approval_v1.rs` 的同构构造生成（kind 2 CapGrant、rev 7→8、
/// authz 3、authority 1、A=seed[0x21]、B=seed[0x22]、cap C1/C2、新 cap C3）。
/// 这里用字节常量避免 app 直接依赖 `sync-proto`（app 只经 `menghunote-sync` 的公开
/// 客户端 API）；解析/签名/编解码全部走真实 `ApprovalDraftV1`。
const APPROVAL_FIXTURE_HEX: &str = "a70001015901d1af000201501111111111111111111111111111111102505a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a03020450a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a10507065820b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b107030801091a6b49d2000a1a6b49d32c0b020c783a2f76322f7661756c74732f35613561356135613561356135613561356135613561356135613561356135612f636f6e74726f6c2f7375626d69740d5889a1005885ad000201501111111111111111111111111111111102505a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a0350c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3045003030303030303030303030303030303055820a09aa5f47a6759802ff955f8dc2d2a14a5c99d23be97f864127ff9383455a4f006183f071a6b49d1c4081a6b49d45809060a030b010cf40e82a400000150010101010101010101010101010101010250c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1035820884b8857f4eaa1613c61504db34d4beaf346517a0e31de3cddd4d9b4201d9d0ba400000150020202020202020202020202020202020250c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2035820a09aa5f47a6759802ff955f8dc2d2a14a5c99d23be97f864127ff9383455a4f00258204b8133500f34827b45eed823a29cc5b6d7d0fdb044fdff58228c626eb16eb1f00300048005825885ad000201501111111111111111111111111111111102505a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a0350c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1045001010101010101010101010101010101055820884b8857f4eaa1613c61504db34d4beaf346517a0e31de3cddd4d9b4201d9d0b06183f071a6b49d1c4081a6b49d45809060a030b010cf45885ad000201501111111111111111111111111111111102505a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a0350c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2045002020202020202020202020202020202055820a09aa5f47a6759802ff955f8dc2d2a14a5c99d23be97f864127ff9383455a4f006183f071a6b49d1c4081a6b49d45809060a030b010cf406a40007015820b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b102030301";

/// UI 测试用的确定性**单签**审批包（kind 11 InviteCreate；一个 role0 槽 A）。
/// 由 `approval_v1` 的同构构造生成；用于"过期/提交失败"这类不需要第二签的用例。
const APPROVAL_SINGLE_FIXTURE_HEX: &str = "a700010159013aaf000201501111111111111111111111111111111102505a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a030b0450a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a10507065820b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b107030801091a6b49d2000a1a6b49d32c0b020c783a2f76322f7661756c74732f35613561356135613561356135613561356135613561356135613561356135612f636f6e74726f6c2f7375626d69740d583ca30050515151515151515151515151515151510158206161616161616161616161616161616161616161616161616161616161616161021a6b49d4580e81a400000150010101010101010101010101010101010250c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1035820884b8857f4eaa1613c61504db34d4beaf346517a0e31de3cddd4d9b4201d9d0b025820ff0109ab2e98882aff44c0a4329ebbd2ae8e2bb5ee827b16c667cbd6b76593720300048005815885ad000201501111111111111111111111111111111102505a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a0350c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1045001010101010101010101010101010101055820884b8857f4eaa1613c61504db34d4beaf346517a0e31de3cddd4d9b4201d9d0b06183f071a6b49d1c4081a6b49d45809060a030b010cf406a40007015820b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b102030301";

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("management-ui-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        // 管理 UI 是实验功能，默认不在设置页出现；本文件显式打开。
        std::env::set_var("MENGHUNOTE_EXPERIMENTAL_V2", "1");
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录。
fn temp_root(prefix: &str) -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("management-ui-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    tempfile::Builder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

fn fixture_bytes() -> Vec<u8> {
    menghunote_sync::ids::parse_hex::<826>(APPROVAL_FIXTURE_HEX)
        .expect("fixture hex 必须合法")
        .to_vec()
}

fn fixture_draft() -> ApprovalDraftV1 {
    ApprovalDraftV1::from_bytes(&fixture_bytes()).expect("fixture 必须是合法审批包")
}

fn single_fixture_bytes() -> Vec<u8> {
    menghunote_sync::ids::parse_hex::<540>(APPROVAL_SINGLE_FIXTURE_HEX)
        .expect("single fixture hex 必须合法")
        .to_vec()
}

fn key(seed: u8) -> DeviceSigningKey {
    DeviceSigningKey::from_seed(Secret32::from_array([seed; 32]))
}

/// 窗口根视图：给 `AppShell` 一个稳定的宿主 selector（与其它 headless 测试同构）。
struct ShellHost {
    shell: Entity<AppShell>,
}

impl Render for ShellHost {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .size_full()
            .debug_selector(|| "app-shell-host".to_string())
            .child(self.shell.clone())
    }
}

fn config_for(relay_url: &str, vault_path: &Path) -> DevSyncConfig {
    DevSyncConfig {
        relay_mode: menghunote_app::onboarding::RelayMode::LocalDev,
        vault: vault_path.to_path_buf(),
        service: ServiceConfig::new(relay_url),
    }
}

// ---------------------------------------------------------------- 脚本化执行体

/// 脚本化管理执行体：用**真实** `ApprovalDraftV1` 做导入/导出/签署/清草稿，用预设快照
/// 投影 V2 概览；`Submit` 只模拟"回执已收到"（真实提交由 sync 集成测试覆盖）。
struct FakeManagementRunner {
    base: ManagementSnapshot,
    /// 本机签名种子（必须匹配 fixture 的一个槽）。
    seed: u8,
    /// 模拟的时间（决定 expired）。
    now: u64,
    /// 预设的提交回执；`None` = 提交失败（写 `submit_error`）。
    submit_ok: Option<ReceiptView>,
    /// 预设的提交失败文案（用于错误脱敏用例）。
    submit_error: Option<String>,
    /// 预设的失效原因（签前重验失败的投影）。
    invalid_reason: Option<String>,
    /// 草稿持久化用的 vault（重启恢复用；`None` = 不落盘）。
    vault: Option<Vault>,
    /// 命令日志（测试断言"确实没有自动签/自动提交"）。
    log: Arc<Mutex<Vec<ManagementCommand>>>,
    draft: Option<ApprovalDraftV1>,
    confirmed: Option<ReceiptView>,
    last_error: Option<String>,
    loaded: bool,
}

impl FakeManagementRunner {
    fn new(seed: u8, log: Arc<Mutex<Vec<ManagementCommand>>>) -> Self {
        Self {
            base: ManagementSnapshot {
                v2: true,
                v2_message: None,
                relay: "http://127.0.0.1:9".to_owned(),
                control_rev: 7,
                head_hash: [0xB1; 32],
                authz_rev: 3,
                authority_rev: 1,
                authz_state: Some(AuthzStateV2::Active),
                policy: Some(PolicyView {
                    highrisk_threshold: 2,
                    min_quorum: 2,
                    max_cap_ttl: 2_592_000,
                    max_invite_ttl: 86_400,
                }),
                caps: vec![
                    CapView {
                        cap_id: [0xC1; 16],
                        subject_device_id: [0x01; 16],
                        scopes: 1 | 2 | 4 | 8 | 16 | 32,
                        not_before: 1_799_999_940,
                        expires_at: 1_800_000_600,
                        revoked: false,
                        effective: true,
                        granted_rev: 3,
                    },
                    CapView {
                        cap_id: [0xC2; 16],
                        subject_device_id: [0x02; 16],
                        scopes: 1 | 2 | 4 | 8 | 16 | 32,
                        not_before: 1_799_999_940,
                        expires_at: 1_800_000_600,
                        revoked: false,
                        effective: true,
                        granted_rev: 4,
                    },
                ],
                devices: vec![
                    DeviceView {
                        device_id: [0x01; 16],
                        status: DeviceStatusView::Active,
                        cert_serial: Some(1),
                        data_rights: Some(3),
                        approved_rev: Some(3),
                        revoked_rev: None,
                        cap_summary: Some(
                            "邀请/批准/撤销/cap 管理/轮换/策略 到期 2027-01-15 08:10 UTC"
                                .to_owned(),
                        ),
                        note: None,
                    },
                    DeviceView {
                        device_id: [0x02; 16],
                        status: DeviceStatusView::Active,
                        cert_serial: Some(1),
                        data_rights: Some(3),
                        approved_rev: Some(4),
                        revoked_rev: None,
                        cap_summary: Some(
                            "邀请/批准/撤销/cap 管理/轮换/策略 到期 2027-01-15 08:10 UTC"
                                .to_owned(),
                        ),
                        note: None,
                    },
                    DeviceView {
                        device_id: [0x0C; 16],
                        status: DeviceStatusView::Pending,
                        cert_serial: None,
                        data_rights: None,
                        approved_rev: None,
                        revoked_rev: None,
                        cap_summary: None,
                        note: Some("已兑换邀请、等待批准（不在链上）".to_owned()),
                    },
                    DeviceView {
                        device_id: [0x0D; 16],
                        status: DeviceStatusView::Revoked,
                        cert_serial: Some(1),
                        data_rights: Some(1),
                        approved_rev: Some(2),
                        revoked_rev: Some(6),
                        cap_summary: None,
                        note: None,
                    },
                ],
                pending_approvals: 0,
                approval: None,
                last_refresh_unix: Some(1_800_000_000),
                loading: false,
                error: None,
                notice: None,
            },
            seed,
            now: 1_800_000_000,
            submit_ok: Some(ReceiptView {
                control_rev: 8,
                head_hash: [0xB2; 32],
                kind: 2,
                authz_rev: 4,
                authority_rev: 1,
            }),
            submit_error: None,
            invalid_reason: None,
            vault: None,
            log,
            draft: None,
            confirmed: None,
            last_error: None,
            loaded: false,
        }
    }

    fn with_vault(mut self, vault: Vault) -> Self {
        self.vault = Some(vault);
        self
    }

    fn with_now(mut self, now: u64) -> Self {
        self.now = now;
        self
    }

    fn with_invalid_reason(mut self, reason: &str) -> Self {
        self.invalid_reason = Some(reason.to_owned());
        self
    }

    fn with_submit_failure(mut self, message: &str) -> Self {
        self.submit_ok = None;
        self.submit_error = Some(message.to_owned());
        self
    }

    fn local_pk(&self) -> [u8; 32] {
        key(self.seed).public_key()
    }

    fn load_once(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        if let Some(vault) = &self.vault {
            self.draft = ApprovalDraftV1::load_draft(vault).unwrap_or(None);
        }
    }

    fn snapshot(&self) -> ManagementSnapshot {
        let mut snapshot = self.base.clone();
        snapshot.pending_approvals = snapshot
            .devices
            .iter()
            .filter(|device| device.status == DeviceStatusView::Pending)
            .count();
        if let Some(draft) = &self.draft {
            let mut view = approval_view(
                draft,
                Some(self.local_pk()),
                self.now,
                self.invalid_reason.clone(),
            );
            if let Some(receipt) = self.confirmed {
                view.state = ApprovalStateV1::Confirmed;
                view.state_label = ApprovalStateV1::Confirmed.label();
                view.receipt = Some(receipt);
                view.can_sign_local = false;
            }
            snapshot.approval = Some(view);
            snapshot.pending_approvals += 1;
        }
        snapshot.error = self.last_error.clone();
        snapshot
    }

    fn report(&mut self, notice: Option<String>) -> ManagementReport {
        let mut snapshot = self.snapshot();
        snapshot.notice = notice;
        ManagementReport {
            snapshot,
            artifact: None,
        }
    }

    fn install(&mut self, draft: ApprovalDraftV1, notice: &str) -> ManagementReport {
        if let Some(vault) = &self.vault {
            if let Err(error) = draft.save_draft(vault) {
                return self.fail(format!("保存本地审批草稿失败：{error}"));
            }
        }
        self.draft = Some(draft);
        self.confirmed = None;
        self.last_error = None;
        self.report(Some(notice.to_owned()))
    }

    fn fail(&mut self, message: String) -> ManagementReport {
        self.last_error = Some(message);
        self.report(None)
    }
}

impl ManagementRunner for FakeManagementRunner {
    fn run(&mut self, command: ManagementCommand) -> ManagementReport {
        if let Ok(mut log) = self.log.lock() {
            log.push(command.clone());
        }
        match command {
            ManagementCommand::Refresh => {
                self.load_once();
                self.last_error = None;
                self.report(None)
            }
            ManagementCommand::ImportFile(path) => match ApprovalDraftV1::read_file(&path) {
                Ok(draft) => self.install(draft, "已导入并本地验证审批包（未执行/未联网提交）"),
                Err(error) => self.fail(format!("读取审批包文件失败：{error}")),
            },
            ManagementCommand::ImportBase64(text) => match ApprovalDraftV1::from_base64(&text) {
                Ok(draft) => self.install(draft, "已导入并本地验证审批包（未执行/未联网提交）"),
                Err(error) => self.fail(format!("解析审批包文本失败：{error}")),
            },
            ManagementCommand::ExportFile(path) => {
                let Some(draft) = self.draft.as_mut() else {
                    return self.fail("当前没有本地审批草稿可导出".to_owned());
                };
                match draft.write_file(&path) {
                    Ok(()) => {
                        if let Some(vault) = &self.vault {
                            let _ = draft.save_draft(vault);
                        }
                        self.report(Some("已导出审批包文件（不自动执行/不联网）".to_owned()))
                    }
                    Err(error) => self.fail(format!("写审批包文件失败：{error}")),
                }
            }
            ManagementCommand::ExportBase64 => {
                let Some(draft) = self.draft.as_mut() else {
                    return self.fail("当前没有本地审批草稿可导出".to_owned());
                };
                match draft.to_base64() {
                    Ok(text) => {
                        draft.mark_handed_off();
                        if let Some(vault) = &self.vault {
                            let _ = draft.save_draft(vault);
                        }
                        let mut report =
                            self.report(Some("已生成审批包复制文本（不联网）".to_owned()));
                        report.artifact = Some(ManagementArtifact::ExportedBase64(text));
                        report
                    }
                    Err(error) => self.fail(format!("编码审批包文本失败：{error}")),
                }
            }
            ManagementCommand::Sign => {
                let Some(draft) = self.draft.as_mut() else {
                    return self.fail("当前没有本地审批草稿可签署".to_owned());
                };
                match draft.sign_with(&key(self.seed)) {
                    Ok(()) => {
                        if let Some(vault) = &self.vault {
                            let _ = draft.save_draft(vault);
                        }
                        let notice = match draft.state() {
                            ApprovalStateV1::Ready => "已签署：包已满签，可提交",
                            _ => "已签署：等待对方加签（不持编辑/同步闸）",
                        };
                        self.report(Some(notice.to_owned()))
                    }
                    Err(error) => self.fail(format!("签署失败：{error}")),
                }
            }
            ManagementCommand::Submit => {
                if self.draft.is_none() {
                    return self.fail("当前没有本地审批草稿可提交".to_owned());
                }
                match self.submit_ok {
                    Some(receipt) => {
                        self.confirmed = Some(receipt);
                        self.last_error = None;
                        self.report(Some(format!(
                            "已提交并完成本地对账：control rev {}",
                            receipt.control_rev
                        )))
                    }
                    None => {
                        let message = self
                            .submit_error
                            .clone()
                            .unwrap_or_else(|| "模拟的传输失败".to_owned());
                        self.fail(format!("提交失败（Transport，可重发原字节）：{message}"))
                    }
                }
            }
            ManagementCommand::Cancel => {
                if let Some(vault) = &self.vault {
                    if let Err(error) = ApprovalDraftV1::clear_draft(vault) {
                        return self.fail(format!("清本地审批草稿失败：{error}"));
                    }
                }
                self.draft = None;
                self.confirmed = None;
                self.last_error = None;
                self.report(Some(
                    "已取消协调：只清本地草稿；已外发签名不能撤回".to_owned(),
                ))
            }
            ManagementCommand::Stop => self.report(None),
        }
    }
}

// ---------------------------------------------------------------- 宿主/驱动

fn add_shell(
    cx: &mut TestAppContext,
    vault: Vault,
    label: &'static str,
    sync: SyncContext,
    dev_sync: Option<DevSyncConfig>,
    management: Option<Box<dyn ManagementRunner>>,
) -> (Entity<ShellHost>, gpui::AnyWindowHandle) {
    let slot = std::rc::Rc::new(std::cell::Cell::new(None));
    let captured = slot.clone();
    let (host, window_cx) = cx.add_window_view(move |window, cx| {
        captured.set(Some(window.window_handle()));
        let shell = match (dev_sync, management) {
            (Some(config), None) => {
                cx.new(|cx| AppShell::new_with_dev_sync(vault, label.to_owned(), config, sync, cx))
            }
            (None, Some(runner)) => cx.new(|cx| {
                let service = SyncService::new(ServiceConfig::new("http://127.0.0.1:9"));
                AppShell::new_with_dev_sync_service_auto_and_management(
                    vault,
                    label.to_owned(),
                    sync,
                    service,
                    None,
                    Some(runner),
                    cx,
                )
            }),
            _ => panic!("测试只允许：真实 dev-sync 或注入管理 runner"),
        };
        shell.update(cx, |shell: &mut AppShell, cx| {
            shell.open_settings_headless(cx);
            shell.toggle_management(cx);
        });
        ShellHost { shell }
    });
    let handle = slot
        .get()
        .or_else(|| window_cx.windows().first().copied())
        .expect("窗口句柄必须可取");
    (host, handle)
}

fn with_shell<R>(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    f: impl FnOnce(&mut AppShell, &mut Context<AppShell>) -> R,
) -> R {
    cx.update(|_window, app| {
        host.update(app, |host, cx| {
            host.shell.update(cx, |shell, cx| f(shell, cx))
        })
    })
}

fn panel_entity(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
) -> Entity<menghunote_app::dev_sync::ManagementPanel> {
    with_shell(cx, host, |shell, _cx| shell.management_panel()).expect("管理面板必须挂载")
}

fn drive_management(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    description: &str,
    expect: impl Fn(&ManagementSnapshot) -> bool,
) -> ManagementSnapshot {
    let panel = panel_entity(cx, host);
    let deadline = Instant::now() + DEADLINE;
    loop {
        // headless 下 GPUI 定时器不会自动走：显式推进管理面板的 40 ms 泵。
        cx.executor().advance_clock(MANAGEMENT_PUMP_INTERVAL);
        cx.update(|_window, app| {
            panel.update(app, |panel, cx| {
                if panel.pump(cx) {
                    cx.notify();
                }
            });
        });
        cx.run_until_parked();
        let snapshot = cx.update(|_window, app| panel.read(app).snapshot().clone());
        if expect(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "30s 内没有收敛（{description}）：{snapshot:?}"
        );
        std::thread::yield_now();
    }
}

/// 等管理面板的首次自动刷新收尾（`activate` 只发一次 Refresh；期间 `try_command`
/// 会以"上一操作仍在进行"拒绝后续动作，测试必须先等它完成）。
fn wait_initial_refresh(cx: &mut VisualTestContext, host: &Entity<ShellHost>) {
    drive_management(cx, host, "初始刷新", |snapshot| {
        !snapshot.loading && (snapshot.v2 || snapshot.error.is_some())
    });
}

fn act(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    action: impl FnOnce(
        &mut menghunote_app::dev_sync::ManagementPanel,
        &mut Context<menghunote_app::dev_sync::ManagementPanel>,
    ),
) {
    let panel = panel_entity(cx, host);
    cx.update(|_window, app| {
        panel.update(app, |panel, cx| {
            action(panel, cx);
            cx.notify();
        });
    });
    cx.run_until_parked();
}

// ---------------------------------------------------------------- 1) 无 V2（真实）

#[gpui::test]
async fn legacy_vault_reports_v2_disabled(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("legacy-real");
    let path = dir.path().join("inst").join("vault.sqlite3");
    let outcome = setup::prepare(
        &config_for(relay.base_url(), &path),
        SetupMode::Create,
        SetupSecrets {
            setup_secret: Some(relay.setup_secret().to_owned()),
            ..SetupSecrets::default()
        },
    )
    .expect("Create 失败");
    let peer = outcome.identity.loro_peer_id();
    let epoch = outcome.identity.current_epoch;
    let vault = outcome.vault.clone();
    let sync = SyncContext::new(peer, epoch);
    let dev_sync = config_for(relay.base_url(), &path);

    let (host, handle) = add_shell(cx, vault, "mgmt-legacy", sync, Some(dev_sync), None);
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("settings-toggle-management").is_some(),
        "真实 AppShell 的设置页必须有「管理」入口"
    );
    assert!(
        cx.debug_bounds("mgmt-panel").is_some(),
        "展开后必须挂载管理区"
    );

    let snapshot = drive_management(&mut cx, &host, "legacy 刷新完成", |snapshot| {
        snapshot.last_refresh_unix.is_some() && !snapshot.loading
    });
    assert!(!snapshot.v2, "legacy vault 不得被显示成 V2");
    let message = snapshot.v2_message.clone().unwrap_or_default();
    assert!(
        message.contains("未启用 V2"),
        "必须明确说明当前 vault 未启用 V2：{snapshot:?}"
    );
    assert!(
        snapshot.error.is_none(),
        "legacy 不是错误（不伪造失败）：{snapshot:?}"
    );
    assert!(
        snapshot.caps.is_empty(),
        "未启用 V2 不得展示 cap：{snapshot:?}"
    );
    assert!(
        snapshot.devices.is_empty(),
        "未启用 V2 不得展示伪造设备：{snapshot:?}"
    );
    assert_eq!(snapshot.pending_approvals, 0);
    assert!(snapshot.approval.is_none());
    // 无 V2 时导入/签署动作只会得到明确提示，不伪造成功。
    act(&mut cx, &host, |panel, cx| panel.request_sign(cx));
    let panel = panel_entity(&mut cx, &host);
    let snapshot = cx.update(|_window, app| panel.read(app).snapshot().clone());
    assert!(
        snapshot
            .notice
            .as_deref()
            .unwrap_or_default()
            .contains("未启用 V2"),
        "无 V2 时必须明确拒绝签署：{snapshot:?}"
    );
}

// ---------------------------------------------------------------- 2) 概览 + 详情

#[gpui::test]
async fn fake_panel_shows_overview_devices_and_approval_details(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("overview");
    let dev = dev_vault::open_vault_at_with_peer(&dir.path().join("vault.sqlite3"), 1)
        .expect("打开 dev vault 失败");
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut runner = FakeManagementRunner::new(0x21, log.clone());
    runner.draft = Some(fixture_draft());
    let (host, handle) = add_shell(
        cx,
        dev.vault.clone(),
        "mgmt-overview",
        SyncContext::new(1, 1),
        None,
        Some(Box::new(runner)),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();

    let snapshot = drive_management(&mut cx, &host, "概览刷新", |snapshot| {
        snapshot.approval.is_some()
    });
    assert!(snapshot.v2, "fake 快照必须是 V2");
    assert_eq!(snapshot.control_rev, 7);
    assert_eq!(snapshot.authz_rev, 3);
    assert_eq!(snapshot.authority_rev, 1);
    assert_eq!(snapshot.caps.len(), 2);
    assert_eq!(snapshot.devices.len(), 4);
    assert!(snapshot
        .devices
        .iter()
        .any(|device| device.status == DeviceStatusView::Pending));
    assert!(snapshot
        .devices
        .iter()
        .any(|device| device.status == DeviceStatusView::Revoked));
    assert_eq!(snapshot.pending_approvals, 2, "1 台 PENDING + 1 个本地草稿");

    let approval = snapshot.approval.clone().expect("审批详情");
    assert_eq!(approval.kind, 2, "fixture 是 CapGrant");
    assert_eq!(approval.kind_label, "授予 cap（CapGrant）");
    assert_eq!(approval.state, ApprovalStateV1::Draft);
    assert_eq!(approval.signer_count, 2);
    assert_eq!(approval.signature_count, 0);
    assert!(approval.can_sign_local, "A（seed 0x21）应能首签");
    assert!(approval.target.contains("授予设备"), "{approval:?}");
    assert!(approval.scopes.contains("cap 管理"), "{approval:?}");
    assert_eq!(
        approval.canonical_path,
        format!(
            "/v2/vaults/{}/control/submit",
            menghunote_sync::ids::hex_of(&[0x5Au8; 16])
        )
    );
    assert_eq!(approval.expected_control_rev, 7);
    assert_eq!(approval.authz_rev, 3);
    assert_eq!(approval.authority_rev, 1);
    assert_eq!(approval.fingerprint_128.len(), 32 + 3, "4×8 hex + 3 空格");
    assert_eq!(approval.fingerprint_256.len(), 64 + 7, "8×8 hex + 7 空格");
    assert_eq!(
        approval.fingerprint_256.replace(' ', ""),
        menghunote_sync::ids::hex_of(&fixture_draft().envelope().digest()),
        "完整 256 位必须等于 h"
    );
    assert_eq!(approval.slots.len(), 2);
    assert_eq!(approval.slots[0].role_label, "设备");
    assert!(!approval.slots[0].signed && !approval.slots[1].signed);

    // 固定 selector：详情/指纹都在布局里，且 256 位默认收起、展开后出现。
    assert!(cx.debug_bounds("mgmt-panel").is_some());
    assert!(cx.debug_bounds("mgmt-v2-status").is_some());
    assert!(cx.debug_bounds("mgmt-control").is_some());
    assert!(cx.debug_bounds("mgmt-approval").is_some());
    assert!(cx.debug_bounds("mgmt-approval-fingerprint-128").is_some());
    assert!(
        cx.debug_bounds("mgmt-approval-fingerprint-256").is_none(),
        "256 位默认收起"
    );
    act(&mut cx, &host, |panel, cx| panel.toggle_fingerprint(cx));
    assert!(cx.debug_bounds("mgmt-approval-fingerprint-256").is_some());
}

// ---------------------------------------------------------------- 3) 导入/导出/双签/提交

#[gpui::test]
async fn import_export_dual_sign_submit_and_restart_recovery(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("flow");
    let vault_a = dev_vault::open_vault_at_with_peer(&dir.path().join("a.sqlite3"), 1)
        .expect("A vault")
        .vault;
    let vault_b = dev_vault::open_vault_at_with_peer(&dir.path().join("b.sqlite3"), 2)
        .expect("B vault")
        .vault;

    // A：导入 fixture → 签署 → 导交给 B（文件 + base64）。
    let log_a = Arc::new(Mutex::new(Vec::new()));
    let runner_a = FakeManagementRunner::new(0x21, log_a.clone()).with_vault(vault_a.clone());
    let (host_a, handle_a) = add_shell(
        cx,
        vault_a.clone(),
        "mgmt-flow-a",
        SyncContext::new(1, 1),
        None,
        Some(Box::new(runner_a)),
    );
    let app_cx = cx.clone();
    let mut cx_a = VisualTestContext::from_window(handle_a, &app_cx);
    cx_a.run_until_parked();
    wait_initial_refresh(&mut cx_a, &host_a);

    // 先写入 fixture 文件，模拟"手工交换 .mn-approval"。
    let fixture_path = dir.path().join("a-in.mn-approval");
    std::fs::write(&fixture_path, fixture_bytes()).expect("写 fixture 文件");
    act(&mut cx_a, &host_a, |panel, cx| {
        panel.set_path_text(&fixture_path.display().to_string(), cx);
        panel.request_import_file(cx);
    });
    let snapshot = drive_management(&mut cx_a, &host_a, "A 导入", |snapshot| {
        snapshot.approval.is_some() && !snapshot.loading
    });
    assert_eq!(
        snapshot.approval.as_ref().map(|approval| approval.state),
        Some(ApprovalStateV1::Draft)
    );

    act(&mut cx_a, &host_a, |panel, cx| panel.request_sign(cx));
    let snapshot = drive_management(&mut cx_a, &host_a, "A 签署", |snapshot| {
        snapshot
            .approval
            .as_ref()
            .is_some_and(|approval| approval.signature_count == 1)
    });
    let approval = snapshot.approval.clone().expect("A 已签");
    assert_eq!(approval.state, ApprovalStateV1::InitiatorSigned);
    assert_eq!(approval.signature_count, 1);
    assert!(!approval.can_sign_local, "A 不能重复签自己的槽");
    assert!(
        log_a
            .lock()
            .expect("log")
            .iter()
            .any(|command| matches!(command, ManagementCommand::Sign)),
        "签署必须真的发给执行体"
    );

    // 导出到文件：真实编码 + 真实原子写；文件可被重新解析且带 A 的签名。
    let export_path = dir.path().join("a-out.mn-approval");
    act(&mut cx_a, &host_a, |panel, cx| {
        panel.set_path_text(&export_path.display().to_string(), cx);
        panel.request_export_file(cx);
    });
    drive_management(&mut cx_a, &host_a, "A 导出文件", |snapshot| {
        snapshot
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("已导出"))
    });
    let exported = ApprovalDraftV1::read_file(&export_path).expect("导出的文件必须可解析");
    // re-read 视为"已通过文件交付"：1 签 + handed_off = AwaitingCosign。
    assert_eq!(exported.state(), ApprovalStateV1::AwaitingCosign);
    assert_eq!(exported.envelope().signatures().len(), 1);

    // 导出 base64：完成后面板把文本写进 `exported`（公开证据）并存剪贴板。
    act(&mut cx_a, &host_a, |panel, cx| {
        panel.request_export_base64(cx)
    });
    drive_management(&mut cx_a, &host_a, "A 导出 base64", |snapshot| {
        snapshot
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains("复制"))
    });
    let panel = panel_entity(&mut cx_a, &host_a);
    let base64 = cx_a
        .update(|_window, app| panel.read(app).exported_base64().map(str::to_owned))
        .expect("必须导出 base64");
    assert!(
        base64.starts_with("mn-approval-v1:"),
        "复制文本必须带版本前缀：{}",
        &base64[..20.min(base64.len())]
    );
    assert_eq!(
        ApprovalDraftV1::from_base64(&base64)
            .expect("base64 可解析")
            .envelope()
            .signatures()
            .len(),
        1
    );

    // B：从 base64 导入 → 加签 → 满签提交 → 已确认（回执 + 状态）。
    let log_b = Arc::new(Mutex::new(Vec::new()));
    let runner_b = FakeManagementRunner::new(0x22, log_b.clone()).with_vault(vault_b.clone());
    let (host_b, handle_b) = add_shell(
        cx,
        vault_b.clone(),
        "mgmt-flow-b",
        SyncContext::new(2, 1),
        None,
        Some(Box::new(runner_b)),
    );
    let mut cx_b = VisualTestContext::from_window(handle_b, &app_cx);
    cx_b.run_until_parked();
    wait_initial_refresh(&mut cx_b, &host_b);
    act(&mut cx_b, &host_b, |panel, cx| {
        panel.set_base64_text(&base64, cx);
        panel.request_import_base64(cx);
    });
    let snapshot = drive_management(&mut cx_b, &host_b, "B 导入", |snapshot| {
        snapshot.approval.is_some() && !snapshot.loading
    });
    let approval = snapshot.approval.clone().expect("B 导入后的草稿");
    // 导入视为"已由对方交付"：1 签 + handed_off = AwaitingCosign（B 仍可加签）。
    assert_eq!(approval.state, ApprovalStateV1::AwaitingCosign);
    assert!(approval.can_sign_local, "B（seed 0x22）应能加签");

    act(&mut cx_b, &host_b, |panel, cx| panel.request_sign(cx));
    let snapshot = drive_management(&mut cx_b, &host_b, "B 加签", |snapshot| {
        snapshot
            .approval
            .as_ref()
            .is_some_and(|approval| approval.fully_signed)
    });
    let approval = snapshot.approval.clone().expect("满签");
    assert_eq!(approval.state, ApprovalStateV1::Ready);
    assert_eq!(approval.signature_count, 2);
    assert!(!approval.can_sign_local, "满签后不能再签");

    act(&mut cx_b, &host_b, |panel, cx| panel.request_submit(cx));
    let snapshot = drive_management(&mut cx_b, &host_b, "提交", |snapshot| {
        snapshot
            .approval
            .as_ref()
            .is_some_and(|approval| approval.state == ApprovalStateV1::Confirmed)
    });
    let approval = snapshot.approval.clone().expect("已确认");
    assert_eq!(approval.state, ApprovalStateV1::Confirmed);
    let receipt = approval.receipt.expect("必须有回执");
    assert_eq!(receipt.control_rev, 8);
    assert_eq!(receipt.authz_rev, 4);
    assert!(
        snapshot
            .notice
            .as_deref()
            .unwrap_or_default()
            .contains("已提交"),
        "{snapshot:?}"
    );

    // 重启恢复：同一 vault + 新的执行体/面板只靠公开 meta 恢复草稿（A 侧仍是 1 签）。
    let log_r = Arc::new(Mutex::new(Vec::new()));
    let runner_r = FakeManagementRunner::new(0x21, log_r.clone()).with_vault(vault_a.clone());
    let (host_r, handle_r) = add_shell(
        cx,
        vault_a.clone(),
        "mgmt-flow-restart",
        SyncContext::new(1, 1),
        None,
        Some(Box::new(runner_r)),
    );
    let mut cx_r = VisualTestContext::from_window(handle_r, &app_cx);
    cx_r.run_until_parked();
    wait_initial_refresh(&mut cx_r, &host_r);
    let snapshot = drive_management(&mut cx_r, &host_r, "重启恢复", |snapshot| {
        snapshot.approval.is_some()
    });
    let approval = snapshot.approval.clone().expect("重启后草稿必须恢复");
    assert_eq!(
        approval.signature_count, 1,
        "A 的签名必须从 vault meta 恢复"
    );
    assert_eq!(approval.state, ApprovalStateV1::AwaitingCosign);
}

// ---------------------------------------------------------------- 4) 取消

#[gpui::test]
async fn cancel_clears_local_draft_only(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("cancel");
    let vault = dev_vault::open_vault_at_with_peer(&dir.path().join("vault.sqlite3"), 1)
        .expect("vault")
        .vault;
    let log = Arc::new(Mutex::new(Vec::new()));
    let runner = FakeManagementRunner::new(0x21, log).with_vault(vault.clone());
    let (host, handle) = add_shell(
        cx,
        vault.clone(),
        "mgmt-cancel",
        SyncContext::new(1, 1),
        None,
        Some(Box::new(runner)),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    wait_initial_refresh(&mut cx, &host);
    let fixture_path = dir.path().join("in.mn-approval");
    std::fs::write(&fixture_path, fixture_bytes()).expect("写 fixture");
    act(&mut cx, &host, |panel, cx| {
        panel.set_path_text(&fixture_path.display().to_string(), cx);
        panel.request_import_file(cx);
    });
    drive_management(&mut cx, &host, "导入", |snapshot| {
        snapshot.approval.is_some()
    });
    act(&mut cx, &host, |panel, cx| panel.request_sign(cx));
    drive_management(&mut cx, &host, "签署", |snapshot| {
        snapshot
            .approval
            .as_ref()
            .is_some_and(|approval| approval.signature_count == 1)
    });

    act(&mut cx, &host, |panel, cx| panel.request_cancel(cx));
    let snapshot = drive_management(&mut cx, &host, "取消", |snapshot| {
        snapshot.approval.is_none()
    });
    assert!(
        snapshot
            .notice
            .as_deref()
            .unwrap_or_default()
            .contains("取消"),
        "{snapshot:?}"
    );
    assert!(
        ApprovalDraftV1::load_draft(&vault)
            .expect("读草稿")
            .is_none(),
        "取消必须清掉本地草稿（只清协调状态）"
    );
    assert!(
        cx.debug_bounds("mgmt-approval-empty").is_some(),
        "取消后审批区必须显示空态"
    );
}

// ---------------------------------------------------------------- 5) 过期/失效/脱敏

#[gpui::test]
async fn expired_and_invalid_drafts_are_blocked_and_errors_are_sanitized(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("stale");
    let vault = dev_vault::open_vault_at_with_peer(&dir.path().join("vault.sqlite3"), 1)
        .expect("vault")
        .vault;
    let log = Arc::new(Mutex::new(Vec::new()));
    // fixture 的 not_after = 1_800_000_300；把"现在"推到之后 → 过期。
    let invalid = "控制链证据与本地已验证位置不符：/home/dev/secret/vault.sqlite3 \
                   https://relay.example/v2 mnrec1abcdef0123456789 \
                   mnaf0000000000000000000000000000000000000000000000000000000000000000";
    let runner = FakeManagementRunner::new(0x21, log.clone())
        .with_vault(vault.clone())
        .with_now(1_800_000_301)
        .with_invalid_reason(invalid);
    let (host, handle) = add_shell(
        cx,
        vault.clone(),
        "mgmt-stale",
        SyncContext::new(1, 1),
        None,
        Some(Box::new(runner)),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    wait_initial_refresh(&mut cx, &host);
    let fixture_path = dir.path().join("in.mn-approval");
    std::fs::write(&fixture_path, single_fixture_bytes()).expect("写 fixture");
    act(&mut cx, &host, |panel, cx| {
        panel.set_path_text(&fixture_path.display().to_string(), cx);
        panel.request_import_file(cx);
    });
    let snapshot = drive_management(&mut cx, &host, "导入过期包", |snapshot| {
        snapshot.approval.is_some()
    });
    let approval = snapshot.approval.clone().expect("草稿");
    assert!(approval.expired, "fixture 的短窗已过必须投影成 expired");
    let reason = approval.invalid_reason.clone().unwrap_or_default();
    assert!(
        !reason.contains("/home/dev") && !reason.contains("https://") && !reason.contains("mnrec1"),
        "失效原因必须脱敏：{reason}"
    );
    assert!(reason.contains("<路径已隐藏>"), "{reason}");
    assert!(reason.contains("<地址已隐藏>"), "{reason}");
    assert!(reason.contains("<恢复码已隐藏>"), "{reason}");

    let before = log.lock().expect("log").len();
    act(&mut cx, &host, |panel, cx| panel.request_sign(cx));
    let panel = panel_entity(&mut cx, &host);
    let snapshot = cx.update(|_window, app| panel.read(app).snapshot().clone());
    let notice = snapshot.notice.clone().unwrap_or_default();
    assert!(
        notice.contains("短窗") || notice.contains("失效"),
        "过期/失效必须被本地拦截并给出原因：{notice}"
    );
    act(&mut cx, &host, |panel, cx| panel.request_submit(cx));
    assert_eq!(
        log.lock().expect("log").len(),
        before,
        "被本地拦截时不得把 Sign/Submit 发给执行体（禁止自动改 rev/延 deadline）"
    );
}

#[gpui::test]
async fn refresh_error_is_sanitized_before_display(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("error");
    let vault = dev_vault::open_vault_at_with_peer(&dir.path().join("vault.sqlite3"), 1)
        .expect("vault")
        .vault;
    let log = Arc::new(Mutex::new(Vec::new()));
    let runner = FakeManagementRunner::new(0x21, log).with_submit_failure(
        "传输失败：读取 /home/dev/secret/vault.sqlite3 与 https://relay.example/v2 \
         token mnrec1abcdef0123456789",
    );
    let (host, handle) = add_shell(
        cx,
        vault,
        "mgmt-error",
        SyncContext::new(1, 1),
        None,
        Some(Box::new(runner)),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    wait_initial_refresh(&mut cx, &host);
    let fixture_path = dir.path().join("in.mn-approval");
    std::fs::write(&fixture_path, single_fixture_bytes()).expect("写 fixture");
    act(&mut cx, &host, |panel, cx| {
        panel.set_path_text(&fixture_path.display().to_string(), cx);
        panel.request_import_file(cx);
    });
    drive_management(&mut cx, &host, "导入", |snapshot| {
        snapshot.approval.is_some()
    });
    act(&mut cx, &host, |panel, cx| panel.request_sign(cx));
    drive_management(&mut cx, &host, "签署", |snapshot| {
        snapshot.approval.as_ref().is_some_and(|a| a.fully_signed)
    });
    act(&mut cx, &host, |panel, cx| panel.request_submit(cx));
    let snapshot = drive_management(&mut cx, &host, "提交失败", |snapshot| {
        snapshot.error.is_some()
    });
    let error = snapshot.error.clone().unwrap_or_default();
    assert!(!error.contains("/home/dev"), "错误不得泄露路径：{error}");
    assert!(!error.contains("https://"), "错误不得泄露 URL：{error}");
    assert!(!error.contains("mnrec1"), "错误不得泄露恢复码：{error}");
    assert!(
        error.contains("<路径已隐藏>") && error.contains("<地址已隐藏>"),
        "应保留脱敏标记：{error}"
    );
}

// ---------------------------------------------------------------- 6) 等待 B 不冻结编辑

#[gpui::test]
async fn awaiting_second_signature_does_not_hold_editing_or_sync_gate(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("nogate");
    let dev = dev_vault::open_vault_at_with_peer(&dir.path().join("vault.sqlite3"), 1)
        .expect("dev vault");
    let log = Arc::new(Mutex::new(Vec::new()));
    let runner = FakeManagementRunner::new(0x21, log);
    let (host, handle) = add_shell(
        cx,
        dev.vault.clone(),
        "mgmt-nogate",
        SyncContext::new(1, 1),
        None,
        Some(Box::new(runner)),
    );
    let app_cx = cx.clone();
    let mut cx = VisualTestContext::from_window(handle, &app_cx);
    cx.run_until_parked();
    wait_initial_refresh(&mut cx, &host);

    let fixture_path = dir.path().join("in.mn-approval");
    std::fs::write(&fixture_path, fixture_bytes()).expect("写 fixture");
    act(&mut cx, &host, |panel, cx| {
        panel.set_path_text(&fixture_path.display().to_string(), cx);
        panel.request_import_file(cx);
    });
    drive_management(&mut cx, &host, "导入", |snapshot| {
        snapshot.approval.is_some()
    });
    act(&mut cx, &host, |panel, cx| panel.request_sign(cx));
    let snapshot = drive_management(&mut cx, &host, "等待 B", |snapshot| {
        snapshot
            .approval
            .as_ref()
            .is_some_and(|approval| approval.state == ApprovalStateV1::InitiatorSigned)
    });
    assert_eq!(
        snapshot.approval.expect("草稿").state,
        ApprovalStateV1::InitiatorSigned
    );

    // 关掉设置覆盖层（等待 B 不应该要求设置页一直开着）。
    with_shell(&mut cx, &host, |shell, cx| shell.close_settings(cx));
    cx.run_until_parked();

    // 没有持闸、没有同步在途。
    let (held, sync_busy) = with_shell(&mut cx, &host, |shell, cx| {
        let held = shell
            .active_editor_view()
            .map(|view| view.read(cx).dev_sync_is_held())
            .unwrap_or(false);
        (held, shell.dev_sync_sync_in_progress())
    });
    assert!(!held, "等待第二签不得持有外部提交闸门");
    assert!(!sync_busy, "等待第二签不得占用同步在途位");

    // 编辑器仍可输入（焦点落在正文，走真实按键 → 文本输入处理器）。
    let before = with_shell(&mut cx, &host, |shell, app| {
        shell
            .active_editor_view()
            .map(|view| view.read(app).dev_sync_current_text())
            .unwrap_or_default()
    });
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("右侧编辑器面板必须画出 selector=editor-pane");
    cx.simulate_click(pane.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("z");
    cx.run_until_parked();
    let after = with_shell(&mut cx, &host, |shell, app| {
        shell
            .active_editor_view()
            .map(|view| view.read(app).dev_sync_current_text())
            .unwrap_or_default()
    });
    assert_ne!(before, after, "等待第二签时必须仍能编辑正文");
}

// ---------------------------------------------------------------- 7) AppRoot 设置页入口

#[gpui::test]
async fn app_root_settings_exposes_management_section(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let relay = DevRelay::start().expect("起进程内 dev relay 失败");
    let dir = temp_root("approot");
    let layout = ProfileLayout::new(
        dir.path().join("config/dev-sync.json"),
        dir.path().join("data/instances"),
    );
    let instance_id = "inst-mgmt-entry";
    let instance_dir = layout.instance_dir(instance_id).expect("实例目录");
    std::fs::create_dir_all(&instance_dir).expect("建实例目录");
    let vault_path = instance_dir.join(VAULT_FILE_NAME);
    let outcome = setup::prepare(
        &config_for(relay.base_url(), &vault_path),
        SetupMode::Create,
        SetupSecrets {
            setup_secret: Some(relay.setup_secret().to_owned()),
            ..SetupSecrets::default()
        },
    )
    .expect("Create 失败");

    let mut profile = DevSyncProfile::default();
    profile.upsert_active(InstanceEntry {
        instance_id: instance_id.to_owned(),
        relay_mode: RelayMode::LocalDev,
        api_version: RelayApiVersion::V1,
        relay_url: relay.base_url().to_owned(),
        vault_path: vault_path.display().to_string(),
        vault_id: menghunote_sync::ids::hex_of(&outcome.vault_id),
        device_id: menghunote_sync::ids::hex_of(&outcome.identity.device_id),
    });
    profile.active_instance = Some(instance_id.to_owned());
    layout.store().save(&profile).expect("写公开配置");
    drop(outcome);

    let options = AppRootOptions {
        layout,
        default_relay: relay.base_url().to_owned(),
        instance_id: Some(instance_id.to_owned()),
        allow_local_vault: false,
        operation_budget: DEFAULT_OPERATION_BUDGET,
    };
    let (root, cx) = cx.add_window_view(move |_window, cx| AppRoot::new(options, cx));
    cx.run_until_parked();
    root.update(cx, |root, cx| root.open_settings(cx));
    cx.run_until_parked();
    // 管理区在「安全」分类里（设置页左侧导航切换，右侧只渲染当前分类）。
    let nav = cx
        .debug_bounds("settings-nav-security")
        .expect("设置页必须有「安全」导航项");
    cx.simulate_click(nav.center(), Modifiers::default());
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("settings-toggle-management").is_some(),
        "真实 AppRoot 设置页必须有「管理」入口"
    );
    root.update(cx, |root, cx| root.settings_toggle_management(cx));
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("mgmt-panel").is_some(),
        "展开「管理」区后必须挂载管理面板"
    );

    let panel = root
        .update(cx, |root, app| {
            root.main_shell()
                .and_then(|shell| shell.read(app).management_panel())
        })
        .expect("AppRoot Main 里的管理面板必须可用");
    let deadline = Instant::now() + DEADLINE;
    loop {
        cx.executor().advance_clock(MANAGEMENT_PUMP_INTERVAL);
        cx.update(|_window, app| {
            panel.update(app, |panel, cx| {
                if panel.pump(cx) {
                    cx.notify();
                }
            });
        });
        cx.run_until_parked();
        let snapshot = cx.update(|_window, app| panel.read(app).snapshot().clone());
        if snapshot.last_refresh_unix.is_some() && !snapshot.loading {
            assert!(!snapshot.v2, "测试实例是 legacy vault");
            let message = snapshot.v2_message.clone().unwrap_or_default();
            assert!(
                message.contains("未启用 V2"),
                "真实设置页必须显示未启用 V2：{snapshot:?}"
            );
            assert!(snapshot.error.is_none(), "{snapshot:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "30s 内管理区没有刷新完：{snapshot:?}"
        );
        std::thread::yield_now();
    }
}
