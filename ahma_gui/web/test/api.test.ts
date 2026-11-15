import assert from "node:assert/strict";
import { test } from "node:test";
import { desktopToken } from "../src/api.ts";

// `ahma desktop`'s window hands the page its token on a frozen global; a page
// in an ordinary browser has none and pairs with a code.
test("a token is taken only from the desktop window's global, and only a non-empty string", () => {
  const host = globalThis as { __ahmaDesktop?: unknown };
  delete host.__ahmaDesktop;
  assert.equal(desktopToken(), null);
  host.__ahmaDesktop = { token: "" };
  assert.equal(desktopToken(), null);
  host.__ahmaDesktop = { token: 42 };
  assert.equal(desktopToken(), null);
  host.__ahmaDesktop = Object.freeze({ token: "1.abc" });
  assert.equal(desktopToken(), "1.abc");
  delete host.__ahmaDesktop;
});
