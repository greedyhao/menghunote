//! ADR 0019 S2b：粘贴/拖放导入 + 资产入库 + 正文插入 + 引用对账（headless 集成测试）。
//!
//! 真实 `AppShell` + 真实临时 vault（`CARGO_TARGET_TMPDIR`，不 sleep）+ 真实临时文件，
//! 走**真实按键**（`Ctrl+V` / `Ctrl+Z`）与**真实剪贴板条目**（`ClipboardItem::new_image` /
//! `ClipboardEntry::ExternalPaths`）。覆盖：
//!
//! 1. 剪贴板 PNG → `![粘贴图片.png](asset://<hash>)`，资产字节可读，S2a 图片对象 Ready；
//! 2. 同图重复粘贴 → 单资产、两条引用；
//! 3. 多文件（图片 + 非图片）一次导入 → **单事务**（一次 Ctrl+Z 全撤）、顺序正确；
//! 4. 超限/目录/图片损坏 → 明确提示，**正文与引用都不动**；
//! 5. 普通文本 Ctrl+V 仍按文本插入；`Ctrl+Shift+V` 强制文本（不把文件路径写进正文）；
//! 6. IME 组合中不插入；
//! 7. 引用对账：移动/删除链接后 `start_byte` 正确、重复对账幂等、落库后可复算；
//! 8. SVG 按普通附件（不执行、不当图片对象）。
//!
//! 未覆盖（真机项，见 ADR 0019 §8）：Windows CF_DIB/CF_DIBV5 剪贴板位图、真实
//! `IDropTarget` 文件拖放像素/交互、DPI 与真实窗口焦点。

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{
    div, AppContext as _, Entity, ExternalPaths, FileDropEvent, Image, ImageFormat,
    InteractiveElement as _, Modifiers, ParentElement as _, Styled as _, TestAppContext,
    VisualTestContext,
};
use menghunote_app::attachment::asset_refs_in_source;
use menghunote_app::editor::theme;
use menghunote_app::editor::view::{EditorView, ObjectImageState};
use menghunote_app::shell::AppShell;
use menghunote_storage::{
    append_updates, asset_bytes, asset_content_hash, asset_meta, insert_asset, list_asset_refs,
    list_assets, list_tree, load_body_doc, StructureWriter, UpdateRecord, Vault,
};
use tempfile::{Builder as TempDirBuilder, TempDir};

const PLACEHOLDER: &str = menghunote_editor::OBJECT_PLACEHOLDER;

// ---------------------------------------------------------------------------
// 脚手架
// ---------------------------------------------------------------------------

/// 6×3 的纯色 PNG（解码后宽 6 高 3）。
fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let buffer = image::RgbaImage::from_pixel(width, height, image::Rgba([200, 60, 40, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("测试 PNG 编码失败");
    out.into_inner()
}

fn temp_vault_dir(prefix: &str) -> TempDir {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("headless-import-{prefix}-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("创建 target/tmp 下的测试根目录失败");
    TempDirBuilder::new()
        .prefix("case-")
        .tempdir_in(&root)
        .expect("在 target/tmp 下创建临时目录失败")
}

/// 在空 vault 里建一篇正文（结构 CRDT → 投影 → 正文 A 事务）。
fn seed_note(vault: &Vault, title: &str, text: &str) -> String {
    let mut writer = StructureWriter::open(vault, 1).expect("打开结构写入口失败");
    let seed = writer
        .doc()
        .create_note(title, 1)
        .expect("结构 CRDT 新建笔记失败");
    writer.commit("headless-import-seed").expect("结构提交失败");

    let (doc, head) = load_body_doc(vault, &seed.body_doc_id, 1, None).expect("加载正文文档失败");
    if !text.is_empty() {
        doc.insert_scalars(0, text).expect("写入正文失败");
    }
    let base = head.version_vector().expect("解码 head VV 失败");
    let update = doc.pending_update(&base).expect("导出正文增量失败");
    append_updates(
        vault,
        &seed.body_doc_id,
        head.head_seq,
        &[UpdateRecord::new("headless-import", update)],
    )
    .expect("A 事务写入正文失败");
    seed.body_doc_id
}

fn note_id_of_doc(vault: &Vault, doc_id: &str) -> String {
    list_tree(vault)
        .expect("读结构树失败")
        .into_iter()
        .find(|entry| entry.body_doc_id.as_deref() == Some(doc_id))
        .and_then(|entry| entry.note_id)
        .expect("笔记必须有 note_id")
}

fn hash_hex(hash: &[u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("写临时文件失败");
    path
}

struct ShellHost {
    shell: Entity<AppShell>,
}

impl gpui::Render for ShellHost {
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

fn open_shell<'a>(
    cx: &'a mut TestAppContext,
    vault: Vault,
    label: &'static str,
) -> (Entity<ShellHost>, &'a mut VisualTestContext) {
    let (host, cx) = cx.add_window_view(move |_, cx| {
        let shell = cx.new(|cx| AppShell::new(vault, label.to_owned(), cx));
        ShellHost { shell }
    });
    cx.run_until_parked();
    (host, cx)
}

fn close_shell(host: &Entity<ShellHost>, cx: &mut VisualTestContext) {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.close_all(cx));
    });
    cx.run_until_parked();
}

fn active_view(host: &Entity<ShellHost>, cx: &mut VisualTestContext) -> Entity<EditorView> {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell
            .read(app)
            .active_editor_view()
            .expect("打开笔记后必须有活动编辑器视图")
    })
}

