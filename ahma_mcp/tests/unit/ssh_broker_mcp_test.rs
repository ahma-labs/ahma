//! The SSH key broker on the MCP path (SPEC R-CRED.1, R-CRED.10): an async
//! `run_terminal_command` signs through the broker, and a refused signature
//! is an alert on the operation while it runs, with the one line that says
//! how to allow it.
#![cfg(unix)]

use ahma_mcp::adapter::Adapter;
use ahma_mcp::credentials::ssh_agent::consent::RecordedBrokers;
use ahma_mcp::operation_monitor::{MonitorConfig, OperationMonitor};
use ahma_mcp::sandbox::{Sandbox, SandboxMode};
use ahma_mcp::shell_pool::{ShellPoolConfig, ShellPoolManager};
use std::sync::Arc;
use std::time::Duration;

// A key made for this test with `ssh-keygen -t ed25519 -N ''`.
const KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACB0oJMvoCd/m51H/zkgLaQqlFDFSF4LCx+uTEYv+1580AAAAJBHctMAR3LT
AAAAAAtzc2gtZWQyNTUxOQAAACB0oJMvoCd/m51H/zkgLaQqlFDFSF4LCx+uTEYv+1580A
AAAECnXzDMMGVedTdmUvGOkGBVmMAnGzCVA3iMrzC36CjrL3Sgky+gJ3+bnUf/OSAtpCqU
UMVIXgsLH65MRi/7XnzQAAAADGZpeHR1cmVAYWhtYQE=
-----END OPENSSH PRIVATE KEY-----
";
const PUB: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHSgky+gJ3+bnUf/OSAtpCqUUMVIXgsLH65MRi/7XnzQ fixture@ahma";

/// A workspace with a public key and a message to sign, and an adapter whose
/// commands are served the recorded brokers for a home with the matching
/// private key. `None` (skipped) without `ssh-keygen`.
fn signing_fixture() -> Option<(
    tempfile::TempDir,
    tempfile::TempDir,
    Adapter,
    Arc<OperationMonitor>,
)> {
    if std::process::Command::new("ssh-keygen")
        .arg("-?")
        .output()
        .is_err()
    {
        eprintln!("skipped: ssh-keygen is not installed");
        return None;
    }
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join(".ssh/id_ed25519"), KEY).unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("key.pub"), format!("{PUB}\n")).unwrap();
    std::fs::write(ws.path().join("msg"), "hello\n").unwrap();

    let monitor = Arc::new(OperationMonitor::new(MonitorConfig::with_timeout(
        Duration::from_secs(30),
    )));
    let sandbox = Arc::new(
        Sandbox::new(
            vec![ws.path().to_path_buf()],
            SandboxMode::Test,
            Default::default(),
        )
        .unwrap(),
    );
    let adapter = Adapter::new(
        monitor.clone(),
        Arc::new(ShellPoolManager::new(ShellPoolConfig::default())),
        sandbox,
    )
    .unwrap()
    .with_credential_brokers(Arc::new(RecordedBrokers::new(
        home.path().to_path_buf(),
        None,
        None,
        None,
        vec![ws.path().to_path_buf()],
    )));
    Some((home, ws, adapter, monitor))
}

#[tokio::test]
async fn a_refused_signature_is_an_alert_on_the_running_operation() {
    let Some((_home, ws, adapter, monitor)) = signing_fixture() else {
        return;
    };
    let op_id = adapter
        .execute_async_in_dir(
            "run_terminal_command",
            "ssh-keygen -Y sign -n file -f key.pub msg",
            None,
            ws.path().to_str().unwrap(),
            Some(30),
        )
        .await
        .unwrap();
    let wait =
        ahma_common::timeouts::TestTimeouts::get(ahma_common::timeouts::TimeoutCategory::ToolCall);
    let op = tokio::time::timeout(wait, monitor.wait_for_operation(&op_id))
        .await
        .expect("the operation ends")
        .expect("the operation is known");
    assert!(
        op.alerts
            .iter()
            .any(|a| a.starts_with("Blocked until a human approves") && a.contains("sshsig:file")),
        "the refusal is an alert: {:?}",
        op.alerts
    );
    assert!(!ws.path().join("msg.sig").exists(), "nothing was signed");
}

/// The synchronous path (terminal hooks, `ahma tool run`, sync-mode
/// `run_terminal_command`) is served the same broker, and has no operation
/// to raise an alert on: what the broker refused is in the error the call
/// returns, one line each (SPEC R-CRED.10).
#[tokio::test]
async fn the_synchronous_path_returns_a_refused_signature_with_its_error() {
    let Some((_home, ws, adapter, _monitor)) = signing_fixture() else {
        return;
    };
    let err = adapter
        .execute_sync_in_dir(
            "ssh-keygen -Y sign -n file -f key.pub msg",
            None,
            ws.path().to_str().unwrap(),
            Some(30),
            None,
        )
        .await
        .expect_err("the signature is refused, so ssh-keygen fails");
    let text = format!("{err:#}");
    assert!(
        text.lines()
            .any(|l| l.starts_with("Blocked until a human approves") && l.contains("sshsig:file")),
        "the refusal is in the error: {text}"
    );
    assert!(!ws.path().join("msg.sig").exists(), "nothing was signed");
}
