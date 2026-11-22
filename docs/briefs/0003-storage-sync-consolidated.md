# 决策简报 0003 — 存储与同步数据模型定稿（v2）

> 状态：**待 GPT 决策**（`bluetrum/gpt-6.1-sol`, `reasoning_effort: "max"`）
> 取代 [`0001-storage-format.md`](../outdated/0001-storage-format.md) 成为本决策的唯一输入；0001 保留作历史。
> 决策落地后写 `docs/adr/0001-storage-format.md`。
> 提交给 `subagent` 时，把「二、用户已定」至「五、输出要求」整段作为 prompt（逐字复制）。

---

## 一、背景（一句话）

menghunote：Rust + GPUI（Zed 系 `gpui-unofficial` 1.27）桌面笔记软件，**Windows 首要平台**，
单人开发 + AI agent 协作，编辑器对标 **Typora 式原位所见即所得**（见
[`0002-editor-core.md`](0002-editor-core.md)），local-first，必须支持多设备同步与用户**自建中心服务器**。

## 二、用户已定，不要重议

1. **不支持外部编辑器编辑 `.md`**。导出目录是**单向产物**：不做回写、不做文件监听、不做回声抑制。
2. 由此，**磁盘真源 = SQLite 单库文件**（WAL 模式），Markdown 仅作导出与冷备人读格式。
3. **同步加密策略：服务端只见密文（E2E），本地明文**。中继/中心节点不持有明文；本地磁盘安全
   依赖 OS 全盘加密（BitLocker）。用户明确选此档以降低复杂度——**不要再讨论"加密整个本地库"**。
4. **编辑器层真源**（0002）：Markdown 源码字符串，`**`、`#` 等标记**真实存在于文本中**，渲染层只是不绘制。
   因此正文 CRDT 必须是**字符级文本**（Loro `LoroText`），**不得**用 Loro Rich Text，
   **不得**把块树（段落/列表项树）作为持久化结构——块只能由增量解析派生。
5. 已排除：文件监听 / 回声抑制 / Blake3 内容比对 / 文件级三级冲突 / cr-sqlite /
   FTS5 trigram 作中文主路径 / 每键一次 SQLite 事务 / 把向量检索放进 v1 主路径。

## 三、已核实事实（2026-11-08 实测，勿臆断版本）

| 组件 | 事实 |
| --- | --- |
| `loro` **1.17.1** | 内置 `LoroTree`（`mov`/`mov_to`/`mov_after`/`mov_before` + fractional index + `get_meta`），即 outl 手写的那套 move-op 语义；`LoroText` 字符级；update/snapshot 双编码 |
| `tantivy` **0.27.0** | BM25、Block-Max WAND；**不自带中文分词**，需注册自定义 tokenizer |
| `cang-jie` **0.22.0** | 依赖 `tantivy ^0.27.0` + `jieba-rs ^0.11`，维护活跃；`TokenizerOption::{All, Default, ForSearch, Unicode}` 齐备 |
| `rmcp` **3.9.0** | Rust MCP SDK，stdio transport 可用 |
| `sqlite-vec` **0.1.9** | 仍 0.1.x FFI 绑定，维持"语义补充、非主路径" |
| 参考架构 outl | `Markdown 文件即真源` + `op log(truth)/物化树(derived)` + `Yrs` 存块内文本 + iroh 传输；其 sidecar 对账成本是本项目要避开的部分 |

## 四、要决断的问题

**Q1 — CRDT 文档划分与 SQL/CRDT 边界。**
一个 vault 用**一个** `LoroDoc`（树 + 所有正文 + 元数据），还是**每篇笔记一个 `LoroDoc` +
一个结构 `LoroDoc`**？下列事实各归谁：正文、标题、笔记树 parent/顺序、标签、删除墓碑、
创建/更新时间、收藏/置顶、导出路径与文件名。
要求：给出"同一事实只有一处权威"的对照表（事实 → CRDT 还是 SQL → 谁可重建）。

**Q2 — 正文与物化文本的更新协议。**
rope（编辑器缓冲）↔ `LoroText` 的 diff 桥怎么写（按编辑区间生成 insert/delete，禁止整篇替换）；
提交节流与事务边界（正文 op、`body_md`、索引 outbox 是否同事务）；物化时机；断电/崩溃恢复；
如何校验 `body_md` 与 CRDT 一致（frontier/版本号落库？）。
另外：**是否保留 CRDT 历史用于"版本回看"**，保留上限与压缩策略（关系到库体积增长）。
大笔记（>1 MB）与超长单行的性能边界是否需要分片。

