# Generate Tool Schema

Utility for generating JSON schemas for Ahma tool configurations.

## Overview

`generate-tool-schema` creates JSON schema definitions for tool configuration files, making it easier to validate and document tool definitions.

## Usage

```bash
# Generate schema in the default directory (docs/)
cargo run -p generate-tool-schema

# Generate schema in a custom directory
cargo run -p generate-tool-schema -- ./schemas
```

## Output

The tool generates a single file: `mtdf-schema.json` in the specified output directory.

Generates JSON schema files that can be used for:
- IDE autocomplete and validation
- Documentation generation
- Configuration validation
- Tool development

## License

`generate_tool_schema` is licensed under **MIT OR Apache-2.0**.

It stays permissive because it is a standalone schema-generation utility rather
than part of the shipped end-user product surface.

The authoritative license declaration for this crate is in
`generate_tool_schema/Cargo.toml`.

