# ADR 0013 — 生产 relay 接入：模式、URL 策略与 ControlPin 持久化

> 2026-11-22 历史整理：本文是整理前的历史文档，文中旧 commit 锚点已清除（原哈希不再对应新 `main`）；原始版本见 `backup/dev-20261122`，旧→新映射见 `docs/notes/history-rewrite-20261122.md`。


> 状态：**已决**（2026-11-13，GPT 决策；S1–S3 实施）。
> 关联：[ADR 0006](0006-sync-v1.md)（控制链/pin/§35 提交规则）、[ADR 0011](0011-sync-onboarding-ui.md)（配置）、
> [ADR 0012](0012-autosync-two-phase-gate.md)（预算）。
> 本轮**不接真实远端 relay**、不做 Windows 真机；离线绿不等于生产可发布。

## 前提校正

- `client_for` **已经传 `ControlPin`**；缺口是普通路径没有 durable `record_hash`，不是完全未 pin。
- `StdTcpTransport` 已有 TLS；生产必须显式选 production policy，不能依赖环境敏感默认构造。
- `sync.pinned_control_hash` 是 **bootstrap 摘要**，不是控制链 `record_hash`，不可混用/改名。
- 当前“identity rev 不同就丢 bootstrap hash”与“advancing rev 不校验旧 bootstrap anchor”是两处必修缺口。

## 决策

### 1. 配置、模式与构建

- 公开配置升 `ProfileV2`：每实例显式保存 `relay_mode: local_dev | remote_tls` 与 `relay_url`；v1 只允许
  显式迁移为 `local_dev`；未知版本/模式拒绝。
- 单一权威 URL parser：`remote_tls` 仅 HTTPS，允许 DNS/IP；拒绝 userinfo/query/fragment/非根 base path/
  非法端口；统一 canonical origin。
- `local_dev` 明文仅 `localhost / 127.0.0.1 / [::1]`，必须显式开发能力；不使用宽泛 `is_loopback()` 或
  环境变量放宽生产策略。
- 新增公共 `sync` feature（默认关闭）；`dev-sync → sync` 并额外开放 `local_dev`；正式包只启 `sync`，
  不把 dev-relay 拉进正常依赖。
- Create/Join/设置里明确选择模式与确认 URL；设置修改重启生效。**确认端点 ≠ 授权接受服务器任意 hash**。

### 2. durable pin 与提交规则

- SQLite meta 新增 `sync.control_pin`，canonical CDE v1：
  `{v:1, vault_id:b16, instance_id:b16, record:{rev,hash}, bootstrap?:{rev,hash,epoch,log_id}}`；
  旧 bootstrap 键保持原语义。
- 管理锚来自受保护 identity 或恢复码可信推导，绝不取 relay genesis key；公开 profile 的本地 instance ID
  不是 relay instance ID。
- record 与 bootstrap **各自带 rev**；identity 只提供额外下界；不能把旧 hash 挂到 `max(rev)`；
  `ControlPin` 用 record anchor，异 rev bootstrap 独立验证。
- 只有管理链、genesis/成员证书、授权事实认证、分页/hash 连续性、已有 pin 检查都通过后，
  `control_head()` 结果才是可提交 `VerifiedHead`；不存裸响应 hash。
- 已建立实例缺 record、元数据残缺/损坏、未知版本或绑定矛盾 → 停止；不得退回 identity-only /
  bootstrap-only / unpinned。
- 按 ADR 0006 §35 事务重读 + CAS：同 rev hash 必须相等；高 rev 必须包含旧 authenticated prefix；
  低 rev/换实例拒绝；冲突重新验证，不覆盖赢家。
- 已验证新 head 在授权事实消费前 durable 提交；业务后失败也不回退。SQLite 与身份文件不能跨文件原子，
  增加 journal 阶段 + 启动对账。

### 3. 首次信任

- 新 A：Create 内用本地提交证书复算 `rev1 record_hash = SHA256(零32 || 证书)`；TLS 后验证该前缀与当前链，
  再存强 pin。
- 旧 A 只有 instance/rev 不足以恢复同 rev 历史完整性：需可信同 rev record pin（rev1 可用原始 genesis 材料）；
  否则 `NeedsTrust`，不提供“盲信继续”。