fn source_of(view: &Entity<EditorView>, cx: &mut VisualTestContext) -> String {
    cx.update(|_window, app| view.read(app).source_text())
}

fn notice_of(
    host: &Entity<ShellHost>,
    cx: &mut VisualTestContext,
) -> (Option<String>, Option<String>) {
    cx.update(|_window, app| {
        let shell = host.read(app).shell.read(app);
        (
            shell.import_error_text().map(str::to_owned),
            shell.import_notice_text().map(str::to_owned),
        )
    })
}

/// 等后台解码收敛 + 强制一帧（让 prepaint 用新的图片尺寸重算几何）。
fn settle_images(view: &Entity<EditorView>, cx: &mut VisualTestContext) {
    let settled = cx.update(|_window, app| {
        view.update(app, |view, cx| {
            view.settle_images(Duration::from_secs(10), cx)
        })
    });
    assert!(settled, "图片解码必须在有界时间内收敛");
    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    cx.simulate_mouse_move(
        gpui::point(
            pane.origin.x + gpui::px(theme::PAD_X + 4.0),
            pane.origin.y + gpui::px(theme::PAD_Y + 4.0),
        ),
        None,
        Modifiers::default(),
    );
    cx.run_until_parked();
}

fn clipboard_image(png: &[u8]) -> gpui::ClipboardItem {
    gpui::ClipboardItem::new_image(&Image {
        format: ImageFormat::Png,
        bytes: png.to_vec(),
        id: 1,
    })
}

fn clipboard_files(paths: &[PathBuf]) -> gpui::ClipboardItem {
    gpui::ClipboardItem {
        entries: vec![gpui::ClipboardEntry::ExternalPaths(ExternalPaths(
            paths.iter().cloned().collect(),
        ))],
    }
}

// ---------------------------------------------------------------------------
// 1) 剪贴板 PNG → 资产 + 正文链接 + S2a 图片对象 Ready
// ---------------------------------------------------------------------------

