# Ahma Core (ahma)

Core library for the ahma server (Ahma), providing tool execution, configuration management, and async orchestration.

For full documentation and architecture details, see the [Ahma Core Documentation](https://ahma-labs.github.io/ahma/doc/ahma/index.html).

## Features

- **Tool Execution**: Synchronous and asynchronous command execution.
- **MCP Service**: Complete Model Context Protocol server implementation.
- **Safe Scoping**: Kernel-level sandboxing (Landlock/Seatbelt).
- **Operation Monitoring**: Track async operations with unique IDs.

## License

`ahma_mcp` is licensed under **MIT OR Apache-2.0**.

This crate remains permissive so other Rust applications can embed Ahma's MCP
service, sandbox, and command-execution primitives directly. Security-focused
feature crates that extend the product surface are kept separate and carry their
own license terms.

The authoritative license declaration for this crate is in
`ahma_mcp/Cargo.toml`.

