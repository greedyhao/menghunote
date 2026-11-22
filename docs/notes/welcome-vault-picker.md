# Welcome 仓库选择页

> 2026-11-20。返回欢迎页后的产品入口；参考 Obsidian 仓库选择器。

## 布局

- **左栏** `welcome-vault-list`（280px，独立滚动）：
  - 已保存同步实例 `welcome-vault-<i>`：目录短名 + 路径 +「上次使用」；点击直接打开，不经过管理页。
  - 本地仓库 `welcome-vault-local`：`startup.json` 最近本地路径（或默认用户文档路径，仅文件存在时出现）；点击打开。
- **右栏**：标题 + 三张操作卡（点击展开）：
  - `welcome-new-local`：选目录 → 新建 `vault.sqlite3` 空库（不播种 demo）；目录已有库时提示改用「打开」。
  - `welcome-open-local`：选已有 `vault.sqlite3` 打开；不存在只报错，不新建。
  - `welcome-sync-remote`：**只粘贴一条链接**，自动识别 `mn-pair://create` / `mn-pair://join`；不再显示 relay / vault_id / 恢复码 / 邀请多字段。
- dev-sync 构建的统一同步表单保留「同意开发同步」；产品构建不显示、不要求。
- 管理仓库和 Welcome 共用同一个同步远程表单；管理仓库只保留一个 `vault-manager-sync-remote` 入口。

## 仓库自定义名称

- 同步实例可设置 `display_name`（可选，1–64 字符；空白 = 无自定义名）；显示层优先用它，
  回退库目录名、再回退短实例 id。
- 创建/加入表单有「仓库名称」`welcome-sync-name`；管理仓库可 `vault-manager-rename-<i>` 行内改名。
- 只改公开配置 `sync.json`，不移动目录、不改 `vault_path`/`vault_id`/`device_id`。

## selector

`welcome-vault-list` / `welcome-vault-<i>` / `welcome-vault-local`；
`welcome-new-local`（+ `-dir` / `-browse` / `-confirm`）；
`welcome-open-local`（+ `-path` / `-browse` / `-confirm`）；
`welcome-sync-remote`（+ `-link` / `-dir` / `-dir-browse` / `-confirm` / `-consent`）；
`welcome-error`。

## 行为

- 创建链接走既有一次性票据流程 → 备份页；join 链接走邀请兑换 → PENDING（已有 5s 自动检查批准）。
- 链接解析失败：内联 `welcome-error`，不联网、不建文件。
- 打开/新建本地库成功后写 `startup.json` 的 `mode=local` + 实际路径。
- 旧 `app-root-create` / `app-root-join` / `app-root-local` 欢迎页入口已移除；内部 CreateForm/JoinForm 状态保留供测试/兼容。

## 测试

`headless_vault_switch`：左栏列出并直接打开、新建本地库、打开已有/缺失；`onboarding_ui`：未知链拒绝；`onboarding_invite`：join 链接进 PENDING。

## 已知限制

- 左栏只列同步实例 + 一个本地库；没有本地库历史列表。
- 真机 DPI/短窗口滚动/深色观感未验。
