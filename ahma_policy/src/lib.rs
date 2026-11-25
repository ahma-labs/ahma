//! Team policy in Cedar for ahma (SPEC R-POLICY.3–5). Experimental.
//!
//! A team, or one careful person, writes Cedar `forbid` statements in a
//! trusted directory. ahma loads them once at startup and consults them,
//! through [`ahma_common::policy::PolicyOverlay`], before every userspace
//! permission decision: a grant, an egress host, a fetched URL, a link, an
//! SSH signature, a tool approval. They can only **narrow** what ahma allows:
//! a `permit` statement is refused at load, so no file can widen anything.
//!
//! What this does not do: Cedar is not evaluated per system call. The kernel
//! sandbox is built from what ahma admits, so a forbid reaches the kernel only
//! where ahma turns it into a profile rule (SPEC R-POLICY.4).

pub mod export;

use ahma_common::policy::{Action, PolicyOverlay, Request, Resource};
use cedar_policy::{
    Authorizer, Context, Decision, Effect, Entities, Entity, EntityId, EntityTypeName, EntityUid,
    Policy, PolicyId, PolicySet, RestrictedExpression, Schema, ValidationMode, Validator,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// The schema every policy file is validated against, strictly.
pub const SCHEMA: &str = include_str!("../ahma.cedarschema");

/// The id of the one built-in `permit`, which makes "a forbid matched" the
/// only way a request is denied.
const PERMIT_ALL: &str = "ahma-builtin-permit-all";

/// Why the policy files could not be used. Any one of these fails closed.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot read {}: {source}", path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: not valid Cedar: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error(
        "{}: `{id}` is a permit; team policy may only forbid, so it can never widen what ahma allows",
        path.display()
    )]
    Permit { path: PathBuf, id: String },
    #[error("the policy files do not match ahma's schema: {0}")]
    Invalid(String),
    #[error("ahma's built-in schema is broken: {0}")]
    Schema(String),
}

/// One loaded forbid, for `ahma policy list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub id: String,
    pub file: PathBuf,
    pub reason: Option<String>,
    pub text: String,
}

/// Loaded, validated team policy.
pub struct TeamPolicy {
    policies: PolicySet,
    schema: Schema,
    rules: Vec<Rule>,
    kernel: ahma_common::policy::KernelDenies,
}

impl std::fmt::Debug for TeamPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamPolicy")
            .field("rules", &self.rules)
            .finish()
    }
}

/// The directories team policy is read from, user first. Both sit in the
/// trusted tier (SPEC R-CFG2): never a project's `.ahma/`, which an agent can
/// write.
pub fn default_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = ahma_common::config::ahma_home_dir() {
        dirs.push(home.join(".ahma").join("policy"));
    }
    dirs.push(system_dir());
    dirs
}

/// The machine-wide policy directory, for an administrator or MDM.
pub fn system_dir() -> PathBuf {
    ahma_common::policy::system_policy_dir()
}

/// Every `*.cedar` file in `dirs`, in a stable order. A directory that does
/// not exist contributes nothing.
pub fn policy_files(dirs: &[PathBuf]) -> Result<Vec<PathBuf>, LoadError> {
    let mut files = Vec::new();
    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(LoadError::Read {
                    path: dir.clone(),
                    source,
                });
            }
        };
        let mut here: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "cedar"))
            .collect();
        here.sort();
        files.extend(here);
    }
    Ok(files)
}

impl TeamPolicy {
    /// Load the policy in `dirs`. `Ok(None)` when there are no policy files.
    pub fn load(dirs: &[PathBuf]) -> Result<Option<Self>, LoadError> {
        let files = policy_files(dirs)?;
        if files.is_empty() {
            return Ok(None);
        }
        let mut sources = Vec::new();
        for path in files {
            let text = std::fs::read_to_string(&path).map_err(|source| LoadError::Read {
                path: path.clone(),
                source,
            })?;
            sources.push((path, text));
        }
        Self::from_sources(&sources).map(Some)
    }

