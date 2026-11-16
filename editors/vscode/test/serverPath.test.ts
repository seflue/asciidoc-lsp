import { test } from "node:test";
import assert from "node:assert/strict";
import { binaryName, resolveServer } from "../src/serverPath";

const bundled = "/ext/server/asciidoc-lsp";
const exists = (...present: string[]) => (p: string) => present.includes(p);

test("setting wins over bundled binary", () => {
  const r = resolveServer({ setting: "/opt/lsp", bundled, exists: exists(bundled) });
  assert.deepEqual(r, { command: "/opt/lsp", source: "setting" });
});

test("blank setting counts as unset", () => {
  const r = resolveServer({ setting: "  ", bundled, exists: exists(bundled) });
  assert.deepEqual(r, { command: bundled, source: "bundled" });
});

test("bundled binary is used when present", () => {
  const r = resolveServer({ setting: "", bundled, exists: exists(bundled) });
  assert.deepEqual(r, { command: bundled, source: "bundled" });
});

test("falls back to PATH when nothing is bundled", () => {
  const r = resolveServer({ setting: "", bundled, exists: exists() });
  assert.deepEqual(r, { command: "asciidoc-lsp", source: "path" });
});

test("binary name carries .exe on Windows only", () => {
  assert.equal(binaryName("win32"), "asciidoc-lsp.exe");
  assert.equal(binaryName("linux"), "asciidoc-lsp");
  assert.equal(binaryName("darwin"), "asciidoc-lsp");
});
