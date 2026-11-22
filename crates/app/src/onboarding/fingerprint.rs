//! 设置页的**安全指纹**只读区（ADR 0013 §4「设置显示 instance/rev/record 指纹」）。
//!
//! ## 为什么单独一层
//!
//! 指纹只由 **durable `sync.control_pin`** 派生（`crate::control_pin::PinV1`）：
//!
//! - 都是**公开定位信息**：relay instance id、control record 的 `rev`/`hash`、
//!   bootstrap 锚的 `rev`/`epoch`/`log_id`/`hash`；**没有** token、恢复码、root、
//!   管理私钥或 identity seed（`PinV1` 类型里就没有这些字段）；
//! - 无 pin 不是"空字符串"，而是明确的 [`FingerprintState::NotTrusted`]：UI 显示
//!   「未建立信任/NeedsTrust」，**不提供**自动"信任并继续"；
//! - 读取失败（损坏/版本未知/vault 绑定不符）不是"没有 pin"，而是
//!   [`FingerprintState::Corrupt`]：按 ADR 0013 §2 停住，不降级成 unpinned。
//!
//! 短展示只用于界面；复制按钮给的是完整值（[`FingerprintState::public_text`]），
//! 方便用户带外核对。换服务器/清 pin **永远不会自动发生**——文案见
//! [`FINGERPRINT_NOTICE`]。

use menghunote_storage::Vault;
use menghunote_sync::control_pin::{BootstrapAnchorV1, PinV1};
use menghunote_sync::error::SyncError;
use menghunote_sync::ids;

use super::layout::RelayMode;

/// 无 durable pin 时的固定文案（**不提供**自动信任/继续）。
pub const NOT_TRUSTED: &str =
    "未建立信任（NeedsTrust）：还没有 durable 控制 pin；不会自动信任，也不提供「信任并继续」。";

/// 指纹区的固定说明（换服务器/清 pin 都不自动发生）。
pub const FINGERPRINT_NOTICE: &str =
    "换服务器 / 清 pin 都不会自动发生：它们是独立的新实例或重新 Join；本页只读。";

/// bootstrap 锚的公开指纹（`{rev, hash, epoch, log_id}`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapFingerprint {
    /// 该 bootstrap 视图对应的 control_rev。
    pub rev: u64,
    /// `control_head_hash` 摘要（完整 32B 十六进制；**不是** record hash）。
    pub hash: String,
    /// 该视图的 epoch。
    pub epoch: u64,
    /// relay log id（完整 16B 十六进制）。
    pub log_id: String,
}

/// 一个已建立 durable pin 的公开指纹。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinFingerprint {
    /// 该实例的 relay 模式（公开配置）。
    pub mode: RelayMode,
    /// canonical origin（公开配置）。
    pub origin: String,
    /// relay instance id（完整 16B 十六进制）。
    pub instance_id: String,
    /// control record 的 rev（下界至少是它）。
    pub record_rev: u64,
    /// control record 链尾 hash；`None` = 只有 `(instance, rev)` 下界（历史实例）。
    pub record_hash: Option<String>,
    /// bootstrap 锚（可选；与 record 各自独立验证）。
    pub bootstrap: Option<BootstrapFingerprint>,
}

impl PinFingerprint {
    /// 从 durable `PinV1` 构造（只复制公开字段；`PinV1` 不含任何秘密）。
    #[must_use]
    pub fn from_pin(mode: RelayMode, origin: &str, pin: &PinV1) -> Self {
        Self {
            mode,
            origin: origin.to_owned(),
            instance_id: ids::hex_of(&pin.instance_id),
            record_rev: pin.record.rev,
            record_hash: pin.record.hash.map(|hash| ids::hex_of(&hash)),
            bootstrap: pin.bootstrap.map(bootstrap_fingerprint),
        }
    }

    /// 界面短展示：instance id（前 8 位）。
    #[must_use]
    pub fn short_instance(&self) -> String {
        short_hex(&self.instance_id)
    }

