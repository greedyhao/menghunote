# 评审 0001 — 同步 v1 轮换安全复核（gpt-6.1-sol，2026-11-11）

> 输入：ADR 0001 §3.6/§3.7、ADR 0006（§1–§27）、`crates/relay`（schema v5）与 `crates/sync` 实现。
> 结论直接决定 v1 轮换是否可交付；本文是原始评审记录，决策摘要见 ADR 0006 §28。

## 总体结论

freeze 的 SQLite 线性化边界基本正确，但 relay 还不是 ADR §8 的安全轮换闭环；
**不要以 119–120 个测试作为完整安全验收**。

## 一、偏差分级

### 可接受并记录
- HTTP 写统一 `api.rs` → `Db::write` → `BEGIN IMMEDIATE`；begin 同一事务分配 epoch、取旧 head 为 S、revoke/auth_rev、置 ROTATING、写 rotation/control；
  complete 在事务中重查冻结状态，因此新旧 data append 按 writer 顺序截断（先于 freeze 的 complete ≤ S，后者 409）。
  旧已完成对象的相同 commit 返回原 ACK、不新增 seq 可接受。
- **不新增吊销的 `begin([])` 没有 bump_auth_rev**；若 ADR freeze 一律废旧 token，需补一次。

### 必须修（编号对应评审原文）

2) 最初 `revoked_count` 局部计数可被 `begin(revoke D)→abort→begin([])→abort` 绕过。
   加 `epochs.revoked_once` 对新库有效，但 v4→v5 `DEFAULT 0` **必须回填**历史 `rotations(from_epoch=e, revoked_count>0)`。
   未来每个 revoke 入口都应在同事务置此事实；restart/revoke-more 必须保持冻结，不能用 clean abort+begin 两请求替代。

3) 候选 cp 提前 `published`/`schedule_gc_plan`；GC 查询跨 epoch；abort 不作废已 published 候选。
   未 activate/aborted 候选能推动旧日志 GC。改 `candidate_ready`，activate 原子 promote 两个指定 cid 并创建 activated pair 的 7 天计划；
   GC 只看 active epoch 的 committed pair，abort 作废候选，旧 pair/log/lease 保持可恢复。

4) activate 只 `COUNT>=2` 且 `MIN(covered_seq)>=S`，有数字覆盖但**没有独立性**：两个 cid 可引用同一 snapshot/manifest，
   且 activate 未重检密文完整性。客户端必须分别在两套空 Loro/空库上下文导入所有 docs/tombstones/全因果历史/assets，
   并比较**实际 VV**（不是相信载荷宣称的 VV）覆盖每个 ≤S 更新。
   relay 可检查 cid/manifest 不同、SNAPSHOT refs 不相交（同 epoch ASSET 可共享）、同 epoch/S、commit/摘要绑定；
   relay 不解密/Loro 是正确边界，不应给它 crypto/Loro 依赖。

5) 新旧管理转换最初仅入库且可 None；最新 helper 虽验双签，仍 raw arbitrary transition + `Expectation::None`，
   没解码绑定 rev/pub/vault/instance/rid/from/to/S，`new_manager` 仍可 None，且只保存 transition 裸 bytes、不保存两签，
   `/control` 不返回 `manager_keys` 转换。
   需要 mandatory 新 manager、shared canonical Transition、两签覆盖同一绑定所有激活材料的记录、永久保存和提供公钥/签名链。

6) `stage_grant` 用 `Expectation::None`；已有 `EpochGrant` validator 未接入；signed_header 与实际 recipient/enc/ciphertext 可不一致；
   `grants_digest` 仅 hash(header)，不是完整包；signature 不存表、不返回最新 grant。
   从 certified recipient HPKE key 和当前事实重建 `GrantAadV1`、enc32、ct hash 验签；包不可变、digest 覆盖 AAD/enc/ct hash/signature；
   最新 grant 无 TTL 的选择正确，应保留。

