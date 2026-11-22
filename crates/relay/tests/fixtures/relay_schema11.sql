-- relay schema **11** 的冻结 DDL（ADR 0014 S2b 的迁移产物；ADR 0015 S4a2 的迁移起点）。
--
-- 来源：从 `92fd6ce`（schema 11 二进制）`Db::open` 一个全新文件库后，
-- 按 `sqlite_master` 的建表顺序导出全部 `CREATE TABLE`/`CREATE INDEX` 语句
-- （不含 `sqlite_autoindex_*`）。**不要手改**：它是历史形状的对照物，
-- 只在 `tests/migration_v12.rs` 里用来构造"真实 schema 11 库"。
-- 新增列/表只能出现在 schema 12 的迁移里，绝不能回填到本文件。
-- table relay_meta
CREATE TABLE relay_meta(
  key   TEXT PRIMARY KEY,
  value BLOB NOT NULL
) STRICT;
-- table vaults
CREATE TABLE vaults(
  vault_id          BLOB PRIMARY KEY CHECK(length(vault_id) = 16),
  instance_id       BLOB NOT NULL CHECK(length(instance_id) = 16),
  log_id            BLOB NOT NULL CHECK(length(log_id) = 16),
  log_incarnation   BLOB NOT NULL CHECK(length(log_incarnation) = 16),
  cursor_key        BLOB NOT NULL CHECK(length(cursor_key) = 32),
  state             TEXT NOT NULL CHECK(state IN('initializing','active','rotating')),
  active_epoch      INTEGER NOT NULL CHECK(active_epoch > 0),
  next_epoch        INTEGER NOT NULL CHECK(next_epoch > active_epoch),
  manager_rev       INTEGER NOT NULL CHECK(manager_rev >= 1),
  control_rev       INTEGER NOT NULL CHECK(control_rev >= 1),
  auth_rev          INTEGER NOT NULL CHECK(auth_rev >= 1),
  head_seq          INTEGER NOT NULL CHECK(head_seq >= 0),
  gc_floor          INTEGER NOT NULL CHECK(gc_floor >= 0 AND gc_floor <= head_seq),
  normal_limit      INTEGER NOT NULL CHECK(normal_limit > 0),
  maintenance_limit INTEGER NOT NULL CHECK(maintenance_limit >= normal_limit),
  legacy_default    INTEGER NOT NULL DEFAULT 0 CHECK(legacy_default IN (0, 1)),
  created_at        INTEGER NOT NULL
) STRICT;
-- table create_tickets
CREATE TABLE create_tickets(
  token_hash  BLOB PRIMARY KEY CHECK(length(token_hash) = 32),
  vault_id    BLOB NOT NULL CHECK(length(vault_id) = 16),
  expires_at  INTEGER NOT NULL,
  quota       INTEGER NOT NULL,
  consumed_at INTEGER
) STRICT;
-- table manager_keys
CREATE TABLE manager_keys(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  rev           INTEGER NOT NULL CHECK(rev >= 1),
  pubkey        BLOB NOT NULL CHECK(length(pubkey) = 32),
  transition    BLOB NOT NULL,
  old_rev       INTEGER NOT NULL DEFAULT 0 CHECK(old_rev >= 0),
  old_signature BLOB,
  new_signature BLOB,
  created_at    INTEGER NOT NULL,
  PRIMARY KEY(vault_id, rev)
) STRICT;
-- table control_commits
CREATE TABLE control_commits(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  rev          INTEGER NOT NULL CHECK(rev >= 1),
  prev_hash    BLOB NOT NULL CHECK(length(prev_hash) = 32),
  record_hash  BLOB NOT NULL CHECK(length(record_hash) = 32),
  signed_record BLOB NOT NULL,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(vault_id, rev)
) STRICT;
-- table epochs
CREATE TABLE epochs(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch        INTEGER NOT NULL CHECK(epoch > 0),
  state        TEXT NOT NULL CHECK(state IN('staged','active','retired','aborted')),
  revoked_once INTEGER NOT NULL DEFAULT 0 CHECK(revoked_once IN (0, 1)),
  created_at   INTEGER NOT NULL,
  retire_at    INTEGER,
  PRIMARY KEY(vault_id, epoch)
) STRICT;
-- table devices
CREATE TABLE devices(
  vault_id              BLOB NOT NULL CHECK(length(vault_id) = 16),
  device_id             BLOB NOT NULL CHECK(length(device_id) = 16),
  sign_pk               BLOB NOT NULL CHECK(length(sign_pk) = 32),
  hpke_pk               BLOB NOT NULL CHECK(length(hpke_pk) = 32),
  certificate           BLOB NOT NULL,
  certificate_signature BLOB,
  status                TEXT NOT NULL CHECK(status IN('pending','active','revoked')),
  created_at            INTEGER NOT NULL,
  revoked_at            INTEGER,
  last_seen             INTEGER,
  PRIMARY KEY(vault_id, device_id)
) STRICT;
-- table challenges
CREATE TABLE challenges(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  challenge_id BLOB NOT NULL CHECK(length(challenge_id) = 16),
  realm        TEXT NOT NULL CHECK(realm IN('device','management')),
  subject      BLOB NOT NULL CHECK(length(subject) = 16),
  nonce        BLOB NOT NULL CHECK(length(nonce) = 32),
  expires_at   INTEGER NOT NULL,
  used_at      INTEGER,
  PRIMARY KEY(vault_id, challenge_id)
) STRICT;
-- table tokens
CREATE TABLE tokens(
  vault_id   BLOB NOT NULL CHECK(length(vault_id) = 16),
  token_hash BLOB NOT NULL CHECK(length(token_hash) = 32),
  realm      TEXT NOT NULL CHECK(realm IN('device','management')),
  subject    BLOB NOT NULL CHECK(length(subject) = 16),
  auth_rev   INTEGER NOT NULL CHECK(auth_rev >= 1),
  expires_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY(vault_id, token_hash)
) STRICT;
-- table invites
CREATE TABLE invites(
  vault_id       BLOB NOT NULL CHECK(length(vault_id) = 16),
  invite_id      BLOB NOT NULL CHECK(length(invite_id) = 16),
  token_hash     BLOB NOT NULL CHECK(length(token_hash) = 32),
  expires_at     INTEGER NOT NULL,
  pending_device BLOB CHECK(pending_device IS NULL OR length(pending_device) = 16),
  used_at        INTEGER,
  PRIMARY KEY(vault_id, invite_id),
  UNIQUE(vault_id, token_hash)
) STRICT;
-- table maintenance_reservations
CREATE TABLE maintenance_reservations(
  vault_id       BLOB NOT NULL CHECK(length(vault_id) = 16),
  reservation_id BLOB NOT NULL CHECK(length(reservation_id) = 16),
  purpose        TEXT NOT NULL CHECK(purpose IN('checkpoint','rotation')),
  resource_id    BLOB NOT NULL,
  owner_device   BLOB NOT NULL CHECK(length(owner_device) = 16),
  reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes >= 0),
  consumed_bytes INTEGER NOT NULL CHECK(consumed_bytes >= 0 AND consumed_bytes <= reserved_bytes),
  state          TEXT NOT NULL CHECK(state IN('active','complete','expired')),
  expires_at     INTEGER NOT NULL,
  PRIMARY KEY(vault_id, reservation_id),
  UNIQUE(vault_id, resource_id),
  FOREIGN KEY(vault_id, owner_device) REFERENCES devices(vault_id, device_id)
) STRICT;
-- table usage
CREATE TABLE usage(
  vault_id          BLOB NOT NULL CHECK(length(vault_id) = 16),
  normal_bytes      INTEGER NOT NULL CHECK(normal_bytes >= 0),
  maintenance_bytes INTEGER NOT NULL CHECK(maintenance_bytes >= 0),
  PRIMARY KEY(vault_id)
) STRICT;
-- table objects
CREATE TABLE objects(
  vault_id         BLOB NOT NULL CHECK(length(vault_id) = 16),
  object_id        BLOB NOT NULL CHECK(length(object_id) = 32),
  message_id       BLOB NOT NULL CHECK(length(message_id) = 16),
  producer         BLOB NOT NULL CHECK(length(producer) = 16),
  epoch            INTEGER NOT NULL CHECK(epoch > 0),
  type             INTEGER NOT NULL CHECK(type BETWEEN 1 AND 5),
  generation       INTEGER NOT NULL CHECK(generation >= 0),
  sender_seq       INTEGER NOT NULL CHECK(sender_seq >= 0),
  part_count       INTEGER NOT NULL CHECK(part_count >= 1),
  plain_len        INTEGER NOT NULL CHECK(plain_len >= 1),
  state            TEXT NOT NULL CHECK(state IN('staging','complete')),
  budget_class     TEXT NOT NULL CHECK(budget_class IN('normal','maintenance','retired')),
  digest           BLOB CHECK(digest IS NULL OR length(digest) = 32),
  signed_commit    BLOB,
  commit_signature BLOB,
  reservation_id   BLOB CHECK(reservation_id IS NULL OR length(reservation_id) = 16),
  created_at       INTEGER NOT NULL,
  touched_at       INTEGER NOT NULL,
  completed_at     INTEGER,
  PRIMARY KEY(vault_id, object_id),
  UNIQUE(vault_id, epoch, message_id),
  FOREIGN KEY(vault_id, reservation_id) REFERENCES maintenance_reservations(vault_id, reservation_id)
) STRICT;
-- table object_parts
CREATE TABLE object_parts(
  vault_id  BLOB NOT NULL CHECK(length(vault_id) = 16),
  object_id BLOB NOT NULL CHECK(length(object_id) = 32),
  part_no   INTEGER NOT NULL CHECK(part_no >= 0),
  envelope  BLOB NOT NULL CHECK(length(envelope) BETWEEN 212 AND 4194304),
  digest    BLOB NOT NULL CHECK(length(digest) = 32),
  PRIMARY KEY(vault_id, object_id, part_no),
  FOREIGN KEY(vault_id, object_id) REFERENCES objects(vault_id, object_id)
) STRICT;
-- table log
CREATE TABLE log(
  vault_id   BLOB NOT NULL CHECK(length(vault_id) = 16),
  relay_seq  INTEGER NOT NULL CHECK(relay_seq > 0),
  kind       TEXT NOT NULL CHECK(kind IN('update','asset','checkpoint','control')),
  object_id  BLOB CHECK(object_id IS NULL OR length(object_id) = 32),
  epoch      INTEGER NOT NULL CHECK(epoch > 0),
  event      BLOB NOT NULL,
  signature  BLOB,
  created_at INTEGER NOT NULL,
  PRIMARY KEY(vault_id, relay_seq)
) STRICT;
-- table checkpoints
CREATE TABLE checkpoints(
  vault_id           BLOB NOT NULL CHECK(length(vault_id) = 16),
  checkpoint_id      BLOB NOT NULL CHECK(length(checkpoint_id) = 16),
  epoch              INTEGER NOT NULL CHECK(epoch > 0),
  covered_seq        INTEGER NOT NULL CHECK(covered_seq >= 0),
  manifest_oid       BLOB CHECK(manifest_oid IS NULL OR length(manifest_oid) = 32),
  refs_digest        BLOB CHECK(refs_digest IS NULL OR length(refs_digest) = 32),
  signed_commit      BLOB,
  commit_signature   BLOB,
  state              TEXT NOT NULL CHECK(state IN('staging','candidate','published','aborted','pruned')),
  role               TEXT CHECK(role IS NULL OR role IN('primary','fallback')),
  validation_version INTEGER NOT NULL CHECK(validation_version >= 1),
  health             TEXT NOT NULL CHECK(health IN('unknown','healthy','unhealthy')),
  created_at         INTEGER NOT NULL,
  published_at       INTEGER,
  PRIMARY KEY(vault_id, checkpoint_id)
) STRICT;
-- table checkpoint_refs
CREATE TABLE checkpoint_refs(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  checkpoint_id BLOB NOT NULL CHECK(length(checkpoint_id) = 16),
  object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
  role          TEXT NOT NULL CHECK(role IN('data','manifest')),
  PRIMARY KEY(vault_id, checkpoint_id, object_id),
  FOREIGN KEY(vault_id, checkpoint_id) REFERENCES checkpoints(vault_id, checkpoint_id)
) STRICT;
-- table asset_objects
CREATE TABLE asset_objects(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch         INTEGER NOT NULL CHECK(epoch > 0),
  object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
  state         TEXT NOT NULL CHECK(state IN('active','retired')),
  finalized_seq INTEGER NOT NULL CHECK(finalized_seq >= 0),
  rotation_id   BLOB CHECK(rotation_id IS NULL OR length(rotation_id) = 16),
  PRIMARY KEY(vault_id, epoch, object_id)
) STRICT;
-- table key_slots
CREATE TABLE key_slots(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  slot_id      BLOB NOT NULL CHECK(length(slot_id) = 32),
  epoch        INTEGER NOT NULL CHECK(epoch > 0),
  kind         TEXT NOT NULL CHECK(kind IN('password','recovery')),
  descriptor   BLOB NOT NULL,
  state        TEXT NOT NULL CHECK(state IN('staging','active','retiring')),
  slot_set_rev INTEGER NOT NULL CHECK(slot_set_rev >= 0),
  retire_at    INTEGER,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(vault_id, slot_id),
  FOREIGN KEY(vault_id, slot_id) REFERENCES objects(vault_id, object_id)
) STRICT;
-- table device_grants
CREATE TABLE device_grants(
  vault_id        BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch           INTEGER NOT NULL CHECK(epoch > 0),
  device_id       BLOB NOT NULL CHECK(length(device_id) = 16),
  manager_rev     INTEGER NOT NULL CHECK(manager_rev >= 1),
  hpke_enc        BLOB NOT NULL CHECK(length(hpke_enc) = 32),
  hpke_ciphertext BLOB NOT NULL,
  signed_header   BLOB NOT NULL,
  signature       BLOB,
  grant_id        BLOB CHECK(grant_id IS NULL OR length(grant_id) = 16),
  activation_ref  BLOB CHECK(activation_ref IS NULL OR length(activation_ref) = 16),
  PRIMARY KEY(vault_id, epoch, device_id),
  FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id)
) STRICT;
-- table rotations
CREATE TABLE rotations(
  vault_id       BLOB NOT NULL CHECK(length(vault_id) = 16),
  rotation_id    BLOB NOT NULL CHECK(length(rotation_id) = 16),
  from_epoch     INTEGER NOT NULL CHECK(from_epoch > 0),
  to_epoch       INTEGER NOT NULL CHECK(to_epoch > from_epoch),
  freeze_seq     INTEGER NOT NULL CHECK(freeze_seq >= 0),
  member_set_hash BLOB CHECK(member_set_hash IS NULL OR length(member_set_hash) = 32),
  asset_set_digest BLOB CHECK(asset_set_digest IS NULL OR length(asset_set_digest) = 32),
  revoked_count  INTEGER NOT NULL CHECK(revoked_count >= 0),
  state          TEXT NOT NULL CHECK(state IN('frozen','staging','activated','aborted')),
  primary_checkpoint BLOB CHECK(primary_checkpoint IS NULL OR length(primary_checkpoint) = 16),
  fallback_checkpoint BLOB CHECK(fallback_checkpoint IS NULL OR length(fallback_checkpoint) = 16),
  activation_commit  BLOB CHECK(activation_commit IS NULL OR length(activation_commit) = 32),
  activated_at       INTEGER,
  activated_control_rev INTEGER CHECK(activated_control_rev IS NULL OR activated_control_rev >= 0),
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  PRIMARY KEY(vault_id, rotation_id)
) STRICT;
-- table bootstrap_leases
CREATE TABLE bootstrap_leases(
  vault_id           BLOB NOT NULL CHECK(length(vault_id) = 16),
  lease_id           BLOB NOT NULL CHECK(length(lease_id) = 16),
  device_id          BLOB NOT NULL CHECK(length(device_id) = 16),
  primary_checkpoint BLOB NOT NULL CHECK(length(primary_checkpoint) = 16),
  fallback_checkpoint BLOB NOT NULL CHECK(length(fallback_checkpoint) = 16),
  floor_seq          INTEGER NOT NULL CHECK(floor_seq >= 0),
  created_at         INTEGER NOT NULL,
  expires_at         INTEGER NOT NULL,
  PRIMARY KEY(vault_id, lease_id),
  FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id)
) STRICT;
-- table device_acks
CREATE TABLE device_acks(
  vault_id       BLOB NOT NULL CHECK(length(vault_id) = 16),
  device_id      BLOB NOT NULL CHECK(length(device_id) = 16),
  received_seq   INTEGER NOT NULL CHECK(received_seq >= 0),
  integrated_seq INTEGER NOT NULL CHECK(integrated_seq >= 0),
  epoch          INTEGER NOT NULL CHECK(epoch > 0),
  updated_at     INTEGER NOT NULL,
  PRIMARY KEY(vault_id, device_id),
  FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id),
  CHECK(received_seq >= integrated_seq)
) STRICT;
-- table dedup
CREATE TABLE dedup(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  scope        TEXT NOT NULL,
  id           BLOB NOT NULL,
  request_hash BLOB NOT NULL CHECK(length(request_hash) = 32),
  accepted_seq INTEGER,
  response     BLOB NOT NULL,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(vault_id, scope, id)
) STRICT;
-- table gc_plans
CREATE TABLE gc_plans(
  vault_id    BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch       INTEGER NOT NULL CHECK(epoch > 0),
  cut_seq     INTEGER NOT NULL CHECK(cut_seq >= 0),
  eligible_at INTEGER NOT NULL,
  proof       BLOB NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY(vault_id, epoch, cut_seq)
) STRICT;
-- index vaults_legacy_default
CREATE UNIQUE INDEX vaults_legacy_default ON vaults(legacy_default) WHERE legacy_default = 1;
-- index create_tickets_vault
CREATE INDEX create_tickets_vault ON create_tickets(vault_id);
-- index devices_status
CREATE INDEX devices_status ON devices(vault_id, status);
-- index challenges_expiry
CREATE INDEX challenges_expiry ON challenges(vault_id, expires_at);
-- index tokens_expiry
CREATE INDEX tokens_expiry ON tokens(vault_id, expires_at);
-- index reservations_state
CREATE INDEX reservations_state ON maintenance_reservations(vault_id, state, expires_at);
-- index objects_state
CREATE INDEX objects_state ON objects(vault_id, state, touched_at);
-- index objects_epoch
CREATE INDEX objects_epoch ON objects(vault_id, epoch, type);
-- index log_epoch
CREATE INDEX log_epoch ON log(vault_id, epoch);
-- index checkpoints_state
CREATE INDEX checkpoints_state ON checkpoints(vault_id, state, epoch);
-- index checkpoints_published
CREATE INDEX checkpoints_published ON checkpoints(vault_id, published_at);
-- index checkpoint_refs_object
CREATE INDEX checkpoint_refs_object ON checkpoint_refs(vault_id, object_id);
-- index asset_objects_epoch
CREATE INDEX asset_objects_epoch ON asset_objects(vault_id, epoch, state, finalized_seq);
-- index active_slot_kind
CREATE UNIQUE INDEX active_slot_kind ON key_slots(vault_id, kind) WHERE state = 'active';
-- index rotations_state
CREATE INDEX rotations_state ON rotations(vault_id, state);
-- index leases_expiry
CREATE INDEX leases_expiry ON bootstrap_leases(vault_id, expires_at);
