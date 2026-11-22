//! dev-sync 持久设备身份与 peer 注入（ADR 0009 切片 2a）。
//!
//! 覆盖四件事（全部读**落库的 CRDT 版本向量**或**文件字节**，不只看结构体字段）：
//!
//! 1. 同一实例路径重开身份 → peer 不变；两个独立临时实例 → peer 不同；
//! 2. 身份文件是私有文件（Linux 0600 / nlink=1）且密文里没有私钥明文；
//! 3. header / 密文损坏 → 明确报错且**不覆盖、不迁移、不删除**；
//! 4. 真实 `AppShell`（dev-sync 启用）的结构写入与会话 flush 实际用身份 peer
//!    （落库的 `VersionVector` 里只有身份 peer、没有默认常量 1）。
//!
//! 纪律（AGENTS.md §6）：临时目录一律在 `CARGO_TARGET_TMPDIR` 下；Linux 用户主键的
//! `XDG_DATA_HOME` 也指到 `target/tmp`，**不碰** `~/.local/share`。收敛点只有
//! `run_until_parked()` 与生产自己的关窗排空（`close_all`），不 sleep。

#![cfg(feature = "dev-sync")]

use std::path::Path;

use gpui::{
    div, AppContext as _, Entity, InteractiveElement as _, Modifiers, MouseButton,
    ParentElement as _, Render, Styled as _, TestAppContext,
};
use menghunote_app::dev_sync::gate::DevSyncConfig;
use menghunote_app::dev_sync::service::ServiceConfig;
use menghunote_app::dev_sync::DevIdentityStore;
use menghunote_app::dev_vault;
use menghunote_app::shell::AppShell;
use menghunote_storage::{list_tree, load_body_doc, load_head, STRUCTURE_DOC_ID};
use tempfile::{Builder as TempDirBuilder, TempDir};

/// 测试进程级的 `XDG_DATA_HOME`（Linux 用户主键；Windows 是 no-op）。
///
/// 用 `Once` 保证只设一次：`cargo test` 多线程下并发 `set_var` 既不安全也没必要。
/// 目录必须落在 `target/tmp`（AGENTS §6），否则会在开发者家目录里创建主键文件。
fn ensure_test_user_key() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("dev-identity-xdg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("创建测试 XDG_DATA_HOME 失败");
        std::env::set_var("XDG_DATA_HOME", &dir);
        menghunote_sync::identity_wrap::initialize_user_key()
            .expect("初始化测试用户主键失败（ADR 0008）");
    });
}

/// 在 `CARGO_TARGET_TMPDIR`（`target/tmp`）下开一个独立临时目录（**不用** `%TEMP%`）。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("dev-identity-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("vault-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 按字节修改一份夹具（损坏用例用）。
fn mutate(bytes: &[u8], edit: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = bytes.to_vec();
    edit(&mut out);
    out
}

// ---------------------------------------------------------------------------
// 1) 同一路径稳定 / 独立实例唯一
// ---------------------------------------------------------------------------

