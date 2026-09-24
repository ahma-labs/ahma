//! Every request `ahma update` and `ahma verify` make to GitHub goes through
//! here, so each one retries transient failures and reports a final failure
//! that leads with which service is down (SPEC R-HTTP).

use std::time::Duration;

use ahma_common::http_retry::{
    Failure, Idempotency, RetryPolicy, ServiceError, classify_status, send_with_retry,
};
use anyhow::{Context, Result, anyhow};

const SERVICE: &str = "GitHub";
const HINT: &str = "Check your network connection, then try again.";

/// Connect window: GitHub answers in well under a second when reachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest silence between reads. Per read, not per download, so a slow link
/// still finishes a large archive; a stalled one fails instead of hanging.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The HTTP client for GitHub: identifies itself and cannot hang forever.
pub(crate) fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("ahma-updater")
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .context("Failed to create HTTP client")
}

/// Send a GET that `build` makes, retrying transient failures. `what` says
/// what the request was for ("release metadata from …"), for the details line.
///
/// Returns the response whatever its status; the caller decides what a 404
/// means and turns anything else into [`status_error`].
pub(crate) async fn get<F>(what: &str, build: F) -> Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    send_with_retry(
        SERVICE,
        &RetryPolicy::DEFAULT,
        Idempotency::Idempotent,
        build,
    )
    .await
    .map(|(response, _)| response)
    .map_err(|failure| {
        ServiceError::new(
            SERVICE,
            failure.failure,
            anyhow::Error::new(failure.error).context(format!("fetching {what}")),
        )
        .with_attempts(failure.attempts)
        .with_hint(HINT)
        .into()
    })
}

/// A non-success `response` to a request for `what`, as a summary-first error
/// that keeps GitHub's own body in the details.
pub(crate) async fn status_error(what: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "<unreadable body>".to_string());
    ServiceError::new(
        SERVICE,
        classify_status(status).unwrap_or(Failure::Permanent),
        anyhow!("{what} failed (HTTP {status}): {body}"),
    )
    .with_hint(HINT)
    .into()
}
