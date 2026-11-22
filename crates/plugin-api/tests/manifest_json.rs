//! manifest JSON 契约测试（ADR 0020 P2）。
//!
//! 这些测试锁定三件事：
//!
//! 1. 完整 manifest 可以被 `serde_json` 反序列化并通过校验；
//! 2. 结构非法（缺必需字段/未知能力/未知设置类型/坏版本）在**反序列化阶段**
//!    就被拒绝；
//! 3. 结构合法但语义非法（未知 locale、重复能力、不支持的 API 版本）由
//!    [`PluginManifest::validate`] 拒绝——未来加载器必须据此拒绝加载。

use menghunote_plugin_api::{
    ApiVersion, Capability, PluginManifest, PluginVersion, SettingScope, SettingType, SettingValue,
    ValidationError,
};

const FULL_MANIFEST: &str = r#"
{
  "id": "com.example.notes",
  "name": "Example Notes",
  "version": "1.2.3",
  "api_version": 1,
  "locales": ["zh-CN", "en"],
  "capabilities": ["notes_read", "notes_write", "settings", "notifications"],
  "commands": [
    {
      "id": "open-dashboard",
      "title": "Open dashboard",
      "description": "Open the example dashboard"
    },
    { "id": "count-words", "title": "Count words" }
  ],
  "panels": [
    { "id": "dashboard", "title": "Dashboard", "slot": "right-sidebar" }
  ],
  "settings": [
    { "id": "enabled", "type": "bool", "default": true, "scope": "user" },
    { "id": "limit", "type": "integer", "default": 10, "scope": "vault" },
    { "id": "greeting", "type": "text", "default": "hello", "scope": "user" }
  ]
}
"#;

#[test]
fn full_manifest_deserializes_and_validates() {
    let manifest: PluginManifest =
        serde_json::from_str(FULL_MANIFEST).expect("完整 manifest 必须可反序列化");

    assert_eq!(manifest.id, "com.example.notes");
    assert_eq!(manifest.name, "Example Notes");
    assert_eq!(manifest.version, PluginVersion::new(1, 2, 3));
    assert_eq!(manifest.api_version, ApiVersion::CURRENT);
    assert_eq!(
        manifest
            .locales
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["zh-CN", "en"]
    );
    assert_eq!(
        manifest.capabilities,
        vec![
            Capability::NotesRead,
            Capability::NotesWrite,
            Capability::Settings,
            Capability::Notifications
        ]
    );
    assert_eq!(manifest.commands.len(), 2);
    assert_eq!(
        manifest.commands[0].description.as_deref(),
        Some("Open the example dashboard")
    );
    assert_eq!(manifest.commands[1].description, None);
    assert_eq!(manifest.panels[0].slot, "right-sidebar");
    assert_eq!(manifest.settings[0].value_type, SettingType::Bool);
    assert_eq!(manifest.settings[0].default, SettingValue::Bool(true));
    assert_eq!(manifest.settings[1].default, SettingValue::Integer(10));
    assert_eq!(
        manifest.settings[2].default,
        SettingValue::Text("hello".to_owned())
    );
    assert_eq!(manifest.settings[1].scope, SettingScope::Vault);
    assert_eq!(manifest.validate(), Ok(()));

    let round_trip = serde_json::to_string(&manifest).expect("manifest 必须可序列化");
    let reparsed: PluginManifest =
        serde_json::from_str(&round_trip).expect("序列化结果必须可反序列化");
    assert_eq!(reparsed, manifest);
    assert_eq!(reparsed.validate(), Ok(()));
}

#[test]
fn minimal_manifest_defaults_optional_sections() {
    let manifest: PluginManifest = serde_json::from_str(
        r#"
        {
          "id": "a",
          "name": "A",
          "version": "0.1.0",
          "api_version": 1,
          "unknown_future_field": { "anything": true }
        }
        "#,
    )
    .expect("可选集合与未知字段必须被容忍");
    assert!(manifest.locales.is_empty());
    assert!(manifest.capabilities.is_empty());
    assert!(manifest.commands.is_empty());
    assert!(manifest.panels.is_empty());
    assert!(manifest.settings.is_empty());
    assert_eq!(manifest.validate(), Ok(()));
}

