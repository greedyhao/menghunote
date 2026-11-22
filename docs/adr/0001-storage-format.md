# ADR 0001 — 存储与同步架构：SQLite 单库真源 + 分区 CRDT + 密文中继

- **状态**：已采纳（2026-11-08，`bluetrum/gpt-6.1-sol`，`reasoning_effort: max`）
- **输入**：[`0003-storage-sync-consolidated.md`](../briefs/0003-storage-sync-consolidated.md)（取代 `0001-storage-format.md`）
- **影响范围**：`crates/storage`、`crates/sync`、`crates/core`、MCP、导出、备份
- **阻塞性**：本文第 8 节第 2 步通过前，禁止并行业务写入实现

---

## 1. 背景

menghunote 是基于 GPUI 的桌面笔记软件（Windows 首要平台，单人 + AI 协作），
编辑器对标 **Typora 式原位所见即所得**：编辑器内的文本就是 Markdown 源码，`#`、`**` 等标记
真实存在于文本中，渲染层只是不绘制（见 [`0002-editor-core.md`](../briefs/0002-editor-core.md)）。

两条产品级前置决策（用户 2026-11-08 拍定）：

1. **不支持外部编辑器编辑 `.md`** → 导出目录是单向产物；因此磁盘真源可以是单一数据库，
   文件监听 / 回声抑制 / 内容哈希比对 / 文件级三级冲突整块消失。
2. **同步走服务端加密（E2E），本地明文** → 中继只见密文；本地安全依赖 OS 全盘加密（BitLocker）。

因此本 ADR 要解决的是：**在一个 SQLite 库里，如何同时承载"字符级 Markdown CRDT"、
"笔记导航树"、"可重建的检索投影"与"逐设备密文同步"，并且崩溃、并发、换代都不丢数据。**

---

## 2. 决策摘要

加粗项为**不可逆**决策（一旦写入用户数据，改动需要迁移或换代）。

1. **一个结构 `LoroDoc`（`LoroTree` + 元数据 Map）+ 每篇笔记一个正文 `LoroDoc`（`LoroText`）**。
2. **结构树只是笔记导航树**，不是 Markdown 块树；**块永远是派生**，不落库。
3. 正文 CRDT 只存原始 Markdown（LF 归一），**不用 Rich Text，不做正文 CRDT 分片**。
4. **编辑即刻映射**（delete(range)+insert），**不做**周期性整篇 diff；提交用"首次 dirty 起固定 250ms 窗口"，
   **不用**会被连续输入无限延期的尾缘 debounce。
5. **两阶段物化**：A 事务持久化 CRDT op 与 outbox；B 事务后台从固定 frontier 物化 `body_md` 与投影。
   **禁止**每 250ms 同事务重写整篇 `body_md`。
6. **同步因果历史与产品版本回看分离**：v1 保留当前代完整因果历史；版本回看用有配额的本机压缩检查点。
7. **relay 是持久密文中继**（密文 update + 完整密文 snapshot + 密文资产 + 加密 manifest），
   **不是纯转发**，也不运行 Loro 或搜索。
8. **附件进 SQLite BLOB**（SHA-256 主键去重），**不放外部 `blobs/` 目录**。
9. **随机 256-bit vault 根密钥 + Argon2id 口令封装 + DPAPI 本地封装**；
   **口令不直接作数据密钥**。
10. 删除语义：**grow-only 删除事件 + 恢复确认事件**，**不用** `LoroTree::delete`，也**不用** SQL `DELETE`。

---

## 3. 详细决策

### 3.1 文档划分与"同一事实只有一处权威"

