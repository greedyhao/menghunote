//! 附件插入（dev-sync 构建）的 headless UI 测试：真实 `AppShell` + 临时 dev vault +
//! 真实临时文件（中文/空格路径），走**真实点击/按键**链路。
//!
//! 覆盖：
//! 1. 插入成功：正文出现 `[附件：<文件名>](asset://<hex>)`、`asset_refs` 有链接、
//!    `asset_bytes` 与文件字节一致；插入后 `Ctrl+Z` 一次可撤销，资产仍在；
//! 2. 重复插入同一文件幂等：单资产、两条引用（内容寻址语义）；
//! 3. 上限（20 MiB）/ 空文件 / 不存在 / 非普通文件 / 相对路径 / 非 UTF-8 文件名 →
//!    明确错误 + **不写 Markdown、不写引用、不写资产**；
//! 4. 无活动笔记 / 同步持闸 → 禁用并说明，提交也不产生任何写入。
//!
//! 纪律：临时目录在 `CARGO_TARGET_TMPDIR` 下；不 sleep（`run_until_parked` 是唯一收敛点）。

#![cfg(feature = "dev-sync")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    div, App, AppContext as _, Context, Entity, InteractiveElement as _, Modifiers,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
};
use menghunote_app::dev_vault::{self, DevVault};
use menghunote_app::shell::AppShell;
use menghunote_storage::session::{ExternalCommitTicket, GateState};
use menghunote_storage::{asset_bytes, asset_meta, list_asset_refs, list_assets, list_tree, Vault};
use tempfile::{Builder as TempDirBuilder, TempDir};

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

/// 在 `CARGO_TARGET_TMPDIR` 下开一个独立临时目录（AGENTS §6：不用 %TEMP%）。
fn temp_root(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("attachment-ui-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

fn open_temp_dev_vault() -> (TempDir, DevVault) {
    let dir = temp_root("vault");
    let dev = dev_vault::open_vault_at(&dir.path().join("vault.sqlite3"))
        .expect("打开临时 dev vault 失败");
    (dir, dev)
}

fn open_window<'a>(
    cx: &'a mut TestAppContext,
    vault: Vault,
    label: &'static str,
) -> (Entity<ShellHost>, &'a mut VisualTestContext) {
    cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, label.to_owned(), cx));
        ShellHost { shell }
    })
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

fn read_shell<R>(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    f: impl FnOnce(&AppShell, &App) -> R,
) -> R {
    cx.update(|_window, app| f(host.read(app).shell.read(app), app))
}

