//! Settings editor for the TUI.
//!
//! Provides a modal settings panel accessible via `/settings` in the command
//! navigator.  Users can browse categorized settings, toggle booleans with
//! Space, edit numbers/strings, and persist changes to `~/.ahma/settings.toml`.

use ahma_common::config::{AhmaSettings, FeatureSettings};

// ─── Setting categories ───────────────────────────────────────────────────────

/// A category of settings in the editor sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsCategory {
    Features,
    Tools,
    Sandbox,
    Logging,
    Http,
    Auth,
    Instance,
}

impl SettingsCategory {
    pub const ALL: &[Self] = &[
        Self::Tools,
        Self::Sandbox,
        Self::Logging,
        Self::Http,
        Self::Auth,
        Self::Instance,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Features => "Features",
            Self::Tools => "Tools",
            Self::Sandbox => "Sandbox",
            Self::Logging => "Logging",
            Self::Http => "HTTP",
            Self::Auth => "Auth",
            Self::Instance => "Instance",
        }
    }

    /// A short ASCII marker for the sidebar. Deliberately not emoji: SPEC R22.3
    /// forbids them in terminal output, and these were rendered ungated by the
    /// crate's unicode detection, so a non-Unicode terminal got mojibake.
    pub fn icon(self) -> &'static str {
        match self {
            Self::Features => "-",
            Self::Tools => "T",
            Self::Sandbox => "S",
            Self::Logging => "L",
            Self::Http => "H",
            Self::Auth => "A",
            Self::Instance => "I",
        }
    }
}

// ─── Setting items ────────────────────────────────────────────────────────────

/// The type of a setting value, determining how it renders and edits.
#[derive(Debug, Clone)]
pub enum SettingValue {
    Bool(bool),
    String(String),
    U64(u64),
    U32(u32),
    Usize(usize),
    StringList(Vec<String>),
}

impl std::fmt::Display for SettingValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool(v) => write!(f, "{}", if *v { "on" } else { "off" }),
            Self::String(v) => write!(f, "{v}"),
            Self::U64(v) => write!(f, "{v}"),
            Self::U32(v) => write!(f, "{v}"),
            Self::Usize(v) => write!(f, "{v}"),
            Self::StringList(v) => {
                if v.is_empty() {
                    write!(f, "[]")
                } else {
                    write!(f, "[{}]", v.join(", "))
                }
            }
        }
    }
}

/// One row in the settings panel.
#[derive(Debug, Clone)]
pub struct SettingItem {
    /// TOML key path, e.g. "features.simplify"
    pub key: &'static str,
    /// Human-readable label
    pub label: &'static str,
    /// Short description shown to the right
    pub description: &'static str,
    /// Current value
    pub value: SettingValue,
    /// Default value (for "reset to default")
    pub default_value: SettingValue,
    /// Whether this is a security-tier setting (read-only in TUI per R-CFG2)
    pub security_tier: bool,
}

impl SettingItem {
    /// Toggle a boolean setting. Returns true if changed.
    pub fn toggle(&mut self) -> bool {
        if self.security_tier {
            return false;
        }
        if let SettingValue::Bool(ref mut v) = self.value {
            *v = !*v;
            true
        } else {
            false
        }
    }

    /// Reset to default. Returns true if changed.
    pub fn reset_to_default(&mut self) -> bool {
        if self.security_tier {
            return false;
        }
        let same = match (&self.value, &self.default_value) {
            (SettingValue::Bool(a), SettingValue::Bool(b)) => a == b,
            (SettingValue::String(a), SettingValue::String(b)) => a == b,
            (SettingValue::U64(a), SettingValue::U64(b)) => a == b,
            (SettingValue::U32(a), SettingValue::U32(b)) => a == b,
            (SettingValue::Usize(a), SettingValue::Usize(b)) => a == b,
            _ => false,
        };
        if same {
            return false;
        }
        self.value = self.default_value.clone();
        true
    }

    /// Check whether the current value differs from the default.
    pub fn is_modified(&self) -> bool {
        match (&self.value, &self.default_value) {
            (SettingValue::Bool(a), SettingValue::Bool(b)) => a != b,
            (SettingValue::String(a), SettingValue::String(b)) => a != b,
            (SettingValue::U64(a), SettingValue::U64(b)) => a != b,
            (SettingValue::U32(a), SettingValue::U32(b)) => a != b,
            (SettingValue::Usize(a), SettingValue::Usize(b)) => a != b,
            _ => true,
        }
    }
}

// ─── Typed assignment from an edited value ────────────────────────────────────

/// Pull the payload of a `SettingValue` out as the type a settings field wants,
/// or `None` when the row carries a different variant than the field it targets.
trait FromSettingValue: Sized {
    fn from_setting_value(value: &SettingValue) -> Option<Self>;
}

impl FromSettingValue for bool {
    fn from_setting_value(value: &SettingValue) -> Option<Self> {
        match value {
            SettingValue::Bool(v) => Some(*v),
            _ => None,
        }
    }
}

impl FromSettingValue for u64 {
    fn from_setting_value(value: &SettingValue) -> Option<Self> {
        match value {
            SettingValue::U64(v) => Some(*v),
            _ => None,
        }
    }
}

impl FromSettingValue for u32 {
    fn from_setting_value(value: &SettingValue) -> Option<Self> {
        match value {
            SettingValue::U32(v) => Some(*v),
            _ => None,
        }
    }
}

impl FromSettingValue for String {
    fn from_setting_value(value: &SettingValue) -> Option<Self> {
        match value {
            SettingValue::String(v) => Some(v.clone()),
            _ => None,
        }
    }
}

/// Assign an edited value into a settings field, ignoring a variant mismatch.
///
/// The `apply_*` dispatchers below are index-to-field tables; without this
/// helper every row repeated the same "match the variant, then assign" nest,
/// which is what made them the densest functions in the module.
fn assign_setting<T: FromSettingValue>(target: &mut T, value: &SettingValue) {
    if let Some(v) = T::from_setting_value(value) {
        *target = v;
    }
}

// ─── Settings editor state ────────────────────────────────────────────────────

