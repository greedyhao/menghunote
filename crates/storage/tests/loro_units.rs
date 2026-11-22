//! **单位夹具（存储侧实测）**：ADR 0002 §11.1 第 1 条 / ADR 0001 §3.4。
//!
//! 这里的断言全部是「跑出来的」而不是抄文档的：
//! `A中😀é`（NFD，即 `e` + U+0301）必须是 11 UTF-8 bytes / 5 scalars / 6 UTF-16 / 4 graphemes，
//! 然后逐项验证 Loro 的 insert/delete、delta retain/delete、cursor 创建与解析的实际单位。

use loro::cursor::Side;
use loro::{LoroDoc, LoroText, TextDelta, VersionVector};

// 通用单位/坐标换算与 `CoordError` 的唯一实现在 menghunote-text（ADR 0003 §4）。
use menghunote_text::units::{byte_len, grapheme_len, scalar_len, utf16_len};
use menghunote_text::{CoordError, InputUtf16, SourceByte, UnicodeScalar};

/// NFD（分解形）的 `A中😀é`：`é` = `e` + U+0301 组合尖音符。
const NFD: &str = "A中😀e\u{301}";
/// NFC（预组合）的 `A中😀é`：`é` = U+00E9。
const NFC: &str = "A中😀\u{e9}";
/// ZWJ 家庭 emoji：4 个 emoji + 3 个 ZWJ。
const ZWJ_FAMILY: &str = "👨\u{200D}👩\u{200D}👧\u{200D}👦";
/// 变体选择符（VS16）的爱心。
const HEART_VS16: &str = "\u{2764}\u{FE0F}";

fn stats(text: &str) -> (usize, usize, usize, usize) {
    (
        byte_len(text),
        scalar_len(text),
        utf16_len(text),
        grapheme_len(text),
    )
}

fn fresh(text: &str) -> (LoroDoc, LoroText) {
    let doc = LoroDoc::new();
    doc.set_peer_id(7).expect("设置 peer id");
    let handle = doc.get_text("body");
    handle.push_str(text).expect("写入初始文本");
    doc.commit();
    (doc, handle)
}

#[test]
fn fixture_counts_match_s0_gate() {
    // ADR 0002 §11.1：A中😀é = 11 UTF-8 bytes / 5 scalars / 6 UTF-16 / 4 graphemes
    assert_eq!(
        stats(NFD),
        (11, 5, 6, 4),
        "NFD `A中😀e\\u{{301}}` 必须是 11/5/6/4"
    );
    // 预组合 é 少一个 scalar / 一个 UTF-16 unit，但 grapheme 数不变
    assert_eq!(stats(NFC), (10, 4, 5, 4), "NFC `A中😀é` 应是 10/4/5/4");
    // ZWJ 家庭 emoji：25 字节、7 scalar、11 UTF-16，但只有 1 个 grapheme
    assert_eq!(
        stats(ZWJ_FAMILY),
        (25, 7, 11, 1),
        "ZWJ 家庭 emoji 应只算 1 个 grapheme"
    );
    // 变体选择符：6 字节、2 scalar，仍然只有 1 个 grapheme
    assert_eq!(stats(HEART_VS16), (6, 2, 2, 1), "VS16 不产生新 grapheme");
}

#[test]
fn loro_insert_delete_measured_in_unicode_scalars() {
    // 单位判别：该串 bytes=11 / scalars=5 / utf16=6。
    // 只有 scalar 口径才会「insert(5) 合法、insert(6) 越界」。
    let (doc, text) = fresh(NFD);
    text.insert(5, "!").expect("insert(5) 在 scalar 口径下合法");
    doc.commit();
    assert_eq!(text.to_string(), "A中😀e\u{301}!");

    let (doc, text) = fresh(NFD);
    let error = text.insert(6, "!").expect_err("insert(6) 必须越界");
    assert!(
        error.to_string().contains("length is 5"),
        "越界信息应暴露 scalar 长度 5，实际：{error}"
    );
    assert!(
        !error.to_string().contains("length is 11"),
        "若是 byte 口径，长度应为 11；实测不是"
    );
    let _ = doc;

    // delete 同样按 scalar：删 2 个正好是 `中😀`
    let (doc, text) = fresh(NFD);
    text.delete(1, 2).expect("delete(1,2)");
    doc.commit();
    assert_eq!(
        text.to_string(),
        "Ae\u{301}",
        "删掉的是 中 + 😀（2 个 scalar）"
    );

    // 反证：若按 byte，delete(0, 11) 会清空；实测 11 超出 scalar 长度 5
    let (_doc, text) = fresh(NFD);
    assert!(
        text.delete(0, 11).is_err(),
        "delete(0,11) 在 scalar 口径下越界（长度为 5）"
    );
}

