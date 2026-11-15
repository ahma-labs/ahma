//! The listener's routes (SPEC R-GUI.1–R-GUI.5).
//!
//! The order of the layers is the security model. A request is refused for what
//! it is before anything is read of what it says ([`guard`]); every answer, a
//! refusal included, carries the page's policy ([`crate::headers::security_headers`]); and
//! everything but the page itself and `/api/pair` wants a tab's token.
//!
//! ```text
//! security_headers          every response
//!   guard                   peer, Host, Origin, Sec-Fetch-Site, method
//!     body limit            64 KiB
//!       GET  /, /app.css, /favicon.svg, /dist/*      the page; no data
//!       POST /api/pair                               a typed code for a token
//!       require_session
//!         GET  /api/events                           the view, as it changes
//!         POST /api/intent                           what the human did
//!         POST /api/report                           what the tab shows
//! ```

use crate::assets;
use crate::auth::{Auth, PairError, TabId};
use crate::dto::{PROTOCOL, WireIntent, WireReport};
use crate::frontend::{IntentError, Published, WebInput};
use crate::{guard, headers};
use axum::{
    Extension, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::stream::{self, Stream};
use serde::Serialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

/// The largest body any route reads.
const MAX_BODY: usize = 64 * 1024;
/// How long an intent waits to be applied before the page is told the server is
/// busy.
const INTENT_WAIT: Duration = Duration::from_secs(5);

/// What the handlers share.
pub struct AppCtx {
    pub auth: Arc<Auth>,
    pub inputs: mpsc::Sender<WebInput>,
    pub published: watch::Receiver<Arc<Published>>,
    /// Becomes `true` when the server is stopping: every open event stream then
    /// says `bye` and ends, so graceful shutdown is not left waiting on a tab.
    /// A handler sets it too, when pairing has to end the server.
    pub stop: Arc<watch::Sender<bool>>,
    /// Open event streams, for the idle stop (SPEC R-GUI.12).
    pub idle: Arc<crate::idle::Idle>,
    /// Where the code was shown, which decides what a lock or a reuse means.
    pub pairing: Pairing,
    /// Run after each successful pairing (clears the clipboard of the code).
    pub on_paired: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Where the pairing code was shown (SPEC R-GUI.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pairing {
    /// On the controlling terminal, which stays attached (`--foreground`):
    /// Enter unlocks, and a new code appears there.
    Terminal,
    /// Once, by a launcher that has since returned to the prompt; `copied`
    /// when it also went on the clipboard. Nobody can press Enter and no new
    /// code can be shown, so a lock or an expiry ends the server.
    Background { copied: bool },
}

/// The whole router.
pub fn router(ctx: Arc<AppCtx>) -> Router {
    let api = Router::new()
        .route("/events", get(events))
        .route("/intent", post(intent))
        .route("/report", post(report))
        .fallback(api_not_found)
        .layer(middleware::from_fn_with_state(ctx.clone(), require_session));
    Router::new()
        .route("/api/pair", post(pair))
        .nest("/api", api)
        .fallback(page)
        .with_state(ctx)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn(guard::guard))
        .layer(middleware::from_fn(headers::security_headers))
}

// ── Answers ──────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_string(&ErrorBody {
            error: ErrorDetail { code, message },
        })
        .unwrap_or_default(),
    )
        .into_response()
}

fn intent_error(e: IntentError) -> Response {
    let (status, code, message) = match e {
        IntentError::Stale => (
            StatusCode::CONFLICT,
            "stale",
            "That question is not the one waiting any more.",
        ),
        IntentError::NotArmed => (
            StatusCode::CONFLICT,
            "not_armed",
            "Read the question first: it can be answered a moment after it is shown here.",
        ),
        IntentError::NeedsConfirm => (
            StatusCode::CONFLICT,
            "needs_confirm",
            "That answer needs a second step.",
        ),
        IntentError::NotOffered => (
            StatusCode::BAD_REQUEST,
            "bad_request",
            "The question does not offer that answer.",
        ),
        IntentError::NotAllowed => (
            StatusCode::FORBIDDEN,
            "not_allowed",
            "This is not available here. Use `ahma tui` for it.",
        ),
        IntentError::Nothing => (StatusCode::CONFLICT, "nothing", "There was nothing to do."),
    };
    error(status, code, message)
}

// ── The page ─────────────────────────────────────────────────────────────────

