//! 固定种子属性测试（ADR 0002 §11.1 第 2 条在本切片的可测部分）。
//!
//! 刻意**不引入 proptest / rand**：依赖越少越好，且失败必须可复现。
//! 这里手写 xorshift64* + 写死种子，任何一次运行得到完全相同的字符序列；
//! 测试失败时按 `round` 号即可重放。
//!
//! 覆盖的性质：
//! 1. byte ↔ scalar ↔ UTF-16 往返恒等（含 0 与尾边界）；
//! 2. 所有报告的边界都是合法 scalar 边界，grapheme 边界是其子集；
//! 3. 随机越界 / 字符内部 / 代理对内部的输入必须 `Err`；
//! 4. UTF-16 输入与 `std::String::from_utf16` 的判定完全一致；
//! 5. `SourceRange` 的半开语义与越界拒绝。

use crate::coords::{DisplayByte, InputUtf16, SourceByte, UnicodeScalar};
use crate::error::CoordError;
use crate::range::SourceRange;
use crate::units;

/// 固定种子：写死以保证可复现（xorshift 状态不得为 0）。
const SEED: u64 = 0x5EED_0BAD_C0FF_EE42;

/// 每条属性跑的随机样例数；毫秒级开销，足够覆盖 ASCII / 中文 / emoji / 组合符。
const ROUNDS: usize = 256;

/// xorshift64* 伪随机数发生器（仅测试用，不追求统计质量）。
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// `[0, bound)`；调用方保证 `bound > 0`。
    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

/// 随机字符：覆盖 ASCII / 中文 / 非 BMP emoji / 组合附加符号 / ZWJ / 变体选择符 /
/// 控制字符。所有码点范围都避开代理区，`char::from_u32` 的 `None` 分支只是防御。
fn random_char(rng: &mut XorShift64) -> char {
    match rng.below(6) {
        // 可打印 ASCII
        0 => char::from_u32(0x20 + rng.below(0x5F) as u32).unwrap_or('a'),
        // CJK 统一表意文字（3 字节 UTF-8 / 1 个 UTF-16 unit）
        1 => char::from_u32(0x4E00 + rng.below(0x5000) as u32).unwrap_or('\u{4E2D}'),
        // 非 BMP emoji（4 字节 UTF-8 / 2 个 UTF-16 unit，代理对来源）
        2 => char::from_u32(0x1F300 + rng.below(0x200) as u32).unwrap_or('\u{1F600}'),
        // 组合附加符号（制造 grapheme 内部位置）
        3 => char::from_u32(0x0300 + rng.below(0x40) as u32).unwrap_or('\u{301}'),
        // ZWJ / 变体选择符（改变 grapheme 边界）
        4 => ['\u{200D}', '\u{FE0F}'][rng.below(2)],
        // 控制字符：任何计数与边界逻辑都不得对它们特殊对待
        _ => ['\n', '\t', '\0'][rng.below(3)],
    }
}

/// 生成长度 `0..=12` 的随机字符串。
fn random_text(rng: &mut XorShift64) -> String {
    let chars = rng.below(13);
    let mut text = String::new();
    for _ in 0..chars {
        text.push(random_char(rng));
    }
    text
}

/// 从候选集中随机取一个；为空返回 `None`。
fn pick_where(rng: &mut XorShift64, candidates: impl Iterator<Item = usize>) -> Option<usize> {
    let all: Vec<usize> = candidates.collect();
    if all.is_empty() {
        None
    } else {
        Some(all[rng.below(all.len())])
    }
}

/// 文本的全部 UTF-16 边界（测试用的独立参考实现，不复用被测代码）。
fn utf16_boundaries(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut units_seen = 0usize;
    for ch in text.chars() {
        out.push(units_seen);
        units_seen += ch.len_utf16();
    }
    out.push(units_seen);
    out
}

