# 附件插入（开发版）——实现说明与非目标

> 任务：用户在真实 AppShell 里用**绝对路径**把本地文件插入为资产，正文写入
> `[附件：<文件名>](asset://<hex>)`，同步后另一台设备能取到**相同资产字节**。
> 开发版（`--features dev-sync`）才有 UI；默认构建零变化。

## 1. 数据流

```text
路径输入（attachment-path）
  → attachment::read_attachment()        // 校验 + 读字节 + sha256 + Markdown（无副作用）
  → EditorView::attachment_insert_offset()  // 光标在 source 字节里的起点（持闸/组合中拒绝）
  → attachment::store_attachment()       // insert_asset(hash,len,mime,data) + link_asset_ref
  → EditorView::insert_attachment_markdown() // 一个 EditRequest = 一个撤销单元
       └─ 失败 → rollback_attachment_ref()（资产保留：内容寻址、无副作用）
```

- **资产表**：`assets(asset_hash, byte_len, media_type, data, orphan_since)` 是内容寻址的；
  `asset_hash = sha256(文件字节)`。同内容重复插入 → `insert_asset` 幂等（单资产），
  `asset_refs` 每次插入新增一行（`start_byte` 是链接在正文 source 坐标里的起点）。
- **哈希来源**：`sha2` 本来就是 `storage` 的依赖（基线快照/同步桥在用），所以
  **没有给 app 新增依赖**：`menghunote_storage::asset_content_hash(data)` 暴露纯计算。
- **Markdown**：链接文本是文件名（转义 `\` `[` `]` `(` `)`，换行压成空格），目标是
  `asset://<64 位小写十六进制 sha256>`。`asset://` 保持为**普通链接文本**：既有 Markdown
  解析/导出语义不变，导出仍是源文（链接原样写出）。
- **失败语义**：读文件/全部校验都在任何写入之前；落库失败不插入 Markdown；编辑器事务
  失败回滚引用。**资产可以保留**（内容寻址，删了要重算且可能仍被别处引用），报告给用户。

## 2. UI（只在 `--features dev-sync`）

| selector | 作用 |
| --- | --- |
| `attachment-insert` | 侧栏底部「插入附件…」；禁用态淡化但仍可点（点它给出明确原因） |
| `attachment-hint` | 禁用原因常驻文案（无活动笔记 / 同步持闸 / 缺 note_id） |
| `attachment-path` | 路径输入（复用行内重命名的单行控件 `RenameInput`：焦点/IME/选区/`Ctrl+A`） |
| `attachment-submit` / `attachment-cancel` | 插入 / 取消（回车与 `Esc` 等价） |
| `attachment-error` | 失败原因（**不含绝对路径**） |
| `attachment-notice` | 成功提示（文件名 + 字节数） |

可见性：整个模块/字段/渲染分支都在 `#[cfg(feature = "dev-sync")]` 下；默认构建里连类型都
不存在。附件插入是**纯本地**动作（不联网、不用身份），所以它不额外要求"面板已挂载"——
headless 测试可以用 `AppShell::new` 直接验证；生产 Main 的 dev-sync 构建必定有面板，
两者不会错位。

为什么放侧栏底部而不是编辑器顶部：`editor-pane` 左上角是既有 headless 测试点击
"聚焦正文"的坐标，插一条工具栏会让那些点击落到工具栏上（回归）。侧栏底部不占编辑器
命中区。

硬前置：必须有**打开的笔记**、编辑器**不处于同步持闸**（当前唯一的"只读"窗口期）、
结构投影里该节点有 `note_id`。三条任一不满足 → 按钮淡化 + `attachment-hint` 说明，
点击只写 `attachment-error`。

## 3. 同步：哪些已有、哪些是本次补的