async fn page(request: Request) -> Response {
    let (method, uri) = (request.method(), request.uri());
    if !matches!(*method, axum::http::Method::GET | axum::http::Method::HEAD) {
        return error(StatusCode::NOT_FOUND, "not_found", "Not found.");
    }
    match assets::lookup(uri.path()) {
        Some(asset) => ([(header::CONTENT_TYPE, asset.content_type)], asset.body).into_response(),
        None => error(StatusCode::NOT_FOUND, "not_found", "Not found."),
    }
}

async fn api_not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not_found", "Not found.")
}

// ── Pairing ──────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PairRequest {
    code: String,
}

async fn pair(State(ctx): State<Arc<AppCtx>>, body: Bytes) -> Response {
    let Ok(request) = serde_json::from_slice::<PairRequest>(&body) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Send {\"code\": \"...\"}.",
        );
    };
    let result = ctx.auth.pair(&request.code);
    let background = matches!(ctx.pairing, Pairing::Background { .. });
    let stop = |why: &str| {
        tracing::warn!("web: stopping: {why}");
        ctx.stop.send_replace(true);
    };
    match result {
        Ok((_tab, token)) => {
            if let Some(hook) = &ctx.on_paired {
                hook();
            }
            (
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::json!({ "token": token }).to_string(),
            )
                .into_response()
        }
        Err(PairError::WrongCode) => error(
            StatusCode::UNAUTHORIZED,
            "wrong_code",
            match ctx.pairing {
                Pairing::Background { copied: true } => {
                    "That is not the pairing code ahma web copied to your clipboard and printed in your terminal."
                }
                _ => "That is not the code shown in your terminal.",
            },
        ),
        Err(PairError::Locked) if background => {
            stop("too many wrong pairing codes");
            error(
                StatusCode::LOCKED,
                "locked",
                "Too many wrong codes, so ahma web has stopped. To pair, run `ahma web` again.",
            )
        }
        Err(PairError::Locked) => error(
            StatusCode::LOCKED,
            "locked",
            "Too many wrong codes. Press Enter in the terminal that started ahma web to try again.",
        ),
        Err(PairError::TooManyTabs) => error(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_clients",
            "Too many tabs are open. Close one and try again.",
        ),
        Err(PairError::Expired) if background => {
            stop("the pairing code expired");
            error(
                StatusCode::GONE,
                "expired",
                "That code has expired, so ahma web has stopped. To pair, run `ahma web` again.",
            )
        }
        Err(PairError::Expired) => error(
            StatusCode::GONE,
            "expired",
            "That code has expired. Type the new one shown in your terminal.",
        ),
        Err(PairError::Reused) => {
            stop("a spent pairing code was used again");
            error(
                StatusCode::CONFLICT,
                "reused",
                "That code was already used. If you did not pair another tab with it, something \
                 else on this machine read it from the clipboard. ahma web has stopped and \
                 disconnected every tab. Run `ahma web` again; add --no-copy to type the code \
                 instead of pasting it.",
            )
        }
    }
}

/// The token in `Authorization: Bearer <token>`; the scheme is case-insensitive.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

async fn require_session(
    State(ctx): State<Arc<AppCtx>>,
    mut request: Request,
    next: Next,
) -> Response {
    let tab = bearer(request.headers()).and_then(|token| ctx.auth.authenticate(token));
    match tab {
        Some(tab) => {
            request.extensions_mut().insert(tab);
            next.run(request).await
        }
        None => error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Pair this tab with the code shown in your terminal.",
        ),
    }
}

// ── The view ─────────────────────────────────────────────────────────────────

/// Tells the frontend a tab's stream is gone when the stream is dropped, however
/// that happens.
struct StreamGuard {
    tab: TabId,
    ctx: Arc<AppCtx>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.ctx.auth.stream_closed(self.tab);
        self.ctx.idle.closed(std::time::Instant::now());
        let inputs = self.ctx.inputs.clone();
        let tab = self.tab;
        // `try_send` first: a full queue is rare, and a dropped stream must not
        // wait on it.
        if inputs.try_send(WebInput::TabClosed(tab)).is_err() {
            tokio::spawn(async move {
                let _ = inputs.send(WebInput::TabClosed(tab)).await;
            });
        }
    }
}

