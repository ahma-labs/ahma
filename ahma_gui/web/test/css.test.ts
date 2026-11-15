import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

// The page is styled by one hand-written sheet. These rules are what keep a
// button at least as wide as its label: `body` sets `overflow-wrap: anywhere`
// so long paths break, and a flex item inherits it, which lets the browser
// squeeze "Send" to one letter per line beside a full-width textarea.
const css = readFileSync(new URL("../app.css", import.meta.url), "utf8").replace(
  /\/\*[\s\S]*?\*\//g,
  "",
);

/** The declarations of the first rule whose selector list is exactly `selector`. */
function declarations(selector: string): Map<string, string> {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\>]/g, "\\$&");
  const match = new RegExp(`(?:^|\\n)${escaped}\\s*\\{([^}]*)\\}`).exec(css);
  assert.ok(match, `no rule for ${selector}`);
  const out = new Map<string, string>();
  for (const decl of (match[1] ?? "").split(";")) {
    const at = decl.indexOf(":");
    if (at > 0) out.set(decl.slice(0, at).trim(), decl.slice(at + 1).trim());
  }
  return out;
}

test("a button never shrinks below its label", () => {
  const btn = declarations(".btn");
  assert.equal(btn.get("flex-shrink"), "0");
  // `break-word`, unlike the inherited `anywhere`, does not lower the
  // min-content width, so a label breaks only at spaces and only when it
  // would otherwise overflow its container.
  assert.equal(btn.get("overflow-wrap"), "break-word");
  assert.equal(btn.get("max-width"), "100%");
});

test("the composer's textarea takes the space the button leaves", () => {
  const input = declarations(".composer > .input");
  assert.equal(input.get("flex"), "1 1 auto");
  assert.equal(input.get("min-width"), "0");
  assert.equal(input.get("width"), "auto");
});
