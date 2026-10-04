//! SPIKE (never merged): what macOS records when Seatbelt denies something,
//! and whether an unprivileged process can read it back. Prints its findings
//! by failing, so CI shows them.
#![cfg(target_os = "macos")]

use std::process::Command;

fn run(profile: &str, args: &[&str]) -> std::process::Output {
    Command::new("sandbox-exec")
        .arg("-p")
        .arg(profile)
        .args(args)
        .output()
        .expect("sandbox-exec")
}

#[test]
fn spike_seatbelt_denial_records() {
    let tag = format!("ahma-spike-{}", std::process::id());
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("denied.txt");
    let mut report = String::new();

    // 1. Does a tagged deny compile, and what does the tag look like in the log?
    let tagged = format!(
        "(version 1)\n(allow default)\n(deny file-write* (subpath \"{}\") (with message \"{tag}\"))\n(deny network-outbound (remote ip \"*:9\") (with message \"{tag}\"))\n(deny signal (target others) (with message \"{tag}\"))\n(deny mach-lookup (global-name \"com.apple.spike.none\") (with message \"{tag}\"))\n",
        dir.path().display()
    );
    let start = std::time::Instant::now();
    let w = run(&tagged, &["/usr/bin/touch", &target.to_string_lossy()]);
    report += &format!(
        "tagged write: exit={:?} stderr={}\n",
        w.status.code(),
        String::from_utf8_lossy(&w.stderr).trim()
    );
    let r = run(&tagged, &["/bin/kill", "-0", "1"]);
    report += &format!(
        "tagged kill 1: exit={:?} stderr={}\n",
        r.status.code(),
        String::from_utf8_lossy(&r.stderr).trim()
    );
    let r = run(&tagged, &["/usr/bin/nc", "-z", "-w", "1", "127.0.0.1", "9"]);
    report += &format!("tagged nc: exit={:?}\n", r.status.code());

    // 2. Read the records back, as this (unprivileged, unsandboxed) process.
    for wait_ms in [0u64, 500, 1500, 3000] {
        std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        let t = std::time::Instant::now();
        let out = Command::new("/usr/bin/log")
            .args(["show", "--last", "1m", "--style", "ndjson", "--predicate"])
            .arg(format!("eventMessage CONTAINS \"{tag}\""))
            .output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                let lines: Vec<&str> = text.lines().filter(|l| l.contains(&tag)).collect();
                report += &format!(
                    "log show after +{wait_ms}ms (took {:?}, exit {:?}): {} records; stderr={}\n",
                    t.elapsed(),
                    o.status.code(),
                    lines.len(),
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                for l in lines.iter().take(6) {
                    let msg = l
                        .split("\"eventMessage\":\"")
                        .nth(1)
                        .and_then(|r| r.split("\",").next())
                        .unwrap_or(l);
                    let sender = l
                        .split("\"senderImagePath\":\"")
                        .nth(1)
                        .and_then(|r| r.split('"').next())
                        .unwrap_or("?");
                    report += &format!("  [{sender}] {msg}\n");
                }
                if !lines.is_empty() {
                    break;
                }
            }
            Err(e) => {
                report += &format!("log show failed to run: {e}\n");
                break;
            }
        }
    }

    // 3. Untagged default deny: what does a plain (deny default) record say?
    let plain = "(version 1)\n(deny default)\n(allow process*)\n(allow file-read*)\n(allow sysctl-read)\n(allow mach-lookup)\n";
    let w = run(plain, &["/usr/bin/touch", &target.to_string_lossy()]);
    report += &format!("plain deny write: exit={:?}\n", w.status.code());
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let out = Command::new("/usr/bin/log")
        .args(["show", "--last", "30s", "--style", "compact", "--predicate"])
        .arg("sender == \"Sandbox\"")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    report += "plain records (sender == Sandbox, last 30s):\n";
    for l in text.lines().filter(|l| l.contains("touch")).take(4) {
        report += &format!("  {l}\n");
    }
    report += &format!("total elapsed {:?}\n", start.elapsed());
    panic!("SPIKE FINDINGS\n{report}");
}
