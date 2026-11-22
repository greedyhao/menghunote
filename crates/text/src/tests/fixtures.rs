//! 单位夹具（ADR 0002 §11.1 第 1 条）：精确断言，**不允许近似**。
//!
//! 这些数字是后续 Loro 区间 / IME / 投影映射的度量基准；一旦某次实现"顺手"
//! 做了 NFC 归一或把 grapheme 当索引，这里会立刻失败。

use crate::coords::{DisplayByte, InputUtf16, SourceByte, UnicodeScalar};
use crate::error::CoordError;
use crate::units;

/// ADR 0002 §11.1 第 1 条冻结的夹具。
///
/// `é` 是**分解形式**（`e` + U+0301），不是预组合 U+00E9；只有分解形式才同时满足
/// 11 UTF-8 bytes / 5 scalars / 6 UTF-16 / 4 graphemes。
const FIXTURE: &str = "A\u{4E2D}\u{1F600}e\u{301}";

#[test]
fn fixture_matches_frozen_unit_counts() {
    assert_eq!(units::byte_len(FIXTURE), 11, "UTF-8 bytes");
    assert_eq!(units::scalar_len(FIXTURE), 5, "Unicode scalars");
    assert_eq!(units::utf16_len(FIXTURE), 6, "UTF-16 code units");
    assert_eq!(units::grapheme_len(FIXTURE), 4, "graphemes");
}

#[test]
fn precomposed_e_is_a_different_fixture() {
    // 对照：预组合 é（U+00E9）得到另一组数字（10/4/5/4）。
    // 夹具必须写成 e + U+0301，"看起来一样"不能作为单位依据。
    let precomposed = "A\u{4E2D}\u{1F600}\u{E9}";
    assert_eq!(units::byte_len(precomposed), 10);
    assert_eq!(units::scalar_len(precomposed), 4);
    assert_eq!(units::utf16_len(precomposed), 5);
    assert_eq!(units::grapheme_len(precomposed), 4);
    assert_ne!(
        units::byte_len(precomposed),
        units::byte_len(FIXTURE),
        "不做 NFC/NFD 归一：两种形式的字节数不同"
    );
}

#[test]
fn fixture_boundaries_are_mapped_exactly() {
    // scalar 边界对齐表：  A | 中 | 😀 | e | ◌́
    // UTF-8 byte:          0   1    4    8   9   11
    // UTF-16 code unit:    0   1    2    4   5    6
    let byte_at_scalar = [0usize, 1, 4, 8, 9, 11];
    let utf16_at_scalar = [0usize, 1, 2, 4, 5, 6];

    assert_eq!(units::scalar_boundaries(FIXTURE), byte_at_scalar.to_vec());
    assert_eq!(
        units::grapheme_boundaries(FIXTURE),
        vec![0, 1, 4, 8, 11],
        "A / 中 / 😀 / é(分解) 共 4 个 grapheme"
    );

    for (scalar, (&byte, &utf16)) in byte_at_scalar
        .iter()
        .zip(utf16_at_scalar.iter())
        .enumerate()
    {
        let source_byte = SourceByte::new(byte);
        let source_scalar = UnicodeScalar::new(scalar);
        let input_utf16 = InputUtf16::new(utf16);

        assert_eq!(source_byte.to_scalar(FIXTURE), Ok(source_scalar));
        assert_eq!(source_scalar.to_source_byte(FIXTURE), Ok(source_byte));
        assert_eq!(source_byte.to_utf16(FIXTURE), Ok(input_utf16));
        assert_eq!(source_scalar.to_utf16(FIXTURE), Ok(input_utf16));
        assert_eq!(input_utf16.to_source_byte(FIXTURE), Ok(source_byte));
        assert_eq!(input_utf16.to_scalar(FIXTURE), Ok(source_scalar));
        assert_eq!(units::check_scalar_boundary(FIXTURE, byte), Ok(()));
    }
}

#[test]
fn zwj_family_emoji_is_one_grapheme() {
    // 👨‍👩‍👧‍👦 = 4 个 emoji + 3 个 ZWJ（U+200D）
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
    assert_eq!(units::byte_len(family), 25);
    assert_eq!(units::scalar_len(family), 7);
    assert_eq!(units::utf16_len(family), 11);
    assert_eq!(units::grapheme_len(family), 1, "整串只占一个光标位");
    assert_eq!(
        units::scalar_boundaries(family),
        vec![0, 4, 7, 11, 14, 18, 21, 25]
    );
    assert_eq!(units::grapheme_boundaries(family), vec![0, 25]);

    // 中间的 scalar 边界不是 grapheme 边界：退格不得把 ZWJ 或单个 emoji 留在串里。
    assert!(units::is_scalar_boundary(family, 4));
    assert!(!units::is_grapheme_boundary(family, 4));
}

