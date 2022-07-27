use thiserror::Error;

pub use crate::messenger::RequestError;
pub use crate::protocol::error::Error as ProtocolError;

/// Context for [`Error::ServerError`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ServerErrorContext {
    Topic(String),
    Partition(String, i32),
}

/// Payload for [`Error::ServerError`].
///
/// This is data that the server sent and that is still usable despite the error.
#[derive(Debug)]
#[allow(missing_copy_implementations)] // wanna extend this later
#[non_exhaustive]
pub enum ServerErrorPayload {
    LeaderForward {
        broker: i32,
        new_leader: i32,
    },
    FetchState {
        high_watermark: i64,
        last_stable_offset: Option<i64>,
    },
}

#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error("Connection error: {0}")]
    Connection(#[from] crate::connection::Error),

    #[error("Request error: {0}")]
    Request(#[from] RequestError),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error(
        "Server error {} with message \"{}\", context: {:?}, payload: {:?}, virtual: {}",
        protocol_error,
        string_or_na(error_message),
        context,
        payload,
        is_virtual
    )]
    ServerError {
        /// Protocol-level error message.
        protocol_error: ProtocolError,

        /// Server message provided by the broker, if any.
        error_message: Option<String>,

        /// Additional context that we can tell the user about.
        context: Option<ServerErrorContext>,

        /// Additional payload that we can provide the user.
        payload: Option<ServerErrorPayload>,

        /// Flags if the error was generated by the client to simulate some server behavior or workaround a bug.
        is_virtual: bool,
    },

    #[error("All retries failed: {0}")]
    RetryFailed(#[from] crate::backoff::BackoffError),
}

impl Error {
    pub(crate) fn exactly_one_topic(len: usize) -> Self {
        Self::InvalidResponse(format!("Expected a single topic in response, got {len}"))
    }

    pub(crate) fn exactly_one_partition(len: usize) -> Self {
        Self::InvalidResponse(format!(
            "Expected a single partition in response, got {len}"
        ))
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Simple formatting function the replaces `None` with `"n/a"`.
fn string_or_na(s: &Option<String>) -> &str {
    match s {
        Some(s) => s.as_str(),
        None => "n/a",
    }
}