7) management `signed.payload` 与实际外层 `expected_epoch/revoke/new_manager` 并存、未逐字段交叉绑定；
   verifier 只 hash `signed.payload`，业务执行消耗外层字段。改为从唯一 canonical unsigned body 解析所有业务字段，或完整一致性校验。
   request body_sha256/request_id 也没实现管理动作 dedup，响应丢失后不能安全找回 r/cid。

8) old slots `retire_at=activate+14d` 正确，但 purge helper 未被 tick 调用，且只删 key_slots，不删 KEY_SLOT parts/body；
   知道 id 的 GET 只查对象 type/budget，没有槽状态/retire_at。
   统一读权限、到期真实删除、即使无 GC plan 也执行 TTL；不应让 later-enrolled 新设备自动享有旧迁移槽窗口。

9) 完整 finalized ASSET 枚举 JOIN 可删 log；GC 后漏资产；候选 ASSET 不进 log，下一轮轮换又漏上一轮复制资产。
   新增持久 opaque 资产登记+冻结 asset cutoff/digest；不要 relay 存真实 SHA256、MIME、明文 VV/doc_id。

10) GC 只清 update parts+未完成暂存；旧 cp 不真正 prune，旧 snapshot/manifest/跨 epoch asset copies 长期 refs 残留。
    先保证 active 两基线/lease/完整资产复制，旧 cp 无根/lease 后 prune 并清 refs、保留签名骨架；
    旧密文 7 天 GC 与槽 14 天期限分开。完整 same-epoch ASSET orphan 删除可后置，跨 epoch 旧副本与候选垃圾不能用它当借口。

11) 客户端 `checkpoint.rs` 先 prepare_docs 再拿 S、先 publish 再由调用者 read_back；不能保证覆盖 S/发布前验证。
    拆 prepare(S) → read-back/import actual VV validation → sign/promote。

12) `bootstrap.rs` 只在 manifest 获取/验签失败时 fallback，snapshot/asset 错误不回退；选 fallback 仍写 primary.S/cursor，
    造成 (S0,S1] 数据跳过；existing body 直接 return，不是 rotation merge；多事务写 docs/asset/cursors，不能宣称总是无半库。
    （6/7 checkpoint 签名字段、`/control` 成员 cert_signature 已存在，但 management transition chain 尚缺。）

### 建议后置
第二台设备见证/receipt、Merkle grant membership proof（v1 可用完整 canonical leaf manifest）、自动接管 UI、资产孤儿删。
**不能后置**：双基线独立、弃候选根、双签、outbox CAS。

## 二、客户端可执行状态机（摘要）

持久 DPAPI journal 至少含：phase、request_id/body_digest、old/from_epoch、rid/to_epoch、freeze_seq/log_id/control_rev/control_hash、
candidate_root、新管理私钥/公钥 rev、已备份 R' 标志、新槽 descriptors/固定密文/IDs、两 cid/对象 IDs/digests/verified receipts、
member/certs/grants/asset-set digest、上传进度、outbox repack 进度。现 `PendingRotationV1` 只有 epoch/rid/root/time，不够。
**新 password 必须是新秘密、R'=random32、新管理 key 来自 R'**，不是旧 password 加新 salt。

A. 发起前验完整管理链/证书、pin instance/log/control head、停止网络 push/encrypt（A 事务编辑继续）；确认新恢复码备份；
   durable 保存 intent/secrets。epoch 未知先存 provision intent，收到 r/E 补齐；任何上传前落固定 ciphertext。
B. 管理签 `POST begin(expected_epoch/control_rev, revoke IDs, request_id)`，固定 S、冻结旧追加；保存 r,E,S；
   丢响应按 request_id/current rotation 查询，不盲 begin。旧 token 失效后未吊销设备重新 challenge；REVOKED ≠ 删本地数据。
C. 发起者用旧根 pull+verify+integrate 至 S；≤S 有 quarantine/pending 或因果洞则禁止继续。历史证书仍可导入被吊销设备在 S 前提交的合法更新。
   合并发起者离线状态，从一致性读快照固定 local cut L。
D. 冻结的 finalized 旧资产逐个下载解密/真实 SHA 校验（含墓碑/非可见引用/无引用 finalized）；新根下重算 HMAC object_id、
   按 ASSET 确定性 nonce 重加密。全 docs（正文+structure+tombstones）full-history snapshot，不改 peer/device/doc generation。
