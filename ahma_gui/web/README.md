# ahma web page

The page `ahma web` and `ahma desktop` serve: plain TypeScript compiled with `tsc`, the only
development dependency. `cargo build` never runs Node. After changing the source:

```bash
cd ahma_gui/web
npm ci --ignore-scripts
npm run build
npm test
```

and commit the result in `ahma_gui/web/dist/`; CI fails if it differs from a fresh compile.
User documentation: [docs/web.md](../../docs/web.md). Requirements: [ahma_gui/SPEC.md](../SPEC.md).
