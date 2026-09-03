//! Content digests shared across the workspace.
//!
//! One encoder for every surface that writes or checks a `SHA256SUMS`-style
//! line — the release-artifact check in `ahma_update` and the bundle content
//! manifest in `ahma_bundle` — so there is no version of this project where
//! the two can be allowed to differ.

/// Lowercase hex of the SHA-256 of `bytes`, matching the format used in the
/// release `SHA256SUMS` files.
///
/// `sha2` 0.11 returns a `hybrid_array::Array`, which — unlike the `GenericArray`
/// of 0.10 — does not implement `LowerHex`, so the encoding is done here.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(test)]
mod tests {
    use super::sha256_hex;

    /// Known-answer test for [`sha256_hex`], pinning it to the exact encoding a
    /// release `SHA256SUMS` line carries.
    ///
    /// The checksum round-trip tests hash with `sha256_hex` on *both* sides, so
    /// an encoding that is wrong but self-consistent (e.g. a dropped zero-pad)
    /// would still satisfy them while silently failing against a real
    /// `SHA256SUMS` from GitHub. These vectors are independent of our code.
    #[test]
    fn sha256_hex_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        // Digest begins with the byte 0xa0: catches a missing `{:02x}` zero-pad,
        // which would render a low nibble as one char and shorten the string.
        let padded = sha256_hex(b"\x00\x0a\xff");
        assert_eq!(
            padded,
            "a0956176ad28cadf4a54b314f9fcd6143d7007957454286ff24580445304b558"
        );
        assert_eq!(padded.len(), 64, "SHA-256 hex must always be 64 chars");
    }
}
