//! Per-key provenance of the effective settings (SPEC R-CFG5), and the report a
//! server logs about them at startup (R-CFG5.2, R-CFG6.3).
//!
//! One row model, two surfaces: `ahma settings show --origin` renders every row
//! (R-CFG5.1), and `ahma serve` / `ahma hub` log the rows whose value differs
//! from the compiled-in default (R-CFG5.2). Both call [`resolve_origins`], so the
//! source a startup log line names is, by construction, the source the
//! `--origin` report names for the same key.
//!
//! Everything here is pure except [`loose_permissions_warning`], which reads one
//! file's metadata. Callers compute the report while configuration is being
//! resolved (R-CFG4.1) and only *emit* it later, so the logging site does no I/O.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::config::{AhmaSettings, SettingsTier, settings_tier};

/// One displayed setting: its dotted key, effective (file-resolved) value, and
/// compiled-in default value, both pre-rendered with `{:?}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingRow {
    /// Dotted key, `table.key` (`"tools.timeout_secs"`).
    pub key: &'static str,
    /// The value the settings files resolved to, rendered with `{:?}`.
    pub value: String,
    /// The compiled-in default, rendered with `{:?}`.
    pub default: String,
}

/// Flatten the displayed subset of [`AhmaSettings`] into dotted-key rows.
///
/// Single source of truth for *which* settings both `settings show` variants
/// print and the startup report considers, so the three can never drift apart.
pub fn setting_rows(s: &AhmaSettings, d: &AhmaSettings) -> Vec<SettingRow> {
    let mut rows = Vec::new();
    macro_rules! row {
        ($key:expr, $field:ident . $($rest:ident).+) => {
            rows.push(SettingRow {
                key: $key,
                value: format!("{:?}", s.$field.$($rest).+),
                default: format!("{:?}", d.$field.$($rest).+),
            });
        };
    }
    row!("lmstudio.base_url", lmstudio.base_url);
    row!("lmstudio.model", lmstudio.model);
    row!("tools.timeout_secs", tools.timeout_secs);
    row!("tools.execution_mode", tools.execution_mode);
    row!("tools.skip_probes", tools.skip_probes);
    row!("sandbox.disable", sandbox.disable);
    row!("sandbox.tmp_access", sandbox.tmp_access);
    row!("sandbox.disable_temp", sandbox.disable_temp);
    row!("sandbox.defer", sandbox.defer);
    row!("sandbox.container_root", sandbox.container_root);
    row!("sandbox.scratch_directory", sandbox.scratch_directory);
    row!(
        "sandbox.use_scratch_directory",
        sandbox.use_scratch_directory
    );
    row!("logging.target", logging.target);
    row!("logging.log_monitor", logging.log_monitor);
    row!(
        "logging.monitor_rate_limit_secs",
        logging.monitor_rate_limit_secs
    );
    row!("http.handshake_timeout_secs", http.handshake_timeout_secs);
    row!("http.disable_quic", http.disable_quic);
    row!("http.disable_http1_1", http.disable_http1_1);
    row!("auth.require_token_path", auth.require_token_path);
    row!("auth.rate_limit_rps", auth.rate_limit_rps);
    row!("auth.rate_limit_burst", auth.rate_limit_burst);
    row!("instance.label", instance.label);
    rows
}

/// Whether the raw settings-file TOML explicitly sets `dotted` (e.g.
/// `"tools.timeout_secs"`), regardless of what value it sets it to.
pub fn file_sets_key(file_toml: Option<&toml::Value>, dotted: &str) -> bool {
    let Some(mut v) = file_toml else { return false };
    for part in dotted.split('.') {
        match v.get(part) {
            Some(next) => v = next,
            None => return false,
        }
    }
    true
}

/// Where a setting's effective value came from (R-CFG5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingSource {
    /// A command-line flag passed this invocation.
    Cli,
    /// `<workspace>/.ahma/settings.toml`.
    Project(PathBuf),
    /// The user settings file (`~/.ahma/settings.toml` or `--settings-path`);
    /// `None` only when no path could be determined.
    User(Option<PathBuf>),
    /// Compiled in.
    Default,
}

impl fmt::Display for SettingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli => f.write_str("cli"),
            Self::Project(p) => write!(f, "project ({})", p.display()),
            Self::User(Some(p)) => write!(f, "user ({})", p.display()),
            Self::User(None) => f.write_str("user"),
            Self::Default => f.write_str("default"),
        }
    }
}

