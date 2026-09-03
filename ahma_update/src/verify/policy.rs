//! The certificate identity policy `ahma verify` / `ahma update` enforce.
//!
//! A Sigstore signature only says "some Fulcio-certified identity signed this".
//! The policy is what turns that into "the official `paulirotta/ahma` GitHub
//! Actions pipeline signed this", so it is the part that must not be loose.
//!
//! Three independent bindings are required, all of them exact:
//!
//! 1. **OIDC issuer** (Fulcio OID `1.3.6.1.4.1.57264.1.1`) is GitHub Actions'
//!    token issuer. Without this, any Sigstore identity from any issuer that
//!    happened to mention our repository would pass.
//! 2. **Workflow repository** (Fulcio OID `1.3.6.1.4.1.57264.1.5`) is exactly
//!    `paulirotta/ahma`.
//! 3. **Subject Alternative Name** — the `build_signer_uri` — starts with
//!    `https://github.com/paulirotta/ahma/`, so the signing workflow lives in
//!    our repository rather than merely being run by it.
//!
//! The workflow *file* and *ref* are deliberately not pinned: the previous
//! implementation passed `signer_workflow: None`, so pinning them here would
//! reject already-published releases built by a differently-named workflow.
//! Everything else is stricter than what came before — `sigstore-verification`
//! checked only that the certificate's issuer common name looked like Fulcio's
//! and never bound the certificate to `paulirotta/ahma` at all.

use sigstore::bundle::verify::policy::{
    AllOf, GitHubWorkflowRepository, OIDCIssuer, PolicyError, VerificationPolicy,
};
use x509_cert::Certificate;
use x509_cert::ext::pkix::{SubjectAltName, name::GeneralName};

/// The OIDC issuer every GitHub Actions Fulcio certificate carries.
pub(crate) const GITHUB_ACTIONS_OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";

/// Requires at least one SAN URI under `https://github.com/{owner}/{repo}/`.
///
/// `sigstore`'s own [`sigstore::bundle::verify::policy::Identity`] matches a SAN
/// *exactly*, which would mean pinning the workflow file and git ref. This is the
/// prefix form: it binds the signer URI to our repository while leaving the
/// workflow file and ref unpinned, exactly as before.
pub(crate) struct SignerUriUnderRepository {
    prefix: String,
}

impl SignerUriUnderRepository {
    pub(crate) fn new(owner: &str, repo: &str) -> Self {
        Self {
            // Trailing slash matters: without it `paulirotta/ahma-evil` would
            // also match.
            prefix: format!("https://github.com/{owner}/{repo}/"),
        }
    }
}

impl VerificationPolicy for SignerUriUnderRepository {
    fn verify(&self, cert: &Certificate) -> Result<(), PolicyError> {
        let (_, san): (bool, SubjectAltName) = match cert.tbs_certificate.get() {
            Ok(Some(result)) => result,
            _ => return Err(PolicyError::ExtensionNotFound),
        };

        let uris: Vec<&str> = san
            .0
            .iter()
            .filter_map(|name| match name {
                GeneralName::UniformResourceIdentifier(uri) => Some(uri.as_str()),
                _ => None,
            })
            .collect();

        if uris.iter().any(|uri| uri.starts_with(&self.prefix)) {
            return Ok(());
        }
        Err(PolicyError::ExtensionCheckFailed {
            extension: "SubjectAltName".to_owned(),
            expected: format!("{}*", self.prefix),
            actual: uris.join(", "),
        })
    }
}

/// The three bindings, held together so the caller can borrow them as one policy.
pub(crate) struct AhmaReleaseIdentity {
    issuer: OIDCIssuer,
    repository: GitHubWorkflowRepository,
    signer_uri: SignerUriUnderRepository,
}