#[test]
fn byte_scalar_utf16_roundtrip_holds_for_random_text() {
    let mut rng = XorShift64::new(SEED);
    for round in 0..ROUNDS {
        let text = random_text(&mut rng);
        let scalar_count = units::scalar_len(&text);

        // 0..=scalar_count：包含首边界与尾边界。
        for scalar in 0..=scalar_count {
            let typed_scalar = UnicodeScalar::new(scalar);
            let source_byte = typed_scalar
                .to_source_byte(&text)
                .unwrap_or_else(|err| panic!("第 {round} 轮：scalar {scalar} 转 byte 失败：{err}"));

            // 所有返回的字节偏移都必须落在合法 scalar 边界上。
            assert!(
                text.is_char_boundary(source_byte.get()),
                "第 {round} 轮：byte {} 不是 scalar 边界（text={text:?}）",
                source_byte.get()
            );

            // scalar → byte → scalar 恒等。
            assert_eq!(
                source_byte
                    .to_scalar(&text)
                    .expect("由 scalar 算出的 byte 必是边界"),
                typed_scalar,
                "第 {round} 轮：scalar {scalar} 往返失败（text={text:?}）"
            );

            // byte → utf16 → byte 恒等。
            let input_utf16 = source_byte
                .to_utf16(&text)
                .expect("由 scalar 算出的 byte 必是边界");
            assert_eq!(
                input_utf16
                    .to_source_byte(&text)
                    .expect("scalar 边界对应的 utf16 序号必可逆"),
                source_byte,
                "第 {round} 轮：byte ↔ utf16 往返失败（text={text:?}）"
            );

            // scalar ↔ utf16 恒等。
            assert_eq!(
                typed_scalar
                    .to_utf16(&text)
                    .expect("scalar 序号在 0..=len 内必合法"),
                input_utf16
            );
            assert_eq!(
                input_utf16
                    .to_scalar(&text)
                    .expect("utf16 边界来自 scalar 边界"),
                typed_scalar
            );

            // DisplayByte 在同一串上做显示坐标往返（本切片 display 串与 source 串
            // 尚未区分，但类型上已强制走 DisplayByte）。
            let display_byte = DisplayByte::new(source_byte.get());
            let display_scalar = display_byte.to_scalar(&text).expect("显示串 scalar 边界");
            assert_eq!(display_scalar, typed_scalar);
            assert_eq!(
                DisplayByte::from_scalar(&text, display_scalar),
                Ok(display_byte)
            );
            let display_utf16 = display_byte.to_utf16(&text).expect("显示串 scalar 边界");
            assert_eq!(
                DisplayByte::from_utf16(&text, display_utf16),
                Ok(display_byte)
            );
        }
    }
}

#[test]
fn reported_boundaries_are_exact_scalar_or_grapheme_boundaries() {
    let mut rng = XorShift64::new(SEED ^ 0x0000_0000_0000_0001);
    for round in 0..ROUNDS {
        let text = random_text(&mut rng);
        let byte_len = units::byte_len(&text);

        let scalar_boundaries = units::scalar_boundaries(&text);
        assert_eq!(scalar_boundaries.first(), Some(&0), "首边界必须是 0");
        assert_eq!(
            scalar_boundaries.last(),
            Some(&byte_len),
            "尾边界必须是长度"
        );
        assert_eq!(
            scalar_boundaries.len(),
            units::scalar_len(&text) + 1,
            "第 {round} 轮：scalar 边界数量（text={text:?}）"
        );
        assert!(
            scalar_boundaries.windows(2).all(|pair| pair[0] < pair[1]),
            "第 {round} 轮：scalar 边界必须严格递增"
        );
        for &at in &scalar_boundaries {
            assert!(units::is_scalar_boundary(&text, at));
            assert_eq!(units::check_scalar_boundary(&text, at), Ok(()));
        }

        let grapheme_boundaries = units::grapheme_boundaries(&text);
        assert_eq!(grapheme_boundaries.first(), Some(&0));
        assert_eq!(grapheme_boundaries.last(), Some(&byte_len));
        assert_eq!(
            grapheme_boundaries.len(),
            units::grapheme_len(&text) + 1,
            "第 {round} 轮：grapheme 边界数量（text={text:?}）"
        );
        for &at in &grapheme_boundaries {
            assert!(
                units::is_scalar_boundary(&text, at),
                "第 {round} 轮：grapheme 边界 {at} 不是 scalar 边界（text={text:?}）"
            );
            assert!(units::is_grapheme_boundary(&text, at));
        }

        // 越界谓词必须为 false，严格校验必须 Err。
        assert!(!units::is_scalar_boundary(&text, byte_len + 1));
        assert!(!units::is_grapheme_boundary(&text, byte_len + 1));
        assert_eq!(
            units::check_scalar_boundary(&text, byte_len + 1),
            Err(CoordError::ByteOutOfBounds {
                offset: byte_len + 1,
                len: byte_len
            })
        );
    }
}