#[test]
fn identity_is_stable_per_instance_and_unique_across_instances() {
    ensure_test_user_key();
    let dir_a = temp_root("stable-a");
    let vault_a = dir_a.path().join("vault.sqlite3");

    let first = DevIdentityStore::load_or_create(&vault_a).expect("首次创建 dev identity");
    let file = DevIdentityStore::path_for(&vault_a);
    assert_eq!(
        file,
        dir_a.path().join("dev-identity.bin"),
        "身份文件与 vault 同目录"
    );
    assert!(file.is_file(), "identity 文件必须已落盘");
    let bytes = std::fs::read(&file).expect("读取身份文件");
    // 明文 header：magic + 版本（其余是 id 与密文长度）。
    assert_eq!(&bytes[..4], b"MNID", "header magic 必须是 MNID");
    assert_eq!(bytes[4], 1, "header 版本必须是 1");
    // 密文里不得出现任何私钥/根明文（header 只放非秘密 id）。
    for (name, secret) in [
        ("ed25519_seed", &first.ed25519_seed),
        ("x25519_secret", &first.x25519_secret),
        ("current_root", &first.current_root),
    ] {
        assert!(
            !bytes
                .windows(secret.expose_slice().len())
                .any(|window| window == secret.expose_slice()),
            "身份文件泄露了 {name} 的明文"
        );
    }

    // 重开：同 peer、文件**逐字节**不变（加载路径只读）。
    let peer = first.loro_peer_id();
    assert_ne!(peer, 0, "0 不是合法 Loro peer");
    let second = DevIdentityStore::load_or_create(&vault_a).expect("重载 dev identity");
    assert_eq!(second.loro_peer_id(), peer, "重开必须得到同一 peer");
    assert_eq!(second.device_id, first.device_id);
    assert_eq!(
        std::fs::read(&file).expect("重开后再读"),
        bytes,
        "加载已有身份不得改写文件"
    );

    // 两个独立临时实例（各自库目录）：peer 必须不同。
    let dir_b = temp_root("stable-b");
    let other =
        DevIdentityStore::load_or_create(&dir_b.path().join("vault.sqlite3")).expect("第二个实例");
    assert_ne!(
        other.loro_peer_id(),
        peer,
        "两个实例不得共用同一 peer（会撞 CRDT）"
    );
}

// ---------------------------------------------------------------------------
// 2) 文件权限
// ---------------------------------------------------------------------------

#[test]
fn identity_file_is_private() {
    ensure_test_user_key();
    let dir = temp_root("perms");
    let vault_path = dir.path().join("vault.sqlite3");
    DevIdentityStore::load_or_create(&vault_path).expect("创建 dev identity");
    let file = DevIdentityStore::path_for(&vault_path);
    let meta = std::fs::symlink_metadata(&file).expect("lstat 身份文件");
    assert!(meta.file_type().is_file(), "身份文件必须是常规文件");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "身份文件必须 0600（只有当前用户可读写）"
        );
        assert_eq!(meta.nlink(), 1, "临时文件名必须被清掉（硬链接数回到 1）");
    }
}

// ---------------------------------------------------------------------------
// 3) 损坏 → 明确报错且不覆盖
// ---------------------------------------------------------------------------

#[test]
fn corrupt_identity_files_fail_without_being_overwritten() {
    ensure_test_user_key();
    let dir = temp_root("corrupt");
    let vault_path = dir.path().join("vault.sqlite3");
    let identity = DevIdentityStore::load_or_create(&vault_path).expect("创建 dev identity");
    let peer = identity.loro_peer_id();
    let file = DevIdentityStore::path_for(&vault_path);
    let pristine = std::fs::read(&file).expect("读取原始身份文件");
    assert!(pristine.len() > 64, "夹具太短，无法覆盖各类损坏");

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("magic 被改", mutate(&pristine, |b| b[0] ^= 0x01)),
        ("版本被改", mutate(&pristine, |b| b[4] = 99)),
        (
            "header device_id 被改",
            mutate(&pristine, |b| b[21] ^= 0xff),
        ),
        ("密文尾字节被改", {
            mutate(&pristine, |b| {
                let last = b.len() - 1;
                b[last] ^= 0x01;
            })
        }),
        ("截断尾部", pristine[..pristine.len() - 1].to_vec()),
        ("尾随多余字节", {
            let mut long = pristine.clone();
            long.push(0);
            long
        }),
    ];

    for (label, bytes) in cases {
        std::fs::write(&file, &bytes).expect("写入损坏夹具");
        let error = DevIdentityStore::load_or_create(&vault_path)
            .err()
            .unwrap_or_else(|| panic!("{label} 必须被明确拒绝，实际加载成功"));
        let message = format!("{error:#}");
        assert!(
            message.contains("dev identity") || message.contains("identity"),
            "{label}：错误信息必须指明身份文件：{message}"
        );
        // 不覆盖、不迁移、不删除：失败路径后文件必须逐字节不变。
        assert_eq!(
            std::fs::read(&file).expect("失败后再读"),
            bytes,
            "{label}：加载失败不得改写/删除身份文件"
        );
    }

    // 恢复原文件 → 仍能加载出同一 peer（前面的失败没有破坏任何东西）。
    std::fs::write(&file, &pristine).expect("恢复原始身份文件");
    let reopened = DevIdentityStore::load_or_create(&vault_path).expect("恢复后加载");
    assert_eq!(reopened.loro_peer_id(), peer);
}