| 事实 | 权威 | SQL 投影（可重建） |
| --- | --- | --- |
| 正文 | 正文 `LoroDoc` 的 `LoroText`（原始 Markdown） | `notes.body_md` |
| 标题 / 树 parent 与顺序 / 标签成员 / 删除状态 / `created_at` / 收藏 / 置顶 | 结构 `LoroDoc`：`LoroTree` 节点 `get_meta()` 的 `LoroMap` + 全局 Map（标签字典） | `notes`、`tree`、`tags`、`note_tags` |
| `updated_at` | **SQL 派生值**：取正文 Doc 与结构元数据中**显式存储的客户端编辑戳**的 `max` | `notes.updated_at` |
| 导出根目录与策略 | **SQL 本机配置**（不同设备可不同） | — |
| 实际导出文件名/路径 | 导出期派生：树 + 标题 + 稳定 `note_id` | — |
| 光标、展开态、本机同步游标、密钥槽 | **SQL 本地事实**（不可从 CRDT 重建） | — |
| 资产字节 | `assets.data` BLOB | — |
| 资产引用 | Markdown 内 `asset://<SHA-256>` 才是引用事实 | `asset_refs` |

要点：

- **结构 `LoroTree` 仅承载笔记导航树**；节点 `meta` 保存标题、`note_id → body_doc_id`、
  创建时间、标签成员、收藏/置顶、删除/恢复事件。
- **正文编辑戳不写结构 Doc**——否则每次正文提交都要触碰结构文档，制造无谓的串行化。
- **时间戳不参与 CRDT 冲突排序**，`updated_at` 也不用落库时间或接收时间。
- **正文首个 H1 与笔记标题是两个独立事实**，不做双向自动绑定。
- `SQL` 里的 `notes/tree/tags/note_tags/body_md/links/chunks/asset_refs` **只由投影器写入**，
  是纯投影；权威在 CRDT 或本地配置。

### 3.2 身份与代际

- `note_id` **永久稳定**（改名、移动、导出都不变）。
- 每台设备的 `device_id` 与 Loro `peer_id` 必须唯一；**克隆/恢复旧备份作为新实例时必须生成新 `peer_id`**，
  绝不能复制活动 peer 身份。
- 协议预留 **doc generation**：将来截断历史必须"新物理 Doc / 新代"，
  **不能把旧代 update 直接导入新代**。
- `key_epoch` 与 doc generation 是两个维度，不得混用。

### 3.3 删除与恢复

- **不用** `LoroTree::delete`，**不用** SQL `DELETE`。
- 记录 **grow-only 的删除事件 ID** 与**观察到的恢复确认 ID**：
  有效删除 = 存在未被恢复确认的删除事件。
- 恢复只确认"它已见过的"删除；**并发的新删除仍然获胜**。
- 并发正文编辑仍持久化到回收站，**不使笔记复活**。

### 3.4 编辑与持久化协议

**坐标与映射**

- 源码统一 **LF**。三套坐标各司其职、**不得混用**：
  **Rope 源码定位用 UTF-8 byte**；**编辑核 ↔ CRDT 桥的逻辑字符坐标用 Unicode scalar**；
  **IME 另接 UTF-16**，视图用 grapheme。
- ⚠️ **不得**仅凭"`LoroText` 是字符级"就断定 Loro 1.17.1 某个 Rust API 的默认索引单位。
  适配层必须用**中文 / emoji / 组合字符**的固定夹具验证其**实际 API 单位**，
  若与 Unicode scalar 不同，则在适配边界显式转换。
  这条是 v1 的**兼容性验收项**，不是注释——单位判断错 = 中文与 emoji 光标全体错位。
- 每次真实编辑按**当前基准版本**执行 `delete(range) + insert(text)`；
  本地与远端对同一文档的修改**串行化**；**过期偏移不得直接应用**。
- 远端 CRDT delta **增量驱动** Rope；**IME preedit 不写 CRDT**，composition commit 一次提交；
  合并时保留 CRDT 相对光标锚点。

**提交窗口**

- 首次 dirty 起**固定 250ms 窗口**（不是尾缘 debounce）；64 KiB 待提交、`Ctrl+S`、切换/关闭立即冲刷。
- UI **不同步等待** SQLite；"已保存"只在 WAL durable commit 之后显示。

**两阶段物化（不可逆）**