- 新 B：恢复码管理锚 + TLS + 认证后的控制链/checkpoint 签名/对象验证；bootstrap 与 record pin 必须对应
  同一验证视图，再纳入 finalize 的原子提交。
- 旧 B：同 rev 校验原 bootstrap hash；head 已前进时按旧 rev/epoch/log/管理链视图验证历史 anchor 再转 record；
  无法证明则停止，不能忽略旧 hash。
- 已有 record 永远优先于弱初始化路径；两类 anchor 同时存在就都验证；不能因 identity rev 更新丢 bootstrap。
- 初始化只开放 `NewGenesis / FreshRecovery / VerifiedMigration` 受限能力；普通 factory 无 None/unpin/reset
  分支。先 durable 保存可恢复 pending 材料，再发初始化请求，完成对账才启动 worker。

### 4. 失败状态与 UI

- 状态：`Disabled → NeedsTrust/Initializing → Trusted`；网络故障 Backoff，TLS/信任/本地 pin 故障
  `SecurityPaused`；只有校验与保存成功才显示可信。
- 错误分类：`ConfigInvalid`、`TlsUntrusted/NameMismatch/Expired`、`InstanceMismatch`、`Rollback`、
  `Equivocation`、`PinCorrupt/NeedsTrust`、`PinSaveFailed`、`Timeout/Offline`。
- 安全失败保留 vault/identity/pin/journal，停止上传/合并/管理操作，允许本地编辑；只有普通网络错误自动退避，
  安全错误不自动重新信任。
- 设置显示 instance/rev/record 指纹（bootstrap 指纹另行标注）；取消开启不清 pin；换 relay instance 走明确
  新实例或重新 Join，不做“清 pin 再试”。

### 5. TLS 与私有 CA

- 本轮生产入口先用系统可信根；TLS 端点认证与控制历史 pin 是两层独立校验，证书指纹不能代替 control hash。
- 保留链/hostname/IP SAN/有效期；禁止跨 origin 重定向与 HTTPS 降级；**绝不**增加 accept-invalid-cert 开关。
- 私有 CA/自签导入 UI 后置：可带外核验并安装系统根；不能从失败连接自动取证书一键信任。
- 后续按实例引入 `with_extra_roots`（显式导入、应用管理的 DER 副本与预期 SHA256），属 TLS 材料而非 ControlPin。

### 6. 统一 wiring 与预算

- `RelayContext { mode, origin, tls, timeouts, pin_store, manager_anchor }`；`open(vault, identity) → PinnedClient`；
  初始化走独立受限入口。
- production factory 必须 `new_with_policy(..., TransportPolicy::production())`；local_dev 分支单独受 feature 闸控，
  禁止生产调用 `StdTcpTransport::new`。
- sync/rotate/apply/publish/members/revoke 全部接受同一 context；Create/Join 用同一 TLS/URL/预算配置 +
  专用初始化能力；消除 rotation/revoke 等裸 transport 散点。
- 预算：手动 30/30/60s、操作 120s；自动 5/10/30s。DNS 目前无严格总限时，不宣称墙钟 deadline 已保证。

## 实施切片

- **S1**：统一模式/URL/feature policy、`PinStore` 与 `VerifiedHead`/CAS、核心安全负例；远端 worker 暂不开放。
- **S2**：统一全部操作与 Create/Join 初始化；补 bootstrap 历史 anchor 验证、journal/对账与离线集成测试。
- **S3**：公开配置迁移、启用/设置/指纹/暂停 UI、全 wiring 回归；Windows 与真实 relay 未验前保持 WIP，
  **不打生产验收 tag**。

## 离线验收

- URL/配置：模式显式选择、v1/v2、手改配置、端口/IPv6/path/userinfo/query/fragment、生产 env 绕过、多实例隔离。
- Pin：首次初始化、同 rev 换 hash、低 rev、换实例、高 rev 改前缀、bootstrap rev 错配/advance、损坏/缺失、
  CAS 并发、持久化故障。
- 沿用 `tests/tls_transport.rs`：可信根、未信任/过期/错误 SAN、跨 origin/降级、开发明文；不连真实远端。
- wiring：每个操作实际拿到 production transport 与强 pin；缺信任时不发数据/管理请求；初始化/身份提交/
  rotation crash-restart。