#[gpui::test]
async fn paste_png_from_clipboard_stores_asset_and_projects_image_object(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("png");
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let probe = vault.clone();
    let doc_id = seed_note(&vault, "粘贴图片", "");
    let note_id = note_id_of_doc(&vault, &doc_id);

    let (host, cx) = open_shell(cx, vault, "headless-import-png");
    let view = active_view(&host, cx);

    let png = png_bytes(6, 3);
    let hash = asset_content_hash(&png);
    let uri = format!("asset://{}", hash_hex(&hash));
    cx.write_to_clipboard(clipboard_image(&png));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();

    let source = source_of(&view, cx);
    assert_eq!(
        source,
        format!("![粘贴图片.png]({uri})"),
        "剪贴板图片必须写成图片 Markdown（不能把二进制当文本插）"
    );
    // 资产字节可读（内容寻址）。
    assert_eq!(
        asset_bytes(&probe, hash).expect("读资产字节失败"),
        Some(png.clone())
    );
    let meta = asset_meta(&probe, hash)
        .expect("读资产元数据失败")
        .expect("资产必须存在");
    assert_eq!(meta.media_type.as_deref(), Some("image/png"));
    // 引用：一条，定位点 = `asset://` 前缀在正文里的字节偏移（与对账扫描同一约定）。
    let refs = list_asset_refs(&probe, &note_id).expect("列引用失败");
    assert_eq!(refs.len(), 1, "{refs:?}");
    assert_eq!(refs[0].asset_hash, hash);
    assert_eq!(refs[0].start_byte, "![粘贴图片.png](".len() as u64);
    assert_eq!(
        refs[0].start_byte, 20,
        "UTF-8 字节偏移：`![粘贴图片.png](` 是 20 字节"
    );
    // 引用集合必须与"扫描正文"一致。
    assert_eq!(
        list_asset_refs(&probe, &note_id)
            .unwrap()
            .into_iter()
            .map(|entry| (entry.asset_hash, entry.start_byte))
            .collect::<Vec<_>>(),
        asset_refs_in_source(&source)
    );
    // 成功提示行（不显示错误）。
    let (error, notice) = notice_of(&host, cx);
    assert_eq!(error, None);
    assert!(notice.expect("必须有成功提示").contains("1 个附件"));

    // S2a：整行独占图片 → 对象占位 + 解码 Ready。
    settle_images(&view, cx);
    let display = cx.update(|_window, app| view.read(app).display_text());
    assert!(display.contains(PLACEHOLDER), "图片必须是对象：{display:?}");
    assert!(!display.contains("![粘贴图片.png]"), "{display:?}");
    let snapshots = cx.update(|_window, app| view.read(app).object_snapshots());
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert_eq!(snapshots[0].state, ObjectImageState::Ready);
    assert_eq!(snapshots[0].span, (0, source.len()));

    close_shell(&host, cx);
    // 落库校验：正文与引用都持久化。
    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (doc, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    assert_eq!(doc.text(), source);
    assert_eq!(list_asset_refs(&vault, &note_id).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// 2) 同图重复粘贴 → 单资产、两条引用
// ---------------------------------------------------------------------------

#[gpui::test]
async fn pasting_the_same_image_twice_keeps_one_asset_and_two_refs(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("dedupe");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let probe = vault.clone();
    let doc_id = seed_note(&vault, "重复粘贴", "");
    let note_id = note_id_of_doc(&vault, &doc_id);

    let (host, cx) = open_shell(cx, vault, "headless-import-dedupe");
    let view = active_view(&host, cx);
    let png = png_bytes(4, 2);
    let hash = asset_content_hash(&png);
    let mark = format!("![粘贴图片.png](asset://{})", hash_hex(&hash));

    cx.write_to_clipboard(clipboard_image(&png));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    // 第二次：caret 已在第一段之后，插在同一行形成两张图（都是内联，不影响引用账本）。
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();

    let source = source_of(&view, cx);
    assert_eq!(source, format!("{mark}{mark}"), "{source:?}");
    assert_eq!(
        list_assets(&probe).expect("列资产失败").len(),
        1,
        "同字节只写一个 asset"
    );
    let refs = list_asset_refs(&probe, &note_id).expect("列引用失败");
    assert_eq!(refs.len(), 2, "重复插入 = 多条引用：{refs:?}");
    let uri_offset = mark.find("asset://").expect("含 asset://") as u64;
    assert_eq!(refs[0].start_byte, uri_offset);
    assert_eq!(refs[1].start_byte, uri_offset + mark.len() as u64);
    assert!(refs.iter().all(|entry| entry.asset_hash == hash));

    close_shell(&host, cx);
}

// ---------------------------------------------------------------------------
// 3) 多文件一次导入 = 单事务（一次撤销全撤）+ 顺序正确
// ---------------------------------------------------------------------------

#[gpui::test]
async fn multi_file_paste_is_one_transaction_and_keeps_order(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("multi");
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let probe = vault.clone();
    let doc_id = seed_note(&vault, "多文件", "");
    let note_id = note_id_of_doc(&vault, &doc_id);

    // 图片 + 文本 + PDF：顺序必须是文件顺序。
    let image = png_bytes(3, 2);
    let text = "附件内容\n".as_bytes();
    let pdf = b"%PDF-1.4 test".to_vec();
    let image_path = write_file(dir.path(), "图.png", &image);
    let text_path = write_file(dir.path(), "说明.txt", text);
    let pdf_path = write_file(dir.path(), "报告.pdf", &pdf);

    let (host, cx) = open_shell(cx, vault, "headless-import-multi");
    let view = active_view(&host, cx);
    // 空正文 → caret 在 0，整批插在最前面（图片独占一行，可投影成对象）。
    cx.write_to_clipboard(clipboard_files(&[
        image_path.clone(),
        text_path.clone(),
        pdf_path.clone(),
    ]));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();

    let expected = format!(
        "![图.png](asset://{})\n[附件：说明.txt](asset://{})\n[附件：报告.pdf](asset://{})",
        hash_hex(&asset_content_hash(&image)),
        hash_hex(&asset_content_hash(text)),
        hash_hex(&asset_content_hash(&pdf)),
    );
    let source = source_of(&view, cx);
    assert_eq!(source, expected, "顺序/语法必须与文件顺序一致");

    let assets = list_assets(&probe).expect("列资产失败");
    assert_eq!(assets.len(), 3, "三个不同字节 = 三个资产");
    assert_eq!(
        asset_bytes(&probe, asset_content_hash(&image)).unwrap(),
        Some(image.clone())
    );
    let refs = list_asset_refs(&probe, &note_id).expect("列引用失败");
    assert_eq!(refs.len(), 3, "{refs:?}");
    let starts: Vec<u64> = refs.iter().map(|entry| entry.start_byte).collect();
    assert_eq!(
        starts,
        asset_refs_in_source(&source)
            .into_iter()
            .map(|(_, start)| start)
            .collect::<Vec<_>>(),
        "引用定位点必须与正文扫描一致"
    );
    assert!(starts[1] > starts[0] && starts[2] > starts[1], "{starts:?}");

    // 单事务：一次 Ctrl+Z 把整批（3 个附件）全部撤掉。
    cx.simulate_keystrokes("ctrl-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "", "一次撤销必须撤掉整批");
    let refs = list_asset_refs(&probe, &note_id).expect("列引用失败");
    assert!(refs.is_empty(), "撤销后引用必须被对账清掉：{refs:?}");
    // 资产保留（v1 不做物理 GC）。
    assert_eq!(list_assets(&probe).unwrap().len(), 3);

    // 重做恢复整批（一个撤销组）。
    cx.simulate_keystrokes("ctrl-shift-z");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), expected, "重做必须整批恢复");
    assert_eq!(
        list_asset_refs(&probe, &note_id).unwrap().len(),
        3,
        "重做后引用同样要重建"
    );

    close_shell(&host, cx);
    // durable 之后引用仍与正文一致（落库路径）。
    let vault = Vault::open(&vault_path).expect("重开 vault");
    let (doc, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    let scanned: Vec<u64> = asset_refs_in_source(&doc.text())
        .into_iter()
        .map(|(_, start)| start)
        .collect();
    let stored: Vec<u64> = list_asset_refs(&vault, &note_id)
        .unwrap()
        .into_iter()
        .map(|entry| entry.start_byte)
        .collect();
    assert_eq!(scanned, stored, "落库后引用必须等于正文扫描结果");
}

// ---------------------------------------------------------------------------
// 4) 失败整批回滚：目录 / 超限 / 损坏图片 → 正文与引用都不动
// ---------------------------------------------------------------------------

#[gpui::test]
async fn rejected_batches_never_touch_body_or_refs(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("reject");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let probe = vault.clone();
    let doc_id = seed_note(&vault, "失败批次", "正文\n");
    let note_id = note_id_of_doc(&vault, &doc_id);

    let (host, cx) = open_shell(cx, vault, "headless-import-reject");
    let view = active_view(&host, cx);
    let before = source_of(&view, cx);

    // 4.1 目录：不递归、整批失败。
    let good = write_file(dir.path(), "good.txt", b"good");
    let subdir = dir.path().join("子目录");
    std::fs::create_dir_all(&subdir).expect("建目录失败");
    cx.write_to_clipboard(clipboard_files(&[good.clone(), subdir]));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), before, "目录批次不得写入正文");
    let (error, notice) = notice_of(&host, cx);
    assert!(notice.is_none());
    assert!(
        error.expect("必须有失败提示").contains("粘贴失败"),
        "失败必须写提示行"
    );
    assert!(list_asset_refs(&probe, &note_id).unwrap().is_empty());
    assert!(
        list_assets(&probe).unwrap().is_empty(),
        "目录/超限失败发生在任何写入之前（连资产都不该有）"
    );

    // 4.2 个数超限（11 个文件 > 10）。
    let mut many = Vec::new();
    for index in 0..11 {
        many.push(write_file(
            dir.path(),
            &format!("f{index}.txt"),
            format!("{index}").as_bytes(),
        ));
    }
    cx.write_to_clipboard(clipboard_files(&many));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), before, "超限批次不得写入正文");
    let (error, _notice) = notice_of(&host, cx);
    assert!(
        error.expect("必须有失败提示").contains("最多导入"),
        "必须说明个数上限"
    );

    // 4.3 损坏图片（声明 PNG、字节不是图片）：不能写半条正文。
    cx.write_to_clipboard(clipboard_image(b"not a png"));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), before, "损坏图片不得写入正文");
    let (error, _notice) = notice_of(&host, cx);
    assert!(
        error.expect("必须有失败提示").contains("图片字节无法识别"),
        "必须说明图片损坏"
    );
    assert!(list_asset_refs(&probe, &note_id).unwrap().is_empty());
    assert!(list_assets(&probe).unwrap().is_empty());

    close_shell(&host, cx);
}

