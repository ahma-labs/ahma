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
        let profile_rules = self.get_macos_profile_rules();
        let temp_rules = self.get_macos_temp_rules();
        let network_rules = self.get_macos_network_rules();
        let exec_config_deny_rules = self.get_macos_exec_config_deny_rules(working_dir);

        let profile = format!(
            r#"(version 1)
(deny default)
(allow process*)
(allow signal)
(allow sysctl-read)
{system_rules}{credential_deny_rules}{keychain_rules}{profile_rules}{scope_rules}{read_scopes_rules}(allow file-read* (subpath "{working_dir}"))
(allow file-write* (subpath "{working_dir}"))
{temp_rules}(allow file-read* (literal "/dev/null"))
(allow file-write* (literal "/dev/null"))
(allow file-read* (literal "/dev/tty"))
(allow file-write* (literal "/dev/tty"))
(allow file-read* (literal "/dev/zero"))
(allow file-write* (literal "/dev/zero"))
{exec_config_deny_rules}{network_rules}(allow mach-lookup)
(allow ipc-posix-shm*)
"#,
            working_dir = wd_str,
            system_rules = system_rules,
            credential_deny_rules = credential_deny_rules,
            keychain_rules = keychain_rules,
            profile_rules = profile_rules,
            scope_rules = scope_rules,
            read_scopes_rules = read_scopes_rules,
            temp_rules = temp_rules,
            exec_config_deny_rules = exec_config_deny_rules,
            network_rules = network_rules,
        );

        tracing::debug!("Generated macOS Sandbox (Seatbelt) profile:\n{}", profile);
        profile
    }

    fn get_macos_scope_rules(&self) -> String {
        let mut rules = String::new();
        for scope in self.scopes.read().iter() {
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
    /// egress are intentionally left permitted.
    ///
    /// That permission used to be justified with "local IPC cannot exfiltrate off
    /// the host on its own". **That reasoning is wrong and is not why unix sockets
    /// stay open.** A container daemon socket is local IPC, and reaching it is a
    /// complete escape: ask `docker.sock` for a `--privileged` container with a
    /// host bind mount and a root process entirely outside this profile does the
    /// write — and can trivially exfiltrate from there. Blanket-denying unix
    /// sockets would break too much (mDNSResponder, `securityd`, editor IPC), so
    /// the real defence is per-socket and lives in
    /// [`Self::get_macos_exec_config_deny_rules`], which denies `file-read*` and
    /// `file-write*` on the known container sockets. That file-level deny is what
    /// actually stops it: on macOS `connect(2)` to a unix socket is classified as
    /// a *network* operation, so `(allow network*)` here would otherwise let the
    /// connection through no matter what the network rules said.
    fn get_macos_network_rules(&self) -> String {
        match *self.egress_proxy_addr.read() {
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

    /// Kernel enforcement for the "write a file the sandbox permits, let a
    /// trusted component outside the sandbox run it" escape class
    /// ([`super::exec_config`]), plus the two capabilities that make the same
    /// trick reachable without writing a config file at all.
    ///
    /// **Ordering is the whole point.** SBPL is last-match-wins, so these rules
    /// are emitted *after* `{scope_rules}`, after the working-directory
    /// `(allow file-write* (subpath …))`, and after the temp-dir allows — they
    /// are the last filesystem word spoken about these paths. Emitting them any
    /// earlier would let the workspace allow silently re-open them, which is
    /// exactly the kind of failure that produces a deny rule nobody notices is
    /// dead. `exec_config_deny_rules_come_after_workspace_allow` asserts the
    /// byte offsets.
    ///
    /// Three groups:
    ///
    /// 1. **Auto-executing config** — git hooks under every *resolved* git
    ///    directory (not the literal spelling `.git`, which
    ///    `git init --separate-git-dir` defeats) and `<workspace>/.ahma`. Either
    ///    can be dropped from this set by an operator escape hatch
    ///    (`[sandbox] allow_git_hooks` / `allow_project_tool_config`, both
    ///    default-off and both disclosed at startup) — the toggle lives in
    ///    [`super::exec_config::deny_write_globs`] so the kernel rule and the
    ///    write-tool guard can never disagree about what is permitted.
    /// 2. **Container daemon sockets** — see
    ///    [`Self::get_macos_network_rules`] for why a file-level deny, not a
    ///    network rule, is what stops this.
    /// 3. **SSH private keys** — `~/.ssh` stays readable so `known_hosts` and
    ///    `config` work, but `id_*` is denied and `id_*.pub` re-allowed
    ///    afterwards (last-match-wins again).
    ///
    /// Paths are canonicalized for the same reason as
    /// [`Self::get_macos_credential_deny_rules`]: Seatbelt's kernel-side subpath
    /// matcher resolves against the canonical vnode, so a rule naming `/tmp/…`
    /// never fires on a read of `/private/tmp/…`. Uncanonicalizable paths fall
    /// back to the raw path rather than being dropped — a deny that cannot be
    /// resolved yet must not fail open.
    fn get_macos_exec_config_deny_rules(&self, working_dir: &Path) -> String {
        let mut rules = String::new();

        // Canonicalize the root before deriving paths from it: `<ws>/.ahma`
        // usually does not exist yet, so it cannot be canonicalized itself, and a
        // rule naming `/tmp/ws/.ahma` would never fire against `/private/tmp/ws`.
        let root = dunce::canonicalize(working_dir).unwrap_or_else(|_| working_dir.to_path_buf());
        let git_dirs = super::exec_config::resolve_git_dirs(&root);
        for deny in super::exec_config::deny_write_globs(&root, &git_dirs) {
            let canonical = dunce::canonicalize(&deny).unwrap_or(deny);
            rules.push_str(&format!(
                "(deny file-write* (subpath \"{}\"))\n",
                canonical.display()
            ));
        }

        let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/Users/Shared".to_string());
        let home = std::path::Path::new(&home_dir);

        // Each socket gets both spellings. Canonicalizing the socket *node*
        // fails outright when the daemon isn't running — `/var/run/docker.sock`
        // would then be emitted raw, and never match, because `/var` is a
        // symlink to `/private/var` and the kernel matches the canonical vnode.
        // Canonicalizing the *parent directory* (which does exist) and rejoining
        // the file name yields a rule that fires the moment the daemon starts,
        // while keeping the raw spelling for the case where the parent itself is
        // absent. A socket deny that only works when the socket already exists
        // is a deny that fails open exactly when it matters.
        let mut emitted: Vec<std::path::PathBuf> = Vec::new();
        for sock in super::credential_reads::container_socket_denies(home) {
            let via_parent = sock.parent().and_then(|p| dunce::canonicalize(p).ok());
            let candidates = [
                Some(sock.clone()),
                via_parent.and_then(|p| sock.file_name().map(|f| p.join(f))),
            ];
            for candidate in candidates.into_iter().flatten() {
                if emitted.contains(&candidate) {
                    continue;
                }
                rules.push_str(&format!(
                    "(deny file-read* file-write* (literal \"{}\"))\n",
                    candidate.display()
                ));
                emitted.push(candidate);
            }
        }
        for re in super::credential_reads::container_socket_deny_regexes(home) {
            rules.push_str(&format!(
                "(deny file-read* file-write* (regex #\"{re}\"))\n"
            ));
        }

        rules.push_str(&format!(
            "(deny file-read* (regex #\"{}\"))\n(allow file-read* (regex #\"{}\"))\n",
            super::credential_reads::ssh_private_key_deny_regex(home),
            super::credential_reads::ssh_public_key_allow_regex(home),
        ));

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

    /// Emit the rules contributed by the enabled sandbox **profiles**
    /// (SPEC R-PERM.5) — what used to be two hard-coded path arrays plus a
    /// cargo-shaped writable set in `pkg_cache.rs`.
    ///
    /// On macOS the *read* rules are largely redundant: `get_macos_system_rules`
    /// already grants blanket `file-read*` because APFS firmlinks defeat read
    /// subpath matching (a platform limitation disclosed by
    /// [`profiles::macos_read_disclosure`]). They are emitted anyway, so the two
    /// backends express the same profile identically and a future macOS that can
    /// scope reads gets correct behavior for free rather than a silent hole.
    ///
    /// The *write* rules are the ones that matter here: without them `cargo add`
    /// fails inside the sandbox, and the obvious workaround — granting all of
    /// `~/.cargo` — would hand over `credentials.toml` and write access to every
    /// binary on the user's PATH.
    fn get_macos_profile_rules(&self) -> String {
        use super::profiles::{RuleKind, applicable_rules, enabled_profile_names};

        // Loaded once per process (see `enabled_profile_names`): sandbox
        // configuration cannot change mid-session, and this runs per spawn.
        let enabled = enabled_profile_names();
        let mut rules = String::new();

        for rule in applicable_rules(enabled, self.package_cache_write) {
            let target = match rule.kind {
                RuleKind::Dir => format!("(subpath \"{}\")", rule.path.display()),
                RuleKind::File => format!("(literal \"{}\")", rule.path.display()),
            };
            // Read and write go out as *separate* rules rather than one combined
            // `(allow file-read* file-write* …)`. Semantically identical, but this
            // is byte-for-byte the SBPL the hard-coded lists used to produce — which
            // keeps this a refactor of *where the paths come from*, not a rewrite of
            // what the kernel is told, and lets the existing profile-text tests go on
            // guarding it unchanged.
            rules.push_str(&format!("(allow file-read* {target})\n"));
            if rule.access.is_write() {
                rules.push_str(&format!("(allow file-write* {target})\n"));
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

    /// The exec-config denies are worthless unless they are the *last* word:
    /// SBPL is last-match-wins, so a `(deny file-write* (subpath ".git/hooks"))`
    /// emitted before `(allow file-write* (subpath <workspace>))` is emitted but
    /// dead. Assert on byte offsets, not presence.
    #[test]
    fn exec_config_deny_rules_come_after_workspace_allow() {
        let dir = tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();

        let sb = Sandbox::new(vec![root.clone()], SandboxMode::Test, false, false, false).unwrap();
        let profile = sb.generate_seatbelt_profile_test(&root);

        let workspace_allow = profile
            .find(&format!(
                "(allow file-write* (subpath \"{}\"))",
                root.display()
            ))
            .unwrap_or_else(|| panic!("workspace write allow missing:\n{profile}"));

        let hooks_deny = profile
            .find(&format!(
                "(deny file-write* (subpath \"{}\"))",
                root.join(".git/hooks").display()
            ))
            .unwrap_or_else(|| panic!("git hooks deny missing:\n{profile}"));
        assert!(
            hooks_deny > workspace_allow,
            "git-hooks deny must come AFTER the workspace allow ({hooks_deny} vs {workspace_allow}):\n{profile}"
        );

        let ahma_deny = profile
            .find(&format!(
                "(deny file-write* (subpath \"{}\"))",
                root.join(".ahma").display()
            ))
            .unwrap_or_else(|| panic!(".ahma deny missing:\n{profile}"));
        assert!(
            ahma_deny > workspace_allow,
            ".ahma deny must come AFTER the workspace allow:\n{profile}"
        );

        // …and after the temp-dir allows, which are emitted later still.
        if let Some(temp_allow) = profile.find("(allow file-write* (subpath \"/private/tmp\"))") {
            assert!(
                hooks_deny > temp_allow,
                "exec-config denies must outrank the temp allows too:\n{profile}"
            );
        }
    }

    /// The operator escape hatches must remove the **kernel** rule, not just the
    /// write-tool guard. If only the application layer relaxed, an
    /// `--allow-git-hooks` session would still fail with a bare
    /// `Operation not permitted` from the kernel — the worst possible outcome:
    /// an opt-in that appears to do nothing.
    #[test]
    fn escape_hatches_remove_their_exec_config_deny_rule_from_the_profile() {
        use super::super::exec_config::{HandoffAllowances, set_handoff_allowances};

        let dir = tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        let sb = Sandbox::new(vec![root.clone()], SandboxMode::Test, false, false, false).unwrap();

        let hooks_deny = format!(
            "(deny file-write* (subpath \"{}\"))",
            root.join(".git/hooks").display()
        );
        let ahma_deny = format!(
            "(deny file-write* (subpath \"{}\"))",
            root.join(".ahma").display()
        );

        // Default policy: both denied.
        set_handoff_allowances(HandoffAllowances::default());
        let profile = sb.generate_seatbelt_profile_test(&root);
        assert!(
            profile.contains(&hooks_deny),
            "default denies hooks:\n{profile}"
        );
        assert!(
            profile.contains(&ahma_deny),
            "default denies .ahma:\n{profile}"
        );

        // allow_git_hooks drops exactly the hooks rule.
        set_handoff_allowances(HandoffAllowances {
            git_hooks: true,
            project_tool_config: false,
        });
        let profile = sb.generate_seatbelt_profile_test(&root);
        assert!(
            !profile.contains(&hooks_deny),
            "allow_git_hooks must drop the kernel deny:\n{profile}"
        );
        assert!(
            profile.contains(&ahma_deny),
            "…and must not drop the .ahma deny:\n{profile}"
        );

        // allow_project_tool_config drops exactly the .ahma rule.
        set_handoff_allowances(HandoffAllowances {
            git_hooks: false,
            project_tool_config: true,
        });
        let profile = sb.generate_seatbelt_profile_test(&root);
        assert!(
            profile.contains(&hooks_deny),
            "allow_project_tool_config must not drop the hooks deny:\n{profile}"
        );
        assert!(
            !profile.contains(&ahma_deny),
            "allow_project_tool_config must drop the kernel deny:\n{profile}"
        );

        // Restore the safe default for any test sharing this process.
        set_handoff_allowances(HandoffAllowances::default());
        let profile = sb.generate_seatbelt_profile_test(&root);
        assert!(profile.contains(&hooks_deny));
        assert!(profile.contains(&ahma_deny));
    }

    /// Container daemon sockets are denied at the *file* level. A network rule
    /// cannot do this job: macOS classifies `connect(2)` to a unix socket as a
    /// network operation, so `(allow network*)` would let it through.
    #[test]
    fn container_daemon_sockets_are_denied_for_read_and_write() {
        let dir = tempdir().unwrap();
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();
        let profile = sb.generate_seatbelt_profile_test(dir.path());

        for sock in [
            "/var/run/docker.sock",
            "/run/docker.sock",
            "/var/run/containerd/containerd.sock",
            "/run/podman/podman.sock",
        ] {
            // The raw spelling is emitted unconditionally; a canonical-parent
            // spelling is emitted alongside it when the parent resolves. Both
            // matter — see the comment on the emission site.
            assert!(
                profile.contains(&format!(
                    "(deny file-read* file-write* (literal \"{sock}\"))"
                )),
                "{sock} must be denied for read and write:\n{profile}"
            );
            let sock_path = std::path::Path::new(sock);
            if let Some(parent) = sock_path.parent().and_then(|p| dunce::canonicalize(p).ok()) {
                let canonical = parent.join(sock_path.file_name().unwrap());
                assert!(
                    profile.contains(&format!(
                        "(deny file-read* file-write* (literal \"{}\"))",
                        canonical.display()
                    )),
                    "the canonical spelling of {sock} must be denied too:\n{profile}"
                );
            }
        }
        assert!(
            profile.contains(r"/\.colima/.*/docker\.sock$"),
            "colima socket regex missing:\n{profile}"
        );
        assert!(
            profile.contains(r"/\.lima/.*/sock$"),
            "lima socket regex missing:\n{profile}"
        );
    }

    /// SSH keeps working (`known_hosts`, `config` readable) but the private key
    /// bytes are denied, with the `.pub` re-allow emitted afterwards so
    /// last-match-wins lets public keys through.
    #[test]
    fn ssh_private_keys_denied_public_keys_reallowed_in_that_order() {
        let dir = tempdir().unwrap();
        let sb = Sandbox::new(
            vec![dir.path().to_path_buf()],
            SandboxMode::Test,
            false,
            false,
            false,
        )
        .unwrap();
        let profile = sb.generate_seatbelt_profile_test(dir.path());

        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/Shared".to_string());
        let deny =
            super::super::credential_reads::ssh_private_key_deny_regex(std::path::Path::new(&home));
        let allow =
            super::super::credential_reads::ssh_public_key_allow_regex(std::path::Path::new(&home));

        let deny_at = profile
            .find(&format!("(deny file-read* (regex #\"{deny}\"))"))
            .unwrap_or_else(|| panic!("ssh private-key deny missing:\n{profile}"));
        let allow_at = profile
            .find(&format!("(allow file-read* (regex #\"{allow}\"))"))
            .unwrap_or_else(|| panic!("ssh public-key re-allow missing:\n{profile}"));
        assert!(
            allow_at > deny_at,
            "the .pub re-allow must come after the deny (last-match-wins):\n{profile}"
        );
        // `~/.ssh` itself is not blanket-denied — git-over-ssh needs known_hosts.
        assert!(
            !profile.contains(&format!("(deny file-read* (subpath \"{home}/.ssh\"))")),
            "~/.ssh must not be denied wholesale:\n{profile}"
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
