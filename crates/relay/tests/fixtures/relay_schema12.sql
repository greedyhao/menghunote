-- relay schema 12 全量 DDL（冻结 fixture；由 schema 12 二进制的 sqlite_master 导出）。
-- 用途：12 → 13 迁移测试的起点（tests/migration_v13.rs）。不要随当前 schema 修改。

CREATE TABLE relay_meta(
  key   TEXT PRIMARY KEY,
  value BLOB NOT NULL
) STRICT;
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
  created_at        INTEGER NOT NULL,
  authz_version     INTEGER NOT NULL DEFAULT 1 CHECK(authz_version IN (1, 2)),
  authz_rev         INTEGER NOT NULL DEFAULT 0 CHECK(authz_rev >= 0),
  authority_rev     INTEGER NOT NULL DEFAULT 0 CHECK(authority_rev >= 0),
  recovery_pub      BLOB CHECK(recovery_pub IS NULL OR length(recovery_pub) = 32),
  policy_bytes      BLOB CHECK(policy_bytes IS NULL OR length(policy_bytes) > 0),
  upgrade_control_rev INTEGER CHECK(upgrade_control_rev IS NULL OR upgrade_control_rev >= 1),
  authz_state       TEXT NOT NULL DEFAULT 'legacy'
                    CHECK(authz_state IN ('legacy', 'active', 'recovery_only')),
  CHECK(
    (authz_state = 'legacy' AND authz_version = 1 AND authz_rev = 0 AND authority_rev = 0
     AND recovery_pub IS NULL AND policy_bytes IS NULL AND upgrade_control_rev IS NULL)
    OR (authz_state = 'active' AND authz_version = 2 AND authz_rev >= 1 AND authority_rev >= 1
        AND recovery_pub IS NOT NULL AND policy_bytes IS NOT NULL
        AND upgrade_control_rev IS NOT NULL)
    OR (authz_state = 'recovery_only' AND authz_version = 2 AND authz_rev >= 1
        AND authority_rev >= 1 AND recovery_pub IS NOT NULL)
  ),
  FOREIGN KEY(vault_id, upgrade_control_rev) REFERENCES control_commits(vault_id, rev)
) STRICT;
CREATE TABLE create_tickets(
  token_hash  BLOB PRIMARY KEY CHECK(length(token_hash) = 32),
  vault_id    BLOB NOT NULL CHECK(length(vault_id) = 16),
  expires_at  INTEGER NOT NULL,
  quota       INTEGER NOT NULL,
  consumed_at INTEGER
) STRICT;
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
CREATE TABLE control_commits(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  rev          INTEGER NOT NULL CHECK(rev >= 1),
  prev_hash    BLOB NOT NULL CHECK(length(prev_hash) = 32),
  record_hash  BLOB NOT NULL CHECK(length(record_hash) = 32),
  signed_record BLOB NOT NULL,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(vault_id, rev)
) STRICT;
CREATE TABLE epochs(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch        INTEGER NOT NULL CHECK(epoch > 0),
  state        TEXT NOT NULL CHECK(state IN('staged','active','retired','aborted')),
  revoked_once INTEGER NOT NULL DEFAULT 0 CHECK(revoked_once IN (0, 1)),
  created_at   INTEGER NOT NULL,
  retire_at    INTEGER,
  PRIMARY KEY(vault_id, epoch)
) STRICT;
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
  approval_control_rev  INTEGER CHECK(approval_control_rev IS NULL OR approval_control_rev >= 1),
  issued_authz_rev      INTEGER CHECK(issued_authz_rev IS NULL OR issued_authz_rev >= 0),
  PRIMARY KEY(vault_id, device_id),
  FOREIGN KEY(vault_id, approval_control_rev) REFERENCES control_commits(vault_id, rev)
) STRICT;
CREATE TABLE device_credentials(
  vault_id             BLOB NOT NULL CHECK(length(vault_id) = 16),
  device_id            BLOB NOT NULL CHECK(length(device_id) = 16),
  serial               INTEGER NOT NULL CHECK(serial >= 1),
  authz_version        INTEGER NOT NULL CHECK(authz_version IN (1, 2)),
  sign_pk              BLOB NOT NULL CHECK(length(sign_pk) = 32),
  hpke_pk              BLOB NOT NULL CHECK(length(hpke_pk) = 32),
  certificate          BLOB NOT NULL,
  certificate_signature BLOB,
  issued_authz_rev     INTEGER CHECK(issued_authz_rev IS NULL OR issued_authz_rev >= 0),
  approval_control_rev INTEGER CHECK(approval_control_rev IS NULL OR approval_control_rev >= 1),
  created_at           INTEGER NOT NULL,
  PRIMARY KEY(vault_id, device_id, serial),
  FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id),
  FOREIGN KEY(vault_id, approval_control_rev) REFERENCES control_commits(vault_id, rev)
) STRICT;
CREATE TABLE management_caps(
  vault_id             BLOB NOT NULL CHECK(length(vault_id) = 16),
  cap_id               BLOB NOT NULL CHECK(length(cap_id) = 16),
  canonical_bytes      BLOB NOT NULL CHECK(length(canonical_bytes) > 0),
  subject_device       BLOB NOT NULL CHECK(length(subject_device) = 16),
  subject_pk           BLOB NOT NULL CHECK(length(subject_pk) = 32),
  scopes               INTEGER NOT NULL CHECK(scopes BETWEEN 1 AND 63),
  not_before           INTEGER NOT NULL,
  expires_at           INTEGER NOT NULL CHECK(expires_at > not_before),
  issued_control_rev   INTEGER NOT NULL CHECK(issued_control_rev >= 1),
  issued_authz_rev     INTEGER NOT NULL CHECK(issued_authz_rev >= 1),
  issuer_authority_rev INTEGER NOT NULL CHECK(issuer_authority_rev >= 1),
  revoked_control_rev  INTEGER CHECK(revoked_control_rev IS NULL OR revoked_control_rev >= 1),
  delegable            INTEGER NOT NULL DEFAULT 0 CHECK(delegable = 0),
  created_at           INTEGER NOT NULL,
  PRIMARY KEY(vault_id, cap_id),
  FOREIGN KEY(vault_id, subject_device) REFERENCES devices(vault_id, device_id),
  FOREIGN KEY(vault_id, issued_control_rev) REFERENCES control_commits(vault_id, rev),
  FOREIGN KEY(vault_id, revoked_control_rev) REFERENCES control_commits(vault_id, rev),
  CHECK(revoked_control_rev IS NULL OR revoked_control_rev > issued_control_rev)
) STRICT;
CREATE TABLE challenges(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  challenge_id  BLOB NOT NULL CHECK(length(challenge_id) = 16),
  realm         INTEGER NOT NULL CHECK(realm IN (1, 2, 3, 4)),
  subject       BLOB NOT NULL CHECK(length(subject) = 16),
  nonce         BLOB NOT NULL CHECK(length(nonce) = 32),
  expires_at    INTEGER NOT NULL,
  used_at       INTEGER,
  authz_version INTEGER NOT NULL DEFAULT 1 CHECK(authz_version IN (1, 2)),
  authz_rev     INTEGER NOT NULL DEFAULT 0 CHECK(authz_rev >= 0),
  authority_rev INTEGER NOT NULL DEFAULT 0 CHECK(authority_rev >= 0),
  cap_id        BLOB CHECK(cap_id IS NULL OR length(cap_id) = 16),
  PRIMARY KEY(vault_id, challenge_id),
  FOREIGN KEY(vault_id, cap_id) REFERENCES management_caps(vault_id, cap_id),
  CHECK(realm <> 3 OR cap_id IS NOT NULL)
) STRICT;
CREATE TABLE tokens(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  token_hash    BLOB NOT NULL CHECK(length(token_hash) = 32),
  realm         INTEGER NOT NULL CHECK(realm IN (1, 2, 3, 4)),
  subject       BLOB NOT NULL CHECK(length(subject) = 16),
  auth_rev      INTEGER NOT NULL CHECK(auth_rev >= 1),
  expires_at    INTEGER NOT NULL,
  created_at    INTEGER NOT NULL,
  authz_version INTEGER NOT NULL DEFAULT 1 CHECK(authz_version IN (1, 2)),
  authz_rev     INTEGER NOT NULL DEFAULT 0 CHECK(authz_rev >= 0),
  authority_rev INTEGER NOT NULL DEFAULT 0 CHECK(authority_rev >= 0),
  cap_id        BLOB CHECK(cap_id IS NULL OR length(cap_id) = 16),
  PRIMARY KEY(vault_id, token_hash),
  FOREIGN KEY(vault_id, cap_id) REFERENCES management_caps(vault_id, cap_id),
  CHECK(realm <> 3 OR cap_id IS NOT NULL)
) STRICT;
CREATE TABLE invites(
  vault_id       BLOB NOT NULL CHECK(length(vault_id) = 16),
  invite_id      BLOB NOT NULL CHECK(length(invite_id) = 16),
  token_hash     BLOB NOT NULL CHECK(length(token_hash) = 32),
  expires_at     INTEGER NOT NULL,
  pending_device BLOB CHECK(pending_device IS NULL OR length(pending_device) = 16),
  used_at        INTEGER,
  issuer_cap_id  BLOB CHECK(issuer_cap_id IS NULL OR length(issuer_cap_id) = 16),
  PRIMARY KEY(vault_id, invite_id),
  UNIQUE(vault_id, token_hash),
  FOREIGN KEY(vault_id, issuer_cap_id) REFERENCES management_caps(vault_id, cap_id)
) STRICT;
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
CREATE TABLE usage(
  vault_id          BLOB NOT NULL CHECK(length(vault_id) = 16),
  normal_bytes      INTEGER NOT NULL CHECK(normal_bytes >= 0),
  maintenance_bytes INTEGER NOT NULL CHECK(maintenance_bytes >= 0),
  PRIMARY KEY(vault_id)
) STRICT;
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
CREATE TABLE object_parts(
  vault_id  BLOB NOT NULL CHECK(length(vault_id) = 16),
  object_id BLOB NOT NULL CHECK(length(object_id) = 32),
  part_no   INTEGER NOT NULL CHECK(part_no >= 0),
  envelope  BLOB NOT NULL CHECK(length(envelope) BETWEEN 212 AND 4194304),
  digest    BLOB NOT NULL CHECK(length(digest) = 32),
  PRIMARY KEY(vault_id, object_id, part_no),
  FOREIGN KEY(vault_id, object_id) REFERENCES objects(vault_id, object_id)
) STRICT;
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
CREATE TABLE checkpoint_refs(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  checkpoint_id BLOB NOT NULL CHECK(length(checkpoint_id) = 16),
  object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
  role          TEXT NOT NULL CHECK(role IN('data','manifest')),
  PRIMARY KEY(vault_id, checkpoint_id, object_id),
  FOREIGN KEY(vault_id, checkpoint_id) REFERENCES checkpoints(vault_id, checkpoint_id)
) STRICT;
CREATE TABLE asset_objects(
  vault_id      BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch         INTEGER NOT NULL CHECK(epoch > 0),
  object_id     BLOB NOT NULL CHECK(length(object_id) = 32),
  state         TEXT NOT NULL CHECK(state IN('active','retired')),
  finalized_seq INTEGER NOT NULL CHECK(finalized_seq >= 0),
  rotation_id   BLOB CHECK(rotation_id IS NULL OR length(rotation_id) = 16),
  PRIMARY KEY(vault_id, epoch, object_id)
) STRICT;
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
  approval_control_rev INTEGER CHECK(approval_control_rev IS NULL OR approval_control_rev >= 1),
  issued_authz_rev     INTEGER CHECK(issued_authz_rev IS NULL OR issued_authz_rev >= 0),
  PRIMARY KEY(vault_id, epoch, device_id),
  FOREIGN KEY(vault_id, device_id) REFERENCES devices(vault_id, device_id),
  FOREIGN KEY(vault_id, approval_control_rev) REFERENCES control_commits(vault_id, rev)
) STRICT;
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
CREATE TABLE dedup_v2(
  vault_id     BLOB NOT NULL CHECK(length(vault_id) = 16),
  request_id   BLOB NOT NULL CHECK(length(request_id) = 16),
  request_hash BLOB NOT NULL CHECK(length(request_hash) = 32),
  control_rev  INTEGER CHECK(control_rev IS NULL OR control_rev >= 1),
  accepted_seq INTEGER,
  response     BLOB NOT NULL,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY(vault_id, request_id),
  FOREIGN KEY(vault_id, control_rev) REFERENCES control_commits(vault_id, rev)
) STRICT;
CREATE TABLE gc_plans(
  vault_id    BLOB NOT NULL CHECK(length(vault_id) = 16),
  epoch       INTEGER NOT NULL CHECK(epoch > 0),
  cut_seq     INTEGER NOT NULL CHECK(cut_seq >= 0),
  eligible_at INTEGER NOT NULL,
  proof       BLOB NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY(vault_id, epoch, cut_seq)
) STRICT;
CREATE UNIQUE INDEX vaults_legacy_default ON vaults(legacy_default) WHERE legacy_default = 1;
CREATE INDEX create_tickets_vault ON create_tickets(vault_id);
CREATE INDEX devices_status ON devices(vault_id, status);
CREATE INDEX devices_approval ON devices(vault_id, approval_control_rev);
CREATE INDEX challenges_expiry ON challenges(vault_id, expires_at);
CREATE INDEX challenges_cap ON challenges(vault_id, cap_id, expires_at);
CREATE INDEX tokens_expiry ON tokens(vault_id, expires_at);
CREATE INDEX tokens_cap ON tokens(vault_id, cap_id, expires_at);
CREATE INDEX management_caps_subject
  ON management_caps(vault_id, subject_device, expires_at);
