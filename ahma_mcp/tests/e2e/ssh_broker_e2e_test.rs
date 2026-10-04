//! The SSH key broker end to end, through the real `ahma hooks run-shell`
//! (SPEC R-CRED.1, R-CRED.3, R-CRED.10): a hooked command signs with a key
//! it cannot read only once a human allowed it, and before that it fails with
//! one line saying how to allow it. `ssh-keygen -Y sign` with only the public
//! key signs through `SSH_AUTH_SOCK`, so no server or network is needed.
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

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
const FP: &str = "SHA256:Ve/vkYCtZQZGMMywicF/HCNoDLJfT3cvTqK0iAJLOw0";

fn hooked(bin: &Path, home: &Path, ws: &Path, command: &str) -> Output {
    Command::new(bin)
        .args(["hooks", "run-shell", "--cwd"])
        .arg(ws)
        .args(["--session-id", "e2e-session", "--command", command])
        .env("AHMA_TEST_HOME", home)
        .env_remove("SSH_AUTH_SOCK")
        .current_dir(ws)
        .output()
        .expect("run ahma hooks run-shell")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_hooked_command_signs_through_the_broker_only_once_allowed() {
    if Command::new("ssh-keygen").arg("-?").output().is_err() {
        eprintln!("skipped: ssh-keygen is not installed");
        return;
    }
    let bin = ahma_mcp::test_utils::cli::build_binary_cached("ahma_bin", "ahma");
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join(".ssh/id_ed25519"), KEY).unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(ws.path().join(".git")).unwrap();
    let ws = dunce::canonicalize(ws.path()).unwrap();
    std::fs::write(ws.join("key.pub"), format!("{PUB}\n")).unwrap();
    std::fs::write(ws.join("msg"), "signed by a hooked command\n").unwrap();
    let sign = "ssh-keygen -Y sign -n file -f key.pub msg";

    let refused = hooked(&bin, home.path(), &ws, sign);
    let said = text(&refused);
    assert!(!refused.status.success(), "no grant yet: {said}");
    assert!(said.contains("Blocked until a human approves"), "{said}");
    assert!(said.contains(&format!("{FP} for sshsig:file")), "{said}");
    assert!(!ws.join("msg.sig").exists());

    // What `ahma permissions grant ssh-sign "<FP> for sshsig:file" --yes` writes.
    ahma_common::ssh_sign::persist(
        &home.path().join(".ahma").join("settings.toml"),
        ahma_common::ssh_sign::SshSignGrant {
            key: FP.into(),
            destination: "sshsig:file".into(),
            label: String::new(),
            workspace: ws.clone(),
            granted_at: ahma_common::config::unix_now(),
            expires_at: None,
            granted_by: Some("e2e".into()),
            owner_pid: None,
        },
        "e2e",
    )
    .unwrap();

    let signed = hooked(&bin, home.path(), &ws, sign);
    assert!(signed.status.success(), "allowed: {}", text(&signed));
    let signers = ws.join("allowed_signers");
    std::fs::write(&signers, format!("me@ahma {PUB}\n")).unwrap();
    let verify = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-n", "file", "-I", "me@ahma", "-f"])
        .arg(&signers)
        .arg("-s")
        .arg(ws.join("msg.sig"))
        .stdin(std::fs::File::open(ws.join("msg")).unwrap())
        .output()
        .unwrap();
    assert!(verify.status.success(), "{}", text(&verify));
    assert!(
        !text(&signed).contains(KEY.lines().nth(2).unwrap()),
        "the key never reaches the command"
    );
}
