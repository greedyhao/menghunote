//! source selection（ADR 0002 §2.1：source selection 是持久编辑语义）。
//!
//! 两端都用 [`UnicodeScalar`]——与协议里的 [`menghunote_storage::session::ScalarEdit`]
//! 同一单位。**不提供字节 / UTF-16 版本**：那两个单位只出现在投影与输入文档里，
//! 混用必须靠类型挡住。

use menghunote_text::UnicodeScalar;

/// source 上的选区：`anchor` 是拖动起点（可大于 `head`），`head` 是光标端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Selection {
    anchor: UnicodeScalar,
    head: UnicodeScalar,
}

impl Default for Selection {
    fn default() -> Self {
        Self::caret(UnicodeScalar::new(0))
    }
}

impl Selection {
    /// 直接构造（调用方负责用控制器校验越界；字段私有，构造本身不做校验）。
    #[must_use]
    pub const fn new(anchor: UnicodeScalar, head: UnicodeScalar) -> Self {
        Self { anchor, head }
    }

    /// 折叠光标。
    #[must_use]
    pub const fn caret(at: UnicodeScalar) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    /// 拖动起点。
    #[must_use]
    pub const fn anchor(self) -> UnicodeScalar {
        self.anchor
    }

    /// 光标端。
    #[must_use]
    pub const fn head(self) -> UnicodeScalar {
        self.head
    }

    /// 半开区间起点（`min(anchor, head)`）。
    #[must_use]
    pub fn start(self) -> UnicodeScalar {
        if self.anchor <= self.head {
            self.anchor
        } else {
            self.head
        }
    }

    /// 半开区间终点（`max(anchor, head)`，不含）。
    #[must_use]
    pub fn end(self) -> UnicodeScalar {
        if self.anchor <= self.head {
            self.head
        } else {
            self.anchor
        }
    }

    /// 是否折叠（无选区）。
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.anchor.get() == self.head.get()
    }

    /// 选区覆盖的 scalar 数。
    #[must_use]
    pub fn len(self) -> usize {
        self.end().get() - self.start().get()
    }
}
