# 评审 0002 — 同步/加密 v1 安全架构评审（最终，gpt-6.1-sol，2026-11-12）

> 对象：`docs/briefs/0005-sync-status-and-plan.md`；取样 HEAD `d875ec9`（简报以 `0b30b2e` 为基线）。
> 评审期间 `5937386`（槽 AAD/grant_id）及 ADR §31–§33 已落地；**新增 ADR 是已决方向，不等于实现已验收**；最终发布须再审最终 diff。
> 本评审只读静态审计，未改文件、未运行 cargo，不把测试数量或 teammate 三绿当安全证明。

## 结论

**不批准按简报当前 M0 清单打 `v0.4-sync` 安全验收 tag；不批准生产部署。**
设计主干（relay-only、独立管理权、epoch 正交、HPKE、永久 revocation、独立 pair、正确 CAS 方向）可保留；
但当前不是"只差两个在途 bug/一条 e2e"的状态。

## 一、对简报 §2/§3/§5/§7 的判断

- **§2 建议改**：信任边界原则可接受，但需明确本地 SQLite/WAL/索引/备份明文不提供静态加密；DPAPI CurrentUser 不抵御同用户进程；
  摘要/自洽 hash 不是身份认证；"只信已签控制链"目前代码做不到；首次恢复回滚与已有设备丢弃 pin/highwater 必须分开。
- **§3 必须修**：密码/R/HKDF/独立管理权/epoch≠generation 选型可接受，但图示把 `K_slot_mne` 挂在 epoch root 下，与 §3.2/ADR §20 矛盾；
  `K_slot_mne` 应由 password KDF 或 R 派生。不能笼统长期 DPAPI 存 R；可在 pending journal 暂存 R′，完成后清除，长期只保存新 manager_seed。
  R 丢失但 DPAPI 管理私钥尚在 ≠ 失去管理权；所有管理私钥副本和 R 均丢才是。
- **§5 必须修**：单 writer/BEGIN IMMEDIATE/密文 relay/KEY_SLOT-only 管理读取可接受，但旧密文 7d 真清理/配额收口/客户端闭环不可验收；
  简报 R=Q 与 ADR §7 默认 R=2Q 及 quota 注释口径不同，需统一 R 与 Q+R 定义与发布后预算归类。
- **§7 必须修**：可以说"ADR 约束已冻结"，不能说"实现安全约束已收敛"；§6.3/§6.5 的 prepare→validate→promote/完整 A–J 声明超出实现证据。

## 二、严重性排序与必须修项

### P0-1 ASSET 同 key+nonce 加密不同 plaintext/AAD
`upload.rs:239–275` nonce 由 asset object/part 确定，AAD 却含 `producer_device_id`；`checkpoint.rs:539–556` 对同 raw hash 可按不同 MIME 重加密，
而 `checkpoint.rs:993–999` plaintext 包含 MIME；`checkpoint.rs:1027–1035` asset_id 仅提交 raw SHA。同文件不同 MIME 或设备即复用 key+nonce。
relay `ID_CONFLICT` 发生在密文已发出后，不能补救。必须使同 key+nonce 对应的完整 plaintext 与 AAD 逐字节唯一；
可选持久化随机密文+验证赢家对象后复用，或新版本完整内容身份+规范 AAD。若采用 §31 的 producer=0/移除，须同步改 relay ownership/签名语义。
任何 ID/HKDF/AAD 变化不能静默改已落盘 v1，必须声明版本/迁移或明确仅销毁实验库。补跨真实 producer、跨 MIME、断电重传回归。

### P0-2 控制链/设备证书未认证，能把候选根封给攻击者 HPKE key
`http_relay.rs:1410–1529` 只验自洽 hash，外层 kind 未签名绑定；证书外签在 1492–1512 可缺省，还按静态 manager pub 验，
而不是 issued_manager_rev 对应的已验证历史 key。恶意 relay 能插入无外签 attacker cert/HPKE、标 active，`plan_grants` 会向其封候选 root——
不能等 activate 被诚实 relay 拒绝才算安全。`rotation.rs:87–99` 还会让裸 genesis entry 覆盖调用者可信 `first_old_public`，并硬要求 `to=from+1`，与 restart 跳 epoch 冲突。
必须先从本地 pin 逐级验管理转换，强制验每个 cert 和签名管理动作/typed event 所有消费字段，再导出成员集合/HPKE key；
历史被吊销 cert 仍保留以验 ≤S 合法历史，不可只缓存 status=active。`/control` 应分页到已认证 head，保存 rev+hash 单调 pin，不能只做 SHA 一致性。