// ---------------------------------------------------------------------------
// 4) 真实 AppShell（dev-sync 启用）：session / structure 实际 peer = 身份 peer
// ---------------------------------------------------------------------------

/// 窗口根视图：给 `AppShell` 一个稳定的宿主 selector（与 headless_ui.rs 同构）。
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

#[gpui::test]
async fn app_shell_uses_identity_peer_for_structure_and_session(cx: &mut TestAppContext) {
    ensure_test_user_key();
    let dir = temp_root("shell");
    let vault_path = dir.path().join("vault.sqlite3");

    // 1) 身份先于库：peer 只能来自持久身份。
    let identity = DevIdentityStore::load_or_create(&vault_path).expect("dev identity");
    let peer = identity.loro_peer_id();
    assert_ne!(
        peer,
        dev_vault::DEV_PEER_ID,
        "随机身份恰好等于常量 1 时本测试没有判别力（概率 ~2^-63）"
    );

    // 2) 用身份 peer 播种实例库。
    let dev = dev_vault::open_vault_at_with_peer(&vault_path, peer).expect("打开实例库");
    assert!(dev.seeded, "空实例库必须播种");
    let tree = list_tree(&dev.vault).expect("读结构树");
    assert_eq!(tree.len(), 3, "demo 树必须 3 个节点");
    let first_doc = tree[0]
        .body_doc_id
        .clone()
        .expect("第一篇笔记必须有 body_doc_id");

    // 3) 真实 AppShell（dev-sync 启用路径，显式注入身份 peer + epoch）。
    let vault = dev.vault.clone();
    let config = DevSyncConfig {
        relay_mode: menghunote_app::onboarding::RelayMode::LocalDev,
        vault: vault_path.clone(),
        service: ServiceConfig::new("http://127.0.0.1:18080"),
    };
    // 切片 2b：AppShell 接收的是完整 SyncContext（peer + 加密 epoch），不再只有 peer。
    let sync = menghunote_storage::SyncContext::new(peer, identity.current_epoch);
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| {
            AppShell::new_with_dev_sync(vault, "dev-identity-peer".to_owned(), config, sync, cx)
        });
        ShellHost { shell }
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("dev-sync-panel").is_some(),
        "dev-sync 必须真的启用（面板已挂载）"
    );

    // 4) 会话路径：点正文 → 键入一个字母（真实输入通道 → 会话 flush）。
    //
    // 先记下 durable seq：播种本身已经带身份 peer，若不确认"本次又写了新 op"，
    // 下面的 VV 断言就没有判别力（新 op 必须让 head_seq 前进）。
    let body_seq_before = load_head(&dev.vault, &first_doc)
        .expect("读正文 head")
        .head_seq;
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出 selector=editor-pane");
    cx.simulate_click(pane.center(), Modifiers::default());
    cx.run_until_parked();
    cx.simulate_keystrokes("a");
    cx.run_until_parked();

    // 5) 结构路径：右键目录行 →「新建笔记」（AppShell::write_structure → commit）。
    let structure_seq_before = load_head(&dev.vault, STRUCTURE_DOC_ID)
        .expect("读结构 head")
        .head_seq;
    let folder = cx
        .debug_bounds("tree-row-1")
        .expect("tree-row-1 必须是目录行");
    cx.simulate_mouse_down(folder.center(), MouseButton::Right, Modifiers::default());
    cx.run_until_parked();
    let item = cx
        .debug_bounds("menu-item-1")
        .expect("目录菜单必须画出「新建笔记」");
    cx.simulate_click(item.center(), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        list_tree(&dev.vault).expect("读树").len(),
        4,
        "「新建笔记」必须让结构树从 3 项变 4 项"
    );

    // 6) 生产关窗路径做有界排空（不 sleep）。
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();

    // 7) 断言落库 CRDT 的版本向量：写入的 op 只用身份 peer，绝不含常量 1。
    let (doc, body_head) =
        load_body_doc(&dev.vault, &first_doc, peer, None).expect("重放第一篇正文");
    assert!(
        body_head.head_seq > body_seq_before,
        "键入必须真的让正文 durable seq 前进（否则下面的 peer 断言没有判别力）：\
         {} → {}",
        body_seq_before,
        body_head.head_seq
    );
    assert!(
        doc.text().chars().count() > 100,
        "键入后正文必须仍有完整内容（实际 {} 字符）",
        doc.text().chars().count()
    );
    let body_peers: Vec<u64> = body_head
        .version_vector()
        .expect("解码正文 head_vv")
        .keys()
        .copied()
        .collect();
    assert!(
        body_peers.contains(&peer),
        "会话 flush 必须用身份 peer 产生 op，实际 VV peers = {body_peers:?}"
    );
    assert!(
        !body_peers.contains(&dev_vault::DEV_PEER_ID),
        "会话不得再用默认常量 1，实际 VV peers = {body_peers:?}"
    );

    let structure_head = load_head(&dev.vault, STRUCTURE_DOC_ID).expect("读结构 head");
    assert!(
        structure_head.head_seq > structure_seq_before,
        "「新建笔记」必须真的让结构 durable seq 前进（否则 peer 断言没有判别力）：\
         {} → {}",
        structure_seq_before,
        structure_head.head_seq
    );
    let structure_peers: Vec<u64> = structure_head
        .version_vector()
        .expect("解码结构 head_vv")
        .keys()
        .copied()
        .collect();
    assert!(
        structure_peers.contains(&peer),
        "结构写入必须用身份 peer，实际 VV peers = {structure_peers:?}"
    );
    assert!(
        !structure_peers.contains(&dev_vault::DEV_PEER_ID),
        "结构写入不得再用默认常量 1，实际 VV peers = {structure_peers:?}"
    );
}

