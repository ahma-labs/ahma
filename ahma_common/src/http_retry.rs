//! Outbound HTTP: retry with backoff, and failures that lead with a plain
//! summary (SPEC R-HTTP).
//!
//! Every crate that talks HTTP — the LLM client, the MCP clients, the agent,
//! `ahma update`, `fetch_webpage` — sends through [`send_with_retry`] and, when
//! it finally gives up, reports a [`ServiceError`]. That keeps two rules in one
//! place instead of one per call site:
//!
//! * **R-HTTP.2 — retry only what is safe.** A request that never reached the
//!   server, or that the server explicitly asked to be retried (429/503), is
//!   always safe to send again. A timeout or a 5xx mid-request is only safe for
//!   an [`Idempotency::Idempotent`] call: a `tools/call` may already have run.
//! * **R-HTTP.3 — lead with a plain summary.** What the user reads first is
//!   which service is not working, in plain words, then what to do, then the
//!   technical chain.

use std::fmt;
use std::hash::{BuildHasher, RandomState};
use std::time::{Duration, Instant};

use tracing::warn;

/// How often, and how patiently, to retry a transient failure (R-HTTP.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Extra attempts after the first. `0` sends once.
    pub max_retries: u32,
    /// Backoff before the first retry; doubles each retry up to `max_delay`.
    pub base_delay: Duration,
    /// Ceiling on the computed backoff.
    pub max_delay: Duration,
    /// Ceiling on a server-sent `Retry-After`, so a hostile or confused
    /// server cannot park a request for an hour.
    pub max_retry_after: Duration,
    /// Whether a timeout counts as transient. Off for a model served on this
    /// machine: re-sending to a local model that is merely slow restarts its
    /// prompt reading from zero (`ahma_tui` R24.10.8).
    pub retry_timeouts: bool,
}

impl RetryPolicy {
    /// The workspace default: 3 retries, 500 ms doubling to 8 s.
    pub const DEFAULT: Self = Self {
        max_retries: 3,
        base_delay: Duration::from_millis(500),
        max_delay: Duration::from_secs(8),
        max_retry_after: Duration::from_secs(60),
        retry_timeouts: true,
    };

    /// Send once; never retry.
    pub const NONE: Self = Self {
        max_retries: 0,
        ..Self::DEFAULT
    };

    /// This policy with timeouts treated as permanent.
    #[must_use]
    pub const fn without_timeout_retries(self) -> Self {
        Self {
            retry_timeouts: false,
            ..self
        }
    }