/// What the provenance resolution needs to know about this invocation.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProvenanceInputs<'a> {
    /// `(dotted key, rendered value)` for each CLI flag passed this invocation
    /// that overrides a settings key.
    pub cli_overrides: &'a [(&'static str, String)],
    /// The project settings file and the keys it actually contributed (only
    /// the accepted, Preference-tier keys — R-CFG2.2).
    pub project: Option<(&'a Path, &'a [String])>,
    /// The user settings file in effect, if any.
    pub user_file: Option<&'a Path>,
    /// That file's raw TOML, when the caller has it: lets a key the file sets
    /// *to the default value* still be attributed to the file.
    pub user_toml: Option<&'a toml::Value>,
}

/// One setting with its effective value and true source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingOrigin {
    /// Dotted key.
    pub key: &'static str,
    /// The effective value: the CLI flag's value when one overrode the key,
    /// otherwise the file-resolved value.
    pub value: String,
    /// The compiled-in default, rendered with `{:?}`.
    pub default: String,
    /// Where `value` came from.
    pub source: SettingSource,
}

impl SettingOrigin {
    /// Whether the effective value differs from the compiled-in default.
    pub fn deviates(&self) -> bool {
        self.value != self.default
    }
}

/// Resolve every row's effective value and source, in R-CFG1.1 precedence
/// order (highest first): cli > project > user > default.
///
/// A value that differs from the default with neither a flag nor the project
/// file behind it can only have come from the user file, so it is attributed
/// there even when the caller has no raw TOML to consult — which is what lets
/// the startup report use this without re-reading the file.
pub fn resolve_origins(rows: &[SettingRow], inputs: &ProvenanceInputs<'_>) -> Vec<SettingOrigin> {
    rows.iter().map(|row| resolve_origin(row, inputs)).collect()
}

fn resolve_origin(row: &SettingRow, inputs: &ProvenanceInputs<'_>) -> SettingOrigin {
    let cli_value = inputs
        .cli_overrides
        .iter()
        .find(|(k, _)| *k == row.key)
        .map(|(_, v)| v.clone());
    let project = inputs
        .project
        .filter(|(_, keys)| keys.iter().any(|k| k == row.key))
        .map(|(path, _)| path.to_path_buf());
    let user_sets = file_sets_key(inputs.user_toml, row.key) || row.value != row.default;
    let (value, source) = match (cli_value, project) {
        (Some(v), _) => (v, SettingSource::Cli),
        (None, Some(path)) => (row.value.clone(), SettingSource::Project(path)),
        (None, None) if user_sets => (
            row.value.clone(),
            SettingSource::User(inputs.user_file.map(Path::to_path_buf)),
        ),
        (None, None) => (row.value.clone(), SettingSource::Default),
    };
    SettingOrigin {
        key: row.key,
        value,
        default: row.default.clone(),
        source,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Startup report (R-CFG5.2, R-CFG6.3)
// ─────────────────────────────────────────────────────────────────────────────

/// The level a startup line is logged at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupLevel {
    /// A Preference-tier deviation.
    Info,
    /// A Security-tier deviation, or a settings file others can write.
    Warn,
}

/// One line of the startup settings report, ready to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupLine {
    /// `info` or `warn`.
    pub level: StartupLevel,
    /// The full message.
    pub message: String,
}

/// Keys whose value is never logged. None of today's rows hold a secret (the
/// bearer token is configured by *path*), but a row added later might, and a
/// startup log is a file that outlives the process.
const SECRET_KEYS: &[&str] = &["auth.require_token"];

/// Keys that only a CLI flag can set (SPEC R-CFG2.3). A file value parses but
/// never takes effect, and `parse_sandbox_settings` already warns that it was
/// ignored, so reporting it here as a deviation would claim the opposite.
const CLI_ONLY_KEYS: &[&str] = &["sandbox.disable"];

/// The trust tier of a dotted key, via the one classification (R-CFG2.1).
fn dotted_tier(dotted: &str) -> Option<SettingsTier> {
    let (table, key) = dotted.split_once('.')?;
    settings_tier(table, key)
}

fn shown_value<'a>(key: &str, value: &'a str) -> &'a str {
    if SECRET_KEYS.contains(&key) {
        "<redacted>"
    } else {
        value
    }
}

/// One line per setting whose effective value differs from the compiled-in
/// default, naming the key, value, default and source (R-CFG5.2).
/// Security-tier deviations — and any key the tier table does not know, which
/// fails toward loudness — are `warn`; the rest are `info`.
pub fn startup_deviation_lines(origins: &[SettingOrigin]) -> Vec<StartupLine> {
    origins
        .iter()
        .filter(|o| o.deviates())
        .filter(|o| o.source == SettingSource::Cli || !CLI_ONLY_KEYS.contains(&o.key))
        .map(|o| {
            let security = dotted_tier(o.key) != Some(SettingsTier::Preference);
            let (level, tier) = if security {
                (StartupLevel::Warn, "security-tier setting")
            } else {
                (StartupLevel::Info, "setting")
            };
            StartupLine {
                level,
                message: format!(
                    "{tier} {} = {} (default {}) from {} (SPEC R-CFG5.2)",
                    o.key,
                    shown_value(o.key, &o.value),
                    shown_value(o.key, &o.default),
                    o.source
                ),
            }
        })
        .collect()
}

