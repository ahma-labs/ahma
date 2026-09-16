//! Thin-wrapper delegation chain detector (the "altitude" lens).
//!
//! Finds functions that do nothing but forward a single call to another function,
//! which itself forwards to a third, and so on. Each layer in such a chain adds a
//! call frame and a name to learn without adding behaviour — the fix for whatever
//! problem motivated the outer layer usually belongs at the bottom of the chain, not
//! bolted on top of it. Like the other lenses, [`find_altitude_chains`] reports
//! candidates for review, not defects: a forwarding layer is often a deliberate
//! facade, trait-impl delegation, or platform shim.

use std::collections::{HashMap, HashSet};

use crate::analysis::source_tree::{CallSite, FunctionNode, SourceTree};
use crate::models::{AltitudeCall, AltitudeChain};

fn is_thin_wrapper(function: &FunctionNode) -> bool {
    function.body_statements == 1 && function.calls.len() == 1
}

/// Resolves one call to the function it names, or `None` when that cannot be known.
///
/// A method call carries a receiver whose type the AST does not record, so its
/// trailing identifier says nothing about which definition it reaches: a bare
/// `.lock()` on a std mutex is textually identical to a local `lock` method, and
/// following that match invents a chain the reader would then have to disprove.
/// Only an unqualified call is resolvable from a name alone.
fn resolve_call(call: &CallSite, name_index: &HashMap<&str, usize>) -> Option<usize> {
    if call.receiver.is_some() {
        return None;
    }
    name_index.get(call.callee.as_str()).copied()
}

/// Finds thin-wrapper delegation chains across `trees`.
///
/// Pure function: takes already-parsed `SourceTree`s and performs no I/O, so it is
/// directly unit-testable. The same input always produces identical output.
pub fn find_altitude_chains(trees: &[SourceTree]) -> Vec<AltitudeChain> {
    let entries: Vec<(&SourceTree, &FunctionNode)> = trees
        .iter()
        .flat_map(|tree| tree.functions.iter().map(move |function| (tree, function)))
        .collect();

    let mut name_counts: HashMap<&str, usize> = HashMap::new();
    for &(_, function) in &entries {
        *name_counts.entry(function.name.as_str()).or_insert(0) += 1;
    }

    // A callee name with more than one definition in the corpus carries no type
    // information to disambiguate, so it is excluded from the index entirely: any
    // wrapper resolving to it stops there rather than guessing which definition it means.
    let mut name_index: HashMap<&str, usize> = HashMap::new();
    for (idx, &(_, function)) in entries.iter().enumerate() {
        if name_counts
            .get(function.name.as_str())
            .copied()
            .unwrap_or(0)
            == 1
        {
            name_index.insert(function.name.as_str(), idx);
        }
    }

    // A function already reached as the target of another thin wrapper is an interior
    // node of some other chain, not a chain of its own; skipping it here is what keeps
    // reported chains maximal instead of reporting every suffix of a longer one too.
    let mut targets: HashSet<usize> = HashSet::new();
    for &(_, function) in &entries {
        if is_thin_wrapper(function)
            && let Some(target_idx) = resolve_call(&function.calls[0], &name_index)
        {
            targets.insert(target_idx);
        }
    }

    let mut chains = Vec::new();
    for (idx, &(_, function)) in entries.iter().enumerate() {
        if !is_thin_wrapper(function) || targets.contains(&idx) {
            continue;
        }
        let calls = walk_chain(idx, &entries, &name_index);
        if calls.len() >= 2 {
            let description = describe_chain(&calls);
            chains.push(AltitudeChain {
                depth: calls.len(),
                calls,
                description,
            });
        }
    }

    chains.sort_by(|a, b| match (a.calls.first(), b.calls.first()) {
        (Some(ca), Some(cb)) => ca.file.cmp(&cb.file).then(ca.line.cmp(&cb.line)),
        _ => std::cmp::Ordering::Equal,
    });
    chains
}