#[test]
fn loro_delta_retain_delete_measured_in_unicode_scalars() {
    // 若 Retain 是 scalar：Retain2 停在 `中` 之后 → "A中|<rest>"
    let (doc, text) = fresh(NFD);
    let delta = [
        TextDelta::Retain {
            retain: 2,
            attributes: None,
        },
        TextDelta::Insert {
            insert: "|".to_owned(),
            attributes: None,
        },
        TextDelta::Retain {
            retain: 3,
            attributes: None,
        },
    ];
    text.apply_delta(&delta).expect("apply_delta");
    doc.commit();
    assert_eq!(text.to_string(), "A中|😀e\u{301}", "Retain 单位是 scalar");

    // Retain1 + Delete2 + Retain2 → 删掉 中😀
    let (doc, text) = fresh(NFD);
    let delta = [
        TextDelta::Retain {
            retain: 1,
            attributes: None,
        },
        TextDelta::Delete { delete: 2 },
        TextDelta::Retain {
            retain: 2,
            attributes: None,
        },
    ];
    text.apply_delta(&delta).expect("apply_delta");
    doc.commit();
    assert_eq!(text.to_string(), "Ae\u{301}", "Delete 单位是 scalar");

    // 反证：字节口径的 Retain 4 在 5-scalar 串上越界（11 字节里本来是合法的）
    let (_doc, text) = fresh(NFD);
    let delta = [
        TextDelta::Retain {
            retain: 4,
            attributes: None,
        },
        TextDelta::Delete { delete: 4 },
    ];
    assert!(
        text.apply_delta(&delta).is_err(),
        "Retain4+Delete4 在 scalar 口径下越界，说明不是 byte 口径"
    );

    // 补充实测：`to_delta()` 只产出 Insert 段（是「相对空文本的富文本值」，不是「上次改动」）
    let (_doc, text) = fresh(NFD);
    let kinds: Vec<&str> = text
        .to_delta()
        .iter()
        .map(|delta| match delta {
            TextDelta::Retain { .. } => "retain",
            TextDelta::Insert { .. } => "insert",
            TextDelta::Delete { .. } => "delete",
        })
        .collect();
    assert_eq!(kinds, ["insert"], "to_delta() 只给 Insert；不要拿它当增量");
}

#[test]
fn loro_cursor_measured_in_unicode_scalars() {
    let (doc, text) = fresh(NFD);

    // 在 0 处插入一个 4 字节 emoji：scalar 位置只 +1（byte 会 +4、UTF-16 会 +2）
    let cursor = text.get_cursor(3, Side::Middle).expect("创建光标");
    assert_eq!(
        doc.get_cursor_pos(&cursor).expect("解析光标").current.pos,
        3,
        "光标创建/解析单位是 scalar"
    );

    text.insert(0, "😀").expect("插入 emoji");
    doc.commit();
    assert_eq!(
        doc.get_cursor_pos(&cursor).expect("解析光标").current.pos,
        4,
        "4 字节 emoji 只让 scalar 位置 +1（若按 byte 会 +4）"
    );

    // `LoroText::convert_pos` 的语义（实测）：半截索引向下取整，不能当边界校验用
    let (_doc, text) = fresh(NFD);
    assert_eq!(
        text.convert_pos(
            2,
            loro::cursor::PosType::Bytes,
            loro::cursor::PosType::Unicode
        ),
        Some(1),
        "byte 2 落在 `中` 中间，convert_pos 会向下取整而不是报错"
    );
    assert_eq!(
        text.convert_pos(
            3,
            loro::cursor::PosType::Utf16,
            loro::cursor::PosType::Unicode
        ),
        Some(2),
        "UTF-16 3 落在 😀 代理对中间，convert_pos 同样向下取整"
    );
}

#[test]
fn loro_utf16_api_rejects_mid_surrogate_positions() {
    // 😀 = 2 个 UTF-16 code unit；插入点 1 落在代理对中间，必须报错
    for pos in [0usize, 2] {
        let (doc, text) = fresh("😀");
        text.insert_utf16(pos, "x")
            .unwrap_or_else(|error| panic!("insert_utf16({pos}) 应合法，实际 {error}"));
        doc.commit();
    }

    let (_doc, text) = fresh("😀");
    let error = text
        .insert_utf16(1, "x")
        .expect_err("insert_utf16(1) 落在 😀 的代理对中间，必须拒绝");
    assert!(
        error.to_string().to_lowercase().contains("utf-16"),
        "错误信息应指出 UTF-16 半代理对问题，实际：{error}"
    );

    // Rust 层根本构造不出孤立代理对字符串（这保证「非法半代理对」只能出现在越界索引上）
    assert!(
        String::from_utf16(&[0xD83D]).is_err(),
        "孤立高代理不能进 Rust String"
    );
    assert_eq!(char::from_u32(0xD800), None);
    assert_eq!(char::from_u32(0xDC00), None);
}