// ---------------------------------------------------------------------------
// 5) 普通文本粘贴 / Ctrl+Shift+V 强制文本
// ---------------------------------------------------------------------------

#[gpui::test]
async fn plain_text_paste_and_force_text_shortcut(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("text");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let probe = vault.clone();
    let doc_id = seed_note(&vault, "文本粘贴", "");
    let note_id = note_id_of_doc(&vault, &doc_id);

    let (host, cx) = open_shell(cx, vault, "headless-import-text");
    let view = active_view(&host, cx);

    // 5.1 普通文本：仍按文本插入（不落资产、不产生引用）。
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("你好".to_owned()));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), "你好");
    assert!(list_assets(&probe).unwrap().is_empty());
    assert!(list_asset_refs(&probe, &note_id).unwrap().is_empty());

    // 5.2 图片 + 文本：Ctrl+V 走图片；Ctrl+Shift+V 强制文本。
    let png = png_bytes(2, 2);
    cx.write_to_clipboard(gpui::ClipboardItem {
        entries: vec![
            gpui::ClipboardEntry::Image(Image {
                format: ImageFormat::Png,
                bytes: png.clone(),
                id: 2,
            }),
            gpui::ClipboardEntry::String(gpui::ClipboardString::new("强制文本".to_owned())),
        ],
    });
    cx.simulate_keystrokes("ctrl-shift-v");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "你好强制文本",
        "Ctrl+Shift+V 必须只插纯文本"
    );
    assert!(
        list_assets(&probe).unwrap().is_empty(),
        "强制文本不得落资产"
    );

    // 5.3 只有文件时 Ctrl+Shift+V：**不**把绝对路径写进正文。
    let path = write_file(dir.path(), "机密.txt", b"secret");
    cx.write_to_clipboard(clipboard_files(&[path]));
    cx.simulate_keystrokes("ctrl-shift-v");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "你好强制文本",
        "强制文本绝不能把 CF_HDROP 的绝对路径插进正文"
    );
    let (error, _notice) = notice_of(&host, cx);
    assert!(error
        .expect("必须提示没有可插入内容")
        .contains("没有可插入"));

    close_shell(&host, cx);
}

