//! The one error type `send` and `execute` return.

use std::time::Duration;

use thiserror::Error;

/// Error from sending a request.
///
/// Everything `reqwest` can fail with passes through unchanged. The only
/// addition is [`AllExhausted`](Self::AllExhausted).
#[derive(Debug, Error)]
pub enum Error {
    /// A transport, TLS, timeout, or builder error from `reqwest`.
    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
    /// Every identity is cooling and the client was built with [`Exhausted::Error`](crate::Exhausted::Error).
    #[error("every identity is exhausted; soonest reset in {retry_after:?}")]
    AllExhausted {
        /// Time until the first identity leaves cooldown.
        retry_after: Duration,
    },
}