    /// 界面短展示：record 链尾 hash（前 8 位；无 hash 时给出"只有 rev 下界"）。
    #[must_use]
    pub fn short_record_hash(&self) -> String {
        self.record_hash.as_deref().map_or_else(
            || "（无 hash：只有 rev 下界，禁止前进/同 rev 校验）".to_owned(),
            short_hex,
        )
    }

    /// 界面短展示：bootstrap hash（前 8 位）。
    #[must_use]
    pub fn short_bootstrap_hash(&self) -> Option<String> {
        self.bootstrap
            .as_ref()
            .map(|bootstrap| short_hex(&bootstrap.hash))
    }

    /// 界面短展示：bootstrap log id（前 8 位）。
    #[must_use]
    pub fn short_bootstrap_log_id(&self) -> Option<String> {
        self.bootstrap
            .as_ref()
            .map(|bootstrap| short_hex(&bootstrap.log_id))
    }

    /// 复制按钮用的**完整公开文本**（无秘密；供带外核对）。
    #[must_use]
    pub fn public_text(&self) -> String {
        let mut text = format!(
            "relay 模式：{}\ncanonical origin：{}\nrelay instance：{}\nrecord：rev {} · hash {}\n",
            self.mode.as_str(),
            self.origin,
            self.instance_id,
            self.record_rev,
            self.record_hash.as_deref().unwrap_or("（无）"),
        );
        match &self.bootstrap {
            Some(bootstrap) => text.push_str(&format!(
                "bootstrap：rev {} · epoch {} · log_id {} · hash {}\n",
                bootstrap.rev, bootstrap.epoch, bootstrap.log_id, bootstrap.hash
            )),
            None => text.push_str("bootstrap：无锚\n"),
        }
        text
    }
}

/// 设置页指纹区的可展示状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FingerprintState {
    /// 没有 durable pin（`Ok(None)`）：未建立信任，**不自动继续**。
    NotTrusted,
    /// 有 durable pin。
    Pin(PinFingerprint),
    /// 读取失败（损坏/版本未知/vault 绑定不符）：停住，不当成"没有 pin"。
    Corrupt(String),
    /// 当前没有打开该实例：无法读取 pin（切换默认只写配置，重启后才生效）。
    Unavailable(String),
}

impl FingerprintState {
    /// 从 durable pin 读取结果构造（`Err` 是"损坏"，不是"没有"）。
    #[must_use]
    pub fn from_result(
        mode: RelayMode,
        origin: &str,
        result: Result<Option<PinV1>, SyncError>,
    ) -> Self {
        match result {
            Ok(Some(pin)) => Self::Pin(PinFingerprint::from_pin(mode, origin, &pin)),
            Ok(None) => Self::NotTrusted,
            Err(error) => Self::Corrupt(sanitize(&error.to_string())),
        }
    }

    /// 是否已经有可展示的 durable pin。
    #[must_use]
    pub const fn has_pin(&self) -> bool {
        matches!(self, Self::Pin(_))
    }

    /// 标题行（"有 pin" / "未建立信任" / "pin 损坏"）。
    #[must_use]
    pub fn headline(&self) -> String {
        match self {
            Self::NotTrusted => NOT_TRUSTED.to_owned(),
            Self::Pin(_) => "已提交 durable 控制 pin（下面全部是公开值）".to_owned(),
            Self::Corrupt(detail) => format!(
                "控制 pin 损坏（PinCorrupt）：同步停止，不自动重建/清 pin；请人工核对（{detail}）"
            ),
            Self::Unavailable(detail) => format!("尚未读取 pin 指纹：{detail}"),
        }
    }