// ---------------------------------------------------------------------------
// 6) IME 组合中不插入
// ---------------------------------------------------------------------------

#[gpui::test]
async fn paste_is_ignored_while_ime_composition_is_active(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("compose");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let probe = vault.clone();
    seed_note(&vault, "组合中粘贴", "正文\n");

    let (host, cx) = open_shell(cx, vault, "headless-import-compose");
    let view = active_view(&host, cx);
    // 真实组合态（debug 构建的 F8 走与平台 IME 相同的 app 侧入口 `update_preedit`）。
    cx.simulate_keystrokes("f8");
    cx.run_until_parked();
    assert!(cx.update(|_window, app| view.read(app).is_composing()));

    let png = png_bytes(2, 2);
    cx.write_to_clipboard(clipboard_image(&png));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    assert_eq!(
        source_of(&view, cx),
        "正文\n",
        "组合中粘贴必须整段忽略（不偷插）"
    );
    assert!(list_assets(&probe).unwrap().is_empty());

    close_shell(&host, cx);
}

// ---------------------------------------------------------------------------
// 7) 引用对账：编辑/删除后 start_byte 正确、重复对账幂等
// ---------------------------------------------------------------------------

#[gpui::test]
async fn refs_follow_edits_and_reconciliation_is_idempotent(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("refs");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let probe = vault.clone();

    let png = png_bytes(4, 2);
    let image_hash = asset_content_hash(&png);
    insert_asset(
        &probe,
        &image_hash,
        png.len() as u64,
        Some("image/png"),
        &png,
    )
    .expect("写图片资产失败");
    let text = b"attachment bytes";
    let text_hash = asset_content_hash(text);
    insert_asset(
        &probe,
        &text_hash,
        text.len() as u64,
        Some("text/plain"),
        text,
    )
    .expect("写附件资产失败");

    let body = format!(
        "![图](asset://{})\n\n[附件：a.txt](asset://{})\n",
        hash_hex(&image_hash),
        hash_hex(&text_hash)
    );
    let doc_id = seed_note(&vault, "对账", &body);
    let note_id = note_id_of_doc(&vault, &doc_id);

    let (host, cx) = open_shell(cx, vault, "headless-import-refs");
    let view = active_view(&host, cx);

    // 7.1 打开笔记即对账（ADR 0019 §3）：两条引用、偏移等于正文扫描。
    let assert_matches_scan = |view: &Entity<EditorView>, cx: &mut VisualTestContext| {
        let source = source_of(view, cx);
        let scanned: Vec<u64> = asset_refs_in_source(&source)
            .into_iter()
            .map(|(_, start)| start)
            .collect();
        let stored: Vec<u64> = list_asset_refs(&probe, &note_id)
            .expect("列引用失败")
            .into_iter()
            .map(|entry| entry.start_byte)
            .collect();
        assert_eq!(
            scanned, stored,
            "引用偏移必须等于正文扫描结果（{source:?}）"
        );
    };
    assert_matches_scan(&view, cx);

    // 7.2 在正文最前面插入 "abc"：两条引用都右移 3。
    cx.simulate_keystrokes("home");
    cx.simulate_input("abc");
    cx.run_until_parked();
    let source = source_of(&view, cx);
    assert!(source.starts_with("abc![图]"), "{source:?}");
    assert_matches_scan(&view, cx);
    let refs = list_asset_refs(&probe, &note_id).unwrap();
    // `abc![图](` = 3 + 1 + 1 + 3(图) + 1 + 1 = 10 字节 → `asset://` 前缀的偏移。
    assert_eq!(
        refs[0].start_byte, 10,
        "行首图片的引用定位点必须是 10：{refs:?}"
    );

    // 7.3 重复对账幂等：再对两次，行集合完全不变。
    let before = list_asset_refs(&probe, &note_id).unwrap();
    for _ in 0..2 {
        let summary =
            cx.update(|_window, app| view.update(app, |view, _cx| view.reconcile_asset_refs_now()));
        assert!(summary.is_some(), "对账必须返回统计");
    }
    assert_eq!(
        list_asset_refs(&probe, &note_id).unwrap(),
        before,
        "重复对账必须幂等"
    );

    // 7.4 删掉第一段链接（图片行）：caret 在第 0 行（刚插完 "abc"），
    //     End → Shift+Home 选整行 → Backspace 删掉 → 引用账本只剩附件那一条。
    let refs_before = list_asset_refs(&probe, &note_id).unwrap();
    assert_eq!(refs_before.len(), 2);
    cx.simulate_keystrokes("end shift-home backspace");
    cx.run_until_parked();
    let source = source_of(&view, cx);
    assert!(
        !source.contains("![图](") && source.contains("[附件：a.txt]"),
        "第一段图片链接必须被删掉、附件链接必须保留：{source:?}"
    );
    assert_matches_scan(&view, cx);
    let remaining = list_asset_refs(&probe, &note_id).unwrap();
    assert_eq!(remaining.len(), 1, "删掉的链接不能留在引用账本里");
    assert_eq!(remaining[0].asset_hash, text_hash);
    assert_eq!(
        remaining[0].start_byte,
        source.find("asset://").expect("剩余链接") as u64
    );

    // 7.5 再对账一次仍然幂等（删除后的稳定态）。
    let stable = list_asset_refs(&probe, &note_id).unwrap();
    cx.update(|_window, app| view.update(app, |view, _cx| view.reconcile_asset_refs_now()));
    assert_eq!(list_asset_refs(&probe, &note_id).unwrap(), stable);

    close_shell(&host, cx);
    // 落库后再复算一次（durable 路径）。
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("重开 vault");
    let (doc, _head) = load_body_doc(&vault, &doc_id, 1, None).expect("重放正文失败");
    let scanned: Vec<u64> = asset_refs_in_source(&doc.text())
        .into_iter()
        .map(|(_, start)| start)
        .collect();
    let stored: Vec<u64> = list_asset_refs(&vault, &note_id)
        .unwrap()
        .into_iter()
        .map(|entry| entry.start_byte)
        .collect();
    assert_eq!(scanned, stored);
}

