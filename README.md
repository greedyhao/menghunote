# menghunote

基于 [GPUI](https://gpui.rs/) 的**桌面端优先**笔记软件，**Windows 为首要平台**，使用 Rust 开发。

编辑器采用 **Typora 式原位所见即所得**：编辑器里的文本就是 Markdown 源码，`**` 等语法标记真实存在、默认隐藏，光标进入时才显现。**SQLite 单库（WAL）是持久化的唯一真源**，Markdown 只做单向导出，不做外部文件编辑或监听。

> 这是个人/实验性项目。端到端加密同步与自建 relay 已实现并默认启用同步 UI，但按 [`AGENTS.md`](AGENTS.md) §8：真实鉴权 relay、Windows DPAPI、TLS 系统根、多设备真机仍待验，**不打生产 tag**，请勿用于生产数据。
>
> [`AGENTS.md`](AGENTS.md) 是本仓库的**行为契约**，优先级高于任何临时指令；架构取舍、依赖基线、当前状态与协作纪律都以它为准。

## 主要功能

| 领域 | 已实现 |
| --- | --- |
| 编辑 | 源码投影 WYSIWYG（隐藏标记 / caret 进入显现）、语义样式、按块渲染（标题字号、代码块等宽、可变行高）、表格对象（增删行列、单元格就地编辑、行内 Markdown）、任务列表、IME 输入闭环、撤销/重做、常用快捷键 |
| 组织 | 侧栏笔记树与文件夹、多文档切换、目录折叠、行内重命名（F2 / 右键）、拖拽移动、右键新建/两步删除、仓库（vault）新建与切换、回收站、笔记本地编辑历史 |
| 检索 | `Ctrl+K` 全文检索（Tantivy + 中文分词）、结果总数、大小写与整词开关、重建进度条、按笔记分组折叠、多行摘要、跳转命中行 |
| 富内容 | 图片/附件粘贴与拖放（内容寻址去重、整篇引用对账）、Markdown 链接点击打开、Wiki 双链 `[[Note]]` 与反向链接、关系图谱、Mermaid 预览、代码块语法高亮、Excalidraw 画布笔记（`.excalidraw`）、YAML frontmatter 属性面板 |
| 外观与扩展 | 应用级主题（浅色 / 深色 / 跟随系统）、中英 i18n、第一方插件（模板 / 日记）、设置分类导航、快捷图标栏、侧栏排序 |
| 同步 | 端到端加密同步（HPKE / Ed25519 / Argon2id / 恢复码、设备吊销、密钥轮换、checkpoint 快速追赶）、自建 relay（多 vault、邀请链接、双签审批）、自动同步两阶段持闸、整库导出 Markdown |
| 工程质量 | 原生 headless UI 测试（`#[gpui::test]`）、进程内 relay e2e、golden vector / wire 契约测试、UI 冒烟脚本 |

## 技术栈

| 层 | 选型 |
| --- | --- |
| UI | **`gpui-kit =0.10.0`**（门面：`gpui-pre 0.3.12` + `gpui-base` + `gpui-component`），平台入口 `gpui::platform::application()`；精确 pin，升级必须整套对齐（见 ADR 0004） |
| 语言 / 构建 | Rust edition 2021，Cargo workspace（13 个 crate） |
| 存储 | SQLite 单库 WAL（`rusqlite` bundled）+ Loro 分区 CRDT；本地明文，Markdown 单向导出 |
| 检索 | Tantivy 0.27 + cang-jie `ForSearch`（索引可丢弃、可从 SQL 重建） |
| Markdown | `pulldown-cmark` 语义扫描、syntect 只读语法高亮、merman Mermaid 渲染 |
| 画布 | vendor 的 `gpui-excalidraw 0.1.9`（MIT，见 [`crates/excalidraw/README.md`](crates/excalidraw/README.md)） |
| 同步 / 加密 | `sync-proto` CBOR wire 契约、HPKE / Ed25519 / Argon2id / XChaCha20-Poly1305、rustls + axum relay；见 [`crates/relay/README.md`](crates/relay/README.md) |

## 快速开始

### 前置条件

- **Windows 桌面**（首要平台；macOS / Linux 仅在架构上预留，不为未落地平台提前抽象）。
- Rust stable（edition 2021）、MSVC 构建工具、可用的 C 编译器（`libsqlite3-sys` 用 `bundled` 编译 `sqlite3.c`）。
- 首次构建会编译整棵 GPUI 依赖树，耗时较长；`target/` 体积很大（仓库 `.gitignore` 已排除）。

> `Cargo.toml` 里对 `libsqlite3-sys` / `stacker` / `zstd-sys` / `ring` 关掉了 dev 调试信息，并为 dev 设了 `debug = "line-tables-only"`：这是本机 `cl.exe` 的 D8050 问题与测试二进制体积的实测规避，**不要随手删**；原因写在 `Cargo.toml` 注释里。

### 构建与运行

```bash
# 默认构建（default = ["sync-ui"]，启动进入 AppRoot 同步向导）
cargo build -p menghunote-app
cargo run   -p menghunote-app

# 不带同步 UI 的编辑器（旧 dev vault 入口，适合编辑器/IME 冒烟）
cargo build -p menghunote-app --no-default-features

# dev 专用同步能力（localhost relay / V2 管理实验 UI；正式构建不得启用）
cargo build -p menghunote-app --features dev-sync
```

### 验证（提交前三绿）

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```

> **已知偏差**：`cargo clippy --all-targets -- -D warnings` 目前因 `crates/app/src/dev_sync/identity.rs:47` 的 `unused import: File` 未通过；这是 2026-11-22 历史整理前既有的问题，与文档改动无关，详见 [`docs/notes/history-rewrite-20261122.md`](docs/notes/history-rewrite-20261122.md)。

UI 改动另需按 [`scripts/README.md`](scripts/README.md) 跑 `ui_smoke.ps1` 做 Windows 真机验证——单元测试证明不了光标形状、候选窗位置、选区像素。

## workspace 结构

| crate | 职责 |
| --- | --- |
| `crates/app` | 应用壳、侧栏、设置、同步 UI、主题、插件、headless 测试 |
| `crates/editor` | 编辑器内核：语义扫描、投影映射、光标/选区、块级布局 |
| `crates/markdown` / `crates/text` | Markdown 语义层 / 文本坐标与范围（四坐标唯一实现） |
| `crates/storage` | SQLite 真源、投影表、CRDT 会话、只读快照与同步桥 |
| `crates/search` | Tantivy 索引管理器（代目录发布、启动对账、增量事件） |
| `crates/sync-proto` | 冻结的 CBOR wire / golden vector / 控制链契约 |
| `crates/sync` | 同步引擎、密钥与设备身份、加密中继客户端 |
| `crates/relay` | 自建密文中继（生产二进制 + admin CLI） |
| `crates/dev-relay` | 仅测试用的进程内 relay harness（`publish = false`） |
| `crates/i18n` / `crates/plugin-api` | 纯基础设施：本地化 catalog / 插件 manifest 契约 |
| `crates/excalidraw` | vendor 的 Excalidraw 视图（MIT） |

依赖方向见 [ADR 0003](docs/adr/0003-module-boundaries.md)：`app → editor/storage/text`，反向不允许。

## 文档导航

| 位置 | 内容 |
| --- | --- |
| [`AGENTS.md`](AGENTS.md) | **行为契约**：项目定位、模型/委派路由、依赖基线、当前状态、协作与提交纪律。优先级最高 |
| [`docs/adr/`](docs/adr/) | 26 篇架构决策记录（「为什么这样做」），不作为操作手册 |
| [`docs/notes/`](docs/notes/) | 阶段性研发记录、功能说明、历史整理账本 |
| [`docs/guides/`](docs/guides/README.md) | 面向开发者/部署者的可操作指南（构建、部署、配置） |
| [`docs/reviews/`](docs/reviews/) | 同步/加密 v1 的安全与架构评审留档 |
| [`docs/outdated/`](docs/outdated/README.md) | **已作废 / 被取代**文档的归档，仅用于追溯，不作当前状态依据 |
| [`scripts/README.md`](scripts/README.md) | 开发期自动化脚本（UI 冒烟等） |

2026-11-22 文档整理时，Phase 1A/1B 期间生成的一次性评审稿与代码快照（约 8.1 MB）已删除，可从 `backup/dev-20261122` 找回。

## 分支与历史

- **`main`** 是开发主线，2026-11-22 从零重建为 **30 组主题语义提交**（首个提交日期 2026-11-22，独立 root），最终代码 tree 与整理前 `dev` tip 逐字节一致。
- 整理前 303 个提交完整保留在 **`backup/dev-20261122`**；6 个 annotated tag（`v0.1-slice6` … `v0.5-sync-web`）均未移动。
- 逐组映射、分支/tag 状态与验证结果见 [`docs/notes/history-rewrite-20261122.md`](docs/notes/history-rewrite-20261122.md)。

## 开发约定（摘要）

- 提交前 `cargo fmt` + `clippy --all-targets -- -D warnings` + `test` 三绿；Lead 亲自跑构建与测试，review 一律看 `git diff`。
- 一次提交只做一件事；提交信息用中文讲清「为什么」。禁止 `git commit --amend` 或改写他人历史。
- **UI / 交互类改动必须先 Windows 真机验证再提交**；未验证的改动用 `wip:` 前缀，验证通过后再补正式提交。
- 发布点用 annotated tag 标记，只打「已被真人验过」的状态。
- 平台相关代码收敛在 `#[cfg(target_os = "...")]` 或 `platform/` 模块，业务层不得出现平台分支。
- `target/`（体积很大）与本地 vault 不入库。

完整规则见 [`AGENTS.md` §3 协作纪律](AGENTS.md)。
