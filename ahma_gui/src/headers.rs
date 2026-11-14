//! What every response says about itself (SPEC R-GUI.4).
//!
//! One policy, applied to every response — pages, API answers, refusals, 404s —
//! so there is no route that forgets it. The page can load only itself: scripts
//! and styles from this origin, no inline script or style, no frames, no forms,
//! no other origin to connect to, and (`require-trusted-types-for`) no string
//! reaching a DOM sink that interprets markup. Nothing is cached, so a token or
//! a view is never kept anywhere but where it was meant to be.

use axum::{
    extract::Request,
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};

/// The Content-Security-Policy, as one constant so a test can pin it.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
style-src 'self'; connect-src 'self'; img-src 'self'; frame-ancestors 'none'; base-uri 'none'; \
form-action 'none'; require-trusted-types-for 'script'";

/// Every header added to every response, and its value.
pub const SECURITY_HEADERS: &[(&str, &str)] = &[
    ("content-security-policy", CONTENT_SECURITY_POLICY),
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "no-referrer"),
    ("cache-control", "no-store"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
    (
        "permissions-policy",
        "camera=(), microphone=(), geolocation=()",
    ),
];

/// The middleware: add the policy to whatever the rest of the stack answered.
/// Outermost, so it covers a refusal made by a layer inside it too.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        headers.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    // Never a CORS header: a page on another origin cannot read an answer.
    headers.remove("access-control-allow-origin");
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy is exactly this. A looser one — an inline-script allowance, a
    /// second connect source, a frame ancestor — is a different product.
    #[test]
    fn the_policy_lets_the_page_load_only_itself() {
        assert_eq!(
            CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
             img-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'; \
             require-trusted-types-for 'script'"
        );
        for forbidden in [
            "unsafe-inline",
            "unsafe-eval",
            "http:",
            "https:",
            "data:",
            "*",
        ] {
            assert!(
                !CONTENT_SECURITY_POLICY.contains(forbidden),
                "the policy must not contain {forbidden:?}"
            );
        }
    }

    #[test]
    fn every_header_is_a_valid_header() {
        for (name, value) in SECURITY_HEADERS {
            HeaderName::from_bytes(name.as_bytes()).unwrap();
            HeaderValue::from_str(value).unwrap();
        }
    }
}
