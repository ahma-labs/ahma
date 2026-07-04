//! Heuristic detection of a *non-path* capability denial in a failed command's
//! output, plus the actionable two-door disclosure for it.
//!
//! ## Why this exists
//!
//! ahma's sandbox scope model is filesystem-path-centric: [`super::denial_scan`]
//! finds an out-of-scope *path* and the grant flow offers to widen scope to it.
//! But a sandbox can also block access to resources that are **not paths at
//! all** — most commonly the OS credential store (macOS Keychain, the
//! Freedesktop Secret Service / gnome-keyring, Windows Credential Manager). When
//! that happens the failure usually surfaces as an opaque, misleading error and
//! there is no path to grant, so the filesystem grant flow cannot help. The user
//! is left staring at a bare `401` or `errSecInteractionNotAllowed` with no hint
//! that a sandbox was even involved.
//!
//! This module recognizes that class from a command's stderr and turns it into a
//! clear disclosure that (a) names the capability, (b) says a sandbox was
//! involved, and (c) offers **both** doors — *stay in the loop* (no trust
//! required) and *grant* (only when meaningful) — with the default being to stay
//! blocked. It is the credential-store analogue of the filesystem grant hint.
//!
//! ## What it does and does NOT do
//!
//! - It **never** grants anything and **never** runs a command unsandboxed. It
//!   only converts an opaque failure into an actionable message.
//! - It matches only **high-confidence** signatures — errors that are
//!   specifically emitted when a *non-interactive/sandboxed* process is refused
//!   the credential store. It deliberately does **not** match a bare `HTTP 401`
//!   or "no token found": tools like `gh` swallow the keychain failure and print
//!   an error indistinguishable from a genuine logout, so matching those would
//!   cry wolf on every real auth failure. Catching that swallowed-error class
//!   reliably needs a capability-aware command registry (future work); this
//!   module intentionally stays precise instead.
//!
//! Same safety properties as [`super::denial_scan`]: plain line scanning (no
//! regex, so no pathological backtracking), returns at most **one** capability,
//! and a command that prints a *fake* signature can at worst produce an
//! informational message — never an escalation.

/// A non-path resource an OS sandbox can deny a process access to.
///
/// Currently only the credential store is modelled; the enum exists so new
/// capability classes (e.g. a Mach service, the system keychain vs. login
/// keychain) can be added without changing call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// The OS credential/secret store: macOS Keychain, the Freedesktop Secret
    /// Service (gnome-keyring / KWallet via libsecret), or Windows Credential
    /// Manager.
    CredentialStore,
}

impl Capability {
    /// A short human name for the capability, used in the disclosure.
    fn label(self) -> &'static str {
        match self {
            Capability::CredentialStore => "the OS credential store (e.g. macOS Keychain)",
        }
    }
}

/// Which sandbox is actually enforcing, so the disclosure can be honest about
/// whether *ahma* can do anything about the denial.
///
/// When ahma is nested inside another sandbox (Cursor, VS Code, Docker, a CI
/// container), ahma is **not** the layer blocking the credential store — the
/// outer sandbox is — so no ahma-side grant can help and the disclosure must say
/// so rather than offer a lever ahma does not control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcingLayer {
    /// ahma applies its own kernel sandbox and is the layer denying the access.
    Ahma,
    /// A host sandbox (Cursor/VS Code/Docker/CI) wraps ahma; ahma cannot widen it.
    HostSandbox,
    /// Enforcement source is unknown; give the platform-neutral guidance.
    Unknown,
}

/// Scan `stderr` for a high-confidence credential-store denial signature.
///
/// Returns `Some(Capability::CredentialStore)` only for errors that a
/// sandboxed/non-interactive process gets specifically because it was refused
/// the credential store — never for an ambiguous auth failure. Returns `None`
/// when nothing matches.
pub fn scan_capability_denial(stderr: &str) -> Option<Capability> {
    let lower = stderr.to_ascii_lowercase();
    if is_credential_store_denial(&lower) {
        return Some(Capability::CredentialStore);
    }
    None
}

