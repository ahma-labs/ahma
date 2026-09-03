use rust_code_analysis::{
    FuncSpace, SpaceKind, get_function_spaces, get_language_for_file, read_file_with_eol,
};
use std::path::Path;

use crate::models::{Cognitive, Cyclomatic, Loc, Metrics, MetricsResults, Mi, SpaceEntry};

// ---------------------------------------------------------------------------
// Conversion helpers: rust-code-analysis native types → our MetricsResults
// ---------------------------------------------------------------------------

fn code_metrics_to_metrics(cm: &rust_code_analysis::CodeMetrics) -> Metrics {
    Metrics {
        cognitive: Cognitive {
            sum: cm.cognitive.cognitive_sum(),
        },
        cyclomatic: Cyclomatic {
            sum: cm.cyclomatic.cyclomatic_sum(),
        },
        mi: Mi {
            mi_visual_studio: cm.mi.mi_visual_studio(),
        },
        loc: Loc {
            sloc: cm.loc.sloc(),
        },
    }
}

fn space_kind_str(kind: SpaceKind) -> &'static str {
    match kind {
        SpaceKind::Function => "function",
        SpaceKind::Class => "class",
        SpaceKind::Struct => "struct",
        SpaceKind::Trait => "trait",
        SpaceKind::Impl => "impl",
        SpaceKind::Unit => "unit",
        SpaceKind::Namespace => "namespace",
        SpaceKind::Interface => "interface",
        SpaceKind::Unknown => "unknown",
    }
}

fn func_space_to_space_entry(space: &FuncSpace) -> SpaceEntry {
    let kind_str = space_kind_str(space.kind).to_string();

    SpaceEntry {
        name: space.name.clone().unwrap_or_default(),
        start_line: space.start_line as u32,
        end_line: space.end_line as u32,
        kind: kind_str,
        metrics: code_metrics_to_metrics(&space.metrics),
        spaces: space.spaces.iter().map(func_space_to_space_entry).collect(),
    }
}

fn func_space_to_metrics_results(space: FuncSpace) -> MetricsResults {
    MetricsResults {
        name: space.name.clone().unwrap_or_default(),
        metrics: code_metrics_to_metrics(&space.metrics),
        spaces: space.spaces.iter().map(func_space_to_space_entry).collect(),
    }
}

// ---------------------------------------------------------------------------
// Per-file analysis using the library
// ---------------------------------------------------------------------------

/// File extensions that rust-code-analysis registers but only *stub*-supports:
/// it parses the grammar yet its cyclomatic/cognitive `compute()` are no-ops
/// (e.g. `implement_metric_trait!(Cyclomatic, KotlinCode, ...)` expands to an
/// empty body). Running rca on these yields garbage metrics (cyclomatic≈1,
/// cognitive=0) that would falsely mark the file "covered" and suppress the
/// external-only analyzer path. We skip rca for them so they flow through the
/// external analyzer cascade (Detekt/Lizard for Kotlin), exactly like Swift —
/// which rca does not parse at all and therefore already bypasses rca.
fn is_rca_stub_only(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| matches!(ext.to_lowercase().as_str(), "kt" | "kts"))
}

pub(crate) fn analyze_file(path: &Path) -> Option<MetricsResults> {
    if is_rca_stub_only(path) {
        return None;
    }
    let lang = get_language_for_file(path)?;
    let source = read_file_with_eol(path).ok().flatten()?;
    let func_space = get_function_spaces(&lang, source, path, None)?;
    Some(func_space_to_metrics_results(func_space))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// rca only stub-parses Kotlin (no-op cyclomatic/cognitive `compute`), so we
    /// must skip it here and let the external analyzer cascade (Detekt/Lizard)
    /// own Kotlin. If this regresses, `.kt` files get bogus rca metrics that
    /// suppress the external-only path.
    #[test]
    fn test_kotlin_is_skipped_by_rca() {
        let tmp = TempDir::new().unwrap();
        let kt = tmp.path().join("Sample.kt");
        fs::write(
            &kt,
            "fun foo(x: Int): Int {\n    if (x > 0) return x else return -x\n}\n",
        )
        .unwrap();
        assert!(
            analyze_file(&kt).is_none(),
            "rca must not analyze Kotlin — Detekt/Lizard own it"
        );
        // .kts script files too.
        let kts = tmp.path().join("build.gradle.kts");
        fs::write(&kts, "val x = 1\n").unwrap();
        assert!(analyze_file(&kts).is_none());
    }

    /// A language rca genuinely supports must still produce metrics, proving the
    /// stub guard is narrow and did not break the normal path.
    #[test]
    fn test_rust_is_analyzed_by_rca() {
        let tmp = TempDir::new().unwrap();
        let rs = tmp.path().join("sample.rs");
        fs::write(
            &rs,
            "fn foo(x: i32) -> i32 {\n    if x > 0 { x } else { -x }\n}\n",
        )
        .unwrap();
        assert!(
            analyze_file(&rs).is_some(),
            "rca must still analyze Rust files"
        );
    }
}
