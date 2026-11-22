# Bases 适配方案（2026-11-21 裁决后修订，草案）

> 状态：**草案，M0 规范核对后转 ADR**。当前仓库没有 Bases 代码。
>
> **规范锚点不在本文拍死**：候选 Obsidian **1.15.1**（2026-11-12 发布）作为
> 2026-11-21 现场查询结果；M0 第一条必须从应用 About 页/官方 release notes 现场核实
> 并写入 `spec/SPEC_LOCK.md`，不沿用任何一方给的日期。优先做**本地存档**，
> `obsidian-help` commit 只作为增强、不是依赖。

参考：
- [Bases syntax](https://obsidian.md/help/bases/syntax)
- [Functions](https://obsidian.md/help/bases/functions)
- [Views](https://obsidian.md/help/bases/views)
- [Formulas](https://obsidian.md/help/bases/formulas)

---

## 1. 结论摘要

| 项 | 决策 |
|---|---|
| `.base` 本体 | **一篇普通文档**，body = YAML 文本；随库同步、版本、搜索 |
| Bases 行数据 | 查现有 `notes` / `tree` / `links` / `tags` / frontmatter 物化；**不扫文件系统、不引入 `notify`** |
| `.base` 配置写回 | **v1 不做可视化编辑**；打开默认渲染，切源码模式手改 YAML，走普通文档保存 |
| 单元格写回 | 只写**目标笔记的 frontmatter**，用目标笔记的文本区间替换 |
| 外部互操作 | 显式**导入/导出 `.base`**；当前导出固定 `.md`，必须新增规则 |
| 规范基线 | M0 现场核实 + 本地存档；候选 Obsidian 1.15.1 |
| v1 工期 | **18 周**（M0 1 + M1 3 + M2 4 + M3 3 + M4 3 + M5 2 + M6 2） |
| 非目标 | 文件 watcher、跨库、map/kanban/插件视图、Dataview 内联字段、`.base` 配置可视化编辑、锚点/别名编辑 |

核心边界：

1. `.base` 不是第二套文件真源；它是库内文档。
2. 行数据来自 storage 物化表，不另建 `bases-index`。
3. Obsidian 直接打开的前提是磁盘存在 `.base` 文件；本应用通过导入/导出提供，不做实时文件监听。
4. UI 不解析表达式；core 只接受“`.base` 文本 + 属性记录”。
5. **`.base` 不写回配置，单元格只写目标笔记**；两件事的测试和风险分开。

---

## 2. 架构

```text
┌──────────────────────────────────────────┐
│ bases-ui      gpui-kit 0.10.0            │
│ BaseView / TableView / CardsView / List  │
│ CellRenderer / CellEditor / 跳转          │
├──────────────────────────────────────────┤
│ bases-query   只读查询层                  │
│ notes/tree/links/tags/frontmatter 物化    │
│ storage 变更事件驱动；不扫盘              │
├──────────────────────────────────────────┤
│ bases-core    ★ 零 UI/零 IO              │
│ .base 语义模型 / 词法 / 语法 / AST / 求值 │
│ 类型系统 / 函数表 / 依赖图 / 资源上限      │
└──────────────────────────────────────────┘
```

三条不可破规则：

1. `bases-core` 不 `use` UI、文件系统、Vault；输入是“`.base` 文本 + 一组 `NoteRecord`”。
2. UI 只消费求值结果，不解析表达式。
3. 单格求值失败 → 该格错误态，不冒泡成整表崩溃。

---

## 3. 文档与数据模型

### 3.1 `.base` 文档类型契约

**唯一判定入口**（纯函数、无 IO，数据层/UI 都调用它，UI 不得自行猜）：

```rust
enum DocKind { Base, Note }

fn doc_kind(title: &str) -> DocKind {
    if title.ends_with(".base") { DocKind::Base } else { DocKind::Note }
}
```

v1 只认 `.base`；`.base.md` 是普通笔记，不做隐式转换（避免导出成 Obsidian 不认的后缀）。

| 场景 | 规则 |
|---|---|
| 打开 | `DocKind::Base` → `bases-ui`；YAML 解析失败 → **源码视图 + 错误提示**，不丢内容 |
| 视图 ⇄ 源码 | 双向切换；源码保存走普通文档保存 |
| 重命名去掉 `.base` | 提示一次“将失去 Base 视图”，不阻止 |
| 重命名加上 `.base` | 直接识别为 Base，无需迁移 |
| 搜索 / 图谱 / 反向链接 | 与普通笔记一致，YAML 当正文纳入全文索引 |
| `[[Books.base]]` | 按标题正常解析，无特判 |
| 导出 | 后缀 `.base` 时保留 `.base`，内容写 body YAML；不拼 `.md` |
| 导入 | 文件选择/拖入 `.base` → 新建标题为该文件名的文档；重名生成 `Books (2).base`，不覆盖 |
| 同步/版本 | 与普通文档完全相同 |

### 3.2 行数据来源（不新建索引）

| Bases 属性 | 来源 | 现状/缺口 |
|---|---|---|
| `file.name` / `path` / `folder` / `ext` | `tree` + `notes`/标题推导 | 可复用；路径是库内虚拟路径 |
| frontmatter 属性 | `notes.body_md` 正文 | **无结构化物化**；需解析缓存 |
| 结构标签 | `note_tags`/`tags`（结构 CRDT） | 可复用 |
| 正文 `#tag` / frontmatter tags | 无 | **需扫描并与结构标签合并** |
| `file.links` | `links`（当前仅 `kind='wiki'`） | **需补 Markdown 链接/ frontmatter 链接** |
| `file.embeds` | 无物化 | **需补**（parser 已有 image/drawing） |
| `file.backlinks` | `links` 反向查询 | 仅 wiki；扩展 kind 后复用 |
| `file.ctime/mtime` | `notes.created_at/updated_at` | **库内文档时间，不是文件系统时间**；列入已知差异 |
| `file.size` | body 字节数 | **正文字节，不是磁盘文件大小**；列入已知差异 |

**决策**：M1 = 3 周。拆解：

| 项 | 周 |
|---|---:|
| frontmatter 解析 + 按 `body_seq` 失效的缓存 | 1 |
| inline `#tag` 扫描、frontmatter tags、与结构标签合并 | 0.5 |
| Markdown links / embeds / backlinks kind 物化 | 1 |
| `file.*` 语义表 + 单测 | 0.5 |

若 v1 不做这些，则从承诺中删除 `file.tags/links/embeds/backlinks` 并列入“已知差异”；不能既承诺又不补。

### 3.3 值类型（待 M0 核对）

```rust
enum Value {
    Null, Bool(bool), Number(f64), Str(String),
    Date(DateOnly), DateTime(DateTime<FixedOffset>), Duration(DurationValue),
    List(Vec<Value>), Link(LinkRef), File(FileRef), Url(UrlRef),
    Unknown,
}
```

M0 逐条核对：
- `Duration` 的精确形态与日期运算；
- `File` / `URL` 是否独立类型；
- `not:` 是否只能列表；
- `%` 与 `list[i]` / `obj["k"]`；
- `Link` 相等性：同一文件，或未解析时 link text 相同；
- `Unknown` 在 `&&/||/!`/比较中的真值。

### 3.4 表达式 AST（草案）

```rust
enum Expr {
    Lit(Lit),
    Prop(PropPath),              // note.x / file.name / formula.y / this.z
    Index(Box<Expr>, Box<Expr>), // list[0] / obj["k"]
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Call(FuncName, Vec<Expr>),
    List(Vec<Expr>),
}
```

### 3.5 filters

- 全局 `filters` 与 view `filters` **按 AND 合并**。
- `and` / `or` / `not` 是递归对象；`not` 的值按官方快照核对列表形态。
- 同一 filter 对象只能出现 `and|or|not` 之一；同时出现要拒绝并给源码位置错误。
- 列表默认按 `and`。

### 3.6 公式依赖

1. 解析 `formula.*` 引用；
2. 拓扑排序；
3. 循环 → 明确错误，不进入求值；
4. 每个公式的错误只影响该列/该格，不炸整表。

### 3.7 写回拆成两个目标（关键修正）

| | 目标 1：`.base` 文件本体 | 目标 2：单元格 → 目标笔记 frontmatter |
|---|---|---|
| 改什么 | `filters`/`sort`/`order`/`groupBy`/`formulas`/`properties` | 被查询那篇笔记的某个属性 |
| 谁触发 | 用户改 Base 配置 | 用户点表格单元格编辑 |
| 丢失风险 | `.base` 自身的注释/引号/锚点 | **目标笔记** frontmatter 的注释/引号 |
| v1 是否必须 | **否**：默认只读渲染，切源码模式手改 | **是**：Bases 的核心交互 |

**目标 1 定稿**：v1 不做 `.base` 配置的可视化编辑。打开默认渲染视图，可切源码模式
手改 YAML，保存走普通文档保存。因此：

- `.base` 文件只需**读**，不需要格式保留写回；
- 锚点/别名/重复键问题降级为“源码编辑器里用户自理”；
- M5 的 `.base` 格式保留工作量归零。

**目标 2 定稿**：单元格编辑只写目标笔记的 frontmatter，采用**目标笔记文本区间替换**
（方案 b），成本约 1 周，计入 M4。不能用属性面板整段重写；验收对**被编辑的那篇笔记**
做文本 diff：

- 除目标值区间外逐字节一致；
- 注释数量与内容不变；
- 引号风格、空行、未知字段不变；
- 新增键追加在 mapping 末尾。

方案 a（整段重写）只允许作为已知差异；方案 c（只读）是缩范围备选，不推荐。

---

## 4. v1 范围

### 纳入

- 读 `.base`、渲染 table/cards/list
- filters（global + view，AND 合并，`and/or/not`）
- formulas + 依赖排序 + 循环检测
- properties / displayName
- views：未知 type 保留配置、不崩
- `summaries`（全局 + view + 默认聚合）
- `groupBy` + `groupOrder` + `order` + `sort` + `limit`
- 值类型与函数表按 M0 快照
- `this` 主视图上下文
- 单元格编辑 → **目标笔记 frontmatter 区间替换**
- 未知函数/单元格错误降级
- 资源上限（YAML 深度、AST 深度、列表长度、求值规模）
- `.base` 导入/导出 + 源码模式
- 文档类型契约（`doc_kind`）

### 不做

- `.base` 配置的可视化编辑/格式化
- map / kanban / 插件视图
- 跨库 / 远程库
- Dataview 内联字段 `key:: value`
- 锚点/别名编辑
- 文件系统 watcher
- 嵌入 base（`base` code block）

发布必须附**已知差异清单**，不能宣称“规范覆盖仅剩 map”。

---

## 5. 导入/导出与文档契约

当前 `crates/app/src/export.rs` 固定导出 `<标题>.md`；标题 `Books.base` 会变成
`Books.base.md`，Obsidian 不会识别为 Base。

v1 必须做：

1. **导出**：`doc_kind(title) == Base` 时，文件名保留 `.base`，内容写 body YAML；不追加 `.md`。
2. **导入**：文件选择/拖入 `.base` → 创建标题为 `*.base` 的文档，body 写原文件文本。
3. **不做**文件系统 watcher；`.base` 的实时外部编辑不在 v1。
4. 导入重名生成 `Books (2).base`，不覆盖；大小写按现有导出登记规则处理。

工时：约 3–5 天（含导出路径测试、导入路径、重名/大小写）。

---

## 6. 里程碑（18 周）

| 里程碑 | 周 | 交付 |
|---|---:|---|
| M0 规范锁定 | 1 | 现场核实版本 + 本地存档 + 函数/类型/视图逐条签字 + fixture |
| M1 数据层 | 3 | frontmatter 缓存、tags 合并、links/embeds 物化、`file.*` 语义表 |
| M2 表达式引擎 | 4 | 词法/AST/求值 + 类型 + Clock/This + 语义表 + 资源上限 |
| M3 table | 3 | summaries、groupOrder、order/sort/groupBy/limit、未知 view 降级 |
| M4 cards/list + 单元格编辑 | 3 | 含目标笔记 frontmatter 区间替换 |
| M5 互操作 + 验收 | 2 | `.base` 导入/导出、文档类型契约、目标笔记文本 diff |
| M6 打磨 | 2 | 用户文档、已知差异清单、打包 |

回到 16 周的三条路径（都要改承诺，不能藏进 M1）：

| 砍掉 | 省 | 代价 |
|---|---:|---|
| cards + list | 1 周 | 只剩 table |
| 单元格只读 | 0.5 周 | 失去双向价值，不建议 |
| 不补 embeds / backlinks | 0.5 周 | 从 `file.*` 承诺删掉并列入差异 |

---

## 7. 测试轨道

| 轨道 | 内容 | 门禁 |
|---|---|---|
| ① 规范一致性 | M0 快照逐条 fixture | CI |
| ② 目标笔记 frontmatter 字节 diff | 除目标值区间外字节一致 | CI |
| ③ `.base` 源码模式 | 只读打开不重写；源码保存逐字节保留 | CI |
| ④ Obsidian 差分抽样 | 人工比对渲染 | 发布前检查，不做 CI |
| ⑤ 资源上限 | 畸形 YAML/超深 AST/超长列表 | CI |

验收：

- 任意合法 `.base` 打开不崩，能渲染；
- 解析失败进源码视图，不丢内容；
- 单格失败不炸整表；
- 单元格编辑后，目标笔记除目标值区间外字节一致；
- `.base` 在视图模式下不被重写；源码保存不格式化；
- 循环公式明确报错；
- `file.*` 已知差异清单完整；
- 资源上限触发时返回错误态，不挂死。

---

## 8. 已决事项 / 未决事项

### 已决

1. `.base` 作为文档；行数据查 storage；不建文件索引、不用 notify。
2. `.base` 配置：v1 只读渲染 + 源码模式，不做可视化写回。
3. 单元格写回：只写目标笔记 frontmatter，用目标笔记文本区间替换（方案 b）。
4. M1 = 3 周（补物化）；总工期 18 周。
5. 导出/导入 `.base` 是 v1 范围，不是默认成立。
6. v1 必须包含 `summaries`、`groupOrder`、global/view filters AND、未知 view 保留、全文件类型、`this`。
7. 资源上限进 v1；性能基准延后。
8. 文档类型：标题后缀 `.base`，唯一 `doc_kind` 入口；不引入 note kind DDL。
9. frontmatter 物化：v1 用按 `body_seq` 失效的内存缓存，不新增表/DDL。
10. `file.ctime/mtime/size` 定义为库内文档时间/正文字节，写进已知差异。
11. `.base` 导入：文件选择 + 拖入；重名不覆盖。

### 未决（M0 前必须拍板）

1. M0 现场核实版本号/发布日期/文档快照，并写入 `spec/SPEC_LOCK.md` 与 `spec/snapshot/`。
2. `obsidian-help` 是否能拿到 commit；拿不到就只依赖本地存档，不阻塞。
3. `file.tags/links/embeds/backlinks` 是否全部进 v1；若砍，更新承诺与差异清单。
4. `.base` 源码模式切换的 UI 入口与错误态文案。
5. M0 的 `UVERIFIED` 函数/类型清单如何冻结到 M1 前。

---

## 9. 风险

| 风险 | 影响 | 对策 |
|---|---|---|
| storage 物化不完整 | 行数据缺列/属性错 | M1 3 周；先补 frontmatter/tags/links/embeds，否则缩承诺 |
| 目标笔记 frontmatter 被整段重写 | 丢用户注释/引号 | 区间替换 + 目标笔记字节 diff |
| `.base` 配置只读 | 用户不能可视化改 filter/sort | 源码模式；已知差异；后续再评估 |
| Obsidian 版本漂移 | 解析/求值对不上 | M0 现场核实 + 本地存档；差异清单 |
| 未知 view/函数 | 打开即崩或空表 | 保留配置 + `Unknown` + 错误态 |
| 大库/大表性能 | 页面卡死 | 资源上限 v1；性能基准发布前 |
| 文件 I/O 被误解为实时同步 | 用户以为 Obsidian 改动自动进来 | 文档明确“仅导入/导出” |

---

## 10. M0 第一周产出

1. `spec/SPEC_LOCK.md`：版本、release date、snapshot date、来源、抓取时间；本地存档 `spec/snapshot/`。
2. 函数表逐条签字（名称、参数、返回类型、边界行为），未核实标 `UNVERIFIED`，不猜。
3. 类型系统确认：`Duration`、`Index`、`File`/`URL`、`not` 形态。
4. filters 三种形态组合矩阵 + `and/or/not` 互斥校验。
5. 每个 `views[].type` 一条 fixture，含至少一个未知类型降级。
6. 最小 `parse → 求值 → 打印` 探针，跑通 `note.x == "y" && file.hasTag("a")`。

第 6 条是本周唯一需要写代码的；其余是规范/测试数据。当前不进入 M1 实现。