/// High-confidence signatures that a credential/secret store was refused because
/// the caller is sandboxed or non-interactive. All comparisons are against an
/// already-lowercased haystack.
fn is_credential_store_denial(lower: &str) -> bool {
    // macOS Security framework: the canonical error a non-interactive/sandboxed
    // process gets from the Keychain. `errSecInteractionNotAllowed` (-25308) is
    // returned when a keychain item exists but UI/authorization is not permitted
    // — exactly the sandboxed-read case.
    const MACOS: &[&str] = &[
        "errsecinteractionnotallowed",
        "-25308",
        "user interaction is not allowed",
        "interaction not allowed",
    ];
    // Freedesktop Secret Service / gnome-keyring / libsecret: raised when the
    // secret service cannot be reached (no session bus / blocked), which is how
    // a sandboxed Linux process is refused stored credentials.
    const LINUX: &[&str] = &[
        "org.freedesktop.secrets",
        "secret service",
        "secretservice",
        "was not provided by any .service files",
        "cannot autolaunch d-bus",
        "no such secret collection",
    ];
    // Windows Credential Manager / DPAPI: access refused reading a stored
    // credential. Kept specific so a generic "access is denied" (already handled
    // by the filesystem path scanner) does not double-fire here.
    const WINDOWS: &[&str] = &[
        "credential manager",
        "credread",
        "the specified credentials could not be read",
        "keyset does not exist",
    ];

    MACOS.iter().any(|s| lower.contains(s))
        || LINUX.iter().any(|s| lower.contains(s))
        || WINDOWS.iter().any(|s| lower.contains(s))
}

