# ADR 0008 — Linux 本地身份封装（用户主密钥文件 + XChaCha20-Poly1305）

- 状态：已采纳（2026-11-13，`gpt-6.1-sol` 决策）
- 关联：ADR 0006 §3（本地身份 BLOB / DPAPI 冻结契约）、§14（域分隔符）、§10（风险）
- 实现：`crates/sync/src/identity_wrap.rs`、`crates/sync/src/error.rs`、`crates/sync/src/domain.rs`；
  调用点：`identity.rs`（`LocalIdentityV1` / `IdentityRotationStateV1`）、`rotation_journal.rs`

## 背景

v1 原先只有 Windows DPAPI（`crates/sync/src/dpapi.rs`）能封装本地身份 BLOB。
非 Windows 上 `protect()` 恒返回 `DpapiUnavailable`，导致约 80 个轮换测试与三条真实 HTTP e2e
无法在 Linux 开发者机/CI 上跑生产封装路径，只能用旁路或直接不跑。用户要求补 Linux 适配；
GPT 已拍板采用「Linux 用户封装主密钥文件 + XChaCha20-Poly1305；Windows 原样 DPAPI；其它平台明确不支持」。

约束：

- Windows 的 DPAPI 契约（CurrentUser、entropy、flags、输出字节）**零改动**，旧裸 BLOB 必须继续可解；
- Linux 不能假装"等价于 DPAPI"，也不能回退明文或自造弱加密；
- 不引入重依赖；测试/e2e 必须走**真实生产封装路径**，不得加测试旁路；
- 失败必须显式且可区分（缺钥、坏钥、权限不安全、格式/版本/kind/认证失败），错误不得携带秘密。

## 选项

| 选项 | 代价/问题 |
| --- | --- |
| A. 用 libsecret / Secret Service（D-Bus 钥匙环） | Linux 桌面可用，但无头 CI/容器常没有 keyring daemon；引入 D-Bus 依赖与失败面，测试不稳定 |
| B. 用 TPM（tpm2-tss）封钥 | 依赖硬件与 tss 库，开发者机/CI 不保证；复杂度高 |
| C. 用户数据目录下的 0600 主密钥文件 + AEAD（**选中**） | 同 UID 的进程/文件读取能力即可解密，安全性**弱于 DPAPI**；但不新增系统服务依赖，行为确定、可测试 |
| D. 派生自主口令 / 环境变量提供密钥 | 需要口令交互或把秘密放进环境变量/命令行，违背"不接受环境变量提供密钥"与本地零交互启动 |
| E. 继续只支持 Windows，Linux 测试 skip | 达不到用户与验收要求，轮换/e2e 长期无法在 Linux 验证 |

## 结论

采用 C，并按平台收敛在 `identity_wrap` 门面：

1. **Windows**：`identity_wrap::protect/unprotect` 直接调用 `dpapi::protect/unprotect`，
   **不增加任何外层头**；不探测 Linux magic，旧裸 BLOB 继续可解。
2. **Linux**：新增外层格式（冻结）：
   `magic = b"MNKW"(4B) | version = 1(u8) | kind = 1(u8, LinuxKeyFile) | nonce = 24B random | ciphertext || tag(16)`。
   AEAD 为 `chacha20poly1305::XChaCha20Poly1305`（32B 主钥）。
   AAD = `b"MN/local-wrap/v1\0" || 完整 30B 头 || 原 entropy`；
   域常量 `LOCAL_WRAP_AAD_DOMAIN` 放 `domain.rs`，沿用 ADR 0006 §14 的 NUL 结尾 golden 检查。
   Linux 严格校验 magic/version/kind，**绝不**回退 DPAPI 或明文；Windows 也不探测新 magic。
3. **其它平台**：`LocalWrapUnsupportedPlatform`，不假装成功。
4. **调用点统一**：`LocalIdentityV1::protect/unprotect`、`IdentityRotationStateV1::save/load`、
   `RotationJournal::save/load` 全部改走门面；内层 CDE 类型/meta 键不变，各自沿用原有 entropy
   （身份/旁路状态为 `b"MN/dpapi/v1\0" || vault_id || device_id`；journal 为
   `b"MN/v1/rotation-journal\0"`）。

### 主密钥路径与生命周期

- 路径：`$XDG_DATA_HOME/menghunote/local-identity/key-v1.bin`；
  `XDG_DATA_HOME` 未设或为空 → `$HOME/.local/share/menghunote/local-identity/key-v1.bin`。
  只接受绝对路径；相对路径/缺失 HOME 显式报错，**不退回 cwd 或 tmp**。