### P0-3 checkpoint 先发布、后弱 validate，能假覆盖 S 并跳日志/触发 GC 丢操作
`checkpoint.rs:469–475` 取得 relay S 就导出当前 local doc，没有拉齐 integrated≤S；`checkpoint.rs:660–667` 已签 `validation_version=1` 并 publish；
`checkpoint.rs:686–752` validate 只读 manifest、refs/self-signature 和 `head>=S`，没有两空库真实导入全部 snapshot/asset、没有实际 VV/codec/frontier/覆盖 S 证明。
`head>=S` 不等于本地覆盖 S；promote 只是本地句柄返回。active epoch publish 会立刻 published/建 GC 计划（`relay/checkpoints.rs:721–750`）。
必须 prepare 仅暂存 → 真实 ≤S 整合/固定导出 → 完整读回两个独立上下文 → 生成不可变 `VerifiedBaseline` → 签名发布/activate。
公开可变字段+私有 validated bool 不能防句柄绑定被改。`rotation_stage::verify_snapshot_integrity:218–238` 值得保留，但"实际 VV 等于声明 VV"仍不是"已消费 relay ≤S 全部事件"的证据，且普通 writer 未调用它。

### P1-1 真实 HTTP I/J 不兼容，并缺完整 activation 授权/包含证明
1. `rotation_apply.rs:448–468` 要 `epoch_activated`，relay/proto 是 `epoch.activated`；又把 relay 的 32B activation digest 当 `ActivationCommitV1` CBOR decode；
   relay 存的是 `rotations.rs:968–992` 的 digest，不是完整 commit。
2. `http_relay.rs:1052–1056` 把 checkpoint detail 键 4 的 `manifest_oid` 当 `manifest_bytes`；`relay/api.rs:1859–1860` 该字段是 OID，
   需用 ref 元数据下载/验 digest/解密，不能直接 `ManifestV1::decode(OID)`（不是 Vec::new）。
3. `rotation_apply.rs:270–275` 把整块 `SnapshotV1` CBOR 存到 `BaselinePayload.docs`，`storage/baseline_apply.rs:255` 要 raw Loro bytes。
4. `rotation_apply.rs:332–381` 信 kind 列表/decoded activation 字段，没有完整双签 T、checkpoint commit、latest grant 全包 digest/inclusion、exact baseline S/cursor 授权链。
   九字段 `ManagerTransition` 双签只保护转换，不替代评审 0001 要求的完整 activation T 双签：T 必须绑定 pins/from/to/S、两基线 commit、slots、成员证书、
   完整 grants、old+new assets。否则旧管理私钥退役后仍有重用已有 new-key 转换签名、拼接不同材料的风险。
   必须统一 shared typed wire、强制全授权绑定和新管理 key 专用 NUL 域签 T；不要先"接线跑通"却保留未认证输入。

### P1-2 restart 不是补一条 POST 即可，epoch 和 durability 契约错位
`rotation_restart.rs:341–365` 用同一个 `current_epoch` 作 from 和 to-1；relay `rotations.rs:1173–1183` 始终保留原 active from，并分配更高 candidate。
active1→candidate2→restart3 时，传 1 会生成废弃 2；传 2 虽生成 3 但 from=2 错，真实 from 仍 1。manager_rev 仅实际 activate 才推进，不随候选推进。
`execute_restart` 不发 restart、不 durable 保存 fresh R′/新 manager private/固定密文/进度，内部 activate 前没有 R′ 备份确认；失败重调会换 root，已 staged grant 不可覆写；
activate 成功后掉返回值可能永久失去新管理权。Fake 测试手动 `relay.epoch=EPOCH+1`，断言的恰是错误 +1 假设（`tests/rotation_restart.rs:294–381`）。

