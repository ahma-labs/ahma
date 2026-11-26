//! Every key of `~/.ahma/settings.toml`, as one enum (SPEC R-CFG2.1, R-CFG5.1,
//! R-CFG10.7).
//!
//! Each key's properties — its trust tier, the command that manages it if it
//! holds grants, whether its value is a secret, whether only a CLI flag may set
//! it, whether the setup view lists it as a switch — are decided in one row of
//! the table below. Before this, about ten hand-kept lists across the
//! workspace each named some of the keys, and a key added to one and not
//! another drifted silently (`tools.context_length` was refused from a
//! repository's file because one list missed it). A key is now added in one
//! place, and the row will not compile until its properties are chosen.
//!
//! The table is in the order `render_documented` writes the file, and a test
//! proves [`SettingKey::ALL`] equals the documented keys and the fields of
//! [`AhmaSettings`], in that order, so the enum cannot gain or lose a key the
//! settings file does not.

use crate::config::{AhmaSettings, SettingsTier};

/// What one key is, beyond its name. Built only through the constants and
/// `const fn`s below, so every row states its tier.
#[derive(Debug, Clone, Copy)]
struct KeySpec {
    tier: SettingsTier,
    /// The dedicated command for a ledger key (SPEC R-CFG10.7).
    ledger: Option<&'static str>,
    /// Never shown or logged in clear (SPEC R-CFG5.1).
    redacted: bool,
    /// A file value is ignored; only a flag sets it (SPEC R-CFG2.3).
    cli_only: bool,
    /// Listed by the setup view as a switch that is on, with this label
    /// (SPEC R-SETUP.4.1).
    switch: Option<&'static str>,
    /// The boolean value that means the switch is *on*: `true` for every
    /// `allow_*`-style key, `false` for a key that names the opt-out
    /// (`disable_temp`: temporary files are on while it is `false`).
    switch_on_when: bool,
}

/// A preference: a repository's `.ahma/settings.toml` may set it.
const PREFERENCE: KeySpec = KeySpec {
    tier: SettingsTier::Preference,
    ledger: None,
    redacted: false,
    cli_only: false,
    switch: None,
    switch_on_when: true,
};

/// Shapes the security boundary: only the user's own file (SPEC R-CFG2.2).
const SECURITY: KeySpec = KeySpec {
    tier: SettingsTier::Security,
    ..PREFERENCE
};

/// A ledger key: it holds grants, and `command` is how they change.
const fn ledger(command: &'static str) -> KeySpec {
    KeySpec {
        ledger: Some(command),
        ..SECURITY
    }
}

impl KeySpec {
    const fn redacted(self) -> Self {
        Self {
            redacted: true,
            ..self
        }
    }

    const fn cli_only(self) -> Self {
        Self {
            cli_only: true,
            ..self
        }
    }

    const fn switch(self, label: &'static str) -> Self {
        Self {
            switch: Some(label),
            ..self
        }
    }

    /// A switch whose key names the opt-out: the switch is on while the key
    /// is `false`, so turning it off writes `true`.
    const fn switch_on_when_false(self, label: &'static str) -> Self {
        Self {
            switch: Some(label),
            switch_on_when: false,
            ..self
        }
    }
}

const SANDBOX_GRANT: &str = "`ahma sandbox grant <path>` (and `ahma sandbox revoke`); it checks the path against the hard denylist";
const PERMISSIONS_LIST: &str = "`ahma permissions list` and `ahma permissions revoke`; grants are made when a request is approved";
const NETWORK_ALLOW: &str = "`ahma network allow <host>` / `ahma network revoke <host>`";
const OPEN_URL_GRANT: &str =
    "`ahma permissions grant open-url <site>` / `ahma permissions revoke open-url <site>`";
const EGRESS_ALLOW: &str = "`ahma egress allow <domain>` / `ahma egress revoke <domain>`";
const EGRESS_DENY: &str = "`ahma egress deny <domain>` / `ahma egress revoke <domain>`";

/// Declares [`SettingKey`] from one row per key: `Variant = table.field =>
/// spec`. The dotted name and the field read for `settings show` both come
/// from `table.field`, so they cannot name different settings.
macro_rules! setting_keys {
    ($($variant:ident = $table:ident . $field:ident => $spec:expr,)+) => {
        /// One key of the settings file, `table.field`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum SettingKey {
            $(
                #[doc = concat!("`", stringify!($table), ".", stringify!($field), "`")]
                $variant,
            )+
        }

        impl SettingKey {
            /// Every key, in the order the settings file lists them.
            pub const ALL: &'static [SettingKey] = &[$(SettingKey::$variant),+];

            /// `table.field`, the name `ahma settings set` takes.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => concat!(stringify!($table), ".", stringify!($field)),)+
                }
            }

            /// The `[table]` the key lives in.
            pub const fn table(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($table),)+
                }
            }

            /// The key within its table.
            pub const fn field(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($field),)+
                }
            }

            const fn spec(self) -> KeySpec {
                match self {
                    $(Self::$variant => $spec,)+
                }
            }

            /// The value in `s`, rendered with `{:?}`: what `ahma settings
            /// show` prints and the startup report compares (SPEC R-CFG5).
            pub fn debug_value(self, s: &AhmaSettings) -> String {
                match self {
                    $(Self::$variant => format!("{:?}", s.$table.$field),)+
                }
            }
        }
    };
}

