use globset::{Glob, GlobSet, GlobSetBuilder};
use std::fs::File;
use std::io::Read;
use std::path::Path;

// ---------------------------------------------------------------------------
// Exclusion filtering (replaces --exclude flags passed to the old CLI)
// ---------------------------------------------------------------------------

/// Default glob patterns excluded from simplify scans.
/// Covers build artifacts, dependency caches, codegen directories,
/// generated bindings/headers, minified assets, and test fixtures.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    // Rust
    "**/target/**",
    "**/target",
    // JavaScript / Node / Frontend
    "**/node_modules/**",
    "**/node_modules",
    "**/dist/**",
    "**/dist",
    "**/build/**",
    "**/build",
    "**/out/**",
    "**/out",
    "**/bin/**",
    "**/bin",
    "**/obj/**",
    "**/obj",
    "**/.next/**",
    "**/.next",
    "**/.nuxt/**",
    "**/.nuxt",
    "**/.turbo/**",
    "**/.turbo",
    "**/.svelte-kit/**",
    "**/.svelte-kit",
    "**/.angular/**",
    "**/.angular",
    "**/.yarn/**",
    "**/.yarn",
    "**/.pnpm-store/**",
    "**/.pnpm-store",
    // Python
    "**/venv/**",
    "**/venv",
    "**/.venv/**",
    "**/.venv",
    "**/env/**",
    "**/env",
    "**/.env/**",
    "**/.env",
    "**/__pycache__/**",
    "**/__pycache__",
    "**/.tox/**",
    "**/.tox",
    "**/.nox/**",
    "**/.nox",
    "**/.pytest_cache/**",
    "**/.pytest_cache",
    "**/.mypy_cache/**",
    "**/.mypy_cache",
    "**/.ruff_cache/**",
    "**/.ruff_cache",
    "**/*.egg-info/**",
    "**/*.egg-info",
    // Kotlin / Android / Gradle
    "**/.gradle/**",
    "**/.gradle",
    "**/.gradle_user_home/**",
    "**/.gradle_user_home",
    "**/gradle_cache/**",
    "**/gradle_cache",
    "**/.gradle-cache/**",
    "**/.gradle-cache",
    "**/gradle/caches/**",
    "**/gradle/wrapper/**",
    "**/.kotlin/**",
    "**/.kotlin",
    "**/.cxx/**",
    "**/.cxx",
    "**/.externalNativeBuild/**",
    "**/.externalNativeBuild",
    "**/intermediates/**",
    "**/intermediates",
    // iOS / macOS / Swift
    "**/Pods/**",
    "**/Pods",
    "**/DerivedData/**",
    "**/DerivedData",
    "**/.build/**",
    "**/.build",
    "**/*.xcworkspace/**",
    "**/*.xcworkspace",
    "**/*.xcodeproj/**",
    "**/*.xcodeproj",
    "**/*.xcassets/**",
    "**/*.xcassets",
    "**/*.framework/**",
    "**/*.framework",
    "**/*.xcframework/**",
    "**/*.xcframework",
    // C/C++ build systems
    "**/cmake-build-*/**",
    "**/cmake-build-*",
    "**/.cmake/**",
    "**/.cmake",
    "**/CMakeFiles/**",
    "**/CMakeFiles",
    // Go / Ruby / PHP vendored deps
    "**/vendor/**",
    "**/vendor",
    "**/.bundle/**",
    "**/.bundle",
    "**/third_party/**",
    "**/third_party",
    "**/third-party/**",
    "**/third-party",
    // Dart / Flutter
    "**/.pub-cache/**",
    "**/.pub-cache",
    "**/.dart_tool/**",
    "**/.dart_tool",
    // Coverage
    "**/coverage/**",
    "**/coverage",
    "**/lcov-report/**",
    "**/lcov-report",
    // Test data & fixtures
    "**/testdata/**",
    "**/testdata",
    "**/fixtures/**",
    "**/fixtures",
    "**/__snapshots__/**",
    "**/__snapshots__",
    "**/snapshots/**",
    "**/snapshots",
    // Database migrations (historical, append-only; refactoring alters checksums)
    "**/migrations/**",
    "**/migrations",
    "**/db/migrations/**",
    "**/db/migrate/**",
    "**/alembic/versions/**",
    // Codegen directories
    "**/generated/**",
    "**/generated",
    "**/gen/**",
    "**/gen",
    "**/codegen/**",
    "**/codegen",
    // Tool caches
    "**/.sentry/**",
    "**/.sentry",
    "**/.sentry-native/**",
    "**/.sentry-native",
    "**/.cache/**",
    "**/.cache",
    "**/cache/**",
    "**/cache",
    // Internal analysis dir
    "**/analysis_results/**",
    "**/analysis_results",
    // VCS
    "**/.git/**",
    "**/.git",
    "**/.svn/**",
    "**/.svn",
    "**/.hg/**",
    "**/.hg",
    // IDE
    "**/.idea/**",
    "**/.idea",
    "**/.vscode/**",
    "**/.vscode",
    "**/.vs/**",
    "**/.vs",
    // Generated bindings, headers, and protocol files
    // UniFFI bindings & C headers
    "**/*FFI.h",
    "**/*FFI.kt",
    "**/*FFI.m",
    "**/*ffi.rs",
    "**/*ffi.kt",
    "**/*ffi.h",
    // Protobuf & gRPC
    "**/*.pb.go",
    "**/*.pb.rs",
    "**/*_pb2.py",
    "**/*_pb2_grpc.py",
    "**/*_pb.js",
    "**/*_pb.d.ts",
    "**/*_pb.ts",
    "**/*.pb.cc",
    "**/*.pb.h",
    // Flatbuffers
    "**/*_generated.rs",
    "**/*_generated.h",
    "**/*_generated.ts",
    "**/*_generated.go",
    // Bindgen & CXX
    "**/*.bindgen.rs",
    "**/*bindings.rs",
    "**/*.cxx.cc",
    "**/*.cxx.h",
    // Mocks
    "**/*_mock.go",
    "**/mock_*.go",
    "**/*_mocks.rs",
    "**/*.mock.ts",
    "**/mock_*.py",
    // Minified & bundled assets
    "**/*.min.js",
    "**/*.min.css",
    "**/*.bundle.js",
    "**/*.bundle.css",
    "**/*-min.js",
    "**/*-min.css",
    "**/*.min.mjs",
    // Test snapshots
    "**/*.snap",
    "**/*.golden",
];