/// Build the two-door disclosure for a detected capability denial.
///
/// The message always names the capability and states the default (blocked). The
/// *grant* door is phrased honestly for the enforcing layer: when a host sandbox
/// is in charge, it says ahma cannot widen it and points only at the
/// stay-in-the-loop door; when ahma is authoritative, it offers the real
/// ahma-side action. It never references a command that does not exist.
pub fn capability_denial_disclosure(cap: Capability, layer: EnforcingLayer) -> String {
    let mut msg = String::new();
    msg.push_str(&format!(
        "✗ Blocked: this command tried to use {cap} that the sandbox denies.\n\n",
        cap = cap.label(),
    ));
    msg.push_str(
        "This is usually not a login problem — the credential often exists, but the \
         sandboxed process is not allowed to read the secret store.\n\n",
    );

    msg.push_str("Two ways forward:\n");
    // Door 1 — always available, requires no trust in ahma. This is the
    // legitimate human-in-the-loop choice and is offered first on purpose.
    msg.push_str(
        "  • Stay in the loop (no trust in ahma required): run the command yourself \
         outside ahma, OR provide the credential in a way that does not touch the OS \
         secret store — e.g. an environment variable or a plaintext token file your \
         tool supports (for gh: `export GH_TOKEN=…`; for git: a credential-store helper; \
         for aws: `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`).\n",
    );
    // Door 2 — the "grant" door, phrased for who actually enforces.
    match layer {
        EnforcingLayer::Ahma => msg.push_str(
            "  • Grant it (only if you trust this run): restart ahma for this session with \
             `--no-sandbox` (the credential store becomes reachable, but so does the rest \
             of the filesystem — use only when you trust the workspace), or launch ahma \
             from a shell where the credential is already exported so no store read is \
             needed.\n",
        ),
        EnforcingLayer::HostSandbox => msg.push_str(
            "  • Grant it: ahma is running INSIDE another sandbox (e.g. Cursor, VS Code, \
             Docker, or CI), which is the layer blocking the credential store — ahma \
             cannot widen it. Use the stay-in-the-loop option above, or relax the host \
             sandbox in that tool's own settings if you trust this run.\n",
        ),
        EnforcingLayer::Unknown => msg.push_str(
            "  • Grant it (only if you trust this run): make the credential available \
             without the OS secret store (an exported environment variable or a token \
             file), then re-run. If a host sandbox (Cursor/VS Code/Docker/CI) is in \
             effect, it — not ahma — is the layer to relax.\n",
        ),
    }

    msg.push_str("\nDefault if you do nothing: BLOCKED (ahma never runs it unsandboxed for you).");
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_interaction_not_allowed_is_credential_denial() {
        let stderr = "SecKeychain: User interaction is not allowed. (errSecInteractionNotAllowed)";
        assert_eq!(
            scan_capability_denial(stderr),
            Some(Capability::CredentialStore)
        );
    }

    #[test]
    fn macos_numeric_status_is_credential_denial() {
        assert_eq!(
            scan_capability_denial("Keychain error -25308 reading item"),
            Some(Capability::CredentialStore)
        );
    }

    #[test]
    fn linux_secret_service_unavailable_is_credential_denial() {
        let stderr = "failed to unlock collection: org.freedesktop.secrets was not provided by any .service files";
        assert_eq!(
            scan_capability_denial(stderr),
            Some(Capability::CredentialStore)
        );
    }

    #[test]
    fn windows_credential_read_failure_is_credential_denial() {
        assert_eq!(
            scan_capability_denial("CredRead failed: the specified credentials could not be read"),
            Some(Capability::CredentialStore)
        );
    }

    #[test]
    fn case_insensitive_match() {
        assert_eq!(
            scan_capability_denial("ERRSECINTERACTIONNOTALLOWED"),
            Some(Capability::CredentialStore)
        );
    }

    #[test]
    fn bare_401_is_not_matched() {
        // The gh swallowed-error case: indistinguishable from a real logout, so
        // it must NOT be auto-classified as a sandbox capability denial.
        assert_eq!(
            scan_capability_denial("HTTP 401: Requires authentication"),
            None
        );
        assert_eq!(
            scan_capability_denial("no oauth token found for github.com"),
            None
        );
    }

    #[test]
    fn ordinary_error_is_not_matched() {
        assert_eq!(
            scan_capability_denial("error[E0382]: borrow of moved value"),
            None
        );
        assert_eq!(scan_capability_denial(""), None);
    }

    #[test]
    fn generic_access_denied_alone_is_not_a_credential_denial() {
        // A bare filesystem "Access is denied" belongs to the path scanner, not
        // here — this module must not double-fire on it.
        assert_eq!(
            scan_capability_denial("open C:/x: Access is denied. (os error 5)"),
            None
        );
    }

    #[test]
    fn disclosure_names_capability_and_default_blocked() {
        let msg =
            capability_denial_disclosure(Capability::CredentialStore, EnforcingLayer::Unknown);
        assert!(msg.contains("credential store"));
        assert!(msg.contains("BLOCKED"));
        assert!(msg.contains("Stay in the loop"));
    }

    #[test]
    fn host_sandbox_disclosure_is_honest_that_ahma_cannot_grant() {
        let msg =
            capability_denial_disclosure(Capability::CredentialStore, EnforcingLayer::HostSandbox);
        assert!(
            msg.contains("ahma cannot widen it"),
            "must disclose that a nested ahma cannot grant: {msg}"
        );
    }

    #[test]
    fn ahma_authoritative_disclosure_offers_real_lever() {
        let msg = capability_denial_disclosure(Capability::CredentialStore, EnforcingLayer::Ahma);
        assert!(
            msg.contains("--no-sandbox"),
            "authoritative disclosure should offer the real ahma-side lever: {msg}"
        );
    }

    #[test]
    fn disclosure_never_references_a_fake_grant_command() {
        // Guard against re-introducing an invented `ahma capability grant …`.
        for layer in [
            EnforcingLayer::Ahma,
            EnforcingLayer::HostSandbox,
            EnforcingLayer::Unknown,
        ] {
            let msg = capability_denial_disclosure(Capability::CredentialStore, layer);
            assert!(
                !msg.contains("capability grant"),
                "disclosure must not reference a nonexistent command: {msg}"
            );
        }
    }
}
