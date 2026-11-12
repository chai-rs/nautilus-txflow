//! Error types for the TxFlow HTTP client.

use std::time::Duration;

use nautilus_network::http::HttpClientError;
use thiserror::Error;

/// Result type for TxFlow HTTP operations.
pub type TxflowHttpResult<T> = Result<T, TxflowHttpError>;

/// Errors returned by the TxFlow HTTP client.
#[derive(Debug, Clone, Error)]
pub enum TxflowHttpError {
    /// Transport failure or timeout.
    #[error("network error: {0}")]
    Network(String),
    /// Non-success HTTP status without a venue error body.
    #[error("HTTP {status}: {body}")]
    Http {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
    /// HTTP 429. The request did not execute.
    #[error("rate limit exceeded: {message}")]
    RateLimit {
        message: String,
        retry_after: Option<Duration>,
    },
    /// `{"success":false,"code":…,"message":…}` or `{"status":"err",…}`.
    #[error("venue error {code}: {message}")]
    Venue { code: i64, message: String },
    /// The request was rejected locally, before any call to the venue.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The response could not be decoded or converted.
    #[error("parse error: {0}")]
    Parse(String),
}

impl TxflowHttpError {
    pub(super) const fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Http { retry_after, .. } | Self::RateLimit { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl From<HttpClientError> for TxflowHttpError {
    fn from(error: HttpClientError) -> Self {
        Self::Network(error.to_string())
    }
}

impl From<serde_json::Error> for TxflowHttpError {
    fn from(error: serde_json::Error) -> Self {
        Self::Parse(error.to_string())
    }
}

impl From<anyhow::Error> for TxflowHttpError {
    fn from(error: anyhow::Error) -> Self {
        Self::Parse(error.to_string())
    }
}

/// Returns whether a failed read request may be sent again.
#[must_use]
pub fn should_retry(error: &TxflowHttpError) -> bool {
    match error {
        TxflowHttpError::Network(_) | TxflowHttpError::RateLimit { .. } => true,
        TxflowHttpError::Http { status, .. } => *status >= 500,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn http(status: u16) -> TxflowHttpError {
        TxflowHttpError::Http {
            status,
            body: String::new(),
            retry_after: Some(Duration::from_secs(2)),
        }
    }

    #[rstest]
    #[case(TxflowHttpError::Network(String::new()), true)]
    #[case(TxflowHttpError::RateLimit { message: String::new(), retry_after: None }, true)]
    #[case(http(503), true)]
    #[case(http(404), false)]
    #[case(TxflowHttpError::Venue { code: 21001, message: String::new() }, false)]
    #[case(TxflowHttpError::Parse(String::new()), false)]
    fn retry_only_transport_rate_limit_and_5xx(
        #[case] error: TxflowHttpError,
        #[case] expected: bool,
    ) {
        assert_eq!(should_retry(&error), expected);
    }
}