setting_keys! {
    LmstudioBaseUrl = lmstudio.base_url => PREFERENCE,
    LmstudioModel = lmstudio.model => PREFERENCE,
    ToolsTimeoutSecs = tools.timeout_secs => PREFERENCE,
    ToolsBackgroundTimeoutSecs = tools.background_timeout_secs => PREFERENCE,
    ToolsAwaitTimeoutSecs = tools.await_timeout_secs => PREFERENCE,
    ToolsIdleTimeoutSecs = tools.idle_timeout_secs => PREFERENCE,
    ToolsRequestBudgetOverrideSecs = tools.request_budget_override_secs => PREFERENCE,
    ToolsForceProgressNotifications = tools.force_progress_notifications => PREFERENCE,
    ToolsExecutionMode = tools.execution_mode => PREFERENCE,
    ToolsSkipProbes = tools.skip_probes => PREFERENCE,
    ToolsToolsDir = tools.tools_dir => PREFERENCE,
    ToolsToolBundles = tools.tool_bundles => PREFERENCE,
    ToolsMinimizeTokens = tools.minimize_tokens => PREFERENCE,
    ToolsSmallModelHarness = tools.small_model_harness => PREFERENCE,
    ToolsContextLength = tools.context_length => PREFERENCE,
    ToolsMaxTurns = tools.max_turns => PREFERENCE,
    ToolsMutexGroups = tools.mutex_groups => PREFERENCE,
    ToolsWorkspaceQueue = tools.workspace_queue => PREFERENCE,
    ToolsEditGuard = tools.edit_guard => PREFERENCE,
    ToolsSourceReaders = tools.source_readers => PREFERENCE,
    // R2.7.10: a declared long-running command runs without the workspace
    // lease, beside every other writer; a cloned repository must not be able
    // to declare its own.
    ToolsLongRunning = tools.long_running => SECURITY,
    SandboxDisable = sandbox.disable => SECURITY.cli_only(),
    SandboxTmpAccess = sandbox.tmp_access => SECURITY,
    // R5.2.5: temp is on by default; the key is the expert opt-out, so the
    // setup view's switch reads it inverted.
    SandboxDisableTemp = sandbox.disable_temp
        => SECURITY.switch_on_when_false("temporary files"),
    SandboxDefer = sandbox.defer => SECURITY,
    SandboxTaskVault = sandbox.task_vault => SECURITY,
    SandboxScopes = sandbox.scopes => SECURITY,
    SandboxWorkingDirs = sandbox.working_dirs => SECURITY,
    SandboxPackageCacheWrite = sandbox.package_cache_write => SECURITY,
    SandboxContainerRoot = sandbox.container_root => SECURITY,
    SandboxScratchDirectory = sandbox.scratch_directory => SECURITY,
    SandboxUseScratchDirectory = sandbox.use_scratch_directory => SECURITY,
    SandboxPersistentScopes = sandbox.persistent_scopes => ledger(SANDBOX_GRANT),
    SandboxSshSign = sandbox.ssh_sign => ledger(PERMISSIONS_LIST),
    SandboxSshAgent = sandbox.ssh_agent => SECURITY,
    SandboxEnvAllow = sandbox.env_allow => SECURITY,
    SandboxAllowKeychain = sandbox.allow_keychain => SECURITY.switch("the login keychain"),
    SandboxSignalOtherProcesses = sandbox.signal_other_processes
        => SECURITY.switch("signalling other processes"),
    SandboxAllowGpu = sandbox.allow_gpu => SECURITY.switch("the GPU (Metal)"),
    SandboxDenyCredentialReads = sandbox.deny_credential_reads => SECURITY,
    SandboxAllowCredentialReads = sandbox.allow_credential_reads => SECURITY,
    SandboxProfiles = sandbox.profiles => SECURITY,
    SandboxAllowGitHooks = sandbox.allow_git_hooks => SECURITY.switch("writing git hooks"),
    SandboxAllowProjectToolConfig = sandbox.allow_project_tool_config
        => SECURITY.switch("writing .ahma/ tool configs"),
    SandboxLinuxDenyTier = sandbox.linux_deny_tier => SECURITY,
    SandboxSyscallFilter = sandbox.syscall_filter => SECURITY,
    SandboxHomeReads = sandbox.home_reads => SECURITY,
    LoggingTarget = logging.target => PREFERENCE,
    LoggingLogMonitor = logging.log_monitor => PREFERENCE,
    LoggingMonitorRateLimitSecs = logging.monitor_rate_limit_secs => PREFERENCE,
    LoggingDir = logging.dir => PREFERENCE,
    HttpHandshakeTimeoutSecs = http.handshake_timeout_secs => PREFERENCE,
    HttpDisableQuic = http.disable_quic => PREFERENCE,
    HttpDisableHttp11 = http.disable_http1_1 => PREFERENCE,
    // R-CFG2.1 names session isolation and the socket path is what selects it:
    // a project file pointing the bridge at another socket redirects every tool
    // call to a server it chose.
    HttpUnixSocketPath = http.unix_socket_path => SECURITY,
    AuthRequireTokenPath = auth.require_token_path => SECURITY,
    AuthRequireToken = auth.require_token => SECURITY.redacted(),
    AuthRateLimitRps = auth.rate_limit_rps => SECURITY,
    AuthRateLimitBurst = auth.rate_limit_burst => SECURITY,
    InstanceLabel = instance.label => PREFERENCE,
    HubIdleTimeoutSecs = hub.idle_timeout_secs => PREFERENCE,
    HubDrainTimeoutSecs = hub.drain_timeout_secs => PREFERENCE,
    AgentProvider = agent.provider => PREFERENCE,
    AgentModel = agent.model => PREFERENCE,
    AgentProviderUrl = agent.provider_url => PREFERENCE,
    // Egress policy for ahma's own HTTP tools; a project file must not be able
    // to add a domain to `always_allow` (SPEC R-WEB).
    WebDefaultPolicy = web.default_policy => SECURITY,
    WebBlockPrivateRanges = web.block_private_ranges => SECURITY,
    WebOnRedirectToNewDomain = web.on_redirect_to_new_domain => SECURITY,
    WebAlwaysAllow = web.always_allow => ledger(EGRESS_ALLOW),
    WebNeverAllow = web.never_allow => ledger(EGRESS_DENY),
    NetworkRestrict = network.restrict => SECURITY.switch("egress restricted to the allow list"),
    NetworkAllow = network.allow => ledger(NETWORK_ALLOW),
    NetworkProfileHosts = network.profile_hosts => SECURITY,
    NetworkDenyProfileHosts = network.deny_profile_hosts => SECURITY,
    NetworkLocalPorts = network.local_ports => SECURITY,
    // The grant ledger itself. A project file proposing grants would be the
    // whole R5.4.5 gate bypassed in one line.
    PermissionsToolApprovals = permissions.tool_approvals => ledger(PERMISSIONS_LIST),
    PermissionsAdvisor = permissions.advisor => SECURITY,
    PermissionsAdvisorTimeoutSecs = permissions.advisor_timeout_secs => SECURITY,
    // Approved log targets widen what the sandbox may read: a cloned
    // repository must not approve a symlink of its own to `~/.ssh`.
    LogTargetsApprovals = log_targets.approvals => ledger(PERMISSIONS_LIST),
    // A cloned repository must not let its commands open links in the
    // human's signed-in browser (SPEC R-LINK.4).
    OpenUrlAllow = open_url.allow => ledger(OPEN_URL_GRANT),
    OpenUrlRepos = open_url.repos => ledger(OPEN_URL_GRANT),
}