/// State for the TUI settings panel overlay.
#[derive(Debug)]
pub struct SettingsEditor {
    /// Whether the settings panel is currently visible.
    pub open: bool,
    /// The loaded settings snapshot being edited.
    settings: AhmaSettings,
    /// Which category is selected in the sidebar.
    pub selected_category: usize,
    /// Which item is selected in the current category.
    pub selected_item: usize,
    /// Whether any value has been changed since last save.
    pub dirty: bool,
    /// Status message shown at the bottom ("✓ Saved", "⚠ Error", etc.)
    pub status_message: Option<(String, std::time::Instant)>,
    /// Set once Esc has been pressed with unsaved changes pending, so the next
    /// Esc discards deliberately rather than by accident.
    pub confirming_discard: bool,
    /// Inline edit mode for string/numeric fields.
    pub editing: Option<String>,
}

impl Default for SettingsEditor {
    fn default() -> Self {
        Self {
            open: false,
            settings: AhmaSettings::load(),
            selected_category: 0,
            selected_item: 0,
            dirty: false,
            status_message: None,
            confirming_discard: false,
            editing: None,
        }
    }
}

impl SettingsEditor {
    /// Open the settings panel, refreshing from disk.
    pub fn open(&mut self) {
        self.settings = AhmaSettings::load();
        self.open = true;
        self.dirty = false;
        self.selected_category = 0;
        self.selected_item = 0;
        self.editing = None;
        self.status_message = None;
    }

    /// Close the panel. With unsaved changes the first attempt asks instead of
    /// discarding: the footer says "unsaved changes", and throwing them away on
    /// a single Esc contradicts it. A second Esc confirms.
    pub fn close(&mut self) {
        if self.dirty && !self.confirming_discard {
            self.confirming_discard = true;
            self.status_message = Some((
                "Unsaved changes — [s] saves, Esc again discards".into(),
                std::time::Instant::now(),
            ));
            return;
        }
        self.force_close();
    }

    /// Close unconditionally, discarding any unsaved edits.
    pub fn force_close(&mut self) {
        self.open = false;
        self.editing = None;
        self.dirty = false;
        self.confirming_discard = false;
    }

    /// The currently selected category.
    pub fn current_category(&self) -> SettingsCategory {
        SettingsCategory::ALL[self.selected_category]
    }

    /// Move category selection up.
    pub fn category_up(&mut self) {
        if self.selected_category > 0 {
            self.selected_category -= 1;
            self.selected_item = 0;
        }
    }

    /// Move category selection down.
    pub fn category_down(&mut self) {
        if self.selected_category + 1 < SettingsCategory::ALL.len() {
            self.selected_category += 1;
            self.selected_item = 0;
        }
    }

    /// Move item selection up within the current category.
    pub fn item_up(&mut self) {
        if self.selected_item > 0 {
            self.selected_item -= 1;
        }
    }

    /// Move item selection down within the current category.
    pub fn item_down(&mut self) {
        let items = self.items_for_category(self.current_category());
        if self.selected_item + 1 < items.len() {
            self.selected_item += 1;
        }
    }

    /// Toggle the currently selected boolean setting.
    /// Toggle the selected setting, or explain why it cannot be toggled.
    ///
    /// Silence was the defect here: security-tier rows and every string/numeric
    /// row simply ignored the keypress, so most of the panel looked broken
    /// rather than read-only.
    pub fn toggle_current(&mut self) {
        let category = self.current_category();
        let mut items = self.items_for_category(category);
        let Some(item) = items.get_mut(self.selected_item) else {
            return;
        };
        if item.toggle() {
            self.apply_item_to_settings(category, self.selected_item, &item.value);
            self.dirty = true;
            return;
        }
        let why = if item.security_tier {
            // R-CFG2.3: the dangerous switches are CLI-flag-only by design, so
            // they are always visible at the invocation site.
            format!(
                "{} is security-tier: set it with a CLI flag, not here",
                item.key
            )
        } else {
            format!(
                "{} is not editable here — edit ~/.ahma/settings.toml",
                item.key
            )
        };
        self.status_message = Some((why, std::time::Instant::now()));
    }

    /// Reset the currently selected setting to its default.
    pub fn reset_current(&mut self) {
        let category = self.current_category();
        let mut items = self.items_for_category(category);
        if let Some(item) = items.get_mut(self.selected_item)
            && item.reset_to_default()
        {
            self.apply_item_to_settings(category, self.selected_item, &item.value);
            self.dirty = true;
        }
    }

    /// Save settings to disk.
    pub fn save(&mut self) {
        match self.settings.save() {
            Ok(()) => {
                self.dirty = false;
                self.status_message = Some(("✓ Saved".into(), std::time::Instant::now()));
            }
            Err(e) => {
                self.status_message =
                    Some((format!("⚠ Save failed: {e}"), std::time::Instant::now()));
            }
        }
    }

    /// Get the current settings snapshot (for reading feature flags).
    pub fn settings(&self) -> &AhmaSettings {
        &self.settings
    }

    /// Get items for a given category, reading from the current settings.
    pub fn items_for_category(&self, category: SettingsCategory) -> Vec<SettingItem> {
        let defaults = AhmaSettings::default();
        match category {
            SettingsCategory::Features => self.feature_items(&defaults),
            SettingsCategory::Tools => self.tool_items(&defaults),
            SettingsCategory::Sandbox => self.sandbox_items(&defaults),
            SettingsCategory::Logging => self.logging_items(&defaults),
            SettingsCategory::Http => self.http_items(&defaults),
            SettingsCategory::Auth => self.auth_items(&defaults),
            SettingsCategory::Instance => self.instance_items(&defaults),
        }
    }

    // ── Category item builders ────────────────────────────────────────────

    fn feature_items(&self, defaults: &AhmaSettings) -> Vec<SettingItem> {
        let f = &self.settings.features;
        let d = &defaults.features;
        vec![
            SettingItem {
                key: "features.simplify",
                label: "Simplify",
                description: "Code complexity analysis (ahma simplify)",
                value: SettingValue::Bool(f.simplify),
                default_value: SettingValue::Bool(d.simplify),
                security_tier: false,
            },
            SettingItem {
                key: "features.vault",
                label: "Vault",
                description: "Task vault isolation (per-session directories)",
                value: SettingValue::Bool(f.vault),
                default_value: SettingValue::Bool(d.vault),
                security_tier: false,
            },
            SettingItem {
                key: "features.egress",
                label: "Egress",
                description: "Network egress proxy for sandboxed tasks",
                value: SettingValue::Bool(f.egress),
                default_value: SettingValue::Bool(d.egress),
                security_tier: false,
            },
            SettingItem {
                key: "features.artifact",
                label: "Artifact",
                description: "HTML artifact output channel",
                value: SettingValue::Bool(f.artifact),
                default_value: SettingValue::Bool(d.artifact),
                security_tier: false,
            },
            SettingItem {
                key: "features.decompose",
                label: "Decompose",
                description: "LLM-powered task decomposition",
                value: SettingValue::Bool(f.decompose),
                default_value: SettingValue::Bool(d.decompose),
                security_tier: false,
            },
        ]
    }

