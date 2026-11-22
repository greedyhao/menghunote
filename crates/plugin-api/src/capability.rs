//! 插件能力（capability）声明（ADR 0020 §3.3 / §10.3）。
//!
//! 能力是 manifest 的**声明**，不是授权结果：host 将来必须按安装/首次使用
//! 时的用户授权决定实际开放哪些能力；本切片不实现授权 UI、签名或加载。
//!
//! JSON 拼写固定为 `snake_case`（见 [`Capability::as_str`]）。serde 使用
//! 精确枚举，**未知能力在反序列化阶段直接拒绝**，不会落到“静默忽略”分支。
//! 本切片没有 `#[serde(other)]` 兜底，这是刻意设计：清单里的未知能力不能被
//! 当成无害字段接受。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 插件可声明的能力集合。
///
/// 本切片只固定“有哪些名字、JSON 怎么拼、各自含义”；具体的 host 服务接口
/// 见 [`crate::host`]。`Network` / `Filesystem` / `Clipboard` 目前**没有**
/// 任何 host 接口，也没有授权 UI——它们只是为未来预留的声明值，未加载。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// 读取笔记列表与正文快照（host 决定粒度；不直接访问 SQLite）。
    NotesRead,
    /// 创建/更新笔记（host 负责事务、撤销与同步边界）。
    NotesWrite,
    /// 发起网络请求（**本切片无接口、无授权 UI**）。
    Network,
    /// 读写用户选择的文件（**本切片无接口、无授权 UI**）。
    Filesystem,
    /// 读写剪贴板（**本切片无接口、无授权 UI**）。
    Clipboard,
    /// 向用户显示通知。
    Notifications,
    /// 读写插件自己的设置项。
    Settings,
    /// 注册命令（命令由 host 执行；本切片不实现命令执行）。
    Commands,
}

impl Capability {
    /// 全部能力（固定顺序，用于 manifest 文档、授权页与测试）。
    pub const ALL: [Self; 8] = [
        Self::NotesRead,
        Self::NotesWrite,
        Self::Network,
        Self::Filesystem,
        Self::Clipboard,
        Self::Notifications,
        Self::Settings,
        Self::Commands,
    ];

    /// manifest JSON 中的稳定拼写（与 serde 完全一致）。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotesRead => "notes_read",
            Self::NotesWrite => "notes_write",
            Self::Network => "network",
            Self::Filesystem => "filesystem",
            Self::Clipboard => "clipboard",
            Self::Notifications => "notifications",
            Self::Settings => "settings",
            Self::Commands => "commands",
        }
    }

    /// 本切片是否已有 host 服务接口。
    ///
    /// `false` 只表示“契约还没有对应 trait”，不表示将来不会开放；加载器与
    /// 授权仍未实现，不能据此宣称能力可用。
    #[must_use]
    pub const fn has_host_interface(self) -> bool {
        matches!(
            self,
            Self::NotesRead | Self::NotesWrite | Self::Settings | Self::Notifications
        )
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_names_are_snake_case_and_round_trip() {
        for capability in Capability::ALL {
            let json = serde_json::to_string(&capability).expect("能力必须可序列化");
            assert_eq!(json, format!("\"{}\"", capability.as_str()));
            let parsed: Capability =
                serde_json::from_str(&json).expect("自己序列化的能力必须可反序列化");
            assert_eq!(parsed, capability);
            assert_eq!(capability.to_string(), capability.as_str());
        }
    }

    #[test]
    fn unknown_capabilities_are_rejected() {
        for raw in [
            "\"secrets\"",
            "\"network_write\"",
            "\"NotesRead\"",
            "\"notes-read\"",
            "\"\"",
            "null",
            "1",
        ] {
            assert!(
                serde_json::from_str::<Capability>(raw).is_err(),
                "未知能力必须拒绝: {raw}"
            );
        }
    }

    #[test]
    fn host_interface_facts_are_explicit() {
        let with_interface: Vec<_> = Capability::ALL
            .into_iter()
            .filter(|capability| capability.has_host_interface())
            .collect();
        assert_eq!(
            with_interface,
            [
                Capability::NotesRead,
                Capability::NotesWrite,
                Capability::Notifications,
                Capability::Settings
            ]
        );
        for capability in [
            Capability::Network,
            Capability::Filesystem,
            Capability::Clipboard,
            Capability::Commands,
        ] {
            assert!(!capability.has_host_interface(), "{capability}");
        }
    }
}
