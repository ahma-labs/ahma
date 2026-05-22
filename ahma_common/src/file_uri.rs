//! Shared `file://` URI parsing for sandbox root resolution.
//!
//! Both the HTTP bridge (`ahma_http_bridge`) and the core MCP service
//! (`ahma_mcp`) need to convert `file://` URIs from the MCP `roots/list`
//! protocol into filesystem paths that the sandbox validation layer can use.
//! This module provides a single, well-tested implementation that both crates
//! can depend on.
//!
//! ## Security notes
//!
//! * Only `file://` URIs are accepted. All other schemes are rejected.
//! * Decoded NUL bytes (`%00`, overlong encodings) are rejected because they
//!   confuse OS path APIs and are never valid sandbox root components.
//! * Percent-decoding is applied exactly once — double-encoded sequences such
//!   as `%2500` produce the literal string `%00` rather than a NUL byte.
//! * On Unix, only absolute paths (`file:///…` or `file://localhost/…`) are
//!   accepted. Relative paths are rejected.
//! * On Windows, drive-letter forms (`file:///C:/…`) and UNC host forms
//!   (`file://server/share/…`) are additionally accepted.
//! * After this parser returns a `PathBuf`, callers **must** still pass it
//!   through `path_security::validate_path` to catch symlink escapes and
//!   traversal sequences.

use std::path::PathBuf;