/// The JSON of one `view` event: the regions that changed after revision
/// `after` (all of them when it is the first), composed from their already
/// serialized JSON.
pub fn view_json(published: &Published, after: Option<u64>) -> String {
    let mut out = format!(r#"{{"protocol":{PROTOCOL},"rev":{}"#, published.rev);
    for region in &published.regions {
        if after.is_none_or(|seen| region.changed > seen) {
            out.push_str(&format!(r#","{}":{}"#, region.name, region.json));
        }
    }
    out.push('}');
    out
}

async fn events(State(ctx): State<Arc<AppCtx>>, Extension(tab): Extension<TabId>) -> Response {
    // The frontend learns a tab is looking before the first view goes out, so
    // the session is a subscriber by the time the human could answer anything.
    if ctx.inputs.send(WebInput::TabOpened(tab)).await.is_err() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "ahma web is shutting down.",
        );
    }
    ctx.auth.stream_opened(tab);
    ctx.idle.opened();
    let guard = StreamGuard {
        tab,
        ctx: ctx.clone(),
    };
    let rx = ctx.published.clone();
    let stop = ctx.stop.subscribe();
    Sse::new(event_stream(rx, stop, guard))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

fn event_stream(
    rx: watch::Receiver<Arc<Published>>,
    stop: watch::Receiver<bool>,
    guard: StreamGuard,
) -> impl Stream<Item = Result<Event, Infallible>> {
    struct State {
        rx: watch::Receiver<Arc<Published>>,
        stop: watch::Receiver<bool>,
        seen: Option<u64>,
        done: bool,
        _guard: StreamGuard,
    }
    fn bye(mut st: State) -> Option<(Result<Event, Infallible>, State)> {
        st.done = true;
        Some((Ok(Event::default().event("bye").data("")), st))
    }
    stream::unfold(
        State {
            rx,
            stop,
            seen: None,
            done: false,
            _guard: guard,
        },
        |mut st| async move {
            loop {
                if st.done {
                    return None;
                }
                if *st.stop.borrow() {
                    return bye(st);
                }
                let current = st.rx.borrow_and_update().clone();
                // Nothing is published before the session's first presentation.
                let ready = !current.regions.is_empty();
                if ready && st.seen.is_none_or(|seen| current.rev > seen) {
                    let data = view_json(&current, st.seen);
                    st.seen = Some(current.rev);
                    return Some((Ok(Event::default().event("view").data(data)), st));
                }
                tokio::select! {
                    changed = st.rx.changed() => {
                        if changed.is_err() {
                            // The session is gone: say so, and the page stops retrying.
                            return bye(st);
                        }
                    }
                    // A stop, or the sender gone (the server is going): either
                    // way the loop's check above says bye.
                    stopped = st.stop.changed() => {
                        if stopped.is_err() {
                            return bye(st);
                        }
                    }
                }
            }
        },
    )
}

// ── What the human did ───────────────────────────────────────────────────────

async fn intent(
    State(ctx): State<Arc<AppCtx>>,
    Extension(tab): Extension<TabId>,
    body: Bytes,
) -> Response {
    let Ok(intent) = serde_json::from_slice::<WireIntent>(&body) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "That is not something this page can ask.",
        );
    };
    let (reply, answer) = oneshot::channel();
    if ctx
        .inputs
        .try_send(WebInput::Intent { tab, intent, reply })
        .is_err()
    {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "ahma is busy; try again.",
        );
    }
    match tokio::time::timeout(INTENT_WAIT, answer).await {
        Ok(Ok(Ok(()))) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(Err(e))) => intent_error(e),
        _ => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "ahma is busy; try again.",
        ),
    }
}