| 阶段 | 内容 |
| --- | --- |
| **A 事务** | 追加 `loro_updates`；更新持久 head frontier / VV / 本机 seq；写 `projection_outbox`、`sync_outbox`。结构类小投影（标题、墓碑等）及其 `index_outbox` 可随结构 op 同事务，确保已删除笔记立即从查询过滤 |
| **B 事务** | 后台从**已持久化的固定 frontier** 物化 `body_md`，原子更新 `body_frontier`/`body_seq`、`chunks`/`links`/`asset_refs`、`index_outbox`。重物化与解析在事务外完成；**旧任务只能清除自己版本的 outbox，不能覆盖新任务** |

节奏：正文 ≤1 MiB 每约 1s 合并物化，较大篇约 2s；**持续输入也不能无限推迟**（数值可调）。

**恢复与校验**

- 启动：snapshot + 后续 update 重放；`body_frontier ≠ head` 就重算。
- **首次打开笔记额外直接比较 CRDT 文本与 `body_md`**——`frontier` 相同**不是**文本正确的证明；
  热路径不做 Blake3 比对。
- A 已提交而 B 未完成：只丢缓存进度。
- **未 ACK 写契约（不可含糊，实现前必须对齐）**：内存中只有**一份可追加的未 ACK 操作日志**，
  **不是**"一个在途批 + 一个新的待提交批"。writer 每次取**连续前缀**提交；durable 后按 ACK **裁剪前缀**；
  过期 flush 请求**合并**；慢 I/O 时用**背压**（暂停接受新写 / 拒绝新的提交），
  而不是假装还能无阻塞吸收输入。
  **持久事务批号与未 ACK 内存日志不是同一概念**：前者可重放，后者丢了就是真丢。
  因此"最多丢一个未 ACK 批"**只在"批"= 连续前缀时成立**；若把"批"理解成固定 250 ms 封口的不可变批次，
  慢 I/O 下就只能拒绝/暂停新写——两者不可能同时成立。本文取**前者**（可追加日志 + 连续前缀 + ACK 裁剪 + 背压）。
  **不承诺**硬件断电下的绝对 250 ms。
- CRDT 权威解码失败 → 恢复模式；**禁止 `body_md` 反向覆盖 CRDT**。
- 索引：commit 后才确认 outbox；重复处理按 `note_id` 删除后重建；可重放。

### 3.5 历史策略：因果历史 ≠ 版本回看

- **v1 保留当前代完整因果历史**，以支持无限天数的未吊销离线设备。
- **普通完整 snapshot 只合并/回收重复 update 行**，**不等于**删除历史或墓碑。
- **产品版本回看**另存压缩 Markdown 检查点：5 分钟一份，AI 调用 / 大替换 / 删除前补份；
  保留 30 天、每篇最多 100 份或 32 MiB 压缩数据、全 vault 最多 1 GiB，先到限先淘汰最旧；
  **v1 仅本机归档，不承诺跨设备复制**。
- 恢复某个版本 = 产生**新的区间 CRDT 编辑**，**不回退 VV**。
- 配额：因果数据默认 **4 GiB/vault**；64 MiB/Doc 或 1 GiB/vault 预警；
  达到配额暂停新增写入/同步并允许调高，**不暗删未同步数据**。
- 真正截断可后补，但必须先满足：全部未退役设备停写、上传、确认**换代屏障**；
  未确认的长期离线设备必须由用户**明确退役**；迟到旧代编辑进入人工差异恢复。
- **不分片正文 CRDT**，只分 Rope / 解析 / 布局。v1 正文 ≤10 MiB、单行 ≤1 MiB 字节可编辑，
  超出转只读 / 源码降级，**不截断**。
  性能门槛：1 MiB 笔记 + 10 万字符单行 + 中文 emoji IME，Windows 输入到视图 **p95 ≤16 ms**
  ——这是**目标，尚未实测**。

### 3.6 同步与中继

- **持久密文中继**（不是纯转发、不是服务端合并）：HTTPS 拉取，WebSocket 仅作唤醒。
  relay 存：鉴权设备公钥与吊销状态、密文 updates、完整密文 snapshots、密文资产、
  加密 manifest / 密钥封装槽、不透明游标。**不运行 Loro、不做搜索、不见明文。**