- **上传（已有，未改 wire/DDL）**：`CheckpointWriter::prepare(&StorageAssets)` 读 `assets`
  表 → `upload_object_with_reservation`（分片 + `submit_object`）→ relay 为每个 ASSET
  追加一条 `asset` 日志事件。所以资产随**发布基线**（以及轮换）走；普通 `SyncOnce`
  只处理 UPDATE outbox。
- **拉取（本次补的最小接线）**：`SyncEngine::pull` 消费 ASSET 事件时本来就会**真实下载
  并验证**（评审 0003 P0-B），但只落一条 `skipped-asset` 记录、**丢弃明文**——于是
  已存在的第二台设备同步后 `asset_bytes` 取不到。现在：
  - `upload::fetch_verified_asset_data()` = 同 [`fetch_verified_asset`] 的验证口径，
    额外交还已验证的资产数据字节（事实集入口保持原样，checkpoint 复核不碰明文）；
  - `SyncEngine::persist_skipped_asset` 在写 `skipped-asset` 记录**之前**调用
    `storage::insert_asset`（幂等；同 hash 不同字节 → `Corrupt` fail-closed，不写记录、
    不推游标）；
  - 覆盖证明语义不变：ASSET 仍必须由 durable `skipped-asset` 记录证明，记录里的事实全部
    来自本机验证。
- **为什么不给普通 `SyncOnce` 加资产上传**：relay 对同一对象要求"全部分片要么都走
  maintenance reservation、要么都不走"（`put_part` 的 `reservation_id` 一致性检查）。
  普通预算上传资产后再发布基线会 `ID_CONFLICT`，属于改 wire 语义/预算分类，超出本任务；
  因此保持"资产由发布基线/轮换上传"，并把它写进限制。

## 4. 非目标（明确不做）

- **内联图片预览 / 图片渲染**：`asset://` 不解析、不显示缩略图。
- **拖放**：没有 `on_drop` / 拖入文件。
- **系统文件对话框**：`rfd` 被禁用，本片不引入任何文件选择器依赖。
- **附件管理 UI**：不列附件、不删除、不重命名；没有资产 GC 删除入口。
- **`asset://` 的点击打开/导出为独立文件**：导出仍只写源文 Markdown。
- **元数据同步**：`asset_refs` 是本机账本，不随同步复制；跨设备以正文链接为准。
- **新 wire 消息 / 新 DDL / 新依赖**：一概没有（唯一的存储新增是
  `asset_content_hash` 纯函数）。
- **秘密**：不适用（附件不是秘密材料；日志只写哈希前 4 字节与字节数，不写路径/文件名）。

## 5. 测试与数字（2026-11-14，Linux）

| 命令 | 结果 |
| --- | --- |
| `cargo test -p menghunote-app`（默认构建） | 134 passed（不受影响） |
| `cargo test -p menghunote-app --features dev-sync` | 349 passed（新增 `attachment_ui` 4 + 两 shell 资产 1） |
| `cargo test -p menghunote-sync --all-targets` | 536 passed（+5 doctests） |
| `cargo test -p menghunote-storage` | 171 passed |
| `cargo clippy -p menghunote-app --all-targets --features dev-sync -- -D warnings` | 干净 |
| `cargo clippy -p menghunote-storage -p menghunote-sync --all-targets -- -D warnings` | 干净 |

无 `sleep`：headless 测试用 `run_until_parked`，两实例测试用面板 `dev_sync_pump` 的
明确收敛点（`dev_sync_sync_in_progress() == false`）；临时目录都在 `CARGO_TARGET_TMPDIR`。

## 6. Windows 验点（未在本机跑）

- `C:\Users\...\报告 最终.pdf`（反斜杠/中文/空格）可作为绝对路径插入；
- UNC `\\server\share\x.bin` 能被识别为绝对路径；读失败/超时给出明确错误（不 panic）；
- IME 组合中提交被拒绝并提示（先上屏或 `Esc`），不产生半链接；
- 无内联预览/无拖放/无系统对话框（与 Linux 行为一致）。
