# [crate_name] Crate Specification

* **Status**: Approved | Approved (Experimental feature) | Dormant
* **License**: MIT OR Apache-2.0 | AGPL-3.0
* **Depends on**: [workspace crates this one uses]
* **Used by**: [workspace crates that use this one, and through which surface]

A SPEC states what the crate does now and why. It holds no history, no incident stories
and no status notes in the body. Open work in the whole product is listed once, in the root
SPEC's Known gaps (§11). What this crate deliberately does not do goes in Out of Scope.

## 1. User Story / Problem Statement

*As a [user type], I want to [do something] so that I can [achieve some goal].*

(One short paragraph: the problem this crate solves, from the user's side.)

## 2. Acceptance Criteria

(Testable conditions, one per bullet. Point at a requirement id rather than restating it.)

- The crate must...
- When the caller does X, the crate does Y.

## 3. Requirements Owned Here

(Only when this crate owns requirement ids. Ids are stable: code cites them, and
`scripts/check-spec-ids.sh` fails CI when a cited id is defined nowhere. Never renumber.
A rule that another SPEC owns is a one-line pointer to its id, not a copy.)

### Rn: [Requirement family title]

- **Rn.1**: **[Short title].** The rule, stated with **must** / **should** / **must not**.
  At most one sentence of why, as a present-tense fact.

## 4. Non-Functional Requirements

- **Performance**: [a budget, and the test that enforces it]
- **Security**: [what input is untrusted, and what guards it]

## 5. Out of Scope

- [What this crate will not do, stated plainly.]
