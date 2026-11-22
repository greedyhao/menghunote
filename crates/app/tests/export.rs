//! 整库导出 Markdown 的端到端测试（S3-1）。
//!
//! 用真实 vault（`CARGO_TARGET_TMPDIR` 下，避免 `%TEMP%` 的访问问题）+ 真实写盘，
//! 覆盖：目录层级、文件名、正文尾换行、内容等于源文（不是投影后的 display 文本）。

use std::fs;
use std::path::PathBuf;

use menghunote_app::dev_vault::{demo_markdown, open_vault_at, second_note_markdown};
use menghunote_app::export::{export_vault, sanitize_name};

fn tmp_root(name: &str) -> PathBuf {
    let dir =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录失败");
    dir
}

#[test]
fn exports_the_whole_tree_as_markdown() {
    let root = tmp_root("export-e2e");
    let vault_path = root.join("vault.sqlite3");
    let dev = open_vault_at(&vault_path).expect("打开/播种 dev vault 失败");

    let out = root.join("export").join("20261110-000000");
    let report = export_vault(&dev.vault, &out).expect("导出失败");
    assert_eq!(report.succeeded, 2, "演示库里有两篇笔记");
    assert!(
        report.failures.is_empty(),
        "不应有失败：{:?}",
        report.failures
    );

    // 根级笔记
    let demo = out.join(format!("{}.md", sanitize_name("menghunote · dev 演示笔记")));
    assert!(demo.is_file(), "根级笔记应直接在导出根：{}", demo.display());
    let text = fs::read_to_string(&demo).expect("读导出文件失败");
    // 内容必须与**源文**一致（末尾多一个换行而已），而不是投影后隐藏了 `#`/`**` 的显示文本
    let expected = demo_markdown();
    let expected = if expected.ends_with('\n') {
        expected
    } else {
        format!("{expected}\n")
    };
    assert_eq!(text, expected, "导出内容必须等于源文");
    assert!(
        text.contains("# menghunote"),
        "源文里的 Markdown 标记要原样保留"
    );
    assert!(text.contains("**粗体**"), "`**` 不能被隐藏");

    // 目录里的笔记：与侧栏层级一致
    let nested = out
        .join(sanitize_name("示例目录"))
        .join(format!("{}.md", sanitize_name("目录里的第二篇")));
    assert!(
        nested.is_file(),
        "目录里的笔记应在同名子目录：{}",
        nested.display()
    );
    let nested_text = fs::read_to_string(&nested).expect("读嵌套导出文件失败");
    let expected = second_note_markdown();
    let expected = if expected.ends_with('\n') {
        expected
    } else {
        format!("{expected}\n")
    };
    assert_eq!(nested_text, expected);

    // 导出根下只有这两项（没有多余文件）
    let mut names: Vec<String> = fs::read_dir(&out)
        .expect("读导出目录失败")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "实际目录项：{names:?}");
}

#[test]
fn export_is_rerunnable_into_a_fresh_timestamp_directory() {
    let root = tmp_root("export-e2e-twice");
    let vault_path = root.join("vault.sqlite3");
    let dev = open_vault_at(&vault_path).expect("打开/播种 dev vault 失败");

    for stamp in ["20261110-000001", "20261110-000002"] {
        let out = root.join("export").join(stamp);
        let report = export_vault(&dev.vault, &out).expect("导出失败");
        assert_eq!(report.succeeded, 2);
        assert!(out.is_dir(), "每次导出都应该有独立的目录");
    }
}
