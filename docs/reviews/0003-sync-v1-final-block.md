# 评审 0003 — 同步 v1 最终 BLOCK 与 M0 最小阻断清单（gpt-6.1-sol，2026-11-12）

> 历史文档：以下 commit 哈希属于 2026-11-16 整理前历史；映射与 backup 分支见 [history-rewrite-20261116](../notes/history-rewrite-20261116.md)。


> **历史留档**：本文写于 2026-11-13 历史整理前；文中哈希只在 `backup/dev-20261113` 可达，
> 见 [`docs/notes/history-rewrite-20261113.md`](../notes/history-rewrite-20261113.md)。

> 对象：committed HEAD `60af841`；证据边界：只读静态审计，未改文件/未跑 cargo。
> 结论：**BLOCK，不批准 `v0.4-sync`（含 localhost/非生产）**。不是可销毁实验版；用户已明确走完整 M0。
> 本文件是最终报告的仓库留档；修复后须对**新的已提交 HEAD** 重新复审。

## 一、P0/P1 闭环裁决

| 项 | 裁决 | 关键证据 |
| --- | --- | --- |
| **P0-1 ASSET nonce/AAD** | 核心实际路径已修 | 完整明文头派生 object_id；MIME/长度校验；producer 规范化为 0（注意 AAD 仍编码完整 header，不是"物理删除"）；MIME/双设备/重传测试有效。generic upload 仍宜强制 canonical message_id。 |
| **P0-2 控制链/证书认证** | **未闭环** | `http_relay.rs:1671-1744` active 资格仍由 relay 未签名的 `member.status` 决定；历史已撤销但证书签名有效的设备被改 active 后，可经 `active_member_hpke_keys`→`plan_grants` 获得候选根；随后拒绝 activate 不能撤回已泄露材料。旧公开 `rotation.rs:74-129` A verifier 仍覆盖裸 rev1 anchor、强制 `to=from+1`、pin 复制 journal hash；`ControlPin` 默认 None；`abort` 不验签；genesis raw 例外。 |
| **P0-3 基线覆盖 S** | **未闭环** | prepare 固定 S 后立即导出；validate 只把 snapshot actual VV 与其自报 manifest VV 比，再检查"当前本地" integrated，未绑定导出快照与 `≤S` 更新。prepare 落后、之后 import 远程 op，再 validate 旧 prepared 仍可 publish。`VerifiedBaseline` 安全字段 pub 可改；promote 只看私有 bool。现有测试只 `set_meta integrated=1` 就要求通过，反而固化假覆盖。 |
| **P1-1 wire/完整授权** | **未闭环** | relay 完整 T/双签有效；但 public `apply_epoch_change_with_client` 不消费 `verified_activation_t`，只看 kind、重算 relay 自报 hash、验 grant header、接受 caller 选 cid；缺 exact pair/signed commit/完整 inclusion/S/cursor/new manager。旧 `collect_baseline_payload` 仍把 SnapshotV1 CBOR 当 Loro。 |
| **P1-2 restart** | **未闭环** | 真实 POST/assigned epoch/fresh 材料有；但 `rotation_restart.rs:1094-1170` POST 早于 durable intent；primary/fallback 完成后才一次保存 journal；primary 成功后故障重跑会撞 role 唯一 CAS；恢复用 relay 摘要替代本地 receipt。 |
| **P1-3 promotion/repack** | 部分闭环 | manager seed/旧根/全状态/多 epoch 根预检/late ACK 已改善；`durable_rotation_barrier` 未覆盖 B 首次 I 的 apply staging；promotion 前失败重启可丢 RAM freeze；身份文件无掉电 durability。 |
| **P1-4 水位单调** | 只修 normal bootstrap | bootstrap finalize max/CAS 正确；但共享 `storage/baseline_apply.rs:606-695 finish` 仍把已有 received/integrated/highwater/cursor 写成 selected S，实际 I 可回退。 |
| **P1-5 GC/预算** | 部分闭环 | 两健康根前置、旧 epoch/candidate/ASSET 真回收有效；candidate `promote_named_candidates` 绕过 future-live gate；`future_live_bytes` 漏 MANIFEST/orphan ASSET；活对象 maintenance→normal 归类需闭环。 |