/// Returns true if a directory name alone identifies it as a build/cache/fixture directory.
pub(crate) fn is_excluded_dir_name(name: &str) -> bool {
    if name.starts_with("cmake-build-") || name.ends_with(".egg-info") {
        return true;
    }
    matches!(
        name,
        "target"
            | "node_modules"
            | "dist"
            | "build"
            | "out"
            | "bin"
            | "obj"
            | ".next"
            | ".nuxt"
            | ".turbo"
            | ".svelte-kit"
            | ".angular"
            | ".yarn"
            | ".pnpm-store"
            | "venv"
            | ".venv"
            | "env"
            | ".env"
            | "__pycache__"
            | ".tox"
            | ".nox"
            | ".pytest_cache"
            | ".mypy_cache"
            | ".ruff_cache"
            | ".gradle"
            | ".gradle_user_home"
            | "gradle_cache"
            | ".gradle-cache"
            | ".kotlin"
            | ".cxx"
            | ".externalNativeBuild"
            | "intermediates"
            | "Pods"
            | "DerivedData"
            | ".build"
            | "vendor"
            | ".bundle"
            | "third_party"
            | "third-party"
            | ".pub-cache"
            | ".dart_tool"
            | "coverage"
            | "lcov-report"
            | "testdata"
            | "fixtures"
            | "__snapshots__"
            | "snapshots"
            | "migrations"
            | "generated"
            | "gen"
            | "codegen"
            | ".sentry"
            | ".sentry-native"
            | ".cache"
            | "cache"
            | ".git"
            | ".svn"
            | ".hg"
            | ".idea"
            | ".vscode"
            | ".vs"
            | "analysis_results"
    )
}