// ---------------------------------------------------------------------------
// 8) SVG 按普通附件（不执行、不当图片对象）
// ---------------------------------------------------------------------------

#[gpui::test]
async fn clipboard_svg_is_stored_as_plain_attachment(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("svg");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let probe = vault.clone();
    let doc_id = seed_note(&vault, "SVG", "");
    let note_id = note_id_of_doc(&vault, &doc_id);

    let (host, cx) = open_shell(cx, vault, "headless-import-svg");
    let view = active_view(&host, cx);

    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="1" height="1"/></svg>"#;
    let hash = asset_content_hash(svg);
    cx.write_to_clipboard(gpui::ClipboardItem::new_image(&Image {
        format: ImageFormat::Svg,
        bytes: svg.to_vec(),
        id: 3,
    }));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();

    let source = source_of(&view, cx);
    assert_eq!(
        source,
        format!("[附件：粘贴图片.svg](asset://{})", hash_hex(&hash)),
        "SVG 必须按普通附件（不写图片语法）"
    );
    assert_eq!(asset_bytes(&probe, hash).unwrap(), Some(svg.to_vec()));
    let meta = asset_meta(&probe, hash).unwrap().expect("资产必须存在");
    assert_eq!(meta.media_type.as_deref(), Some("image/svg+xml"));
    assert_eq!(list_asset_refs(&probe, &note_id).unwrap().len(), 1);
    // 不执行/不栅格化：没有图片对象。
    settle_images(&view, cx);
    let snapshots = cx.update(|_window, app| view.read(app).object_snapshots());
    assert!(snapshots.is_empty(), "SVG 不能变成图片对象：{snapshots:?}");

    close_shell(&host, cx);
}

