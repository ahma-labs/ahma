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

    // ── SettingsCategory label/icon (all variants) ────────────────────────

    #[test]
    fn category_label_and_icon_cover_all_variants() {
        let expected = [
            (SettingsCategory::Features, "Features", "⚡"),
            (SettingsCategory::Tools, "Tools", "🔧"),
            (SettingsCategory::Sandbox, "Sandbox", "🔒"),
            (SettingsCategory::Logging, "Logging", "📋"),
            (SettingsCategory::Http, "HTTP", "🌐"),
            (SettingsCategory::Auth, "Auth", "🔑"),
            (SettingsCategory::Instance, "Instance", "🏷"),
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
        // Features has 6 items.
        assert_eq!(editor.selected_item, 0);
        editor.item_up(); // already at top, stays
        assert_eq!(editor.selected_item, 0);
        editor.item_down();
        assert_eq!(editor.selected_item, 1);
        editor.item_down();
        assert_eq!(editor.selected_item, 2);
        // Walk to the last item (index 5) and try to overshoot.
        editor.item_down();
        editor.item_down();
        editor.item_down();
        assert_eq!(editor.selected_item, 5);
        editor.item_down(); // clamp at last
        assert_eq!(editor.selected_item, 5);
        editor.item_up();
        assert_eq!(editor.selected_item, 4);
    }

    #[test]
    fn category_down_at_last_stays_and_resets_item() {
        let mut editor = SettingsEditor::default();
        editor.item_down(); // selected_item = 1
        assert_eq!(editor.selected_item, 1);
        editor.category_down(); // moves to Tools and resets item
        assert_eq!(editor.current_category(), SettingsCategory::Tools);
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
            6
        );
        assert_eq!(editor.items_for_category(SettingsCategory::Tools).len(), 6);
        assert_eq!(
            editor.items_for_category(SettingsCategory::Sandbox).len(),
            6
        );
        assert_eq!(
            editor.items_for_category(SettingsCategory::Logging).len(),
            3
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
        // Navigate to Sandbox (index 2); item 0 = disable (security_tier).
        editor.category_down();
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
        editor.category_down(); // Tools
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
        // Go to Logging (index 3); item 0 = target (String, default "file").
        editor.category_down();
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
        e.apply_feature(2, &SettingValue::Bool(true));
        e.apply_feature(3, &SettingValue::Bool(false));
        e.apply_feature(4, &SettingValue::Bool(false));
        e.apply_feature(5, &SettingValue::Bool(false));
        assert!(!e.settings().features.simplify);
        assert!(e.settings().features.vault);
        assert!(e.settings().features.cluster);
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
        e.apply_tool(5, &SettingValue::Bool(true));
        let t = &e.settings().tools;
        assert_eq!(t.timeout_secs, 123);
        assert!(t.force_sync);
        assert!(t.hot_reload);
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
        let s = &e.settings().sandbox;
        assert!(s.disable);
        assert!(s.tmp_access);
        assert!(s.disable_temp);
        assert!(s.defer);
        assert!(!s.package_cache_write);
        assert!(!s.allow_keychain);
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
        let l = &e.settings().logging;
        assert_eq!(l.target, "stderr");
        assert!(l.log_monitor);
        assert_eq!(l.monitor_rate_limit_secs, 15);
        // Wrong types ignored.
        e.apply_logging(0, &SettingValue::Bool(true));
        assert_eq!(e.settings().logging.target, "stderr");
        e.apply_logging(2, &SettingValue::Bool(true));
        assert_eq!(e.settings().logging.monitor_rate_limit_secs, 15);
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
        unsafe {
            std::env::set_var("AHMA_TEST_HOME", dir.path());
        }
        let mut editor = SettingsEditor::default();
        // Mutate a value, then persist via the no-arg save() (uses settings_path()).
        editor.toggle_current(); // features.simplify -> false
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
        assert!(!reloaded.features.simplify);

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