E. 两份全基线独立（cid/manifest/snapshot 对象独立，assets 可共享同 E 已验证对象）；各自从 relay 全量读回、到**互不共享状态**的两个空 import 上下文，
   检查信封/签名/refs_digest/manifest 实际 VV/frontier/min_reader/全部资产。两个 signed CheckpointCommit 都 S>=freeze_seq；ready 前不对普通 bootstrap/GC 生效。
F. 给确认证书集合中每个未吊销设备（含 90 天离线者）seal HPKE root，持久化并 stage，不能看裸 relay 公钥。
   staged 可用旧管理 rev m 签；最终双签 activation 锁定所有 grant 完整 digest，客户端验证 inclusion；激活后 m 不再有新增授权权。
   新成员 cert 用 m+1 签，激活原子发布。
G. 共享 T：pv,instance/vault,rid,from/to,S,old/new_manager_rev,new_pub,prev_control_hash/expected_rev,pair cid+commit digests,
   slot-set digest,full member cert digest,grants digest,old/new asset-set digest；旧/新管理 key 用专用 NUL 域双签同一 T。
   activate 携带整套材料与 CAS；relay 一个事务验签/绑定/完整性/预算→发布 slots+两 cid+certs+grants+manager transition→切 epoch/auth_rev→EpochActivated。
   网络超时不能 abort，先查 activation_commit/result。
H. 远端 activated 后 initiator 读回自己最新 grant 解封并比 candidate root，再原子本地推广 DPAPI root/manager pin/epoch+repack fencing。
   其它设备看到 state/wake epoch 变化先暂停旧 push，重新认证，逐条验证 management 转换/成员状态/activated commit+自己的 grant 外签/recipient/epoch/HPKE key 和集合 inclusion，
   解出 E 根；不接收低于可信 epoch/control/highwater 的回滚。
I. 用最新完整 cp 先校验，再把 full Loro snapshot import/merge 入现有状态；不是 bootstrap existing body skip、更不是替换 SQLite。
   按成功 baseline 的 S_c/cursor 持久化，补 S_c 后日志；全文、墓碑、offline ops、local-only docs/assets 全保留。
   下载验证阶段不写真库；merge 分批需 journal，全部完成才能推进 cursor/ACK。
J. outbox：只用"已激活、远端 durable、已验证 baseline 的该 doc/generation VV"（或新 epoch durable ACK）判断端点 VV 覆盖；
   绝不能用合并后的 local VV（含未上传编辑）。覆盖的旧意图可 CAS 删除；其余保持 update_id/batch_id/origin/原 Loro bytes，
   用新 message_id/object_id/E/nonce 重包，先 durable 固定 parts 再发送；替换以旧 (message_id,epoch,digest) CAS 提交，迟到旧 ACK 忽略。

**resume 规则**：同 r、同成员/grant/slots/cid digest、候选根秘密未丢未泄露 → 固定密文继续；lease 过期续租不换根。
远端 activated → 比对 journal committed hash 并推广，不重加密/再 activate。候选根丢失/泄露或后来吊销任何已生成 grant 收件人 →
原子 restart（旧候选彻底 aborted、更高 epoch、fresh root/slots/R'/管理 key），旧写保持冻结。已吊销设备的离线 ops 不自动重包，只能显式导出后以新身份/新 ops 恢复。
只在无任何历史吊销/旧秘密风险且用户明确 cancel 的独立路径考虑解冻。

## 三、wire 必补

已有：`GET rotations/{r}` 键 0 r/1 state/2 from/3 to/4 S/5 revoked_count/6 member_hash?（激活才有）/7 grant_count/8 digest/9 cp_count；
`GET rotations/{r}` 仅 device realm，管理恢复不通；digest 未完整绑定包。

必补：
1. `state.current_rotation_id` 或 `GET rotations/current`（管理+device 可读元数据），request-id 幂等查结果；
2. r view 含 control/auth/manager rev、不可解冻事实、freeze asset cutoff/count/digest、完整成员 cert digest、
   两个候选 cid 和 manifest/commit/signature/health/readback-ready、slot ids/descriptor digest、reservation owner/expiry/bytes、activated commit hash/seq；
