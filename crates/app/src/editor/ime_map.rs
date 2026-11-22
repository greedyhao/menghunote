//! 虚拟输入文档 ↔ source 的 UTF-16 换算（纯逻辑，可单测）。
//!
//! 平台（IME / ATM / `bounds_for_range`）只认**虚拟输入文档**的 UTF-16 坐标；
//! 编辑请求只认 **source** 的 scalar 坐标。这一层负责把两边对起来：
//!
//! ```text
//! source:          [0 .. sel_start)   [sel_start .. sel_end)   [sel_end ..)
//! 虚拟输入文档:     [0 .. sel_start)   [   preedit   ]          (sel_end 之后顺延)
//! ```
//!
//! 组合期间 preedit 顶替选区，所以选区之后的 UTF-16 坐标要整体平移
//! `preedit_utf16 - (sel_end_utf16 - sel_start_utf16)`。
//!
//! 抽成纯函数的原因：这是 IME 里最容易错、又最难靠肉眼发现的地方
//! （中文/emoji/分解形式各占不同 UTF-16 宽度），必须能在没有窗口的环境里测。

use std::ops::Range;

use menghunote_editor::{units, InputUtf16, Selection, SourceByte};

/// 一次换算所需的上下文：source、当前选区、当前 preedit。
#[derive(Debug, Clone, Copy)]
pub(crate) struct InputMapping<'a> {
    source: &'a str,
    selection: Selection,
    preedit: &'a str,
}

/// 换算用的中间量（选区的字节 / UTF-16 边界 + preedit 的 UTF-16 宽度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geo {
    sel_start_byte: usize,
    sel_end_byte: usize,
    sel_start_utf16: usize,
    sel_end_utf16: usize,
    preedit_utf16: usize,
}

impl<'a> InputMapping<'a> {
    /// 构造。
    #[must_use]
    pub(crate) fn new(source: &'a str, selection: Selection, preedit: &'a str) -> Self {
        Self {
            source,
            selection,
            preedit,
        }
    }

    fn geo(&self) -> Option<Geo> {
        let sel_start_byte = self
            .selection
            .start()
            .to_source_byte(self.source)
            .ok()?
            .get();
        let sel_end_byte = self.selection.end().to_source_byte(self.source).ok()?.get();
        let sel_start_utf16 = self.selection.start().to_utf16(self.source).ok()?.get();
        let sel_end_utf16 = self.selection.end().to_utf16(self.source).ok()?.get();
        Some(Geo {
            sel_start_byte,
            sel_end_byte,
            sel_start_utf16,
            sel_end_utf16,
            preedit_utf16: units::utf16_len(self.preedit),
        })
    }

    /// 光标选区在虚拟输入文档里的 UTF-16 区间。
    ///
    /// - 没有 preedit：就是 source 上的选区；
    /// - 有 preedit：`sel_start .. sel_start + preedit_utf16`（组合串整体落在选区位置）。
    #[must_use]
    pub(crate) fn selection_utf16(&self) -> Option<Range<usize>> {
        let geo = self.geo()?;
        let end = if self.preedit.is_empty() {
            geo.sel_end_utf16
        } else {
            geo.sel_start_utf16 + geo.preedit_utf16
        };
        Some(geo.sel_start_utf16..end)
    }

    /// source 字节 → 虚拟输入文档的 UTF-16 序号。
    ///
    /// 落在被 preedit 顶替的选区内部时，收敛到组合串末尾（IME 语境里那是"光标之后"）。
    #[must_use]
    pub(crate) fn source_byte_to_utf16(&self, byte: usize) -> Option<usize> {
        let geo = self.geo()?;
        if byte <= geo.sel_start_byte {
            Some(SourceByte::new(byte).to_utf16(self.source).ok()?.get())
        } else if byte < geo.sel_end_byte {
            Some(geo.sel_start_utf16 + geo.preedit_utf16)
        } else {
            let tail = SourceByte::new(byte).to_utf16(self.source).ok()?.get() - geo.sel_end_utf16;
            Some(geo.sel_start_utf16 + geo.preedit_utf16 + tail)
        }
    }

