# ahma_bin

The `ahma` binary crate. It links the permissive runtime crates and the
AGPL-licensed product crates into the shipped `ahma` executable.

## License

This crate is licensed under **AGPL-3.0-or-later**. Although it depends on
permissive crates such as `ahma_mcp` and `ahma_common`, the combined binary
also links AGPL-licensed crates, so distribution of the resulting `ahma`
executable is governed by **AGPL-3.0-or-later**.

Any modified version of the `ahma` binary offered to remote users over a
network must provide access to its modified source code per AGPL-3.0 §13.

Each crate's `Cargo.toml` is the authoritative license declaration for that
crate; this README is a summary.

## Dependency license summary

| Crate | License | Why it is here |
|---|---|---|
| `ahma_mcp` | MIT OR Apache-2.0 | Core MCP service, sandbox, and command execution |
| `ahma_common` | MIT OR Apache-2.0 | Shared runtime types and configuration |
| `ahma_decompose` | AGPL-3.0-or-later | Decomposition runtime |
| `ahma_worker` | AGPL-3.0-or-later | Ephemeral worker execution |
| `ahma_renewal` | AGPL-3.0-or-later | Renewal / unattended-session controls |
| `ahma_tui` | AGPL-3.0-or-later | Terminal UI and approval flow |

The `ahma` executable is the assembly point for these crates, so the binary is
documented and released as **AGPL-3.0-or-later**.