    /// Parse and validate `(file, text)` pairs.
    pub fn from_sources(sources: &[(PathBuf, String)]) -> Result<Self, LoadError> {
        let (schema, _warnings) =
            Schema::from_cedarschema_str(SCHEMA).map_err(|e| LoadError::Schema(e.to_string()))?;
        let mut policies = PolicySet::new();
        let mut rules = Vec::new();
        for (path, text) in sources {
            let parsed = PolicySet::from_str(text).map_err(|e| LoadError::Parse {
                path: path.clone(),
                message: e.to_string(),
            })?;
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            for (n, policy) in parsed.policies().enumerate() {
                let id = policy
                    .annotation("id")
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{stem}#{n}"));
                if policy.effect() == Effect::Permit {
                    return Err(LoadError::Permit {
                        path: path.clone(),
                        id,
                    });
                }
                let renamed = policy.new_id(PolicyId::new(&id));
                rules.push(Rule {
                    id: id.clone(),
                    file: path.clone(),
                    reason: policy.annotation("reason").map(str::to_string),
                    text: policy.to_string(),
                });
                policies
                    .add(renamed)
                    .map_err(|e| LoadError::Invalid(format!("{}: {e}", path.display())))?;
            }
        }
        let validation = Validator::new(schema.clone()).validate(&policies, ValidationMode::Strict);
        if !validation.validation_passed() {
            let errors: Vec<String> = validation
                .validation_errors()
                .map(|e| e.to_string())
                .collect();
            return Err(LoadError::Invalid(errors.join("; ")));
        }
        let kernel = kernel_denies_of(&policies);
        let permit = Policy::parse(
            Some(PolicyId::new(PERMIT_ALL)),
            "permit(principal, action, resource);",
        )
        .map_err(|e| LoadError::Schema(e.to_string()))?;
        policies
            .add(permit)
            .map_err(|e| LoadError::Schema(e.to_string()))?;
        Ok(Self {
            policies,
            schema,
            rules,
            kernel,
        })
    }

    /// The loaded forbids.
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    /// The forbids the kernel can hold (SPEC R-POLICY.4).
    pub fn kernel_denies(&self) -> ahma_common::policy::KernelDenies {
        self.kernel.clone()
    }

    /// The forbid that denies `request`, if one does: its id and its reason.
    pub fn evaluate(&self, request: &Request<'_>) -> Option<(String, String)> {
        let built = self.cedar_request(request)?;
        let response =
            Authorizer::new().is_authorized(&built.request, &self.policies, &built.entities);
        if response.decision() == Decision::Allow {
            return None;
        }
        let id = response
            .diagnostics()
            .reason()
            .map(|id| id.to_string())
            .find(|id| id != PERMIT_ALL)
            .unwrap_or_else(|| "team policy".to_string());
        let reason = self
            .rules
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.reason.clone())
            .unwrap_or_else(|| format!("forbidden by team policy `{id}`"));
        Some((id, reason))
    }

    fn cedar_request(&self, request: &Request<'_>) -> Option<Built> {
        let mut entities = Vec::new();
        let mut attrs = HashMap::new();
        if let Some(ws) = request.principal.workspace {
            attrs.insert(
                "workspace".to_string(),
                RestrictedExpression::new_string(ws.display().to_string()),
            );
        }
        let principal = uid("Agent", "agent")?;
        entities.push(Entity::new(principal.clone(), attrs, HashSet::new()).ok()?);
        let resource = match request.resource {
            Resource::Path(p) => chain("Path", &path_chain(p), &mut entities)?,
            Resource::Host(h) => chain("Host", &host_chain(h), &mut entities)?,
            Resource::Tool(t) => leaf("Tool", t, &mut entities)?,
            Resource::Namespace(n) => leaf("Namespace", n, &mut entities)?,
        };
        let action = uid("Action", action_name(request.action))?;
        let request = cedar_policy::Request::new(
            principal,
            action,
            resource,
            Context::empty(),
            Some(&self.schema),
        )
        .ok()?;
        let entities = Entities::from_entities(entities, Some(&self.schema)).ok()?;
        Some(Built { request, entities })
    }
}

struct Built {
    request: cedar_policy::Request,
    entities: Entities,
}

impl PolicyOverlay for TeamPolicy {
    fn kernel_denies(&self) -> ahma_common::policy::KernelDenies {
        self.kernel.clone()
    }

    fn forbids(&self, request: &Request<'_>) -> Option<String> {
        // A request the schema cannot express (an action applied to a resource
        // it does not take) is not something a forbid can name; it is allowed
        // through to ahma's own decision rather than refused for a typo here.
        self.evaluate(request).map(|(_, reason)| reason)
    }
}

/// Policy that failed to load. It forbids everything, so ahma fails closed
/// until the files are fixed (SPEC R-POLICY.3).
#[derive(Debug, Clone)]
pub struct BrokenPolicy {
    pub error: String,
}

