# ahma_bin

The `ahma` binary crate. Wires the MIT-licensed `ahma_mcp` library with the
GPL-licensed feature crates (`ahma_vault`, `ahma_decompose`, `ahma_worker`,
`ahma_renewal`, `ahma_tui`) and the AGPL-licensed `ahma_cluster`.

## License

This crate is licensed under **GPL-3.0-or-later**.

Because it statically links `ahma_cluster` (AGPL-3.0-or-later), the combined
compiled `ahma` binary is effectively **AGPL-3.0-or-later** for redistribution
and network-service purposes. Any modified version of the `ahma` binary offered
to remote users over a network must provide access to its modified source code
per AGPL-3.0 §13.

## Dependency license summary

| Crate | License |
|---|---|
| `ahma_mcp` | MIT OR Apache-2.0 |
| `ahma_common` | MIT OR Apache-2.0 |
| `ahma_llm_monitor` | MIT OR Apache-2.0 |
| `ahma_http_bridge` | MIT OR Apache-2.0 |
| `ahma_vault` | GPL-3.0-or-later |
| `ahma_decompose` | GPL-3.0-or-later |
| `ahma_worker` | GPL-3.0-or-later |
| `ahma_renewal` | GPL-3.0-or-later |
| `ahma_tui` | GPL-3.0-or-later |
| `ahma_cluster` | **AGPL-3.0-or-later** |
