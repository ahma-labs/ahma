# Licensing

Ahma is licensed **per crate**. Each crate's `Cargo.toml` is the authoritative declaration;
the repository root carries `MIT_LICENSE.txt`, `APACHE_LICENSE.txt` and `AGPL_LICENSE.txt`
because different crates use different licenses.

## Two tiers

- **The embeddable engine is `MIT OR Apache-2.0`**, so other Rust applications can use
  ahma's sandbox, MCP service and protocol without copyleft obligations. Apache-2.0 adds an
  explicit patent grant; MIT keeps the usual Rust dual-license option.
- **The shipped product surface is `AGPL-3.0`**: the `ahma` binary and the interfaces a human
  answers permission questions in. Anyone distributing a modified `ahma`, or running one for
  remote users, must publish its source.

| Crate | License | What it is |
|---|---|---|
| `ahma_mcp` | MIT OR Apache-2.0 | MCP server engine: sandbox, shell pool, tool execution |
| `ahma_core` | MIT OR Apache-2.0 | Library for embedding ahma in other Rust applications ([guide](ahma-core-library.md)) |
| `ahma_common` | MIT OR Apache-2.0 | Shared configuration, permissions ledger, timeouts, TLS helpers |
| `ahma_http_bridge` | MIT OR Apache-2.0 | HTTP bridge and session isolation |
| `ahma_http_mcp_client` | MIT OR Apache-2.0 | MCP client for Streamable HTTP, with OAuth 2.0 + PKCE |
| `ahma_harness_tools` | MIT OR Apache-2.0 | Scope-validated file I/O, search and web fetch behind the built-in tools |
| `ahma_harness_guard` | MIT OR Apache-2.0 | Small-model guards: tool-name and argument healing, loop detection |
| `ahma_llm_monitor` | MIT OR Apache-2.0 | OpenAI-compatible LLM client for log analysis and chat |
| `ahma_log_monitor` | MIT OR Apache-2.0 | Live log monitor: level detection, redaction (`monitor_level`) |
| `ahma_simplify` | MIT OR Apache-2.0 | Code complexity analysis (`ahma simplify`) |
| `ahma_bundle` | MIT OR Apache-2.0 | Tool-bundle audit and checksums (`ahma bundle`) |
| `ahma_update` | MIT OR Apache-2.0 | Self-update and Sigstore verification (`ahma update`, `ahma verify`) |
| `ahma_vault` | MIT OR Apache-2.0 | Task vaults |
| `generate_tool_schema`, `ahma_test_support`, `xtask` | MIT OR Apache-2.0 | Schema generator, test helpers, release tooling (dev only, not shipped) |
| `ahma_tui` | AGPL-3.0 | Terminal dashboard, chat and approvals (`ahma tui`) |
| `ahma_gui` | AGPL-3.0 | Browser and desktop surfaces (`ahma web`, `ahma desktop`) |
| `ahma_bin` | AGPL-3.0 | The shipped `ahma` binary |

## Common uses

| If you want to… | Then |
|---|---|
| Embed `ahma_core`, `ahma_mcp` or `ahma_http_mcp_client` in your application | MIT OR Apache-2.0 applies to those crates |
| Distribute a modified `ahma` binary | AGPL-3.0: publish the corresponding source |
| Offer a modified `ahma` to remote users | AGPL-3.0: the same source obligation applies |
| Use ahma for your own or your company's work | Allowed under the crates' terms |

## Why AGPL, and what it does not do

AGPL keeps every distributed `ahma` open, so a closed, backdoored fork cannot legally be
shipped as ahma. It cannot tell you whether the binary *you* installed was built from that
source; build provenance does that. Every release is built by GitHub Actions and carries a
Sigstore-backed SLSA build-provenance attestation, with no private key to leak. `ahma update`
and the installer check it; to check it independently of the binary in question:

```bash
gh attestation verify ahma-release-linux-x86_64.tar.gz --repo ahma-labs/ahma
```

Details, including macOS Developer-ID signing (wired, not yet active):
[release-signing.md](release-signing.md).

## See also

- [SPEC.md §2.1 Crates and licenses](../SPEC.md#21-crates-and-licenses)
- [release-signing.md](release-signing.md)
