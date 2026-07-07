use anyhow::Result;
use std::path::Path;

use super::core::Sandbox;

impl Sandbox {
    pub(super) fn build_macos_sandbox_command(
        &self,
        command: &[String],
        working_dir: &Path,
    ) -> Result<(String, Vec<String>)> {
        let profile = self.generate_seatbelt_profile(working_dir);

        let mut args = vec!["-p".to_string(), profile];
        args.extend(command.iter().cloned());

        Ok(("sandbox-exec".to_string(), args))
    }

    /// Expose the seatbelt profile string for test inspection.
    /// Hidden from docs — not part of the public API contract.
    #[doc(hidden)]
    pub fn generate_seatbelt_profile_test(&self, working_dir: &Path) -> String {
        self.generate_seatbelt_profile(working_dir)
    }

    fn generate_seatbelt_profile(&self, working_dir: &Path) -> String {
        let wd_str = working_dir.to_string_lossy();
        let scope_rules = self.get_macos_scope_rules();
        let read_scopes_rules = self.get_macos_read_scopes_rules();
        let system_rules = self.get_macos_system_rules();
        let credential_deny_rules = self.get_macos_credential_deny_rules();
        let keychain_rules = self.get_macos_keychain_rules();
        let user_tool_rules = self.get_macos_user_tool_rules();
        let temp_rules = self.get_macos_temp_rules();
        let pkg_cache_rules = self.get_macos_package_cache_write_rules();
        let network_rules = self.get_macos_network_rules();

        let profile = format!(
            r#"(version 1)
(deny default)
(allow process*)
(allow signal)
(allow sysctl-read)
{system_rules}{credential_deny_rules}{keychain_rules}{user_tool_rules}{scope_rules}{read_scopes_rules}(allow file-read* (subpath "{working_dir}"))
(allow file-write* (subpath "{working_dir}"))
{pkg_cache_rules}{temp_rules}(allow file-read* (literal "/dev/null"))
(allow file-write* (literal "/dev/null"))
(allow file-read* (literal "/dev/tty"))
(allow file-write* (literal "/dev/tty"))
(allow file-read* (literal "/dev/zero"))
(allow file-write* (literal "/dev/zero"))
{network_rules}(allow mach-lookup)
(allow ipc-posix-shm*)
"#,
            working_dir = wd_str,
            system_rules = system_rules,
            credential_deny_rules = credential_deny_rules,
            keychain_rules = keychain_rules,
            user_tool_rules = user_tool_rules,
            scope_rules = scope_rules,
            read_scopes_rules = read_scopes_rules,
            pkg_cache_rules = pkg_cache_rules,
            temp_rules = temp_rules,
            network_rules = network_rules,
        );

        tracing::debug!("Generated macOS Sandbox (Seatbelt) profile:\n{}", profile);
        profile
    }

    fn get_macos_scope_rules(&self) -> String {
        let mut rules = String::new();
        for scope in self.scopes.read().unwrap().iter() {
            rules.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n(allow file-write* (subpath \"{}\"))\n",
                scope.display(),
                scope.display()
            ));
        }
        rules
    }

    /// Network rules (R-NET enforcement). With no egress proxy configured, the
    /// blanket `(allow network*)` is emitted (advisory tier / restriction off).
    /// With a proxy address set (`--restrict-network`), all outbound IP egress is
    /// denied *except* the proxy — so a sandboxed subprocess can only reach the
    /// network through the allow-listed, SSRF-guarded proxy. Uses last-match-wins
    /// SBPL semantics: start from `(allow network*)` (keeps unix sockets, mach,
    /// local binds, DNS-via-mDNSResponder working), deny all outbound IP, then
    /// re-allow the single proxy address. `network-inbound`/`bind` and unix-socket
    /// egress are intentionally left permitted (local IPC cannot exfiltrate off the
    /// host on its own).
    fn get_macos_network_rules(&self) -> String {
        match *self.egress_proxy_addr.read().unwrap() {
            None => "(allow network*)\n".to_string(),
            Some(addr) => format!(
                "(allow network*)\n\
                 (deny network-outbound (remote ip \"*:*\"))\n\
                 (allow network-outbound (remote ip \"{}:{}\"))\n",
                addr.ip(),
                addr.port()
            ),
        }
    }

