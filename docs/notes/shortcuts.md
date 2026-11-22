# 快捷键：设置、冲突提醒与持久化

> 2026-11-20。设置 →「快捷键」分类；本机偏好，不进 vault、不参与同步。

## 命令与默认绑定

| 命令 | 默认绑定 |
| --- | --- |
| 搜索 | `Ctrl+K` / `Ctrl+F` |
| 重命名笔记 | `F2` |
| 加粗 | `Ctrl+B` |
| 斜体 | `Ctrl+I` |
| 新建笔记 | `Ctrl+N` |
| 打开图谱 | `Ctrl+G` |
| 打开设置 | `Ctrl+,` |

默认值沿用既有编辑器行为；`Ctrl+F` 作为搜索的第二个默认别名保留。

## 行为

- 设置页按命令逐行显示当前绑定；「修改」进入录制态，下一次按键成为新绑定，`Esc`
  取消。录制期间按键被根视图 capture 相吞掉，不会同时触发原命令或写进正文。
- 录制只接受带 `Ctrl/Cmd` 或 `Alt` 的组合，或 `F1–F24`；裸字母/数字/Enter/Tab/Backspace
  一律拒绝，避免把正常输入改成快捷键。
- **冲突只提醒、不禁止保存**：同一个按键被多个命令使用时，行内显示红色冲突标记，
  顶部汇总显示「冲突 N 组」。运行时按固定优先级匹配；冲突配置仍写入文件，方便用户
  自己决定保留哪一个。
- 「恢复默认」只恢复该行；保存后立即生效，不需要重启。

## 持久化

```json
{
  "version": 1,
  "bindings": [
    {"command": "search", "keys": ["ctrl-k", "ctrl-f"]}
  ]
}
```

- 路径：`<数据根>/shortcuts.json`；`MENGHUNOTE_SHORTCUTS_PATH` 可覆盖。
- Windows：`%LOCALAPPDATA%\menghunote\shortcuts.json`；Linux：
  `$XDG_DATA_HOME/menghunote/shortcuts.json`。
- 缺失/损坏/版本不支持 → 全部默认；未知命令忽略；已知命令非法/空组合 → 该命令默认。
- 原子写；文件不在 vault、不参与同步、不写正文。

## 接线

- 编辑器：搜索 / `F2` / 粗体 / 斜体从中心绑定读取；既有链接确认、右键菜单、
  表格 cell 编辑、IME、持闸守卫保持不变。
- 宿主：`Ctrl+N` 新建笔记、`Ctrl+G` 打开图谱、`Ctrl+,` 打开设置；AppRoot Main
  统一拦截，设置/管理仓库/切换菜单/标题编辑/搜索/图谱/表格编辑/IME 组合时不触发。
- AppShell 独立 dev-sync 设置页没有快捷键 UI；绑定仍从同一份 `shortcuts.json` 生效。

## 测试

- `crates/app/tests/headless_shortcuts.rs`：导航、7 行默认值、录制并落盘重载、
  冲突红标 + 汇总、`Esc` 取消、录制吞键、`Ctrl+,`、宿主路由。
- `crates/app/tests/headless_shortcut_keys.rs`：`Ctrl+N` / `Ctrl+G` / 改绑后立即生效。
- `shortcuts` 纯函数单测：默认、录制规则、解析/展示、保存重载、损坏回退、冲突。

## 已知限制

- 没有「全部恢复默认」按钮（单行恢复已实现）。
- Windows/macOS 真实键盘布局（尤其 `Ctrl+,` 的 `,`/`comma` 映射、Cmd）仍需真机确认。
- 冲突时运行时按固定优先级，不弹二次确认。