/// Normalizes a user-supplied pattern into one or more glob patterns.
/// Handles forms like:
/// - `/generated/` or `generated/` -> `**/generated/**`, `**/generated`
/// - `stat3.kt` -> `**/stat3.kt`, `**/stat3.kt/**`
/// - `*.kt` -> `**/*.kt`, `**/*.kt/**`
/// - `fi/neubit/stat3/stat3.kt` -> `fi/neubit/stat3/stat3.kt`, `**/fi/neubit/stat3/stat3.kt`
pub(crate) fn normalize_custom_pattern(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let normalized = trimmed.replace('\\', "/");
    let mut patterns = Vec::new();

    let slash_trimmed = normalized.trim_matches('/');
    if normalized.ends_with('/') || (normalized.starts_with('/') && normalized.contains('/')) {
        patterns.push(format!("**/{slash_trimmed}/**"));
        patterns.push(format!("**/{slash_trimmed}"));
        return patterns;
    }

    if !normalized.contains('/') && !normalized.starts_with("**") {
        patterns.push(format!("**/{normalized}"));
        patterns.push(format!("**/{normalized}/**"));
    } else if !normalized.starts_with("**/") {
        let relative = normalized.trim_start_matches('/');
        patterns.push(relative.to_string());
        patterns.push(format!("**/{relative}"));
        if !relative.ends_with('*') {
            patterns.push(format!("**/{relative}/**"));
        }
    } else {
        patterns.push(normalized.clone());
        if !normalized.ends_with('*') && !normalized.ends_with("/**") {
            patterns.push(format!("{normalized}/**"));
        }
    }
    patterns
}

/// Compiled matcher that tests paths and directories against default and custom exclusion globs.
#[derive(Clone)]
pub(crate) struct ExclusionMatcher {
    glob_set: GlobSet,
    custom_dir_names: Vec<String>,
}

impl ExclusionMatcher {
    pub fn new(custom_excludes: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        for pattern in DEFAULT_EXCLUDES {
            if let Ok(glob) = Glob::new(pattern) {
                builder.add(glob);
            }
        }

        let mut custom_dir_names = Vec::new();
        for custom in custom_excludes {
            let normalized = custom.trim().replace('\\', "/");
            let clean = normalized.trim_matches('/');
            if !clean.contains('/') && !clean.contains('*') && !clean.is_empty() {
                custom_dir_names.push(clean.to_string());
            }
            for norm_pattern in normalize_custom_pattern(custom) {
                if let Ok(glob) = Glob::new(&norm_pattern) {
                    builder.add(glob);
                }
            }
        }

        let glob_set = builder.build().unwrap_or_else(|_| GlobSet::empty());
        Self {
            glob_set,
            custom_dir_names,
        }
    }

    /// Check if a path matches any exclusion glob or contains an excluded directory component.
    pub fn is_excluded_path(&self, path: &Path) -> bool {
        // Fast component check for excluded directory names
        for c in path.components() {
            if let std::path::Component::Normal(os_str) = c {
                let name = os_str.to_string_lossy();
                if is_excluded_dir_name(&name) || self.custom_dir_names.iter().any(|d| d == &name) {
                    return true;
                }
            }
        }

        let path_str = path.to_string_lossy().replace('\\', "/");
        let clean = path_str.trim_start_matches("./");

        if self.glob_set.is_match(clean) {
            return true;
        }

        if let Some(file_name) = path.file_name().and_then(|n| n.to_str())
            && self.glob_set.is_match(file_name)
        {
            return true;
        }

        false
    }

    /// Check if a directory itself should be skipped by the directory walker.
    pub fn is_excluded_dir(&self, dir: &Path) -> bool {
        if let Some(file_name) = dir.file_name().and_then(|n| n.to_str())
            && (is_excluded_dir_name(file_name)
                || self.custom_dir_names.iter().any(|d| d == file_name))
        {
            return true;
        }

        let dir_str = dir.to_string_lossy().replace('\\', "/");
        let clean = dir_str.trim_start_matches("./");

        self.glob_set.is_match(clean)
            || self.glob_set.is_match(format!("{clean}/"))
            || self.is_excluded_path(dir)
    }
}

