-- migration v3：已验证基线分批应用 journal（ADR 0006 §28.2 阶段 I）。
--
-- 只记录应用进度与 cursor CAS 快照；不复制 Loro/资产数据。abort 只清本表，
-- 已导入的 loro_updates/assets 保留（宁可重放，不丢本地内容）。

CREATE TABLE baseline_apply(
  singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
  checkpoint_id BLOB NOT NULL CHECK(length(checkpoint_id) = 16),
  baseline_s INTEGER NOT NULL CHECK(baseline_s >= 0),
  source_cursor TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN('staging','completed')),
  expected_received_seq INTEGER NOT NULL CHECK(expected_received_seq >= 0),
  expected_integrated_seq INTEGER NOT NULL CHECK(expected_integrated_seq >= 0),
  expected_confirmed_highwater INTEGER,
  expected_relay_cursor BLOB,
  applied_docs INTEGER NOT NULL DEFAULT 0,
  applied_assets INTEGER NOT NULL DEFAULT 0,
  kept_local_edit INTEGER NOT NULL DEFAULT 0,
  chosen_cursor TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE baseline_apply_docs(
  doc_id TEXT PRIMARY KEY,
  snapshot_digest BLOB NOT NULL CHECK(length(snapshot_digest) = 32),
  baseline_vv BLOB NOT NULL,
  applied INTEGER NOT NULL DEFAULT 1 CHECK(applied IN(0, 1)),
  updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE baseline_apply_tombstones(
  doc_id TEXT PRIMARY KEY,
  action TEXT NOT NULL CHECK(action IN('deleted', 'kept_local_edit')),
  updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE baseline_apply_assets(
  object_id BLOB PRIMARY KEY CHECK(length(object_id) = 32),
  asset_hash BLOB NOT NULL CHECK(length(asset_hash) = 32),
  byte_len INTEGER NOT NULL CHECK(byte_len >= 0),
  applied INTEGER NOT NULL DEFAULT 1 CHECK(applied IN(0, 1)),
  updated_at INTEGER NOT NULL
) STRICT;
