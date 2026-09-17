//! Shared `SubcommandConfig` builders for unit tests.
//!
//! These were previously copy-pasted verbatim across several `#[cfg(test)]`
//! modules (`shell::resolution`, `mcp_service::subcommand`).

use crate::config::SubcommandConfig;

pub fn make_subcommand(name: &str, enabled: bool) -> SubcommandConfig {
    SubcommandConfig {
        extra: Default::default(),
        mutates: None,
        name: name.to_string(),
        description: format!("{} subcommand", name),
        enabled,
        ..Default::default()
    }
}

pub fn make_subcommand_with_nested(
    name: &str,
    enabled: bool,
    nested: Vec<SubcommandConfig>,
) -> SubcommandConfig {
    SubcommandConfig {
        extra: Default::default(),
        mutates: None,
        name: name.to_string(),
        description: format!("{} subcommand", name),
        subcommand: Some(nested),
        enabled,
        ..Default::default()
    }
}
