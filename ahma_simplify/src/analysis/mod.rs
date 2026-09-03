pub mod checkstyle;
mod conversion;
pub mod detekt;
pub mod detekt_cli;
mod exclusion;
pub mod external;
pub mod lizard;
pub mod paths;
mod pipeline;
pub mod swiftlint;
pub mod workspace;

pub use external::{AnalyzerRegistry, ExternalIssue, ExternalMetrics, Severity};
pub use paths::{get_package_name, get_relative_path};
pub use pipeline::{perform_analysis, run_analysis};
pub use workspace::{get_project_name, is_cargo_workspace};