- 更新单位 = 每个 Doc **已落库批次**的增量 update；多字符 op 合并；
  网络约 1s 或 256 KiB 批量；单传输片 ≤1 MiB；资产 4 MiB 片。
  **这是传输切片，不是 CRDT 分片。**
- VV / base / target frontier 在 **E2E envelope 内**交换；`relay_seq` 只表示送达顺序，
  **不能当因果版本**。
- 断线：按游标补日志 + 按 VV 求缺失；若依赖缺失或日志已回收，
  取**全因果 snapshot** 与本地待同步更新合并，**绝不覆盖本地离线状态**；导入幂等；durable 后才 ACK。
- 跨 Doc 用 `batch_id` + 缺失依赖占位；**不声称**跨文档分布式原子合并。
- **新设备 bootstrap 无需任何对端在线**：先经认证获得 vault key，
  再从 relay 加密 manifest 取结构 + 正文完整快照、所需资产与统一水位 S，最后补 S 之后的日志；
  完成后得到当前代全量因果历史 + 当前状态（**不含**已被淘汰的本机版本归档）。
- **relay GC（v1 即做）**：客户端累计 16 MiB 增量或每 24h 产生**可独立导入**的完整 checkpoint；
  必须先拉齐所声明水位 S、重新解密导入验证覆盖，再发布签名 manifest；
  保留最近两份有效 checkpoint，只删较老那份覆盖的日志，并保留 **7 天宽限**；
  checkpoint **不截断因果历史**，因此**不需要按每台离线设备永久留日志，也不能按 `last_seen` 静默删设备**；
  vault 数据保留至用户显式删除或配额处置；达配额拒绝新增、保留已有数据，并为 checkpoint 替换预留空间。
- **E2E 不防恶意 relay 删数据**：客户端记录已确认水位以检测回退；
  新设备在无可信旧水位时**无法独立识别"有效签名的旧快照回滚"** → 已写入第 6 节已知边界。
- **UI**：非阻断并发合并提示。VV 并发编辑时提示来源并展示增量；删除 + 编辑并发提示
  "回收站有新内容"；语义误合并由有界版本回看恢复；**不建立文件三级冲突机制**。

### 3.7 密钥与 E2E

- **随机 256-bit vault 根密钥**；`Argon2id`（64 MiB、t=3、p=1、16-byte 随机 salt；参数随槽存、可向上校准）
  **只派生 KEK 用于封装根密钥**；改口令仅重包，不重加密历史。
- ⚠️ **口令只在客户端进入 Argon2id，永远不得发送给 relay**；尤其**不得**复用为会明文提交给服务端的
  "登录口令"。relay 的公钥挑战 / 短期设备 token / 独立管理凭证属于**另一个控制面**。
  否则服务端一旦拿到登录口令，配上公开 salt 与口令封装槽即可解开根密钥，**E2E 名存实亡**。
  新设备从 relay 取回密钥槽后，也**必须本地解封**，不得把口令或根密钥交给服务端代解。
- ⚠️ **"改口令" ≠ "撤销已泄露的解密能力"**。日常改口令只重包随机根密钥，成本低；
  但一旦怀疑**口令或旧封装槽已泄露**，仅重包无效——攻击者可留着旧槽解出同一个根密钥。
  此时必须走**设备吊销流程：轮换随机 `key_epoch` 并发布新加密基线**，
  旧槽只在迁移窗口内保留给仍授权的设备。
  产品文案**不得**宣传"改口令即可收回攻击者已拿到的解密能力"。
- Windows 默认用 **DPAPI CurrentUser 包裹本机根密钥**，不要求每次口令解锁；
  **不改变本地明文选择**。
- 算法：XChaCha20-Poly1305、随机 24-byte nonce、HKDF-SHA256 分用途子密钥；
  AAD 绑定 vault / object / type / generation / key_epoch / msg / 片号 / 协议。