    /// 界面逐行展示（label, short value）；`None` = 没有那么多行。
    #[must_use]
    pub fn display_lines(&self) -> Vec<(String, String)> {
        let Self::Pin(pin) = self else {
            return Vec::new();
        };
        let mut lines = vec![
            ("模式".to_owned(), pin.mode.as_str().to_owned()),
            ("canonical origin".to_owned(), pin.origin.clone()),
            ("relay instance".to_owned(), pin.short_instance()),
            (
                "record".to_owned(),
                format!("rev {} · hash {}", pin.record_rev, pin.short_record_hash()),
            ),
        ];
        if let Some(bootstrap) = &pin.bootstrap {
            lines.push((
                "bootstrap".to_owned(),
                format!(
                    "rev {} · epoch {} · log_id {} · hash {}",
                    bootstrap.rev,
                    bootstrap.epoch,
                    pin.short_bootstrap_log_id().unwrap_or_default(),
                    pin.short_bootstrap_hash().unwrap_or_default(),
                ),
            ));
        } else {
            lines.push((
                "bootstrap".to_owned(),
                "无锚（首次 genesis 还没有历史锚）".to_owned(),
            ));
        }
        lines
    }

    /// 复制按钮的完整公开文本；没有 pin 时复制固定说明（**不复制任何秘密**）。
    #[must_use]
    pub fn public_text(&self) -> String {
        match self {
            Self::Pin(pin) => pin.public_text(),
            Self::NotTrusted => NOT_TRUSTED.to_owned(),
            Self::Corrupt(detail) => {
                format!("控制 pin 损坏（PinCorrupt）：{detail}；不自动重建/清 pin")
            }
            Self::Unavailable(detail) => format!("尚未读取 pin 指纹：{detail}"),
        }
    }
}

/// 读取某个库的 durable 控制 pin（**只读**；`Ok(None)` = 从未建立）。
pub fn load(vault: &Vault, vault_id: [u8; 16]) -> Result<Option<PinV1>, SyncError> {
    menghunote_sync::control_pin::load_control_pin(vault, vault_id)
}

fn bootstrap_fingerprint(anchor: BootstrapAnchorV1) -> BootstrapFingerprint {
    BootstrapFingerprint {
        rev: anchor.rev,
        hash: ids::hex_of(&anchor.hash),
        epoch: anchor.epoch,
        log_id: ids::hex_of(&anchor.log_id),
    }
}

/// 十六进制短展示：<= 10 个字符原样，否则前 8 个 + `…`（与设置页其它公开 id 一致）。
fn short_hex(value: &str) -> String {
    if value.chars().count() <= 10 {
        value.to_owned()
    } else {
        let head: String = value.chars().take(8).collect();
        format!("{head}…")
    }
}