fn walk_chain(
    start: usize,
    entries: &[(&SourceTree, &FunctionNode)],
    name_index: &HashMap<&str, usize>,
) -> Vec<AltitudeCall> {
    let mut calls = Vec::new();
    // Tracks functions visited on this walk; a direct or mutual recursion among thin
    // wrappers would otherwise revisit the same node forever.
    let mut visited: HashSet<usize> = HashSet::new();
    let mut current = start;
    loop {
        let (tree, function) = entries[current];
        if !is_thin_wrapper(function) || !visited.insert(current) {
            break;
        }
        let call_site = &function.calls[0];
        calls.push(AltitudeCall {
            caller: function.name.clone(),
            callee: call_site.callee.clone(),
            file: tree.path.clone(),
            line: call_site.line,
        });
        match resolve_call(call_site, name_index) {
            Some(next) => current = next,
            None => break,
        }
    }
    calls
}

fn describe_chain(calls: &[AltitudeCall]) -> String {
    let mut names: Vec<&str> = calls.iter().map(|call| call.caller.as_str()).collect();
    if let Some(last) = calls.last() {
        names.push(last.callee.as_str());
    }
    let chain = names
        .iter()
        .map(|name| format!("{name}()"))
        .collect::<Vec<_>>()
        .join(" \u{2192} ");
    format!(
        "{chain}: {} layers, each forwarding to the next",
        names.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::source_tree::{CallSite, Visibility, parse_source_tree};
    use crate::models::Language;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn thin(name: &str, callee: &str, call_line: usize, start_line: usize) -> FunctionNode {
        FunctionNode {
            name: name.to_string(),
            qualifier: None,
            visibility: Visibility::Public,
            start_line,
            end_line: start_line + 1,
            body_statements: 1,
            calls: vec![CallSite {
                callee: callee.to_string(),
                receiver: None,
                line: call_line,
            }],
        }
    }

    fn leaf(name: &str, start_line: usize, calls: Vec<CallSite>) -> FunctionNode {
        FunctionNode {
            name: name.to_string(),
            qualifier: None,
            visibility: Visibility::Public,
            start_line,
            end_line: start_line + 3,
            body_statements: 3,
            calls,
        }
    }

    fn make_tree(path: &str, functions: Vec<FunctionNode>) -> SourceTree {
        SourceTree {
            path: PathBuf::from(path),
            language: Language::Rust,
            functions,
        }
    }

    fn thin_method_call(
        name: &str,
        receiver: &str,
        callee: &str,
        start_line: usize,
    ) -> FunctionNode {
        let mut node = thin(name, callee, start_line + 1, start_line);
        node.calls[0].receiver = Some(receiver.to_string());
        node
    }

    #[test]
    fn a_method_call_is_not_resolved_by_name_alone() {
        // Regression: `guard.lock()` on a std mutex reads exactly like a local
        // `lock` method, and resolving it by name invented chains that did not
        // exist anywhere in the program.
        let tree = make_tree(
            "src/lib.rs",
            vec![
                thin_method_call("outer", "guard", "lock", 1),
                thin("lock", "inner", 20, 10),
                leaf("inner", 30, vec![]),
            ],
        );
        assert!(
            find_altitude_chains(&[tree]).is_empty(),
            "a receiver means the callee's type is unknown, so the chain must stop"
        );
    }

    #[test]
    fn three_deep_chain_is_reported_with_depth_two() {
        let tree = make_tree(
            "src/lib.rs",
            vec![
                thin("a", "b", 10, 1),
                thin("b", "c", 20, 2),
                leaf("c", 30, vec![]),
            ],
        );
        let chains = find_altitude_chains(&[tree]);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].depth, 2);
        assert_eq!(chains[0].calls.len(), 2);
        assert_eq!(chains[0].calls[0].caller, "a");
        assert_eq!(chains[0].calls[0].callee, "b");
        assert_eq!(chains[0].calls[1].caller, "b");
        assert_eq!(chains[0].calls[1].callee, "c");
    }

    #[test]
    fn single_forwarding_call_is_not_reported() {
        let tree = make_tree(
            "src/lib.rs",
            vec![thin("a", "b", 10, 1), leaf("b", 20, vec![])],
        );
        let chains = find_altitude_chains(&[tree]);
        assert!(chains.is_empty());
    }

    #[test]
    fn multi_statement_body_breaks_the_chain() {
        let d_call = CallSite {
            callee: "d".to_string(),
            receiver: None,
            line: 31,
        };
        let tree = make_tree(
            "src/lib.rs",
            vec![
                thin("a", "b", 10, 1),
                thin("b", "c", 20, 2),
                // `c` calls `d` too, but its multi-statement body disqualifies it as a
                // thin wrapper, so the chain must stop at `c` rather than reach `d`.
                leaf("c", 30, vec![d_call]),
                leaf("d", 40, vec![]),
            ],
        );
        let chains = find_altitude_chains(&[tree]);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].depth, 2);
        assert!(chains[0].calls.iter().all(|c| c.callee != "d"));
    }

    #[test]
    fn ambiguous_callee_name_stops_resolution_instead_of_guessing() {
        let tree = make_tree(
            "src/lib.rs",
            vec![
                thin("a", "b", 10, 1),
                thin("b", "c", 20, 2),
                // Two distinct `c` definitions make the name ambiguous.
                thin("c", "x", 30, 3),
                thin("c", "y", 40, 4),
            ],
        );
        let chains = find_altitude_chains(&[tree]);
        // Only a -> b -> c is reported; resolution stops at the ambiguous `c` rather
        // than picking one of the two definitions to continue into `x` or `y`.
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].depth, 2);
        assert_eq!(chains[0].calls[1].callee, "c");
    }

    #[test]
    fn direct_mutual_cycle_without_a_root_yields_no_chain() {
        let tree = make_tree(
            "src/lib.rs",
            vec![thin("a", "b", 10, 1), thin("b", "a", 20, 2)],
        );
        // Neither `a` nor `b` qualifies as a maximal-chain root (each is the other's
        // target), so no walk starts and the mutual recursion is never traversed.
        let chains = find_altitude_chains(&[tree]);
        assert!(chains.is_empty());
    }

    #[test]
    fn cycle_reached_from_a_root_terminates_instead_of_looping_forever() {
        let tree = make_tree(
            "src/lib.rs",
            vec![
                thin("root", "a", 10, 1),
                thin("a", "b", 20, 2),
                thin("b", "a", 30, 3),
            ],
        );
        let chains = find_altitude_chains(&[tree]);
        // The visited-set guard in `walk_chain` stops the walk the second time it
        // would revisit `a`, so the result is finite: root -> a -> b -> a.
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].depth, 3);
        assert_eq!(chains[0].calls[2].caller, "b");
        assert_eq!(chains[0].calls[2].callee, "a");
    }

    #[test]
    fn subchain_of_a_reported_chain_is_not_reported_separately() {
        let tree = make_tree(
            "src/lib.rs",
            vec![
                thin("a", "b", 10, 1),
                thin("b", "c", 20, 2),
                thin("c", "d", 30, 3),
                leaf("d", 40, vec![]),
            ],
        );
        let chains = find_altitude_chains(&[tree]);
        // Only one chain (a -> b -> c -> d): b and c are each the target of another
        // thin wrapper, so no separate b -> c -> d (or c -> d) chain is reported.
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].depth, 3);
    }

    #[test]
    fn is_deterministic_across_runs() {
        let tree = make_tree(
            "src/lib.rs",
            vec![
                thin("a", "b", 10, 1),
                thin("b", "c", 20, 2),
                leaf("c", 30, vec![]),
            ],
        );
        let first = find_altitude_chains(std::slice::from_ref(&tree));
        let second = find_altitude_chains(&[tree]);
        assert_eq!(first, second);
    }

    #[test]
    fn end_to_end_with_real_parser() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("lib.rs");
        let source = "fn c(x: i32) {\n    let _ = x;\n}\n\nfn b(x: i32) {\n    c(x);\n}\n\nfn a(x: i32) {\n    b(x);\n}\n";
        fs::write(&file, source).unwrap();
        let tree = parse_source_tree(&file).expect("parses");

        let chains = find_altitude_chains(&[tree]);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].depth, 2);
        assert_eq!(chains[0].calls[0].caller, "a");
        assert_eq!(chains[0].calls[0].callee, "b");
        assert_eq!(chains[0].calls[1].caller, "b");
        assert_eq!(chains[0].calls[1].callee, "c");
    }
}