- VV 与真实资产 hash 留在密文内；relay 侧资产名用 **HMAC 派生的不透明 ID**，避免明文 hash 的已知图片探测。
- 配对：已授权设备扫描**高熵一次性邀请**并核对公钥指纹，成熟 HPKE 封装给新设备公钥；
  **不自造 6 位码当密钥**。新设备也可凭口令槽 / 独立恢复码解封后从 relay bootstrap。
- relay 设备鉴权：公钥证明 + 短期 token；新增/吊销需**独立账户管理权**，
  **不能仅凭知道 vault key**。吊销立即拒绝旧设备认证并轮换随机 `key_epoch`，
  为当前 bootstrap checkpoint 与所需资产重新加密发布到新 epoch，
  旧解密槽仅供仍授权设备迁移，旧密文按 GC 收尾。
  **已被旧设备得到的明文与旧历史不可撤回。**
- **纠正"忘口令必丢"**：有 DPAPI 授权设备或恢复码即可重包新口令；
  只有 relay 密文 + 口令 / 恢复码 / 授权设备全失才永久不可恢复。
  本地明文 DB 备份仍可恢复笔记并建立新 vault。创建时强制确认"无服务器后门"并校验恢复码已备份。

### 3.8 附件

- **SQLite BLOB + SHA-256 主键去重**；不选外部 `blobs/<hash>`：
  外部 CAS 会让真源变成"DB + 目录"，跨资源提交与备份都要额外协议；
  BLOB 是单事务、一次 SQLite 在线备份即恢复全部。
- 代价（接受）：DB 与 WAL 变大；导出必须复制为 `assets/` 目录。
- v1 单附件 ≤64 MiB；不把大视频放主路径。
- 本地新增资产字节与引用它的正文 op **同 A 事务**；
  远端可先有 missing 资产占位（`assets.data IS NULL`），下载并校验 hash 后再填字节，
  **不假装已离线可用**。
- 导出固定已提交 frontier（不盲读旧 `body_md`）；`asset://hash` 与 `note://uuid`
  只在单向导出时重写为相对路径，并随导出 manifest 保存对应关系；导入是显式导入，不回写、不监听。
- 孤儿 GC：活笔记 / 回收站 / 保留版本 / 保留 bootstrap checkpoint / 待同步任务均无引用，
  且满 30 天、所有未吊销设备确认稳定 GC 水位，才可删；否则**只标记不删**。
  从未引用或发布的暂存资产 24h 可删。
- 备份：**不需要** `blobs/` 目录；必须用 SQLite backup API / 一致备份，
  **不能直接拷贝运行中的主文件**（会丢 WAL）。Tantivy 不需备份，可重建。

### 3.9 迁移、兼容与平台约束

- 每 vault：Windows 命名 mutex + **单 writer actor**；`journal_mode=WAL`、`synchronous=FULL`、
  `busy_timeout=5000`。
- 库存 `%LOCALAPPDATA%`；**禁止** OneDrive / SMB / 网络盘；备份与导出可放别处。
- `user_version` 逐级事务迁移；变更前做 SQLite 一致备份；新 schema 下旧程序**只读拒写**；
  **不是**每次启动自动重建权威。
- 每个 Loro snapshot/update 携带实际 codec 标识、`app_doc_schema`、`min_reader`；
  升级用固定 1.17.1 测试夹具证明；**不把 crate semver 当编码兼容承诺**；
  双读旧写，所有未退役设备兼容后才切新写。
- 检索：**Tantivy 0.27 + cang-jie 0.22 `ForSearch`** 做中文 BM25 主路径；
  schema / tokenizer / 词典变更 → **新索引代**从 SQL 全量重建并原子切换；
  `chunks` 是可删派生区域，无持久块树；向量检索后补。

---

## 4. 被否方案