    fn tool_items(&self, defaults: &AhmaSettings) -> Vec<SettingItem> {
        let t = &self.settings.tools;
        let d = &defaults.tools;
        vec![
            SettingItem {
                key: "tools.timeout_secs",
                label: "Timeout",
                description: "Default tool timeout (seconds)",
                value: SettingValue::U64(t.timeout_secs),
                default_value: SettingValue::U64(d.timeout_secs),
                security_tier: false,
            },
            SettingItem {
                key: "tools.force_sync",
                label: "Force sync",
                description: "Run all tools synchronously",
                value: SettingValue::Bool(t.force_sync),
                default_value: SettingValue::Bool(d.force_sync),
                security_tier: false,
            },
            SettingItem {
                key: "tools.skip_probes",
                label: "Skip probes",
                description: "Skip availability probes at startup",
                value: SettingValue::Bool(t.skip_probes),
                default_value: SettingValue::Bool(d.skip_probes),
                security_tier: false,
            },
            SettingItem {
                key: "tools.minimize_tokens",
                label: "Minimize tokens",
                description: "Output compression and token minimization",
                value: SettingValue::Bool(t.minimize_tokens),
                default_value: SettingValue::Bool(d.minimize_tokens),
                security_tier: false,
            },
            SettingItem {
                key: "tools.small_model_harness",
                label: "Small model harness",
                description: "Coaching hints for small-context LLMs",
                value: SettingValue::Bool(t.small_model_harness),
                default_value: SettingValue::Bool(d.small_model_harness),
                security_tier: false,
            },
        ]
    }

    fn sandbox_items(&self, defaults: &AhmaSettings) -> Vec<SettingItem> {
        let s = &self.settings.sandbox;
        let d = &defaults.sandbox;
        vec![
            SettingItem {
                key: "sandbox.disable",
                label: "Disable sandbox",
                description: "⚠ UNSAFE: disable kernel sandbox",
                value: SettingValue::Bool(s.disable),
                default_value: SettingValue::Bool(d.disable),
                security_tier: true,
            },
            SettingItem {
                key: "sandbox.tmp_access",
                label: "Temp access",
                description: "Add system temp dir to sandbox scope",
                value: SettingValue::Bool(s.tmp_access),
                default_value: SettingValue::Bool(d.tmp_access),
                security_tier: true,
            },
            SettingItem {
                key: "sandbox.disable_temp",
                label: "Disable temp",
                description: "Block all temp directory access",
                value: SettingValue::Bool(s.disable_temp),
                default_value: SettingValue::Bool(d.disable_temp),
                security_tier: true,
            },
            SettingItem {
                key: "sandbox.defer",
                label: "Defer lock",
                description: "Defer sandbox until client provides roots",
                value: SettingValue::Bool(s.defer),
                default_value: SettingValue::Bool(d.defer),
                security_tier: true,
            },
            SettingItem {
                key: "sandbox.package_cache_write",
                label: "Package cache write",
                description: "Allow cargo registry/git cache writes",
                value: SettingValue::Bool(s.package_cache_write),
                default_value: SettingValue::Bool(d.package_cache_write),
                security_tier: true,
            },
            SettingItem {
                key: "sandbox.allow_keychain",
                label: "Allow keychain",
                description: "macOS: allow keychain read/write (gh, git-credential-osxkeychain)",
                value: SettingValue::Bool(s.allow_keychain),
                default_value: SettingValue::Bool(d.allow_keychain),
                security_tier: true,
            },
            SettingItem {
                key: "sandbox.allow_git_hooks",
                label: "Allow git hooks",
                description: "Let tools write <git dir>/hooks/**; a hook runs OUTSIDE the sandbox",
                value: SettingValue::Bool(s.allow_git_hooks),
                default_value: SettingValue::Bool(d.allow_git_hooks),
                security_tier: true,
            },
            SettingItem {
                key: "sandbox.allow_project_tool_config",
                label: "Allow project tool config",
                description: "Let tools write this workspace's .ahma/ MTDF tool definitions",
                value: SettingValue::Bool(s.allow_project_tool_config),
                default_value: SettingValue::Bool(d.allow_project_tool_config),
                security_tier: true,
            },
        ]
    }

    fn logging_items(&self, defaults: &AhmaSettings) -> Vec<SettingItem> {
        let l = &self.settings.logging;
        let d = &defaults.logging;
        vec![
            SettingItem {
                key: "logging.target",
                label: "Log target",
                description: "\"file\" (rolling) or \"stderr\"",
                value: SettingValue::String(l.target.clone()),
                default_value: SettingValue::String(d.target.clone()),
                security_tier: false,
            },
            SettingItem {
                key: "logging.log_monitor",
                label: "Log monitor",
                description: "Enable live log monitoring via LLM",
                value: SettingValue::Bool(l.log_monitor),
                default_value: SettingValue::Bool(d.log_monitor),
                security_tier: false,
            },
            SettingItem {
                key: "logging.monitor_rate_limit_secs",
                label: "Monitor rate limit",
                description: "Min seconds between log-monitor alerts",
                value: SettingValue::U64(l.monitor_rate_limit_secs),
                default_value: SettingValue::U64(d.monitor_rate_limit_secs),
                security_tier: false,
            },
            SettingItem {
                key: "logging.dir",
                label: "Log directory",
                description: "Empty = repo root's logs/; set to keep logs out of the tree",
                value: SettingValue::String(l.dir.clone()),
                default_value: SettingValue::String(d.dir.clone()),
                security_tier: false,
            },
        ]
    }