- 真实鉴权 relay、Windows 系统根/DPAPI/原子替换、两设备与 UI 真机验收**明确后置**。

## 非目标与回滚

- 非目标：服务器部署、私有 CA 导入 UI、其他 metadata 任务、全局 split-view 检测；无独立不可回滚锚时
  不承诺检测整机备份回滚。
- 回滚只关闭远端入口并保留配置/pin；旧程序只用隔离的旧 profile/vault，不把新实例降回 dev、不清 pin；
  保留 `v0.4-sync`。

## 实施记录

### S1（2026-11-13，旧基线）

`RelayMode`/URL 唯一权威 parser、`PinStore`（`sync.control_pin` canonical CDE v1）、
`VerifiedHead`/`commit_verified_head`（事务重读 + CAS）与 `open_pinned_client` 受限入口。

### S2a（2026-11-13，本片）

- **统一 wiring**：新增 `sync::relay_context::{RelayContext, PinnedClient, RelayTarget, PinPolicy}`；
  `DevLocal::sync/publish/rotate/apply/members/revoke`（含两阶段 Prepare/Integrate/Finish）
  全部经 `RelayContext::open`（`DevLocal` 的 URL 入口只是先构造 `local_dev` context），
  `dev_local`/`dev_local_rotation` 里不再有 `StdTcpTransport::new_local_dev*` 散点；
  `Create/Join` 的初始化仍单独（S2b），只复用 context 的传输。
- **提交时序**：`open` 给客户端装 committer；`/control` 全链验签 + pin 单调性通过后、
  **安装授权缓存之前**先 `commit_verified_head`（失败即不缓存、不消费）。
- **bootstrap 历史 anchor**：`ControlPin.bootstrap` 改为整枚 `BootstrapAnchorV1`
  （rev/hash/epoch/log_id 独立携带，不再"同 rev 才带 hash"）；`verify_control_pin` 同 rev
  用当前视图、异 rev 按锚自己的 rev/epoch/log + **按生效 epoch 截断**的管理链前缀复算
  （`/control` 键 5 的 rev 是管理 rev，不是 control rev）；`VerifiedHead::with_bootstrap`
  自己做历史视图校验；`bootstrap::validate_remote/finalize` 不再忽略 rev 前进后的旧摘要。
- **未做（留给后续）**：远端 UI/worker 的启用与设置页（S3）、Create/Join 的受限初始化能力
  （S2b 已补齐，见下）、journal/启动对账（S2b 已补齐，见下）、Windows 真机与真实鉴权 relay 验收。

### S2b（2026-11-13，本片）

- **受限初始化能力** `sync::init_session`：`InitSession` 只能由三种材料证明构造——
  `NewGenesis` 要本地**已提交**的 genesis 证书 + 管理私钥、`FreshRecovery` 要恢复码、
  `VerifiedMigration` 要已有身份 + 旧 pin；**没有** `unpinned()` / `reset()` / `trust_any()`。
  `HttpRelayClient::with_unpinned_bootstrap_control` 降为 `pub(crate)`，全 crate 唯一调用点
  在 FreshRecovery 的首次 bootstrap，因此"普通 factory 无 None/unpin 分支"是可见性上的事实。
- **NewGenesis**：`genesis::build_materials`（本地材料）→ pending（`Prepared`）→
  `POST /v1/vaults`（写前推进 `GenesisRequested`）→ 用本地提交证书复算
  `rev1 record_hash = SHA256(零32 || CDE(cert))`（`genesis::rev1_record_hash`）→ 带该 pin 构造
  客户端做 `/state + /control` 全链验签 + rev1 前缀检查 → `commit_verified_head` durable 强 pin
  （rev1 record 锚、无 bootstrap 锚）。rev1 公式用**已知证书向量**在 sync 与 relay 两侧各钉一份。
- **FreshRecovery**：恢复码管理锚 → TLS/传输 → `self_enroll`（唯一 unpinned `/control` 消费，
  仍强制管理锚 + 逐条签名）→ 认证 `bootstrap::run_with_pins`（pins/cursor 单事务推进）→
  `verified_head_with_anchor(sync.pinned_* 锚)` 复算历史视图 → `commit_verified_head`。
  **record pin 与 bootstrap 锚必须同 rev/同视图**才提交（`VerifiedHead::with_bootstrap` 拒绝
  "自洽但不是同一视图"的锚）。