- 目录：`menghunote` 不得 group/other 可写；`local-identity` 必须严格 0700；
  属主必须是当前有效 UID；拒绝符号链接与错误 owner。**不自动修复**已有权限。
- 钥文件：0600、同 UID、普通文件、单硬链接、恰好 32 字节；拒绝符号链接。
- **显式初始化** `initialize_user_key()`：OS 随机 32B → 私有临时文件（0600，create_new）
  → 写入后 fsync → `renameat2(RENAME_NOREPLACE)` 原子发布（绝不覆盖已有钥）→ 目录 fsync。
  已存在且合法时幂等 `Ok(())`；已存在但损坏/权限不安全 → 显式失败、不换钥、不修复。
- `protect` / `unprotect` **只加载校验**，不创建、不换钥；缺钥/坏钥显式失败。
- 明文/主钥/临时缓冲全部 `Zeroizing`/`Secret32`；不日志、不接受环境变量提供密钥。

## 威胁模型

**防护目标**：vault 目录/备份被复制到其它用户或离线介质时，本地身份私钥（Ed25519 种子、
X25519 私钥、epoch 根、管理种子）不随明文泄露；AAD 绑定 vault/device/记录类型，防止错配与
跨记录替换；权限校验防止同机其它用户读取/替换主钥。

**明确不声称（弱于 DPAPI）**：

- 主钥是 **0600 裸密钥文件**。同一 UID 下的任何进程（含恶意脚本、被攻陷的开发工具）都能
  读取该文件并解密身份 BLOB；DPAPI CurrentUser 至少把密钥绑定到用户 profile/凭据；
- 不抵抗拥有 root 的攻击者；
- 不抵抗内存抓取（解密期间明文在主进程内存中，`Zeroizing` 只保证释放时擦除）；
- 数据家目录本身被视为用户选择的信任锚（XDG 规范）；若它被同 UID 之外的实体可写，
  Linux 适配不额外保证。应用子树（`menghunote` / `local-identity`）逐级校验；
- 明文正文/索引/导出仍不加密（ADR 0006 §10 风险不变）。

## 后果

- Linux 上轮换测试（约 80 个）与真实 HTTP e2e 可跑生产封装路径；测试 harness 只在进程首次
  使用时设置独立 `XDG_DATA_HOME` 并调用生产 `initialize_user_key()`，此后既有
  `protect/unprotect` 调用完全不变。
- Windows 行为零变化；Linux 产出的 `identity.bin` 不能拿到 Windows 上解（反之亦然），
  跨平台复制 vault 需要重新用口令槽/恢复码解锁并生成设备身份（与跨 SID 迁移同语义）。
- 用户主钥丢失（删除 `key-v1.bin`）等价于 DPAPI 密钥丢失：必须用口令槽/恢复码重新解锁，
  生成新设备身份；**不要**尝试重建主钥来解开旧 BLOB（不可能，且会被显式拒绝）。
- 新增依赖：Linux target `rustix = { version = "1.1", features = ["fs", "process"] }`
  （Cargo.lock 已有 1.1.6；`fs` 用于 `O_NOFOLLOW`/`fstat`/`renameat2`/目录 fsync，
  `process` 只用于 `geteuid()`）。Windows target 仍只有 `windows-sys`。

## 回滚

- 若需回退 Linux 适配：删除 `identity_wrap` 的 Linux 分支并恢复调用点直连 `dpapi` 即可回到
  "Linux 明确不可用"；已写入的 `MNKW` BLOB 在回退后无法解开（与主钥丢失同语义），
  需用口令槽/恢复码重新解锁。
- Linux 外层格式一旦发布即冻结；升级走 version=2 + 双读，不静默改 AAD/头布局。
- Windows DPAPI 契约不在本 ADR 的回滚范围内（ADR 0006 §3 冻结）。

## 验证

- `cargo test -p menghunote-sync --lib`：Linux 主钥格式/AAD golden、权限/符号链接/硬链接/
  截断/篡改/版本/kind/AAD 错配、跨目录复制、两次封装 nonce 不同等负正例。
- `cargo test -p menghunote-sync --all-targets`：此前 87 个 `DpapiUnavailable` 失败全部消失。
- `cargo build -p menghunote-sync --examples`：e2e 示例（Linux 上真实 genesis → identity.protect）
  编译通过；本机无 pwsh，三份 relay e2e 脚本未执行。