#[test]
fn adapter_rejects_half_char_and_half_surrogate_indexes() {
    // 通用边界换算归 text 管：这里只验证「storage 侧的坐标入口拿到的是同一份语义」
    // 越界
    assert_eq!(
        SourceByte::new(12).to_scalar(NFD),
        Err(CoordError::ByteOutOfBounds {
            offset: 12,
            len: 11
        })
    );
    // 字节 2 在 `中` 中间
    assert_eq!(
        SourceByte::new(2).to_scalar(NFD),
        Err(CoordError::NotScalarBoundary { offset: 2, len: 11 })
    );
    // UTF-16 3 在 😀 代理对中间
    assert_eq!(
        InputUtf16::new(3).to_scalar(NFD),
        Err(CoordError::Utf16InsideSurrogatePair { index: 3, len: 6 })
    );
    // 合法边界
    assert_eq!(SourceByte::new(4).to_scalar(NFD), Ok(UnicodeScalar::new(2)));
    assert_eq!(
        UnicodeScalar::new(2).to_source_byte(NFD),
        Ok(SourceByte::new(4))
    );
    // UTF-16 4 = `A`(1) + `中`(1) + `😀`(2) 之后 → scalar 3
    assert_eq!(InputUtf16::new(4).to_scalar(NFD), Ok(UnicodeScalar::new(3)));
    assert_eq!(UnicodeScalar::new(2).to_utf16(NFD), Ok(InputUtf16::new(2)));
    // 区间端点等价于逐个端点换算：[0,4) 合法、[0,2) 非法
    assert_eq!(SourceByte::new(0).to_scalar(NFD), Ok(UnicodeScalar::new(0)));
    assert_eq!(SourceByte::new(4).to_scalar(NFD), Ok(UnicodeScalar::new(2)));
    assert!(SourceByte::new(2).to_scalar(NFD).is_err());
}

#[test]
fn adapter_agrees_with_loro_convert_pos_on_valid_boundaries() {
    let (_doc, text) = fresh(NFD);
    for scalar in 0..=scalar_len(NFD) {
        let byte = UnicodeScalar::new(scalar)
            .to_source_byte(NFD)
            .expect("scalar→byte")
            .get();
        let utf16 = UnicodeScalar::new(scalar)
            .to_utf16(NFD)
            .expect("scalar→utf16")
            .get();

        // 与 Loro 自己的换算逐点对齐（合法边界上双方必须一致）
        assert_eq!(
            text.convert_pos(
                byte,
                loro::cursor::PosType::Bytes,
                loro::cursor::PosType::Unicode
            ),
            Some(scalar),
            "byte {byte} → scalar {scalar} 与 Loro 不一致"
        );
        assert_eq!(
            text.convert_pos(
                utf16,
                loro::cursor::PosType::Utf16,
                loro::cursor::PosType::Unicode
            ),
            Some(scalar),
            "utf16 {utf16} → scalar {scalar} 与 Loro 不一致"
        );

        // 往返（text 的强类型坐标与本循环的裸序号等价）
        assert_eq!(
            SourceByte::new(byte).to_scalar(NFD),
            Ok(UnicodeScalar::new(scalar))
        );
        assert_eq!(
            InputUtf16::new(utf16).to_scalar(NFD),
            Ok(UnicodeScalar::new(scalar))
        );
    }
}

#[test]
fn zwj_and_variation_selector_are_scalar_addressable_but_one_grapheme() {
    // CRDT 层面只认 scalar：可以删 ZWJ 本身，允许把字素拆坏（grapheme 不是 CRDT 索引）
    let (doc, text) = fresh(ZWJ_FAMILY);
    assert_eq!(
        text.len_unicode(),
        7,
        "ZWJ 家庭 = 4 emoji + 3 ZWJ = 7 scalar"
    );
    text.delete(1, 1).expect("删掉第一个 ZWJ");
    doc.commit();
    assert_eq!(
        text.to_string(),
        "👨👩\u{200D}👧\u{200D}👦",
        "scalar 层删除会拆开 ZWJ 序列（grapheme 是视图边界，不是 CRDT 索引）"
    );

    let (doc, text) = fresh(HEART_VS16);
    assert_eq!(text.len_unicode(), 2);
    text.delete(1, 1).expect("删掉 VS16");
    doc.commit();
    assert_eq!(text.to_string(), "\u{2764}", "变体选择符是可寻址的 scalar");
}

#[test]
fn loro_update_chain_replays_to_identical_text_and_vv() {
    // 存储侧依赖：整链 update 重放必须得到同一文本 / 同一 frontier
    let doc = LoroDoc::new();
    doc.set_peer_id(11).expect("peer");
    let text = doc.get_text("body");
    text.push_str(NFD).expect("push");
    doc.commit();

    let empty = VersionVector::default();
    let first = doc.export(loro::ExportMode::updates(&empty)).expect("导出");
    // 基准必须是 first 覆盖到的位置（即第一次提交后的 VV），否则第二段会漏掉中间编辑
    let base = doc.oplog_vv();
    text.insert(0, "😀").expect("insert");
    doc.commit();
    text.insert(0, "\u{301}").expect("insert");
    doc.commit();
    let second = doc
        .export(loro::ExportMode::updates(&base))
        .expect("导出增量");

    let replay = LoroDoc::new();
    replay.set_peer_id(99).expect("peer");
    replay.import(&first).expect("导入 first");
    replay.import(&second).expect("导入 second");
    let replay_text = replay.get_text("body");
    assert_eq!(replay_text.to_string(), doc.get_text("body").to_string());
    assert_eq!(replay.state_frontiers(), doc.state_frontiers());
    assert_eq!(replay.oplog_vv(), doc.oplog_vv());
}