    /// How long to wait before retry `attempt` (0-based).
    ///
    /// Exponential backoff with jitter over its upper half, so a burst of
    /// clients that failed together does not retry in lockstep. A server's
    /// `Retry-After` is a floor, capped at `max_retry_after`.
    pub fn delay_for(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let backoff = self
            .base_delay
            .saturating_mul(1u32 << attempt.min(16))
            .min(self.max_delay);
        let half = backoff / 2;
        let jitter = half.mul_f64(unit_jitter());
        let computed = half + jitter;
        match retry_after {
            Some(server) => computed.max(server.min(self.max_retry_after)),
            None => computed,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A uniform value in `[0, 1)` from the std hasher's per-instance random
/// keys — enough to de-synchronise retries without a `rand` dependency.
fn unit_jitter() -> f64 {
    let bits = RandomState::new().hash_one(Instant::now());
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

/// Whether re-sending a request could repeat a side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Idempotency {
    /// Safe to repeat: GET, `initialize`, `tools/list`, downloads, an LLM
    /// completion.
    Idempotent,
    /// Might act twice: `tools/call`, `sampling/createMessage`. Only retried
    /// when the request provably never arrived, or the server asked for it.
    NotIdempotent,
}

/// What kind of failure an attempt ended in (R-HTTP.2). Typed, so no caller
/// ever has to guess from rendered error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The request never reached the server: refused connection, DNS, TLS
    /// handshake. Always safe to retry.
    NotDelivered,
    /// The server said "not now" (429, 503). Always safe to retry, after any
    /// `Retry-After` it gave.
    Throttled,
    /// The server may have seen the request but did not finish it: a timeout,
    /// a dropped connection, 408/500/502/504. Retried only when idempotent.
    Interrupted,
    /// Retrying cannot help: a 4xx, a malformed request, an undecodable body.
    Permanent,
}

impl Failure {
    /// Whether this failure may be retried for a call of `idempotency`.
    pub fn is_retryable(self, idempotency: Idempotency) -> bool {
        match self {
            Self::NotDelivered | Self::Throttled => true,
            Self::Interrupted => idempotency == Idempotency::Idempotent,
            Self::Permanent => false,
        }
    }

    /// Whether the fault is the connection's rather than the request's — the
    /// condition under which a person, or the TUI on their behalf, might
    /// sensibly try again later.
    pub fn is_transient(self) -> bool {
        self != Self::Permanent
    }
}

/// Classify a transport-level error from `reqwest`.
pub fn classify_transport(error: &reqwest::Error) -> Failure {
    if error.is_connect() {
        Failure::NotDelivered
    } else if error.is_timeout() || error.is_request() || error.is_body() {
        // `is_request` covers the connection closing before a response — the
        // shape of a local model server dropping us while it loads.
        Failure::Interrupted
    } else {
        Failure::Permanent
    }
}

/// Classify a response status, or `None` when it succeeded.
pub fn classify_status(status: reqwest::StatusCode) -> Option<Failure> {
    if status.is_success() || status.is_redirection() || status.is_informational() {
        return None;
    }
    Some(match status.as_u16() {
        429 | 503 => Failure::Throttled,
        408 | 500 | 502 | 504 => Failure::Interrupted,
        _ => Failure::Permanent,
    })
}

/// `Retry-After` as (possibly fractional) delta-seconds. The HTTP-date form is
/// rare on the endpoints ahma talks to and is ignored rather than mis-parsed.
pub fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: f64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// How many attempts a call took and how long they spanned — reported in the
/// `Details:` of a [`ServiceError`] so "it failed" and "it failed four times
/// over eleven seconds" read differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempts {
    /// Total sends, the first included.
    pub count: u32,
    /// Wall time from the first send to the final outcome.
    pub elapsed: Duration,
}

impl fmt::Display for Attempts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.elapsed.as_secs_f64();
        if self.count == 1 {
            write!(f, "1 attempt, {secs:.1}s")
        } else {
            write!(f, "{} attempts over {secs:.1}s", self.count)
        }
    }
}

/// A transport error that survived every retry.
#[derive(Debug)]
pub struct TransportFailure {
    /// The last attempt's error.
    pub error: reqwest::Error,
    /// Its classification.
    pub failure: Failure,
    /// Every attempt made.
    pub attempts: Attempts,
}

/// Send the request `build` makes, retrying per `policy` (R-HTTP.1/2).
///
/// `build` is called once per attempt, since a `RequestBuilder` is consumed by
/// sending. `service` names the far end in the retry log lines.
///
/// A response is returned as `Ok` whatever its status once retrying stops —
/// the caller's own status handling decides what a 404 or an exhausted 503
/// means. A transport error is returned only when it is not retryable or the
/// retries ran out.
pub async fn send_with_retry<F>(
    service: &str,
    policy: &RetryPolicy,
    idempotency: Idempotency,
    build: F,
) -> Result<(reqwest::Response, Attempts), TransportFailure>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    send_with_retry_classified(service, policy, idempotency, build, classify_transport).await
}