/// 错误文案只保留一行（不把整段 SQL/传输日志塞进设置页）。
fn sanitize(text: &str) -> String {
    let one_line = text.split(['\n', '\r']).next().unwrap_or(text).trim();
    if one_line.chars().count() > 160 {
        let mut truncated: String = one_line.chars().take(160).collect();
        truncated.push('…');
        truncated
    } else {
        one_line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use menghunote_sync::control_pin::{PinAnchorV1, PinV1};

    fn pin_without_bootstrap() -> PinV1 {
        PinV1 {
            vault_id: [0x11; 16],
            instance_id: [0x22; 16],
            record: PinAnchorV1 {
                rev: 1,
                hash: Some([0x33; 32]),
            },
            bootstrap: None,
        }
    }

    fn pin_with_bootstrap() -> PinV1 {
        PinV1 {
            bootstrap: Some(BootstrapAnchorV1 {
                rev: 2,
                hash: [0x44; 32],
                epoch: 3,
                log_id: [0x55; 16],
            }),
            ..pin_without_bootstrap()
        }
    }

    #[test]
    fn no_pin_is_not_trusted_and_has_no_auto_trust_text() {
        let state =
            FingerprintState::from_result(RelayMode::LocalDev, "http://127.0.0.1:1", Ok(None));
        assert_eq!(state, FingerprintState::NotTrusted);
        assert!(!state.has_pin());
        assert!(state.headline().contains("未建立信任"));
        assert!(state.headline().contains("NeedsTrust"));
        assert!(state.display_lines().is_empty());
        assert_eq!(state.public_text(), NOT_TRUSTED);
        assert!(state.public_text().contains("不会自动信任"));
    }

    #[test]
    fn pin_fingerprint_exposes_only_public_identity_fields() {
        let state = FingerprintState::from_result(
            RelayMode::RemoteTls,
            "https://relay.example",
            Ok(Some(pin_with_bootstrap())),
        );
        let FingerprintState::Pin(pin) = &state else {
            panic!("必须是 Pin 状态");
        };
        assert_eq!(pin.mode, RelayMode::RemoteTls);
        assert_eq!(pin.origin, "https://relay.example");
        assert_eq!(pin.instance_id, "22".repeat(16));
        assert_eq!(pin.record_rev, 1);
        assert_eq!(pin.record_hash.as_deref(), Some("33".repeat(32).as_str()));
        assert_eq!(pin.short_instance(), "22222222…");
        assert_eq!(pin.short_record_hash(), "33333333…");
        let bootstrap = pin.bootstrap.as_ref().expect("bootstrap 锚");
        assert_eq!(bootstrap.rev, 2);
        assert_eq!(bootstrap.epoch, 3);
        assert_eq!(bootstrap.log_id, "55".repeat(16));
        assert_eq!(bootstrap.hash, "44".repeat(32));
        // 复制文本：完整公开值；不含任何秘密形态。
        let text = state.public_text();
        assert!(text.contains(&"22".repeat(16)), "{text}");
        assert!(text.contains(&"33".repeat(32)), "{text}");
        assert!(text.contains("bootstrap：rev 2"), "{text}");
        for forbidden in ["mnrec1", "secret", "root", "private", "token"] {
            assert!(
                !text.to_ascii_lowercase().contains(forbidden),
                "指纹不得含 {forbidden}：{text}"
            );
        }
        // 展示行是短值（复制按钮才给完整值）。
        let lines = state.display_lines();
        assert!(lines
            .iter()
            .any(|(label, value)| label == "模式" && value == "remote_tls"));
        assert!(lines
            .iter()
            .any(|(label, value)| label == "record" && value.contains("33333333…")));
    }

    #[test]
    fn pin_without_record_hash_is_explicit_lower_bound() {
        let mut pin = pin_without_bootstrap();
        pin.record.hash = None;
        let state =
            FingerprintState::from_result(RelayMode::LocalDev, "http://127.0.0.1:1", Ok(Some(pin)));
        let FingerprintState::Pin(pin) = &state else {
            panic!("必须是 Pin 状态");
        };
        assert!(pin.short_record_hash().contains("只有 rev 下界"));
        assert!(state.public_text().contains("record：rev 1 · hash （无）"));
    }

    #[test]
    fn unavailable_instance_is_not_reported_as_not_trusted() {
        let state = FingerprintState::Unavailable("该实例未打开".to_owned());
        assert!(!state.has_pin());
        assert!(state.headline().contains("尚未读取"));
        assert!(!state.headline().contains("NeedsTrust"));
        assert!(state.public_text().contains("未打开"));
    }

    #[test]
    fn corrupt_pin_is_reported_as_error_not_as_missing_pin() {
        let state = FingerprintState::from_result(
            RelayMode::LocalDev,
            "http://127.0.0.1:1",
            Err(SyncError::PinCorrupt("字节损坏")),
        );
        match &state {
            FingerprintState::Corrupt(detail) => {
                assert!(detail.contains("控制 pin 损坏"), "{detail}");
                assert!(detail.contains("字节损坏"), "{detail}");
            }
            other => panic!("损坏必须单独成类：{other:?}"),
        }
        assert!(!state.has_pin());
        assert!(state.headline().contains("PinCorrupt"));
        assert!(!state.public_text().contains("未建立信任"));
    }
}
