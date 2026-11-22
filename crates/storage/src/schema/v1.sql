-- migration v1：ADR 0001 §7 附录 DDL 全量建表。
--
-- 逐字对应 docs/adr/0001-storage-format.md 的 §7 DDL 草图，不增删业务字段、不改权威归属。
-- §7 顶部那四条 PRAGMA（journal_mode / synchronous / foreign_keys / busy_timeout）
-- 由连接建立流程负责（crate::connection），因此这里只包含建表与索引语句。
-- 注意：本文件里的表全部是 SQLite STRICT 表，要求 SQLite >= 3.37。

CREATE TABLE meta(key TEXT PRIMARY KEY, value BLOB NOT NULL) STRICT;

CREATE TABLE loro_docs(
  doc_id TEXT PRIMARY KEY, kind TEXT NOT NULL CHECK(kind IN('structure','body')),
  generation INTEGER NOT NULL DEFAULT 1, app_doc_schema INTEGER NOT NULL,
  codec_version TEXT NOT NULL, min_reader INTEGER NOT NULL,
  snapshot BLOB, snapshot_seq INTEGER NOT NULL DEFAULT 0, snapshot_frontier BLOB,
  head_seq INTEGER NOT NULL DEFAULT 0, head_frontier BLOB, head_vv BLOB,
  CHECK(head_seq>=snapshot_seq)
) STRICT;

CREATE TABLE loro_updates(
  doc_id TEXT NOT NULL REFERENCES loro_docs(doc_id), seq INTEGER NOT NULL,
  update_id TEXT NOT NULL UNIQUE, batch_id TEXT NOT NULL,
  codec_version TEXT NOT NULL, payload BLOB NOT NULL,
  end_frontier BLOB NOT NULL, end_vv BLOB NOT NULL,
  PRIMARY KEY(doc_id,seq)
) STRICT;

CREATE TABLE notes(
  note_id TEXT PRIMARY KEY,
  body_doc_id TEXT NOT NULL UNIQUE REFERENCES loro_docs(doc_id),
  title TEXT NOT NULL, body_md TEXT,
  body_seq INTEGER NOT NULL DEFAULT 0, body_frontier BLOB,
  meta_seq INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  deleted INTEGER NOT NULL CHECK(deleted IN(0,1)),
  is_favorite INTEGER NOT NULL CHECK(is_favorite IN(0,1)),
  is_pinned INTEGER NOT NULL CHECK(is_pinned IN(0,1))
) STRICT;
CREATE INDEX notes_recent ON notes(updated_at DESC,note_id) WHERE deleted=0;
CREATE INDEX notes_flags ON notes(is_favorite,is_pinned,updated_at DESC) WHERE deleted=0;

CREATE TABLE tree(
  node_id TEXT PRIMARY KEY, parent_id TEXT REFERENCES tree(node_id) DEFERRABLE INITIALLY DEFERRED,
  note_id TEXT UNIQUE REFERENCES notes(note_id), title TEXT NOT NULL,
  sibling_rank INTEGER NOT NULL, deleted INTEGER NOT NULL CHECK(deleted IN(0,1)),
  meta_seq INTEGER NOT NULL
) STRICT;
CREATE INDEX tree_parent_order ON tree(parent_id,sibling_rank,node_id);

CREATE TABLE tags(tag_id TEXT PRIMARY KEY,name TEXT NOT NULL,normalized_name TEXT NOT NULL) STRICT;
CREATE INDEX tags_name ON tags(normalized_name);

CREATE TABLE note_tags(
  note_id TEXT NOT NULL REFERENCES notes(note_id),tag_id TEXT NOT NULL REFERENCES tags(tag_id),
  PRIMARY KEY(note_id,tag_id)
) STRICT;
CREATE INDEX note_tags_reverse ON note_tags(tag_id,note_id);

CREATE TABLE chunks(
  chunk_id INTEGER PRIMARY KEY,note_id TEXT NOT NULL REFERENCES notes(note_id),ordinal INTEGER NOT NULL,
  start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,body_seq INTEGER NOT NULL,
  parse_version INTEGER NOT NULL,plain_text TEXT NOT NULL,
  UNIQUE(note_id,ordinal),CHECK(end_byte>=start_byte)
) STRICT;
CREATE INDEX chunks_note_seq ON chunks(note_id,body_seq);

CREATE TABLE links(
  from_note_id TEXT NOT NULL REFERENCES notes(note_id),start_byte INTEGER NOT NULL,
  end_byte INTEGER NOT NULL,to_note_id TEXT,target TEXT NOT NULL,kind TEXT NOT NULL,body_seq INTEGER NOT NULL,
  PRIMARY KEY(from_note_id,start_byte,target)
) STRICT;
CREATE INDEX links_backlinks ON links(to_note_id,from_note_id);

CREATE TABLE assets(
  asset_hash BLOB PRIMARY KEY CHECK(length(asset_hash)=32),byte_len INTEGER,media_type TEXT,data BLOB,
  orphan_since INTEGER,CHECK(data IS NULL OR (byte_len IS NOT NULL AND length(data)=byte_len))
) STRICT;

CREATE TABLE asset_refs(
  note_id TEXT NOT NULL REFERENCES notes(note_id),asset_hash BLOB NOT NULL REFERENCES assets(asset_hash),
  start_byte INTEGER NOT NULL,body_seq INTEGER NOT NULL,
  PRIMARY KEY(note_id,asset_hash,start_byte)
) STRICT;
CREATE INDEX asset_refs_reverse ON asset_refs(asset_hash,note_id);

CREATE TABLE versions(
  version_id INTEGER PRIMARY KEY,note_id TEXT NOT NULL REFERENCES notes(note_id),
  captured_at INTEGER NOT NULL,generation INTEGER NOT NULL,frontier BLOB NOT NULL,
  title TEXT NOT NULL,body_zstd BLOB NOT NULL,asset_hashes BLOB NOT NULL
) STRICT;
CREATE INDEX versions_retention ON versions(note_id,captured_at DESC);

CREATE TABLE projection_outbox(
  doc_id TEXT PRIMARY KEY REFERENCES loro_docs(doc_id),target_seq INTEGER NOT NULL
) STRICT;

CREATE TABLE index_outbox(
  note_id TEXT PRIMARY KEY REFERENCES notes(note_id),target_body_seq INTEGER NOT NULL,
  target_meta_seq INTEGER NOT NULL,index_version INTEGER NOT NULL,
  action TEXT NOT NULL CHECK(action IN('upsert','delete'))
) STRICT;

CREATE TABLE sync_outbox(
  message_id TEXT PRIMARY KEY,batch_id TEXT NOT NULL,doc_id TEXT NOT NULL,seq INTEGER NOT NULL,
  key_epoch INTEGER NOT NULL,ciphertext BLOB,
  FOREIGN KEY(doc_id,seq) REFERENCES loro_updates(doc_id,seq)
) STRICT;
CREATE INDEX sync_batches ON sync_outbox(batch_id);
