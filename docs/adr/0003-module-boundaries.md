# ADR 0003 — 模块边界与依赖方向（`crates/text` / `storage::session` / `crates/editor`）

- **状态**：已采纳（2026-11-09，`bluetrum/gpt-6.1-sol`，`reasoning_effort: max`）
- **前置**：[ADR 0001](0001-storage-format.md)、[ADR 0002](0002-editor-core.md) 的磁盘/CRDT 协议**不变**；
  本 ADR 只定"代码放哪、谁依赖谁"，不借机改协议。
- **影响范围**：`crates/text`（新增）、`crates/storage`、`crates/editor`、`crates/app`、根 `Cargo.toml`

---

## 1. 背景与问题

实现推进到 S0 末尾时暴露出两个结构问题：

1. **单位/坐标有两份实现**：`crates/storage/src/units.rs`（174 行）与 `crates/editor/src/units.rs`（213 行）
   都在做 UTF-8 byte ↔ Unicode scalar ↔ UTF-16 换算，语义重叠、错误类型不同 → 必然漂移。
2. **文档会话 bridge 无归属**：`EditorDocumentPort`（`OpenSnapshot` / `EditRequest` / `Applied` / `StaleBase` /
   `DurableAck` / `Flush` …）要持有 live `LoroDoc`、未 ACK journal、A/B 两阶段状态；放错位置会导致
   "把会话塞进 GPUI 层（不可测）"或"形成 `storage → editor` 的环"。

约束：`crates/editor` 必须**不依赖 GPUI**（无窗口可测）；`crates/storage` 已有单写者 actor + loro；
单人开发，不为"整洁"引入无收益的 crate。

---

## 2. 决策

**选 B + ①**：

- **B**：新建**薄的 `crates/text`**，只收拢坐标与单位原语；
- **①**：bridge / 具体 `EditorDocumentPort` 与协议类型放 **`storage::session`**；
- **`crates/editor` 允许静态依赖 `crates/storage`**（"无窗口可测" ≠ "不得依赖 SQLite/Loro"）。

为什么不是其他组合：

| 被否方案 | 否决理由 |
| --- | --- |
| A：`storage` 依赖 `editor` 持有单位 | 具体端口又要 `editor → storage`，两者成环 |
| ②：bridge 放 `editor` | 等于把 journal/durable 协调搬进 editor，无必要的依赖倒置 |
| ③：新建 `crates/doc` | 没有真实收益，只是多一层 |
| C：保留两份 units，靠测试对齐 | 两份实现必然漂移，测试对齐是事后补救 |
| D：`storage` 拥有通用单位 | 把通用 Unicode 契约绑死在数据库包里 |

---

## 3. 依赖 DAG（不可有环，含 dev-dependency）

```text
app  →  editor / storage / text / gpui
editor →  storage / text
storage →  text / loro / rusqlite
text →  unicode-segmentation（及错误实现所需的轻依赖）
```

硬约束：

- **禁止任何循环**，包括用 dev-dependency 绕行；
- `storage` **不依赖** `editor`；
- `editor` / `storage` / `text` **不依赖** GPUI 或 `app`（`editor` 也不许加 GPUI dev-dependency）；
- 不另建 `ui` / `doc` crate。

---

## 4. 类型与职责归属

### `crates/text`（唯一来源）

- `SourceByte` / `UnicodeScalar` / `InputUtf16` / `DisplayByte`（四个 newtype）；
- `SourceRange`；
- **共同的 `CoordError`**（以及保留时的 `TextUnit`）；
- byte / scalar / UTF-16 的严格转换与校验、grapheme 原语。

**不在 text**：`DisplayByte ↔ SourceByte` 映射（那是 `ProjectionMap` 的事，属于隐藏标记的多对一语义，
不是单位换算）；grapheme 的移动/删除策略（属于 editor 的交互策略）；
Loro、SQL、协议类型、解析器、GPUI。

### `crates/storage`

- `BodyDoc` 与**Loro 实际 API 单位适配**（只调用 text，不再实现第二套通用换算）；
- 具体 `EditorDocumentPort` / `DocumentSession` 串行 reducer；
- 会话协议：`doc/generation/session_epoch/live_rev/command`、`OpenSnapshot`、`EditRequest`、`Applied`、
  `LiveDelta`、`StaleBase`、`DurableAck`、`FlushTicket`、`HistoryState`、`AnchorToken`、`SaveError`；
- 已有的 `Vault`、journal、A/B 事务、replay、snapshot。

### `crates/editor`

- `EditorController`（持**具体端口**）；
- Rope source 镜像、选择、ProjectionMap、临时输入文档 / preedit；
- **不含 GPUI 类型**的 `LayoutFrame` / `VisualCaret`。

### `crates/app`

- 仅：GPUI `Element`、native 输入 / shape / executor 适配；
- **不实现**文档协议、Undo 或保存状态机。

---

## 5. bridge 必须守住的运行时边界

1. **每个打开的正文只有一个 live owner**；生产调用**不得**拿到可绕过 session 的 live `LoroDoc` 可变句柄。
2. 受理顺序固定：校验 `doc/generation/epoch/base/command` 幂等与区间 → **journal 容量准入/预留** →
   才允许改 Loro。**背压拒绝必须发生在 Loro 变更之前**。
3. `local / remote / undo / redo` **都过同一个 reducer 与 composition 闸门**；Rope 只按确认 delta **更新一次**
   （local echo 不得重复 apply）。
