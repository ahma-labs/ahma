//! File Tools Integration Tests
//!
//! These tests verify that the file_tools work correctly when invoked via the ahma_mcp binary.
//! They provide coverage for the file operations in a real integration scenario.
//!
//! Test philosophy:
//! - Tests use temp directories as per R13.5 (Test File Isolation)
//! - Tests verify exit codes and output content
//! - Tests skip gracefully if the tool is disabled (enabled: false in JSON config)

use ahma_mcp::skip_if_disabled;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

fn workspace_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("Failed to locate workspace root")
        .to_path_buf()
}

fn build_binary(package: &str, binary: &str) -> PathBuf {
    ahma_mcp::test_utils::cli::build_binary_cached(package, binary)
}

/// Create a command for a binary with test mode enabled (bypasses sandbox checks)
fn test_command(binary: &PathBuf) -> Command {
    let mut cmd = Command::new(binary);
    cmd.env("AHMA_DISABLE_SANDBOX", "1");
    cmd
}

mod file_tools_tests {
    use super::*;

    #[test]
    fn test_file_tools_pwd() {
        skip_if_disabled!("run_terminal_command");

        let binary = build_binary("ahma_mcp", "ahma");
        let workspace = workspace_dir();
        let tools_dir = workspace.join(".ahma");
        let temp_dir = TempDir::new().expect("Failed to create temp dir");

        let output = test_command(&binary)
            .current_dir(temp_dir.path())
            .env("AHMA_TOOLS_DIR", &tools_dir)
            .args(["tool", "run", "run_terminal_command", "pwd"])
            .output()
            .expect("Failed to execute pwd via run_terminal_command");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert!(
            output.status.success(),
            "pwd via run_terminal_command should succeed. stdout: {}, stderr: {}",
            stdout,
            stderr
        );

        // Should output the temp dir path
        // Note: on macOS /var is a symlink to /private/var, so we need to be careful with exact matching
        // But the output should definitely contain the path components
        assert!(
            stdout.contains(temp_dir.path().file_name().unwrap().to_str().unwrap()),
            "pwd output should contain temp dir name. Got: {}",
            stdout
        );
    }

    #[test]
    fn test_file_tools_touch_and_ls() {
        skip_if_disabled!("run_terminal_command");

        let binary = build_binary("ahma_mcp", "ahma");
        let workspace = workspace_dir();
        let tools_dir = workspace.join(".ahma");
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let test_file = "test_file.txt";

        // 1. Touch a file
        let output_touch = test_command(&binary)
            .current_dir(temp_dir.path())
            .env("AHMA_TOOLS_DIR", &tools_dir)
            .args([
                "tool",
                "run",
                "run_terminal_command",
                &format!("touch {}", test_file),
            ])
            .output()
            .expect("Failed to execute touch via run_terminal_command");

        assert!(
            output_touch.status.success(),
            "touch via run_terminal_command should succeed. stderr: {}",
            String::from_utf8_lossy(&output_touch.stderr)
        );

        // Verify file exists
        assert!(
            temp_dir.path().join(test_file).exists(),
            "File should be created"
        );

        // 2. List the file
        let output_ls = test_command(&binary)
            .current_dir(temp_dir.path())
            .env("AHMA_TOOLS_DIR", &tools_dir)
            .args([
                "tool",
                "run",
                "run_terminal_command",
                &format!("ls {}", test_file),
            ])
            .output()
            .expect("Failed to execute ls via run_terminal_command");

        let stdout_ls = String::from_utf8_lossy(&output_ls.stdout);
        assert!(
            output_ls.status.success(),
            "ls via run_terminal_command should succeed. stderr: {}",
            String::from_utf8_lossy(&output_ls.stderr)
        );

        assert!(
            stdout_ls.contains(test_file),
            "ls output should contain file name. Got: {}",
            stdout_ls
        );
    }

    #[test]
    fn test_file_tools_cp_and_mv() {
        skip_if_disabled!("run_terminal_command");

        let binary = build_binary("ahma_mcp", "ahma");
        let workspace = workspace_dir();
        let tools_dir = workspace.join(".ahma");
        let temp_dir = TempDir::new().expect("Failed to create temp dir");

        let source_file = "source.txt";
        let dest_file = "dest.txt";
        let moved_file = "moved.txt";

        // Create source file
        fs::write(temp_dir.path().join(source_file), "content")
            .expect("Failed to write source file");

        // 1. Copy file
        let output_cp = test_command(&binary)
            .current_dir(temp_dir.path())
            .env("AHMA_TOOLS_DIR", &tools_dir)
            .args([
                "tool",
                "run",
                "run_terminal_command",
                &format!("cp {} {}", source_file, dest_file),
            ])
            .output()
            .expect("Failed to execute cp via run_terminal_command");

        assert!(
            output_cp.status.success(),
            "cp via run_terminal_command should succeed. stderr: {}",
            String::from_utf8_lossy(&output_cp.stderr)
        );

        assert!(
            temp_dir.path().join(dest_file).exists(),
            "Destination file should exist"
        );

        // 2. Move file
        let output_mv = test_command(&binary)
            .current_dir(temp_dir.path())
            .env("AHMA_TOOLS_DIR", &tools_dir)
            .args([
                "tool",
                "run",
                "run_terminal_command",
                &format!("mv {} {}", dest_file, moved_file),
            ])
            .output()
            .expect("Failed to execute mv via run_terminal_command");

        assert!(
            output_mv.status.success(),
            "mv via run_terminal_command should succeed. stderr: {}",
            String::from_utf8_lossy(&output_mv.stderr)
        );

        assert!(
            temp_dir.path().join(moved_file).exists(),
            "Moved file should exist"
        );
        assert!(
            !temp_dir.path().join(dest_file).exists(),
            "Old file should not exist"
        );
    }