/// The R-CFG6.3 warning for a settings file with permission bits `mode`, or
/// `None` when neither group nor others may write it.
///
/// Split from [`loose_permissions_warning`] so the rule is testable on every
/// platform; only reading the mode is Unix-specific.
pub fn loose_mode_warning(path: &Path, mode: u32) -> Option<String> {
    if mode & 0o022 == 0 {
        return None;
    }
    Some(format!(
        "settings file {} is group- or world-writable (mode {:03o}): another local \
         user could change how ahma is configured, including its sandbox. Fix it \
         with `chmod go-w {}` (SPEC R-CFG6.3).",
        path.display(),
        mode & 0o777,
        path.display()
    ))
}

/// The R-CFG6.3 warning for the settings file at `path`, if group or others
/// can write it. `None` for a file that is missing or unreadable (the loader
/// reports those), and always `None` off Unix, where there are no mode bits.
pub fn loose_permissions_warning(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode();
        loose_mode_warning(path, mode)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// The whole startup report: every deviation (R-CFG5.2), then a `warn` for
/// each settings file in `files` that others can write (R-CFG6.3).
pub fn startup_settings_report(origins: &[SettingOrigin], files: &[&Path]) -> Vec<StartupLine> {
    let mut lines = startup_deviation_lines(origins);
    lines.extend(
        files
            .iter()
            .filter_map(|f| loose_permissions_warning(f))
            .map(|message| StartupLine {
                level: StartupLevel::Warn,
                message,
            }),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(
        key: &'static str,
        value: &str,
        default: &str,
        source: SettingSource,
    ) -> SettingOrigin {
        SettingOrigin {
            key,
            value: value.to_string(),
            default: default.to_string(),
            source,
        }
    }

    fn user(path: &str) -> SettingSource {
        SettingSource::User(Some(PathBuf::from(path)))
    }

    // ── resolve_origins ──────────────────────────────────────────────────────

    #[test]
    fn a_deviating_value_without_a_higher_source_is_attributed_to_the_user_file() {
        let s = AhmaSettings::parse("[tools]\ntimeout_secs = 999\n").unwrap();
        let rows = setting_rows(&s, &AhmaSettings::default());
        let file = PathBuf::from("/home/u/.ahma/settings.toml");
        let inputs = ProvenanceInputs {
            user_file: Some(file.as_path()),
            ..ProvenanceInputs::default()
        };
        let origins = resolve_origins(&rows, &inputs);
        let timeout = origins
            .iter()
            .find(|o| o.key == "tools.timeout_secs")
            .unwrap();
        assert_eq!(timeout.source, SettingSource::User(Some(file.clone())));
        assert_eq!(timeout.value, "999");
        let mode = origins
            .iter()
            .find(|o| o.key == "tools.execution_mode")
            .unwrap();
        assert_eq!(mode.source, SettingSource::Default);
    }

    #[test]
    fn cli_beats_project_beats_user() {
        let rows = vec![
            SettingRow {
                key: "tools.timeout_secs",
                value: "300".into(),
                default: "1800".into(),
            },
            SettingRow {
                key: "instance.label",
                value: "\"proj\"".into(),
                default: "\"ahma\"".into(),
            },
        ];
        let project = PathBuf::from("/repo/.ahma/settings.toml");
        let project_keys = vec![
            "tools.timeout_secs".to_string(),
            "instance.label".to_string(),
        ];
        let cli = vec![("tools.timeout_secs", "30".to_string())];
        let inputs = ProvenanceInputs {
            cli_overrides: &cli,
            project: Some((project.as_path(), project_keys.as_slice())),
            ..ProvenanceInputs::default()
        };
        let origins = resolve_origins(&rows, &inputs);
        assert_eq!(origins[0].source, SettingSource::Cli);
        assert_eq!(
            origins[0].value, "30",
            "the flag's value is the effective one"
        );
        assert_eq!(origins[1].source, SettingSource::Project(project));
    }

    #[test]
    fn sources_render_as_the_origin_report_labels_them() {
        assert_eq!(SettingSource::Cli.to_string(), "cli");
        assert_eq!(SettingSource::Default.to_string(), "default");
        assert_eq!(SettingSource::User(None).to_string(), "user");
        assert_eq!(user("/u/s.toml").to_string(), "user (/u/s.toml)");
        assert_eq!(
            SettingSource::Project(PathBuf::from("/r/s.toml")).to_string(),
            "project (/r/s.toml)"
        );
    }

    // ── startup_deviation_lines (R-CFG5.2) ───────────────────────────────────

    #[test]
    fn only_settings_that_differ_from_the_default_are_reported() {
        let lines = startup_deviation_lines(&[
            origin("tools.timeout_secs", "1800", "1800", user("/u/s.toml")),
            origin(
                "tools.skip_probes",
                "false",
                "false",
                SettingSource::Default,
            ),
        ]);
        assert!(lines.is_empty(), "nothing deviates: {lines:?}");
    }

    #[test]
    fn a_preference_deviation_is_info_and_names_key_value_default_and_source() {
        let lines = startup_deviation_lines(&[origin(
            "tools.timeout_secs",
            "600",
            "1800",
            user("/u/s.toml"),
        )]);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].level, StartupLevel::Info);
        let m = &lines[0].message;
        for needle in ["tools.timeout_secs", "600", "1800", "user (/u/s.toml)"] {
            assert!(m.contains(needle), "{needle:?} missing from {m:?}");
        }
    }

    #[test]
    fn a_security_deviation_is_warn() {
        let lines = startup_deviation_lines(&[
            origin("sandbox.tmp_access", "true", "false", user("/u/s.toml")),
            origin("auth.rate_limit_rps", "5", "0", SettingSource::Cli),
        ]);
        assert_eq!(lines.len(), 2);
        assert!(
            lines.iter().all(|l| l.level == StartupLevel::Warn),
            "security-tier deviations must log at warn: {lines:?}"
        );
        assert!(lines[1].message.contains("from cli"), "{lines:?}");
    }

    #[test]
    fn a_key_with_no_known_tier_fails_toward_warn() {
        let lines =
            startup_deviation_lines(&[origin("nosuchtable.key", "1", "0", SettingSource::Cli)]);
        assert_eq!(lines[0].level, StartupLevel::Warn);
    }

    #[test]
    fn a_file_set_cli_only_key_is_not_reported_as_taking_effect() {
        // R-CFG2.3: `[sandbox] disable = true` in a file is ignored (and warned
        // about as ignored); a startup line saying it is in effect would lie.
        let from_file = startup_deviation_lines(&[origin(
            "sandbox.disable",
            "true",
            "false",
            user("/u/s.toml"),
        )]);
        assert!(from_file.is_empty(), "{from_file:?}");
        let from_flag = startup_deviation_lines(&[origin(
            "sandbox.disable",
            "true",
            "false",
            SettingSource::Cli,
        )]);
        assert_eq!(from_flag.len(), 1);
        assert_eq!(from_flag[0].level, StartupLevel::Warn);
    }

    #[test]
    fn a_secret_value_is_redacted() {
        let lines = startup_deviation_lines(&[origin(
            "auth.require_token",
            "\"s3cret-token\"",
            "None",
            user("/u/s.toml"),
        )]);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].message.contains("s3cret-token"), "{lines:?}");
        assert!(lines[0].message.contains("<redacted>"), "{lines:?}");
    }

    // ── loose permissions (R-CFG6.3) ─────────────────────────────────────────

    #[test]
    fn group_or_world_writable_modes_warn_and_private_modes_do_not() {
        let p = Path::new("/u/.ahma/settings.toml");
        assert_eq!(loose_mode_warning(p, 0o100600), None);
        assert_eq!(loose_mode_warning(p, 0o100644), None);
        for mode in [0o100620, 0o100602, 0o100666] {
            let msg = loose_mode_warning(p, mode).expect("writable by others must warn");
            assert!(msg.contains("/u/.ahma/settings.toml"), "{msg}");
            assert!(msg.contains("chmod go-w"), "the remedy is named: {msg}");
            assert!(msg.contains(&format!("{:03o}", mode & 0o777)), "{msg}");
        }
    }

    #[cfg(unix)] // needs PermissionsExt to set mode bits
    #[test]
    fn a_world_writable_settings_file_on_disk_warns() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "").unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let msg = loose_permissions_warning(&path).expect("0o666 must warn");
        assert!(msg.contains(&path.display().to_string()), "{msg}");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(loose_permissions_warning(&path), None);
    }

    #[test]
    fn a_missing_settings_file_produces_no_permission_warning() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            loose_permissions_warning(&dir.path().join("absent.toml")),
            None
        );
    }

    #[cfg(unix)] // needs PermissionsExt to set mode bits
    #[test]
    fn the_report_lists_deviations_then_loose_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        let report = startup_settings_report(
            &[origin(
                "tools.timeout_secs",
                "600",
                "1800",
                user("/u/s.toml"),
            )],
            &[path.as_path()],
        );
        assert_eq!(report.len(), 2, "{report:?}");
        assert_eq!(report[0].level, StartupLevel::Info);
        assert_eq!(report[1].level, StartupLevel::Warn);
        assert!(report[1].message.contains("R-CFG6.3"), "{report:?}");
    }
}