impl SettingKey {
    /// The key named `table.field`, or `None` when this build has no such key.
    pub fn from_dotted(dotted: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == dotted)
    }

    /// The key `field` of `[table]`, or `None` when this build has no such key.
    pub fn from_parts(table: &str, field: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|k| k.table() == table && k.field() == field)
    }

    /// Who may set it (SPEC R-CFG2.1).
    pub const fn tier(self) -> SettingsTier {
        self.spec().tier
    }

    /// The command that manages a *ledger* key — one that holds grants, each
    /// checked and audited by its own command — or `None` for an ordinary
    /// setting (SPEC R-CFG10.7, R-CFG11.3).
    pub const fn ledger_command(self) -> Option<&'static str> {
        self.spec().ledger
    }

    /// Whether its value is shown as `<redacted>` in every report.
    pub const fn is_redacted(self) -> bool {
        self.spec().redacted
    }

    /// Whether only a CLI flag sets it: a file value parses but never takes
    /// effect (SPEC R-CFG2.3).
    pub const fn is_cli_only(self) -> bool {
        self.spec().cli_only
    }

    /// The setup view's label for this switch when it is on, or `None` for a
    /// key that is not listed as a switch.
    pub const fn switch_label(self) -> Option<&'static str> {
        self.spec().switch
    }

    /// The boolean value of this key that turns its switch on — `true` for
    /// all but an opt-out key such as `sandbox.disable_temp`, whose switch
    /// ("temporary files") is on while the key is `false`. Turning the switch
    /// off writes the negation.
    pub const fn switch_on_value(self) -> bool {
        self.spec().switch_on_when
    }

    /// Whether this key's switch is on in `s`: `None` for a key that is not
    /// a switch. The one reading of a switch, so an inverted key cannot be
    /// read the wrong way round by a second caller.
    pub fn switch_is_on(self, s: &AhmaSettings) -> Option<bool> {
        self.switch_label()?;
        Some(self.value(s) == Some(toml::Value::Boolean(self.switch_on_value())))
    }

    /// Whether `settings show` and the startup report list it: every key but
    /// the ledger keys, which their own commands list (SPEC R-CFG5.1).
    pub const fn in_settings_report(self) -> bool {
        self.ledger_command().is_none()
    }

    /// The value in `s` as TOML, or `None` for an optional key with no value.
    pub fn value(self, s: &AhmaSettings) -> Option<toml::Value> {
        toml::Value::try_from(s)
            .ok()?
            .get(self.table())?
            .get(self.field())
            .cloned()
    }

    /// Whether every key of `[table]` is Security-tier, so an unknown key
    /// there is Security too: the table is what makes it dangerous, not the
    /// name, and an omission fails closed.
    pub fn is_security_table(table: &str) -> bool {
        let mut keys = Self::ALL.iter().filter(|k| k.table() == table).peekable();
        keys.peek().is_some() && keys.all(|k| k.tier() == SettingsTier::Security)
    }
}

