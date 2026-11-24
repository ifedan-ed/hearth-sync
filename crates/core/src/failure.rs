//! Errors classified once, in one place (docs/sync.md 3.10), and the
//! backoff of retries.
//!
//! Every failure of a request or a disk call becomes one of four answers:
//! retry soon, wait for a change (and list the item), plan again, or stop
//! the root. Code that meets an error asks here and acts on the answer; it
//! never decides by a status number of its own.

use crate::fs::FsError;
use crate::remote::{ErrorCode, RemoteError};
use crate::tree::ProblemKind;

/// What to do about a failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Network down, timeout, 5xx, 429: back off and try again; the
    /// request is safe to repeat (sync-api.md 8.5). `after_s` is Hearth's
    /// `Retry-After` when given.
    Retry {
        /// Hearth's `Retry-After`, in seconds.
        after_s: Option<u64>,
    },
    /// Locked, over quota, refused, infected, a name Hearth refuses: list
    /// the item and leave it until it, its lock, the quota or the rights
    /// change (or the slow timer).
    WaitForChange(ProblemKind, String),
    /// 412, 404, 409 and the like: Hearth or the disk is not what was
    /// assumed. Drop the operation, take in what the answer says, plan
    /// again.
    Replan,
    /// Signed out, refused client, root gone, disk full: stop the whole
    /// root and say why; touch nothing.
    Stop(StopReason),
}

/// Why a failure stops the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// 401 or the account disabled.
    SignedOut(String),
    /// 403 `client_refused`.
    ClientRefused(String),
    /// 400 `sync_api_*`.
    ApiVersion,
    /// Another `X-Hearth-Instance`.
    InstanceChanged(String),
    /// The sync root is not there.
    RootUnavailable,
    /// The disk is full.
    DiskFull,
}

/// The answer for a failed request.
pub fn classify_remote(e: &RemoteError) -> Failure {
    let api = match e {
        RemoteError::Lost(_) => return Failure::Retry { after_s: None },
        RemoteError::InstanceChanged(id) => {
            return Failure::Stop(StopReason::InstanceChanged(id.clone()));
        }
        RemoteError::Api(a) => a,
    };
    let msg = api.message.clone();
    match &api.code {
        ErrorCode::Throttled | ErrorCode::ServerError | ErrorCode::Unavailable => Failure::Retry {
            after_s: api.retry_after_s,
        },
        ErrorCode::Unauthorized | ErrorCode::DeviceRevoked | ErrorCode::AccountDisabled => {
            Failure::Stop(StopReason::SignedOut(msg))
        }
        ErrorCode::ClientRefused => Failure::Stop(StopReason::ClientRefused(msg)),
        ErrorCode::SyncApi => Failure::Stop(StopReason::ApiVersion),
        ErrorCode::Locked => {
            let who = api
                .details
                .lock
                .as_ref()
                .map(|l| format!("locked by {} ({})", l.by, l.kind))
                .unwrap_or_else(|| "locked".into());
            Failure::WaitForChange(ProblemKind::Locked, who)
        }
        ErrorCode::Quota => Failure::WaitForChange(ProblemKind::Quota, msg),
        ErrorCode::NoPermission => Failure::WaitForChange(ProblemKind::ReadOnly, msg),
        ErrorCode::Infected => Failure::WaitForChange(ProblemKind::Infected, msg),
        ErrorCode::NameRefused => Failure::WaitForChange(
            ProblemKind::NameRefused,
            api.details.reason.clone().unwrap_or(msg),
        ),
        ErrorCode::HashMismatch => Failure::WaitForChange(ProblemKind::HashMismatch, msg),
        ErrorCode::MountRoot | ErrorCode::CrossStorage | ErrorCode::Crypt => {
            Failure::WaitForChange(ProblemKind::Refused, format!("{:?}: {msg}", api.code))
        }
        ErrorCode::Precondition
        | ErrorCode::NotFound
        | ErrorCode::Exists
        | ErrorCode::IsFolder
        | ErrorCode::NotFolder
        | ErrorCode::IntoItself
        | ErrorCode::HashUnknown
        | ErrorCode::UploadGone
        | ErrorCode::UploadMismatch
        | ErrorCode::UploadIncomplete
        | ErrorCode::UploadFinished
        | ErrorCode::TooLarge
        | ErrorCode::Range
        | ErrorCode::CursorExpired
        | ErrorCode::PageInvalid => Failure::Replan,
        // A request this client got wrong: a bug. List it, never loop.
        ErrorCode::BadRequest
        | ErrorCode::DeviceRequired
        | ErrorCode::Forbidden
        | ErrorCode::LengthRequired
        | ErrorCode::Invalid
        | ErrorCode::Chunk
        | ErrorCode::PreconditionRequired => Failure::WaitForChange(
            ProblemKind::Refused,
            format!("refused as a faulty request ({:?}): {msg}", api.code),
        ),
        // An unknown code is acted on by its status class (1.8).
        ErrorCode::Unknown(code) => {
            if api.status >= 500 || api.status == 429 {
                Failure::Retry {
                    after_s: api.retry_after_s,
                }
            } else {
                Failure::WaitForChange(ProblemKind::Refused, format!("{code}: {msg}"))
            }
        }
    }
}