#[test]
fn variation_selector_stays_inside_grapheme() {
    // ❤️ = U+2764 + VS16（U+FE0F）
    let heart = "\u{2764}\u{FE0F}";
    assert_eq!(units::byte_len(heart), 6);
    assert_eq!(units::scalar_len(heart), 2);
    assert_eq!(units::utf16_len(heart), 2);
    assert_eq!(units::grapheme_len(heart), 1);
    assert_eq!(units::grapheme_boundaries(heart), vec![0, 6]);
    assert!(
        !units::is_grapheme_boundary(heart, 3),
        "VS16 之前不是交互边界"
    );
}

#[test]
fn combining_mark_forms_one_grapheme() {
    let text = "e\u{301}";
    assert_eq!(units::byte_len(text), 3);
    assert_eq!(units::scalar_len(text), 2);
    assert_eq!(units::utf16_len(text), 2);
    assert_eq!(units::grapheme_len(text), 1);
    assert_eq!(units::grapheme_boundaries(text), vec![0, 3]);

    // scalar 序号 1 是合法边界（组合符本身是一个 scalar），但不是 grapheme 边界：
    // 按 scalar 删会留下孤立重音，用户预期是按 grapheme 删。
    assert_eq!(
        UnicodeScalar::new(1).to_source_byte(text),
        Ok(SourceByte::new(1))
    );
    assert!(!units::is_grapheme_boundary(text, 1));
}

#[test]
fn unpaired_surrogates_are_rejected() {
    // 半代理对是真实数据损坏（典型来源：手工拼 UTF-16 缓冲区、截断的剪贴板数据），
    // 不能替换成 U+FFFD 蒙混过关。
    let lone_high = [0xD83D_u16];
    let lone_low = [0xDE00_u16];
    let ascii_then_high = [0x0041_u16, 0xD83D];

    // 交叉验证：std 也认为这些输入非法。
    assert!(String::from_utf16(&lone_high).is_err());
    assert!(String::from_utf16(&lone_low).is_err());
    assert!(String::from_utf16(&ascii_then_high).is_err());

    assert_eq!(
        units::utf16_to_string(&lone_high),
        Err(CoordError::UnpairedSurrogate { index: 0, len: 1 })
    );
    assert_eq!(
        units::utf16_to_string(&lone_low),
        Err(CoordError::UnpairedSurrogate { index: 0, len: 1 })
    );
    assert_eq!(
        units::utf16_to_string(&ascii_then_high),
        Err(CoordError::UnpairedSurrogate { index: 1, len: 2 })
    );

    // 合法代理对与空输入的对照。
    assert_eq!(
        units::utf16_to_string(&[0xD83D, 0xDE00]),
        Ok("\u{1F600}".to_owned())
    );
    assert_eq!(units::utf16_to_string(&[]), Ok(String::new()));
}

#[test]
fn out_of_range_and_interior_offsets_are_errors() {
    let len = units::byte_len(FIXTURE); // 11

    // 越界字节偏移（12 > 11）。
    assert_eq!(
        SourceByte::new(12).to_scalar(FIXTURE),
        Err(CoordError::ByteOutOfBounds { offset: 12, len })
    );
    assert_eq!(
        SourceByte::new(12).to_utf16(FIXTURE),
        Err(CoordError::ByteOutOfBounds { offset: 12, len })
    );

    // 2 落在 3 字节的『中』(bytes 1..4) 内部。
    assert_eq!(
        SourceByte::new(2).to_scalar(FIXTURE),
        Err(CoordError::NotScalarBoundary { offset: 2, len })
    );
    assert_eq!(
        SourceByte::new(2).to_utf16(FIXTURE),
        Err(CoordError::NotScalarBoundary { offset: 2, len })
    );

    // scalar 6 > 5。
    assert_eq!(
        UnicodeScalar::new(6).to_source_byte(FIXTURE),
        Err(CoordError::ScalarOutOfBounds { index: 6, len: 5 })
    );
    assert_eq!(
        UnicodeScalar::new(6).to_utf16(FIXTURE),
        Err(CoordError::ScalarOutOfBounds { index: 6, len: 5 })
    );

    // UTF-16 7 > 6。
    assert_eq!(
        InputUtf16::new(7).to_source_byte(FIXTURE),
        Err(CoordError::Utf16OutOfBounds { index: 7, len: 6 })
    );

    // UTF-16 序号 3 落在 😀 的代理对中间（scalar 2 占 units 2..4）。
    assert_eq!(
        InputUtf16::new(3).to_source_byte(FIXTURE),
        Err(CoordError::Utf16InsideSurrogatePair { index: 3, len: 6 })
    );
    assert_eq!(
        InputUtf16::new(3).to_scalar(FIXTURE),
        Err(CoordError::Utf16InsideSurrogatePair { index: 3, len: 6 })
    );

    // 尾边界本身合法。
    assert_eq!(
        SourceByte::new(11).to_scalar(FIXTURE),
        Ok(UnicodeScalar::new(5))
    );
    assert_eq!(
        UnicodeScalar::new(5).to_source_byte(FIXTURE),
        Ok(SourceByte::new(11))
    );
    assert_eq!(
        InputUtf16::new(6).to_scalar(FIXTURE),
        Ok(UnicodeScalar::new(5))
    );
}

