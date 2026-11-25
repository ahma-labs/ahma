# ahma_policy Crate Specification

* **Status**: Approved (Experimental feature)
* **License**: MIT OR Apache-2.0
* **Depends on**: `ahma_common`, `cedar-policy`
* **Used by**: `ahma_mcp` (`ahma policy`), `ahma_bin` (loads policy at startup)

## 1. User Story / Problem Statement

*As the person responsible for how agents are used on a team, I want a few rules that hold on
every developer's ahma, that no prompt or grant can talk past, and that I can review as text,
so that "never give an agent the cloud credentials" is enforced rather than hoped for.*

## 2. Acceptance Criteria

The requirements are root SPEC.md R-POLICY.3–5; this crate implements them:

- `TeamPolicy::load` reads every `*.cedar` file in `default_dirs()` (`~/.ahma/policy/`, then
  the machine's policy directory), in a stable order, and validates them strictly against
  `ahma.cedarschema`. No files means no policy.
- A `permit` statement, a parse error or a schema error refuses the whole set
  (`LoadError`). `install_from` then installs `BrokenPolicy`, which forbids every decision, so
  ahma fails closed (R-POLICY.3).
- `TeamPolicy` implements `ahma_common::policy::PolicyOverlay`: a request is denied exactly
  when a loaded forbid matches it, since one built-in `permit` covers everything else. Its
  reason is the rule's `@reason`, else a sentence naming its id (`@id`, else `<stem>#<n>`).
- Paths and hosts are entity hierarchies (`path_chain`, `host_chain`), so `resource in`
  covers a subtree or a parent domain.

## 3. Non-Functional Requirements

- Pure evaluation: no I/O after load, no network, no process.
- Loaded once per process (R-CFG4); never reloaded.

## 4. Out of Scope

- Kernel enforcement of a forbidden path inside the scope (R-POLICY.4, planned).
- Export and proof (R-POLICY.6, planned).
- Context attributes (grant tier, surface) in the schema.