#[test]
fn random_out_of_range_inputs_are_rejected() {
    let mut rng = XorShift64::new(SEED ^ 0xDEAD_BEEF);
    for round in 0..ROUNDS {
        let text = random_text(&mut rng);
        let byte_len = units::byte_len(&text);
        let scalar_count = units::scalar_len(&text);
        let unit_count = units::utf16_len(&text);

        // 越界字节。
        let over_byte = byte_len + 1 + rng.below(8);
        assert_eq!(
            SourceByte::new(over_byte).to_scalar(&text),
            Err(CoordError::ByteOutOfBounds {
                offset: over_byte,
                len: byte_len
            }),
            "第 {round} 轮"
        );
        assert_eq!(
            SourceByte::new(over_byte).to_utf16(&text),
            Err(CoordError::ByteOutOfBounds {
                offset: over_byte,
                len: byte_len
            })
        );
        assert_eq!(
            DisplayByte::new(over_byte).to_scalar(&text),
            Err(CoordError::ByteOutOfBounds {
                offset: over_byte,
                len: byte_len
            })
        );

        // 多字节字符内部的字节偏移（随机挑一个，存在才测；夹具里另有固定反例）。
        if let Some(interior) = pick_where(
            &mut rng,
            (0..byte_len).filter(|&at| !text.is_char_boundary(at)),
        ) {
            assert_eq!(
                SourceByte::new(interior).to_scalar(&text),
                Err(CoordError::NotScalarBoundary {
                    offset: interior,
                    len: byte_len
                }),
                "第 {round} 轮"
            );
            assert_eq!(
                SourceByte::new(interior).to_utf16(&text),
                Err(CoordError::NotScalarBoundary {
                    offset: interior,
                    len: byte_len
                })
            );
        }

        // 越界 scalar 序号。
        let over_scalar = scalar_count + 1 + rng.below(8);
        assert_eq!(
            UnicodeScalar::new(over_scalar).to_source_byte(&text),
            Err(CoordError::ScalarOutOfBounds {
                index: over_scalar,
                len: scalar_count
            }),
            "第 {round} 轮"
        );
        assert_eq!(
            UnicodeScalar::new(over_scalar).to_utf16(&text),
            Err(CoordError::ScalarOutOfBounds {
                index: over_scalar,
                len: scalar_count
            })
        );

        // 越界 UTF-16 序号。
        let over_unit = unit_count + 1 + rng.below(8);
        assert_eq!(
            InputUtf16::new(over_unit).to_source_byte(&text),
            Err(CoordError::Utf16OutOfBounds {
                index: over_unit,
                len: unit_count
            }),
            "第 {round} 轮"
        );

        // 代理对内部的 UTF-16 序号（存在才测）。
        let unit_boundaries = utf16_boundaries(&text);
        if let Some(inside) = pick_where(
            &mut rng,
            (0..unit_count).filter(|at| !unit_boundaries.contains(at)),
        ) {
            assert_eq!(
                InputUtf16::new(inside).to_source_byte(&text),
                Err(CoordError::Utf16InsideSurrogatePair {
                    index: inside,
                    len: unit_count
                }),
                "第 {round} 轮：代理对内部序号 {inside}（text={text:?}）"
            );
            assert_eq!(
                InputUtf16::new(inside).to_scalar(&text),
                Err(CoordError::Utf16InsideSurrogatePair {
                    index: inside,
                    len: unit_count
                })
            );
        }
    }
}

