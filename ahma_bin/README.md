# ahma_bin

The `ahma` binary crate. Wires the MIT-licensed `ahma_mcp` library with the
AGPL-licensed feature crates (`ahma_vault`, `ahma_decompose`, `ahma_worker`,
`ahma_renewal`, `ahma_tui`) and the AGPL-licensed `ahma_cluster`.

## License

This crate is licensed under **AGPL-3.0-or-later**.

Any modified version of the `ahma` binary offered to remote users over a
network must provide access to its modified source code per AGPL-3.0 §13.

## Dependency license summary

| Crate | License |
|---|---|
| `ahma_mcp` | MIT OR Apache-2.0 |
| `ahma_common` | MIT OR Apache-2.0 |
| `ahma_llm_monitor` | MIT OR Apache-2.0 |
| `ahma_http_bridge` | MIT OR Apache-2.0 |
| `ahma_vault` | AGPL-3.0-or-later |
| `ahma_decompose` | AGPL-3.0-or-later |
| `ahma_worker` | AGPL-3.0-or-later |
| `ahma_renewal` | AGPL-3.0-or-later |
| `ahma_tui` | AGPL-3.0-or-later |
| `ahma_cluster` | **AGPL-3.0-or-later** |
