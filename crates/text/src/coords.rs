//! 四个强类型坐标（Newtype，字段私有，互不隐式转换）。
//!
//! 依据 ADR 0002 §4：`SourceByte` / `UnicodeScalar` / `InputUtf16` / `DisplayByte`
//! （本切片不含 `VisualCaret`，它属于布局层）。这些类型存在的原因只有一个：
//! **单位混淆必须是编译错误，而不是等到光标错位才发现**。
//!
//! ## 契约
//!
//! 1. 字段私有，只能经 [`SourceByte::new`] 之类显式构造 + [`SourceByte::get`] 取值；
//! 2. 四者之间**没有 `impl From` / `TryFrom`**。跨单位换算必须同时提供 `&str`
//!    上下文，所以这里用显式方法（如 [`SourceByte::to_scalar`]）而不是类型转换：
//!    "第 3 个单位"只有在说清是哪串文本、哪个单位时才有意义；
//! 3. `DisplayByte` 与 `SourceByte` 之间**故意不提供任何直接换算**：source 与逻辑
//!    显示串的多对一关系要靠后续 `ProjectionMap` 的段 + 边界 bias 表达，任何
//!    "看起来等价"的直转都会掩盖 Elide（隐藏标记）造成的信息丢失；
//! 4. `grapheme` 只有交互边界（见 [`crate::units`]），没有对应坐标类型，
//!    以免它被误当成 CRDT 索引。
//!
//! 所有 `to_*` 方法在越界、非 scalar 边界、代理对内/半代理等情况下返回
//! [`CoordError`]，不会静默截断或钳制。

use crate::error::CoordError;
use crate::units;

/// source（UTF-8 Markdown 源码）上的字节偏移。
///
/// 持久化真源与 CRDT op 的切分单位；`0..=source.byte_len()` 都合法。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceByte(usize);

impl SourceByte {
    /// 直接构造。
    ///
    /// **不做越界校验**——这里不知道所属 source 的长度。越界与边界合法性由
    /// 依赖 `&str` 的换算方法、[`crate::range::SourceRange`] 负责返回错误。
    #[must_use]
    pub const fn new(offset: usize) -> Self {
        Self(offset)
    }

    /// 取出裸偏移（只用于比较 / 传给底层 API，不要拿它跨单位计算）。
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// 换成 Unicode scalar 序号；要求本偏移是 scalar 边界。
    pub fn to_scalar(self, text: &str) -> Result<UnicodeScalar, CoordError> {
        units::byte_to_scalar(text, self.0).map(UnicodeScalar::new)
    }

    /// 换成 UTF-16 code unit 序号；要求本偏移是 scalar 边界。
    pub fn to_utf16(self, text: &str) -> Result<InputUtf16, CoordError> {
        units::byte_to_utf16(text, self.0).map(InputUtf16::new)
    }
}

/// source 上的 Unicode scalar 序号。
///
/// 等于"第几个 `char`"；`0..=source.scalar_len()` 都合法（尾边界可取）。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UnicodeScalar(usize);

impl UnicodeScalar {
    /// 直接构造；越界由 [`UnicodeScalar::to_source_byte`] 等换算方法返回错误。
    #[must_use]
    pub const fn new(index: usize) -> Self {
        Self(index)
    }

    /// 取出裸序号。
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// 换成 source 的 UTF-8 字节偏移。
    pub fn to_source_byte(self, text: &str) -> Result<SourceByte, CoordError> {
        units::scalar_to_byte(text, self.0).map(SourceByte::new)
    }

    /// 换成该文本的 UTF-16 code unit 序号。
    pub fn to_utf16(self, text: &str) -> Result<InputUtf16, CoordError> {
        units::scalar_to_utf16(text, self.0).map(InputUtf16::new)
    }
}

/// 输入 / 显示文本上的 UTF-16 code unit 序号（Windows IME 的原生单位）。
///
/// `0..=text.utf16_len()` 合法；代理对只能整体命中，落在中间的序号会报错。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct InputUtf16(usize);

impl InputUtf16 {
    /// 直接构造；越界与代理对内部位置由换算方法返回错误。
    #[must_use]
    pub const fn new(index: usize) -> Self {
        Self(index)
    }

    /// 取出裸序号。
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// 换成 source 的 UTF-8 字节偏移。
    pub fn to_source_byte(self, text: &str) -> Result<SourceByte, CoordError> {
        units::utf16_to_byte(text, self.0).map(SourceByte::new)
    }

    /// 换成 Unicode scalar 序号。
    pub fn to_scalar(self, text: &str) -> Result<UnicodeScalar, CoordError> {
        self.to_source_byte(text)?.to_scalar(text)
    }
}

/// **逻辑显示串**（source 经投影后的纯文本）上的字节偏移。
///
/// 本切片只定义类型与基础换算，**不接 `ProjectionMap`**；因此它既不能与
/// [`SourceByte`] 互转，也不会被任何 source 侧 API 接受。
///
/// 显示串是派生数据（见 ADR 0002 §4）：它可能比 source 短（Elide 隐藏标记）
/// 或更长（Replace / 转义渲染），source ↔ display 的多对一必须带 bias。
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DisplayByte(usize);

impl DisplayByte {
    /// 直接构造；越界由换算方法返回错误。
    #[must_use]
    pub const fn new(offset: usize) -> Self {
        Self(offset)
    }

    /// 取出裸偏移。
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// 在**逻辑显示串**上换成 Unicode scalar 序号。
    ///
    /// 参数必须是显示串，不能传 source；两者由后续 `ProjectionMap` 建立映射。
    pub fn to_scalar(self, display: &str) -> Result<UnicodeScalar, CoordError> {
        units::byte_to_scalar(display, self.0).map(UnicodeScalar::new)
    }

    /// 在**逻辑显示串**上换成 UTF-16 code unit 序号（命中/IME 几何用）。
    pub fn to_utf16(self, display: &str) -> Result<InputUtf16, CoordError> {
        units::byte_to_utf16(display, self.0).map(InputUtf16::new)
    }

    /// 在**逻辑显示串**上由 Unicode scalar 序号反算显示字节偏移。
    pub fn from_scalar(display: &str, scalar: UnicodeScalar) -> Result<Self, CoordError> {
        units::scalar_to_byte(display, scalar.get()).map(Self::new)
    }

    /// 在**逻辑显示串**上由 UTF-16 code unit 序号反算显示字节偏移。
    pub fn from_utf16(display: &str, units_at: InputUtf16) -> Result<Self, CoordError> {
        units::utf16_to_byte(display, units_at.get()).map(Self::new)
    }
}
