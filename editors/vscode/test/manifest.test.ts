import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";

const extensionRoot = path.resolve(__dirname, "../..");
const read = (file: string): string => readFileSync(path.join(extensionRoot, file), "utf8");
const manifest = JSON.parse(read("package.json"));

/** Types and modifiers VS Code knows without a declaration. */
const standardTypes = new Set([
  "namespace", "class", "enum", "interface", "struct", "typeParameter", "type", "parameter",
  "variable", "property", "enumMember", "decorator", "event", "function", "method", "macro",
  "label", "comment", "string", "keyword", "number", "regexp", "operator",
]);
const standardModifiers = new Set([
  "declaration", "definition", "readonly", "static", "deprecated", "abstract", "async",
  "modification", "documentation", "defaultLibrary",
]);

/** The string literals of a `const NAME: [&str; N] = [...]` in the server's legend. */
function legendOf(name: string): string[] {
  const source = readFileSync(
    path.resolve(extensionRoot, "../../crates/asciidoc-lsp/src/semantic_tokens.rs"),
    "utf8",
  );
  const body = new RegExp(`const ${name}: \\[&str; \\d+\\] = \\[([^\\]]*)\\]`).exec(source);
  assert.ok(body, `${name} not found in semantic_tokens.rs`);
  return [...body[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]);
}

const declared = (key: string): string[] =>
  (manifest.contributes[key] as { id: string }[]).map((entry) => entry.id);

test("every custom token type of the server legend is declared", () => {
  const custom = legendOf("TOKEN_TYPES").filter((t) => !standardTypes.has(t));
  assert.ok(custom.length > 0);
  for (const type of custom) {
    assert.ok(declared("semanticTokenTypes").includes(type), `type ${type} is not declared`);
  }
});

test("every custom token modifier of the server legend is declared", () => {
  const custom = legendOf("TOKEN_MODIFIERS").filter((m) => !standardModifiers.has(m));
  assert.ok(custom.length > 0);
  for (const modifier of custom) {
    assert.ok(
      declared("semanticTokenModifiers").includes(modifier),
      `modifier ${modifier} is not declared`,
    );
  }
});

test("declared token types are the legend's, no leftovers", () => {
  const legend = legendOf("TOKEN_TYPES");
  for (const type of declared("semanticTokenTypes")) {
    assert.ok(legend.includes(type), `declared type ${type} is not in the legend`);
  }
});

test("scope fallbacks map the markup types to markup scopes", () => {
  const [entry] = manifest.contributes.semanticTokenScopes;
  assert.equal(entry.language, "asciidoc");
  assert.deepEqual(entry.scopes.heading, ["markup.heading"]);
  for (let level = 1; level <= 6; level++) {
    assert.deepEqual(entry.scopes[`heading.level${level}`], [
      "markup.heading",
      `markup.heading.markdown heading.${level}.markdown entity.name.section.markdown`,
    ]);
  }
  assert.deepEqual(entry.scopes.strong, ["markup.bold"]);
  assert.deepEqual(entry.scopes.emphasis, ["markup.italic"]);
  assert.deepEqual(entry.scopes.raw, ["markup.inline.raw"]);
  assert.deepEqual(entry.scopes.link, ["markup.underline.link"]);
  const selectors = Object.keys(entry.scopes).map((s) => s.split(".")[0]);
  const known = new Set([...declared("semanticTokenTypes"), ...standardTypes]);
  for (const selector of selectors) {
    assert.ok(known.has(selector), `scope map names unknown type ${selector}`);
  }
});

test("semantic highlighting is on for asciidoc whatever the theme says", () => {
  assert.equal(
    manifest.contributes.configurationDefaults["[asciidoc]"]["editor.semanticHighlighting.enabled"],
    true,
  );
});

test("language configuration is referenced, shipped and comments out with // and ////", () => {
  const language = manifest.contributes.languages.find((l: { id: string }) => l.id === "asciidoc");
  assert.equal(language.configuration, "./language-configuration.json");
  assert.match(read(".vscodeignore"), /^!language-configuration\.json$/m);

  const configuration = JSON.parse(read("language-configuration.json"));
  assert.equal(configuration.comments.lineComment, "//");
  assert.deepEqual(configuration.comments.blockComment, ["////", "////"]);
  assert.ok(configuration.brackets.length > 0);
  assert.ok(configuration.autoClosingPairs.length > 0);
});
