# ADR 0005：检索索引（`crates/search`，Tantivy + cang-jie）

- 状态：**已接受**（2026-11-10，GPT 决策 + 人类"开始检索开发"指令）
- 上游决策：[ADR 0001](0001-storage-format.md) 已定引擎（Tantivy 0.27 + cang-jie 0.22 `ForSearch`）与"索引可丢弃、可从 SQL 重建"；
  本 ADR 细化**代码位置、版本水位、发布协议、失败矩阵与 v1 范围**。
- 影响范围：新增 `crates/search`；根 `Cargo.toml` members；`crates/storage` 增加只读投影快照与轻量变更通知；`crates/app` 接入搜索 UI。

## 1. 决策

1. **新增 `crates/search`（package `menghunote-search`）**，依赖方向：`app → search → storage`，
   保持 `app → editor/storage/text`；`storage`/`editor`/`text` **不依赖** `search` 或 GPUI（dev-dependency 也不成环）。
   - 收益：`editor`/`storage` 的无头测试不被 Tantivy/Jieba 重依赖牵连；将来 MCP/CLI 可复用同一索引生命周期与 query API。
   - **`storage` 只提供只读投影快照与轻量变更通知，禁止知道 Tantivy/IndexWriter。**
2. **一篇笔记一个 Tantivy 文档**，字段 `note_id`（STRING/raw）、`title`（TEXT）、`body`（TEXT，纯文本）。
   `chunks` 表当前未写入，**v1 不使用**；不为了纯文本提取让 `search → editor`。
3. **新鲜度不用全局 `max(body_seq)`，也不能只按 `body_seq` 去重。** 每个文档记录投影版本：
   `body_doc_id`、`body_seq`、`body_frontier`（哈希即可）、`meta_seq`（保守失效）、以及索引相关元数据哈希（title/deleted/tag IDs）。
   标题、删除、标签走**结构投影**（`meta_seq`），正文走 B 物化（`body_seq`/`frontier`）——两类变化必须都能触发更新。
4. **发布用"代目录 + SQLite 指针"**，不依赖 Windows 上覆盖/重命名开放目录：
   新代在唯一 generation 目录构建 → 全量扫描（流式读 `body_md`，不整库进内存）→ commit → 验证 open/reload + fingerprint + 最小查询
   → 由一个 SQLite `meta` 事务提交 `active_search_generation = Gnew` → 再换进程内 `Arc<IndexReader>`。
   切换期间旧代继续服务；旧代在 reader 释放、文件句柄关闭后 GC，GC 失败可重试，不让切换失败。
   **目录身份不可复用**（发布后仍会增量写，不是 immutable）。
5. **删除硬保证**：返回结果前，在**新的只读 SQL 快照**批量确认 `notes.deleted=0` 且仍存在（含 tag 过滤）；
   旧代/旧 reader 的墓碑命中同样被剔除；不足则继续取候选，**不能只过滤 top-K 后少返回**；打开笔记时再验一次。
   语义边界：*"在查询的最终 SQL 校验快照之前已提交的删除绝不返回"*；与查询并发、校验之后的删除由事件清屏/打开时检查兜底。
   若产品要求从删除 `DurableAck` 起就隐藏，需额外维护"已接受删除 ID 抑制集"；否则删除的 UI 收敛点应为结构投影完成。

## 2. 新鲜度与运行时协议（v1）

- 时序：编辑 → A 持久化（Loro + journal）→ **原 `DurableAck`，完全不等搜索** → B 从固定 frontier 物化并提交 `notes.body_md`
  → 提交成功后发 `note_id` 失效通知；`B Stale` 不建无效任务；"同 seq/frontier 的修复"若 `Applied` 仍须通知。
