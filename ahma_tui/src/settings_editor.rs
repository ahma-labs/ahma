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
        Self::Features,
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

    pub fn icon(self) -> &'static str {
        match self {
            Self::Features => "⚡",
            Self::Tools => "🔧",
            Self::Sandbox => "🔒",
            Self::Logging => "📋",
            Self::Http => "🌐",
            Self::Auth => "🔑",
            Self::Instance => "🏷",
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

    /// Close the settings panel.
    pub fn close(&mut self) {
        self.open = false;
        self.editing = None;
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
    pub fn toggle_current(&mut self) {
        let category = self.current_category();
        let mut items = self.items_for_category(category);
        if let Some(item) = items.get_mut(self.selected_item)
            && item.toggle()
        {
            self.apply_item_to_settings(category, self.selected_item, &item.value);
            self.dirty = true;
        }
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
                key: "features.cluster",
                label: "Cluster",
                description: "Distributed scheduling (requires peer setup)",
                value: SettingValue::Bool(f.cluster),
                default_value: SettingValue::Bool(d.cluster),
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
                key: "tools.hot_reload",
                label: "Hot reload",
                description: "Reload tool JSON on file change",
                value: SettingValue::Bool(t.hot_reload),
                default_value: SettingValue::Bool(d.hot_reload),
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
            SettingItem {
                key: "tools.separate_cargo_target",
                label: "Separate cargo target",
                description: "Use target/ahma/ to avoid IDE contention",
                value: SettingValue::Bool(t.separate_cargo_target),
                default_value: SettingValue::Bool(d.separate_cargo_target),
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
            2 => f.cluster = *v,
            3 => f.egress = *v,
            4 => f.artifact = *v,
            5 => f.decompose = *v,
            _ => {}
        }
    }

    fn apply_tool(&mut self, index: usize, value: &SettingValue) {
        let t = &mut self.settings.tools;
        match index {
            0 => {
                if let SettingValue::U64(v) = value {
                    t.timeout_secs = *v;
                }
            }
            1 => {
                if let SettingValue::Bool(v) = value {
                    t.force_sync = *v;
                }
            }
            2 => {
                if let SettingValue::Bool(v) = value {
                    t.hot_reload = *v;
                }
            }
            3 => {
                if let SettingValue::Bool(v) = value {
                    t.skip_probes = *v;
                }
            }
            4 => {
                if let SettingValue::Bool(v) = value {
                    t.minimize_tokens = *v;
                }
            }
            5 => {
                if let SettingValue::Bool(v) = value {
                    t.small_model_harness = *v;
                }
            }
            6 => {
                if let SettingValue::Bool(v) = value {
                    t.separate_cargo_target = *v;
                }
            }
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
            _ => {}
        }
    }

    fn apply_logging(&mut self, index: usize, value: &SettingValue) {
        let l = &mut self.settings.logging;
        match index {
            0 => {
                if let SettingValue::String(v) = value {
                    l.target = v.clone();
                }
            }
            1 => {
                if let SettingValue::Bool(v) = value {
                    l.log_monitor = *v;
                }
            }
            2 => {
                if let SettingValue::U64(v) = value {
                    l.monitor_rate_limit_secs = *v;
                }
            }
            _ => {}
        }
    }

    fn apply_http(&mut self, index: usize, value: &SettingValue) {
        let h = &mut self.settings.http;
        match index {
            0 => {
                if let SettingValue::U64(v) = value {
                    h.handshake_timeout_secs = *v;
                }
            }
            1 => {
                if let SettingValue::Bool(v) = value {
                    h.disable_quic = *v;
                }
            }
            2 => {
                if let SettingValue::Bool(v) = value {
                    h.disable_http1_1 = *v;
                }
            }
            _ => {}
        }
    }

    fn apply_auth(&mut self, index: usize, value: &SettingValue) {
        let a = &mut self.settings.auth;
        match index {
            0 => {
                if let SettingValue::U64(v) = value {
                    a.rate_limit_rps = *v;
                }
            }
            1 => {
                if let SettingValue::U32(v) = value {
                    a.rate_limit_burst = *v;
                }
            }
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
    fn toggle_feature() {
        let mut editor = SettingsEditor::default();
        assert!(editor.settings().features.simplify);
        // selected_category=0 and selected_item=0 are already the defaults
        editor.toggle_current();
        assert!(!editor.settings().features.simplify);
        assert!(editor.dirty);
    }

    #[test]
    fn reset_to_default() {
        let mut editor = SettingsEditor::default();
        // selected_category=0 and selected_item=0 are already the defaults
        editor.toggle_current(); // simplify → false
        assert!(!editor.settings().features.simplify);
        editor.reset_current();
        assert!(editor.settings().features.simplify);
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
        assert_eq!(editor.current_category(), SettingsCategory::Features);
        editor.category_down();
        assert_eq!(editor.current_category(), SettingsCategory::Tools);
        editor.category_up();
        assert_eq!(editor.current_category(), SettingsCategory::Features);
        // Should not go below 0
        editor.category_up();
        assert_eq!(editor.current_category(), SettingsCategory::Features);
    }

    #[test]
    fn save_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");

        let mut editor = SettingsEditor::default();
        // selected_category=0 and selected_item=0 are already the defaults
        editor.toggle_current();

        editor.settings.save_to(&path).unwrap();

        let reloaded = AhmaSettings::load_from(&path);
        assert!(!reloaded.features.simplify);
    }

    #[test]
    fn items_count_per_category() {
        let editor = SettingsEditor::default();
        assert_eq!(
            editor.items_for_category(SettingsCategory::Features).len(),
            6
        );
        assert!(editor.items_for_category(SettingsCategory::Tools).len() >= 5);
        assert!(editor.items_for_category(SettingsCategory::Sandbox).len() >= 3);
    }
}