- **VerifiedMigration**：构造期就拒绝完整 pin（`Unsupported`）与空身份/缺锚
  （`NeedsTrust`；`remote_tls` 必须有同 rev 锚，因为 identity 的 `(instance, rev)` 只是下界）；
  `run_migration` 用身份下界 + 锚验链，新增 `control_pin::upgrade_lower_bound_pin` 作为
  **唯一**允许同 rev 补 `record.hash` 的路径（要求锚 rev == 已验证链尾 rev，仍走 canonical 字节
  CAS），把旧下界 pin 转成 canonical v1。
- **pending / journal**：`sync.init_pending` canonical CDE v1
  `{v,kind,mode,origin,vault_id,expected_device,phase,created_at}`；阶段
  `Prepared → GenesisRequested → IdentityWritten → PinCommitted → Done` 只进不退、CAS。
  `Prepared` 是**写前记录**（请求尚未发出），`abort_before_request` 是唯一的非 Done 清理；
  `clear_pending` 只在 `Done`。`IdentityWritten` 留给 app 在"临时 + 原子 rename"落盘身份后推进
  （S3 接线）；本地 dev 流程 Create/Join 在 `PinCommitted` 后直接 `Done`。
- **启动对账** `init_session::reconcile(vault, Option<&identity>)`：无 pending → no-op（不联网）；
  `Done` → 清理；`Prepared` → 安全放弃；`GenesisRequested/IdentityWritten/PinCommitted` + 身份 →
  用已有材料补齐 finalize（FreshRecovery 若 bootstrap 未完成则用身份里的 root 续跑 bootstrap，
  **不重复 genesis/enroll**）→ Done + 清理；无身份/损坏/绑定矛盾 → `ConfigInvalid`/`NeedsTrust`，
  不自动重建、不删用户数据。`RelayContext::open` 在 pending 未完成时直接 `NeedsTrust`
  （初始化内部唯一 `open_for_init` 使用点是 Create 的初始 recovery 槽上传）。
- **app 接线**：`DevLocal::create/join` 内部改走 `InitSession`（仍是 `local_dev`，remote 配置/UI
  留 S3）；`dev_sync::setup::prepare_existing` 启动时若发现 pending 就调 `reconcile`（无 pending
  纯 no-op）。返回错误的清理语义**不变**（清掉本次新建的库/身份文件，ADR 0009 app 验收依赖它）；
  pending 服务于**崩溃**（进程消失，清理代码没跑）。
- **测试**：sync **524 passed + 1 ignored**（基线 499+1：新增 11 个 lib 单测 + 14 个集成）；
  storage **171**；app dev-sync **316**；app 默认 **134**。
  新增覆盖：rev1 已知向量（client/relay 双份）、错 rev1/伪造回执（换实例）/回退/前缀不符、
  FreshRecovery 同视图、迁移升级与三类拒绝、pending 阶段矩阵 + 崩溃重启对账、损坏 pending 停止、
  普通 `open` 被 pending 挡住、重复 Join 的 pin 收敛。
- **未做（S3）**：remote 配置迁移/启用/指纹/暂停 UI、`IdentityWritten` 阶段接线、Windows 真机与
  真实鉴权 relay 验收。本轮不接真实远端、不打 tag。

### S3a（2026-11-13，本片）

- **公开配置/模式 UI**：设置页与 Create/Join 表单都有 relay 模式选择；默认 `local_dev`，
  `remote_tls` 必须显式勾选「远程服务器/HTTPS」。模式 + canonical origin 由**唯一权威
  parser** 校验（local 只接受明文 loopback；remote 只接受 https，拒绝 userinfo/query/
  fragment/非根路径），保存写 `ProfileV2` 并提示重启生效。`open_instance` 不再拒绝
  remote 实例：按配置模式打开（remote 走 production TLS policy），不降级 local_dev。
  **Create/Join 选 remote 会在表单内明确拒绝**（S3b 才接网络；不联网/不建库/不写配置）。
