-- migration v2：S7 同步桥（ADR 0006 §5 本地迁移）。
--
-- v1 的 sync_outbox 只有 message_id/batch_id/doc_id/seq/key_epoch/ciphertext；
-- 这里原地加列（旧行 object_id 可为 NULL、state 默认 pending），并新增
-- parts/inbox 两张 STRICT 表。所有 DDL 与 user_version 在同一个迁移事务里。
ALTER TABLE sync_outbox ADD COLUMN object_id BLOB
  CHECK(object_id IS NULL OR length(object_id)=32);
ALTER TABLE sync_outbox ADD COLUMN state TEXT NOT NULL DEFAULT 'pending'
  CHECK(state IN('pending','encrypted','sent','acked'));
ALTER TABLE sync_outbox ADD COLUMN cipher_digest BLOB
  CHECK(cipher_digest IS NULL OR length(cipher_digest)=32);

CREATE INDEX sync_outbox_state ON sync_outbox(state);
CREATE INDEX sync_outbox_doc_seq ON sync_outbox(doc_id,seq);

-- 加密后的 MNE1 parts；主键锁定 (message, epoch, part_no)。
-- digest 是该 part envelope 的 SHA256（由 sync 计算后写入，storage 不校验哈希）。
CREATE TABLE sync_outbox_parts(
  message_id BLOB NOT NULL,
  epoch INTEGER NOT NULL CHECK(epoch>=0),
  part_no INTEGER NOT NULL CHECK(part_no>=0),
  envelope BLOB NOT NULL,
  digest BLOB NOT NULL CHECK(length(digest)=32),
  PRIMARY KEY(message_id,epoch,part_no)
) STRICT;

-- 入站已验签/已解密对象；依赖未齐也先持久化。
CREATE TABLE sync_inbox(
  log_id BLOB NOT NULL CHECK(length(log_id)=16),
  relay_seq INTEGER NOT NULL CHECK(relay_seq>=0),
  object_id BLOB NOT NULL CHECK(length(object_id)=32),
  epoch INTEGER NOT NULL CHECK(epoch>=0),
  state TEXT NOT NULL CHECK(state IN('received','verified','integrated','quarantine')),
  verified_payload BLOB,
  PRIMARY KEY(log_id,relay_seq)
) STRICT;
CREATE INDEX sync_inbox_state ON sync_inbox(log_id,state,relay_seq);