- 结构投影 `project()` 提交成功后独立发新增/标题/删除/恢复/标签变化通知；当前整批投影可只发一个 `reconcile_required`。
- 事件只传 ID/失效，不把全文塞队列；队列用 `try_send` + 可合并 dirty 集合；队列满置 `reconcile_required`，**不等待、不静默丢**。
- 后台 **search 单 writer**；读 SQL 用只读连接/快照，不占写 actor；同 note 重复通知合并。
- 批处理：`delete_term(note_id)` + `add_document`；**debounce 200ms、正常负载最迟约 1s 发起 commit**（作为指标而非未验证承诺）。
  `commit` 成功 → `reader.reload()` → 更新 `searchable` 版本/通知 UI；**不得在 `add_document` 返回时认定新鲜**。
- 启动：先完成 storage A/B/结构投影恢复，再**对账**（枚举 Tantivy 已提交文档的版本 vs SQL 清单，补缺/改/删）；
  兼容代可复用，但必须仍做 O(N) 元数据对账（数千篇可接受）。运行期每 30s 一次元数据对账，兜底漏通知与 out-of-process 变化。
- 失败：commit/reload 失败不推进 `searchable`，保留 dirty 与 `reconcile_required`，显示 degraded，退避重试。
  crash 或漏通知不会永久丢更新——SQL 当前投影 + Tantivy 已提交文档的版本标识就是可重放依据。
- v1 **不新增**持久 `search_outbox`/全局 change journal；将来若需要跨进程强 SLA，再在投影事务内加 `search_dirty`。

## 3. 全量重建触发条件

无代、代损坏/打不开、vault identity 不匹配、`schema`/tokenizer/`ForSearch`/词典内容哈希/纯文本转换算法版本变更、或用户显式 rebuild。
**普通重启、正文编辑、标题/标签/删除都不触发重建。** 每代 fingerprint 与 vault UUID 校验；不拿路径名当 vault 身份。
旧词典不兼容的代不能冒充可用；重建期间显示状态，必要时禁搜索。

## 4. v1 范围

### analyzer 管线（探针实测，2026-11-10）

`cang-jie` 的 `ForSearch` **不能裸用**，实测两个坑：

- **大小写敏感**：`gpui` 查不到、`GPUI` 才命中（且 snippet 为空）；
- **空白也成 token**：`ForSearch` 会输出 `" "` 这种 token，并产生重叠词（`编辑`/`编辑器`），放大词表与 BM25 长度。

因此固定管线：**`CangJieTokenizer{ ForSearch }` → 自定义「丢弃纯空白 token」filter → `LowerCaser`**，
索引与查询**共用同一个 analyzer**；索引字段必须 `WithFreqsAndPositions`（短语查询依赖位置）。
查询语义显式化：带引号 = `PhraseQuery`；空格分隔 = `BooleanQuery`（默认 AND，`set_conjunction_by_default`）；
**不要依赖"无引号的中文连写会被当短语"** 这个偶然行为。

- 中文 BM25（标题 + 正文），**标题 boost 3 / 正文 1**（默认起点，不声称最佳）。
- 简单 snippet + token 高亮；输出 **UTF-8 字节区间绑定于同一份 snippet 文本**，
  **不是** editor 的 `SourceByte`，v1 **不承诺**跳转到原文具体位置（只跳笔记）。
- 标签精确过滤（SQL 末端过滤/refill 即可，按 `tag_id`，不另维护冗余 tag 索引）。
- 覆盖：新增、编辑、重命名、删除、恢复、启动补差；UI 查询后台执行、latest-request-wins。
- **明确不做**：拼音/首字母、语义/向量/混合 rerank、模糊/同义词/高级 query DSL、多块/chunk 索引与命中跳转、
  外部 Markdown 监听、用户动态词典、逐键即时一致、对活跃编辑器里未持久化的 Loro 内容做搜索。

## 5. 性能验收（初始目标，待实测）

2000 篇短文：重建 wall < 3s；增量"可见" p95 < 1s；热查询 p95 < 50ms；保存 p95 相对无索引基线无明显回归。
同时记录 peak RSS、索引盘占用、tokenizer 初始化成本、B 投影完成 → commit/reload 可搜索 p95。
**达不到先做批量/提交预算优化与 UI indexing 状态，不改"新增 crate"的 A 决策。**

## 6. 失败与恢复矩阵（必须测）

