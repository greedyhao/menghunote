//! `ProjectionMap` 的 identity 行为与段表结构测试（ADR 0002 §4 / ADR 0003 §7 切片 3）。
//!
//! S1 只承诺 identity；这里同时锁住"查询走段表、不是整篇偏移差"这一结构要求。

use menghunote_editor::{
    DisplayByte, ProjectionError, ProjectionMap, Segment, SegmentKind, SourceByte, SourceRange,
};

fn copy_segment(source: &str, start: usize, end: usize, display_start: usize) -> Segment {
    let range = SourceRange::within(source, SourceByte::new(start), SourceByte::new(end))
        .expect("测试段端点必须是 scalar 边界");
    Segment::copy(range, DisplayByte::new(display_start)).expect("Copy 段构造")
}

#[test]
fn identity_maps_source_to_display_one_to_one() {
    let text = "A中😀e\u{301}"; // 11 bytes / 5 scalars
    let map = ProjectionMap::identity(text).expect("identity 投影");
    assert!(map.is_identity());
    assert_eq!(map.segments().len(), 1);
    assert_eq!(map.segments()[0].kind(), SegmentKind::Copy);
    assert_eq!(map.segments()[0].display_len(), 11);
    assert_eq!(map.source_len(), 11);
    assert_eq!(map.display_len(), 11);

    // scalar 边界表（ADR 0002 §11.1 夹具）。
    for at in [0usize, 1, 4, 8, 9, 11] {
        assert_eq!(
            map.display_byte(SourceByte::new(at)),
            Ok(DisplayByte::new(at)),
            "source {at} → display"
        );
        assert_eq!(
            map.source_byte(DisplayByte::new(at)),
            Ok(SourceByte::new(at)),
            "display {at} → source"
        );
    }
}

#[test]
fn identity_rejects_out_of_bounds() {
    let map = ProjectionMap::identity("abc").expect("identity 投影");
    assert_eq!(
        map.display_byte(SourceByte::new(4)),
        Err(ProjectionError::SourceByteOutOfBounds { offset: 4, len: 3 })
    );
    assert_eq!(
        map.source_byte(DisplayByte::new(4)),
        Err(ProjectionError::DisplayByteOutOfBounds { offset: 4, len: 3 })
    );
}

#[test]
fn empty_document_is_an_empty_map() {
    let map = ProjectionMap::identity("").expect("空文档也合法");
    assert!(map.segments().is_empty());
    assert_eq!(map.source_len(), 0);
    assert_eq!(map.display_len(), 0);
    assert_eq!(
        map.display_byte(SourceByte::new(0)),
        Ok(DisplayByte::new(0))
    );
    assert_eq!(map.source_byte(DisplayByte::new(0)), Ok(SourceByte::new(0)));
    assert!(map.display_byte(SourceByte::new(1)).is_err());
    assert!(map.source_byte(DisplayByte::new(1)).is_err());
}

#[test]
fn queries_use_the_segment_table_not_a_global_byte_delta() {
    // source "abc"，其中 "b" 被 Elide（显示为 "ac"）。
    // 若用"偏移差"实现，source 2 会映射到 display 2；段表实现必须给出 display 1。
    let source = "abc";
    let copy_a = copy_segment(source, 0, 1, 0);
    let elide_b_range =
        SourceRange::within(source, SourceByte::new(1), SourceByte::new(2)).expect("段端点合法");
    let elide_b = Segment::elide(elide_b_range, DisplayByte::new(1)).expect("Elide 段构造");
    let copy_c = copy_segment(source, 2, 3, 1);
    let map = ProjectionMap::from_segments(source, vec![copy_a, elide_b, copy_c])
        .expect("连续覆盖的段表");

    assert!(!map.is_identity());
    assert_eq!(map.display_len(), 2);
    assert_eq!(
        map.display_byte(SourceByte::new(2)),
        Ok(DisplayByte::new(1)),
        "若得到 2 说明用了整篇偏移差"
    );
    assert_eq!(
        map.display_byte(SourceByte::new(1)),
        Err(ProjectionError::UnsupportedSegment(SegmentKind::Elide)),
        "隐藏区间内部点属于 S2 多对一语义，S1 不猜"
    );
    assert_eq!(
        map.source_byte(DisplayByte::new(1)),
        Ok(SourceByte::new(2)),
        "显示边界 1 对应可见段起点（After bias 的自然结果）"
    );
}

#[test]
fn from_segments_rejects_incomplete_or_invalid_layouts() {
    let source = "abc";

    // 只覆盖 1 个字节：非法。
    let only_one = copy_segment(source, 0, 1, 0);
    assert!(matches!(
        ProjectionMap::from_segments(source, vec![only_one]),
        Err(ProjectionError::InvalidLayout(_))
    ));

    // 段表为空但 source 非空：非法。
    assert!(matches!(
        ProjectionMap::from_segments(source, Vec::new()),
        Err(ProjectionError::InvalidLayout(_))
    ));

    // 源段之间留空洞：非法。
    let first = copy_segment(source, 0, 1, 0);
    let third = copy_segment(source, 2, 3, 1);
    assert!(matches!(
        ProjectionMap::from_segments(source, vec![first, third]),
        Err(ProjectionError::InvalidLayout(_))
    ));

    // Elide 不允许零长度 source 区间。
    let empty_range =
        SourceRange::within(source, SourceByte::new(1), SourceByte::new(1)).expect("空区间合法");
    assert!(matches!(
        Segment::elide(empty_range, DisplayByte::new(0)),
        Err(ProjectionError::InvalidLayout(_))
    ));
}