### P1-3 journal、冻结恢复、身份 promotion 和 outbox 不是闭环
`rotation_journal.rs:286–320` 把任何本地 pending/encrypted outbox 都当 ≤S 未整合，混淆 local seq 与 relay seq，持续编辑会永远卡 C；
应只卡 ≤S inbox 缺口/quarantine/unsupported，允许本地离线意图。`engine.rs:205–214` `rotation_frozen` 启动重置且 thaw 是 RAM 操作；
durable `repack_required` 存在但不能代替从 journal/relay 事实恢复全阶段 barrier。
promotion（`rotation_activate.rs:273–291`）新身份保留旧 manager_seed，`control_rev` 却写 `new_manager_rev`（两个 revision 域混了）；
没有在 rename 之后 durable 保留重包所需 old root/或全部原始明文意图，`fs::write+rename` 与 SQLite 不是一个跨介质原子提交。
`finish_promotion:300–304` 从 Activated 直接 `advance(Completed)`，状态机要求经过 Promoted，helper 必失败。
journal 必须保护 old root/原始意图、new root/R′/new manager seed、backup 确认、精确 request/rid/assigned epochs、固定所有 ciphertext、receipts 和 phase；
成功 G ≠ 可解冻/可清 journal。
`rotation_repack.rs:118–193` 实际重包所有旧 epoch，不做声称的 baseline VV 覆盖选择；NULL digest 的 pending_encryption 被当失败，且只有一个 old_root，
部分迁移/多 epoch 旧 row 无法全部处理。可简化为"所有原始意图保守重包"，但必须支持 pending/encrypted/sent、跨 doc batch 和 assets，不得丢原 update_id/Loro bytes/origin。
正面：原子 row+parts repack CAS（`sync_bridge.rs:609–718`）和晚 ACK triple CAS（`423–455`）可接受。

### P1-4 bootstrap 和水位/身份 pin 可回退，整库导入非原子
`bootstrap.rs:115–152` body/structure/assets 分批事务，cursor/log/highwater 又单独写；崩溃可半库或错 cursor，结构真实 import 失败可能发生在 body 已写之后；
现有验证比较声明 snapshot VV/manifest VV，不充分检查实际 VV/full-history/结构语义。对已有库没有先校验 trusted pin/highwater，却覆盖 highwater；
`baseline_apply.rs:647–660` 也把 highwater 直接写 baseline S 而非保留 max。fallback 整体预验证和 merge 本地编辑已有改善，可保留，
但须统一可恢复 import journal/事务 CAS，所有实际导入和 assets 完成后原子推进 exact fallback 自己的 S/cursor，instance/log/control/epoch/highwater 保持单调。
初次恢复边界不能豁免已有设备的回退缺陷。

### P1-5 旧 epoch/candidate 数据不回收，可连续轮换耗尽预算后永久冻住
`checkpoints.rs:993–1061` 只枚举 published、每个 epoch 无条件 skip(2)，每个 retired epoch 永留两套完整 snapshot/manifest；candidate/aborted 清理未收口；
明确不处理 Asset。GC 只清旧 updates，旧 epoch ASSET 永久保留。不能把"跨 epoch 复制、废弃 candidate 垃圾"归入可延期的"同 epoch 资产孤儿 GC"。
`slots.rs:397–431` 14d 实际删 slot/body 和直读防线是正面改善，但 staging/旧迁移槽应有 actor/current-epoch/lifecycle 绑定，不能让新 enroll 误选旧/staging recovery 槽。
发布/activate 未来 live≤Q、对象 budget 退/升类、磁盘门禁本次静态未找到完整证据，应通过 T20 及代码收口。槽 TTL 是独立安全策略，不要靠延长掩盖基线不健康。

### 生产阻断：客户端只有 HTTP
`http_relay.rs:22–23` 说明 `StdTcpTransport` 无 HTTPS 且接受任意 HTTP host；必须真实 TLS client、hostname/cert 验证、拒绝降级/跨 origin 重定向。
HTTP 必须强制显式 localhost 开发。E2EE 不能保护 bearer token、控制平面/设备元数据和可用性。