| 方案 | 否决理由 |
| --- | --- |
| vault 单体 `LoroDoc` | 正文无法按需加载 / 独立同步与恢复；结构会承受全部正文历史 |
| 正文用 Loro Rich Text | 与 Typora 模型冲突：`**` 根本不存在于数据里 |
| 持久化块树 | 真源分裂 + 往返保真成本；且不是 Typora 编辑模型 |
| 周期性整篇 diff 同步到 CRDT | 写放大，与 1 MiB 正文目标冲突 |
| 尾缘 debounce 提交 | 连续输入时可无限延期，永远不落盘 |
| 每 250ms 同事务重写 `body_md` | 大篇持续全文写放大 |
| 用完整 snapshot 当"历史压缩/截断" | 只合并重复日志，不截断因果历史 |
| `LoroTree::delete` / SQL `DELETE` 当删除真源 | 无法表达"删除 vs 恢复"的并发语义 |
| 6 位配对码当密钥 | 弱密钥，可暴力 |
| 口令直接派生数据密钥 | 改口令要重加密全库 |
| 附件走外部 `blobs/<hash>` CAS | 真源变 DB+目录；跨资源提交与备份失去原子性 |
| relay 纯转发 | 新设备 bootstrap 依赖在线对端；离线设备日志无处存放 |
| 直接拷贝运行中的 sqlite 主文件备份 | 丢 WAL，备份不一致 |
| 文件监听 / 回声抑制 / 文件级三级冲突 | 场景已不存在（不做外部编辑），整块删除 |

---

## 5. 后果

**正面**：单一事实链；投影全部可重建；崩溃只丢未 ACK 的内存批次；
支持无限天数离线设备；可自建纯密文中继，服务器不可信也不泄内容。

**代价（接受）**：
- 库与备份体积随附件增长（BLOB 入了库）；
- 正文与结构两套 frontier / outbox，状态机复杂度上升；
- 版本回看 v1 仅本机；
- 配额与"换代屏障"是产品级运维负担；
- relay 存储成本与 GC 正确性要求高。

---

## 6. 已知边界（安全，必须写进 ADR）

- SQLite / WAL / SHM、Tantivy 索引、临时文件、导出目录、本地备份**均可能明文**。
- **DPAPI 只护钥不护笔记**；同一 Windows 用户下的恶意进程 / 内存读取不可抵御；
  安全依赖 BitLocker / OS。
- relay 可见对象数量、大小、时序、IP。
- E2E **不防**恶意 relay 删数据；新设备无可信旧水位时无法独立识别"有效签名的旧快照回滚"。
- 已被吊销设备**已经获得**的明文与历史不可撤回。

---

## 7. 附录：DDL 草图（SQLite STRICT 表，要求 ≥3.37；非实际建库代码）