fn current_text(cx: &mut VisualTestContext, host: &Entity<ShellHost>) -> String {
    read_shell(cx, host, |shell, app| shell.dev_sync_current_text(app))
        .expect("当前活动文档的镜像正文")
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("selector={selector} 必须画出 headless 帧"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

fn focus_editor(cx: &mut VisualTestContext) {
    let pane = cx.debug_bounds("editor-pane").expect("编辑器面板 selector");
    cx.simulate_click(
        gpui::point(
            pane.origin.x + gpui::px(20.0),
            pane.origin.y + gpui::px(10.0),
        ),
        Modifiers::default(),
    );
    cx.run_until_parked();
}

/// 通过真实 UI 链路插入一个附件：展开（若收起）→ 点路径框取焦点 → 输入路径 → Enter。
fn insert_via_ui(cx: &mut VisualTestContext, host: &Entity<ShellHost>, path: &Path) {
    let open = read_shell(cx, host, |shell, _app| shell.attachment_is_open());
    if !open {
        click(cx, "attachment-insert");
    }
    let open = read_shell(cx, host, |shell, _app| shell.attachment_is_open());
    assert!(open, "点「插入附件…」必须展开路径输入");
    // 点输入框（真实命中 → 宿主把焦点交给它）：插入成功后输入框会被重建。
    click(cx, "attachment-path");
    cx.simulate_input(&path.to_string_lossy());
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
}

/// 一个真实临时文件：路径含中文、空格与 Markdown 定界符（`[]()`）。
fn make_attachment_file(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
    let nested = dir.join("附件 目录");
    std::fs::create_dir_all(&nested).expect("创建中文/空格目录失败");
    let path = nested.join(name);
    std::fs::write(&path, data).expect("写临时附件失败");
    path
}

fn first_note_ids(vault: &Vault) -> (String, String) {
    let entry = list_tree(vault)
        .expect("读结构树失败")
        .into_iter()
        .find(|entry| entry.is_note())
        .expect("必须有第一篇笔记");
    (
        entry.node_id.clone(),
        entry.note_id.clone().expect("笔记必须有 note_id"),
    )
}

/// 1) 插入成功 + 幂等重复 + undo 一次。
#[gpui::test]
async fn attachment_insert_writes_markdown_link_asset_and_can_undo(cx: &mut TestAppContext) {
    let (dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let (_node_id, note_id) = first_note_ids(&vault);
    let data = b"attachment payload for headless ui".to_vec();
    let path = make_attachment_file(dir.path(), "报告 最终[1].txt", &data);

    let (host, cx) = open_window(cx, vault.clone(), "attachment-ui-insert");
    cx.run_until_parked();
    let before = current_text(cx, &host);

    // --- 插入 ---
    insert_via_ui(cx, &host, &path);
    let error = read_shell(cx, &host, |shell, _app| {
        shell.attachment_error_text().map(str::to_owned)
    });
    assert_eq!(error, None, "插入失败：{error:?}");
    let notice = read_shell(cx, &host, |shell, _app| {
        shell.attachment_notice_text().map(str::to_owned)
    })
    .unwrap_or_else(|| panic!("成功必须写 attachment-notice"));
    assert!(
        notice.contains("报告 最终[1].txt") && notice.contains("字节"),
        "成功提示必须带文件名与字节数：{notice}"
    );

    let text = current_text(cx, &host);
    assert!(text.len() > before.len(), "正文必须变长");
    let refs = list_asset_refs(&vault, &note_id).expect("读 asset_refs 失败");
    assert_eq!(refs.len(), 1, "必须正好一条资产引用：{refs:?}");
    let asset_hash = refs[0].asset_hash;
    let start = usize::try_from(refs[0].start_byte).expect("start_byte 必须能转 usize");
    let expected_markdown = "[附件：报告 最终\\[1\\].txt](asset://";
    assert!(
        text[start..].starts_with(expected_markdown),
        "引用起点必须正好是链接开头（start={start}）：{:?}",
        text.chars().skip(start).take(48).collect::<String>()
    );
    assert_eq!(
        asset_bytes(&vault, asset_hash).expect("读资产失败"),
        Some(data.clone()),
        "资产字节必须与文件一致"
    );
    let meta = asset_meta(&vault, asset_hash)
        .expect("读资产元数据失败")
        .expect("资产必须存在");
    assert_eq!(meta.media_type.as_deref(), Some("text/plain"));
    assert!(meta.has_data, "资产必须有明文");
    assert_eq!(list_assets(&vault).expect("列资产失败").len(), 1);

    // --- 重复插入同一文件：单资产、两条引用（内容寻址 + 引用按位置） ---
    insert_via_ui(cx, &host, &path);
    let refs = list_asset_refs(&vault, &note_id).expect("读 asset_refs 失败");
    assert_eq!(refs.len(), 2, "重复插入必须新增一条引用：{refs:?}");
    assert!(
        refs.iter().all(|entry| entry.asset_hash == asset_hash),
        "重复插入必须复用同一资产（内容寻址）"
    );
    assert_eq!(
        list_assets(&vault).expect("列资产失败").len(),
        1,
        "重复插入不得产生第二个资产"
    );

    // --- undo 一次：最后一条插入被撤销，资产仍在（不删资产） ---
    focus_editor(cx);
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    let after_undo = current_text(cx, &host);
    assert_eq!(
        after_undo.matches("[附件：报告 最终\\[1\\].txt]").count(),
        1,
        "undo 一次必须撤掉最后一条插入（只剩第一条）：{:?}",
        after_undo.chars().take(80).collect::<String>()
    );
    assert_eq!(
        asset_bytes(&vault, asset_hash).expect("读资产失败"),
        Some(data),
        "undo 只撤正文，不得删除资产"
    );
    cx.simulate_keystrokes("ctrl-shift-z");
    cx.run_until_parked();
    assert_eq!(
        current_text(cx, &host)
            .matches("[附件：报告 最终\\[1\\].txt]")
            .count(),
        2,
        "redo 必须能恢复被撤销的那次插入"
    );
}

/// 2) 错误路径：明确错误 + 零写入（无 Markdown、无引用、无资产）。
#[gpui::test]
async fn attachment_rejections_leave_no_markdown_or_refs(cx: &mut TestAppContext) {
    let (dir, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let (_node_id, note_id) = first_note_ids(&vault);

    // 20 MiB + 1（稀疏文件：只 set_len，不真写 20MB 数据）。
    let too_large = dir.path().join("too-large.bin");
    {
        let file = std::fs::File::create(&too_large).expect("创建超大文件失败");
        file.set_len(20 * 1024 * 1024 + 1)
            .expect("设置文件长度失败");
    }
    let empty = dir.path().join("empty.txt");
    std::fs::write(&empty, b"").expect("写空文件失败");
    let missing = dir.path().join("not-there.bin");
    let directory = dir.path().join("a-directory");
    std::fs::create_dir_all(&directory).expect("创建目录失败");

    let (host, cx) = open_window(cx, vault.clone(), "attachment-ui-errors");
    cx.run_until_parked();
    let before_text = current_text(cx, &host);

    let cases: Vec<(PathBuf, &str)> = vec![
        (too_large, "超过"),
        (empty, "空文件"),
        (missing, "文件不存在"),
        (directory, "不是普通文件"),
        (PathBuf::from("relative.txt"), "绝对路径"),
        (PathBuf::from(""), "绝对路径"),
    ];
    for (path, expected) in cases {
        let raw = path.to_string_lossy().into_owned();
        with_shell(cx, &host, |shell, cx| {
            shell.attachment_submit_path(&raw, cx);
        });
        let error = read_shell(cx, &host, |shell, _app| {
            shell.attachment_error_text().map(str::to_owned)
        })
        .unwrap_or_else(|| panic!("{path:?} 必须写 attachment-error"));
        assert!(
            error.contains(expected),
            "{path:?} 的错误文案必须含 {expected:?}：{error}"
        );
        assert_eq!(
            read_shell(cx, &host, |shell, _app| shell
                .attachment_notice_text()
                .map(str::to_owned)),
            None,
            "失败时不得留下成功提示"
        );
        assert_eq!(
            current_text(cx, &host),
            before_text,
            "{path:?} 失败后正文不得变化（不插 Markdown）"
        );
        assert!(
            list_asset_refs(&vault, &note_id)
                .expect("读 asset_refs 失败")
                .is_empty(),
            "{path:?} 失败后不得留下引用"
        );
        assert!(
            list_assets(&vault).expect("列资产失败").is_empty(),
            "{path:?} 失败后不得留下资产"
        );
    }

    // 非 UTF-8 文件名（仅 Unix 可构造；Windows 用 UTF-16，不适用）。
    //
    // 输入框只能给 UTF-8 字符串，所以这一条走模块 API（`read_attachment` 接受真实
    // `Path`）——它必须在**任何写入之前**失败。
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let name = std::ffi::OsStr::from_bytes(b"bad-\xff-name.txt");
        let path = dir.path().join(name);
        std::fs::write(&path, b"x").expect("写非 UTF-8 文件名文件失败");
        let error = menghunote_app::attachment::read_attachment(&path)
            .expect_err("非 UTF-8 文件名必须报错");
        assert_eq!(error.category(), "non-utf8-file-name");
        assert!(
            error.to_string().contains("UTF-8"),
            "错误文案必须说明文件名不是 UTF-8：{error}"
        );
        assert!(list_assets(&vault).expect("列资产失败").is_empty());
    }
}

/// 3) 禁用态：没有活动笔记 / 同步持闸 → 说明原因且提交零写入。
#[gpui::test]
async fn attachment_disabled_without_active_note(cx: &mut TestAppContext) {
    // --- 没有活动笔记：空树库 ---
    let dir = temp_root("empty");
    let empty_vault = Vault::open(dir.path().join("empty.sqlite3")).expect("打开空临时 vault 失败");
    assert!(list_tree(&empty_vault).expect("读空树失败").is_empty());
    let (host, cx) = open_window(cx, empty_vault.clone(), "attachment-ui-no-note");
    cx.run_until_parked();
    let reason = read_shell(cx, &host, |shell, app| {
        shell.attachment_disabled_reason(app)
    })
    .expect("没有活动笔记时必须给出禁用原因");
    assert!(reason.contains("没有打开的笔记"), "{reason}");
    // 点按钮：不展开输入，但必须显示原因（可诊断，而不是静默无反应）。
    assert!(
        cx.debug_bounds("attachment-hint").is_some(),
        "禁用态必须常驻画出 attachment-hint 说明"
    );
    click(cx, "attachment-insert");
    assert!(
        !read_shell(cx, &host, |shell, _app| shell.attachment_is_open()),
        "禁用态点按钮不得展开路径输入"
    );
    assert!(
        cx.debug_bounds("attachment-error").is_some(),
        "禁用态点击必须画出 attachment-error 说明"
    );
    // 即使绕过 UI 直接提交：不写 Markdown、不写资产。
    let file = make_attachment_file(dir.path(), "x.txt", b"abc");
    with_shell(cx, &host, |shell, cx| {
        shell.attachment_submit_path(&file.to_string_lossy(), cx);
    });
    let error = read_shell(cx, &host, |shell, _app| {
        shell.attachment_error_text().map(str::to_owned)
    })
    .expect("直接提交也必须明确失败");
    assert!(error.contains("没有打开的笔记"), "{error}");
    assert!(list_assets(&empty_vault).expect("列资产失败").is_empty());
}

/// 4) 同步持闸（唯一"只读"窗口期）→ 禁用并说明，提交零写入。
#[gpui::test]
async fn attachment_disabled_during_sync_hold(cx: &mut TestAppContext) {
    // --- 同步持闸：有活动笔记但编辑器 Held ---
    let (_dir2, dev) = open_temp_dev_vault();
    let vault = dev.vault.clone();
    let (host, cx) = open_window(cx, vault.clone(), "attachment-ui-held");
    cx.run_until_parked();
    let ticket = with_shell(cx, &host, |shell, cx| shell.dev_sync_begin_external(cx))
        .expect("begin 外部提交");
    drive_to_held(cx, &host, &ticket);
    assert!(
        with_shell(cx, &host, |shell, app| shell
            .attachment_disabled_reason(app))
        .is_some_and(|reason| reason.contains("持闸")),
        "Held 期间必须给出「同步持闸」原因"
    );
    let before_text = current_text(cx, &host);
    let file = make_attachment_file(_dir2.path(), "held.txt", b"held-bytes");
    with_shell(cx, &host, |shell, cx| {
        shell.attachment_submit_path(&file.to_string_lossy(), cx);
    });
    let error = read_shell(cx, &host, |shell, _app| {
        shell.attachment_error_text().map(str::to_owned)
    })
    .expect("Held 期间提交必须明确失败");
    assert!(error.contains("持闸") || error.contains("冻结"), "{error}");
    assert_eq!(
        current_text(cx, &host),
        before_text,
        "Held 期间不得改动正文"
    );
    with_shell(cx, &host, |shell, cx| {
        shell.dev_sync_cancel_external(&ticket, cx)
    })
    .expect("cancel 外部提交");
}

/// 把闸门驱到 `Held`（明确收敛点，不 sleep）。
fn drive_to_held(
    cx: &mut VisualTestContext,
    host: &Entity<ShellHost>,
    ticket: &ExternalCommitTicket,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match with_shell(cx, host, |shell, cx| {
            shell.dev_sync_poll_external(ticket, cx)
        }) {
            GateState::Held(_) => return,
            GateState::Flushing => std::thread::yield_now(),
            GateState::WaitingIME => panic!("没有 composition 时不应 WaitingIME"),
            other => panic!("闸门未收敛到 Held：{other:?}"),
        }
        assert!(Instant::now() < deadline, "闸门 10s 内没有进入 Held");
    }
}