#[test]
fn empty_text_has_only_the_zero_boundary() {
    let empty = "";
    assert_eq!(units::byte_len(empty), 0);
    assert_eq!(units::scalar_len(empty), 0);
    assert_eq!(units::utf16_len(empty), 0);
    assert_eq!(units::grapheme_len(empty), 0);
    assert_eq!(units::scalar_boundaries(empty), vec![0]);
    assert_eq!(units::grapheme_boundaries(empty), vec![0]);
    assert!(units::is_scalar_boundary(empty, 0));
    assert!(units::is_grapheme_boundary(empty, 0));

    assert_eq!(
        SourceByte::new(0).to_scalar(empty),
        Ok(UnicodeScalar::new(0))
    );
    assert_eq!(
        UnicodeScalar::new(0).to_source_byte(empty),
        Ok(SourceByte::new(0))
    );
    assert_eq!(
        InputUtf16::new(0).to_source_byte(empty),
        Ok(SourceByte::new(0))
    );

    assert_eq!(
        SourceByte::new(1).to_scalar(empty),
        Err(CoordError::ByteOutOfBounds { offset: 1, len: 0 })
    );
    assert_eq!(
        UnicodeScalar::new(1).to_source_byte(empty),
        Err(CoordError::ScalarOutOfBounds { index: 1, len: 0 })
    );
    assert_eq!(
        InputUtf16::new(1).to_source_byte(empty),
        Err(CoordError::Utf16OutOfBounds { index: 1, len: 0 })
    );
}

#[test]
fn display_byte_measures_the_logical_display_string() {
    // 本切片 display 与 source 是两套坐标；即使内容相同也故意不提供互转。
    let display = "b\u{1F600}"; // b + 😀
    assert_eq!(
        DisplayByte::new(0).to_scalar(display),
        Ok(UnicodeScalar::new(0))
    );
    assert_eq!(
        DisplayByte::new(1).to_scalar(display),
        Ok(UnicodeScalar::new(1))
    );
    assert_eq!(
        DisplayByte::new(1).to_utf16(display),
        Ok(InputUtf16::new(1))
    );
    assert_eq!(
        DisplayByte::from_scalar(display, UnicodeScalar::new(1)),
        Ok(DisplayByte::new(1))
    );
    assert_eq!(
        DisplayByte::from_utf16(display, InputUtf16::new(1)),
        Ok(DisplayByte::new(1))
    );
    assert_eq!(
        DisplayByte::from_utf16(display, InputUtf16::new(3)),
        Ok(DisplayByte::new(5)),
        "UTF-16 序号 3 在代理对之后，对应显示字节 5"
    );
    // 序号 2 是 😀 的低代理内部，必须报错而不是四舍五入到字节 1 或 5。
    assert_eq!(
        DisplayByte::from_utf16(display, InputUtf16::new(2)),
        Err(CoordError::Utf16InsideSurrogatePair { index: 2, len: 3 })
    );

    assert!(
        DisplayByte::new(2).to_scalar(display).is_err(),
        "落在 😀 内部"
    );
    assert!(DisplayByte::new(99).to_scalar(display).is_err(), "越界");
    assert!(DisplayByte::from_scalar(display, UnicodeScalar::new(9)).is_err());
}

#[test]
fn boundary_predicates_agree_with_boundary_tables() {
    let scalar_boundaries = units::scalar_boundaries(FIXTURE);
    for at in 0..=units::byte_len(FIXTURE) + 1 {
        assert_eq!(
            units::is_scalar_boundary(FIXTURE, at),
            scalar_boundaries.contains(&at),
            "at={at}"
        );
    }
    assert!(!units::is_scalar_boundary(FIXTURE, 12));
    assert!(units::check_scalar_boundary(FIXTURE, 12).is_err());

    let grapheme_boundaries = units::grapheme_boundaries(FIXTURE);
    for at in 0..=units::byte_len(FIXTURE) {
        assert_eq!(
            units::is_grapheme_boundary(FIXTURE, at),
            grapheme_boundaries.contains(&at),
            "at={at}"
        );
    }
    assert!(!units::is_grapheme_boundary(FIXTURE, 12));
    assert!(
        !units::is_grapheme_boundary(FIXTURE, 9),
        "9 在 e + 组合重音内部"
    );
}