```sql
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA foreign_keys=ON;
PRAGMA busy_timeout=5000;

CREATE TABLE meta(key TEXT PRIMARY KEY, value BLOB NOT NULL) STRICT;

CREATE TABLE loro_docs(
  doc_id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN('structure','body')),
  generation INTEGER NOT NULL DEFAULT 1, app_doc_schema INTEGER NOT NULL,
  codec_version TEXT NOT NULL, min_reader INTEGER NOT NULL,
  snapshot BLOB, snapshot_seq INTEGER NOT NULL DEFAULT 0, snapshot_frontier BLOB,
  head_seq INTEGER NOT NULL DEFAULT 0, head_frontier BLOB, head_vv BLOB,
  CHECK(head_seq>=snapshot_seq)
) STRICT;

CREATE TABLE loro_updates(
  doc_id TEXT NOT NULL REFERENCES loro_docs(doc_id), seq INTEGER NOT NULL,
  update_id TEXT NOT NULL UNIQUE, batch_id TEXT NOT NULL,
  codec_version TEXT NOT NULL, payload BLOB NOT NULL,
  end_frontier BLOB NOT NULL, end_vv BLOB NOT NULL,
  PRIMARY KEY(doc_id,seq)
) STRICT;

CREATE TABLE notes(
  note_id TEXT PRIMARY KEY,
  body_doc_id TEXT NOT NULL UNIQUE REFERENCES loro_docs(doc_id),
  title TEXT NOT NULL, body_md TEXT,
  body_seq INTEGER NOT NULL DEFAULT 0, body_frontier BLOB,
  meta_seq INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  deleted INTEGER NOT NULL CHECK(deleted IN(0,1)),
  is_favorite INTEGER NOT NULL CHECK(is_favorite IN(0,1)),
  is_pinned INTEGER NOT NULL CHECK(is_pinned IN(0,1))
) STRICT;
CREATE INDEX notes_recent ON notes(updated_at DESC,note_id) WHERE deleted=0;
CREATE INDEX notes_flags ON notes(is_favorite,is_pinned,updated_at DESC) WHERE deleted=0;

CREATE TABLE tree(
  node_id TEXT PRIMARY KEY, parent_id TEXT REFERENCES tree(node_id) DEFERRABLE INITIALLY DEFERRED,
  note_id TEXT UNIQUE REFERENCES notes(note_id), title TEXT NOT NULL,
  sibling_rank INTEGER NOT NULL, deleted INTEGER NOT NULL CHECK(deleted IN(0,1)),
  meta_seq INTEGER NOT NULL
) STRICT;
CREATE INDEX tree_parent_order ON tree(parent_id,sibling_rank,node_id);

CREATE TABLE tags(tag_id TEXT PRIMARY KEY,name TEXT NOT NULL,normalized_name TEXT NOT NULL) STRICT;
CREATE INDEX tags_name ON tags(normalized_name);

CREATE TABLE note_tags(
  note_id TEXT NOT NULL REFERENCES notes(note_id),tag_id TEXT NOT NULL REFERENCES tags(tag_id),
  PRIMARY KEY(note_id,tag_id)
) STRICT;
CREATE INDEX note_tags_reverse ON note_tags(tag_id,note_id);

CREATE TABLE chunks(
  chunk_id INTEGER PRIMARY KEY,note_id TEXT NOT NULL REFERENCES notes(note_id),ordinal INTEGER NOT NULL,
  start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,body_seq INTEGER NOT NULL,
  parse_version INTEGER NOT NULL,plain_text TEXT NOT NULL,
  UNIQUE(note_id,ordinal),CHECK(end_byte>=start_byte)
) STRICT;
CREATE INDEX chunks_note_seq ON chunks(note_id,body_seq);

CREATE TABLE links(
  from_note_id TEXT NOT NULL REFERENCES notes(note_id),start_byte INTEGER NOT NULL,
  end_byte INTEGER NOT NULL,to_note_id TEXT,target TEXT NOT NULL,kind TEXT NOT NULL,body_seq INTEGER NOT NULL,
  PRIMARY KEY(from_note_id,start_byte,target)
) STRICT;
CREATE INDEX links_backlinks ON links(to_note_id,from_note_id);

CREATE TABLE assets(
  asset_hash BLOB PRIMARY KEY CHECK(length(asset_hash)=32),byte_len INTEGER,media_type TEXT,data BLOB,
  orphan_since INTEGER,CHECK(data IS NULL OR (byte_len IS NOT NULL AND length(data)=byte_len))
) STRICT;

CREATE TABLE asset_refs(
  note_id TEXT NOT NULL REFERENCES notes(note_id),asset_hash BLOB NOT NULL REFERENCES assets(asset_hash),
  start_byte INTEGER NOT NULL,body_seq INTEGER NOT NULL,
  PRIMARY KEY(note_id,asset_hash,start_byte)
) STRICT;
CREATE INDEX asset_refs_reverse ON asset_refs(asset_hash,note_id);

CREATE TABLE versions(
  version_id INTEGER PRIMARY KEY,note_id TEXT NOT NULL REFERENCES notes(note_id),
  captured_at INTEGER NOT NULL,generation INTEGER NOT NULL,frontier BLOB NOT NULL,
  title TEXT NOT NULL,body_zstd BLOB NOT NULL,asset_hashes BLOB NOT NULL
) STRICT;
CREATE INDEX versions_retention ON versions(note_id,captured_at DESC);

CREATE TABLE projection_outbox(
  doc_id TEXT PRIMARY KEY REFERENCES loro_docs(doc_id),target_seq INTEGER NOT NULL
) STRICT;

CREATE TABLE index_outbox(
  note_id TEXT PRIMARY KEY REFERENCES notes(note_id),target_body_seq INTEGER NOT NULL,
  target_meta_seq INTEGER NOT NULL,index_version INTEGER NOT NULL,
  action TEXT NOT NULL CHECK(action IN('upsert','delete'))
) STRICT;

CREATE TABLE sync_outbox(
  message_id TEXT PRIMARY KEY,batch_id TEXT NOT NULL,doc_id TEXT NOT NULL,seq INTEGER NOT NULL,
  key_epoch INTEGER NOT NULL,ciphertext BLOB,
  FOREIGN KEY(doc_id,seq) REFERENCES loro_updates(doc_id,seq)
) STRICT;
CREATE INDEX sync_batches ON sync_outbox(batch_id);

-- 完整迁移事务最后设置 PRAGMA user_version=1。
```

