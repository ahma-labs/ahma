//! TUI rendering helpers.

use super::app::{OperationSummary, TuiApp};

/// Format an operation summary for display in a plain-text listing.
pub fn format_operation(op: &OperationSummary, selected: bool) -> String {
    let prefix = if selected { "►" } else { " " };
    format!(
        "{prefix} {:12} [{:10}] {:>4}s  {}",
        op.id, op.status, op.elapsed_secs, op.tool_name
    )
}

/// Render the full TUI to a string (used for testing and the text fallback).
pub fn render_text(app: &TuiApp) -> String {
    let health = if app.server_healthy {
        "HEALTHY"
    } else {
        "UNREACHABLE"
    };

    let mut out = format!(
        "=== Ahma Task Control Plane | Server: {} [{}] ===\n\n",
        app.server_url, health
    );

    out.push_str("Active Tasks:\n");
    if app.operations.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for (i, op) in app.operations.iter().enumerate() {
            out.push_str(&format!("  {}\n", format_operation(op, i == app.selected)));
        }
    }

    out.push('\n');

    out.push_str("Recent log:\n");
    let tail: Vec<&str> = app
        .log_lines
        .iter()
        .rev()
        .take(10)
        .map(|s| s.as_str())
        .collect();
    for line in tail.iter().rev() {
        out.push_str(&format!("  {line}\n"));
    }

    if let Some((op_id, desc)) = &app.pending_approval {
        out.push_str(&format!(
            "\n[APPROVAL REQUIRED] op={op_id}: {desc}\n  Press [y] to approve, [n] to reject\n"
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{OperationSummary, TuiApp};

    fn make_app() -> TuiApp {
        let mut app = TuiApp::new("http://localhost:3000");
        app.server_healthy = true;
        app.operations = vec![OperationSummary {
            id: "op_1".into(),
            tool_name: "cargo_build".into(),
            status: "Running".into(),
            elapsed_secs: 12,
        }];
        app
    }

    #[test]
    fn render_shows_server_url() {
        let app = make_app();
        let out = render_text(&app);
        assert!(out.contains("http://localhost:3000"));
        assert!(out.contains("HEALTHY"));
    }

    #[test]
    fn render_shows_operations() {
        let app = make_app();
        let out = render_text(&app);
        assert!(out.contains("op_1"));
        assert!(out.contains("cargo_build"));
    }

    #[test]
    fn render_shows_approval_prompt() {
        let mut app = make_app();
        app.pending_approval = Some(("op_2".into(), "delete 5 files".into()));
        let out = render_text(&app);
        assert!(out.contains("APPROVAL REQUIRED"));
        assert!(out.contains("op_2"));
    }

    #[test]
    fn format_operation_marks_selected() {
        let op = OperationSummary {
            id: "op_1".into(),
            tool_name: "test".into(),
            status: "Running".into(),
            elapsed_secs: 5,
        };
        let selected = format_operation(&op, true);
        let not_selected = format_operation(&op, false);
        assert!(selected.contains('►'));
        assert!(!not_selected.contains('►'));
    }
}