| 场景 | 期望 |
| --- | --- |
| B 提交后漏通知 | 30s 对账补上；重启对账也补上 |
| commit 前/后杀进程 | 启动对账收敛；墓碑不返回 |
| S0/S1 期间删除/改名 | 发布后不返回已删；标题为最新 |
| 指针提交前崩溃 | 仍用旧代，启动补差 |
| 指针提交后崩溃 | 用新代，启动补尾差 |
| 索引代损坏 | 标 degraded → 重建新代 → 原子切换 |

测试**等明确收敛点**（`indexed` 屏障/完成通知），禁止 sleep。

## 7. 落地顺序

1. **S4-1**：`crates/search` 核心（建/增/删/查/snippet/代切换/fingerprint）+ 单测；`storage` 补只读快照与 meta 指针 API。
2. **S4-2**：app 搜索 UI（`gpui-component` 的 Input/List，Ctrl+K），跳转笔记。
3. **S4-3**：B/结构投影通知接线、debounce/commit/reload、degraded 状态、周期对账。
4. **S4-4**：性能与故障矩阵实测（见 §5、§6）。

## 8. 相关

- [ADR 0001](0001-storage-format.md)：存储与派生索引总原则（检索选型在此，不重复改）。
- [ADR 0003](0003-module-boundaries.md)：workspace 成员与依赖方向（本 ADR 新增第 5 个成员 `crates/search`）。

## 9. 实测性能（2026-11-11，实现完成后）

环境：Windows、本机；语料两个档位：**2000 短文**（每篇 300–600 字）与 **full**（再加 3 篇 ~1MB 长文）；
`cargo run --release --example bench_search`。

| 指标 | 2000 短文 | full（+3×1MB） |
| --- | --- | --- |
| 全量重建 | 1.62s | 2.7–3.2s（冷缓存曾 4.8s） |
| 查询 p95（优化前，用 Tantivy SnippetGenerator） | 3.7ms | **227ms ❌** |
| 查询 p95（自实现 fast_snippet，单行） | — | 12.2ms ✅ |
| 查询 p95（多行分组后，全量命中行） | — | 27.8–39.9ms ✅（接近门槛） |
| 增量可见 p95 | 0.56s | 0.48–0.79s ✅（冷缓存 1.9s） |
| 索引盘占用 | 0.60MiB | 2.5MiB（schema v2 后约 2.1–2.5×） |
| peak RSS | 70.6MiB | 136–152MiB |

**已知代价与后续**：

- 2000 篇**首次**「补齐正文」约 **40–50s**（逐篇 `materialize_body`；之后建索引 ~2.7s）——需批量物化优化；
- 全量命中行使摘要成本随命中行数线性上升；若真实笔记命中行极多，考虑 UI 折叠之外的懒加载；
- **门槛不因实测而调整**：未达标时先优化实现，不改 ADR 数值。

## 10. 批量物化（2026-11-11）

2000 篇全未物化时，逐篇 `materialize_body` 实测 **46.8s**；阶段分解显示 **SQL UPDATE+commit 占 97.9%**
（22.9ms/篇），Loro 解码/apply 仅 0.34ms/篇。根因是**每篇一个 SQLite 事务**，不是 CRDT 或压缩。

因此新增 `reconcile_batched(vault, peer_id, batch_size≈200, progress)`：**每批一个事务**、每批回调一次进度；
单篇准备/事务失败计入 `failed` 并继续。`reconcile` 保留为逐篇兼容路径。

| 指标（2000 篇） | 逐篇 | 批量 |
| --- | --- | --- |
| 物化（storage bench） | 46.8s | **0.38s（≈123×）** |
| 物化（搜索服务内） | 41.8s | **0.59–0.66s** |
| worker 启动→索引就绪 | ≈44.2s | **2.3–3.0s** |
| 进程启动→索引就绪 | 44.2s | **6.2s** |

10001 篇线性成立（物化 2.4–3.0s、就绪 4.25s）。批量后**建索引**（1.7–2.4s）成为首建主要成本，
进度条常态可见性下降属预期；若要让首建全程可见，需要把建索引也做成分批可中断。