/// Inspects the first 2 KB of a file for standard machine-generated code banners.
/// Returns true if the file header indicates it was generated by a tool
/// (e.g., UniFFI, Protobuf, OpenAPI, Bindgen, etc.) and should not be refactored.
pub fn is_generated_content(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut buffer = [0u8; 2048];
    let Ok(bytes_read) = file.read(&mut buffer) else {
        return false;
    };
    if bytes_read == 0 {
        return false;
    }
    let text = String::from_utf8_lossy(&buffer[..bytes_read]);
    contains_generated_marker(&text)
}

fn contains_generated_marker(text: &str) -> bool {
    for line in text.lines().take(50) {
        let trimmed = line.trim();
        let comment_content = if let Some(rest) = trimmed.strip_prefix("//") {
            rest.trim()
        } else if let Some(rest) = trimmed.strip_prefix("/*") {
            rest.trim()
        } else if let Some(rest) = trimmed.strip_prefix('*') {
            rest.trim()
        } else if let Some(rest) = trimmed.strip_prefix('#') {
            rest.trim()
        } else if let Some(rest) = trimmed.strip_prefix("--") {
            rest.trim()
        } else {
            trimmed
        };

        let lower = comment_content.to_ascii_lowercase();

        // Standard markers across ecosystems
        if lower.contains("@generated")
            || lower.contains("code generated by")
            || lower.contains("generated by uniffi")
            || lower.contains("autogenerated by uniffi")
            || lower.contains("generated by the protocol buffer compiler")
            || lower.contains("generated by openapi")
            || lower.contains("generated by swagger")
            || lower.contains("generated by bindgen")
            || lower.contains("generated by jooq")
            || lower.contains("generated by wire")
            || lower.contains("generated by sqlc")
            || lower.contains("generated by thrift")
            || lower.contains("this file was autogenerated")
            || lower.contains("this file is autogenerated")
            || lower.contains("this file was generated by")
            || lower.contains("this file is generated by")
            || lower.contains("this file was automatically generated")
            || lower.contains("this file is automatically generated")
            || lower.contains("automatically generated - do not")
            || lower.contains("automatically generated, do not")
            || lower.contains("automatically generated by")
            || (lower.contains("generated from") && lower.contains("do not edit"))
        {
            return true;
        }

        // Standalone explicit "DO NOT EDIT" or "DO NOT MODIFY" warnings in comments
        if (comment_content.contains("DO NOT EDIT")
            || comment_content.contains("DO NOT MODIFY")
            || comment_content.contains("Do not edit"))
            && (lower.contains("generated")
                || lower.contains("autogenerated")
                || lower.contains("machine")
                || lower.contains("warning")
                || lower.contains("auto-generated"))
        {
            return true;
        }
    }
    false
}

pub fn should_exclude(path: &Path, custom_excludes: &[String]) -> bool {
    let matcher = ExclusionMatcher::new(custom_excludes);
    matcher.is_excluded_path(path) || is_generated_content(path)
}