    fn get_macos_read_scopes_rules(&self) -> String {
        let mut rules = String::new();
        for scope in &self.read_scopes() {
            rules.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                scope.display()
            ));
        }
        rules
    }

    /// `(deny file-read* …)` rules for the operator's credential-read deny set.
    /// Emitted right after the global `(allow file-read*)` so they override it,
    /// but before the workspace-scope allows so an explicit scope grant still
    /// wins (SBPL is last-match-wins).
    ///
    /// Each path is canonicalized before being written into the profile —
    /// unlike `self.scopes` (canonicalized once in `Sandbox::new` via
    /// `scopes::canonicalize_scopes`), this deny set is a free-standing global
    /// installed independently of sandbox construction, so nothing else
    /// resolves symlinks in it first. On macOS `/tmp` and `/var` are symlinks
    /// to `/private/tmp` and `/private/var`; Seatbelt's kernel-side subpath
    /// matcher resolves against the canonical vnode, so a `(deny … (subpath
    /// "/var/folders/…"))` rule silently fails to match a read of
    /// `/private/var/folders/…` — the deny is emitted but never fires. This
    /// bit `test_credential_read_deny_is_kernel_enforced`, whose test fixture
    /// lived under the symlinked temp dir. Falls back to the raw path if
    /// canonicalization fails (e.g. the directory doesn't exist yet) rather
    /// than dropping the rule.
    fn get_macos_credential_deny_rules(&self) -> String {
        let mut rules = String::new();
        for deny in super::credential_reads::credential_read_denies() {
            let canonical = dunce::canonicalize(&deny).unwrap_or(deny);
            rules.push_str(&format!(
                "(deny file-read* (subpath \"{}\"))\n",
                canonical.display()
            ));
        }
        rules
    }

    /// Keychain access rules, gated by `[sandbox] allow_keychain` (default on;
    /// see [`super::credential_reads::keychain_access_allowed`]).
    ///
    /// When allowed, sandboxed tools need to *write* the login keychain (default
    /// deny blocks writes; the working dir doesn't cover `~/Library/Keychains`) and
    /// read/write the `com.apple.security*` preference plists. **Reads** of the
    /// keychain already work via the global `(allow file-read*)` because keychain
    /// is not in the credential-read deny set when this toggle is on. mach access
    /// to `securityd` / `SecurityServer` is already granted by the blanket
    /// `(allow mach-lookup)` in the base profile.
    ///
    /// This is the fix for `gh auth login` (and `git-credential-osxkeychain`)
    /// silently breaking under the sandbox: the OAuth token was written somewhere
    /// gh could not read back. When the toggle is off, no rule is emitted and the
    /// startup wiring instead re-adds `~/Library/Keychains` to the read-deny set.
    ///
    /// The keychain dir is canonicalized (matching the deny rules) so the kernel
    /// subpath matcher — which resolves against the canonical vnode — fires; falls
    /// back to the raw path if canonicalization fails.
    fn get_macos_keychain_rules(&self) -> String {
        if !super::credential_reads::keychain_access_allowed() {
            return String::new();
        }
        let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/Users/Shared".to_string());
        let keychains = std::path::Path::new(&home_dir).join("Library/Keychains");
        let keychains = dunce::canonicalize(&keychains).unwrap_or(keychains);
        format!(
            "(allow file-write* (subpath \"{}\"))\n\
             (allow file-read* file-write* (regex #\"^.*/Library/Preferences/com\\.apple\\.security.*\\.plist$\"))\n",
            keychains.display()
        )
    }

    fn get_macos_system_rules(&self) -> String {
        // Use a global file-read* rule (no path qualifier) because macOS seatbelt
        // on Apple Silicon / macOS 26+ cannot reliably match specific subpaths for
        // reads — the APFS firmlink / cryptex volume structure means bash and dyld
        // access paths whose resolved vnodes don't match any traditional /usr,
        // /System, etc. prefix.  Write access remains tightly scoped to the
        // working directory and temp directories.
        "(allow file-read*)\n".to_string()
    }

    fn get_macos_user_tool_rules(&self) -> String {
        let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/Users/Shared".to_string());
        let home_path = std::path::Path::new(&home_dir);
        let mut rules = String::new();

        let tool_paths = [".cargo", ".rustup"];
        for tool in &tool_paths {
            let path = home_path.join(tool);
            if path.exists() {
                rules.push_str(&format!(
                    "(allow file-read* (subpath \"{}\"))\n",
                    path.display()
                ));
            }
        }
        rules
    }

    /// Generate macOS Seatbelt write rules for package-manager caches.
    ///
    /// When `package_cache_write` is enabled (the default), grants `file-write*`
    /// for `registry/`, `git/`, and the two cargo lock files.  Sensitive paths
    /// (`bin/`, `config.toml`, `credentials.toml`) receive no write rule and
    /// remain under the existing global `file-read*` rule only.
    fn get_macos_package_cache_write_rules(&self) -> String {
        if !self.package_cache_write {
            return String::new();
        }

        use super::pkg_cache::{all_writable_package_cache_paths, pre_create_package_cache_paths};

        let mut rules = String::new();
        for cache in all_writable_package_cache_paths() {
            pre_create_package_cache_paths(&cache);

            for dir in &cache.writable_dirs {
                rules.push_str(&format!(
                    "(allow file-write* (subpath \"{}\"))\n",
                    dir.display()
                ));
            }
            for file in &cache.writable_files {
                if file.exists() {
                    rules.push_str(&format!(
                        "(allow file-write* (literal \"{}\"))\n",
                        file.display()
                    ));
                }
            }
        }
        rules
    }

    fn get_macos_temp_rules(&self) -> String {
        if self.no_temp_files {
            String::new()
        } else {
            "(allow file-read* (subpath \"/private/tmp\"))\n\
             (allow file-write* (subpath \"/private/tmp\"))\n\
             (allow file-read* (subpath \"/private/var/folders\"))\n\
             (allow file-write* (subpath \"/private/var/folders\"))\n"
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::core::Sandbox;
    use super::super::types::SandboxMode;
    use tempfile::tempdir;

    /// The generated Seatbelt profile keeps the blanket network allow when no
    /// egress proxy is configured (restriction off / advisory tier), and switches
    /// to deny-all-outbound-IP-except-the-proxy when `--restrict-network` set the
    /// proxy address (R-NET enforcement).
    #[test]
    fn network_rules_reflect_egress_proxy_addr() {
        let dir = tempdir().unwrap();
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();

        // No proxy → blanket allow, no deny rule.
        let p = sb.generate_seatbelt_profile_test(dir.path());
        assert!(p.contains("(allow network*)"), "default keeps network open");
        assert!(
            !p.contains("(deny network-outbound"),
            "no deny rule without restrict-network"
        );

        // Proxy set → deny all outbound IP, re-allow only the proxy address.
        sb.set_egress_proxy_addr(Some("127.0.0.1:34567".parse().unwrap()));
        let p = sb.generate_seatbelt_profile_test(dir.path());
        assert!(
            p.contains("(deny network-outbound (remote ip \"*:*\"))"),
            "enforcement must deny all outbound IP egress, got:\n{p}"
        );
        assert!(
            p.contains("(allow network-outbound (remote ip \"127.0.0.1:34567\"))"),
            "enforcement must re-allow exactly the proxy address, got:\n{p}"
        );

        // Clearing the address returns to the open policy.
        sb.set_egress_proxy_addr(None);
        let p = sb.generate_seatbelt_profile_test(dir.path());
        assert!(p.contains("(allow network*)"));
        assert!(!p.contains("(deny network-outbound"));
    }

    /// Regression test for the bug behind `test_credential_read_deny_is_kernel_enforced`
    /// flaking on CI: `/tmp` is a symlink to `/private/tmp` on macOS, and Seatbelt's
    /// kernel-side subpath matcher resolves against the canonical vnode — a
    /// `(deny … (subpath "/tmp/…"))` rule silently never matches a read of
    /// `/private/tmp/…`, so the deny is emitted but never fires. The emitted rule
    /// must use the canonicalized path.
    #[test]
    fn credential_deny_rule_uses_canonical_path_not_symlink() {
        use super::super::credential_reads::set_credential_read_denies;

        let dir = tempdir().unwrap();
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();

        let symlinked_deny = std::path::PathBuf::from("/tmp");
        set_credential_read_denies(vec![symlinked_deny.clone()]);
        let profile = sb.generate_seatbelt_profile_test(dir.path());
        set_credential_read_denies(Vec::new());

        let canonical = dunce::canonicalize(&symlinked_deny).expect("/tmp must resolve on macOS");
        assert!(
            profile.contains(&format!(
                "(deny file-read* (subpath \"{}\"))",
                canonical.display()
            )),
            "deny rule must use the canonicalized path, got:\n{profile}"
        );
        assert!(
            !profile.contains("(deny file-read* (subpath \"/tmp\"))"),
            "deny rule must not use the raw symlinked path, got:\n{profile}"
        );
    }

    /// With keychain access on (the default), the profile grants keychain writes
    /// and the security-prefs plist, but emits no keychain read-deny — so `gh` and
    /// other Keychain-backed tools work. Reads already flow through the global
    /// `(allow file-read*)`.
    #[test]
    fn keychain_rules_emitted_when_allowed() {
        use super::super::credential_reads::set_keychain_access_allowed;

        let dir = tempdir().unwrap();
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();

        set_keychain_access_allowed(true);
        let profile = sb.generate_seatbelt_profile_test(dir.path());
        set_keychain_access_allowed(false);

        assert!(
            profile.contains("Library/Keychains"),
            "keychain write allow must be present when allowed, got:\n{profile}"
        );
        assert!(
            profile.contains("(allow file-write* (subpath") && profile.contains("Keychains\"))"),
            "keychain dir must get a file-write* allow, got:\n{profile}"
        );
        assert!(
            profile.contains("com\\.apple\\.security"),
            "security prefs plist allow must be present, got:\n{profile}"
        );
    }

    /// With keychain access off, no keychain allow rule is emitted (the startup
    /// wiring separately re-adds `~/Library/Keychains` to the read-deny set).
    #[test]
    fn keychain_rules_absent_when_disallowed() {
        use super::super::credential_reads::set_keychain_access_allowed;

        let dir = tempdir().unwrap();
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();

        set_keychain_access_allowed(false);
        let profile = sb.generate_seatbelt_profile_test(dir.path());

        assert!(
            !profile.contains("com\\.apple\\.security"),
            "no security-prefs allow when keychain access disabled, got:\n{profile}"
        );
        assert!(
            !profile.contains("Keychains"),
            "no keychain allow rule when disabled, got:\n{profile}"
        );
    }

    /// A deny path that doesn't exist (so it can't be canonicalized) must still be
    /// emitted, falling back to the raw path rather than being silently dropped —
    /// dropping it would be a fail-open regression of the credential deny list.
    #[test]
    fn credential_deny_rule_falls_back_to_raw_path_when_uncanonicalizable() {
        use super::super::credential_reads::set_credential_read_denies;

        let dir = tempdir().unwrap();
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();

        let missing = std::path::PathBuf::from("/no/such/path/ever-XYZ123");
        set_credential_read_denies(vec![missing.clone()]);
        let profile = sb.generate_seatbelt_profile_test(dir.path());
        set_credential_read_denies(Vec::new());

        assert!(
            profile.contains(&format!(
                "(deny file-read* (subpath \"{}\"))",
                missing.display()
            )),
            "an uncanonicalizable deny path must still be emitted (raw), got:\n{profile}"
        );
    }
}
