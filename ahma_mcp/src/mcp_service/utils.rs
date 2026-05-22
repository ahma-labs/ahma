use std::path::PathBuf;

impl super::AhmaMcpService {
    /// Parses a `file://` URI into a `PathBuf`.
    ///
    /// Delegates to the shared implementation in `ahma_common::file_uri` so
    /// that security improvements apply consistently across all callers.
    #[allow(dead_code)]
    pub(crate) fn parse_file_uri_to_path(uri: &str) -> Option<PathBuf> {
        ahma_common::file_uri::parse_file_uri_to_path(uri)
    }

    /// Decodes a percent-encoded UTF-8 string.
    ///
    /// Delegates to the shared implementation in `ahma_common::file_uri`.
    #[allow(dead_code)]
    pub(crate) fn percent_decode_utf8(input: &str) -> Option<String> {
        ahma_common::file_uri::percent_decode_utf8(input)
    }
}