impl PolicyOverlay for BrokenPolicy {
    fn forbids(&self, _request: &Request<'_>) -> Option<String> {
        Some(format!(
            "team policy could not be loaded, so ahma refuses everything it decides: {}. \
             Run `ahma policy validate` to see the problem.",
            self.error
        ))
    }
}

/// The forbids the kernel can hold: unconditional, for any principal, on a
/// literal `Ahma::Path`, for `read` or `write` (or any action). Every other
/// path rule is held only where ahma decides, and is counted.
fn kernel_denies_of(policies: &PolicySet) -> ahma_common::policy::KernelDenies {
    use cedar_policy::{ActionConstraint, PrincipalConstraint, ResourceConstraint};
    let mut out = ahma_common::policy::KernelDenies::default();
    for policy in policies.policies() {
        let path = match policy.resource_constraint() {
            ResourceConstraint::In(uid) | ResourceConstraint::Eq(uid)
                if uid.type_name().to_string() == "Ahma::Path" =>
            {
                PathBuf::from(uid.id().unescaped())
            }
            _ => continue,
        };
        if policy.has_non_scope_constraint()
            || !matches!(policy.principal_constraint(), PrincipalConstraint::Any)
        {
            out.userspace_only += 1;
            continue;
        }
        let names: Vec<String> = match policy.action_constraint() {
            ActionConstraint::Any => vec!["read".into(), "write".into()],
            ActionConstraint::Eq(a) => vec![a.id().unescaped().to_string()],
            ActionConstraint::In(v) => v.iter().map(|a| a.id().unescaped().to_string()).collect(),
        };
        if names.iter().any(|n| n == "read") {
            out.read.push(path.clone());
        }
        if names.iter().any(|n| n == "write") {
            out.write.push(path);
        }
    }
    out
}

fn action_name(action: Action) -> &'static str {
    action.as_str()
}

fn uid(kind: &str, id: &str) -> Option<EntityUid> {
    let name = EntityTypeName::from_str(&format!("Ahma::{kind}")).ok()?;
    Some(EntityUid::from_type_name_and_id(name, EntityId::new(id)))
}

fn leaf(kind: &str, id: &str, entities: &mut Vec<Entity>) -> Option<EntityUid> {
    let u = uid(kind, id)?;
    entities.push(Entity::new_no_attrs(u.clone(), HashSet::new()));
    Some(u)
}

/// `ids` from the resource itself to its outermost ancestor; each entity's
/// parent is the next. Returns the resource's uid.
fn chain(kind: &str, ids: &[String], entities: &mut Vec<Entity>) -> Option<EntityUid> {
    let uids: Vec<EntityUid> = ids.iter().map(|id| uid(kind, id)).collect::<Option<_>>()?;
    for (i, u) in uids.iter().enumerate() {
        let parents: HashSet<EntityUid> = uids.get(i + 1).cloned().into_iter().collect();
        entities.push(Entity::new_no_attrs(u.clone(), parents));
    }
    uids.into_iter().next()
}

/// `/a/b/c` → `["/a/b/c", "/a/b", "/a", "/"]`.
pub fn path_chain(path: &Path) -> Vec<String> {
    path.ancestors()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.display().to_string())
        .collect()
}

/// `api.github.com` → `["api.github.com", "github.com", "com"]`.
pub fn host_chain(host: &str) -> Vec<String> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    (0..labels.len()).map(|i| labels[i..].join(".")).collect()
}

/// Load team policy from [`default_dirs`] and install it for every decision
/// this process makes. Called once, at startup. Returns what was installed,
/// for the startup report.
pub fn install_from_default_dirs() -> Installed {
    install_from(&default_dirs())
}

/// [`install_from_default_dirs`] for explicit directories.
pub fn install_from(dirs: &[PathBuf]) -> Installed {
    match TeamPolicy::load(dirs) {
        Ok(None) => Installed::None,
        Ok(Some(policy)) => {
            let count = policy.rules().len();
            let _ = ahma_common::policy::install_overlay(std::sync::Arc::new(policy));
            Installed::Rules(count)
        }
        Err(e) => {
            let error = e.to_string();
            let _ = ahma_common::policy::install_overlay(std::sync::Arc::new(BrokenPolicy {
                error: error.clone(),
            }));
            Installed::Broken(error)
        }
    }
}

/// What [`install_from`] installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    None,
    Rules(usize),
    Broken(String),
}
