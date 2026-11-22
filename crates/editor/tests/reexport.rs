//! 迁移守门测试：`menghunote-editor` 的坐标 / 单位必须是 `menghunote-text` 的**同一条定义**。
//!
//! 20 条单位语义测试已随实现一起迁到 `menghunote-text`（ADR 0003 §6 第 5 条：
//! 通用 Unicode 测试移入 / 复用 text）。本文件只锁 re-export 的**同一性**：
//! 若谁在 editor 里重新定义 `SourceByte` / `SourceRange` / `CoordError` 或另写一份
//! `units`，下面的编译期断言会直接失败。

use menghunote_editor::units as editor_units;
use menghunote_editor::{
    CoordError, DisplayByte, InputUtf16, SourceByte, SourceRange, UnicodeScalar,
};

/// 编译期同一性断言：参数类型来自 `menghunote_text`，editor 的 re-export 必须能直接传入。
fn text_byte(byte: menghunote_text::SourceByte) -> usize {
    byte.get()
}

fn text_range(range: menghunote_text::SourceRange) -> usize {
    range.len()
}

fn text_error_text(error: &menghunote_text::CoordError) -> String {
    error.to_string()
}

#[test]
fn editor_reexports_the_single_text_definition() {
    // ADR 0002 §11.1 第 1 条冻结的夹具（`é` 为分解形式 e + U+0301）。
    let text = "A\u{4E2D}\u{1F600}e\u{301}";
    assert_eq!(editor_units::byte_len(text), 11);
    assert_eq!(editor_units::scalar_len(text), 5);
    assert_eq!(editor_units::utf16_len(text), 6);
    assert_eq!(editor_units::grapheme_len(text), 4);

    // 类型同一性：editor 的值可以直接喂给要求 `menghunote_text` 类型的函数。
    assert_eq!(text_byte(SourceByte::new(4)), 4);

    let scalar: UnicodeScalar = SourceByte::new(4)
        .to_scalar(text)
        .expect("4 是 scalar 边界");
    assert_eq!(scalar, UnicodeScalar::new(2));
    assert_eq!(
        DisplayByte::new(1).to_scalar("b\u{1F600}"),
        Ok(UnicodeScalar::new(1))
    );

    let range = SourceRange::within(text, SourceByte::new(4), SourceByte::new(11))
        .expect("4 与 11 都是 scalar 边界");
    assert_eq!(text_range(range), 7);

    let error: CoordError = InputUtf16::new(3)
        .to_source_byte(text)
        .expect_err("3 落在 😀 的代理对中间");
    assert_eq!(
        error,
        menghunote_text::CoordError::Utf16InsideSurrogatePair { index: 3, len: 6 }
    );
    assert!(text_error_text(&error).contains("代理对中间"));
}