    fn http_items(&self, defaults: &AhmaSettings) -> Vec<SettingItem> {
        let h = &self.settings.http;
        let d = &defaults.http;
        vec![
            SettingItem {
                key: "http.handshake_timeout_secs",
                label: "Handshake timeout",
                description: "MCP handshake timeout (seconds)",
                value: SettingValue::U64(h.handshake_timeout_secs),
                default_value: SettingValue::U64(d.handshake_timeout_secs),
                security_tier: false,
            },
            SettingItem {
                key: "http.disable_quic",
                label: "Disable QUIC",
                description: "Disable HTTP/3; fall back to HTTP/2",
                value: SettingValue::Bool(h.disable_quic),
                default_value: SettingValue::Bool(d.disable_quic),
                security_tier: false,
            },
            SettingItem {
                key: "http.disable_http1_1",
                label: "Disable HTTP/1.1",
                description: "Require HTTP/2+",
                value: SettingValue::Bool(h.disable_http1_1),
                default_value: SettingValue::Bool(d.disable_http1_1),
                security_tier: false,
            },
        ]
    }

    fn auth_items(&self, defaults: &AhmaSettings) -> Vec<SettingItem> {
        let a = &self.settings.auth;
        let d = &defaults.auth;
        vec![
            SettingItem {
                key: "auth.rate_limit_rps",
                label: "Rate limit RPS",
                description: "Max requests/second (0 = unlimited)",
                value: SettingValue::U64(a.rate_limit_rps),
                default_value: SettingValue::U64(d.rate_limit_rps),
                security_tier: true,
            },
            SettingItem {
                key: "auth.rate_limit_burst",
                label: "Rate limit burst",
                description: "Burst allowance for rate limiter",
                value: SettingValue::U32(a.rate_limit_burst),
                default_value: SettingValue::U32(d.rate_limit_burst),
                security_tier: true,
            },
        ]
    }

    fn instance_items(&self, defaults: &AhmaSettings) -> Vec<SettingItem> {
        let i = &self.settings.instance;
        let d = &defaults.instance;
        vec![SettingItem {
            key: "instance.label",
            label: "Instance label",
            description: "Name shown in TUI and daemon",
            value: SettingValue::String(i.label.clone()),
            default_value: SettingValue::String(d.label.clone()),
            security_tier: false,
        }]
    }

    // ── Apply edits back to the settings struct ──────────────────────────

    fn apply_item_to_settings(
        &mut self,
        category: SettingsCategory,
        index: usize,
        value: &SettingValue,
    ) {
        match category {
            SettingsCategory::Features => self.apply_feature(index, value),
            SettingsCategory::Tools => self.apply_tool(index, value),
            SettingsCategory::Sandbox => self.apply_sandbox(index, value),
            SettingsCategory::Logging => self.apply_logging(index, value),
            SettingsCategory::Http => self.apply_http(index, value),
            SettingsCategory::Auth => self.apply_auth(index, value),
            SettingsCategory::Instance => self.apply_instance(index, value),
        }
    }

    fn apply_feature(&mut self, index: usize, value: &SettingValue) {
        let SettingValue::Bool(v) = value else {
            return;
        };
        let f = &mut self.settings.features;
        match index {
            0 => f.simplify = *v,
            1 => f.vault = *v,
            2 => f.egress = *v,
            3 => f.artifact = *v,
            4 => f.decompose = *v,
            _ => {}
        }
    }

    fn apply_tool(&mut self, index: usize, value: &SettingValue) {
        let t = &mut self.settings.tools;
        match index {
            0 => assign_setting(&mut t.timeout_secs, value),
            1 => assign_setting(&mut t.force_sync, value),
            2 => assign_setting(&mut t.skip_probes, value),
            3 => assign_setting(&mut t.minimize_tokens, value),
            4 => assign_setting(&mut t.small_model_harness, value),
            _ => {}
        }
    }

    fn apply_sandbox(&mut self, index: usize, value: &SettingValue) {
        let SettingValue::Bool(v) = value else {
            return;
        };
        let s = &mut self.settings.sandbox;
        match index {
            0 => s.disable = *v,
            1 => s.tmp_access = *v,
            2 => s.disable_temp = *v,
            3 => s.defer = *v,
            4 => s.package_cache_write = *v,
            5 => s.allow_keychain = *v,
            6 => s.allow_git_hooks = *v,
            7 => s.allow_project_tool_config = *v,
            _ => {}
        }
    }

    fn apply_logging(&mut self, index: usize, value: &SettingValue) {
        let l = &mut self.settings.logging;
        match index {
            0 => assign_setting(&mut l.target, value),
            1 => assign_setting(&mut l.log_monitor, value),
            2 => assign_setting(&mut l.monitor_rate_limit_secs, value),
            3 => assign_setting(&mut l.dir, value),
            _ => {}
        }
    }

    fn apply_http(&mut self, index: usize, value: &SettingValue) {
        let h = &mut self.settings.http;
        match index {
            0 => assign_setting(&mut h.handshake_timeout_secs, value),
            1 => assign_setting(&mut h.disable_quic, value),
            2 => assign_setting(&mut h.disable_http1_1, value),
            _ => {}
        }
    }

    fn apply_auth(&mut self, index: usize, value: &SettingValue) {
        let a = &mut self.settings.auth;
        match index {
            0 => assign_setting(&mut a.rate_limit_rps, value),
            1 => assign_setting(&mut a.rate_limit_burst, value),
            _ => {}
        }
    }

    fn apply_instance(&mut self, index: usize, value: &SettingValue) {
        let i = &mut self.settings.instance;
        if index == 0
            && let SettingValue::String(v) = value
        {
            i.label = v.clone();
        }
    }

    /// Apply the features from this editor to a mutable `FeatureSettings` reference.
    /// Useful for propagating changes without restart.
    pub fn feature_settings(&self) -> &FeatureSettings {
        &self.settings.features
    }

