//! sync-proto —— 同步/加密 v1 的纯类型与 wire 编解码。
//!
//! 契约见 `docs/adr/0006-sync-v1.md`：MNE1 196B header、CDE record、AAD/域
//! 分隔符、固定切片、ObjectDigest/cursor 类型、错误码与 limits。
//! 本 crate 不做 IO / crypto / Loro / SQLite / GPUI。

pub mod activation_t;
pub mod cde;
pub mod constants;
pub mod cursor;
pub mod envelope;
pub mod error;
pub mod limits;
pub mod records;
mod rotation_records;

pub use activation_t::{activation_t_signing_input, ActivationTV1, ACTIVATION_T_FIELD_COUNT};
pub use cde::CdeValue as CborValue;
pub use cde::{decode as decode_cde, encode as encode_cde, CdeValue, MAX_CDE_BYTES};
pub use constants::*;
pub use cursor::{
    Cursor, CursorMacVerifier, CURSOR_BASE64_LEN, CURSOR_MAC_INPUT_LEN, CURSOR_MAC_LEN,
    CURSOR_RAW_LEN, CURSOR_VERSION,
};
pub use envelope::EnvelopeHeader as Header;
pub use envelope::{aad, Envelope, EnvelopeHeader, AAD_DOMAIN};
pub use envelope::{Envelope as Mne1Envelope, EnvelopeHeader as Mne1Header};
pub use error::{ErrorBody, ErrorCode, ProtoError};
pub use limits::*;
pub use records::ObjectType as MessageType;
pub use records::{
    check_device_count, device_cert_signed_bytes, grant_signed_bytes,
    management_signing_input as management_request_signing_input, object_digest_input,
    refs_digest_input, Argon2idParams, AssetPrefix, AuthRealm, Challenge, ChallengeId,
    CheckpointCommit, CheckpointId, DeviceCertV1, DeviceId, Ed25519PublicKey, GrantAadV1,
    HttpMethod, Kdf, ManifestAsset, ManifestDoc, ManifestV1, MessageId, ObjectCommit, ObjectId,
    ObjectRefV1, ObjectType, RelayInstanceId, Rights, RootSecretV1, Sha256, SlotDescriptorV1,
    SlotKind, SnapshotV1, UpdateV1, VaultId, X25519PublicKey,
};
pub use rotation_records::{
    grant_package_digest, member_cert_digest, slot_set_digest, AbortBodyV1, ActivateBodyV1,
    ActivationCommitV1, ApproveBodyV1, BeginBodyV1, ControlEvent, ControlKind, DeviceApproveBodyV1,
    EpochActivated, GrantStageBodyV1, GrantsBodyV1, InviteCreateBodyV1, InvitesBodyV1,
    MemberCertDigestInputV1, NewManagerV1, RevocationsAdded, RotationAbortBodyV1, RotationAborted,
    RotationActivateBodyV1, RotationBeginBodyV1, RotationRestarted, SignedControlRecord,
    SlotActivateBodyV1, SlotSetDigestInputV1, SlotsActivateBodyV1,
};