/// 随机 UTF-16 code unit（刻意大量生成半代理）。
fn random_u16(rng: &mut XorShift64) -> u16 {
    match rng.below(4) {
        0 => rng.below(0x0080) as u16,
        1 => (0x4E00 + rng.below(0x1000)) as u16,
        // 整个代理区：保证覆盖合法成对、孤立高代理、孤立低代理、以及反序成对。
        2 => (0xD800 + rng.below(0x0800)) as u16,
        _ => rng.below(0x1_0000) as u16,
    }
}

#[test]
fn utf16_input_accepts_exactly_the_valid_sequences() {
    let mut rng = XorShift64::new(SEED ^ 0x1234_5678);
    for round in 0..ROUNDS {
        let len = rng.below(8);
        let input: Vec<u16> = (0..len).map(|_| random_u16(&mut rng)).collect();

        let expected_valid = String::from_utf16(&input).is_ok();
        let decoded = units::utf16_to_string(&input);
        assert_eq!(
            decoded.is_ok(),
            expected_valid,
            "第 {round} 轮：与 std::String::from_utf16 判定不一致，input={input:?}"
        );

        if let Ok(text) = decoded {
            assert_eq!(
                text.encode_utf16().collect::<Vec<u16>>(),
                input,
                "合法 UTF-16 必须逐 unit 往返"
            );
        }
    }
}

#[test]
fn source_range_enforces_half_open_bounds() {
    let mut rng = XorShift64::new(SEED ^ 0xCAFE);
    for round in 0..ROUNDS {
        let text = random_text(&mut rng);
        let byte_len = units::byte_len(&text);
        let start = rng.below(byte_len + 1);
        let end = start + rng.below(byte_len - start + 1);

        // 半开区间：`end == byte_len` 合法。
        let range = SourceRange::within(&text, SourceByte::new(start), SourceByte::new(end));

        match range {
            Ok(range) => {
                assert!(
                    text.is_char_boundary(start) && text.is_char_boundary(end),
                    "第 {round} 轮：非 scalar 边界竟然构造成功（text={text:?}）"
                );
                assert_eq!(range.start(), SourceByte::new(start));
                assert_eq!(range.end(), SourceByte::new(end));
                assert_eq!(range.len(), end - start);
                assert_eq!(range.is_empty(), start == end);

                if start < end {
                    assert!(range.contains(SourceByte::new(start)), "起点必须在内");
                    assert!(!range.contains(SourceByte::new(end)), "半开区间不含终点");
                } else {
                    assert!(!range.contains(SourceByte::new(start)), "空区间不含任何点");
                }
            }
            Err(err) => {
                assert!(
                    !text.is_char_boundary(start) || !text.is_char_boundary(end),
                    "第 {round} 轮：合法端点却报错 {err}（text={text:?}）"
                );
            }
        }

        // 倒序必须报错。
        if start < end {
            assert_eq!(
                SourceRange::within(&text, SourceByte::new(end), SourceByte::new(start)),
                Err(CoordError::ReversedRange {
                    start: end,
                    end: start
                }),
                "第 {round} 轮"
            );
        }

        // 越界必须报错（与端点是否为 scalar 边界无关）。
        let over = byte_len + 1;
        assert_eq!(
            SourceRange::new(SourceByte::new(0), SourceByte::new(over), byte_len),
            Err(CoordError::RangeOutOfBounds {
                start: 0,
                end: over,
                len: byte_len
            }),
            "第 {round} 轮"
        );
    }
}