// ---------------------------------------------------------------------------
// 5) 轮换身份写回（ADR 0009 Phase 2 切片 6a）：原子替换 + 同 vault/device 约束
// ---------------------------------------------------------------------------

/// `DevIdentityStore::replace_protected` 的契约：
///
/// - 只允许**同 vault / 同 device** 的身份替换（轮换/apply 只推进 epoch/root/pin）；
/// - 成功：MNID 外层不变、可重载、新字节真的落盘、目录里没有临时文件残留；
/// - 失败（不同 device/vault、目标缺失/损坏）：**旧文件逐字节不变**，不半写。
#[test]
fn replace_protected_is_atomic_and_only_accepts_same_vault_device() {
    ensure_test_user_key();
    let dir = temp_root("replace");
    let vault_path = dir.path().join("vault.sqlite3");
    let store = DevIdentityStore;
    let base = DevIdentityStore::load_or_create(&vault_path).expect("创建 dev identity 失败");
    let path = DevIdentityStore::path_for(&vault_path);
    let before = std::fs::read(&path).expect("读身份文件失败");

    // 同 vault/device、只推进 epoch/control_rev（轮换/apply 之后就是这种形状）。
    // `LocalIdentityV1` 刻意不实现 `Clone`（秘密不复制）：从磁盘再解封一份。
    let mut promoted = store.load(&vault_path).expect("重新解封身份失败");
    promoted.current_epoch = base.current_epoch + 1;
    promoted.control_rev = base.control_rev + 3;
    store
        .replace_protected(&path, &promoted)
        .expect("同 vault/device 的替换必须成功");

    let after = std::fs::read(&path).expect("替换后读身份文件失败");
    assert_ne!(before, after, "新身份必须真的写进身份文件");
    assert_eq!(&after[..4], b"MNID", "MNID 外层格式不得变化");
    assert_eq!(after[4], 1, "header 版本不得变化");
    let reloaded = store.load(&vault_path).expect("替换后的身份必须可重载");
    assert_eq!(reloaded.current_epoch, promoted.current_epoch);
    assert_eq!(reloaded.control_rev, promoted.control_rev);
    assert_eq!(reloaded.device_id, base.device_id);
    assert_eq!(
        reloaded.loro_peer_id(),
        base.loro_peer_id(),
        "替换不得改变 peer（peer 是设备的稳定映射）"
    );
    assert!(
        reloaded.current_root.ct_eq(&base.current_root),
        "本用例只换 epoch 元数据，根不变"
    );

    // 失败路径都不能碰旧文件；成功路径不留临时文件。
    for entry in std::fs::read_dir(dir.path()).expect("读实例目录失败") {
        let name = entry
            .expect("目录项")
            .file_name()
            .to_string_lossy()
            .into_owned();
        assert!(
            !name.starts_with(".dev-identity.bin"),
            "身份替换不得残留临时文件：{name}"
        );
    }

    // 换设备：拒绝，且旧文件逐字节不变。
    let mut other_device = store.load(&vault_path).expect("重新解封身份失败");
    other_device.device_id = [0xab; 16];
    let error = store
        .replace_protected(&path, &other_device)
        .expect_err("不同 device 的替换必须拒绝");
    assert!(
        error.to_string().contains("vault/device"),
        "拒绝原因必须说清同 vault/device 约束：{error}"
    );
    assert_eq!(
        std::fs::read(&path).expect("拒绝后读身份文件失败"),
        after,
        "拒绝替换时旧文件必须逐字节不变"
    );

    // 换库：同样拒绝。
    let mut other_vault = store.load(&vault_path).expect("重新解封身份失败");
    other_vault.vault_id = [0xcd; 16];
    let error = store
        .replace_protected(&path, &other_vault)
        .expect_err("不同 vault 的替换必须拒绝");
    assert!(
        error.to_string().contains("vault/device"),
        "拒绝原因必须说清同 vault/device 约束：{error}"
    );
    assert_eq!(
        std::fs::read(&path).expect("拒绝后读身份文件失败"),
        after,
        "拒绝替换时旧文件必须逐字节不变"
    );

    // 目标不存在：替换语义不负责创建（Create/Join 才创建身份）。
    let missing = dir.path().join("missing-identity.bin");
    let error = store
        .replace_protected(&missing, &promoted)
        .expect_err("目标不存在必须拒绝");
    assert!(
        error.to_string().contains("不存在"),
        "错误必须说清目标缺失：{error}"
    );
    assert!(!missing.exists(), "拒绝不得凭空创建身份文件");

    // 损坏的 header：拒绝，旧文件保留。
    let corrupt_path = dir.path().join("corrupt-identity.bin");
    std::fs::write(&corrupt_path, b"NOT-MNID").expect("写损坏身份失败");
    let error = store
        .replace_protected(&corrupt_path, &promoted)
        .expect_err("损坏身份必须拒绝替换");
    assert!(
        error.to_string().contains("header") || error.to_string().contains("magic"),
        "错误必须点明 header/magic 损坏：{error}"
    );
    assert_eq!(
        std::fs::read(&corrupt_path).expect("读损坏身份失败"),
        b"NOT-MNID",
        "拒绝替换不得改写损坏文件（不覆盖、不迁移、不删除）"
    );
}