    /// 虚拟输入文档的 UTF-16 序号 → source 字节。
    ///
    /// 落在组合串内部时收敛到组合起点（选区里那个位置在 source 上就是 `sel_start`）。
    #[must_use]
    pub(crate) fn utf16_to_source_byte(&self, utf16: usize) -> Option<usize> {
        let geo = self.geo()?;
        let preedit_end = geo.sel_start_utf16 + geo.preedit_utf16;
        if utf16 <= geo.sel_start_utf16 {
            InputUtf16::new(utf16)
                .to_source_byte(self.source)
                .ok()
                .map(|byte| byte.get())
        } else if utf16 < preedit_end {
            Some(geo.sel_start_byte)
        } else {
            let tail = utf16 - preedit_end;
            InputUtf16::new(geo.sel_end_utf16 + tail)
                .to_source_byte(self.source)
                .ok()
                .map(|byte| byte.get())
        }
    }

    /// 组合串内部的 UTF-16 位置 → preedit 的**字节前缀长度**（画候选窗 / 光标 x 用）。
    ///
    /// 位置在组合串之前返回 0，之后返回整串长度。
    #[must_use]
    pub(crate) fn preedit_prefix_bytes(&self, utf16: usize) -> usize {
        let Some(geo) = self.geo() else {
            return 0;
        };
        if utf16 <= geo.sel_start_utf16 {
            return 0;
        }
        let within = utf16 - geo.sel_start_utf16;
        let mut seen = 0usize;
        for (byte, ch) in self.preedit.char_indices() {
            if seen >= within {
                return byte;
            }
            seen += ch.len_utf16();
        }
        self.preedit.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use menghunote_editor::UnicodeScalar;

    /// ADR 0002 §11.1 冻结夹具：11 bytes / 5 scalar / 6 UTF-16 / 4 grapheme。
    const NFD: &str = "A\u{4E2D}\u{1F600}e\u{301}";

    fn caret_in(_source: &str, scalar: usize) -> Selection {
        Selection::caret(UnicodeScalar::new(scalar))
    }

    #[test]
    fn without_preedit_the_mapping_is_the_identity_on_utf16() {
        let source = NFD;
        let mapping = InputMapping::new(source, caret_in(source, 1), "");
        assert_eq!(mapping.selection_utf16(), Some(1..1));
        // 每一个 scalar 边界都要双向一致
        for scalar in 0..=5 {
            let byte = UnicodeScalar::new(scalar)
                .to_source_byte(source)
                .expect("边界");
            let utf16 = mapping.source_byte_to_utf16(byte.get()).expect("换算");
            assert_eq!(mapping.utf16_to_source_byte(utf16), Some(byte.get()));
        }
        // 逐个 UTF-16 宽度：A=1、中=1、😀=2、e+U+0301=2 → 共 6
        let emoji_byte = UnicodeScalar::new(2).to_source_byte(source).expect("边界");
        assert_eq!(
            mapping.source_byte_to_utf16(emoji_byte.get()),
            Some(2),
            "😀 之前的 UTF-16 偏移应为 2"
        );
        let after_emoji = UnicodeScalar::new(3).to_source_byte(source).expect("边界");
        assert_eq!(
            mapping.source_byte_to_utf16(after_emoji.get()),
            Some(4),
            "😀 占 2 个 UTF-16 单位"
        );
    }

    #[test]
    fn selection_is_reported_in_utf16_without_preedit() {
        let source = NFD;
        let selection = Selection::new(UnicodeScalar::new(1), UnicodeScalar::new(4));
        let mapping = InputMapping::new(source, selection, "");
        // A(1) 之后 = 1；到 scalar 4（e 之后） = 1+1+2+1 = 5
        assert_eq!(mapping.selection_utf16(), Some(1..5));
    }

    #[test]
    fn preedit_shifts_every_offset_after_the_caret() {
        let source = NFD;
        // 光标在 scalar 1（"中" 之前）
        let mapping = InputMapping::new(source, caret_in(source, 1), "nihao");
        assert_eq!(mapping.selection_utf16(), Some(1..6), "组合串占 1..6");
        // 组合串内部两点都映射回同一个 source 位置（组合起点）
        let caret_byte = UnicodeScalar::new(1).to_source_byte(source).expect("边界");
        assert_eq!(mapping.utf16_to_source_byte(1), Some(caret_byte.get()));
        assert_eq!(mapping.utf16_to_source_byte(3), Some(caret_byte.get()));
        assert_eq!(mapping.utf16_to_source_byte(6), Some(caret_byte.get()));
        // 组合串之后整体平移 5
        let after = UnicodeScalar::new(2).to_source_byte(source).expect("边界");
        assert_eq!(
            mapping.source_byte_to_utf16(after.get()),
            Some(2 + 5),
            "组合串之后的 UTF-16 偏移要 +5"
        );
        assert_eq!(mapping.utf16_to_source_byte(2 + 5), Some(after.get()));
    }

    #[test]
    fn preedit_replacing_a_selection_uses_the_full_span() {
        let source = "abcdef";
        // 选中 "cd"（scalar 2..4），组合串 "中😀"（1 + 2 = 3 个 UTF-16）
        let selection = Selection::new(UnicodeScalar::new(2), UnicodeScalar::new(4));
        let mapping = InputMapping::new(source, selection, "中\u{1F600}");
        assert_eq!(mapping.selection_utf16(), Some(2..5));
        // 组合串之后（source 的 "ef"）从 UTF-16 5 开始：e 在 byte 4、f 在 byte 5
        assert_eq!(mapping.source_byte_to_utf16(4), Some(5), "e 紧跟组合串");
        assert_eq!(mapping.source_byte_to_utf16(5), Some(6), "f 再往后一位");
        assert_eq!(mapping.utf16_to_source_byte(5), Some(4));
        assert_eq!(mapping.utf16_to_source_byte(6), Some(5));
        // 组合串内部 → 选区起点（byte 2）
        assert_eq!(mapping.utf16_to_source_byte(3), Some(2));
        // 组合串之前的偏移不受影响
        assert_eq!(mapping.utf16_to_source_byte(1), Some(1));
    }

    #[test]
    fn preedit_prefix_bytes_counts_utf16_units() {
        let source = "ab";
        let selection = caret_in(source, 1);
        let preedit = "中\u{1F600}x"; // 1 + 2 + 1 = 4 个 UTF-16，字节数 3+4+1=8
        let mapping = InputMapping::new(source, selection, preedit);
        assert_eq!(mapping.preedit_prefix_bytes(1), 0, "组合起点之前是 0");
        assert_eq!(mapping.preedit_prefix_bytes(2), 3, "中 占 3 字节");
        assert_eq!(mapping.preedit_prefix_bytes(4), 7, "中+😀 占 7 字节");
        assert_eq!(mapping.preedit_prefix_bytes(5), 8, "整串 8 字节");
        assert_eq!(mapping.preedit_prefix_bytes(99), 8, "越界收敛到末尾");
    }

    #[test]
    fn out_of_range_utf16_is_rejected_not_clamped() {
        let source = "abc";
        let mapping = InputMapping::new(source, caret_in(source, 0), "");
        assert_eq!(mapping.utf16_to_source_byte(4), None, "越界必须 None");
        // 落在代理对中间（😀 是 2 个 UTF-16）同样 None
        let emoji = "a\u{1F600}b";
        let mapping = InputMapping::new(emoji, caret_in(emoji, 0), "");
        assert_eq!(mapping.utf16_to_source_byte(2), None, "代理对中间不可切");
        assert_eq!(mapping.utf16_to_source_byte(1), Some(1));
        assert_eq!(mapping.utf16_to_source_byte(3), Some(5));
    }
}