pub fn should_exclude_dir(dir: &Path, custom_excludes: &[String]) -> bool {
    let matcher = ExclusionMatcher::new(custom_excludes);
    matcher.is_excluded_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn default_excludes_build_caches_and_artifacts() {
        let matcher = ExclusionMatcher::new(&[]);
        assert!(matcher.is_excluded_path(Path::new("target/debug/build.rs")));
        assert!(matcher.is_excluded_path(Path::new("node_modules/pkg/index.js")));
        assert!(
            matcher.is_excluded_path(Path::new("android/polar/.gradle_user_home/caches/foo.kt"))
        );
        assert!(matcher.is_excluded_path(Path::new("gradle_cache/sub/lib.kt")));
        assert!(matcher.is_excluded_path(Path::new(".gradle/daemon/7.5/registry.bin")));
        assert!(matcher.is_excluded_path(Path::new("intermediates/javac/classes/A.class")));
        assert!(matcher.is_excluded_path(Path::new("DerivedData/App/Build/Products/main.m")));
        assert!(matcher.is_excluded_path(Path::new("Pods/Headers/Public/Pod.h")));
        assert!(matcher.is_excluded_path(Path::new(".cxx/Debug/hash/arm64-v8a/lib.cpp")));
        assert!(matcher.is_excluded_path(Path::new("migrations/20260101_init.sql")));
        assert!(matcher.is_excluded_path(Path::new("db/migrations/001_create.sql")));
        assert!(matcher.is_excluded_path(Path::new("fixtures/mock_data.json")));
        assert!(matcher.is_excluded_path(Path::new("testdata/corpus.rs")));
        assert!(matcher.is_excluded_path(Path::new(".sentry-native/cache/sentry.h")));
    }

    #[test]
    fn default_excludes_generated_naming_patterns() {
        let matcher = ExclusionMatcher::new(&[]);
        // UniFFI bindings & C headers
        assert!(matcher.is_excluded_path(Path::new("ios/Headers/stat3FFI.h")));
        assert!(matcher.is_excluded_path(Path::new("android/core/stat3FFI.kt")));
        assert!(matcher.is_excluded_path(Path::new("src/ffi.rs")));
        // Protobuf
        assert!(matcher.is_excluded_path(Path::new("api/service.pb.go")));
        assert!(matcher.is_excluded_path(Path::new("proto/messages.pb.rs")));
        assert!(matcher.is_excluded_path(Path::new("client/api_pb2.py")));
        // Flatbuffers / Bindgen
        assert!(matcher.is_excluded_path(Path::new("src/schema_generated.rs")));
        assert!(matcher.is_excluded_path(Path::new("src/c_bindings.rs")));
        // Minified / bundled
        assert!(matcher.is_excluded_path(Path::new("static/app.min.js")));
        assert!(matcher.is_excluded_path(Path::new("assets/main.bundle.css")));
    }

    #[test]
    fn custom_excludes_exact_file_and_patterns() {
        let matcher = ExclusionMatcher::new(&[
            "stat3.kt".to_string(),
            "/generated/".to_string(),
            "*.custom".to_string(),
            "fi/neubit/stat3/stat3.kt".to_string(),
        ]);

        assert!(matcher.is_excluded_path(Path::new("stat3.kt")));
        assert!(matcher.is_excluded_path(Path::new(
            "android/core/src/main/java/fi/neubit/stat3/stat3.kt"
        )));
        assert!(matcher.is_excluded_path(Path::new("ios/generated/header.h")));
        assert!(matcher.is_excluded_path(Path::new("src/file.custom")));
        assert!(!matcher.is_excluded_path(Path::new("src/regular_code.rs")));
    }

    #[test]
    fn excluded_dir_detection() {
        let matcher = ExclusionMatcher::new(&["my_custom_cache".to_string()]);
        assert!(matcher.is_excluded_dir(Path::new("gradle_cache")));
        assert!(matcher.is_excluded_dir(Path::new(".gradle_user_home")));
        assert!(matcher.is_excluded_dir(Path::new("node_modules")));
        assert!(matcher.is_excluded_dir(Path::new("my_custom_cache")));
        assert!(!matcher.is_excluded_dir(Path::new("src")));
        assert!(!matcher.is_excluded_dir(Path::new("crates/core")));
    }

    #[test]
    fn content_marker_detects_uniffi_and_codegen_headers() {
        let mut f1 = NamedTempFile::new().unwrap();
        writeln!(
            f1,
            "// This file was autogenerated by some hot sauce.\n// DO NOT EDIT"
        )
        .unwrap();
        assert!(is_generated_content(f1.path()));

        let mut f2 = NamedTempFile::new().unwrap();
        writeln!(
            f2,
            "/* Warning: this file is code-generated. DO NOT EDIT. */"
        )
        .unwrap();
        assert!(is_generated_content(f2.path()));

        let mut f3 = NamedTempFile::new().unwrap();
        writeln!(f3, "// Code generated by protoc-gen-go. DO NOT EDIT.").unwrap();
        assert!(is_generated_content(f3.path()));

        let mut f4 = NamedTempFile::new().unwrap();
        writeln!(f4, "/**\n * @generated SignedSource<<...>>\n */").unwrap();
        assert!(is_generated_content(f4.path()));

        let mut f5 = NamedTempFile::new().unwrap();
        writeln!(
            f5,
            "// This module implements the main audio synthesizer logic.\npub struct Voice;"
        )
        .unwrap();
        assert!(!is_generated_content(f5.path()));
    }
}