- **安全指纹**：设置页新增只读区（`onboarding::fingerprint`）：模式、canonical origin、
  relay instance 短 id、record `rev`+hash 短、bootstrap `rev/epoch/log_id`+hash 短；
  复制按钮给完整公开值。无 pin → 「未建立信任（NeedsTrust）…不会自动信任」；pin 损坏 →
  `PinCorrupt` 明确停住（不当成"没有 pin"）。没有重置 pin/强制信任/接受无效证书按钮。
- **SecurityPaused**：`autosync` 新增 `SecurityCategory`（NeedsTrust/PinCorrupt/
  Equivocation/ControlRollback/InstanceMismatch/Tls/TransportPolicy）与
  `FailureClass::Security`/`classify_sync_error`（类型化映射，先认 `is_retryable()`）。
  TLS/信任/pin 类错误 → `StatusState::SecurityPaused`：自动调度暂停、不重新连接；
  状态行「需要处理：安全暂停（<类别>）」、`sync-status-error` 指向「设置 → 安全指纹」；
  **手动同步也被拒绝**（`SkipReason::SecurityPaused`），不自动清 pin/不盲信；网络错误
  仍按 5/10/20…退避。设置页显示错误类别与指纹。
- **启动 pending 对账**：`AppRoot::open_instance` 在构造 `AppShell` 前调用
  `init_session::reconcile`：无 pending → no-op（不联网）；`Prepared` → 安全放弃；
  `Done` → 清理；已请求 + 有身份 → 补 finalize（不重复 genesis/enroll）；无身份/损坏/
  绑定矛盾 → Error 恢复页（不自动重建、不删库/身份/pin、不启动 worker）。
- **IdentityWritten 接线**：`DevLocal::create/join_deferring_identity` 让 pending 停在
  `PinCommitted`，app（onboarding 的 Create/Join worker 与 CLI setup）在身份原子 rename
  后调用 `init_session::mark_identity_written` + `finish_identity_written`（`PinCommitted`
  在阶段序上晚于 `IdentityWritten`，mark 幂等不回退；`GenesisRequested → IdentityWritten`
  留给远端 S3b 规范顺序）。收尾失败不删数据，留给下次启动对账。
- **app worker 模式**：`ServiceConfig`/`DevLocalRunner` 显式携带 `RelayMode`
  （`RelayContext::from_mode`），不再在 app 层硬编 `local_dev`；remote 走
  `production_transport`，local_dev 仍只 loopback。
- **测试**：app dev-sync **336**（基线 316；onboarding 30 = 基线 19 + 11，含 remote
  模式/非法 URL/v1 迁移/指纹（有 pin/无 pin）/远程 Create/Join 拒绝/pending 四阶段/
  无 pending 不联网）；app 默认 **134**；sync **528 + 1 ignored**（基线 524+1；新增
  deferred Create/Join、规范顺序 IdentityWritten、无 pending 幂等）；storage **171**。
  类型化 `SyncError → Security/Network/Other` 映射与 SecurityPaused 手动拒绝/自动不重试
  都有单测/headless 断言。
- **未做（S3b/后置）**：远端 Create/Join 网络接线、真实鉴权 relay、私有 CA 导入 UI、
  Windows 真机/DPAPI 与真实 TLS 服务器验收。本轮不接真实远端、不打 tag；手工步骤见
  `docs/notes/dev-sync-manual-test.md`「远程模式、安全指纹与 SecurityPaused」。

### S3b（2026-11-14，本片）

- **remote Create / Join 接线**：新增 `sync::remote_init`（`create_remote` / `join_remote`），
  只接受 `remote_tls` context（`local_dev` 明确拒绝，不静默降级），把 onboarding 的远端
  初始化接到受限能力 `InitSession`：
  - **Create（NewGenesis）**：本地材料 → 新库 + pending `Prepared` → `POST /v1/vaults`
    （写前 `GenesisRequested`）→ 回执里的 cursor/log_id/结构行/标签/初始槽材料先落库 →
    调用方回调把身份"临时文件 + 原子 rename"落盘 → `mark_identity_written`
    （`IdentityWritten`）→ 用本地提交证书复算 rev1 `SHA256(零32||CDE(cert))`、TLS 上验证
    `/state + /control` 全链/分页/前缀 → `commit_verified_head` 强 pin（`PinCommitted`）→
    上传初始 recovery 槽 → `finish`（`Done`）。
  - **Join（FreshRecovery）**：恢复码 → 新库 + pending → `self_enroll`（唯一 unpinned
    `/control` 消费）→ 认证 `bootstrap::run_with_pins` → 身份回调落盘 → `IdentityWritten`
    → record pin + bootstrap 锚**同一验证视图** → `commit_recovery_pin`（`PinCommitted`）
    → `Done`。