async fn report(
    State(ctx): State<Arc<AppCtx>>,
    Extension(tab): Extension<TabId>,
    body: Bytes,
) -> Response {
    let Ok(report) = serde_json::from_slice::<WireReport>(&body) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "That is not a report.",
        );
    };
    // Dropped under load rather than waited for: the next report corrects it, and
    // a report that is late only delays a tab's standing, never grants one.
    match ctx.inputs.try_send(WebInput::Report { tab, report }) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "busy",
            "ahma is busy; try again.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::Region;
    use ahma_common::timeouts::{TestTimeouts, TimeoutCategory};
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Method, Request as HttpRequest};
    use http_body_util_lite::collect;
    use std::net::SocketAddr;
    use tower::ServiceExt;

    /// Just enough of a body collector for tests, without another dependency.
    mod http_body_util_lite {
        pub async fn collect(body: axum::body::Body) -> Vec<u8> {
            axum::body::to_bytes(body, usize::MAX)
                .await
                .map(|b| b.to_vec())
                .unwrap_or_default()
        }
    }

    struct Harness {
        ctx: Arc<AppCtx>,
        inputs: mpsc::Receiver<WebInput>,
        published: watch::Sender<Arc<Published>>,
        stop: Arc<watch::Sender<bool>>,
        code: Arc<std::sync::Mutex<String>>,
        paired_hook: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn harness() -> Harness {
        harness_with(16)
    }

    fn harness_with(capacity: usize) -> Harness {
        harness_for(
            capacity,
            Pairing::Terminal,
            crate::auth::PairingPolicy::default(),
        )
    }

    fn harness_for(
        capacity: usize,
        pairing: Pairing,
        policy: crate::auth::PairingPolicy,
    ) -> Harness {
        let code = Arc::new(std::sync::Mutex::new(String::new()));
        let sink = code.clone();
        let auth = Arc::new(
            Auth::new(Arc::new(move |c: &str| {
                *sink.lock().unwrap() = c.to_string();
            }))
            .with_policy(policy),
        );
        let (inputs_tx, inputs_rx) = mpsc::channel(capacity);
        let (published_tx, published_rx) = watch::channel(Arc::new(Published::default()));
        let stop = Arc::new(watch::channel(false).0);
        let paired_hook = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = paired_hook.clone();
        Harness {
            ctx: Arc::new(AppCtx {
                auth,
                inputs: inputs_tx,
                published: published_rx,
                stop: stop.clone(),
                idle: Arc::new(crate::idle::Idle::new(std::time::Instant::now())),
                pairing,
                on_paired: Some(Arc::new(move || {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })),
            }),
            inputs: inputs_rx,
            published: published_tx,
            stop,
            code,
            paired_hook,
        }
    }

    async fn try_pair(h: &Harness, code: &str) -> (StatusCode, serde_json::Value) {
        let r = send(
            &h.ctx,
            request(Method::POST, "/api/pair", HOST),
            &format!(r#"{{"code":"{code}"}}"#),
        )
        .await;
        let status = r.status();
        let body = serde_json::from_slice(&collect(r.into_body()).await).unwrap_or_default();
        (status, body)
    }

    const CLIPBOARD: Pairing = Pairing::Background { copied: true };
    const BURN: crate::auth::PairingPolicy = crate::auth::PairingPolicy {
        code_ttl: crate::auth::CODE_TTL,
        burn_on_reuse: true,
    };

    /// The code was on the clipboard: when the human's paste finds it already
    /// used, someone else read it. The answer says so and the server stops.
    #[tokio::test]
    async fn a_reused_clipboard_code_stops_the_server_and_says_why() {
        let h = harness_for(16, CLIPBOARD, BURN);
        let code = h.code.lock().unwrap().replace('-', "");
        let (first, _) = try_pair(&h, &code).await;
        assert_eq!(first, StatusCode::OK);
        assert_eq!(
            h.paired_hook.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a pairing runs the hook that clears the clipboard"
        );
        let (status, body) = try_pair(&h, &code).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "reused");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("ahma web") && message.contains("--no-copy"),
            "{message}"
        );
        assert!(*h.stop.borrow(), "the server was told to stop");
    }

    /// In the background nobody can press Enter, and no new code can be shown:
    /// a lock or an expiry ends the server, and the page says to run it again.
    #[tokio::test]
    async fn a_lock_in_the_background_stops_the_server() {
        let h = harness_for(16, CLIPBOARD, BURN);
        let mut last = (StatusCode::OK, serde_json::Value::Null);
        for _ in 0..crate::auth::MISSES_BEFORE_LOCK {
            last = try_pair(&h, "WRONGCODE").await;
        }
        assert_eq!(last.0, StatusCode::LOCKED);
        let message = last.1["error"]["message"].as_str().unwrap();
        assert!(message.contains("run `ahma web` again"), "{message}");
        assert!(*h.stop.borrow());
    }

    #[tokio::test]
    async fn an_expired_code_in_the_background_stops_the_server() {
        let h = harness_for(
            16,
            CLIPBOARD,
            crate::auth::PairingPolicy {
                code_ttl: std::time::Duration::ZERO,
                burn_on_reuse: true,
            },
        );
        let code = h.code.lock().unwrap().replace('-', "");
        let (status, body) = try_pair(&h, &code).await;
        assert_eq!(status, StatusCode::GONE);
        assert_eq!(body["error"]["code"], "expired");
        assert!(*h.stop.borrow());
    }

    /// At a terminal the same lock waits for Enter, and the server stays.
    #[tokio::test]
    async fn a_lock_at_the_terminal_keeps_the_server() {
        let h = harness();
        for _ in 0..crate::auth::MISSES_BEFORE_LOCK {
            try_pair(&h, "WRONGCODE").await;
        }
        assert!(!*h.stop.borrow());
    }

    /// An open stream is a tab watching; the idle clock counts only when none
    /// is open.
    #[tokio::test]
    async fn an_open_stream_holds_off_the_idle_stop() {
        let mut h = harness();
        let token = paired(&h).await;
        h.published.send_replace(published(1, &[("log", "{}", 1)]));
        let later = std::time::Instant::now() + std::time::Duration::from_secs(3600);
        let r = send(&h.ctx, authed(Method::GET, "/api/events", &token), "").await;
        assert!(matches!(
            h.inputs.recv().await,
            Some(WebInput::TabOpened(_))
        ));
        assert!(
            !h.ctx.idle.due(later, std::time::Duration::from_secs(60)),
            "a tab is watching"
        );
        drop(r);
        let _ = tokio::time::timeout(
            TestTimeouts::get(TimeoutCategory::HttpRequest),
            h.inputs.recv(),
        )
        .await;
        assert!(h.ctx.idle.due(later, std::time::Duration::from_secs(60)));
    }

    fn request(method: Method, path: &str, host: &str) -> axum::http::request::Builder {
        HttpRequest::builder()
            .method(method)
            .uri(path)
            .header("host", host)
    }

    async fn send(
        ctx: &Arc<AppCtx>,
        builder: axum::http::request::Builder,
        body: &str,
    ) -> Response {
        let mut request = builder.body(Body::from(body.to_string())).unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
        ));
        router(ctx.clone()).oneshot(request).await.unwrap()
    }

    const HOST: &str = "127.0.0.1:5000";

    async fn paired(h: &Harness) -> String {
        let code = h.code.lock().unwrap().replace('-', "");
        let r = send(
            &h.ctx,
            request(Method::POST, "/api/pair", HOST),
            &format!(r#"{{"code":"{code}"}}"#),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&collect(r.into_body()).await).unwrap();
        body["token"].as_str().unwrap().to_string()
    }

    fn authed(method: Method, path: &str, token: &str) -> axum::http::request::Builder {
        request(method, path, HOST).header("authorization", format!("Bearer {token}"))
    }

    // ── routes and credentials ───────────────────────────────────────────────

    /// Every route under `/api` except `/api/pair` refuses a request with no
    /// token, with a wrong one, and with another scheme — and says nothing about
    /// whether the route exists beyond that.
    #[tokio::test]
    async fn every_api_route_but_pair_wants_a_token() {
        let h = harness();
        for (method, path) in [
            (Method::GET, "/api/events"),
            (Method::POST, "/api/intent"),
            (Method::POST, "/api/report"),
            (Method::GET, "/api/nonexistent"),
            (Method::POST, "/api/anything/else"),
        ] {
            let r = send(&h.ctx, request(method.clone(), path, HOST), "{}").await;
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
            for header_value in ["Bearer nope", "Bearer 1.deadbeef", "Basic abc", "bearer"] {
                let r = send(
                    &h.ctx,
                    request(method.clone(), path, HOST).header("authorization", header_value),
                    "{}",
                )
                .await;
                assert_eq!(
                    r.status(),
                    StatusCode::UNAUTHORIZED,
                    "{method} {path} {header_value}"
                );
            }
        }
    }

    /// The page needs no credential and carries no data, so it is served to a
    /// request without one; everything else under it is a 404, not a hint.
    #[tokio::test]
    async fn the_page_is_served_without_a_token_and_nothing_else_is() {
        let h = harness();
        for path in ["/", "/app.css", "/favicon.svg", "/dist/main.js"] {
            let r = send(&h.ctx, request(Method::GET, path, HOST), "").await;
            assert_eq!(r.status(), StatusCode::OK, "{path}");
        }
        for path in [
            "/etc/passwd",
            "/dist/",
            "/dist/../Cargo.toml",
            "/index.html",
            "/api",
        ] {
            let r = send(&h.ctx, request(Method::GET, path, HOST), "").await;
            assert!(
                matches!(r.status(), StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED),
                "{path}: {}",
                r.status()
            );
        }
        let r = send(&h.ctx, request(Method::POST, "/", HOST), "").await;
        assert_eq!(
            r.status(),
            StatusCode::NOT_FOUND,
            "the page is read, not written"
        );
    }

    // ── the guard and the headers ────────────────────────────────────────────

    /// A request that is not from this page is refused before its credential is
    /// looked at: a foreign Host, a foreign Origin, a cross-site fetch — even
    /// with a perfectly good token, and without spending a pairing attempt.
    #[tokio::test]
    async fn a_request_that_is_not_from_this_page_is_refused_before_its_credential() {
        let h = harness();
        let token = paired(&h).await;
        let cases = [
            authed(Method::POST, "/api/intent", &token).header("host", "evil.example"),
            authed(Method::POST, "/api/intent", &token).header("origin", "http://127.0.0.1:9999"),
            authed(Method::POST, "/api/intent", &token).header("sec-fetch-site", "cross-site"),
            authed(Method::DELETE, "/api/intent", &token),
        ];
        for (i, builder) in cases.into_iter().enumerate() {
            // `header()` appends, so the first Host is the valid one: rebuild
            // the foreign-host case without it.
            let builder = if i == 0 {
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/api/intent")
                    .header("host", "evil.example")
                    .header("authorization", format!("Bearer {token}"))
            } else {
                builder
            };
            let r = send(&h.ctx, builder, r#"{"type":"cancel_turn"}"#).await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "case {i}");
        }

        // A page that is not this one cannot burn the human's pairing attempts.
        for _ in 0..30 {
            let r = send(
                &h.ctx,
                HttpRequest::builder()
                    .method(Method::POST)
                    .uri("/api/pair")
                    .header("host", "evil.example")
                    .header("origin", "http://evil.example"),
                r#"{"code":"AAAAAAAA"}"#,
            )
            .await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
        }
        let code = h.code.lock().unwrap().replace('-', "");
        let r = send(
            &h.ctx,
            request(Method::POST, "/api/pair", HOST),
            &format!(r#"{{"code":"{code}"}}"#),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK, "the real code still works");
    }

    /// Every answer says the same thing about what the page may load, and none
    /// is cacheable or cross-origin readable — the page, an API answer, a
    /// refusal and a 404 alike.
    #[tokio::test]
    async fn every_response_carries_the_policy() {
        let h = harness();
        let token = paired(&h).await;
        let responses = [
            send(&h.ctx, request(Method::GET, "/", HOST), "").await,
            send(&h.ctx, request(Method::GET, "/missing", HOST), "").await,
            send(&h.ctx, request(Method::POST, "/api/intent", HOST), "{}").await,
            send(
                &h.ctx,
                authed(Method::POST, "/api/intent", &token),
                r#"{"type":"nonsense"}"#,
            )
            .await,
            send(
                &h.ctx,
                HttpRequest::builder()
                    .uri("/")
                    .header("host", "evil.example"),
                "",
            )
            .await,
        ];
        for r in responses {
            let headers = r.headers();
            assert_eq!(
                headers.get("content-security-policy").unwrap(),
                headers::CONTENT_SECURITY_POLICY,
                "{}",
                r.status()
            );
            assert_eq!(headers.get("cache-control").unwrap(), "no-store");
            assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
            assert!(
                headers.get("access-control-allow-origin").is_none(),
                "never a CORS header"
            );
        }
    }

    // ── pairing ──────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn pairing_gives_a_token_that_works_and_a_wrong_code_does_not() {
        let h = harness();
        let wrong = send(
            &h.ctx,
            request(Method::POST, "/api/pair", HOST),
            r#"{"code":"AAAA-AAAA"}"#,
        )
        .await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let body = String::from_utf8(collect(wrong.into_body()).await).unwrap();
        assert!(body.contains("wrong_code"), "{body}");

        let token = paired(&h).await;
        assert_eq!(token.split_once('.').unwrap().1.len(), 64);
        let r = send(
            &h.ctx,
            authed(Method::POST, "/api/report", &token),
            r#"{"type":"gate_hidden","decision_id":"x"}"#,
        )
        .await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);

        // Not JSON, an unknown field and an oversized body are refused.
        for bad in ["code=1", r#"{"code":"x","extra":1}"#, "[]", ""] {
            let r = send(&h.ctx, request(Method::POST, "/api/pair", HOST), bad).await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad:?}");
        }
        let huge = format!(r#"{{"code":"{}"}}"#, "A".repeat(MAX_BODY + 10));
        let r = send(&h.ctx, request(Method::POST, "/api/pair", HOST), &huge).await;
        assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn repeated_wrong_codes_lock_pairing_with_a_message_that_says_how_to_unlock() {
        let h = harness();
        let mut last = StatusCode::OK;
        let mut body = String::new();
        for _ in 0..crate::auth::MISSES_BEFORE_LOCK {
            let r = send(
                &h.ctx,
                request(Method::POST, "/api/pair", HOST),
                r#"{"code":"WRONGCDE"}"#,
            )
            .await;
            last = r.status();
            body = String::from_utf8(collect(r.into_body()).await).unwrap();
        }
        assert_eq!(last, StatusCode::LOCKED);
        assert!(
            body.contains("locked") && body.contains("terminal"),
            "{body}"
        );
    }

    // ── intents and reports ──────────────────────────────────────────────────

    /// A request the page cannot make — an unknown intent, an extra field, a
    /// persistent answer — is a 400 before it reaches the session.
    #[tokio::test]
    async fn an_intent_outside_the_allow_list_never_reaches_the_session() {
        let mut h = harness();
        let token = paired(&h).await;
        for bad in [
            r#"{"type":"run_host_command","command":"id"}"#,
            r#"{"type":"answer_web_approval","decision_id":"d","answer":"always"}"#,
            r#"{"type":"submit","text":"x","persist":true}"#,
            r#"not json"#,
        ] {
            let r = send(&h.ctx, authed(Method::POST, "/api/intent", &token), bad).await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
        assert!(h.inputs.try_recv().is_err(), "nothing was forwarded");
    }

    /// An accepted intent is the session's to answer; its refusal reaches the
    /// page as the typed error the contract names.
    #[tokio::test]
    async fn the_sessions_answer_to_an_intent_is_the_pages() {
        let mut h = harness();
        let token = paired(&h).await;
        for (outcome, status, code) in [
            (Ok(()), StatusCode::NO_CONTENT, ""),
            (Err(IntentError::Stale), StatusCode::CONFLICT, "stale"),
            (
                Err(IntentError::NotArmed),
                StatusCode::CONFLICT,
                "not_armed",
            ),
            (
                Err(IntentError::NotAllowed),
                StatusCode::FORBIDDEN,
                "not_allowed",
            ),
            (
                Err(IntentError::NotOffered),
                StatusCode::BAD_REQUEST,
                "bad_request",
            ),
            (Err(IntentError::Nothing), StatusCode::CONFLICT, "nothing"),
            (
                Err(IntentError::NeedsConfirm),
                StatusCode::CONFLICT,
                "needs_confirm",
            ),
        ] {
            let request = send(
                &h.ctx,
                authed(Method::POST, "/api/intent", &token),
                r#"{"type":"cancel_turn"}"#,
            );
            let answer = async {
                match h.inputs.recv().await {
                    Some(WebInput::Intent { reply, .. }) => {
                        let _ = reply.send(outcome);
                    }
                    _ => panic!("an intent was forwarded"),
                }
            };
            let (r, ()) = tokio::join!(request, answer);
            assert_eq!(r.status(), status, "{code}");
            if !code.is_empty() {
                let body = String::from_utf8(collect(r.into_body()).await).unwrap();
                assert!(body.contains(code), "{body}");
            }
        }
    }

    /// A busy session is said to be busy, not waited on for ever.
    #[tokio::test]
    async fn a_full_queue_is_a_busy_not_a_hang() {
        let h = harness_with(1);
        let token = paired(&h).await;
        // The queue is full: nothing has taken what is in it.
        h.ctx
            .inputs
            .try_send(WebInput::TabClosed(TabId(9)))
            .unwrap();
        let r = send(
            &h.ctx,
            authed(Method::POST, "/api/intent", &token),
            r#"{"type":"cancel_turn"}"#,
        )
        .await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        let r = send(
            &h.ctx,
            authed(Method::POST, "/api/report", &token),
            r#"{"type":"gate_hidden","decision_id":"x"}"#,
        )
        .await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ── the stream ───────────────────────────────────────────────────────────

    fn published(rev: u64, regions: &[(&'static str, &str, u64)]) -> Arc<Published> {
        Arc::new(Published {
            rev,
            regions: regions
                .iter()
                .map(|(name, json, changed)| Region {
                    name,
                    json: json.to_string(),
                    changed: *changed,
                })
                .collect(),
        })
    }

    /// The first view carries every region; a later one only the regions that
    /// changed after the last revision sent.
    #[test]
    fn a_view_carries_the_regions_that_changed_and_the_first_carries_all() {
        let p = published(
            5,
            &[
                ("header", r#"{"a":1}"#, 1),
                ("gates", r#"{"waiting":0,"top":null}"#, 5),
            ],
        );
        assert_eq!(
            view_json(&p, None),
            r#"{"protocol":1,"rev":5,"header":{"a":1},"gates":{"waiting":0,"top":null}}"#
        );
        assert_eq!(
            view_json(&p, Some(3)),
            r#"{"protocol":1,"rev":5,"gates":{"waiting":0,"top":null}}"#
        );
        assert_eq!(view_json(&p, Some(5)), r#"{"protocol":1,"rev":5}"#);
        let parsed: serde_json::Value = serde_json::from_str(&view_json(&p, None)).unwrap();
        assert_eq!(parsed["protocol"], 1);
    }

    /// A stream opens with the whole view, then sends what changes, and says
    /// `bye` when the session ends so the page stops reconnecting. The tab is
    /// announced to the frontend before the first view goes out.
    #[tokio::test]
    async fn the_stream_sends_the_view_then_changes_then_bye() {
        use futures::StreamExt;
        let mut h = harness();
        let token = paired(&h).await;
        h.published.send_replace(published(
            1,
            &[
                ("header", r#"{"workspace":"/w"}"#, 1),
                ("log", r#"{"lines":[]}"#, 1),
            ],
        ));

        let r = send(&h.ctx, authed(Method::GET, "/api/events", &token), "").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        assert!(matches!(
            h.inputs.recv().await,
            Some(WebInput::TabOpened(_))
        ));

        let mut body = r.into_body().into_data_stream();
        let mut next = async || {
            let chunk =
                tokio::time::timeout(TestTimeouts::get(TimeoutCategory::HttpRequest), body.next())
                    .await
                    .expect("an event arrives")
                    .expect("the stream is open")
                    .expect("a chunk");
            String::from_utf8(chunk.to_vec()).unwrap()
        };
        let first = next().await;
        assert!(first.contains("event: view"), "{first}");
        assert!(
            first.contains(r#""header":{"workspace":"/w"}"#) && first.contains(r#""log":"#),
            "{first}"
        );

        h.published.send_replace(published(
            2,
            &[
                ("header", r#"{"workspace":"/w"}"#, 1),
                ("log", r#"{"lines":["x"]}"#, 2),
            ],
        ));
        let second = next().await;
        assert!(second.contains(r#""log":{"lines":["x"]}"#), "{second}");
        assert!(
            !second.contains(r#""header""#),
            "the header did not change: {second}"
        );

        drop(h.published);
        let last = next().await;
        assert!(last.contains("event: bye"), "{last}");
    }

    /// Stopping the server ends every open stream with `bye`. Graceful shutdown
    /// waits for in-flight responses, and an event stream never finishes on its
    /// own: without this, stopping with a tab open waited for the browser, and
    /// the page was never told the server had gone.
    #[tokio::test]
    async fn stopping_the_server_says_bye_and_ends_an_open_stream() {
        use futures::StreamExt;
        let mut h = harness();
        let token = paired(&h).await;
        h.published.send_replace(published(1, &[("log", "{}", 1)]));
        let r = send(&h.ctx, authed(Method::GET, "/api/events", &token), "").await;
        assert!(matches!(
            h.inputs.recv().await,
            Some(WebInput::TabOpened(_))
        ));
        let mut body = r.into_body().into_data_stream();
        let wait = TestTimeouts::get(TimeoutCategory::HttpRequest);
        let first = tokio::time::timeout(wait, body.next()).await.unwrap();
        assert!(first.is_some(), "the first view arrives");

        h.stop.send_replace(true);
        let bye = tokio::time::timeout(wait, body.next())
            .await
            .expect("bye arrives after stop")
            .expect("the stream is still open for bye")
            .expect("a chunk");
        let bye = String::from_utf8(bye.to_vec()).unwrap();
        assert!(bye.contains("event: bye"), "{bye}");
        let end = tokio::time::timeout(wait, body.next())
            .await
            .expect("the stream ends after bye");
        assert!(end.is_none(), "nothing follows bye");
    }

    /// A stream that goes away tells the frontend, so the session can stop being
    /// a viewer, and its token starts the reconnect grace.
    #[tokio::test]
    async fn a_dropped_stream_tells_the_frontend() {
        let mut h = harness();
        let token = paired(&h).await;
        h.published.send_replace(published(1, &[("log", "{}", 1)]));
        let r = send(&h.ctx, authed(Method::GET, "/api/events", &token), "").await;
        assert!(matches!(
            h.inputs.recv().await,
            Some(WebInput::TabOpened(_))
        ));
        drop(r);
        match tokio::time::timeout(
            TestTimeouts::get(TimeoutCategory::HttpRequest),
            h.inputs.recv(),
        )
        .await
        {
            Ok(Some(WebInput::TabClosed(_))) => {}
            other => panic!("the frontend is told the tab went: {}", other.is_ok()),
        }
    }
}