#[test]
fn unknown_capability_is_rejected_by_deserialization() {
    let error = serde_json::from_str::<PluginManifest>(
        r#"
        {
          "id": "a.b",
          "name": "A",
          "version": "1.0.0",
          "api_version": 1,
          "capabilities": ["secrets"]
        }
        "#,
    )
    .expect_err("未知能力必须被 serde 拒绝");
    let message = error.to_string();
    assert!(
        message.contains("unknown variant") || message.contains("unknown field"),
        "{message}"
    );
}

#[test]
fn unknown_setting_type_and_scope_are_rejected() {
    for (field, json) in [
        (
            "type",
            r#"{"id":"x","type":"float","default":1.5,"scope":"user"}"#,
        ),
        (
            "scope",
            r#"{"id":"x","type":"text","default":"v","scope":"global"}"#,
        ),
    ] {
        let manifest = serde_json::from_str::<PluginManifest>(&format!(
            r#"{{
              "id": "a.b",
              "name": "A",
              "version": "1.0.0",
              "api_version": 1,
              "settings": [{json}]
            }}"#
        ));
        let error = manifest.expect_err("未知 {field} 必须被拒绝");
        assert!(
            error.to_string().contains("unknown variant"),
            "{field}: {error}"
        );
    }
}

#[test]
fn missing_required_fields_and_invalid_version_are_rejected() {
    let base = serde_json::json!({
        "id": "a.b",
        "name": "A",
        "version": "1.0.0",
        "api_version": 1
    });
    for field in ["id", "name", "version", "api_version"] {
        let mut object = base.clone();
        object
            .as_object_mut()
            .expect("测试夹具必须是 JSON 对象")
            .remove(field);
        let error =
            serde_json::from_value::<PluginManifest>(object).expect_err("缺必需字段必须被拒绝");
        assert!(
            error.to_string().contains("missing field"),
            "{field}: {error}"
        );
    }

    for version in ["1", "1.2", "1.2.3.4", "1.02.3", "x.y.z"] {
        let error = serde_json::from_str::<PluginManifest>(&format!(
            r#"{{ "id":"a.b", "name":"A", "version":"{version}", "api_version":1 }}"#
        ))
        .expect_err("非法版本必须被拒绝");
        assert!(
            error.to_string().contains("版本") || error.to_string().contains("version"),
            "{version}: {error}"
        );
    }
}

#[test]
fn semantic_errors_are_rejected_by_validate() {
    let api_version: PluginManifest =
        serde_json::from_str(r#"{ "id":"a.b", "name":"A", "version":"1.0.0", "api_version":2 }"#)
            .expect("结构合法");
    assert!(matches!(
        api_version.validate().expect_err("api_version=2 必须被拒绝").as_slice(),
        [ValidationError::UnsupportedApiVersion { found, .. }] if found.get() == 2
    ));

    let locale: PluginManifest = serde_json::from_str(
        r#"{ "id":"a.b", "name":"A", "version":"1.0.0", "api_version":1, "locales":["fr"] }"#,
    )
    .expect("结构合法");
    assert_eq!(
        locale.validate().expect_err("fr 必须被拒绝"),
        [ValidationError::InvalidLocale {
            value: "fr".to_owned()
        }]
    );

    let duplicate_capability: PluginManifest = serde_json::from_str(
        r#"{ "id":"a.b", "name":"A", "version":"1.0.0", "api_version":1,
             "capabilities":["notes_read","notes_read"] }"#,
    )
    .expect("结构合法");
    assert_eq!(
        duplicate_capability
            .validate()
            .expect_err("重复能力必须被拒绝"),
        [ValidationError::DuplicateCapability {
            capability: Capability::NotesRead
        }]
    );
}

#[test]
fn duplicate_locales_compare_after_parsing() {
    let manifest: PluginManifest = serde_json::from_str(
        r#"
        {
          "id": "a.b",
          "name": "A",
          "version": "1.0.0",
          "api_version": 1,
          "locales": ["zh", "zh-CN"]
        }
        "#,
    )
    .expect("结构合法");
    let errors = manifest.validate().expect_err("zh 与 zh-CN 是同一语言");
    assert_eq!(
        errors,
        [ValidationError::DuplicateLocale {
            value: "zh-CN".to_owned()
        }]
    );
}
