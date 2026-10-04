//! SSH keys as the broker handles them: public key blobs and their
//! fingerprints, signature verification, and signing with an unencrypted
//! ed25519 key file read on the host. Every private-key operation goes through
//! `aws-lc-rs`, the one crypto provider in ahma's graph (SPEC R-CRED.8): no
//! second implementation of any primitive, and none of the `rsa` crate, whose
//! timing advisory `deny.toml` tolerates only for public-key operations.

use super::proto::{Reader, Writer};
use aws_lc_rs::signature::{self, KeyPair, UnparsedPublicKey};
use base64::Engine as _;

/// Why a key file cannot be used to sign on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// Not an `openssh-key-v1` file.
    NotOpenSsh,
    /// Protected by a passphrase: it is used through the human's own agent.
    Encrypted,
    /// A key type the host does not sign with (RSA, ECDSA, security keys): it
    /// is used through the human's own agent.
    Unsupported(String),
    /// The file does not parse.
    Malformed,
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::NotOpenSsh => write!(f, "not an OpenSSH private key"),
            KeyError::Encrypted => write!(f, "protected by a passphrase"),
            KeyError::Unsupported(kind) => write!(f, "a {kind} key"),
            KeyError::Malformed => write!(f, "malformed"),
        }
    }
}

/// An unencrypted ed25519 key read from a file on the host, for one signature.
/// The secret is wiped when this is dropped.
pub struct FileKey {
    public_blob: Vec<u8>,
    seed: [u8; 32],
    comment: String,
}

impl std::fmt::Debug for FileKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileKey")
            .field("fingerprint", &fingerprint(&self.public_blob))
            .field("comment", &self.comment)
            .finish_non_exhaustive()
    }
}

impl Drop for FileKey {
    fn drop(&mut self) {
        for b in self.seed.iter_mut() {
            // SAFETY: `b` is a valid, aligned reference into `self.seed`.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl FileKey {
    pub fn public_blob(&self) -> &[u8] {
        &self.public_blob
    }

    pub fn comment(&self) -> &str {
        &self.comment
    }

    /// Sign `data`, returning an SSH signature blob (`string "ssh-ed25519",
    /// string signature`).
    pub fn sign(&self, data: &[u8]) -> Option<Vec<u8>> {
        let public = ed25519_public(&self.public_blob)?;
        let pair = signature::Ed25519KeyPair::from_seed_and_public_key(&self.seed, public).ok()?;
        debug_assert_eq!(pair.public_key().as_ref(), public);
        let sig = pair.sign(data);
        Some(
            Writer::new()
                .string(b"ssh-ed25519")
                .string(sig.as_ref())
                .finish(),
        )
    }
}

/// The 32-byte public key of an `ssh-ed25519` public key blob.
fn ed25519_public(blob: &[u8]) -> Option<&[u8]> {
    let mut r = Reader::new(blob);
    if r.string()? != b"ssh-ed25519" {
        return None;
    }
    let key = r.string()?;
    (key.len() == 32).then_some(key)
}

/// Parse an `openssh-key-v1` private key file into a key the host can sign
/// with: unencrypted ed25519 only.
pub fn parse_private_key(text: &str) -> Result<FileKey, KeyError> {
    let body: String = text
        .lines()
        .map(str::trim)
        .skip_while(|l| *l != "-----BEGIN OPENSSH PRIVATE KEY-----")
        .skip(1)
        .take_while(|l| *l != "-----END OPENSSH PRIVATE KEY-----")
        .collect();
    if body.is_empty() {
        return Err(KeyError::NotOpenSsh);
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|_| KeyError::Malformed)?;
    const MAGIC: &[u8] = b"openssh-key-v1\0";
    let rest = raw.strip_prefix(MAGIC).ok_or(KeyError::NotOpenSsh)?;
    let mut r = Reader::new(rest);
    let cipher = r.string().ok_or(KeyError::Malformed)?;
    let kdf = r.string().ok_or(KeyError::Malformed)?;
    let _kdf_options = r.string().ok_or(KeyError::Malformed)?;
    if cipher != b"none" || kdf != b"none" {
        return Err(KeyError::Encrypted);
    }
    if r.u32() != Some(1) {
        return Err(KeyError::Malformed);
    }
    let public_blob = r.string().ok_or(KeyError::Malformed)?.to_vec();
    let mut private = Reader::new(r.string().ok_or(KeyError::Malformed)?);
    let (check1, check2) = (private.u32(), private.u32());
    if check1.is_none() || check1 != check2 {
        return Err(KeyError::Malformed);
    }
    let kind = private.utf8().ok_or(KeyError::Malformed)?;
    if kind != "ssh-ed25519" {
        return Err(KeyError::Unsupported(kind.to_string()));
    }
    let public = private.string().ok_or(KeyError::Malformed)?;
    let secret = private.string().ok_or(KeyError::Malformed)?;
    let comment =
        String::from_utf8_lossy(private.string().ok_or(KeyError::Malformed)?).into_owned();
    // The secret is seed || public key, and must agree with the public blob.
    if secret.len() != 64 || &secret[32..] != public || ed25519_public(&public_blob) != Some(public)
    {
        return Err(KeyError::Malformed);
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&secret[..32]);
    Ok(FileKey {
        public_blob,
        seed,
        comment,
    })
}

/// The public key blob of an `authorized_keys`-style line (`type base64 [comment]`).
pub fn parse_public_key_line(line: &str) -> Option<(Vec<u8>, String)> {
    let mut parts = line.split_whitespace();
    let kind = parts.next()?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(parts.next()?)
        .ok()?;
    if Reader::new(&blob).string()? != kind.as_bytes() {
        return None;
    }
    Some((blob, parts.collect::<Vec<_>>().join(" ")))
}

/// `SHA256:<base64>` — the fingerprint `ssh-keygen -l` prints.
pub fn fingerprint(blob: &[u8]) -> String {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, blob);
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest.as_ref())
    )
}