CREATE INDEX management_caps_revoked
  ON management_caps(vault_id, revoked_control_rev);
CREATE INDEX device_credentials_approval
  ON device_credentials(vault_id, approval_control_rev);
CREATE INDEX invites_issuer_cap ON invites(vault_id, issuer_cap_id);
CREATE INDEX reservations_state ON maintenance_reservations(vault_id, state, expires_at);
CREATE INDEX objects_state ON objects(vault_id, state, touched_at);
CREATE INDEX objects_epoch ON objects(vault_id, epoch, type);
CREATE INDEX log_epoch ON log(vault_id, epoch);
CREATE INDEX checkpoints_state ON checkpoints(vault_id, state, epoch);
CREATE INDEX checkpoints_published ON checkpoints(vault_id, published_at);
CREATE INDEX checkpoint_refs_object ON checkpoint_refs(vault_id, object_id);
CREATE INDEX asset_objects_epoch ON asset_objects(vault_id, epoch, state, finalized_seq);
CREATE UNIQUE INDEX active_slot_kind ON key_slots(vault_id, kind) WHERE state = 'active';
CREATE INDEX rotations_state ON rotations(vault_id, state);
CREATE INDEX leases_expiry ON bootstrap_leases(vault_id, expires_at);
CREATE INDEX device_grants_approval
  ON device_grants(vault_id, approval_control_rev);