impl AhmaReleaseIdentity {
    pub(crate) fn new(owner: &str, repo: &str) -> Self {
        Self {
            issuer: OIDCIssuer(GITHUB_ACTIONS_OIDC_ISSUER.to_owned()),
            repository: GitHubWorkflowRepository(format!("{owner}/{repo}")),
            signer_uri: SignerUriUnderRepository::new(owner, repo),
        }
    }

    /// Borrow the three checks as a single "all of" policy.
    ///
    /// `AllOf::new` returns `None` only for an empty child list, which cannot
    /// happen here.
    pub(crate) fn as_policy(&self) -> AllOf<'_> {
        AllOf::new([
            &self.issuer as &dyn VerificationPolicy,
            &self.repository as &dyn VerificationPolicy,
            &self.signer_uri as &dyn VerificationPolicy,
        ])
        .expect("three child policies is never empty")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use x509_cert::der::Decode;

    /// The leaf certificate from the real ahma v0.19.7 build-provenance bundle.
    /// SAN: `https://github.com/paulirotta/ahma/.github/workflows/build.yml@refs/heads/main`.
    const BUNDLE: &str = include_str!("../../tests/fixtures/ahma_build_provenance_bundle.json");

    fn real_certificate() -> Certificate {
        let bundle: serde_json::Value =
            serde_json::from_str(BUNDLE).expect("fixture bundle must be valid JSON");
        let raw = bundle["verificationMaterial"]["certificate"]["rawBytes"]
            .as_str()
            .expect("fixture must carry a leaf certificate");
        let der = BASE64.decode(raw).expect("certificate must be base64");
        Certificate::from_der(&der).expect("certificate must be valid DER")
    }

    #[test]
    fn accepts_the_real_ahma_release_certificate() {
        let identity = AhmaReleaseIdentity::new("paulirotta", "ahma");
        identity
            .as_policy()
            .verify(&real_certificate())
            .expect("the official release certificate must satisfy the policy");
    }

    #[test]
    fn rejects_a_different_repository() {
        let identity = AhmaReleaseIdentity::new("paulirotta", "not-ahma");
        let err = identity
            .as_policy()
            .verify(&real_certificate())
            .expect_err("a certificate for another repository must be rejected");
        assert!(
            err.to_string().contains("policies failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_a_different_owner() {
        let identity = AhmaReleaseIdentity::new("attacker", "ahma");
        identity
            .as_policy()
            .verify(&real_certificate())
            .expect_err("a certificate for another owner must be rejected");
    }

    #[test]
    fn rejects_a_different_oidc_issuer() {
        let issuer = OIDCIssuer("https://accounts.google.com".to_owned());
        issuer
            .verify(&real_certificate())
            .expect_err("a certificate from another OIDC issuer must be rejected");
    }

    #[test]
    fn accepts_the_github_actions_oidc_issuer() {
        let issuer = OIDCIssuer(GITHUB_ACTIONS_OIDC_ISSUER.to_owned());
        issuer
            .verify(&real_certificate())
            .expect("the release certificate is issued to GitHub Actions' OIDC identity");
    }

    #[test]
    fn signer_uri_prefix_is_not_satisfied_by_a_sibling_repository() {
        // `paulirotta/ahma-evil` must not match the `paulirotta/ahma` prefix.
        let policy = SignerUriUnderRepository::new("paulirotta", "ahma-evil");
        policy
            .verify(&real_certificate())
            .expect_err("a sibling repository name must not match");
    }

    #[test]
    fn signer_uri_prefix_accepts_the_real_certificate() {
        SignerUriUnderRepository::new("paulirotta", "ahma")
            .verify(&real_certificate())
            .expect("the release certificate's SAN is under the repository");
    }

    #[test]
    fn signer_uri_prefix_ends_with_a_slash() {
        // Guards the trailing-slash invariant that makes the sibling-repository
        // test above meaningful.
        assert_eq!(
            SignerUriUnderRepository::new("o", "r").prefix,
            "https://github.com/o/r/"
        );
    }
}