### 测试证据界限
已有 SQL/CAS/字段测试有价值，但不能替代全栈认证和 import。`relay/tests/support/mod.rs:143–161` 对多数 `Expectation` 直接 Ok，不是密码学证明；
`rotation_apply` 测试硬编码下划线 kind/完整 CBOR，与真实 relay 不符；baseline 测试直接喂 raw Loro，掩盖 collector CBOR 误用。
应以真实 HTTP + Ed25519 hook + 真实 SQLite/Loro 的双设备闭环做门禁，不能只把 FakeRelay 绿算作 v1 验收。

## 三、A–J 与 restart 推荐顺序

- **A**：durable 保存本地意图、old root 或可重建原始 updates、fresh root 原始材料、password/R′/new manager private、`request_id`，完成新恢复材料备份确认
  （不得仅返回值里才给 R′）。未知 `to_epoch` 时不猜 +1，材料等 B 正式分配后绑定。
- **B**：签名 begin + CAS 在 relay 短事务固定 S、永久 revocation 事实、`auth_rev`、candidate 分配；丢响应按 exact `request_id`/rid 查询结果，timeout=未知，不 fresh begin/不 thaw。
- **C**：从已验历史控制/证书验收并真正 integrate 全部 relay ≤S；保持本地编辑/L/outbox，不拿 merged local VV 当远端覆盖证据。
- **D**：固定原始快照/所有 opaque frozen 资产映射/全部待上传密文字节，生成两套独立 snapshot+manifest。
- **E**：两个互不共享空上下文分别下载、digest/AEAD/codec/full-history/实际 VV/frontier/refs/assets 验证，且与 C 的覆盖凭据绑定；只有 E 成功才可签 `validation_version=1`。
- **F**：从已认证当前成员 cert 集合为全部未吊销（含 90d 离线）设备封 HPKE grants，固定/持久 leaf manifest，签名并 stage，自己 readback 验证；
  已为某设备封包后追加吊销必须废弃 candidate root。
- **G**：完整 T 旧/新 manager 专用域双签，exact 材料 digest+CAS，relay 一事务发布 slots/pair/grants/manager/control/`auth_rev`/`active_epoch`；
  丢 reply 查 exact request，不用 current 的任意 activated view 作成功；旧 manager token 成功后失效，查询要可用新 manager/未吊销 device。
- **H**：发起者验完整已提交授权、自己的包与 candidate root 相等，durable promotion 新身份+新 manager 私钥+可信 pins，保留 J 所需旧材料及 barrier。
- **I**：普通/离线设备先重新鉴权，分页验历史 manager/control 链和最新 grant inclusion，再完整验证选中 baseline 并 merge 本地状态；不能先改身份/水位后验。
- **J**：仅用 exact 已授权 durable baseline 的实际 per-doc generation VV 剪覆盖 row，或更简单全部原意图重包；原 op/update_id 不改，新的 message/object/epoch 和 row+parts CAS；
  完整完成后才清 barrier/old keys/journal，旧 ACK 不得清新 row。
- **restart**：durable `RestartIntent`（old rid/request/CAS/from active epoch/abandoned candidate）→ 签名 `POST restart` → 恢复该请求分配的新 rid/new_to_epoch
  （strictly greater，旧 from/S 不变，manager_rev 不随候选推进）→ fresh 材料绑定 assigned epoch 并 durable 保存/确认 R′ 备份 → D/E/F/G/H/J。
  网络错误不判断"没执行"；所有恢复按请求回执/已认证状态，失败不回旧写，永不复用被撤销候选根。

## 四、简报 §10 八问答复

1. **KEY_SLOT-only 管理读取**：可接受。R 已同时具数据+管理恢复权，加每对象一次性 challenge 不解决 R 泄露，只增复杂度。
   必须限制当前 vault/instance、`auth_rev`/expiry、可恢复的 current/authorized migration lifecycle，仍不放宽数据读/写；审计、速率限制、秘密不进日志。
   新 device 不能任意读 staged 或旧迁移 slot。
2. **restart 怎么接**：如上 durable intent → 真实 relay 分配 → exact 恢复 → fresh/备份 → restage/revalidate → 完整 activate；不是一条 POST 加本地 +1。
   epoch 允许跳，manager_rev 仅 activate 推进。
