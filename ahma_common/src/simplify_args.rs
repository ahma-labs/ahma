//! Command-line arguments for `ahma simplify`.
//!
//! Lives in `ahma_common` rather than in `ahma_simplify` so that the `ahma` CLI
//! parser (`ahma_mcp::shell::cli::Subcommands`) can reserve the subcommand and
//! its help text without depending on the analysis engine: `ahma_simplify` is an
//! optional dependency of `ahma_bin` (cargo feature `simplify`, on by default),
//! and `ahma_mcp` must keep exactly one feature flavour (see
//! `docs/build-and-test-performance.md`). The engine crate re-exports this type
//! as `ahma_simplify::SimplifyArgs`.

use clap::Args;
use std::path::PathBuf;

/// Default set of file extensions analyzed by `ahma simplify`.
///
/// Includes Rust (full AST), Kotlin, Swift, Objective-C (external analyzers), and
/// all Lizard-supported languages. Exposed as a constant so tests can assert that
/// new languages appear here without parsing the `--help` output.
pub const DEFAULT_EXTENSIONS: &str =
    "rs,py,js,ts,tsx,c,h,cpp,cc,hpp,hh,cs,java,go,css,html,kt,kts,swift,m,mm";

/// Analyze source code complexity and generate a simplicity report.
///
/// Scores are calibrated for AI-assisted maintenance. An AI agent making a change
/// must hold the relevant context in its context window; large, deeply nested functions
/// increase the risk of misunderstanding and regression. The scoring formula rewards
/// decomposed, focused code:
///
///   Score = 0.4 × MI + 0.3 × Cognitive Density + 0.2 × Peak Cognitive + 0.1 × Length
///
/// MI (40%) — function-weighted Maintainability Index; rewards well-structured decomposition.
/// Cognitive Density (30%) — cognitive complexity per SLOC; rewards focused functions.
/// Peak Cognitive (20%) — complexity of the single worst function; the primary hotspot signal.
/// Length Score (10%) — 100% at ≤300 SLOC, scaling down above; reflects context-window pressure.
/// Cyclomatic — reported for context only; already embedded inside MI, not double-counted.
#[derive(Args, Debug)]
#[command(
    about = "Analyze source code complexity and generate a simplicity report",
    long_about = "Analyzes source code metrics and generates a simplicity report.\n\n\
        Scores are calibrated for AI-assisted maintenance. An AI agent making a change\n\
        must hold the relevant context in its context window; large, deeply nested functions\n\
        increase the risk of misunderstanding and regression. The scoring formula rewards\n\
        decomposed, focused code:\n\n\
          Score = 0.4 × MI + 0.3 × Cognitive Density + 0.2 × Peak Cognitive + 0.1 × Length\n\n\
        MI (40%) — function-weighted Maintainability Index; rewards well-structured decomposition.\n\
        Cognitive Density (30%) — cognitive complexity per SLOC; rewards focused functions.\n\
        Peak Cognitive (20%) — complexity of the single worst function; the primary hotspot signal.\n\
        Length Score (10%) — 100% at ≤300 SLOC, scaling down above; reflects context-window pressure.\n\
        Cyclomatic — reported for context only; already embedded inside MI, not double-counted.\n\n\
        Supported languages: Rust (full AST metrics), Kotlin (detekt-cli → Gradle detekt → Lizard),\n\
        Swift (SwiftLint → Lizard), Python/JS/TS/C/C++/Java/Go/C#/ObjC/HTML/CSS (Lizard fallback)."
)]
pub struct SimplifyArgs {
    /// Directory to analyze (absolute or relative)
    pub directory: PathBuf,

    /// Output directory for analysis results
    #[arg(short, long, default_value = "analysis_results")]
    pub output: PathBuf,

    /// Number of issues to show in the report
    #[arg(short, long, default_value_t = 50)]
    pub limit: usize,

    /// Open the report automatically
    #[arg(long)]
    pub open: bool,

    /// Additionally renders CODE_SIMPLICITY.html next to the Markdown report,
    /// and (like --open/--output-path) causes the report to be written to
    /// disk rather than printed to stdout.
    #[arg(long)]
    pub html: bool,

    /// Shorthand for --html and --open combined
    #[arg(long)]
    pub heml: bool,

    /// File extensions or language names to analyze, comma-separated.
    /// Accepts raw extensions (e.g. rs,py,kt) or language names (e.g. rust,kotlin,python).
    /// Language names are case-insensitive and expand to all their extensions.
    /// Supported languages: rust, python, javascript, typescript, kotlin, swift, objc, c, c++, java, c#, go, html, css.
    /// Default: all supported extensions.
    #[arg(
        short,
        long,
        default_value = DEFAULT_EXTENSIONS,
        value_delimiter = ','
    )]
    pub extensions: Vec<String>,

    /// Additional paths/patterns to exclude, as a comma-separated list.
    /// Example: --exclude "**/generated/**,**/vendor/**"
    #[arg(short = 'x', long, value_delimiter = ',')]
    pub exclude: Vec<String>,

    /// Disable external language-specific analyzers (e.g. Detekt for Kotlin).
    /// When set, only rust-code-analysis metrics are used. Useful for faster
    /// CI runs or when external tools are not available.
    #[arg(long)]
    pub no_external: bool,

    /// Output directory for CODE_SIMPLICITY.md and CODE_SIMPLICITY.html files.
    /// If omitted (and --html/--open not set), report is printed to stdout.
    /// When specified, writes files to the given directory.
    #[arg(long)]
    pub output_path: Option<PathBuf>,

    /// Generate an AI fix prompt for the Nth most complex file (1-indexed).
    /// When set, outputs the full simplicity report and a structured prompt
    /// instructing the AI to plan and implement a fix for that issue.
    #[arg(long)]
    pub ai_fix: Option<usize>,

    /// Verify improvement by re-analyzing a specific file and comparing
    /// against the baseline from the previous analysis run. Shows before/after
    /// metrics with relative improvement percentages.
    #[arg(long)]
    pub verify: Option<PathBuf>,

    /// Which analysis lenses to run, comma-separated. Valid values: complexity
    /// (the default metrics analysis), reuse (duplicate-code detection), or
    /// all. Reach for a narrower set when you only care about one signal, for
    /// example --lens reuse to check for duplication without a full metrics
    /// pass. Default: all.
    #[arg(long, value_delimiter = ',', default_value = "all")]
    pub lens: Vec<String>,

    /// Restrict analysis to files changed in git (staged, unstaged, and
    /// untracked) instead of the whole tree. Reach for this after making a
    /// change, to check what you just touched rather than re-scanning the
    /// entire project.
    #[arg(long)]
    pub diff: bool,
}
