//! Error type of the Soulseek client (ports of the Soulseek.NET exception hierarchy).

use crate::protocol::ProtocolError;

/// Convenience alias for results produced by this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Rejection message sent to a peer when the enqueue resolver fails with an
/// error that is not an explicit rejection (avoids leaking internal details).
pub const ENQUEUE_INTERNAL_ERROR_MESSAGE: &str = "Enqueue failed due to internal error";

/// Errors produced by the Soulseek client.
///
/// Variants carrying a `String` hold the complete human readable message (as
/// Soulseek.NET exception messages do); `Display` prints it verbatim.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An argument was invalid (`ArgumentException`).
    #[error("{0}")]
    Argument(String),
    /// The client is not in a state permitting the operation, e.g. not connected
    /// or logged in (`InvalidOperationException`).
    #[error("{0}")]
    InvalidState(String),
    /// The operation timed out (`TimeoutException`).
    #[error("{0}")]
    Timeout(String),
    /// The operation was cancelled (`OperationCanceledException`).
    #[error("The operation was canceled.")]
    Cancelled,
    /// A connection could not be established or failed while in use, including
    /// read/write and proxy failures (`ConnectionException` and subclasses).
    #[error("{0}")]
    Connection(String),
    /// An address could not be resolved (`AddressException`).
    #[error("{0}")]
    Address(String),
    /// The listener could not bind (`ListenException`).
    #[error("{0}")]
    Listen(String),
    /// The server rejected the login (`LoginRejectedException`).
    #[error("{0}")]
    LoginRejected(String),
    /// The server disconnected the client, typically because the same user logged
    /// in elsewhere (`KickedFromServerException`).
    #[error("Kicked from server")]
    KickedFromServer,
    /// The remote user is offline (`UserOfflineException`).
    #[error("{0}")]
    UserOffline(String),
    /// The remote user does not exist (`UserNotFoundException`).
    #[error("{0}")]
    UserNotFound(String),
    /// The endpoint of a user could not be resolved (`UserEndPointException`).
    #[error("{0}")]
    UserEndPoint(String),
    /// The server or peer did not respond (`NoResponseException`).
    #[error("{0}")]
    NoResponse(String),
    /// A chat room operation failed (`RoomException`).
    #[error("{0}")]
    Room(String),
    /// The server refused to let the client join a room (`RoomJoinForbiddenException`).
    #[error("{0}")]
    RoomJoinForbidden(String),
    /// A generic transfer failure (`TransferException`).
    #[error("{0}")]
    Transfer(String),
    /// The remote peer rejected the transfer; holds the rejection message.
    /// Also accepted from [`crate::Resolvers::enqueue_download`] to reject an enqueue.
    #[error("{0}")]
    TransferRejected(String),
    /// No matching transfer is active (`TransferNotFoundException`).
    #[error("{0}")]
    TransferNotFound(String),
    /// The remote peer reported the transfer as failed (`TransferReportedFailedException`).
    #[error("{0}")]
    TransferReportedFailed(String),
    /// The size reported by the remote peer differs from the expected size.
    #[error("{message}")]
    TransferSizeMismatch {
        message: String,
        local_size: u64,
        remote_size: u64,
    },
    /// Reading from or writing to the transfer stream failed (`TransferStreamException`).
    #[error("{0}")]
    TransferStream(String),
    /// A transfer of the same file with the same user is already active or queued.
    #[error("{0}")]
    DuplicateTransfer(String),
    /// The specified or generated token is already in use.
    #[error("{0}")]
    DuplicateToken(String),
    /// Returned from [`crate::Resolvers::enqueue_download`] to reject an enqueue;
    /// the message is passed through to the remote user (`DownloadEnqueueException`).
    #[error("{0}")]
    DownloadEnqueue(String),
    /// I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Malformed or unexpected protocol message (`MessageException` and subclasses).
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    /// Any other failure (`SoulseekClientException`).
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Whether this is [`Error::Cancelled`].
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Error::Cancelled)
    }

    /// Whether this is [`Error::Timeout`].
    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::Timeout(_))
    }

    /// The rejection message sent to a peer when the enqueue resolver returned
    /// this error: explicit rejections pass their message through, anything else
    /// yields [`ENQUEUE_INTERNAL_ERROR_MESSAGE`].
    pub fn enqueue_rejection_message(&self) -> String {
        match self {
            Error::DownloadEnqueue(message) | Error::TransferRejected(message) => message.clone(),
            _ => ENQUEUE_INTERNAL_ERROR_MESSAGE.to_string(),
        }
    }
}

impl From<tokio::time::error::Elapsed> for Error {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Error::Timeout("The operation has timed out.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_rejection_message_passes_explicit_rejections() {
        assert_eq!(
            Error::DownloadEnqueue("File not shared.".into()).enqueue_rejection_message(),
            "File not shared."
        );
        assert_eq!(
            Error::TransferRejected("Queued".into()).enqueue_rejection_message(),
            "Queued"
        );
        assert_eq!(
            Error::Other("disk path /secret".into()).enqueue_rejection_message(),
            ENQUEUE_INTERNAL_ERROR_MESSAGE
        );
    }

    #[test]
    fn display_is_message() {
        assert_eq!(Error::Timeout("x".into()).to_string(), "x");
        assert_eq!(Error::Cancelled.to_string(), "The operation was canceled.");
    }
}
