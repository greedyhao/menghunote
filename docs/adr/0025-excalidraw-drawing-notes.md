# ADR 0025：Excalidraw 画布笔记（`.excalidraw`）

- 状态：已接受（2026-11-19）
- 决策者：用户（产品取舍）+ Lead（落地方案）
- 相关：ADR 0001（SQLite 单库）、ADR 0019（Object Block）、ADR 0020（插件契约）

## 背景

用户希望在笔记里使用 Excalidraw 画布，并像 Obsidian 一样把画布当图片插入：

```md
![[流程图.excalidraw]]
```

外部实现 `gpui-excalidraw 0.1.9` 已 vendor 到 `crates/excalidraw`，并适配
`gpui-kit = "=0.10.0"`。当前插件 API 只有 manifest/host 契约，没有 UI/运行时；
因此本轮是**第一方编入模块**，不是第三方运行时插件。

## 选项

### A. SQLite 为真源 + `.excalidraw` 导入/导出

- 画布是普通笔记，标题以 `.excalidraw` 结尾；正文存 `.excalidraw` JSON。
- 不新增 schema / 同步消息；打开时在普通 `EditorView` 上叠 Excalidraw 编辑器。
- `.excalidraw` 文件只作为导入/导出格式。

代价：数据库是内部真源，用户不能直接编辑 vault 里的文件。

### B. `.excalidraw` 文件为真源

- vault 下放真实文件，SQLite 只做索引。
- 更符合文件直觉，但要新增文件监听、外部编辑冲突、文件同步/备份/GC 语义，
  与 ADR 0001「SQLite 单库」冲突。

## 决定

选 **A**。

- 画布笔记用**标题后缀**识别（大小写不敏感），不新增 `note_kind` 列。
- 正文就是 `.excalidraw` JSON 信封，同步/撤销/物化/搜索沿用普通笔记链路。
- 保存用「整篇替换」写回正文：一条 `EditRequest` = 一个撤销组，走现有同步 outbox。
- 编辑器是**内容区**覆盖层（侧栏保持可见）。vendor 的鼠标命中直接使用窗口
  坐标，因此在 `crates/excalidraw` 的 `canvas` paint 里记录画布原点，所有画布
  鼠标命中先减原点；不把覆盖层做成全窗口。
- 保存/关闭动作在 shell 底栏状态栏（不遮挡画布工具栏）；Esc/点外部不关闭，避免误丢。
- 本轮不做 `![[xxx.excalidraw]]` 嵌入预览；后续切片再做 Object Block 预览。

## 后果

- 不触碰 storage schema、sync/relay/wire；旧 vault 无需迁移。
- 画布正文是 JSON，搜索/图谱会按普通笔记索引，暂不做场景内检索。
- 文件真源、外部编辑、导入/导出 UI、Markdown 嵌入预览均后置。
- 升级 `gpui-kit` 时必须同步改 `crates/excalidraw/Cargo.toml` 的精确 pin，
  否则会出现两套 GPUI 类型。