/// The answer for a failed disk call.
pub fn classify_fs(e: &FsError) -> Failure {
    match e {
        FsError::NotFound | FsError::Exists | FsError::NotAFolder | FsError::NotEmpty => {
            Failure::Replan
        }
        FsError::PermissionDenied => Failure::WaitForChange(
            ProblemKind::Refused,
            "the system refused access to this item".into(),
        ),
        FsError::NameNotHoldable => Failure::WaitForChange(
            ProblemKind::NameNotHoldable,
            "this device cannot hold the name".into(),
        ),
        FsError::InUse | FsError::Io(_) => Failure::Retry { after_s: None },
        FsError::NoSpace => Failure::Stop(StopReason::DiskFull),
        FsError::RootUnavailable => Failure::Stop(StopReason::RootUnavailable),
    }
}

/// The delay before the `attempt`-th retry (from 1): exponential from
/// `base_ms`, capped at `cap_ms`, with the upper half drawn at random so
/// many devices do not retry in step. `Retry-After` is a floor.
pub fn backoff_ms(
    attempt: u32,
    base_ms: u64,
    cap_ms: u64,
    random: u64,
    after_s: Option<u64>,
) -> u64 {
    let exp = base_ms.saturating_mul(1u64 << attempt.saturating_sub(1).min(30));
    let d = exp.min(cap_ms).max(1);
    let jittered = d / 2 + random % (d / 2 + 1);
    let floor = after_s.unwrap_or(0).saturating_mul(1000);
    jittered.max(floor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::{ApiError, Details};

    fn api(status: u16, code: &str) -> RemoteError {
        RemoteError::Api(ApiError {
            status,
            code: ErrorCode::parse(code, status),
            message: "m".into(),
            details: Details::default(),
            retry_after_s: Some(7),
        })
    }

    #[test]
    fn the_four_classes() {
        assert_eq!(
            classify_remote(&RemoteError::Lost("cut".into())),
            Failure::Retry { after_s: None }
        );
        assert_eq!(
            classify_remote(&api(503, "unavailable")),
            Failure::Retry { after_s: Some(7) }
        );
        assert_eq!(
            classify_remote(&api(429, "throttled")),
            Failure::Retry { after_s: Some(7) }
        );
        assert!(matches!(
            classify_remote(&api(423, "locked")),
            Failure::WaitForChange(ProblemKind::Locked, _)
        ));
        assert!(matches!(
            classify_remote(&api(507, "quota")),
            Failure::WaitForChange(ProblemKind::Quota, _)
        ));
        assert_eq!(classify_remote(&api(412, "precondition")), Failure::Replan);
        assert_eq!(classify_remote(&api(404, "not_found")), Failure::Replan);
        assert_eq!(classify_remote(&api(409, "exists")), Failure::Replan);
        assert!(matches!(
            classify_remote(&api(401, "unauthorized")),
            Failure::Stop(StopReason::SignedOut(_))
        ));
        // Unknown codes by status class, never success.
        assert!(matches!(
            classify_remote(&api(599, "new_thing")),
            Failure::Retry { .. }
        ));
        assert!(matches!(
            classify_remote(&api(418, "new_thing")),
            Failure::WaitForChange(ProblemKind::Refused, _)
        ));
        assert_eq!(
            classify_fs(&FsError::NoSpace),
            Failure::Stop(StopReason::DiskFull)
        );
    }

    #[test]
    fn backoff_grows_caps_and_obeys_retry_after() {
        let a = backoff_ms(1, 1000, 300_000, 0, None);
        assert_eq!(a, 500);
        let b = backoff_ms(4, 1000, 300_000, u64::MAX, None);
        assert!((4000..=8000).contains(&b), "{b}");
        let c = backoff_ms(40, 1000, 300_000, 12345, None);
        assert!(c <= 300_000 && c >= 150_000);
        assert_eq!(backoff_ms(1, 1000, 300_000, 0, Some(60)), 60_000);
    }
}
