//! `ahma policy`: team policy in Cedar (SPEC R-POLICY.3–5). Reads and prints;
//! writes nothing.

use ahma_common::policy::{Action, Request, Resource};
use ahma_policy::{TeamPolicy, default_dirs, host_chain, policy_files};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// `ahma policy validate [FILE…]`: the files given, or every file ahma would
/// load. Exit 0 only when every one would load.
pub(crate) fn run_validate(files: &[PathBuf]) -> Result<()> {
    let files = if files.is_empty() {
        policy_files(&default_dirs())?
    } else {
        files.to_vec()
    };
    if files.is_empty() {
        println!("No team policy files in:");
        for dir in default_dirs() {
            println!("  {}", dir.display());
        }
        return Ok(());
    }
    let mut sources = Vec::new();
    for f in &files {
        sources.push((f.clone(), std::fs::read_to_string(f)?));
    }
    match TeamPolicy::from_sources(&sources) {
        Ok(policy) => {
            println!(
                "OK: {} forbid rule(s) in {} file(s).",
                policy.rules().len(),
                files.len()
            );
            Ok(())
        }
        Err(e) => bail!("team policy would not load, so ahma would refuse everything: {e}"),
    }
}

/// `ahma policy list`: every loaded rule, where it comes from, and its reason.
pub(crate) fn run_list() -> Result<()> {
    let dirs = default_dirs();
    println!("Team policy is read from:");
    for dir in &dirs {
        println!("  {}", dir.display());
    }
    match TeamPolicy::load(&dirs)? {
        None => println!("No policy files: nothing is forbidden beyond ahma's own rules."),
        Some(policy) => {
            for rule in policy.rules() {
                println!("\n{}  ({})", rule.id, rule.file.display());
                if let Some(reason) = &rule.reason {
                    println!("  reason: {reason}");
                }
                for line in rule.text.lines() {
                    println!("  | {line}");
                }
            }
        }
    }
    Ok(())
}

/// `ahma policy explain ACTION RESOURCE [--workspace DIR]`: whether team
/// policy forbids it, and which rule says so.
pub(crate) fn run_explain(action: &str, resource: &str, workspace: Option<&Path>) -> Result<()> {
    let action = parse_action(action)?;
    let target = Target::parse(action, resource)?;
    let resource = target.as_resource();
    let mut request = Request::new(action, resource);
    if let Some(ws) = workspace {
        request = request.in_workspace(ws);
    }
    let Some(policy) = TeamPolicy::load(&default_dirs())? else {
        println!("No team policy files: team policy forbids nothing.");
        return Ok(());
    };
    match policy.evaluate(&request) {
        Some((id, reason)) => {
            println!("FORBIDDEN by `{id}`: {reason}");
            println!("No grant or answer can allow it while that rule is loaded.");
        }
        None => println!(
            "Not forbidden by team policy. ahma's own rules and your grants still decide it."
        ),
    }
    Ok(())
}

fn parse_action(name: &str) -> Result<Action> {
    const ALL: [Action; 7] = [
        Action::Read,
        Action::Write,
        Action::Connect,
        Action::Fetch,
        Action::OpenLink,
        Action::SshSign,
        Action::CallTool,
    ];
    ALL.into_iter().find(|a| a.as_str() == name).ok_or_else(|| {
        let names: Vec<&str> = ALL.iter().map(|a| a.as_str()).collect();
        anyhow::anyhow!("unknown action `{name}`; one of: {}", names.join(", "))
    })
}

/// The resource as the decision sees it: a canonical path, a lower-case host,
/// or a tool name.
enum Target {
    Path(PathBuf),
    Host(String),
    Tool(String),
}

impl Target {
    fn parse(action: Action, raw: &str) -> Result<Self> {
        Ok(match action {
            Action::Read | Action::Write => {
                let p = PathBuf::from(raw);
                Target::Path(dunce::canonicalize(&p).unwrap_or(p))
            }
            Action::CallTool => Target::Tool(raw.to_string()),
            _ => {
                let host = raw
                    .split_once("://")
                    .map_or(raw, |(_, rest)| rest)
                    .split(['/', ':', '?', '#'])
                    .next()
                    .unwrap_or_default();
                if host.is_empty() {
                    bail!("`{raw}` names no host");
                }
                Target::Host(host_chain(host).remove(0))
            }
        })
    }

    fn as_resource(&self) -> Resource<'_> {
        match self {
            Target::Path(p) => Resource::Path(p),
            Target::Host(h) => Resource::Host(h),
            Target::Tool(t) => Resource::Tool(t),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_or_a_host_names_the_same_host() {
        for raw in [
            "https://API.Example.com:443/x?y",
            "api.example.com",
            "api.example.com/x",
        ] {
            match Target::parse(Action::Fetch, raw).unwrap() {
                Target::Host(h) => assert_eq!(h, "api.example.com", "{raw}"),
                _ => panic!("{raw} is a host"),
            }
        }
        assert!(Target::parse(Action::Fetch, "https://").is_err());
    }

    #[test]
    fn an_unknown_action_names_the_known_ones() {
        let err = parse_action("delete").unwrap_err().to_string();
        assert!(err.contains("call_tool") && err.contains("read"), "{err}");
    }
}
