# Egress Sandbox

> **Experimental** — introduced in v0.7.

The egress sandbox closes a class of security gap present in cloud agent tools: web-fetch and MCP connections can bypass an organisation's network egress policy. Ahma's egress sandbox puts a deny-by-default HTTP proxy between each task subprocess and the network, controlled by a per-vault allowlist file.

> This deny-by-default posture is **per task vault**. An ordinary (non-vault) ahma session has **unrestricted** network egress unless you pass `--restrict-network` / set `[network] restrict = true`, which is a separate mechanism with its own `[network] allow` list — seeded by the hostnames each enabled sandbox profile declares, so restricted mode survives a first `cargo build`. See [network-egress.md](network-egress.md) for that mechanism and [security-sandbox.md](security-sandbox.md#network-egress) for the platform enforcement story.

Both mechanisms share one matcher (`egress::host_pattern::HostPattern`) and therefore one set of pattern semantics; the rules below are that matcher's, not this file's.

## Why egress sandboxing?

Kernel-level filesystem sandboxing (Landlock / Seatbelt) keeps an agent from *writing* outside the vault. Egress sandboxing keeps it from *sending data out* via HTTP — for example, exfiltrating vault contents to an attacker-controlled server via a prompt-injection payload in an input file. Note that the filesystem half of that pair is not symmetric across platforms: reads are confined on Linux but not on macOS or Windows (SPEC R6.1.6, R6.2.2, R6.3.9), which makes the egress control the load-bearing one for exfiltration on macOS.

On Linux and macOS the kernel FS sandbox also blocks the subprocess from tampering with `/etc/hosts` or `/etc/resolv.conf`, closing the DNS-rebinding route around the proxy. On Windows that write is not OS-blocked yet (SPEC R6.3.9).

## How it works

When `ahma serve` starts with a task vault, an HTTP proxy is bound to a random localhost port and injected into the subprocess environment:

```
HTTP_PROXY=http://127.0.0.1:<port>
HTTPS_PROXY=http://127.0.0.1:<port>
NO_PROXY=127.0.0.1,::1,localhost
```

Every outbound HTTP/HTTPS request from the subprocess passes through this proxy. Requests to domains in the vault's `egress.allowlist` are forwarded; all others receive `407 Proxy Authentication Required` (for CONNECT/HTTPS) or `403 Forbidden` (for plain HTTP) — indistinguishable from a real network failure.

## Allowlist format

Create `egress.allowlist` inside the vault root (or let ahma create an empty one automatically):

```
# ahma egress allowlist
# One pattern per line. Empty file = deny all outbound.

api.openai.com          # exact domain
*.anthropic.com         # single-level wildcard (sub.anthropic.com matches; deep.sub.anthropic.com does not)
```

Pattern rules:

| Pattern | Matches | Does not match |
|---------|---------|----------------|
| `api.openai.com` | `api.openai.com` only | `openai.com`, `other.openai.com` |
| `*.openai.com` | `api.openai.com`, `beta.openai.com` | `openai.com`, `deep.api.openai.com` |
| `*` | everything | — |

Lines beginning with `#` and blank lines are ignored. A line that is not a well-formed hostname pattern — a URL (`https://api.openai.com`), a `host:port`, a misplaced wildcard (`api*.openai.com`), or a non-ASCII name — is **dropped with a warning**, not coerced into an exact host. An entry that can never match must not look like one that can, or you read the file back, see your domain, and conclude egress works.

Two properties are worth stating outright, because the obvious implementation gets both wrong:

* Matching is anchored at a label boundary, never a plain suffix test: `evilopenai.com` does **not** satisfy `*.openai.com`.
* Non-ASCII names are rejected rather than folded to punycode, because `оpenai.com` (Cyrillic `о`) and `openai.com` render identically and are different hosts. Write the A-label (`xn--…`) if you mean an internationalised domain.

See [network-egress.md](network-egress.md#host-matching-semantics) for the full semantics and the regression tests behind them.

## Default allowlist: deny all

An empty `egress.allowlist` — or no file at all — means no outbound connections are permitted. This is the default for every new vault.

Add a domain only when the task explicitly needs it:

```bash
# Allow a local LLM tool to reach Ollama on localhost (already in NO_PROXY)
# No changes needed — localhost is excluded from the proxy by default.

# Allow a cloud LLM for one task:
echo "api.openai.com" >> "$VAULT/egress.allowlist"
```

## Egress and local Ollama

Ollama runs on `localhost`, which is in `NO_PROXY` and bypasses the proxy entirely. Local-LLM workflows therefore require no allowlist entries.

## Managing the allowlist programmatically

```bash
# View the current allowlist for a vault
cat "$VAULT/egress.allowlist"

# Add a domain
echo "api.example.com" >> "$VAULT/egress.allowlist"

# Replace the whole allowlist
printf "# my task\napi.example.com\n" > "$VAULT/egress.allowlist"
```

Via the `EgressAllowlist` Rust API (`ahma_core`):

```rust
use ahma_core::EgressAllowlist;

let mut list = EgressAllowlist::deny_all();
list.add("api.openai.com");
list.save(vault.path().join("egress.allowlist"))?;
```

## Using `EgressClient` in Rust code

Internal ahma services that need to make outbound HTTP calls should use
[`EgressClient`](../ahma_mcp/src/egress/client.rs) instead of a raw
`reqwest::Client`. It enforces the vault's egress policy before every request:

```rust
use ahma_mcp::egress::{EgressClient, EgressPolicy};

let policy = EgressPolicy::from_vault_path(&vault_path);
let client = EgressClient::new(policy);

// Checked against the allowlist — returns EgressError::Denied if blocked.
let body: serde_json::Value = client.get_json("https://api.openai.com/v1/models").await?;
```

This prevents code from accidentally bypassing policy by constructing a raw client.

## Limitations

- The proxy covers `HTTP_PROXY` / `HTTPS_PROXY` convention. Tools that make raw TCP connections or use their own resolver may bypass it. Landlock/Seatbelt still prevents filesystem-level workarounds.
- QUIC (HTTP/3) connections are not proxied; disable HTTP/3 in tools that support it if strict egress control is needed.

## See also

- [docs/security-sandbox.md](security-sandbox.md) — filesystem sandbox and vault scope
- [docs/task-vault.md](task-vault.md) — vault creation and layout
- [SPEC.md](../SPEC.md) — egress sandbox design notes