/// [`send_with_retry`] with the caller's own transport classifier, for a
/// client whose errors carry meaning `reqwest` cannot see — an egress guard's
/// refusal surfaces as a resolver (connect) error, but retrying a policy
/// decision is pointless and calling it "couldn't reach" would be wrong.
pub async fn send_with_retry_classified<F, C>(
    service: &str,
    policy: &RetryPolicy,
    idempotency: Idempotency,
    build: F,
    classify: C,
) -> Result<(reqwest::Response, Attempts), TransportFailure>
where
    F: Fn() -> reqwest::RequestBuilder,
    C: Fn(&reqwest::Error) -> Failure,
{
    let started = Instant::now();
    let mut attempt = 0u32;
    loop {
        let attempts = || Attempts {
            count: attempt + 1,
            elapsed: started.elapsed(),
        };
        match build().send().await {
            Ok(response) => {
                let status = response.status();
                let retry = classify_status(status).is_some_and(|f| f.is_retryable(idempotency))
                    && attempt < policy.max_retries;
                if !retry {
                    return Ok((response, attempts()));
                }
                let delay = policy.delay_for(attempt, parse_retry_after(response.headers()));
                warn!(%service, %status, attempt = attempt + 1, ?delay, "http: retryable status — backing off");
                tokio::time::sleep(delay).await;
            }
            Err(error) => {
                let mut failure = classify(&error);
                if error.is_timeout() && !policy.retry_timeouts {
                    failure = Failure::Permanent;
                }
                if !(failure.is_retryable(idempotency) && attempt < policy.max_retries) {
                    return Err(TransportFailure {
                        error,
                        failure,
                        attempts: attempts(),
                    });
                }
                let delay = policy.delay_for(attempt, None);
                warn!(%service, error = %error, attempt = attempt + 1, ?delay, "http: transport error — backing off");
                tokio::time::sleep(delay).await;
            }
        }
        attempt += 1;
    }
}

/// A failure to use an outside service, worded for the person reading it
/// (R-HTTP.3).
///
/// `Display` renders, in order: the plain one-line `summary`, an optional
/// `hint` saying what to do, then `Details:` with the full technical chain and
/// the attempt count. It implements [`std::error::Error`], so it travels
/// inside an `anyhow` chain and any surface can find it with
/// [`find_service_error`].
#[derive(Debug)]
pub struct ServiceError {
    summary: String,
    hint: Option<String>,
    failure: Failure,
    attempts: Option<Attempts>,
    source: anyhow::Error,
}

impl ServiceError {
    /// A failure of `service` (a plain name: "the ahma daemon", "GitHub",
    /// "your local model server at localhost:11434"), summarised from how it
    /// failed.
    pub fn new(service: &str, failure: Failure, source: impl Into<anyhow::Error>) -> Self {
        let summary = match failure {
            Failure::NotDelivered => format!("Couldn't reach {service}."),
            Failure::Throttled => format!("{} is busy and asked us to wait.", capitalise(service)),
            Failure::Interrupted => format!("{} stopped responding.", capitalise(service)),
            Failure::Permanent => format!("{} couldn't complete the request.", capitalise(service)),
        };
        Self {
            summary,
            hint: None,
            failure,
            attempts: None,
            source: source.into(),
        }
    }

    /// Replace the generated summary with a more specific one-liner.
    #[must_use]
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    /// Say what the reader can do about it.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Record how many attempts were made.
    #[must_use]
    pub fn with_attempts(mut self, attempts: Attempts) -> Self {
        self.attempts = Some(attempts);
        self
    }

    /// Build one from a [`TransportFailure`] that [`send_with_retry`] gave up on.
    pub fn from_transport(service: &str, failure: TransportFailure) -> Self {
        Self::new(service, failure.failure, failure.error).with_attempts(failure.attempts)
    }

    /// The same failure attributed to `service` — for a caller that knows
    /// more about the far end than the layer that failed ("the MCP server at
    /// 127.0.0.1:4242" is, to the agent, "the ahma daemon"). The summary is
    /// regenerated; the hint, attempts and technical detail are kept.
    pub fn for_service(&self, service: &str) -> Self {
        Self {
            hint: self.hint.clone(),
            attempts: self.attempts,
            ..Self::new(service, self.failure, anyhow::anyhow!("{:#}", self.source))
        }
    }

    /// The plain one-line summary.
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// What to do about it, if the call site said.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// The technical detail: the full cause chain and attempt count.
    pub fn details(&self) -> String {
        match self.attempts {
            Some(a) if a.count > 1 => format!("{:#} (gave up after {a})", self.source),
            _ => format!("{:#}", self.source),
        }
    }

    /// How it failed.
    pub fn failure(&self) -> Failure {
        self.failure
    }

