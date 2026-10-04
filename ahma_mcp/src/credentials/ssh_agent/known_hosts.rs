//! Naming a server from its host key, through `known_hosts`. The key bound to
//! a connection (verified by its own signature, [`super::purpose`]) is what a
//! consent decision is keyed on; the name is what the human reads, and is
//! shown as verified only when a `known_hosts` line for it holds that key.

use aws_lc_rs::hmac;
use base64::Engine as _;

/// What `known_hosts` says about one host key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownHost {
    /// Names whose `known_hosts` line holds the key: plain entries as written
    /// (`github.com`, `[git.example.com]:2222`), and hashed entries matched
    /// against the candidates the caller supplied.
    pub names: Vec<String>,
    /// A `@revoked` line names the key.
    pub revoked: bool,
}

/// Look `host_key_blob` up in the text of a `known_hosts` file. `candidates`
/// are the names a hashed entry is checked against (hashed entries cannot be
/// read back).
pub fn lookup(contents: &str, host_key_blob: &[u8], candidates: &[String]) -> KnownHost {
    let mut found = KnownHost::default();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let mut first = fields.next().unwrap_or_default();
        let marker = first.strip_prefix('@');
        if marker.is_some() {
            first = fields.next().unwrap_or_default();
        }
        let (Some(_kind), Some(key)) = (fields.next(), fields.next()) else {
            continue;
        };
        let Ok(blob) = base64::engine::general_purpose::STANDARD.decode(key) else {
            continue;
        };
        if blob != host_key_blob {
            continue;
        }
        match marker {
            Some("revoked") => {
                found.revoked = true;
                continue;
            }
            // A CA key signs host certificates; it names no host itself.
            Some(_) => continue,
            None => {}
        }
        for pattern in first.split(',') {
            if let Some(hashed) = pattern.strip_prefix("|1|") {
                for name in candidates {
                    if hashed_matches(hashed, name) && !found.names.contains(name) {
                        found.names.push(name.clone());
                    }
                }
            } else if !pattern.starts_with('!')
                && !pattern.contains(['*', '?'])
                && !found.names.iter().any(|n| n == pattern)
            {
                found.names.push(pattern.to_string());
            }
        }
    }
    found
}

/// Whether a hashed entry (`<salt>|<hash>`, base64) is `name`:
/// HMAC-SHA1(salt, name) == hash, as `ssh-keygen -H` writes it.
fn hashed_matches(hashed: &str, name: &str) -> bool {
    let Some((salt, hash)) = hashed.split_once('|') else {
        return false;
    };
    let decode = |s: &str| base64::engine::general_purpose::STANDARD.decode(s).ok();
    let (Some(salt), Some(hash)) = (decode(salt), decode(hash)) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
    hmac::verify(&key, name.as_bytes(), &hash).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";

    fn blob(b64: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap()
    }

    #[test]
    fn plain_entries_name_the_host() {
        let text = format!(
            "# a comment\n\
             github.com,140.82.121.4 ssh-ed25519 {GITHUB}\n\
             [git.example.com]:2222 ssh-ed25519 {GITHUB}\n\
             *.example.org ssh-ed25519 {GITHUB}\n\
             other.example ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHSgky+gJ3+bnUf/OSAtpCqUUMVIXgsLH65MRi/7XnzQ\n"
        );
        let found = lookup(&text, &blob(GITHUB), &[]);
        assert_eq!(
            found.names,
            vec!["github.com", "140.82.121.4", "[git.example.com]:2222"],
            "wildcards name no host"
        );
        assert!(!found.revoked);
        assert_eq!(lookup(&text, b"unknown", &[]), KnownHost::default());
    }

    #[test]
    fn hashed_entries_match_a_candidate_and_revoked_keys_are_flagged() {
        // `ssh-keygen -H` form of `github.com`, computed for this test.
        let salt = [1u8; 20];
        let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
        let tag = hmac::sign(&key, b"github.com");
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        let hashed = format!(
            "|1|{}|{} ssh-ed25519 {GITHUB}",
            b64(&salt),
            b64(tag.as_ref())
        );
        let candidates = vec!["gitlab.com".to_string(), "github.com".to_string()];
        assert_eq!(
            lookup(&hashed, &blob(GITHUB), &candidates).names,
            vec!["github.com"]
        );
        assert!(lookup(&hashed, &blob(GITHUB), &[]).names.is_empty());

        let revoked = format!("@revoked * ssh-ed25519 {GITHUB}\n{hashed}");
        assert!(lookup(&revoked, &blob(GITHUB), &candidates).revoked);
    }
}
