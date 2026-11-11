//! `UiError`: what every command returns on failure.

use mesh_admin::ErrorCode;
use mesh_proto::RefuseReason;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum UiErrorCode {
    NotRunning,
    WrongState,
    InvalidArgument,
    NotFound,
    JoinRefused,
    LastHost,
    Busy,
    Internal,
    DaemonUnreachable,
    DaemonStarting,
    Cancelled,
    PickerCancelled,
    Io,
}

/// Why an invite was refused (the admin `RefuseReason`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
pub enum JoinRefusalDto {
    Expired,
    Used,
    Unknown,
    WrongMesh,
    Removed,
    InviterUnavailable,
}

impl From<RefuseReason> for JoinRefusalDto {
    fn from(r: RefuseReason) -> Self {
        match r {
            RefuseReason::Expired => JoinRefusalDto::Expired,
            RefuseReason::Used => JoinRefusalDto::Used,
            RefuseReason::Unknown => JoinRefusalDto::Unknown,
            RefuseReason::WrongMesh => JoinRefusalDto::WrongMesh,
            RefuseReason::Removed => JoinRefusalDto::Removed,
            RefuseReason::InviterUnavailable => JoinRefusalDto::InviterUnavailable,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS, thiserror::Error)]
#[ts(export)]
#[serde(rename_all = "camelCase")]
#[error("{message}")]
pub struct UiError {
    pub code: UiErrorCode,
    pub message: String,
    pub hint: Option<String>,
    pub reason: Option<JoinRefusalDto>,
}

/// English hint text for an error, written once here.
pub fn hint_for(code: UiErrorCode, reason: Option<JoinRefusalDto>) -> Option<&'static str> {
    match (code, reason) {
        (UiErrorCode::JoinRefused, Some(JoinRefusalDto::Expired)) => {
            Some("Ask the other device for a new invite.")
        }
        (UiErrorCode::JoinRefused, Some(JoinRefusalDto::Used)) => {
            Some("Each invite works once. Ask for a new one.")
        }
        (UiErrorCode::JoinRefused, Some(JoinRefusalDto::InviterUnavailable)) => {
            Some("Keep Meshnode open on the inviting device and try again.")
        }
        (UiErrorCode::LastHost, _) => Some("Add another host device before removing this one."),
        (UiErrorCode::DaemonUnreachable, _) => Some("Start Meshnode, or install it as a service."),
        (UiErrorCode::NotRunning, _) => Some("Start the node."),
        _ => None,
    }
}

impl UiError {
    pub fn new(code: UiErrorCode, message: impl Into<String>) -> UiError {
        UiError::with_reason(code, message, None)
    }

    pub fn with_reason(
        code: UiErrorCode,
        message: impl Into<String>,
        reason: Option<JoinRefusalDto>,
    ) -> UiError {
        UiError {
            code,
            message: message.into(),
            hint: hint_for(code, reason).map(str::to_owned),
            reason,
        }
    }

    pub fn internal(message: impl Into<String>) -> UiError {
        UiError::new(UiErrorCode::Internal, message)
    }

    pub fn invalid(message: impl Into<String>) -> UiError {
        UiError::new(UiErrorCode::InvalidArgument, message)
    }
}

/// The `Error { code, message }` of an admin response.
impl From<(ErrorCode, String)> for UiError {
    fn from((code, message): (ErrorCode, String)) -> UiError {
        let (code, reason) = match code {
            ErrorCode::NotRunning => (UiErrorCode::NotRunning, None),
            ErrorCode::WrongState => (UiErrorCode::WrongState, None),
            ErrorCode::InvalidArgument => (UiErrorCode::InvalidArgument, None),
            ErrorCode::NotFound => (UiErrorCode::NotFound, None),
            ErrorCode::JoinRefused(r) => (UiErrorCode::JoinRefused, Some(r.into())),
            ErrorCode::LastHost => (UiErrorCode::LastHost, None),
            ErrorCode::Busy => (UiErrorCode::Busy, None),
            ErrorCode::Internal => (UiErrorCode::Internal, None),
        };
        UiError::with_reason(code, message, reason)
    }
}