**Q3 — 同步协议与中继职责。**
update 的单位与批处理；版本向量交换与重连补偿；离线设备长期不上的保留策略；
relay 到底存什么（纯转发？密文 update log？是否存密文快照？保留多久、按什么条件 GC）；
**新设备 bootstrap**（无对端在线时能否从 relay 拿到全量历史）；
设备配对与密钥分发；CRDT 静默合并在 UI 上是否需要冲突提示。

**Q4 — 密钥与本地安全边界。**
vault 密钥派生（口令 + Argon2id？）与本地存放（Windows DPAPI / 每次口令解锁 / 组合）；
relay 鉴权与设备吊销；口令丢失 = 密文不可恢复，如何提示与接受；
"本地明文 + Tantivy 明文索引"的残余风险是否要在 ADR 里写成**已知边界**。

**Q5 — 附件/图片。**
content-addressed 文件（`blobs/<hash>`，DB 存引用）vs SQLite BLOB：
在"单库真源 + 导出 Markdown + 备份/恢复"三件事上各自代价如何？
导出/导入时如何保持引用一致；孤儿 blob 回收；备份是否必须包含 blobs 目录。

**Q6 — Schema 与落地顺序。**
核心表（notes / tree / loro_docs / chunks / tags / links / index_outbox / meta / assets）+ 索引 +
迁移策略（`user_version`）+ 版本兼容（Loro 编码版本、Tantivy schema 变更）。
给 8 步左右的 MVP 顺序，每步产出与验收标准，并标明**哪一步之前不能并行开发**。

## 五、输出要求

- 直接给结论与理由，不复述问题。
- 每处取舍写"**我选 X 而不是 Y，因为…**"。
- 明确标注 **不可逆** 决定 vs **可后补** 决定。
- 给 SQLite DDL 草图（方言明确的 `CREATE TABLE`/索引）。
- 最后回答："**如果只做一件事，先做什么。**"

---

## 附：Lead 主张（**非结论**，供 GPT 反驳）

- Q1：树 + 元数据放一个结构 `LoroDoc`（`LoroTree` + `LoroMap`），正文每篇一个 `LoroDoc`；
  标题/标签/顺序/墓碑进 CRDT，`updated_at` 等只做 SQL 投影。
- Q2：`body_md` 只是缓存，CRDT 权威；250 ms 合并提交；物化后写 frontier；启动时不一致就重算。
- Q3：relay 只存**密文 update log + 定期密文快照**，新设备靠快照 bootstrap；GC 先不做（只增不删）。
- Q4：口令 → Argon2id → vault key；本地用 DPAPI 包裹后落盘；relay 只见密文。
- Q5：附件走 content-addressed 文件，DB 存哈希引用；备份必须含 `blobs/`。
- Q6：单实例互斥；库存 `%LOCALAPPDATA%`，禁止放 OneDrive 等同步目录；`busy_timeout`。

---

## 六、GPT 首轮反馈（2026-11-08；完整结论见 [ADR 0001](../adr/0001-storage-format.md)）

> 只记录已明确表态的 5 条；其中 3 条**推翻 Lead 附录主张**，实现前以本节为准。

1. **赞同** Q1 划分：结构 `LoroDoc`（树 + 元数据）+ 每篇笔记一个正文 `LoroDoc`。
2. **推翻 Lead 的 blobs/ 方案**：附件选 **SQLite BLOB**。理由：只有 BLOB 入库才真正维持"单库 = 唯一真源"
   与**原子备份**（一个文件即快照，不会出现 DB 与 `blobs/` 目录版本错位）。
   → ADR 需记录：库体积增长与备份体积的代价，以及导出 Markdown 时把附件落盘的规则。
3. **推翻 Lead 的密钥派生**：不做"口令直接派生数据密钥"。改为
   **随机 vault 根密钥** + **Argon2id 口令封装**（口令只用于解开根密钥）+ **DPAPI 本地封装**。
   好处：改口令不需要重加密全部数据；多设备可分别封装同一根密钥。
4. **推翻 Lead 的物化时机**：正文 op 持久化 与 `body_md` 物化必须**分两阶段、各自持 frontier**；
   禁止"每 250 ms 把整篇正文重写进 `body_md`"——>1 MB 笔记会变成持续全文写放大。
   → 待完整结论明确：物化的触发点、增量/懒物化形态、崩溃后如何对齐两个 frontier。
5. **纠正一段因果史观**：完整 CRDT snapshot **只合并/回收重复日志**，**不等于已截断因果历史**；
   v1 必须区分"同步所必需的历史"与"有上限的产品版本回看"，不能把 snapshot 当历史压缩手段。
   → 影响 Q2 的"是否保留 CRDT 历史"与 Q3 的 relay 快照语义，等完整结论定边界。