    /// Whether trying again later might work.
    pub fn is_transient(&self) -> bool {
        self.failure.is_transient()
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary)?;
        if let Some(hint) = &self.hint {
            write!(f, "\n{hint}")?;
        }
        write!(f, "\nDetails: {}", self.details())
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// The first [`ServiceError`] in `error`'s chain, if any.
pub fn find_service_error(error: &anyhow::Error) -> Option<&ServiceError> {
    error.chain().find_map(|e| e.downcast_ref::<ServiceError>())
}

/// How to show `error` to a person (R-HTTP.3): a [`ServiceError`] anywhere in
/// the chain renders in its summary-first form; anything else renders its full
/// chain (`{:#}`) rather than just the outermost context, which hides the
/// cause.
pub fn user_message(error: &anyhow::Error) -> String {
    match find_service_error(error) {
        Some(service) => service.to_string(),
        None => format!("{error:#}"),
    }
}

/// `s` with its first letter upper-cased, so a service name such as "your
/// local model server" can open a sentence.
pub fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Retries fast enough for a unit test; the backoff maths is covered
    /// separately by the `delay_for` tests.
    const FAST: RetryPolicy = RetryPolicy {
        max_retries: 3,
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(4),
        max_retry_after: Duration::from_millis(20),
        retry_timeouts: true,
    };

    #[test]
    fn delay_for_stays_within_the_upper_half_of_the_capped_backoff() {
        let p = RetryPolicy::DEFAULT;
        for attempt in 0..10 {
            let full = p
                .base_delay
                .saturating_mul(1u32 << attempt)
                .min(p.max_delay);
            for _ in 0..50 {
                let d = p.delay_for(attempt, None);
                assert!(
                    d >= full / 2 && d <= full,
                    "attempt {attempt}: {d:?} vs {full:?}"
                );
            }
        }
    }

    #[test]
    fn retry_after_is_a_floor_but_capped() {
        let p = RetryPolicy::DEFAULT;
        assert!(p.delay_for(0, Some(Duration::from_secs(5))) >= Duration::from_secs(5));
        assert_eq!(
            p.delay_for(0, Some(Duration::from_secs(3600))),
            p.max_retry_after,
            "a server must not park a request for an hour"
        );
    }

    #[test]
    fn status_classification() {
        use reqwest::StatusCode as S;
        assert_eq!(classify_status(S::OK), None);
        assert_eq!(
            classify_status(S::TOO_MANY_REQUESTS),
            Some(Failure::Throttled)
        );
        assert_eq!(
            classify_status(S::SERVICE_UNAVAILABLE),
            Some(Failure::Throttled)
        );
        for s in [
            S::REQUEST_TIMEOUT,
            S::INTERNAL_SERVER_ERROR,
            S::BAD_GATEWAY,
            S::GATEWAY_TIMEOUT,
        ] {
            assert_eq!(classify_status(s), Some(Failure::Interrupted), "{s}");
        }
        for s in [
            S::BAD_REQUEST,
            S::UNAUTHORIZED,
            S::FORBIDDEN,
            S::NOT_FOUND,
            S::CONFLICT,
        ] {
            assert_eq!(classify_status(s), Some(Failure::Permanent), "{s}");
        }
    }

    #[test]
    fn only_undelivered_or_throttled_requests_retry_when_not_idempotent() {
        use Idempotency::*;
        assert!(Failure::NotDelivered.is_retryable(NotIdempotent));
        assert!(Failure::Throttled.is_retryable(NotIdempotent));
        assert!(
            !Failure::Interrupted.is_retryable(NotIdempotent),
            "a tool may already have run"
        );
        assert!(Failure::Interrupted.is_retryable(Idempotent));
        assert!(!Failure::Permanent.is_retryable(Idempotent));
    }

