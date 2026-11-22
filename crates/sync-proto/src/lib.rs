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
pub mod management_cap_v2;
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
pub use management_cap_v2::{
    auth_signing_input_v2, canonical_submit_path, control_signing_input, pop_signing_input,
    recover_signing_input, set_digest_v2, upgrade_signing_input, ActivationV2, CapGrantPayloadV2,
    CapId, CapRevokePayloadV2, CapScope, ChallengeV2, ControlKindV2, DeviceApprovePayloadV2,
    DeviceCertV2, DeviceCredentialPayloadV2, DeviceRevokePayloadV2, DeviceRightsChangePayloadV2,
    EpochActivatePayloadV2, GrantAadV2, GrantIdV2, GrantPackageV2, InviteCancelPayloadV2,
    InviteCreatePayloadV2, ManagementCapV2, OperationV2, PolicyV2, RecoveryKeyChangePayloadV2,
    RecoveryResetPayloadV2, RequestIdV2, RotationAbortPayloadV2, RotationAddRevokesPayloadV2,
    RotationBeginPayloadV2, RotationIdV2, RotationRestartPayloadV2, SetDigestEntryV2, Sha256Fn,
    SignedControlV2, SignerRole, SignerV2, UpgradePolicyPayloadV2, ACTIVATION_V2_FIELD_COUNT,
    CAP_GRANT_PAYLOAD_V2_FIELD_COUNT, CAP_REVOKE_PAYLOAD_V2_FIELD_COUNT, CHALLENGE_V2_FIELD_COUNT,
    CHALLENGE_V2_REALM_DEVICE, CHALLENGE_V2_REALM_MANAGEMENT_DEVICE, CHALLENGE_V2_REALM_RECOVERY,
    DEVICE_CERT_V2_FIELD_COUNT, DEVICE_CREDENTIAL_PAYLOAD_V2_FIELD_COUNT,
    DEVICE_REVOKE_PAYLOAD_V2_FIELD_COUNT, EPOCH_ACTIVATE_PAYLOAD_V2_FIELD_COUNT,
    GRANT_AAD_V2_FIELD_COUNT, GRANT_PACKAGE_V2_FIELD_COUNT, INVITE_CANCEL_PAYLOAD_V2_FIELD_COUNT,
    INVITE_CREATE_PAYLOAD_V2_FIELD_COUNT, MANAGEMENT_CAP_V2_FIELD_COUNT, OPERATION_V2_FIELD_COUNT,
    POLICY_V2_FIELD_COUNT, RECOVERY_KEY_CHANGE_PAYLOAD_V2_FIELD_COUNT,
    RECOVERY_RESET_PAYLOAD_V2_FIELD_COUNT, ROTATION_ABORT_PAYLOAD_V2_FIELD_COUNT,
    ROTATION_ADD_REVOKES_PAYLOAD_V2_FIELD_COUNT, ROTATION_BEGIN_PAYLOAD_V2_FIELD_COUNT,
    ROTATION_RESTART_PAYLOAD_V2_FIELD_COUNT, SIGNED_CONTROL_V2_FIELD_COUNT, SIGNER_V2_FIELD_COUNT,
    UPGRADE_POLICY_PAYLOAD_V2_FIELD_COUNT, V2_WIRE_VERSION,
};
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
