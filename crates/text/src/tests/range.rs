//! `SourceRange` 半开区间的确定性测试（随机版本见 [`super::property`]）。

use crate::coords::SourceByte;
use crate::error::CoordError;
use crate::range::SourceRange;

#[test]
fn half_open_range_excludes_its_end() {
    let source = "A\u{4E2D}\u{1F600}e\u{301}"; // 11 bytes
    let range = SourceRange::within(source, SourceByte::new(4), SourceByte::new(11))
        .expect("4 与 11 都是 scalar 边界");
    assert_eq!(range.start(), SourceByte::new(4));
    assert_eq!(range.end(), SourceByte::new(11));
    assert_eq!(range.len(), 7);
    assert!(!range.is_empty());

    assert!(range.contains(SourceByte::new(4)), "含起点");
    assert!(range.contains(SourceByte::new(10)));
    assert!(!range.contains(SourceByte::new(11)), "半开区间不含终点");
    assert!(!range.contains(SourceByte::new(3)));
}

#[test]
fn empty_range_is_allowed_and_contains_nothing() {
    let source = "abc";
    let empty =
        SourceRange::within(source, SourceByte::new(3), SourceByte::new(3)).expect("空区间合法");
    assert!(empty.is_empty());
    assert_eq!(empty.len(), 0);
    assert!(!empty.contains(SourceByte::new(3)));
}

#[test]
fn reversed_and_out_of_bounds_ranges_are_rejected() {
    let source = "abc";

    // 倒序：先报顺序错误，不做任何钳制。
    assert_eq!(
        SourceRange::within(source, SourceByte::new(2), SourceByte::new(1)),
        Err(CoordError::ReversedRange { start: 2, end: 1 })
    );

    // 越界：end 超过 source 长度。
    assert_eq!(
        SourceRange::new(SourceByte::new(0), SourceByte::new(4), 3),
        Err(CoordError::RangeOutOfBounds {
            start: 0,
            end: 4,
            len: 3
        })
    );

    // start 越界必然先被倒序/越界检查拦住。
    assert_eq!(
        SourceRange::new(SourceByte::new(9), SourceByte::new(9), 3),
        Err(CoordError::RangeOutOfBounds {
            start: 9,
            end: 9,
            len: 3
        })
    );
}

#[test]
fn ranges_must_sit_on_scalar_boundaries() {
    // 『中』占 3 字节；区间端点在 1..3 内部时会切坏 UTF-8。
    let cjk = "\u{4E2D}";
    assert_eq!(
        SourceRange::within(cjk, SourceByte::new(1), SourceByte::new(3)),
        Err(CoordError::NotScalarBoundary { offset: 1, len: 3 })
    );
    assert_eq!(
        SourceRange::within(cjk, SourceByte::new(0), SourceByte::new(2)),
        Err(CoordError::NotScalarBoundary { offset: 2, len: 3 })
    );
    assert_eq!(
        SourceRange::within(cjk, SourceByte::new(0), SourceByte::new(3)),
        Ok(SourceRange::new(SourceByte::new(0), SourceByte::new(3), 3).expect("端点与长度都合法"))
    );
}
