# Egress Sandbox

> **Experimental** — introduced in v0.7.

The egress sandbox closes a class of security gap present in cloud agent tools: web-fetch and MCP connections can bypass an organisation's network egress policy. Ahma's egress sandbox puts a deny-by-default HTTP proxy between each task subprocess and the network, controlled by a per-vault allowlist file.

## Why egress sandboxing?

Kernel-level filesystem sandboxing (Landlock / Seatbelt) prevents an agent from *reading or writing* outside the vault. Egress sandboxing prevents it from *sending data out* via HTTP — for example, exfiltrating vault contents to an attacker-controlled server via a prompt-injection payload in an input file.

The kernel FS sandbox also prevents the subprocess from tampering with `/etc/hosts` or `/etc/resolv.conf`, so DNS rebinding cannot be used to route traffic around the proxy.

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

Lines beginning with `#` and blank lines are ignored.

## Default allowlist: deny all

An empty `egress.allowlist` — or no file at all — means no outbound connections are permitted. This is the default for every new vault.

Add a domain only when the task explicitly needs it:

```bash
# Allow the decompose tool to reach Ollama on localhost (already in NO_PROXY)
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

## Limitations

- The proxy covers `HTTP_PROXY` / `HTTPS_PROXY` convention. Tools that make raw TCP connections or use their own resolver may bypass it. Landlock/Seatbelt still prevents filesystem-level workarounds.
- QUIC (HTTP/3) connections are not proxied; disable HTTP/3 in tools that support it if strict egress control is needed.

## See also

- [docs/security-sandbox.md](security-sandbox.md) — filesystem sandbox and vault scope
- [docs/task-vault.md](task-vault.md) — vault creation and layout
- [SPEC.md](../SPEC.md) — egress sandbox design notes