// ---------------------------------------------------------------------------
// 9) 文件拖放（GPUI `ExternalPaths` 入口）
// ---------------------------------------------------------------------------

#[gpui::test]
async fn dropped_files_are_imported_at_the_current_caret(cx: &mut TestAppContext) {
    let dir = temp_vault_dir("drop");
    let vault = Vault::open(dir.path().join("vault.sqlite3")).expect("打开临时 vault");
    let probe = vault.clone();
    let doc_id = seed_note(&vault, "拖放", "第一行\n第二行\n");
    let note_id = note_id_of_doc(&vault, &doc_id);

    let (host, cx) = open_shell(cx, vault, "headless-import-drop");
    let view = active_view(&host, cx);
    // caret 先走到**第二行**：拖放落点在最上面一行，插入点仍必须是 caret（不看落点）。
    cx.simulate_keystrokes("down");
    cx.run_until_parked();

    let pane = cx
        .debug_bounds("editor-pane")
        .expect("编辑器面板必须画出来");
    let drop_at = gpui::point(
        pane.origin.x + gpui::px(theme::PAD_X + 20.0),
        pane.origin.y + gpui::px(theme::PAD_Y + 10.0),
    );

    let text = b"dropped bytes";
    let path = write_file(dir.path(), "拖放.txt", text);
    let payload = ExternalPaths(vec![path].into_iter().collect());

    // 真实拖拽本身就是鼠标手势：先来一次鼠标移动，把窗口的输入模态从"键盘"切回"鼠标"。
    // GPUI 的 drop 派发用 `hitbox.is_hovered(window)`，键盘模态下恒为 false（按键后
    // 不显示 hover 高亮）；不补这一步，headless 里直接注入 FileDrop 不会命中 drop 监听。
    cx.simulate_mouse_move(drop_at, None, Modifiers::default());
    cx.run_until_parked();

    // 平台文件拖放的三个事件（Entered → Pending → Submit），GPUI 会把它转成内部
    // 拖拽并在 Submit 时对命中的元素派发 `on_drop::<ExternalPaths>`。
    cx.simulate_event(FileDropEvent::Entered {
        position: drop_at,
        paths: payload.clone(),
    });
    cx.simulate_event(FileDropEvent::Pending { position: drop_at });
    cx.simulate_event(FileDropEvent::Submit { position: drop_at });
    cx.run_until_parked();

    let expected = format!(
        "第一行\n[附件：拖放.txt](asset://{})第二行\n",
        hash_hex(&asset_content_hash(text))
    );
    assert_eq!(
        source_of(&view, cx),
        expected,
        "拖放必须在当前 caret（第二行）处插入，而不是落点（第一行）"
    );
    assert_eq!(
        asset_bytes(&probe, asset_content_hash(text)).unwrap(),
        Some(text.to_vec())
    );
    assert_eq!(list_asset_refs(&probe, &note_id).unwrap().len(), 1);

    close_shell(&host, cx);
}

