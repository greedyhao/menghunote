-- migration v4：笔记本机编辑历史（ADR 0024）。
--
-- v1 的 versions 表只有 version_id/note_id/captured_at/generation/frontier/
-- title/body_zstd/asset_hashes，当前仓库没有写入方。本迁移只**原地加列**：
--
-- - source：版本来源（本机编辑 / 同步导入 / 已验证基线），默认 'local'；
-- - device_id：捕获设备（本机历史可空，跨设备时间线后置）；
-- - body_hash：正文 SHA-256（32B），用于「同一篇正文不重复写版本」。
--
-- 旧行保留：source 取默认值，device_id/body_hash 为 NULL（旧版本不参与去重）。
-- 所有 DDL 与 user_version 在同一个迁移事务里（见 migrate.rs）。

ALTER TABLE versions ADD COLUMN source TEXT NOT NULL DEFAULT 'local'
  CHECK(source IN('local','sync','checkpoint'));
ALTER TABLE versions ADD COLUMN device_id TEXT;
ALTER TABLE versions ADD COLUMN body_hash BLOB
  CHECK(body_hash IS NULL OR length(body_hash)=32);
