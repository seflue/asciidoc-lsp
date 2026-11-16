import { test } from "node:test";
import assert from "node:assert/strict";
import { disposeQuietly, runningStatus, startFailure } from "../src/lifecycle";

test("dispose failure of a client that never started is swallowed", async () => {
  let called = false;
  await disposeQuietly({
    dispose: async () => {
      called = true;
      throw new Error("Client is not running and can't be stopped");
    },
  });
  assert.equal(called, true);
});

test("dispose without a client is a no-op", async () => {
  await disposeQuietly(undefined);
});

test("running status shows server name and version", () => {
  assert.deepEqual(runningStatus({ name: "asciidoc-lsp", version: "0.7.0" }), {
    text: "asciidoc-lsp 0.7.0",
    severity: "info",
  });
});

test("running status without version or serverInfo", () => {
  assert.equal(runningStatus({ name: "asciidoc-lsp" }).text, "asciidoc-lsp");
  assert.deepEqual(runningStatus(undefined), { text: "asciidoc-lsp", severity: "info" });
});

test("failure with configured path names that path", () => {
  const f = startFailure({ command: "~/bin/asciidoc-lsp", source: "setting" });
  assert.equal(f.severity, "error");
  assert.match(f.detail, /~\/bin\/asciidoc-lsp/);
  assert.match(f.detail, /asciidoc-lsp\.server\.path/);
  assert.doesNotMatch(f.detail, /PATH/);
});

test("failure without setting hints at setting and PATH", () => {
  for (const source of ["bundled", "path"] as const) {
    const f = startFailure({ command: "asciidoc-lsp", source });
    assert.match(f.detail, /asciidoc-lsp\.server\.path/);
    assert.match(f.detail, /PATH/);
  }
});