// ─────────────────────────────────────────────────────────────────────────────
// Windows helper (compiled out on non-Windows)
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn is_windows_drive_path(path: &str) -> bool {
    // Strip extended-path prefix before checking.
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let path = path.strip_prefix(r"\\?\/").unwrap_or(path);
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Parses a `file://` URI into a `PathBuf`.
///
/// Accepts the following forms:
/// * `file:///abs/path` — standard absolute path
/// * `file://localhost/abs/path` — RFC 8089 `localhost` authority
/// * `file:///C:/abs/path` — Windows drive-letter (Windows targets only)
/// * `file://server/share/path` — Windows UNC host (Windows targets only)
///
/// Returns `None` for:
/// * Non-`file://` schemes
/// * Relative paths (on Unix)
/// * Invalid percent-encoding
/// * Decoded NUL bytes
/// * Invalid UTF-8 in the decoded path
///
/// Query (`?`) and fragment (`#`) components are stripped before decoding.
pub fn parse_file_uri_to_path(uri: &str) -> Option<PathBuf> {
    // RFC 8089-ish minimal parsing.
    const PREFIX: &str = "file://";
    if !uri.starts_with(PREFIX) {
        return None;
    }

    // Remove scheme prefix.
    let mut rest = &uri[PREFIX.len()..];

    // Strip query/fragment before decoding.
    if let Some(idx) = rest.find(['?', '#']) {
        rest = &rest[..idx];
    }

    // Strip optional `localhost` authority.
    if let Some(after_localhost) = rest.strip_prefix("localhost") {
        rest = after_localhost;
    }

    #[cfg(target_os = "windows")]
    {
        // Accept:
        //   file:///C:/Users/name      → strip leading /
        //   file://localhost/C:/Users  → after localhost strip, looks like /C:/...
        //   file://C:/Users/name       → rest starts with C:/
        //   file://server/share/path   → UNC host
        let decoded = percent_decode_utf8(rest)?;

        if let Some(without_slash) = decoded.strip_prefix('/')
            && is_windows_drive_path(without_slash)
        {
            return Some(PathBuf::from(without_slash));
        }

        if is_windows_drive_path(&decoded) {
            return Some(PathBuf::from(decoded));
        }

        // UNC: rest did not start with / and is not a drive path → treat as
        // //server/share/…
        if !decoded.starts_with('/') && !decoded.starts_with("//") {
            return Some(PathBuf::from(format!("//{decoded}")));
        }

        return None;
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Unix: require an absolute path.
        if !rest.starts_with('/') {
            return None;
        }

        let decoded = percent_decode_utf8(rest)?;
        Some(PathBuf::from(decoded))
    }
}

/// Decodes a percent-encoded UTF-8 string.
///
/// Returns `None` if:
/// * A `%` sequence is incomplete (fewer than 2 hex digits follow)
/// * A `%` sequence contains non-hex characters
/// * The resulting bytes are not valid UTF-8
/// * The decoded string contains a NUL byte (`\0`)
pub fn percent_decode_utf8(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                // Need exactly two hex digits after `%`.
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hi = hex_digit(bytes[i + 1])?;
                let lo = hex_digit(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }

    let s = String::from_utf8(out).ok()?;
    // Security: NUL bytes are not valid in paths and can confuse OS path APIs.
    if s.contains('\0') {
        return None;
    }
    Some(s)
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

#[inline]
fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── percent_decode_utf8 ─────────────────────────────────────────────────

    #[test]
    fn decode_plain_ascii() {
        assert_eq!(percent_decode_utf8("/foo/bar"), Some("/foo/bar".into()));
    }

    #[test]
    fn decode_space_lower_hex() {
        assert_eq!(percent_decode_utf8("/path%20to"), Some("/path to".into()));
    }

    #[test]
    fn decode_space_upper_hex() {
        assert_eq!(percent_decode_utf8("/path%20to"), Some("/path to".into()));
        assert_eq!(
            percent_decode_utf8("/path%20to"),
            percent_decode_utf8("/path%20to")
        );
    }

    #[test]
    fn decode_mixed_case_hex() {
        // %2f (lower) == %2F (upper) == /
        assert_eq!(percent_decode_utf8("%2f"), Some("/".into()));
        assert_eq!(percent_decode_utf8("%2F"), Some("/".into()));
    }

    #[test]
    fn decode_unicode_multibyte() {
        // "café" as percent-encoded UTF-8: é = %C3%A9
        assert_eq!(percent_decode_utf8("/caf%C3%A9"), Some("/café".into()));
    }

    #[test]
    fn decode_incomplete_percent_sequence() {
        // Only one hex digit after %
        assert_eq!(percent_decode_utf8("%2"), None);
    }

    #[test]
    fn decode_percent_at_end() {
        // % with nothing following
        assert_eq!(percent_decode_utf8("foo%"), None);
    }

    #[test]
    fn decode_invalid_hex_chars() {
        // %GG — neither G nor G are hex
        assert_eq!(percent_decode_utf8("%GG"), None);
        assert_eq!(percent_decode_utf8("%zz"), None);
    }

    #[test]
    fn decode_invalid_utf8_bytes() {
        // 0x80 is not valid as a leading UTF-8 byte
        assert_eq!(percent_decode_utf8("%80%81"), None);
    }

    #[test]
    fn decode_nul_byte_rejected() {
        // %00 decodes to 0x00, which must be rejected.
        assert_eq!(percent_decode_utf8("%00"), None);
    }

    #[test]
    fn decode_overlong_nul_invalid_utf8() {
        // %C0%80 is the overlong encoding of NUL in CESU-8/Modified UTF-8.
        // Standard UTF-8 does not allow this sequence — String::from_utf8 rejects it.
        assert_eq!(percent_decode_utf8("%C0%80"), None);
    }

    #[test]
    fn decode_double_encoded_is_literal() {
        // %2500 → '%' + '0' + '0' → the literal string "%00" (not a NUL byte)
        assert_eq!(percent_decode_utf8("%2500"), Some("%00".into()));
    }

    #[test]
    fn decode_slash_in_path_segment() {
        // %2F → '/' — the parser does NOT prevent this; sandbox validation does.
        assert_eq!(percent_decode_utf8("a%2Fb"), Some("a/b".into()));
    }

    // ── parse_file_uri_to_path (Unix) ───────────────────────────────────────

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_empty_rejected() {
        assert_eq!(parse_file_uri_to_path(""), None);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_non_file_scheme_rejected() {
        assert_eq!(parse_file_uri_to_path("http://example.com/path"), None);
        assert_eq!(parse_file_uri_to_path("ftp://example.com/path"), None);
        assert_eq!(parse_file_uri_to_path("vscode://folder"), None);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_absolute_path() {
        let p = parse_file_uri_to_path("file:///home/user/project");
        assert_eq!(p, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_localhost_form() {
        let p = parse_file_uri_to_path("file://localhost/home/user/project");
        assert_eq!(p, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_relative_rejected() {
        // file://relative/path — no leading slash after stripping authority
        assert_eq!(parse_file_uri_to_path("file://relative/path"), None);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_query_stripped() {
        let p = parse_file_uri_to_path("file:///home/user?query=1");
        assert_eq!(p, Some(PathBuf::from("/home/user")));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_fragment_stripped() {
        let p = parse_file_uri_to_path("file:///home/user#section");
        assert_eq!(p, Some(PathBuf::from("/home/user")));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_query_and_fragment_stripped() {
        let p = parse_file_uri_to_path("file:///home/user?q=1#frag");
        assert_eq!(p, Some(PathBuf::from("/home/user")));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_percent_encoded_space() {
        let p = parse_file_uri_to_path("file:///home/my%20project");
        assert_eq!(p, Some(PathBuf::from("/home/my project")));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_percent_encoded_unicode() {
        let p = parse_file_uri_to_path("file:///home/caf%C3%A9");
        assert_eq!(p, Some(PathBuf::from("/home/café")));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_nul_byte_rejected() {
        assert_eq!(parse_file_uri_to_path("file:///home/%00evil"), None);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_malformed_percent_rejected() {
        assert_eq!(parse_file_uri_to_path("file:///home/%2"), None);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_invalid_hex_rejected() {
        assert_eq!(parse_file_uri_to_path("file:///home/%GG"), None);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_invalid_utf8_rejected() {
        assert_eq!(parse_file_uri_to_path("file:///%80%81"), None);
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_double_encoded_path_segment() {
        // %252F → literal "%2F" in the path (not a slash). This is not a traversal escape.
        // The result is a path containing the literal text "%2F", which is unusual but
        // not a security concern — validate_path will resolve it further.
        let p = parse_file_uri_to_path("file:///home/scope%252Fsubdir");
        assert_eq!(p, Some(PathBuf::from("/home/scope%2Fsubdir")));
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn uri_percent_encoded_slash_in_segment() {
        // %2F (encoded slash) in the path is decoded to '/'. This can create unexpected
        // path segments. The sandbox validate_path must still be applied after parsing.
        let p = parse_file_uri_to_path("file:///scope/a%2F..%2Foutside");
        // Decoded: /scope/a/../outside — validate_path handles the traversal check.
        assert_eq!(p, Some(PathBuf::from("/scope/a/../outside")));
    }

    // ── parse_file_uri_to_path (Windows — compile-guarded) ─────────────────

    #[test]
    #[cfg(target_os = "windows")]
    fn uri_windows_drive_letter() {
        let p = parse_file_uri_to_path("file:///C:/Users/name");
        assert_eq!(p, Some(PathBuf::from("C:/Users/name")));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn uri_windows_drive_localhost() {
        let p = parse_file_uri_to_path("file://localhost/C:/Users/name");
        assert_eq!(p, Some(PathBuf::from("C:/Users/name")));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn uri_windows_unc_host() {
        let p = parse_file_uri_to_path("file://server/share/path");
        assert_eq!(p, Some(PathBuf::from("//server/share/path")));
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn uri_windows_nul_byte_rejected() {
        assert_eq!(parse_file_uri_to_path("file:///C:/%00evil"), None);
    }
}