impl std::fmt::Display for SettingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SettingKey {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_dotted(s).ok_or_else(|| format!("unknown setting `{s}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_round_trips_through_its_name() {
        for &k in SettingKey::ALL {
            assert_eq!(SettingKey::from_dotted(k.as_str()), Some(k), "{k}");
            assert_eq!(k.as_str().parse::<SettingKey>(), Ok(k));
            assert_eq!(SettingKey::from_parts(k.table(), k.field()), Some(k));
            assert_eq!(format!("{}.{}", k.table(), k.field()), k.to_string());
        }
        let mut names: Vec<&str> = SettingKey::ALL.iter().map(|k| k.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SettingKey::ALL.len(), "a name is used twice");
        assert_eq!(SettingKey::from_dotted("tools.no_such_key"), None);
        assert_eq!(SettingKey::from_dotted("tools"), None);
        assert!("hub.nope".parse::<SettingKey>().is_err());
    }

    /// A ledger key is always Security: grants never come from a repository.
    #[test]
    fn every_ledger_key_is_security_tier() {
        for &k in SettingKey::ALL {
            if k.ledger_command().is_some() {
                assert_eq!(k.tier(), SettingsTier::Security, "{k}");
                assert!(!k.in_settings_report(), "{k}");
            }
        }
    }

    /// SPEC R5.2.5, R-SETUP.4.1: "temporary files" is a switch that is on by
    /// default, and it is on while `disable_temp` is `false`.
    #[test]
    fn the_temporary_files_switch_reads_disable_temp_inverted() {
        let k = SettingKey::SandboxDisableTemp;
        assert_eq!(k.switch_label(), Some("temporary files"));
        assert!(!k.switch_on_value());
        let mut s = AhmaSettings::default();
        assert_eq!(k.switch_is_on(&s), Some(true), "on by default");
        s.sandbox.disable_temp = true;
        assert_eq!(k.switch_is_on(&s), Some(false), "the opt-out turns it off");

        // Every other switch is on when its key is `true`.
        let keychain = SettingKey::SandboxAllowKeychain;
        assert!(keychain.switch_on_value());
        assert_eq!(keychain.switch_is_on(&AhmaSettings::default()), Some(true));
        assert_eq!(SettingKey::ToolsTimeoutSecs.switch_is_on(&s), None);
    }

    #[test]
    fn a_table_is_a_security_table_only_when_every_key_in_it_is() {
        assert!(SettingKey::is_security_table("sandbox"));
        assert!(SettingKey::is_security_table("open_url"));
        assert!(
            !SettingKey::is_security_table("http"),
            "one key is Security"
        );
        assert!(
            !SettingKey::is_security_table("tools"),
            "one key is Security"
        );
        assert!(!SettingKey::is_security_table("not_a_table"));
    }
}
