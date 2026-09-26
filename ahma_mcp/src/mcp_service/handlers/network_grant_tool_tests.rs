use super::*;

// ── classify_network_risk: Refused (hard denylist) ──────────────────────────

#[test]
fn refused_blanket_star() {
    let pattern = HostPattern::parse("*").unwrap();
    let risk = classify_network_risk(&pattern, "*");
    assert!(
        matches!(risk, NetGrantRisk::Refused(_)),
        "blanket '*' must be refused"
    );
}

#[test]
fn refused_localhost() {
    for host in [
        "localhost",
        "sub.localhost",
        "app.local",
        "internal.internal",
    ] {
        let pattern = HostPattern::parse(host).unwrap();
        let risk = classify_network_risk(&pattern, host);
        assert!(
            matches!(risk, NetGrantRisk::Refused(_)),
            "{host} must be refused"
        );
    }
}

#[test]
fn refused_loopback_ip() {
    for ip in ["127.0.0.1", "127.0.1.1"] {
        let pattern = HostPattern::parse(ip).unwrap();
        let risk = classify_network_risk(&pattern, ip);
        assert!(
            matches!(risk, NetGrantRisk::Refused(_)),
            "loopback {ip} must be refused"
        );
    }
}

#[test]
fn refused_metadata_and_link_local_ip() {
    for ip in ["169.254.169.254", "169.254.0.1"] {
        let pattern = HostPattern::parse(ip).unwrap();
        let risk = classify_network_risk(&pattern, ip);
        assert!(
            matches!(risk, NetGrantRisk::Refused(_)),
            "metadata/link-local {ip} must be refused"
        );
    }
}

#[test]
fn refused_private_rfc1918_ips() {
    for ip in ["10.0.0.1", "192.168.1.1", "172.16.0.1"] {
        let pattern = HostPattern::parse(ip).unwrap();
        let risk = classify_network_risk(&pattern, ip);
        assert!(
            matches!(risk, NetGrantRisk::Refused(_)),
            "private {ip} must be refused"
        );
    }
}

#[test]
fn refused_tld_broad_wildcard() {
    for wildcard in ["*.com", "*.org", "*.net", "*.io"] {
        let pattern = HostPattern::parse(wildcard).unwrap();
        let risk = classify_network_risk(&pattern, wildcard);
        assert!(
            matches!(risk, NetGrantRisk::Refused(_)),
            "TLD wildcard {wildcard} must be refused"
        );
    }
}

// ── classify_network_risk: High and Normal ──────────────────────────────────

#[test]
fn high_risk_domain_wildcard() {
    let pattern = HostPattern::parse("*.github.com").unwrap();
    let risk = classify_network_risk(&pattern, "*.github.com");
    assert!(
        matches!(risk, NetGrantRisk::High(_)),
        "*.github.com should be high risk"
    );
}

#[test]
fn normal_risk_exact_host() {
    for host in ["crates.io", "api.github.com", "registry.npmjs.org"] {
        let pattern = HostPattern::parse(host).unwrap();
        let risk = classify_network_risk(&pattern, host);
        assert_eq!(risk, NetGrantRisk::Normal, "{host} should be normal risk");
    }
}

// ── parse_host_argument ─────────────────────────────────────────────────────

#[test]
fn parse_host_rejects_urls_and_ports() {
    assert!(parse_host_argument("https://crates.io").is_err());
    assert!(parse_host_argument("crates.io:443").is_err());
    assert!(parse_host_argument("   ").is_err());
}

#[test]
fn parse_host_accepts_valid_hosts() {
    assert_eq!(parse_host_argument("crates.io").unwrap(), "crates.io");
    assert_eq!(parse_host_argument("*.github.com").unwrap(), "*.github.com");
    assert_eq!(
        parse_host_argument("  Api.Github.Com  ").unwrap(),
        "api.github.com"
    );
}

// ── preview and persist helpers ─────────────────────────────────────────────

#[test]
fn preview_text_contains_key_elements() {
    let file = std::path::Path::new("/tmp/settings.toml");
    let text = preview_text("crates.io", file, &NetGrantRisk::Normal);
    assert!(text.contains("PREVIEW ONLY"));
    assert!(text.contains("crates.io"));
    assert!(text.contains("/tmp/settings.toml"));
    assert!(text.contains("confirm: true"));
}

#[test]
fn agent_requested_text_names_cli_command() {
    let file = std::path::Path::new("/tmp/settings.toml");
    let text = agent_requested_text("crates.io", file);
    assert!(text.contains("ahma network allow crates.io"));
    assert!(text.contains("cannot self-persist"));
}

#[test]
fn grant_prompt_message_states_host_and_answers() {
    let file = std::path::Path::new("/tmp/settings.toml");
    let msg = grant_prompt_message("crates.io", file, &NetGrantRisk::Normal);
    assert!(msg.contains("crates.io"));
    assert!(msg.contains("'always'"));
    assert!(msg.contains("'session'"));
    assert!(msg.contains("'deny'"));
}

#[test]
fn declined_text_reports_nothing_written() {
    let file = std::path::Path::new("/tmp/settings.toml");
    let text = declined_text("crates.io", file);
    assert!(text.contains("Not granted"));
    assert!(text.contains("Nothing was appended"));
    assert!(text.contains("crates.io"));
}

#[test]
fn success_text_states_status() {
    let file = std::path::Path::new("/tmp/settings.toml");
    let text_added = success_text("crates.io", file, true);
    assert!(text_added.contains("Added \"crates.io\""));
    let text_existing = success_text("crates.io", file, false);
    assert!(text_existing.contains("already present"));
}

#[test]
fn schema_requires_host() {
    let schema = network_grant_schema();
    let required = schema.get("required").and_then(Value::as_array).unwrap();
    assert!(required.iter().any(|v| v.as_str() == Some("host")));
}