3. **全部设备在线能否简化**：不能替代 crash/revoke race/offline correctness，也与 v1 多设备离线目标冲突。
   可后置自动接管/自动重试/UI，可要求"一个管理发起者在线"，不可把 90d 离线正确性、candidate 泄露后换根、immutable grants/fail-stop 删掉。
   若降级为无 rotation 实验版，要明确产品能力降低。
4. **真实 VV 证据要上传 relay 吗**：本地两空库真实 import 是必须，relay 不会因收到报告就能证明覆盖 S。
   可上传不透明加密/签名证据 digest 供审计，但不上传明文 VV/doc id/真实 asset hash；签名声明不是因果证明，不能让 relay 运行 Loro。
   建议先不增加上传证据 API，收口不可变本地 receipt 与 signed cp/activate 绑定。
5. **首次恢复回滚边界是否接受**：v1 可明确接受"无旧可信水位无法辨别有效签名历史前缀"。要更强新鲜性需独立诚实见证/外部锚；relay 自己签或自洽 Merkle consistency 不创造新鲜性。
   已有 client 必须保留 instance/log/epoch/control rev+hash/highwater 单调，不准 bootstrap 降低。
6. **baseline VV 支配 outbox endVV 是否足够**：对同 doc+generation 的原 Loro update，来自独立真实 import、exact activated durable baseline 的 actual VV domination 可证明包含；
   不能用声明 VV/merged local VV。multi-doc 逐个检查且 batch 不能假装跨 doc 原子；assets 另需完整认证/持久依赖及新 epoch 映射。
   当前代码没做宣称的 VV 选择，pending NULL rows 还失败；建议 v1 先全部原始意图保守重包，延后优化。
7. **UI 还是 TLS**：M0 安全契约先，真实 TLS/硬化在任何非 localhost 上线前，优先于完整 UI。恢复材料备份确认、不可逆吊销、冻结/故障保留编辑/导出等最小 CLI/UI 安全交互不能后置。
   完整管理 UI 美化可延期。
8. **最小必测**：rotation+GC+assets 启用时 T01–T22 没有实质安全行可删，可合并 fixture/场景；T21/T22 可同一 >256 长期链场景覆盖。
   当前 M0 只选 10 行会漏最关键 SQL/客户端崩溃、假覆盖 S、后续吊销、旧 token、fallback/asset/quota/长控制链。另补 nonce 和真实 TLS 回归。

## 五、M0–M3 门禁与最小矩阵

### M0（修正确性/localhost 可验收候选）
以上 P0/P1 全收口、shared typed wire/golden、真实完整双签 T/包含绑定、durable restart/journal/promotion/fencing、真 baseline 验证/原子或可恢复导入、
all outbox states、cross-epoch/candidate GC。所有安全测试必须在 M0，而不是留 M3。Lead 审最终 diff 亲跑 fmt/clippy/test 与真实双设备；提交已知小 bug 和 e2e 不自动过门。