    #[test]
    fn test_file_tools_rm() {
        skip_if_disabled!("run_terminal_command");

        let binary = build_binary("ahma_mcp", "ahma");
        let workspace = workspace_dir();
        let tools_dir = workspace.join(".ahma");
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let test_file = "to_delete.txt";

        // Create file
        fs::write(temp_dir.path().join(test_file), "content").expect("Failed to write file");

        // Remove file
        let output_rm = test_command(&binary)
            .current_dir(temp_dir.path())
            .env("AHMA_TOOLS_DIR", &tools_dir)
            .args([
                "tool",
                "run",
                "run_terminal_command",
                &format!("rm {}", test_file),
            ])
            .output()
            .expect("Failed to execute rm via run_terminal_command");

        assert!(
            output_rm.status.success(),
            "rm via run_terminal_command should succeed. stderr: {}",
            String::from_utf8_lossy(&output_rm.stderr)
        );

        assert!(
            !temp_dir.path().join(test_file).exists(),
            "File should be deleted"
        );
    }

    #[test]
    fn test_file_tools_cat_and_grep() {
        skip_if_disabled!("run_terminal_command");

        let binary = build_binary("ahma_mcp", "ahma");
        let workspace = workspace_dir();
        let tools_dir = workspace.join(".ahma");
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let test_file = "content.txt";
        let content = "Hello World\nAnother Line\nTarget String";

        // Create file
        fs::write(temp_dir.path().join(test_file), content).expect("Failed to write file");

        // 1. Cat file
        let output_cat = test_command(&binary)
            .current_dir(temp_dir.path())
            .env("AHMA_TOOLS_DIR", &tools_dir)
            .args([
                "tool",
                "run",
                "run_terminal_command",
                &format!("cat {}", test_file),
            ])
            .output()
            .expect("Failed to execute cat via run_terminal_command");

        let stdout_cat = String::from_utf8_lossy(&output_cat.stdout);
        assert!(
            output_cat.status.success(),
            "cat via run_terminal_command should succeed. stderr: {}",
            String::from_utf8_lossy(&output_cat.stderr)
        );
        assert!(
            stdout_cat.contains("Hello World"),
            "cat output should contain content"
        );

        // 2. Grep file
        let output_grep = test_command(&binary)
            .current_dir(temp_dir.path())
            .env("AHMA_TOOLS_DIR", &tools_dir)
            .args([
                "tool",
                "run",
                "run_terminal_command",
                &format!("grep Target {}", test_file),
            ])
            .output()
            .expect("Failed to execute grep via run_terminal_command");

        let stdout_grep = String::from_utf8_lossy(&output_grep.stdout);
        assert!(
            output_grep.status.success(),
            "grep via run_terminal_command should succeed. stderr: {}",
            String::from_utf8_lossy(&output_grep.stderr)
        );
        assert!(
            stdout_grep.contains("Target String"),
            "grep output should contain match"
        );
        assert!(
            !stdout_grep.contains("Hello World"),
            "grep output should not contain non-matching lines"
        );
    }
}

mod run_terminal_command_tests {
    use super::*;

    #[test]
    fn test_run_terminal_command_echo() {
        skip_if_disabled!("run_terminal_command");
        let binary = build_binary("ahma_mcp", "ahma");
        let workspace = workspace_dir();
        let tools_dir = workspace.join(".ahma");
        let temp_dir = TempDir::new().expect("Failed to create temp dir");

        let output = test_command(&binary)
            .current_dir(temp_dir.path())
            .env("AHMA_TOOLS_DIR", &tools_dir)
            .args([
                "tool",
                "run",
                "run_terminal_command",
                "echo 'Hello from shell'",
            ])
            .output()
            .expect("Failed to execute run_terminal_command");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "run_terminal_command should succeed. stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("Hello from shell"),
            "Output should contain echoed text"
        );
    }

    #[test]
    fn test_run_terminal_command_write_file() {
        skip_if_disabled!("run_terminal_command");
        let binary = build_binary("ahma_mcp", "ahma");
        let workspace = workspace_dir();
        let tools_dir = workspace.join(".ahma");
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let test_file = "shell_created.txt";

        let script = if cfg!(windows) {
            format!(
                "Set-Content -Path {} -Value 'content' -Encoding UTF8",
                test_file
            )
        } else {
            format!("echo 'content' > {}", test_file)
        };

        let output = test_command(&binary)
            .current_dir(temp_dir.path())
            .env("AHMA_TOOLS_DIR", &tools_dir)
            .args(["tool", "run", "run_terminal_command", &script])
            .output()
            .expect("Failed to execute run_terminal_command");

        assert!(
            output.status.success(),
            "run_terminal_command should succeed. stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(
            temp_dir.path().join(test_file).exists(),
            "File should be created by shell"
        );
        let content =
            fs::read_to_string(temp_dir.path().join(test_file)).expect("Failed to read file");
        assert!(content.contains("content"), "File content should match");
    }
}