    #[test]
    fn parse_retry_after_accepts_delta_seconds_only() {
        let mut h = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&h), None);
        h.insert(reqwest::header::RETRY_AFTER, "2.5".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_millis(2500)));
        h.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn service_error_leads_with_the_summary_then_hint_then_details() {
        let err = ServiceError::new(
            "your local model server at localhost:11434",
            Failure::NotDelivered,
            anyhow::anyhow!("connection refused").context("error sending request"),
        )
        .with_hint("Check it is running, then send your message again.")
        .with_attempts(Attempts {
            count: 4,
            elapsed: Duration::from_millis(11_200),
        });
        let text = err.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "Couldn't reach your local model server at localhost:11434."
        );
        assert_eq!(
            lines[1],
            "Check it is running, then send your message again."
        );
        assert_eq!(
            lines[2],
            "Details: error sending request: connection refused (gave up after 4 attempts over 11.2s)",
            "details must carry the whole chain, not just the outer context"
        );
        assert!(err.is_transient());
    }

    #[test]
    fn for_service_renames_but_keeps_the_detail() {
        let original = ServiceError::new(
            "the MCP server at 127.0.0.1:4242",
            Failure::NotDelivered,
            anyhow::anyhow!("connection refused").context("tools/call request"),
        )
        .with_attempts(Attempts {
            count: 4,
            elapsed: Duration::from_secs(3),
        });
        let renamed = original.for_service("the ahma daemon");
        assert_eq!(renamed.summary(), "Couldn't reach the ahma daemon.");
        assert_eq!(renamed.details(), original.details());
    }

    #[test]
    fn user_message_finds_a_service_error_under_added_context() {
        let service = ServiceError::new("GitHub", Failure::Throttled, anyhow::anyhow!("HTTP 429"));
        let err = anyhow::Error::new(service).context("checking for updates");
        assert!(
            user_message(&err).starts_with("GitHub is busy"),
            "{}",
            user_message(&err)
        );

        let plain = anyhow::anyhow!("root cause").context("outer");
        assert_eq!(user_message(&plain), "outer: root cause");
    }

    /// A one-connection-per-request HTTP/1.1 stub that answers with each
    /// status in `script` in turn, then 200 forever. Returns its URL and a
    /// count of requests seen.
    async fn scripted_server(script: Vec<u16>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let status = script.get(n).copied().unwrap_or(200);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (url, seen)
    }

    #[tokio::test]
    async fn idempotent_call_retries_through_5xx_and_503() {
        let (url, seen) = scripted_server(vec![500, 503, 502]).await;
        let client = reqwest::Client::new();
        let (resp, attempts) =
            send_with_retry("stub", &FAST, Idempotency::Idempotent, || client.get(&url))
                .await
                .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(attempts.count, 4);
        assert_eq!(seen.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn non_idempotent_call_is_not_repeated_after_a_500() {
        let (url, seen) = scripted_server(vec![500]).await;
        let client = reqwest::Client::new();
        let (resp, attempts) = send_with_retry("stub", &FAST, Idempotency::NotIdempotent, || {
            client.post(&url)
        })
        .await
        .unwrap();
        assert_eq!(resp.status(), 500, "the 500 is handed back, not retried");
        assert_eq!(attempts.count, 1);
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn non_idempotent_call_still_retries_a_throttle() {
        let (url, seen) = scripted_server(vec![429]).await;
        let client = reqwest::Client::new();
        let (resp, _) = send_with_retry("stub", &FAST, Idempotency::NotIdempotent, || {
            client.post(&url)
        })
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(seen.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn exhausted_retries_return_the_last_response() {
        let (url, seen) = scripted_server(vec![503; 10]).await;
        let client = reqwest::Client::new();
        let (resp, attempts) =
            send_with_retry("stub", &FAST, Idempotency::Idempotent, || client.get(&url))
                .await
                .unwrap();
        assert_eq!(resp.status(), 503);
        assert_eq!(attempts.count, FAST.max_retries + 1);
        assert_eq!(seen.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn refused_connection_is_retried_even_when_not_idempotent_then_reported() {
        // Bind then drop, so the port is (almost certainly) closed.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{port}/");
        let client = reqwest::Client::new();
        let err = send_with_retry("stub", &FAST, Idempotency::NotIdempotent, || {
            client.post(&url)
        })
        .await
        .expect_err("nothing is listening");
        assert_eq!(err.failure, Failure::NotDelivered);
        assert_eq!(err.attempts.count, FAST.max_retries + 1);

        let service = ServiceError::from_transport("the ahma daemon", err);
        assert!(
            service
                .to_string()
                .starts_with("Couldn't reach the ahma daemon.")
        );
        assert!(service.details().contains("gave up after 4 attempts"));
    }
}