/// The key type named in a public key blob (`ssh-ed25519`, `ssh-rsa`, …).
pub fn key_type(blob: &[u8]) -> Option<String> {
    Reader::new(blob).utf8().map(str::to_string)
}

/// An SSH `mpint` as an unsigned big-endian integer without leading zeros.
fn unsigned(mpint: &[u8]) -> &[u8] {
    let start = mpint.iter().position(|b| *b != 0).unwrap_or(mpint.len());
    &mpint[start..]
}

/// Whether `signature_blob` is a valid signature of `data` by the key in
/// `public_blob`: `ssh-ed25519`, `ecdsa-sha2-nistp256/384`, and RSA with
/// `rsa-sha2-256/512`. Anything else — including SHA-1 `ssh-rsa` — is false.
pub fn verify(public_blob: &[u8], data: &[u8], signature_blob: &[u8]) -> bool {
    verify_inner(public_blob, data, signature_blob).unwrap_or(false)
}

fn verify_inner(public_blob: &[u8], data: &[u8], signature_blob: &[u8]) -> Option<bool> {
    let mut key = Reader::new(public_blob);
    let key_kind = key.utf8()?;
    let mut sig = Reader::new(signature_blob);
    let sig_kind = sig.utf8()?;
    let sig_bytes = sig.string()?;
    match (key_kind, sig_kind) {
        ("ssh-ed25519", "ssh-ed25519") => {
            let public = key.string()?;
            Some(
                UnparsedPublicKey::new(&signature::ED25519, public)
                    .verify(data, sig_bytes)
                    .is_ok(),
            )
        }
        ("ecdsa-sha2-nistp256", "ecdsa-sha2-nistp256")
        | ("ecdsa-sha2-nistp384", "ecdsa-sha2-nistp384") => {
            let (alg, width): (&'static signature::EcdsaVerificationAlgorithm, usize) =
                if key_kind.ends_with("256") {
                    (&signature::ECDSA_P256_SHA256_FIXED, 32)
                } else {
                    (&signature::ECDSA_P384_SHA384_FIXED, 48)
                };
            let _curve = key.string()?;
            let point = key.string()?;
            let mut rs = Reader::new(sig_bytes);
            let (r, s) = (unsigned(rs.string()?), unsigned(rs.string()?));
            if r.len() > width || s.len() > width {
                return Some(false);
            }
            let mut fixed = vec![0u8; 2 * width];
            fixed[width - r.len()..width].copy_from_slice(r);
            fixed[2 * width - s.len()..].copy_from_slice(s);
            Some(
                UnparsedPublicKey::new(alg, point)
                    .verify(data, &fixed)
                    .is_ok(),
            )
        }
        ("ssh-rsa", "rsa-sha2-256") | ("ssh-rsa", "rsa-sha2-512") => {
            let e = unsigned(key.string()?);
            let n = unsigned(key.string()?);
            let alg = if sig_kind == "rsa-sha2-256" {
                &signature::RSA_PKCS1_2048_8192_SHA256
            } else {
                &signature::RSA_PKCS1_2048_8192_SHA512
            };
            Some(
                signature::RsaPublicKeyComponents { n, e }
                    .verify(alg, data, sig_bytes)
                    .is_ok(),
            )
        }
        _ => Some(false),
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! A key made for these tests with `ssh-keygen -t ed25519 -N ''`. It
    //! protects nothing.
    pub const PLAIN_ED25519: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACB0oJMvoCd/m51H/zkgLaQqlFDFSF4LCx+uTEYv+1580AAAAJBHctMAR3LT
AAAAAAtzc2gtZWQyNTUxOQAAACB0oJMvoCd/m51H/zkgLaQqlFDFSF4LCx+uTEYv+1580A
AAAECnXzDMMGVedTdmUvGOkGBVmMAnGzCVA3iMrzC36CjrL3Sgky+gJ3+bnUf/OSAtpCqU
UMVIXgsLH65MRi/7XnzQAAAADGZpeHR1cmVAYWhtYQE=
-----END OPENSSH PRIVATE KEY-----
";
    pub const PLAIN_ED25519_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHSgky+gJ3+bnUf/OSAtpCqUUMVIXgsLH65MRi/7XnzQ fixture@ahma";
    pub const PLAIN_ED25519_FP: &str = "SHA256:Ve/vkYCtZQZGMMywicF/HCNoDLJfT3cvTqK0iAJLOw0";
    /// `ssh-keygen -t ed25519 -N secret`.
    pub const LOCKED_ED25519: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABDeTrtu0X
1MxW2Gr960g2jzAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIGkUmTXRaT8jyUUM
TVcMVJJtkpIIqlTQU+m7SaTUpO4GAAAAkPJ/J591rDg36aO4aRygBcvBa9cBEc6lxVQa2H
MMsh7MoUmUMFA4xrKwsEicX+GLVKRFl8oVs+cfhGakm9NVV7kyV3FI6rG9s1O8ekrH/+dN
Jl0xB6PUZH8pwIYOulHc0vIT74eV+7F00Ic8jZ9IZMgmgixMHDgr39vkSWha7uNeLDsHEP
zdbYUnMIxPdT/DZw==
-----END OPENSSH PRIVATE KEY-----
";
    /// `ssh-keygen -t ecdsa -b 256 -N ''`.
    pub const PLAIN_ECDSA: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNlY2RzYS
1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQSavwEZgQKcuyf3LdGPDXLblrBhuA5A
nK98/2kDWbfi61tJwDftJicoa05QKuJmij+8DQgky8dh3M7K3kEitjnFAAAAoD18SlY9fE
pWAAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBJq/ARmBApy7J/ct
0Y8NctuWsGG4DkCcr3z/aQNZt+LrW0nAN+0mJyhrTlAq4maKP7wNCCTLx2HczsreQSK2Oc
UAAAAgb7aSNxA3WMmSyfkRHINIQqv++ErAcI2buzfm4qNRfVgAAAAHZWNAYWhtYQE=
-----END OPENSSH PRIVATE KEY-----
";
}

#[cfg(test)]
mod interop_tests {
    //! Signatures made by OpenSSH's own `ssh-keygen -Y sign -n file` over
    //! `fixture message\n`, checked with ahma's code: the verifier is what
    //! decides which server a connection is bound to, so it is tested
    //! against the real thing for each host-key type, not only against itself.
    use super::fixtures::*;
    use super::*;

    const MESSAGE: &[u8] = b"fixture message\n";
    const ECDSA_PUB: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBJq/ARmBApy7J/ct0Y8NctuWsGG4DkCcr3z/aQNZt+LrW0nAN+0mJyhrTlAq4maKP7wNCCTLx2HczsreQSK2OcU= ec@ahma";
    const RSA_PUB: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDynaYYOyDHBePuVPnfwCmnc+JxpabzUVazhSH3qPkczp9ekdX4AHeT81ZGICVOz9M7ImazuG+FXzACPF0XE61IoTFxIObYbUNAtO1JKBBRLHr/EMBJfCKKZM4i444v3JorcgxcMl0ooD/uKqFk070GTLxt0VECOyDzwPleJUJH0FEUux0BaJE+NmfEF0QQwPXXusvTN1yoVSpnyTocOBiykLQuQfPb/dp7H9Ca0BPKJ713W+CW+fus1+HFtmhi5295+qG4uEmkLcW2FW3/XSFMrXGpky2IpIhSIj9posHpPHsiy3LqrT8ZeYVMwKGBfziv3LhfJOvPxYRbw0uWEGc/ rsa@ahma";
    const ECDSA_SIG: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAAGgAAAATZWNkc2Etc2hhMi1uaXN0cDI1NgAAAAhuaXN0cDI1NgAAAE
EEmr8BGYECnLsn9y3Rjw1y25awYbgOQJyvfP9pA1m34utbScA37SYnKGtOUCriZoo/vA0I
JMvHYdzOyt5BIrY5xQAAAARmaWxlAAAAAAAAAAZzaGE1MTIAAABkAAAAE2VjZHNhLXNoYT
ItbmlzdHAyNTYAAABJAAAAIQD6RWHlqLknMQnpP3lIDsUYTU/6IQ6TRtlyDDx4c1050QAA
ACAFRk6CSJKvX3Zn9ry8uSXK/zm3JtrY9JcOdeua14eY6A==
-----END SSH SIGNATURE-----
";
    const RSA_SIG: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAARcAAAAHc3NoLXJzYQAAAAMBAAEAAAEBAPKdphg7IMcF4+5U+d/AKa
dz4nGlpvNRVrOFIfeo+RzOn16R1fgAd5PzVkYgJU7P0zsiZrO4b4VfMAI8XRcTrUihMXEg
5thtQ0C07UkoEFEsev8QwEl8IopkziLjji/cmityDFwyXSigP+4qoWTTvQZMvG3RUQI7IP
PA+V4lQkfQURS7HQFokT42Z8QXRBDA9de6y9M3XKhVKmfJOhw4GLKQtC5B89v92nsf0JrQ
E8onvXdb4Jb5+6zX4cW2aGLnb3n6obi4SaQtxbYVbf9dIUytcamTLYikiFIiP2miwek8ey
LLcuqtPxl5hUzAoYF/OK/cuF8k68/FhFvDS5YQZz8AAAAEZmlsZQAAAAAAAAAGc2hhNTEy
AAABFAAAAAxyc2Etc2hhMi01MTIAAAEAfrA78MUkdV61k+7YaqY2wKuyBESyVbb89vni2r
mKBwmBfUifc4X6+XDSSRHXB6nZSuV91/633hUOJ86WwMfybYhrIX9P0uADplgCxjQ/pXno
hOXht+KCpdB6G60mFl2JizLdc4V1T/jQFdVJ31YoKIANs1BQYbuGF1gKXjGdpg3Exgz+OY
0IfGCTqNPXGQGzIXeA52u0CLmBZOyzK3Ck20rW7zn5P2bosjXA5+/FnbUJFrriMGha+81d
v304Bi7/MNS0/zlhLofhB8BetfT2IjqF6HXAjxRxSbHlGZVHRGzSVU7nY2xc9Gjuf0LpJ/
t/d31dKeVL/wol093QdVtSOg==
-----END SSH SIGNATURE-----
";
    const ED25519_SIG: &str = "-----BEGIN SSH SIGNATURE-----
U1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgdKCTL6Anf5udR/85IC2kKpRQxU
heCwsfrkxGL/tefNAAAAAEZmlsZQAAAAAAAAAGc2hhNTEyAAAAUwAAAAtzc2gtZWQyNTUx
OQAAAEDpi8zbMGRHLLTkgogu3u77aZ4eaBctfYezFxad2+jMXqyKKjPclL+8OeVJLxTVFK
+og2Wa6oYLhFUj+zFGLzsL
-----END SSH SIGNATURE-----
";

    /// The data an SSHSIG signature covers, and the signature blob in it.
    fn unwrap_sshsig(armored: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let body: String = armored
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(body)
            .unwrap();
        let mut r = Reader::new(raw.strip_prefix(b"SSHSIG").unwrap());
        assert_eq!(r.u32(), Some(1));
        let public = r.string().unwrap().to_vec();
        let namespace = r.string().unwrap();
        let reserved = r.string().unwrap();
        let hash_alg = r.string().unwrap();
        let sig = r.string().unwrap().to_vec();
        assert_eq!(hash_alg, b"sha512");
        let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA512, MESSAGE);
        let mut signed = b"SSHSIG".to_vec();
        signed.extend(
            Writer::new()
                .string(namespace)
                .string(reserved)
                .string(hash_alg)
                .string(hash.as_ref())
                .finish(),
        );
        (public, signed, sig)
    }

    #[test]
    fn openssh_signatures_verify_for_every_host_key_type() {
        for (public_line, armored) in [
            (PLAIN_ED25519_PUB, ED25519_SIG),
            (ECDSA_PUB, ECDSA_SIG),
            (RSA_PUB, RSA_SIG),
        ] {
            let (public, _) = parse_public_key_line(public_line).unwrap();
            let (embedded, signed, sig) = unwrap_sshsig(armored);
            assert_eq!(embedded, public, "{public_line}");
            assert!(verify(&public, &signed, &sig), "{public_line}");
            let mut tampered = signed.clone();
            *tampered.last_mut().unwrap() ^= 1;
            assert!(!verify(&public, &tampered, &sig), "{public_line}");
        }
    }

    #[test]
    fn the_host_signs_exactly_as_openssh_does() {
        let (_, signed, openssh_sig) = unwrap_sshsig(ED25519_SIG);
        let ours = parse_private_key(PLAIN_ED25519)
            .unwrap()
            .sign(&signed)
            .unwrap();
        assert_eq!(ours, openssh_sig, "ed25519 is deterministic");
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn an_unencrypted_ed25519_key_signs_what_its_public_key_verifies() {
        let key = parse_private_key(PLAIN_ED25519).expect("parses");
        let (public, comment) = parse_public_key_line(PLAIN_ED25519_PUB).unwrap();
        assert_eq!(key.public_blob(), public.as_slice());
        assert_eq!(key.comment(), "fixture@ahma");
        assert_eq!(comment, "fixture@ahma");
        assert_eq!(
            fingerprint(&public),
            PLAIN_ED25519_FP,
            "as ssh-keygen -l prints it"
        );
        assert_eq!(key_type(&public).as_deref(), Some("ssh-ed25519"));

        let sig = key.sign(b"session data").expect("signs");
        assert!(verify(&public, b"session data", &sig));
        assert!(!verify(&public, b"other data", &sig));
        assert!(
            !format!("{key:?}").contains("seed"),
            "Debug never prints the secret: {key:?}"
        );
    }

    #[test]
    fn keys_the_host_does_not_sign_with_say_why() {
        assert_eq!(
            parse_private_key(LOCKED_ED25519).unwrap_err(),
            KeyError::Encrypted
        );
        assert_eq!(
            parse_private_key(PLAIN_ECDSA).unwrap_err(),
            KeyError::Unsupported("ecdsa-sha2-nistp256".into())
        );
        assert_eq!(
            parse_private_key(
                "-----BEGIN RSA PRIVATE KEY-----\nMII=\n-----END RSA PRIVATE KEY-----"
            )
            .unwrap_err(),
            KeyError::NotOpenSsh
        );
        assert_eq!(
            parse_private_key("not a key").unwrap_err(),
            KeyError::NotOpenSsh
        );
    }

    /// `ahma doctor` reads only a key file's header to decide whether a key
    /// needs the human's agent (SPEC R-DOCTOR.6); it must name exactly the
    /// keys this parser refuses.
    #[test]
    fn the_doctor_agrees_which_key_files_the_host_signs_with() {
        for text in [PLAIN_ED25519, LOCKED_ED25519, PLAIN_ECDSA, "not a key"] {
            assert_eq!(
                ahma_common::ssh_sign::host_signs_key_file(text),
                parse_private_key(text).is_ok(),
                "{text}"
            );
        }
    }

    #[test]
    fn a_signature_by_another_key_or_algorithm_does_not_verify() {
        let key = parse_private_key(PLAIN_ED25519).unwrap();
        let sig = key.sign(b"x").unwrap();
        let other = Writer::new()
            .string(b"ssh-ed25519")
            .string(&[7u8; 32])
            .finish();
        assert!(!verify(&other, b"x", &sig));
        let sha1_rsa = Writer::new().string(b"ssh-rsa").string(b"sig").finish();
        let rsa_key = Writer::new()
            .string(b"ssh-rsa")
            .string(&[1, 0, 1])
            .string(&[0xff; 256])
            .finish();
        assert!(
            !verify(&rsa_key, b"x", &sha1_rsa),
            "SHA-1 ssh-rsa is never accepted"
        );
    }
}