## 二、T01–T22 矩阵裁决（摘要）

- T01–T03 部分：缺首次 restart POST 前后 durable intent、B 首次 I 重启 fencing。
- T04–T05 部分：snapshot 自身覆盖 S、不可变验证凭据未覆盖。
- T06 部分：缺 primary 半程/fallback 故障、POST-before-journal、power-loss durability。
- T07–T08 relay 端较充分，消费者仍接受 None-T。
- T09–T10 部分：缺 revoked→active 状态伪造、完整 grant inclusion 到 public I。
- T11 relay 永久事实基本覆盖；客户端 abort 认证例外未关闭。
- T12 核心保守 all-repack 覆盖有效；不能替代 B fencing/水位修复。
- T13 分段不足：90d 测试仅 1→2→3、无 restart 跳 epoch、grant 用 fixture 假密文；双设备 e2e 一次激活且同进程知道新根，不能证明同 durable 旧身份链跨多轮+GC 后恢复。
- T14–T15 基础有效；P0-2 使"撤销 HPKE 得不到候选根"不成立。
- T16–T17 normal bootstrap 良好；I 的 exact 授权/水位/GC 前提不足。
- T18–T19 多数覆盖；同 epoch orphan GC 可延期但保留字节不能漏计。
- T20 部分：缺 candidate activate、manifest/orphan-only over-Q、近 Q 多轮预算恢复。
- T21–T22 HTTP 新 verifier/TLS 很强；旧 A、None pin、unsigned status、raw/abort 仍绕过。

## 三、已知限制裁决

1. candidate activate 不做 future-live：**M0/tag 阻断**。
2. MANIFEST/orphan ASSET 漏计：**阻断**（orphan 物理 GC 可延期，但字节必须保守计账）。
3. ControlPin opt-in、真实构造 None：**阻断**，是已验协议调用链，不是 UI polish。
4. T13 分段：架构可接受，不要求制造 dev-dep 环；但当前证据不能标完成。
5. 完整 UI/MCP/生产运维可后置；恢复材料备份确认、不可逆吊销确认、冻结/故障编辑保留、手工导出最低交互不可后置。
6. ring 单包 `debug=false`：非阻断，AGENTS §6 既有 D8050 workaround，不是关闭 TLS 校验。

## 四、最小可执行阻断清单（修复后对新 HEAD 复审）

1. **P0-A 统一控制/成员认证**：active/revoked 只由已验签 canonical 动作推导，禁止 unsigned status 决定 grant recipient；统一旧 A verifier、强制持久 pins、关闭 raw/abort 例外；负例"保留全部真实签名，仅 revoked→active 篡改"必须在任何 HPKE seal/upload 前失败。
2. **P0-B 不可变 S 证明**：VerifiedBaseline 私有只读；promote 只消费 exact prepared + 独立验证 receipt；snapshot actual VV 必须覆盖 freeze cut 的真实 op；日志空页/不前进/未到 S 失败；"prepare 后才 import 的 remote op 使旧 prepared 失败、重新 prepare 才成功"。
3. **P1-C I/H 消费完整授权 receipt**：T/双签/exact cp/signed commit/完整 inclusion/S/cursor/new manager；共享 finish 全部 max+CAS；移除 SnapshotV1-CDE-as-Loro。
4. **P1-D 最窄 crash 窗口**：POST 前 durable RestartIntent；primary/fallback 分别保存 same-cid/固定材料/验证 receipt；B 首次 I 写前 durable phase；身份 flush/replace durability。
5. **P1-E 同事务完整 Q/R**：candidate activate 与 publish 共用计量；含全部实际保留对象、去重、正确归类；candidate/manifest-only/orphan-only over-Q 零效应拒绝；不抢删安全根。

**满足上述修复及指定负例后，才能讨论 APPROVE `v0.4-sync` 的非生产范围；即使批准也不代表生产就绪（M1/M2/M3 另行门禁）。**