4. **唯一 journal**。现有 `journal.rs` 的 `JournalWriter::flush` 是同步调用 `sink.write_prefix` 后裁剪，
   **不能直接在 GPUI 回调里调用**：需要"连续前缀任务 → 后台 actor → 完成消息回 reducer"，
   两者只能择一封装，**不可出现两份 journal**。
5. **A 成功后才发 `DurableAck` 并裁剪**；"已保存"只由**通过当前 `live_rev` 的 durable 水位**决定；
   **B（物化）不参与保存条件**。
6. **B 是独立重放任务**：从已持久化的固定 frontier 物化，写回仍走同一 `Vault` actor + CAS，
   **不读取/不改动正在编辑的 live Doc**。
7. live reducer 可在 UI owner 线程做**短内存操作**；I/O 与重物化在后台。
   **crate 放在 storage 不决定执行线程**——位置是源码归属，不是线程模型。

---

## 6. 迁移步骤

1. 先冻结本 ADR 与端口/事件类型签名；
2. 以**现有 editor 的强类型 + 严格错误契约**为基线建立 `text`，补齐 storage 需要的函数；
3. storage 侧改为调用同一实现；`CoordError` **只有一份**，`StorageError` 包装它；
4. 旧路径可用 `pub use` **别名**过渡，但**只能别名，不得第二定义**；
5. 通用 Unicode 测试移入/复用 `text`；**Loro API 实测测试留在 storage**；
6. 现有 A/B SQL、schema、`exec_transaction` 守卫、snapshot/replay/actor 的**位置与语义不动**；`app` 空窗口暂不动。

验收（切片 1 的门禁）：原单位/范围行为全部保留；NFC/NFD、代理对中间位、多字节内部、孤立半代理、
空/尾边界等统一严格测试；**无重复通用转换或第二坐标定义**；依赖图无环。

> **不算完成的信号**：只创建空 `text`、或只共享测试但仍保留双实现。

---

## 7. 六个交付切片

| 片 | 内容 | 依赖 | 验收要点 |
| --- | --- | --- | --- |
| **1** | `text` + 双消费者迁移 | 必须先完成 | 原行为保留；无重复实现/第二坐标；无环 |
| **2** | `storage` 会话核 | 端口签名固定后可 ‖3 | stale base / 旧 epoch / 重复 command；同 request 原始基准半开区间、**逆序**应用；单 live owner；背压先于 Loro 变更；唯一 UndoManager + composition 闸门 |
| **3** | `editor` 纯模型 | 可 ‖2 | delta 单次更新、不重复 local echo；UTF-16/byte/scalar 与 source/input/display 不混用；preedit 不成为真实 source |
| **4** | 无头端到端集成 | 等 2+3 | 慢盘/保存失败/持续输入背压；旧 ACK 不标新输入已保存；A 提交 B 未完成重启恢复；旧 B 任务 CAS 不覆盖新版；两 peer + Undo/redo/remote；切换/关闭异步等待；复跑 S0 全链路门禁 |
| **5** | `app` 单 `Element` 源码 UI | 等 4（S0 通过） | **首次 GPUI 接入**；选择/命中/折行/滚动/绘制与 caret/IME geometry 统一同一 `LayoutFrame`；真实 Windows 用 `中😀é`（NFD）与长行，当场回归 |
| **6** | Windows IME + Undo + durable 恢复闭环 | 等 5 geometry 稳定 | 微软拼音 + 至少一种常用 IME；preedit 不写 CRDT/journal/Undo，commit 一次请求/单组；composition 闸门不阻塞 writer/窗口；Undo 仅本 peer 且生成新 op；Ctrl+S/切换/关闭 ticket/水位/错误/重启正文一致；**重启不恢复 Undo 栈** |

**序列：1 → (2 ‖ 3) → 4 → 5 → 6。**
2/3 必须先锁**端口签名与事件类型**，且写域（文件、Cargo 根清单）不得重叠。

**S1 只承诺"源码可见 + identity mapping"**；真实隐藏/显现属于 ADR 0002 的 **S2**，不能把 S1 当作 v1 终点。

---

## 8. v1 冻结 vs 可后补

**冻结（v1 内不再调整）**：

- `text` 的坐标/单位唯一归属；
- `storage::session` 拥有会话/协议、`editor → storage` 的依赖方向；
- 每文档一个 live owner、一份 journal、一个 Undo 栈；
- 基准校验、composition 闸门、A durable 与 B 派生的边界、GPUI 与内核的隔离。

**可后补**：O(n) 参考换算的索引加速（同单位、同错误契约）、Rope/解析器实现、容量与前缀批大小、
B 的计算调度、缓存优化、UI 细节、旧路径别名。
**不得借优化另生第二套语义实现。**

---

## 9. 后果

- **正面**：单位语义只有一处；依赖图无环且可解释；会话核可无头测试；`editor` 永不接触 GPUI；
  第 5 片之前不存在"UI 先写、协议后补"的返工。
- **代价**：`editor` 编译期会拉入 `loro` + `rusqlite`；`text` 多一个 crate 需要维护；
  第 1 片是纯重构，没有用户可见产出（必须先做完）。
- **风险**：`journal` 的同步 `flush` 若被直接搬进 GPUI 回调，会造成 UI 卡顿 + 保存语义错乱；
  第 4 片正是把这条边界测出来。

---

## 10. 一句话

**先把两套单位/坐标收成 `text` 的唯一实现（含 storage 侧迁移），这个门禁过了，才开 session 与 UI。**