// ---------------------------------------------------------------------------
// 10) 同步持闸（Held）期间不插入（dev-sync 构建）
// ---------------------------------------------------------------------------

/// 同步持闸是**唯一**的只读窗口期：粘贴/拖放一律整段拒绝，正文/引用都不动。
#[cfg(feature = "dev-sync")]
#[gpui::test]
async fn paste_is_ignored_while_sync_gate_is_held(cx: &mut TestAppContext) {
    use std::time::Instant;

    use menghunote_storage::session::{ExternalCommitTicket, GateState};

    let dir = temp_vault_dir("held");
    let vault_path = dir.path().join("vault.sqlite3");
    let vault = Vault::open(&vault_path).expect("打开临时 vault");
    let probe = vault.clone();
    let doc_id = seed_note(&vault, "持闸粘贴", "正文\n");
    let note_id = note_id_of_doc(&vault, &doc_id);

    let (host, cx) = open_shell(cx, vault, "headless-import-held");
    let view = active_view(&host, cx);
    let before = source_of(&view, cx);

    let ticket: ExternalCommitTicket = cx
        .update(|_window, app| {
            let shell = host.read(app).shell.clone();
            shell.update(app, |shell, cx| shell.dev_sync_begin_external(cx))
        })
        .expect("登记外部提交失败");
    // 收敛判据是闸门状态 `Held`（不 sleep）。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = cx.update(|_window, app| {
            let shell = host.read(app).shell.clone();
            shell.update(app, |shell, cx| shell.dev_sync_poll_external(&ticket, cx))
        });
        match state {
            GateState::Held(_) => break,
            GateState::Flushing => std::thread::yield_now(),
            GateState::WaitingIME => panic!("没有组合时不应 WaitingIME"),
            other => panic!("闸门未收敛到 Held：{other:?}"),
        }
        assert!(Instant::now() < deadline, "闸门 10s 内没有进入 Held");
    }
    assert!(cx.update(|_window, app| view.read(app).is_sync_held()));

    // 持闸期间粘贴文件：整段忽略（不读剪贴板、不写资产/引用/正文）。
    let path = write_file(dir.path(), "held.txt", b"held-bytes");
    cx.write_to_clipboard(clipboard_files(&[path]));
    cx.simulate_keystrokes("ctrl-v");
    cx.run_until_parked();
    assert_eq!(source_of(&view, cx), before, "Held 期间不得改动正文");
    assert!(
        list_assets(&probe).unwrap().is_empty(),
        "Held 期间不得写资产"
    );
    assert!(list_asset_refs(&probe, &note_id).unwrap().is_empty());

    cx.update(|_window, app| {
        let shell = host.read(app).shell.clone();
        shell.update(app, |shell, cx| shell.dev_sync_cancel_external(&ticket, cx))
    })
    .expect("释放闸门失败");
    close_shell(&host, cx);
}