DDL 说明（实现时勿丢）：

- `notes/tree/tags/note_tags/chunks/links/asset_refs` 是投影，**仅投影器写**。
- `tree.sibling_rank` 是从 `LoroTree` 遍历生成的 **dense rank**，**别猜** native fractional-index 编码
  在 SQLite 里可直接字典排序。
- `tags.normalized_name` **不设 UNIQUE**，避免并发创建同名标签导致 SQL 投影失败。
- `assets.data IS NULL` 表示**未下载**，不是空内容。
- outbox 任务可合并，但须**按版本 CAS 确认**；`sync_outbox` 指向的 update 不得被 checkpoint GC 删除。
- `versions.asset_hashes` 必须参与 **GC 根扫描**。
- 正式 schema 可加状态 / 错误列，**不改变权威归属**。

---

## 8. 落地顺序（8 步）

| 步 | 内容 | 验收 |
| --- | --- | --- |
| 1 | ADR + 契约：ID、权威归属、编辑坐标、两阶段 frontier、envelope/`min_reader`、删除恢复语义 | 完整数据流**无双权威** |
| 2 | 存储最小链路：单文档编辑 / 两副本并发 / 快照重放 / 两事务故障注入 | 崩溃后只丢未 ACK 内存批；head/body 不同必重建；peer 复制被阻止。**此步前禁止并行业务写入实现** |
| 3 | 编辑器垂直切片：真实标记、局部 op、增量解析、IME、光标锚点 | Windows Unicode / 1 MiB / 长行性能门槛通过 |
| 4 | 领域与资产：树移动、标签、删除恢复、收藏置顶、BLOB、单向导出、SQLite 备份恢复 | 两副本"删除 × 编辑"、"移动 × 移动"并发不丢内容；备份复原含图片 |
| 5 | 派生检索：`chunks`/`links`/outbox、Tantivy + cang-jie | 中英混搜；重复消费与索引损坏可全重建；已删笔记不返回 |
| 6 | 密钥与配对：DPAPI、口令/恢复槽、认证 envelope、轮换 | 改口令不用重加密历史；篡改被拒；换 Windows 可凭恢复材料恢复 |
| 7 | 自建 relay：持久日志、两 checkpoint GC、manifest bootstrap、VV 补偿、设备吊销 | 所有其他客户端关闭时新设备仍能完整恢复；长离线编辑合并；被吊销设备拿不到新 epoch；配额不暗删 |
| 8 | Windows 集成：两设备端到端、断网/断电故障矩阵、IME 大笔记、备份恢复、打包 | `fmt`/`clippy`/`test` + 实际 Windows 验收，由 Lead 亲验 |

并行门槛：**第 2 步通过后**，3 / 4 / 6 可在冻结接口与不重叠写域下并行；5 等 3、4；7 等 6；8 集成串行。
只读 `rmcp`、向量检索等**不是**核心验收阻塞项。

---

## 9. 一句话

**先完成"单篇笔记的区间编辑 → CRDT → durable op → 崩溃重放 → 两副本合并 → 带 frontier 物化"的契约验收；
不先堆 UI，不先写 relay。**