3. candidate cp 元数据/GET、固定 role primary/fallback+request-id 创建，candidate_ready 不 published，renew/takeover reservation；
4. `GET staged grants` 限 management，最新 grant 保存返回 signature64/grant_id/activation_ref/完整 grant leaf manifest/inclusion，包 digest 有 cipher/enc/签名；
5. mandatory typed 双签 `ManagementTransition` + 全永久管理公钥链和 `SignedControlRecord`，不仅裸 record_hash；
6. activate 明确指名两个 cid、新两槽、证书集合、成员/grants/assets digest 和 expected CAS，返回精确激活 commit/cids；
7. 原子 `rotations/{r}:restart/revoke-more` 保持冻结；
8. frozen finalized 资产分页读不依赖 log、同 cutoff 下一致分页+digest；
9. degraded 读回/repair：一基线损坏可读另一成功基线与其 S，但禁止 GC 前进直到补齐两份；
10. 控制事件要有明确 `EpochActivated` 的 rid/E/S/commit hash，不是只有不知种类的 signed payload。

所有增量在 `sync-proto` 冻结 CDE/整数键/真正 NUL/golden vectors；v1 拒未知键，不假设静默加字段兼容，须显式版本/能力协商。MNE1 本体无需变更。

## 四、必须测试矩阵（T01–T21）

T01 并发旧 PUT/complete 与 freeze；T02 freeze 事务各写后 kill/reopen + begin 响应丢；T03 freeze 后客户端崩溃；
T04 两 cid 复用相同 snapshot/manifest、shallow、宣称 VV>S 但实际缺 op；T05 candidate ready 后拖 8 天/abort/多候选；
T06 activate 前/各 SQL 点 kill、commit 后丢响应；T07 双签缺一/None/错字段；T08 管理 payload 与外层不一致/request_id 重放；
T09 grant AAD/enc/ct 篡改或移花接木；T10 候选 D grant 已生成后吊销 D 再 restart；T11 revoke→abort→begin([])→abort + v4 升级；
T12 outbox 延迟旧 ACK；T13 离线 90 天/多次轮换/旧槽 14 天/GC 后；T14 旧 token 全端点 + freeze/activate 屏障；
T15 旧 root/P/R/被吊销 HPKE 私钥不能解新密文；T16 fallback 各损坏路径 + S0<S1 游标；T17 GC 前 part 缺失/摘要损坏 + lease；
T18 无可见引用/已 GC/log 外的 finalized 资产 + 上轮候选资产；T19 14d-1/14d 列表与直接 GET + tick 无 plan；T20 reservation 过期/额度/磁盘；
T21 control 分页 + 旧公钥永久 + 自洽裸 hash 链不能当授权链。

## 五、最高风险排序

1. 只改 root/set_root 或只删 grant 行；
2. 比较 local VV 误删所有 offline outbox；
3. 候选被当已发布基线驱动 GC；
4. 未类型绑定/未保存的双签和裸 hash 链，给人"已验证"错觉；
5. 把资产清单当可见引用/可删 log；
6. 把 fallback 的较老 state 与 primary 较新 cursor 混用，静默永久丢操作。

**以上任一点不修，不建议开启生产吊销轮换。**

## 补充（评审收尾追加）

- 测试矩阵再补 **T22 长控制链**：超过 256 条控制记录、多次管理转换、自洽伪造 hash 链；
  必须从 pin 分页续验、旧公钥永久可用、新公钥可信、裸 hash 连续不被当授权证明、S 前历史签名仍可验证。
- 明确"可接受并记录"的两条边界：relay 不证因果覆盖（不运行 Loro/不解密是正确边界）、最新 grant 不按离线时间淘汰（无 last_seen TTL）。
- 现有 `crates/relay/tests/support` 使用弱 TestHook，不能证明端到端密码学不变量；安全链路必须真实 Ed25519/HPKE + HTTP。