**最小合并矩阵**（不 sleep；FakeClock、SQL 注入、DurableAck/明确 phase；每组真实 HTTP+Ed25519 必要）：
- T01/T02/T03：old PUT/complete 与 freeze 交错；begin 每 SQL 点 kill/reopen/丢 reply；客户端 freeze 后各 phase kill。结果 S/revocation/`auth_rev`/control 要么全有要么全无，exact 请求恢复，old write 不因失败解冻，本地 L 保留。
- T04/T05：shared cid/manifest/snapshot、shallow 但 flag=true、假声明/实际 VV 或缺 doc、未 integrate ≤S；candidate 8d/abort/多候选。不得 publish/GC/bootstrap 候选，不假覆盖。
- T06：activate 各 SQL 点/丢 reply；identity 临时文件、rename、DB fence、导入、repack 每点 kill。只认 exact authorized activation；重启仍持正确秘密/barrier，无半库/丢管理权。
- T07/T08：完整旧/新签名、None/bad signature/每个 T 字段、outer vs signed body、同 request 同 body 与异 body、CAS 冲突。错误零效应，不让旧管理 key 拼接新材料。
- T09/T10：cert 无外签/伪 HPKE、recipient/vault/epoch/manager/grant_id/AAD/enc/ct/sig 交换、package digest/inclusion；grant 后吊销→restart 重新随机根和更高 assigned epoch。
- T11：revoke→abort→begin([])→abort；v4→v5 回填/重启永久事实，不旧写复活。
- T12：pending/encrypted/sent 各状态、multi-doc 一覆盖一未覆盖/assets 依赖、多 epoch/中断 J、延迟 old ACK、CAS 事务失败。原 op 字节/ID 保留、新 row/parts 不可被旧 ACK 清。
- T13：未吊销 B 离线 90d、跨多次含 restart 跳 epoch、GC 7d/slot 14d 后最新 grant 无 TTL 并 merge 未上传编辑；被吊销设备只能手工 export/new identity，不自动重加。
- T14/T15：begin/activate 前后旧 token 访问所有 read/write/slot/WS 端点；旧 root/password/R/被吊销 HPKE 私钥即使拿到新密文也不能解，未吊销 HPKE 能解自己的 grant。补跨 producer/MIME nonce 唯一性/稳定密文重传。
- T16/T17：primary/fallback 每阶段 manifest/commit/snapshot/structure/asset/actual VV/codec 腐坏，S0<S1；完整预验证无部分写，使用 fallback 自己的 cursor；baseline 损坏/lease 续期/GC 不得越安全 floor。
- T18/T19：无引用/log 已 GC/之前候选 assets 都复制；至少重复多次 rotation，retired epoch 最后两 cp 和 assets/aborted candidate 真回收且留签名骨架；14d 前后直接 GET/无 GC 计划 tick slot 边界，新 enroll 不选旧 staging 槽。
- T20：reservation expiry、Q/R/峰值/未来 live 预算、disk 满/写失败所有 phase；只拒绝/暂停不抢删安全根，故障可 resume。
- T21/T22：>256 controls、多个历史 manager transition 和跳 epoch、historical revoked cert；缺页/重排/外层 kind 伪造/自洽 hash/替换 genesis/低 pin/highwater 全部 fail-stop，首次无 pin 历史回放按明确边界。补真实 HTTPS 无效/过期/错 hostname 证书和 http downgrade 拒绝。

### M1（网络预发布，建议提前）
真正 TLS client + relay 部署、localhost-only HTTP enforcement、token/challenge/origin/速率/请求体/秘密日志 canary、持久 quota 与 disk 门禁。通过前不得向非 localhost 发布。

### M2（最小/完整 app UI，可与 M1 局部并行）
连接/配对/新身份/恢复备份/吊销确认/冻结与故障状态/手工导出，真实两 Windows 设备断网/杀进程、IME remote import 同 reducer，不用复制 DB 冒充第二设备。UI polish 可延期。

### M3（生产运维）
非 root 持久目录、SQLite backup API 和 restore 新 `log_incarnation` 显式 reconcile、重复轮换容量/恢复演练、告警/升级/低磁盘；
安全 fault injection 已是 M0 门禁，不能 M3 才补。全部生产门禁过后才谈生产标签。

## 六、可延期 / 不可延期

**可延期**：完整 UI、美化/预览、自动接管、自动恢复编排、Merkle inclusion（但完整 authenticated leaf manifest 不能删）、
外部新鲜性见证（明确首次回滚边界）、同 epoch asset 孤儿 GC、HA/跨平台。

**不可延期**：root/package/activation 认证、离线 latest grant、两独立真验证、durable 秘密/不可逆冻结、outbox 全状态、跨 epoch/candidate 清理、非 localhost TLS。

**若需缩范围**：先修 nonce/身份认证/正常 checkpoint 真实性；实验版显式关闭 rotation 和破坏性 GC，可仅 localhost/可丢弃测试库。
不能仍称完整 v1 安全吊销；"禁旧 token"只是网络访问吊销，不是旧根无法解新数据。附件若后置应明确禁止 asset 输入/引用，不能留下悬空依赖。

## 七、总体

认可 relay-only/独立管理权/epoch 正交/HPKE/永久 revocation/独立 pair/正确 CAS 方向；
**拒绝当前实现与简报的"已收敛"及 M0 标签结论**。修补两个在途 wire bug、堆测试数或跑通一条 fake 双设备链，都不足以覆盖上述安全闭环。