    /// Clear the status message if it's been shown for more than 3 seconds.
    pub fn tick_status(&mut self) {
        if let Some((_, when)) = &self.status_message
            && when.elapsed() > std::time::Duration::from_secs(3)
        {
            self.status_message = None;
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// The panel opens on Tools now: the old first category (Features) held
    /// five switches nothing in the workspace reads, so a user's first
    /// interaction with settings was toggling controls wired to nothing.
    fn toggle_first_category_item() {
        let mut editor = SettingsEditor::default();
        assert_eq!(editor.current_category(), SettingsCategory::Tools);
        editor.item_down(); // index 1 = force_sync (bool)
        assert!(!editor.settings().tools.force_sync);
        editor.toggle_current();
        assert!(editor.settings().tools.force_sync);
        assert!(editor.dirty);
    }

    #[test]
    fn reset_to_default() {
        let mut editor = SettingsEditor::default();
        editor.item_down(); // Tools → force_sync (bool, default false)
        editor.toggle_current();
        assert!(editor.settings().tools.force_sync);
        editor.reset_current();
        assert!(!editor.settings().tools.force_sync);
    }

    #[test]
    fn security_tier_items_readonly() {
        let mut item = SettingItem {
            key: "sandbox.disable",
            label: "Disable sandbox",
            description: "test",
            value: SettingValue::Bool(false),
            default_value: SettingValue::Bool(false),
            security_tier: true,
        };
        assert!(!item.toggle()); // should not change
    }

    #[test]
    fn category_navigation() {
        let mut editor = SettingsEditor::default();
        assert_eq!(editor.current_category(), SettingsCategory::Tools);
        editor.category_down();
        assert_eq!(editor.current_category(), SettingsCategory::Sandbox);
        editor.category_up();
        assert_eq!(editor.current_category(), SettingsCategory::Tools);
        // Should not go below 0
        editor.category_up();
        assert_eq!(editor.current_category(), SettingsCategory::Tools);
    }

    #[test]
    fn save_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        let mut editor = SettingsEditor::default();
        editor.item_down(); // Tools → force_sync (bool)
        editor.toggle_current();

        editor.settings.save_to(&path).unwrap();

        let reloaded = AhmaSettings::load_from(&path);
        assert!(
            reloaded.tools.force_sync,
            "the edit round-trips through disk"
        );
    }

    /// A row that cannot be toggled must say so. Silence made most of the
    /// panel look broken: string and numeric rows swallowed Space with no
    /// feedback, and so did the security-tier rows.
    #[test]
    fn non_toggleable_rows_explain_themselves() {
        let mut editor = SettingsEditor::default();

        // Sandbox item 0 is security-tier (CLI-flag-only per R-CFG2.3).
        editor.category_down();
        assert_eq!(editor.current_category(), SettingsCategory::Sandbox);
        editor.toggle_current();
        let (msg, _) = editor.status_message.clone().expect("must explain");
        assert!(msg.contains("security-tier"), "got: {msg}");
        assert!(
            !editor.dirty,
            "and must not pretend to have changed anything"
        );

        // Logging item 0 is a String — editable, but only in the file.
        editor.status_message = None;
        editor.category_down();
        assert_eq!(editor.current_category(), SettingsCategory::Logging);
        editor.toggle_current();
        let (msg, _) = editor.status_message.clone().expect("must explain");
        assert!(msg.contains("settings.toml"), "got: {msg}");
        assert!(!editor.dirty);
    }

    /// Esc with unsaved changes asks before discarding — the footer already
    /// says "unsaved changes", so throwing them away on one keypress
    /// contradicts the panel's own message. A second Esc confirms.
    #[test]
    fn closing_dirty_asks_before_discarding() {
        let mut editor = SettingsEditor {
            open: true,
            ..Default::default()
        };
        editor.item_down();
        editor.toggle_current();
        assert!(editor.dirty);

        editor.close();
        assert!(editor.open, "first Esc must not discard");
        let (msg, _) = editor.status_message.clone().expect("must warn");
        assert!(msg.contains("Unsaved"), "got: {msg}");

        editor.close();
        assert!(!editor.open, "second Esc confirms the discard");
    }

    /// A clean panel closes on the first Esc — the confirmation is for unsaved
    /// work, not a toll on every exit.
    #[test]
    fn closing_clean_needs_no_confirmation() {
        let mut editor = SettingsEditor {
            open: true,
            ..Default::default()
        };
        editor.close();
        assert!(!editor.open);
    }

    #[test]
    fn items_count_per_category() {
        let editor = SettingsEditor::default();
        assert!(editor.items_for_category(SettingsCategory::Tools).len() >= 5);
        assert!(editor.items_for_category(SettingsCategory::Sandbox).len() >= 3);
    }

    // ── SettingsCategory label/icon (all variants) ────────────────────────

    #[test]
    fn category_label_and_icon_cover_all_variants() {
        // Features is intentionally absent from ALL: its switches were inert.
        // Markers are ASCII — SPEC R22.3 forbids emoji in terminal output.
        let expected = [
            (SettingsCategory::Tools, "Tools", "T"),
            (SettingsCategory::Sandbox, "Sandbox", "S"),
            (SettingsCategory::Logging, "Logging", "L"),
            (SettingsCategory::Http, "HTTP", "H"),
            (SettingsCategory::Auth, "Auth", "A"),
            (SettingsCategory::Instance, "Instance", "I"),
        ];
        assert_eq!(SettingsCategory::ALL.len(), expected.len());
        for (cat, label, icon) in expected {
            assert_eq!(cat.label(), label);
            assert_eq!(cat.icon(), icon);
            // Every category in ALL must appear with its expected label.
            assert!(SettingsCategory::ALL.contains(&cat));
        }
    }

    // ── SettingValue Display (every variant) ──────────────────────────────

    #[test]
    fn setting_value_display_all_variants() {
        assert_eq!(SettingValue::Bool(true).to_string(), "on");
        assert_eq!(SettingValue::Bool(false).to_string(), "off");
        assert_eq!(SettingValue::String("hi".into()).to_string(), "hi");
        assert_eq!(SettingValue::U64(42).to_string(), "42");
        assert_eq!(SettingValue::U32(7).to_string(), "7");
        assert_eq!(SettingValue::Usize(9).to_string(), "9");
        assert_eq!(SettingValue::StringList(vec![]).to_string(), "[]");
        assert_eq!(
            SettingValue::StringList(vec!["a".into(), "b".into()]).to_string(),
            "[a, b]"
        );
    }

    // ── SettingItem::is_modified across value types + mismatch arm ─────────

    #[test]
    fn setting_item_is_modified_all_arms() {
        fn item(value: SettingValue, default_value: SettingValue) -> SettingItem {
            SettingItem {
                key: "k",
                label: "l",
                description: "d",
                value,
                default_value,
                security_tier: false,
            }
        }
        // Equal => not modified.
        assert!(!item(SettingValue::Bool(true), SettingValue::Bool(true)).is_modified());
        assert!(
            !item(
                SettingValue::String("x".into()),
                SettingValue::String("x".into())
            )
            .is_modified()
        );
        assert!(!item(SettingValue::U64(1), SettingValue::U64(1)).is_modified());
        assert!(!item(SettingValue::U32(1), SettingValue::U32(1)).is_modified());
        assert!(!item(SettingValue::Usize(1), SettingValue::Usize(1)).is_modified());
        // Different value => modified.
        assert!(item(SettingValue::Bool(true), SettingValue::Bool(false)).is_modified());
        assert!(
            item(
                SettingValue::String("x".into()),
                SettingValue::String("y".into())
            )
            .is_modified()
        );
        assert!(item(SettingValue::U64(1), SettingValue::U64(2)).is_modified());
        assert!(item(SettingValue::U32(1), SettingValue::U32(2)).is_modified());
        assert!(item(SettingValue::Usize(1), SettingValue::Usize(2)).is_modified());
        // Mismatched types => `_ => true` arm.
        assert!(item(SettingValue::Bool(true), SettingValue::U64(1)).is_modified());
        assert!(
            item(
                SettingValue::StringList(vec![]),
                SettingValue::StringList(vec![])
            )
            .is_modified()
        );
    }

    // ── SettingItem::reset_to_default branches ────────────────────────────

    #[test]
    fn setting_item_reset_to_default_same_returns_false() {
        let mut it = SettingItem {
            key: "k",
            label: "l",
            description: "d",
            value: SettingValue::Bool(true),
            default_value: SettingValue::Bool(true),
            security_tier: false,
        };
        // Already equal => no change.
        assert!(!it.reset_to_default());
    }

    #[test]
    fn setting_item_reset_to_default_security_tier_returns_false() {
        let mut it = SettingItem {
            key: "k",
            label: "l",
            description: "d",
            value: SettingValue::Bool(true),
            default_value: SettingValue::Bool(false),
            security_tier: true,
        };
        assert!(!it.reset_to_default());
        // value untouched
        assert!(matches!(it.value, SettingValue::Bool(true)));
    }

    #[test]
    fn setting_item_reset_to_default_numeric_and_string_and_mismatch() {
        // String reset
        let mut s = SettingItem {
            key: "k",
            label: "l",
            description: "d",
            value: SettingValue::String("changed".into()),
            default_value: SettingValue::String("orig".into()),
            security_tier: false,
        };
        assert!(s.reset_to_default());
        assert_eq!(s.value.to_string(), "orig");

        // U64 reset
        let mut u = SettingItem {
            key: "k",
            label: "l",
            description: "d",
            value: SettingValue::U64(5),
            default_value: SettingValue::U64(9),
            security_tier: false,
        };
        assert!(u.reset_to_default());
        assert_eq!(u.value.to_string(), "9");

        // U32 reset
        let mut u32i = SettingItem {
            key: "k",
            label: "l",
            description: "d",
            value: SettingValue::U32(1),
            default_value: SettingValue::U32(2),
            security_tier: false,
        };
        assert!(u32i.reset_to_default());
        assert_eq!(u32i.value.to_string(), "2");

        // Usize reset
        let mut us = SettingItem {
            key: "k",
            label: "l",
            description: "d",
            value: SettingValue::Usize(3),
            default_value: SettingValue::Usize(4),
            security_tier: false,
        };
        assert!(us.reset_to_default());
        assert_eq!(us.value.to_string(), "4");

        // Mismatched-type pair hits the `_ => false` ("not same") arm, so reset
        // proceeds: the value is replaced with the default and `true` is returned.
        // (Consistent with `is_modified`'s `_ => true` for mismatched types.)
        let mut mismatch = SettingItem {
            key: "k",
            label: "l",
            description: "d",
            value: SettingValue::Bool(true),
            default_value: SettingValue::U64(1),
            security_tier: false,
        };
        assert!(mismatch.reset_to_default());
        assert_eq!(mismatch.value.to_string(), "1");
    }

    #[test]
    fn setting_item_toggle_non_bool_returns_false() {
        let mut it = SettingItem {
            key: "k",
            label: "l",
            description: "d",
            value: SettingValue::String("x".into()),
            default_value: SettingValue::String("x".into()),
            security_tier: false,
        };
        assert!(!it.toggle());
    }

    // ── open() / close() state management ─────────────────────────────────

    #[test]
    fn open_resets_state_and_close_clears() {
        let mut editor = SettingsEditor {
            selected_category: 3,
            selected_item: 2,
            dirty: true,
            editing: Some("partial".into()),
            status_message: Some(("stale".into(), std::time::Instant::now())),
            ..Default::default()
        };

        editor.open();
        assert!(editor.open);
        assert_eq!(editor.selected_category, 0);
        assert_eq!(editor.selected_item, 0);
        assert!(!editor.dirty);
        assert!(editor.editing.is_none());
        assert!(editor.status_message.is_none());

        editor.editing = Some("typing".into());
        editor.close();
        assert!(!editor.open);
        assert!(editor.editing.is_none());
    }

    // ── Item navigation within a category (bounds, no wrap) ───────────────

    #[test]
    fn item_navigation_clamps_at_bounds() {
        let mut editor = SettingsEditor::default();
        // Features has 5 items.
        assert_eq!(editor.selected_item, 0);
        editor.item_up(); // already at top, stays
        assert_eq!(editor.selected_item, 0);
        editor.item_down();
        assert_eq!(editor.selected_item, 1);
        editor.item_down();
        assert_eq!(editor.selected_item, 2);
        // Walk to the last item (index 4) and try to overshoot.
        editor.item_down();
        editor.item_down();
        assert_eq!(editor.selected_item, 4);
        editor.item_down(); // clamp at last
        assert_eq!(editor.selected_item, 4);
        editor.item_up();
        assert_eq!(editor.selected_item, 3);
    }

    #[test]
    fn category_down_at_last_stays_and_resets_item() {
        let mut editor = SettingsEditor::default();
        editor.item_down(); // selected_item = 1
        assert_eq!(editor.selected_item, 1);
        editor.category_down(); // moves to Sandbox and resets item
        assert_eq!(editor.current_category(), SettingsCategory::Sandbox);
        assert_eq!(editor.selected_item, 0);

        // Jump to the last category and confirm category_down is a no-op there.
        for _ in 0..10 {
            editor.category_down();
        }
        assert_eq!(editor.current_category(), SettingsCategory::Instance);
        let last = editor.selected_category;
        editor.category_down();
        assert_eq!(editor.selected_category, last);
    }

    // ── items_for_category for every category (counts) ────────────────────

    #[test]
    fn items_for_category_counts_all_categories() {
        let editor = SettingsEditor::default();
        assert_eq!(
            editor.items_for_category(SettingsCategory::Features).len(),
            5
        );
        assert_eq!(editor.items_for_category(SettingsCategory::Tools).len(), 5);
        assert_eq!(
            editor.items_for_category(SettingsCategory::Sandbox).len(),
            8
        );
        assert_eq!(
            editor.items_for_category(SettingsCategory::Logging).len(),
            4
        );
        assert_eq!(editor.items_for_category(SettingsCategory::Http).len(), 3);
        assert_eq!(editor.items_for_category(SettingsCategory::Auth).len(), 2);
        assert_eq!(
            editor.items_for_category(SettingsCategory::Instance).len(),
            1
        );
    }

    // ── toggle_current on security-tier item makes no change ──────────────

    #[test]
    fn toggle_current_security_tier_is_noop() {
        let mut editor = SettingsEditor::default();
        // Navigate to Sandbox (index 1); item 0 = disable (security_tier).
        editor.category_down();
        assert_eq!(editor.current_category(), SettingsCategory::Sandbox);
        let before = editor.settings().sandbox.disable;
        editor.toggle_current();
        assert_eq!(editor.settings().sandbox.disable, before);
        assert!(!editor.dirty);
    }

    #[test]
    fn toggle_current_tools_bool_field() {
        let mut editor = SettingsEditor::default();
        assert_eq!(editor.current_category(), SettingsCategory::Tools);
        editor.item_down(); // index 1 = force_sync (bool)
        assert!(!editor.settings().tools.force_sync);
        editor.toggle_current();
        assert!(editor.settings().tools.force_sync);
        assert!(editor.dirty);
    }

    #[test]
    fn reset_current_noop_when_already_default() {
        let mut editor = SettingsEditor::default();
        // Fresh editor: every value already equals its default.
        editor.reset_current();
        assert!(!editor.dirty);
    }

    #[test]
    fn reset_current_restores_modified_string_field() {
        let mut editor = SettingsEditor::default();
        // Go to Logging (index 2); item 0 = target (String, default "file").
        editor.category_down();
        editor.category_down();
        assert_eq!(editor.current_category(), SettingsCategory::Logging);
        editor.apply_logging(0, &SettingValue::String("stderr".into()));
        assert_eq!(editor.settings().logging.target, "stderr");
        editor.reset_current();
        assert_eq!(editor.settings().logging.target, "file");
        assert!(editor.dirty);
    }

    // ── apply_* per-index, wrong-type, and out-of-range branches ──────────

    #[test]
    fn apply_feature_all_indices_and_guards() {
        let mut e = SettingsEditor::default();
        e.apply_feature(0, &SettingValue::Bool(false));
        e.apply_feature(1, &SettingValue::Bool(true));
        e.apply_feature(2, &SettingValue::Bool(false));
        e.apply_feature(3, &SettingValue::Bool(false));
        e.apply_feature(4, &SettingValue::Bool(false));
        assert!(!e.settings().features.simplify);
        assert!(e.settings().features.vault);
        assert!(!e.settings().features.egress);
        assert!(!e.settings().features.artifact);
        assert!(!e.settings().features.decompose);
        // Out-of-range index: no-op, no panic.
        e.apply_feature(99, &SettingValue::Bool(true));
        // Wrong value type: early return guard.
        let before = e.settings().features.vault;
        e.apply_feature(1, &SettingValue::U64(1));
        assert_eq!(e.settings().features.vault, before);
    }

    #[test]
    fn apply_tool_all_indices_and_guards() {
        let mut e = SettingsEditor::default();
        e.apply_tool(0, &SettingValue::U64(123));
        e.apply_tool(1, &SettingValue::Bool(true));
        e.apply_tool(2, &SettingValue::Bool(true));
        e.apply_tool(3, &SettingValue::Bool(true));
        e.apply_tool(4, &SettingValue::Bool(true));
        let t = &e.settings().tools;
        assert_eq!(t.timeout_secs, 123);
        assert!(t.force_sync);
        assert!(t.skip_probes);
        assert!(t.minimize_tokens);
        assert!(t.small_model_harness);
        // Wrong value types are ignored for each arm.
        e.apply_tool(0, &SettingValue::Bool(true));
        assert_eq!(e.settings().tools.timeout_secs, 123);
        e.apply_tool(1, &SettingValue::U64(0));
        assert!(e.settings().tools.force_sync);
        // Out-of-range index.
        e.apply_tool(42, &SettingValue::Bool(true));
    }

    #[test]
    fn apply_sandbox_all_indices_and_guards() {
        let mut e = SettingsEditor::default();
        e.apply_sandbox(0, &SettingValue::Bool(true));
        e.apply_sandbox(1, &SettingValue::Bool(true));
        e.apply_sandbox(2, &SettingValue::Bool(true));
        e.apply_sandbox(3, &SettingValue::Bool(true));
        e.apply_sandbox(4, &SettingValue::Bool(false));
        e.apply_sandbox(5, &SettingValue::Bool(false));
        // The two trust-handoff hatches are default-off, so "changed" is `true`.
        e.apply_sandbox(6, &SettingValue::Bool(true));
        e.apply_sandbox(7, &SettingValue::Bool(true));
        let s = &e.settings().sandbox;
        assert!(s.disable);
        assert!(s.tmp_access);
        assert!(s.disable_temp);
        assert!(s.defer);
        assert!(!s.package_cache_write);
        assert!(!s.allow_keychain);
        assert!(s.allow_git_hooks);
        assert!(s.allow_project_tool_config);
        // Non-bool early return guard.
        e.apply_sandbox(0, &SettingValue::U64(1));
        assert!(e.settings().sandbox.disable);
        // Out-of-range index.
        e.apply_sandbox(99, &SettingValue::Bool(false));
    }

    #[test]
    fn apply_logging_all_indices_and_guards() {
        let mut e = SettingsEditor::default();
        e.apply_logging(0, &SettingValue::String("stderr".into()));
        e.apply_logging(1, &SettingValue::Bool(true));
        e.apply_logging(2, &SettingValue::U64(15));
        e.apply_logging(3, &SettingValue::String("~/.ahma/logs".into()));
        let l = &e.settings().logging;
        assert_eq!(l.target, "stderr");
        assert!(l.log_monitor);
        assert_eq!(l.monitor_rate_limit_secs, 15);
        assert_eq!(l.dir, "~/.ahma/logs");
        // Wrong types ignored.
        e.apply_logging(0, &SettingValue::Bool(true));
        assert_eq!(e.settings().logging.target, "stderr");
        e.apply_logging(2, &SettingValue::Bool(true));
        assert_eq!(e.settings().logging.monitor_rate_limit_secs, 15);
        e.apply_logging(3, &SettingValue::Bool(true));
        assert_eq!(e.settings().logging.dir, "~/.ahma/logs");
        // Out-of-range.
        e.apply_logging(7, &SettingValue::U64(1));
    }

    #[test]
    fn apply_http_all_indices_and_guards() {
        let mut e = SettingsEditor::default();
        e.apply_http(0, &SettingValue::U64(99));
        e.apply_http(1, &SettingValue::Bool(true));
        e.apply_http(2, &SettingValue::Bool(true));
        let h = &e.settings().http;
        assert_eq!(h.handshake_timeout_secs, 99);
        assert!(h.disable_quic);
        assert!(h.disable_http1_1);
        // Wrong type ignored.
        e.apply_http(0, &SettingValue::Bool(false));
        assert_eq!(e.settings().http.handshake_timeout_secs, 99);
        // Out-of-range.
        e.apply_http(9, &SettingValue::U64(1));
    }

    #[test]
    fn apply_auth_all_indices_and_guards() {
        let mut e = SettingsEditor::default();
        e.apply_auth(0, &SettingValue::U64(50));
        e.apply_auth(1, &SettingValue::U32(20));
        let a = &e.settings().auth;
        assert_eq!(a.rate_limit_rps, 50);
        assert_eq!(a.rate_limit_burst, 20);
        // Wrong types ignored.
        e.apply_auth(0, &SettingValue::Bool(true));
        assert_eq!(e.settings().auth.rate_limit_rps, 50);
        e.apply_auth(1, &SettingValue::U64(1));
        assert_eq!(e.settings().auth.rate_limit_burst, 20);
        // Out-of-range.
        e.apply_auth(5, &SettingValue::U64(1));
    }

    #[test]
    fn apply_instance_index_and_guards() {
        let mut e = SettingsEditor::default();
        e.apply_instance(0, &SettingValue::String("worker-1".into()));
        assert_eq!(e.settings().instance.label, "worker-1");
        // Wrong type ignored.
        e.apply_instance(0, &SettingValue::U64(1));
        assert_eq!(e.settings().instance.label, "worker-1");
        // Out-of-range index ignored.
        e.apply_instance(3, &SettingValue::String("nope".into()));
        assert_eq!(e.settings().instance.label, "worker-1");
    }

    #[test]
    fn apply_item_to_settings_dispatches_each_category() {
        let mut e = SettingsEditor::default();
        e.apply_item_to_settings(SettingsCategory::Features, 0, &SettingValue::Bool(false));
        e.apply_item_to_settings(SettingsCategory::Tools, 0, &SettingValue::U64(7));
        e.apply_item_to_settings(SettingsCategory::Sandbox, 1, &SettingValue::Bool(true));
        e.apply_item_to_settings(
            SettingsCategory::Logging,
            0,
            &SettingValue::String("stderr".into()),
        );
        e.apply_item_to_settings(SettingsCategory::Http, 0, &SettingValue::U64(11));
        e.apply_item_to_settings(SettingsCategory::Auth, 0, &SettingValue::U64(3));
        e.apply_item_to_settings(
            SettingsCategory::Instance,
            0,
            &SettingValue::String("n".into()),
        );
        assert!(!e.settings().features.simplify);
        assert_eq!(e.settings().tools.timeout_secs, 7);
        assert!(e.settings().sandbox.tmp_access);
        assert_eq!(e.settings().logging.target, "stderr");
        assert_eq!(e.settings().http.handshake_timeout_secs, 11);
        assert_eq!(e.settings().auth.rate_limit_rps, 3);
        assert_eq!(e.settings().instance.label, "n");
    }

    #[test]
    fn accessors_settings_and_feature_settings() {
        let editor = SettingsEditor::default();
        // Both accessors point at the same underlying features.
        assert_eq!(
            editor.settings().features.simplify,
            editor.feature_settings().simplify
        );
    }

    // ── save() via the AHMA_TEST_HOME debug seam (success path) ────────────

    #[test]
    fn save_writes_to_home_seam_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: debug-only test seam; nextest isolates each test in its own process.
        let _home = crate::HOME_SEAM_GUARD.lock();
        unsafe {
            std::env::set_var("AHMA_TEST_HOME", dir.path());
        }
        let mut editor = SettingsEditor::default();
        // Mutate a value, then persist via the no-arg save() (uses settings_path()).
        editor.item_down(); // Tools → force_sync (bool, default false)
        editor.toggle_current();
        editor.dirty = true;
        editor.save();

        assert!(!editor.dirty, "save() should clear dirty");
        let (msg, _) = editor
            .status_message
            .clone()
            .expect("status set after save");
        assert!(msg.contains("Saved"), "unexpected status: {msg}");

        // Round-trip: a fresh load through the same home seam sees the change.
        let reloaded = AhmaSettings::load();
        assert!(reloaded.tools.force_sync);

        unsafe {
            std::env::remove_var("AHMA_TEST_HOME");
        }
    }

    // ── tick_status: clears stale, keeps fresh ─────────────────────────────

    #[test]
    fn tick_status_clears_stale_keeps_fresh() {
        let mut editor = SettingsEditor::default();

        // Stale message (>3s old) is cleared.
        let stale = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(10))
            .expect("monotonic clock far enough from epoch");
        editor.status_message = Some(("old".into(), stale));
        editor.tick_status();
        assert!(editor.status_message.is_none());

        // Fresh message is retained.
        editor.status_message = Some(("new".into(), std::time::Instant::now()));
        editor.tick_status();
        assert!(editor.status_message.is_some());

        // No message: tick_status is a harmless no-op.
        editor.status_message = None;
        editor.tick_status();
        assert!(editor.status_message.is_none());
    }
}