- **阶段序**：`finalize_genesis` / `commit_recovery_pin` 现在接受 `GenesisRequested` **或**
  `IdentityWritten` 作为前驱（远端是"先身份、后 pin"，local_dev 保持"先 pin、后身份"；
  `mark_identity_written` 对 `PinCommitted` 仍幂等不回退）。phase 推进仍用 canonical 字节 CAS。
- **失败语义（安全优先）**：拿到回执/身份之前失败（URL/TLS/relay 明确拒绝/传输中断）→
  `init_journal::abort_before_identity`（只允许 `Prepared`/`GenesisRequested`，`pub(crate)`）
  + 删除本次新建的库文件（远端最多留下不可用的空 vault/孤儿设备，与 ADR 0009 既有语义
  一致），不写配置、可换目录重试；拿到回执/身份材料之后失败（含身份落盘失败）→
  **保留** pending/库与已落盘事实（身份未落盘时保留现场供人工诊断，不重跑 genesis/
  enroll 覆盖远端事实），Error 页显示安全暂停类别，`Done` 前由启动对账继续。初始槽
  上传 / pending 收尾失败只降级为告警（实例可用；恢复码仍在 app 备份页）。
- **传输与信任**：remote 初始化只经 `RelayContext::transport`（显式
  `TransportPolicy::production()`，系统信任根）；新增
  `RelayContext::with_test_extra_roots`（`#[doc(hidden)]`，只**增加**显式 CA，不关闭链/
  SAN/有效期校验）供本地 TLS harness 注入自签根——不是 accept-invalid，也不是"一键信任"。
- **TLS harness**：`menghunote-dev-relay` 新增 `start_tls()`（rcgen 现场生成自签 CA +
  `localhost` 叶子证书；rustls/ring；axum `Listener` + tokio-rustls）与 `tls_ca_der()`；
  仍是仅测试、只绑 loopback、放行验签的脚手架。见 `crates/sync/tests/remote_init.rs`。
- **app 接线**：Create/Join worker 按 `relay_mode` 分支（local_dev 仍走
  `DevLocal::*_deferring_identity`；remote 走 `remote_init::*` + 身份回调 =
  `DevIdentityStore::persist_protected`）；`CreateOutcome/JoinOutcome` 携带 canonical
  `relay_mode`/`relay_url`，ProfileV2 写 `remote_tls`；`CreateEvent/JoinEvent` 的失败带
  `SecurityCategory`（TLS/NeedsTrust/Equivocation/InstanceMismatch/PinCorrupt），UI 显示
  「安全暂停（<类别>）」与可操作提示，**不自动重试/信任/清 pin**；表单显示 DEV 状态
  「未在真实 relay / Windows 验证」。移除 S3a 的"表单内明确拒绝"分支。
- **测试**：sync 新增 `tests/remote_init.rs`（真实 TLS + 进程内 axum relay：A 创建/B 恢复码
  Join/pin 落盘/崩溃点 `IdentityWritten → reconcile` 续跑/错 setup secret/未信任 TLS/错
  恢复码/local_dev context 拒绝/pending CDE 往返）；app onboarding_ui 新增 6 个用例
  （remote 表单与 DEV 标注、remote Create 真实 TLS 全链路 + 指纹 + 重启、remote Join
  真实 TLS 全链路（自动打开 A 的笔记）+ bootstrap 锚同视图 + 重启、未信任 TLS/错 secret
  不留库与配置、错恢复码回表单且不留库、remote `Prepared` 启动对账不联网）。
- **未做**：真实鉴权 relay（生产 `Ed25519Hook`）上的 remote Create/Join、Windows 真机/
  DPAPI/系统证书存储、私有 CA 导入 UI、跨机器网络/时钟/代理实测。本轮不接真实远端、
  不打 tag。
